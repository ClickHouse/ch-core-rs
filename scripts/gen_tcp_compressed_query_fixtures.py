#!/usr/bin/env python3
"""Capture full compressed SELECT exchanges at TCP revision 54460.

Only Python's standard library, the zstd CLI, loopback, and empty passwords are
used. Packet headers remain raw at this revision. Data, Totals, and Extremes
Native bodies use ClickHouse compression frames. The parser finds boundaries
from decoded Native blocks rather than searching byte patterns.
"""

import argparse
from datetime import date
import hashlib
import io
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gen_tcp_fixtures import Reader, lz4, string, varuint
import gen_tcp_query_fixtures as query
from gen_tcp_handshake_fixtures import read_hello


METHOD_BYTES = {"NONE": 0x02, "LZ4": 0x82, "ZSTD": 0x90}
CASES = {
    "lz4_multi": {
        "method": "LZ4",
        "sql": (
            "SELECT number + 13 AS n, concat('user_', toString(number)) AS s "
            "FROM numbers(5) SETTINGS max_block_size=2, max_threads=1"
        ),
    },
    "lz4_large": {
        "method": "LZ4",
        "sql": "SELECT toUInt64(79) AS n, repeat('xx', 550000) AS s",
    },
    "lz4_totals_extremes": {
        "method": "LZ4",
        "sql": (
            "SELECT number % 2 AS k, count() AS n FROM numbers(5) "
            "GROUP BY k WITH TOTALS ORDER BY k SETTINGS max_threads=1, extremes=1"
        ),
    },
    "lz4_exception": {
        "method": "LZ4",
        "sql": (
            "SELECT intDiv(toUInt64(79), number != 1000) AS n FROM numbers(2000) "
            "SETTINGS max_block_size=128, max_threads=1"
        ),
    },
    "none": {
        "method": "NONE",
        "sql": "SELECT toUInt64(13) AS n, 'user_1' AS s",
    },
    "zstd": {
        "method": "ZSTD",
        "sql": "SELECT toUInt64(79) AS n, repeat('user_2', 79) AS s",
    },
}
CANCEL_SQL = (
    "SELECT number AS n FROM numbers(1000000000) "
    "WHERE sleepEachRow(0.00001) = 0 "
    "SETTINGS max_block_size=65536, max_threads=1"
)


def settings(method):
    return query.CAPTURE_SETTINGS + [("network_compression_method", method)]


def request_bytes(label, sql, method):
    return query.query_packet(
        sql,
        f"ch-core-compressed-fixture-{label}",
        settings=settings(method),
        compression=True,
    )


def decode_frame(method, payload, plain_size):
    if method == METHOD_BYTES["NONE"]:
        if len(payload) != plain_size:
            raise ValueError("NONE frame size mismatch")
        return payload
    if method == METHOD_BYTES["LZ4"]:
        return bytes(lz4(payload, plain_size))
    if method == METHOD_BYTES["ZSTD"]:
        decoded = subprocess.run(
            ["zstd", "--decompress", "--stdout", "--quiet"],
            input=payload,
            capture_output=True,
            check=True,
        ).stdout
        if len(decoded) != plain_size:
            raise ValueError("ZSTD frame size mismatch")
        return decoded
    raise ValueError(f"unexpected compression method 0x{method:02x}")


def compressed_native_block(reader, expected_method):
    """Read frames through exactly one complete Native block."""
    start = len(reader.record)
    plain = bytearray()
    frames = []
    while True:
        checksum = reader.read(16)
        header = reader.read(9)
        method, compressed_size, plain_size = struct.unpack("<BII", header)
        if method != METHOD_BYTES[expected_method]:
            raise ValueError(
                f"expected {expected_method} method 0x{METHOD_BYTES[expected_method]:02x}, "
                f"got 0x{method:02x}"
            )
        if not 9 <= compressed_size <= 0x40000000:
            raise ValueError(f"invalid compressed_size {compressed_size}")
        if not 0 < plain_size <= 0x40000000:
            raise ValueError(f"invalid uncompressed_size {plain_size}")
        payload = reader.read(compressed_size - 9)
        plain.extend(decode_frame(method, payload, plain_size))
        frames.append(
            {
                "method": method,
                "compressed_size": compressed_size,
                "uncompressed_size": plain_size,
                "checksum": checksum.hex(),
            }
        )
        block_reader = Reader(io.BytesIO(plain))
        try:
            rows, columns, schema, bodies, block = query.native_block(block_reader)
        except EOFError:
            continue
        if len(block_reader.record) != len(plain):
            raise ValueError("compression frame plaintext extends past Native block")
        return {
            "rows": rows,
            "columns": columns,
            "schema": schema,
            "_column_bodies": [body.hex() for body in bodies],
            "native": block,
            "wire_length": len(reader.record) - start,
            "frames": frames,
        }


