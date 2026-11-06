#!/usr/bin/env python3
"""Capture revision-54460 HTTP Native streams and raw TCP Data packets.

Uses only Python's standard library. This is a deliberately narrow fixture
client, not a general ClickHouse driver. See tests/fixtures/README.md.
"""

import argparse
import io
import json
from pathlib import Path
import socket
import struct
import urllib.parse
import urllib.request

REVISION = 54460
VERSION = "26.8.1.2041"
SMALL_QUERY = (
    "SELECT number + 13 AS n, concat('user_', toString(number)) AS s "
    "FROM numbers(5) SETTINGS max_block_size=2, max_threads=1"
)
LARGE_QUERY = "SELECT toUInt64(79) AS n, repeat('xx', 550000) AS s"
# NONE frame for BlockInfo + zero columns + zero rows at 54460. The checksum
# is CityHash128 1.0.2 of the 9-byte header and ten plaintext bytes. Keeping
# this fixed handshake marker here avoids a Python compression dependency.
EMPTY_COMPRESSED = bytes.fromhex(
    "bb9489a1c169e900ecaff168956a6474"
    "02130000000a000000010002ffffffff000000"
)


def varuint(value):
    result = bytearray()
    while value >= 128:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def string(value):
    value = value.encode() if isinstance(value, str) else value
    return varuint(len(value)) + value


class Reader:
    def __init__(self, source):
        self.source = source
        self.record = bytearray()

    def read(self, size):
        data = self.source.read(size)
        if len(data) != size:
            raise EOFError(f"wanted {size} bytes, got {len(data)}")
        self.record.extend(data)
        return data

    def uint(self):
        value = 0
        for shift in range(0, 70, 7):
            byte = self.read(1)[0]
            value |= (byte & 127) << shift
            if byte < 128:
                return value
        raise ValueError("oversized varuint")

    def string(self):
        return self.read(self.uint())


def lz4(payload, expected):
    """Decode only to find captured Native block boundaries, not for production."""
    reader = Reader(io.BytesIO(payload))
    output = bytearray()
    while len(reader.record) < len(payload):
        token = reader.read(1)[0]
        length = token >> 4
        if length == 15:
            while True:
                extra = reader.read(1)[0]
                length += extra
                if extra != 255:
                    break
        output.extend(reader.read(length))
        if len(reader.record) == len(payload):
            break
        offset = int.from_bytes(reader.read(2), "little")
        if not offset or offset > len(output):
            raise ValueError("invalid LZ4 offset")
        length = (token & 15) + 4
        if (token & 15) == 15:
            while True:
                extra = reader.read(1)[0]
                length += extra
                if extra != 255:
                    break
        if len(output) + length > expected:
            raise ValueError("LZ4 output exceeds frame size")
        for _ in range(length):
            output.append(output[-offset])
    if len(output) != expected:
        raise ValueError("LZ4 size mismatch")
    return output


def native_block(reader):
    """Read the UInt64/String schema used by these two queries at 54460."""
    while True:
        field = reader.uint()
        if field == 0:
            break
        if field == 1:
            reader.read(1)
        elif field == 2:
            reader.read(4)
        else:
            raise ValueError(f"unexpected BlockInfo field {field}")
    columns, rows = reader.uint(), reader.uint()
    for _ in range(columns):
        reader.string()
        kind = reader.string()
        if reader.read(1) != b"\0":
            raise ValueError("unexpected custom serialization")
        if kind == b"UInt64":
            reader.read(8 * rows)
        elif kind == b"String":
            for _ in range(rows):
                reader.string()
        else:
            raise ValueError(f"unexpected fixture type {kind!r}")
    return rows, columns


def compressed_block(reader):
    plain = bytearray()
    frames = []
    while True:
        reader.read(16)  # Checksums are verified by the Rust fixture tests.
        header = reader.read(9)
        method, compressed_size, plain_size = struct.unpack("<BII", header)
        if not 9 <= compressed_size <= 0x40000000 or not 0 < plain_size <= 0x40000000:
            raise ValueError("invalid compressed frame sizes")
        payload = reader.read(compressed_size - 9)
        if method == 0x82:
            plain.extend(lz4(payload, plain_size))
        elif method == 0x02:
            if len(payload) != plain_size:
                raise ValueError("NONE size mismatch")
            plain.extend(payload)
        else:
            raise ValueError(f"unexpected compression method {method}")
        frames.append(plain_size)
        block = Reader(io.BytesIO(plain))
        try:
            rows, columns = native_block(block)
        except EOFError:
            continue
        if len(block.record) != len(plain):
            raise ValueError("compressed frame extends past Native block")
        return rows, columns, frames


def http(opener, host, port, query, **settings):
    params = urllib.parse.urlencode({"query": query, **settings})
    with opener.open(f"http://{host}:{port}/?{params}", timeout=30) as response:
        return response.read()


