#!/usr/bin/env python3
"""Capture full uncompressed SELECT exchanges at TCP revision 54460.

Only Python's standard library, loopback, and empty passwords are used.
The fixture client parses packet boundaries rather than searching byte patterns.
Layouts confirmed at v26.8.1.2041-lts in Connection::sendQuery,
ClientInfo::write, TCPHandler::send*, ProgressValues::write, ProfileInfo::write,
and NativeWriter::write. See tests/fixtures/README.md.
"""

import argparse
from datetime import date
import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import sys
import urllib.parse
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gen_tcp_fixtures import Reader, string, varuint
from gen_tcp_handshake_fixtures import read_hello

REVISION = 54460
VERSION = "26.8.1.2041"
CLIENT_NAME = "ch-core-query-fixture"
EMPTY_BLOCK = bytes.fromhex("010002ffffffff000000")
QUERIES = {
    "select": "SELECT 1",
    "multi": (
        "SELECT number + 13 AS n, concat('user_', toString(number)) AS s "
        "FROM numbers(5) SETTINGS max_block_size=2, max_threads=1"
    ),
    # A blockwise expression prevents the exception from being folded before
    # earlier blocks are sent. The capture asserts positive-row Data precedes it.
    "exception": (
        "SELECT intDiv(toUInt64(79), number != 4) AS n FROM numbers(8) "
        "SETTINGS max_block_size=2, max_threads=1"
    ),
    "totals": (
        "SELECT number % 2 AS k, count() AS n FROM numbers(5) "
        "GROUP BY k WITH TOTALS ORDER BY k SETTINGS max_threads=1, extremes=1"
    ),
    "parameter": "SELECT {value:String} AS s",
    "escaped_parameter": "SELECT {value:String} AS s",
    # Container text and \N must reach the server verbatim after one quoting
    # layer; a second layer breaks both (Array parse error, NULL -> "\N").
    "container_parameter": "SELECT {items:Array(String)} AS a, {missing:Nullable(String)} AS n",
}
# Parameter values are HTTP `param_` text, the server's escaped text form.
# ESCAPED_PARAMETER_TEXT is what the escaped_parameter capture sends, and
# ESCAPED_PARAMETER is the String value the server returns for it.
ESCAPED_PARAMETER_TEXT = r"quote\' slash\\ nul\0 newline\n carriage\r tab\t backspace\b formfeed\f"
ESCAPED_PARAMETER = "quote' slash\\ nul\0 newline\n carriage\r tab\t backspace\b formfeed\f"
PARAMETERS = {
    "parameter": [("value", "user_1")],
    "escaped_parameter": [("value", ESCAPED_PARAMETER_TEXT)],
    "container_parameter": [("items", "['user_1','it\\'s']"), ("missing", "\\N")],
}


def client_hello():
    return (
        varuint(0) + string(CLIENT_NAME) + varuint(26) + varuint(8)
        + varuint(REVISION) + string("") + string("default") + string("")
    )


def dump_field_string(value):
    """Encode a String Field in the query-parameter text format."""
    escapes = {
        ord("'"): b"\\'", ord("\\"): b"\\\\", 0: b"\\0", 10: b"\\n",
        13: b"\\r", 9: b"\\t", 8: b"\\b", 12: b"\\f",
    }
    return b"'" + b"".join(escapes.get(byte, bytes([byte]))
                         for byte in value.encode()) + b"'"


def dump_query_parameter(text):
    """Apply the single String Field layer that readQueryParameters undoes.

    At v26.8.1.2041-lts, readQueryParameters (src/Core/Settings.cpp) runs one
    readQuoted per value, the inverse of writeQueryParameters. The result is
    parsed exactly like an HTTP `param_` value.
    """
    return dump_field_string(text)


def query_packet(query, query_id, parameters=()):
    client_info = (
        b"\x01" + string("default") + string(query_id) + string("127.0.0.1:0")
        + struct.pack("<Q", 0) + b"\x01" + string("") + string("")
        + string(CLIENT_NAME) + varuint(26) + varuint(8) + varuint(REVISION)
        + string("") + varuint(0) + varuint(0) + b"\0"
        + varuint(0) + varuint(0) + varuint(0)
    )
    settings = (
        string("send_profile_events") + varuint(1) + string("1")
        + string("send_logs_level") + varuint(1) + string("debug") + string("")
    )
    params = b"".join(
        string(name) + varuint(2) + string(dump_query_parameter(value))
        for name, value in parameters
    )
    return (
        varuint(1) + string(query_id) + client_info + settings + string("")
        + varuint(2) + varuint(0) + string(query) + params + string("")
        + varuint(2) + string("") + EMPTY_BLOCK
    )