def response(reader, method):
    packets = bytearray()
    native = bytearray()
    boundaries = []
    while True:
        reader.record.clear()
        code = reader.uint()
        entry = {"code": code, "offset": len(packets)}
        if code in (1, 7, 8):
            entry["name"] = reader.string().decode()
            block_offset = len(reader.record)
            block = compressed_native_block(reader, method)
            native_offset = len(native)
            native.extend(block.pop("native"))
            entry.update(
                block,
                block_offset=entry["offset"] + block_offset,
                block_length=len(reader.record) - block_offset,
                native_offset=native_offset,
                native_length=len(native) - native_offset,
                native_sha256=hashlib.sha256(native[native_offset:]).hexdigest(),
            )
        elif code in (10, 14):
            # At revision 54460 logs and profile events remain uncompressed.
            entry["name"] = reader.string().decode()
            block_offset = len(reader.record)
            rows, columns, schema, bodies, block = query.native_block(reader)
            native_offset = len(native)
            native.extend(block)
            entry.update(
                rows=rows,
                columns=columns,
                schema=schema,
                _column_bodies=[body.hex() for body in bodies],
                block_offset=entry["offset"] + block_offset,
                block_length=len(block),
                native_offset=native_offset,
                native_length=len(block),
                native_sha256=hashlib.sha256(block).hexdigest(),
                frames=[],
            )
        elif code == 3:
            entry["progress"] = [reader.uint() for _ in range(6)]
        elif code == 6:
            entry["profile"] = [reader.uint() for _ in range(3)]
            entry["applied_limit"] = reader.read(1)[0]
            entry["rows_before_limit"] = reader.uint()
            entry["obsolete"] = reader.read(1)[0]
        elif code == 2:
            entry["exception_code"] = struct.unpack("<i", reader.read(4))[0]
            entry["exception_name"] = reader.string().decode()
            entry["message"] = reader.string().decode(errors="replace")
            entry["stack_trace"] = reader.string().decode(errors="replace")
            entry["has_nested"] = reader.read(1)[0] != 0
        elif code != 5:
            raise ValueError(f"unexpected server packet {code}")
        entry["length"] = len(reader.record)
        packets.extend(reader.record)
        boundaries.append(entry)
        if code in (2, 5):
            return bytes(packets), bytes(native), boundaries


def public_packets(packets):
    return [
        {key: value for key, value in packet.items() if not key.startswith("_")}
        for packet in packets
    ]


def open_connection(host, port):
    sock = socket.create_connection((host, port), timeout=30)
    sock.sendall(query.client_hello())
    source = sock.makefile("rb", buffering=0)
    read_hello(Reader(source))
    sock.sendall(string(query.QUOTA_KEY))
    return sock, source


def capture_query(sock, reader, label, sql, method):
    request = request_bytes(label, sql, method)
    sock.sendall(request)
    packets, native, boundaries = response(reader, method)
    return request, packets, native, boundaries


def add_capture(captures, manifest, label, sql, method, request, packets, native, boundaries):
    request_name = f"tcp_compressed_query_{label}_client_rev54460.packet"
    response_name = f"tcp_compressed_query_{label}_server_rev54460.packets"
    captures[request_name] = request
    captures[response_name] = packets
    manifest["queries"][label] = {
        "sql": sql,
        "query_id": f"ch-core-compressed-fixture-{label}",
        "network_compression_method": method,
        "decompressed_native_length": len(native),
        "decompressed_native_sha256": hashlib.sha256(native).hexdigest(),
        "packets": public_packets(boundaries),
    }