def tcp_capture(host, port, query, compressed):
    with socket.create_connection((host, port), timeout=30) as sock:
        sock.sendall(
            varuint(0) + string("ch-core-fixture") + varuint(26) + varuint(8)
            + varuint(REVISION) + string("") + string("default") + string("")
        )
        reader = Reader(sock.makefile("rb"))
        if reader.uint() != 0:
            raise ValueError("expected Server Hello")
        name = reader.string().decode()
        major, minor, revision = reader.uint(), reader.uint(), reader.uint()
        timezone, display, patch = reader.string(), reader.string(), reader.uint()
        if (major, minor, patch) != (26, 8, 1) or revision != 54492:
            raise ValueError(f"unexpected TCP server version: {major}.{minor}.{patch}/{revision}")
        sock.sendall(string(""))  # Addendum quota_key.
        client_info = (
            b"\x01" + string("default") + string("") + string("127.0.0.1:0")
            + struct.pack("<Q", 0) + b"\x01"
            + string("") + string("fixture") + string("ch-core-fixture")
            + varuint(26) + varuint(8) + varuint(REVISION)
            + string("") + varuint(0) + varuint(1) + b"\0"
            + varuint(0) + varuint(0) + varuint(0)
        )
        settings = (
            string("send_profile_events") + varuint(0) + string("0")
            + string("send_logs_level") + varuint(0) + string("none") + string("")
        )
        sock.sendall(
            varuint(1) + string("") + client_info + settings + string("")
            + varuint(2) + varuint(int(compressed)) + string(query) + string("")
        )
        # Query ends its external-table input with an empty Data block. This
        # first marker can use the NONE method in a compressed connection.
        empty = bytes.fromhex("010002ffffffff000000")
        if compressed:
            empty = EMPTY_COMPRESSED
        sock.sendall(varuint(2) + string("") + empty)
        packets, boundaries = bytearray(), []
        while True:
            reader.record.clear()
            code = reader.uint()
            if code == 1:
                block_name = reader.string()
                if compressed:
                    rows, columns, frames = compressed_block(reader)
                else:
                    rows, columns = native_block(reader)
                    frames = []
                offset = len(packets)
                packets.extend(reader.record)
                boundaries.append({
                    "offset": offset, "length": len(reader.record), "rows": rows,
                    "columns": columns, "name": block_name.decode(), "frame_plain_sizes": frames,
                })
            elif code == 3:
                for _ in range(6):
                    reader.uint()
            elif code == 6:
                for _ in range(3):
                    reader.uint()
                reader.read(1)
                reader.uint()
                reader.read(1)
            elif code == 5:
                break
            elif code == 2:
                raise ValueError(
                    f"server exception {int.from_bytes(reader.read(4), 'little')}: "
                    f"{reader.string().decode()}: {reader.string().decode()}"
                )
            else:
                raise ValueError(f"unexpected server packet {code}")
        return bytes(packets), boundaries


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--http-port", type=int, default=8123)
    parser.add_argument("--tcp-port", type=int, default=9000)
    parser.add_argument("--output", type=Path, default=Path(__file__).resolve().parents[1] / "tests/fixtures")
    args = parser.parse_args()
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    version = http(opener, args.host, args.http_port, "SELECT version()").decode().strip()
    if version != VERSION:
        raise ValueError(f"expected server {VERSION}, got {version}")
    reference = (Path(__file__).resolve().parents[1] / ".server-ref").read_text().strip()
    if reference != f"v{VERSION}-lts":
        raise ValueError(f"source pin does not match capture server: {reference}")
    args.output.mkdir(parents=True, exist_ok=True)
    manifest = {"server_version": version, "source_tag": reference, "protocol_revision": REVISION,
                "small_query": SMALL_QUERY, "large_query": LARGE_QUERY, "files": {}}
    for compressed in (False, True):
        suffix = "_lz4" if compressed else ""
        name = f"http_multi_rev54460{suffix}.native"
        data = http(opener, args.host, args.http_port, SMALL_QUERY + " FORMAT Native",
                    client_protocol_version=REVISION, compress=int(compressed))
        (args.output / name).write_bytes(data)
        manifest["files"][name] = {"length": len(data)}
        name = f"tcp_multi_rev54460{suffix}.packets"
        data, boundaries = tcp_capture(args.host, args.tcp_port, SMALL_QUERY, compressed)
        (args.output / name).write_bytes(data)
        manifest["files"][name] = {"length": len(data), "packets": boundaries}
    name = "tcp_large_rev54460_lz4.packets"
    data, boundaries = tcp_capture(args.host, args.tcp_port, LARGE_QUERY, True)
    (args.output / name).write_bytes(data)
    manifest["files"][name] = {"length": len(data), "packets": boundaries}
    (args.output / "tcp_rev54460.json").write_text(json.dumps(manifest, indent=2) + "\n")
    for name, entry in manifest["files"].items():
        print(f"{name}: {entry['length']} bytes")


if __name__ == "__main__":
    main()