def native_block(reader):
    """Read only the fixed-width/String schemas used by these captures."""
    start = len(reader.record)
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
    schema = []
    widths = {
        b"UInt8": 1, b"Int8": 1, b"UInt32": 4, b"Int32": 4,
        b"UInt64": 8, b"Int64": 8, b"DateTime": 4,
    }
    for _ in range(columns):
        name, kind = reader.string(), reader.string()
        schema.append([name.decode(), kind.decode()])
        if reader.read(1) != b"\0":
            raise ValueError("unexpected custom serialization")
        if kind == b"String":
            for _ in range(rows):
                reader.string()
        elif kind == b"Nullable(String)":
            reader.read(rows)
            for _ in range(rows):
                reader.string()
        elif kind == b"Array(String)":
            offsets = struct.unpack(f"<{rows}Q", reader.read(rows * 8))
            for _ in range(offsets[-1] if offsets else 0):
                reader.string()
        elif kind.startswith(b"Enum8("):
            reader.read(rows)
        elif kind in widths:
            reader.read(rows * widths[kind])
        else:
            raise ValueError(f"unexpected fixture type {kind!r}")
    return rows, columns, schema, bytes(reader.record[start:])


def response(reader):
    packets, native, boundaries = bytearray(), bytearray(), []
    while True:
        reader.record.clear()
        code = reader.uint()
        entry = {"code": code, "offset": len(packets)}
        if code in (1, 7, 8, 10, 14):
            entry["name"] = reader.string().decode()
            block_offset = len(reader.record)
            rows, columns, schema, block = native_block(reader)
            entry.update(
                rows=rows, columns=columns, schema=schema,
                block_offset=entry["offset"] + block_offset,
                block_length=len(block),
            )
            # Zero-column, zero-row Data is the TCP end-of-data marker,
            # rather than a block in the result schema's Native stream.
            if code == 1 and (columns or rows):
                native.extend(block)
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


def rust_string(value):
    return json.dumps(value, ensure_ascii=False)