def capture_all(host, port):
    captures = {}
    manifest = {
        "captured_on": date.today().isoformat(),
        "server_version": query.VERSION,
        "source_tag": f"v{query.VERSION}-lts",
        "declared_revision": query.REVISION,
        "compression": True,
        "framed_packet_kinds": ["Data", "Totals", "Extremes"],
        "raw_packet_kinds": [
            "Exception",
            "Progress",
            "EndOfStream",
            "ProfileInfo",
            "Log",
            "ProfileEvents",
        ],
        "queries": {},
        "files": {},
    }
    for label, case in CASES.items():
        if label == "lz4_exception":
            continue
        sock, source = open_connection(host, port)
        try:
            reader = Reader(source)
            result = capture_query(sock, reader, label, case["sql"], case["method"])
            if result[3][-1]["code"] != 5:
                raise ValueError(f"{label} did not end with EndOfStream")
            add_capture(
                captures,
                manifest,
                label,
                case["sql"],
                case["method"],
                *result,
            )
        finally:
            source.close()
            sock.close()

    case = CASES["lz4_exception"]
    sock, source = open_connection(host, port)
    try:
        reader = Reader(source)
        result = capture_query(sock, reader, "lz4_exception", case["sql"], "LZ4")
        if result[3][-1]["code"] != 2:
            raise ValueError("exception fixture did not end with Exception")
        if not any(
            packet["code"] == 1 and packet.get("rows", 0) > 0 for packet in result[3]
        ):
            raise ValueError("exception fixture has no flushed positive-row Data")
        add_capture(
            captures,
            manifest,
            "lz4_exception",
            case["sql"],
            "LZ4",
            *result,
        )
        recovery_sql = "SELECT toUInt64(13) AS n"
        result = capture_query(sock, reader, "lz4_exception_recovery", recovery_sql, "LZ4")
        if result[3][-1]["code"] != 5:
            raise ValueError("connection did not recover after query-time Exception")
        add_capture(
            captures,
            manifest,
            "lz4_exception_recovery",
            recovery_sql,
            "LZ4",
            *result,
        )
    finally:
        source.close()
        sock.close()

    sock, source = open_connection(host, port)
    try:
        reader = Reader(source)
        request = request_bytes("lz4_cancel", CANCEL_SQL, "LZ4")
        sock.sendall(request)
        sock.sendall(varuint(3))
        packets, native, boundaries = response(reader, "LZ4")
        if boundaries[-1]["code"] != 5:
            raise ValueError("cancel fixture did not end with EndOfStream")
        add_capture(
            captures,
            manifest,
            "lz4_cancel",
            CANCEL_SQL,
            "LZ4",
            request + varuint(3),
            packets,
            native,
            boundaries,
        )
        manifest["queries"]["lz4_cancel"]["request_includes_raw_cancel"] = True
        recovery_sql = "SELECT toUInt64(79) AS n"
        result = capture_query(sock, reader, "lz4_cancel_recovery", recovery_sql, "LZ4")
        if result[3][-1]["code"] != 5:
            raise ValueError("connection did not recover after Cancel")
        add_capture(
            captures,
            manifest,
            "lz4_cancel_recovery",
            recovery_sql,
            "LZ4",
            *result,
        )
    finally:
        source.close()
        sock.close()
    return captures, manifest


def check_fixtures(output):
    manifest_path = output / "tcp_compressed_query_rev54460.json"
    manifest = json.loads(manifest_path.read_text())
    for name, expected in manifest["files"].items():
        data = (output / name).read_bytes()
        actual = {
            "length": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
        }
        if actual != {key: expected[key] for key in actual}:
            raise ValueError(f"{name} does not match manifest: {actual}")
    for label, expected in manifest["queries"].items():
        name = f"tcp_compressed_query_{label}_server_rev54460.packets"
        data = (output / name).read_bytes()
        packets, native, boundaries = response(
            Reader(io.BytesIO(data)),
            expected["network_compression_method"],
        )
        if packets != data:
            raise ValueError(f"{name} parser did not consume all bytes")
        if public_packets(boundaries) != expected["packets"]:
            raise ValueError(f"{name} packet oracle mismatch")
        if len(native) != expected["decompressed_native_length"]:
            raise ValueError(f"{name} decompressed length mismatch")
        if hashlib.sha256(native).hexdigest() != expected["decompressed_native_sha256"]:
            raise ValueError(f"{name} decompressed hash mismatch")
    print(f"parsed {len(manifest['queries'])} compressed SELECT captures")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", choices=["127.0.0.1"], default="127.0.0.1")
    parser.add_argument("--http-port", type=int, default=8123)
    parser.add_argument("--tcp-port", type=int, default=9000)
    parser.add_argument(
        "--output",
        type=Path,
        default=Path(__file__).resolve().parents[1] / "tests/fixtures",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="Parse committed captures and verify their manifest without a server",
    )
    args = parser.parse_args()
    if args.check:
        check_fixtures(args.output)
        return

    os.environ["no_proxy"] = args.host
    os.environ["NO_PROXY"] = args.host
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    version = query.http_query(
        opener, args.host, args.http_port, "SELECT version()"
    ).strip()
    reference = (
        Path(__file__).resolve().parents[1] / ".server-ref"
    ).read_text().strip()
    if version != query.VERSION or reference != f"v{query.VERSION}-lts":
        raise ValueError(f"capture server/source pin mismatch: {version}/{reference}")

    captures, manifest = capture_all(args.host, args.tcp_port)
    args.output.mkdir(parents=True, exist_ok=True)
    for name, data in captures.items():
        (args.output / name).write_bytes(data)
        manifest["files"][name] = {
            "length": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
            "first_bytes": data[:16].hex(" "),
        }
        print(f"{name}: {len(data)} bytes")
    manifest_path = args.output / "tcp_compressed_query_rev54460.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    check_fixtures(args.output)


if __name__ == "__main__":
    main()