def write_rust_oracle(output, queries):
    """Write a dependency-free Rust oracle from independently parsed packets."""
    lines = [
        "// Generated by scripts/gen_tcp_query_fixtures.py. Do not edit.",
        "const ORACLES: &[CaptureOracle] = &[",
    ]
    block_names = {
        1: "Data", 7: "Totals", 8: "Extremes", 10: "Log", 14: "ProfileEvents",
    }
    for label, query in queries.items():
        lines.append("    CaptureOracle {")
        lines.append(f"        label: {rust_string(label)},")
        lines.append("        events: &[")
        for packet in query["packets"]:
            code = packet["code"]
            if code == 1 and packet["rows"] == 0 and packet["columns"] == 0:
                continue
            if code in block_names:
                lines.append(
                    "            ExpectedEvent::Block { "
                    f"kind: BlockKind::{block_names[code]}, "
                    f"packet_offset: {packet['offset']}, "
                    f"offset: {packet['block_offset']}, "
                    f"length: {packet['block_length']} }},"
                )
            elif code == 3:
                values = ", ".join(str(value) for value in packet["progress"])
                lines.append(f"            ExpectedEvent::Progress([{values}]),")
            elif code == 6:
                values = ", ".join(str(value) for value in packet["profile"])
                lines.append(
                    "            ExpectedEvent::ProfileInfo { "
                    f"values: [{values}], applied_limit: "
                    f"{str(bool(packet['applied_limit'])).lower()}, "
                    f"rows_before_limit: {packet['rows_before_limit']}, "
                    f"obsolete: {str(bool(packet['obsolete'])).lower()} }},"
                )
            elif code == 2:
                lines.append(
                    "            ExpectedEvent::Exception { "
                    f"code: {packet['exception_code']}, "
                    f"name: {rust_string(packet['exception_name'])}, "
                    f"message: {rust_string(packet['message'])}, "
                    "has_nested: "
                    f"{str(packet['has_nested']).lower()} }},"
                )
            elif code == 5:
                lines.append("            ExpectedEvent::EndOfStream,")
            else:
                raise ValueError(f"no Rust oracle mapping for packet {code}")
        lines.extend(["        ],", "    },"])
    lines.extend(["];", ""])
    (output / "tcp_query_oracle_rev54460.rs").write_text("\n".join(lines))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", choices=["127.0.0.1"], default="127.0.0.1")
    parser.add_argument("--http-port", type=int, default=8123)
    parser.add_argument("--tcp-port", type=int, default=9000)
    parser.add_argument("--output", type=Path,
                        default=Path(__file__).resolve().parents[1] / "tests/fixtures")
    args = parser.parse_args()
    os.environ["no_proxy"] = args.host
    os.environ["NO_PROXY"] = args.host
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    params = urllib.parse.urlencode({"query": "SELECT version()"})
    with opener.open(f"http://{args.host}:{args.http_port}/?{params}", timeout=30) as source:
        version = source.read().decode().strip()
    reference = (Path(__file__).resolve().parents[1] / ".server-ref").read_text().strip()
    if version != VERSION or reference != f"v{VERSION}-lts":
        raise ValueError(f"capture server/source pin mismatch: {version}/{reference}")
    captures = {}
    manifest = {
        "captured_on": date.today().isoformat(), "server_version": version,
        "source_tag": reference, "declared_revision": REVISION,
        "client_name": CLIENT_NAME, "user": "default", "password": "",
        "settings": [
            ["send_profile_events", 1, "1"], ["send_logs_level", 1, "debug"],
        ],
        "queries": {}, "files": {},
    }
    for label, sql in QUERIES.items():
        query_id = f"ch-core-fixture-{label}"
        parameters = PARAMETERS.get(label, [])
        request = query_packet(sql, query_id, parameters)
        with socket.create_connection((args.host, args.tcp_port), timeout=30) as sock:
            sock.sendall(client_hello())
            with sock.makefile("rb") as source:
                reader = Reader(source)
                read_hello(reader)
                sock.sendall(string("") + request)
                packets, native, boundaries = response(reader)
                if label == "exception":
                    if boundaries[-1]["code"] != 2:
                        raise ValueError("expected mid-stream exception")
                    if not any(p["code"] == 1 and p["rows"] > 0 for p in boundaries):
                        raise ValueError("exception arrived before positive-row Data")
                    # Confirm a query-time Exception does not require EOS and the
                    # same transport accepts another query immediately.
                    recovery = query_packet("SELECT 1", "ch-core-fixture-recovery")
                    sock.sendall(recovery)
                    reply, body, recovered = response(reader)
                    if recovered[-1]["code"] != 5:
                        raise ValueError("connection was not reusable after exception")
                    captures["tcp_query_recovery_client_rev54460.packet"] = recovery
                    captures["tcp_query_recovery_server_rev54460.packets"] = reply
                    captures["tcp_query_recovery_rev54460.native"] = body
                    manifest["queries"]["recovery"] = {
                        "sql": "SELECT 1", "query_id": "ch-core-fixture-recovery",
                        "packets": recovered,
                    }
                elif boundaries[-1]["code"] != 5:
                    raise ValueError(f"query {label} failed: {boundaries[-1]}")
                # Spot-check the parameter contract on the decoded payload.
                expected = {
                    "escaped_parameter": [ESCAPED_PARAMETER.encode()],
                    # One Array row of two strings, then a 1 null-mask byte.
                    "container_parameter": [
                        struct.pack("<Q", 2) + string("user_1") + string("it's")
                        + b"\x01",
                    ],
                }.get(label, [])
                if any(part not in native for part in expected):
                    raise ValueError(f"query {label} parameter value mismatch")
        captures[f"tcp_query_{label}_client_rev54460.packet"] = request
        captures[f"tcp_query_{label}_server_rev54460.packets"] = packets
        captures[f"tcp_query_{label}_rev54460.native"] = native
        manifest["queries"][label] = {
            "sql": sql, "query_id": query_id, "parameters": parameters,
            "packets": boundaries,
        }
    all_codes = {p["code"] for q in manifest["queries"].values() for p in q["packets"]}
    if not {1, 2, 3, 5, 6, 7, 8, 10, 14}.issubset(all_codes):
        raise ValueError(f"missing packet coverage: {all_codes}")
    args.output.mkdir(parents=True, exist_ok=True)
    for name, data in captures.items():
        (args.output / name).write_bytes(data)
        manifest["files"][name] = {
            "length": len(data), "sha256": hashlib.sha256(data).hexdigest(),
            "first_bytes": data[:16].hex(" "),
        }
        print(f"{name}: {len(data)} bytes")
    (args.output / "tcp_query_rev54460.json").write_text(json.dumps(manifest, indent=2) + "\n")
    write_rust_oracle(args.output, manifest["queries"])


if __name__ == "__main__":
    main()
