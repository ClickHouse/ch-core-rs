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
import errno
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import socket
import struct
import sys
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gen_tcp_fixtures import Reader, string, varuint
from gen_tcp_handshake_fixtures import read_hello

REVISION = 54460
VERSION = "26.8.1.2041"
CLIENT_NAME = "ch-core-query-fixture"
CLIENT_VERSION_MAJOR = 13
CLIENT_VERSION_MINOR = 79
QUOTA_KEY = "fixture_quota_13"
INITIAL_ADDRESS = "192.0.2.13:79"
INITIAL_QUERY_START_TIME_MICROSECONDS = 1791500000000013
OS_USER = "fixture_os_user_13"
CLIENT_HOSTNAME = "fixture_host_79"
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
        "SELECT intDiv(toUInt64(79), number != 1000) AS n FROM numbers(2000) "
        "SETTINGS max_block_size=128, max_threads=1"
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
CAPTURE_SETTINGS = [
    ("send_profile_events", "1"),
    ("send_logs_level", "debug"),
    ("log_queries", "1"),
]


def client_hello():
    return (
        varuint(0) + string(CLIENT_NAME) + varuint(CLIENT_VERSION_MAJOR)
        + varuint(CLIENT_VERSION_MINOR)
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


def query_packet(query, query_id, parameters=(), settings=CAPTURE_SETTINGS):
    client_info = (
        b"\x01" + string("default") + string(query_id) + string(INITIAL_ADDRESS)
        + struct.pack("<Q", INITIAL_QUERY_START_TIME_MICROSECONDS)
        + b"\x01" + string(OS_USER) + string(CLIENT_HOSTNAME)
        + string(CLIENT_NAME) + varuint(CLIENT_VERSION_MAJOR)
        + varuint(CLIENT_VERSION_MINOR) + varuint(REVISION)
        + string(QUOTA_KEY) + varuint(0) + varuint(0) + b"\0"
        + varuint(0) + varuint(0) + varuint(0)
    )
    encoded_settings = (
        b"".join(
            string(name) + varuint(1) + string(value) for name, value in settings
        )
        + string("")
    )
    params = b"".join(
        string(name) + varuint(2) + string(dump_query_parameter(value))
        for name, value in parameters
    )
    return (
        varuint(1) + string(query_id) + client_info + encoded_settings + string("")
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
    bodies = []
    widths = {
        b"UInt8": 1, b"Int8": 1, b"UInt32": 4, b"Int32": 4,
        b"UInt64": 8, b"Int64": 8, b"DateTime": 4,
    }
    for _ in range(columns):
        name, kind = reader.string(), reader.string()
        schema.append([name.decode(), kind.decode()])
        if reader.read(1) != b"\0":
            raise ValueError("unexpected custom serialization")
        body_start = len(reader.record)
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
        bodies.append(bytes(reader.record[body_start:]))
    return rows, columns, schema, bodies, bytes(reader.record[start:])


def response(reader):
    packets, native, boundaries = bytearray(), bytearray(), []
    while True:
        reader.record.clear()
        code = reader.uint()
        entry = {"code": code, "offset": len(packets)}
        if code in (1, 7, 8, 10, 14):
            entry["name"] = reader.string().decode()
            block_offset = len(reader.record)
            rows, columns, schema, bodies, block = native_block(reader)
            entry.update(
                rows=rows, columns=columns, schema=schema,
                _column_bodies=[body.hex() for body in bodies],
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


def public_packets(packets):
    """Drop parser-only fields before writing the fixture manifest."""
    return [
        {key: value for key, value in packet.items() if not key.startswith("_")}
        for packet in packets
    ]


def http_query(opener, host, port, query):
    request = urllib.request.Request(
        f"http://{host}:{port}/", data=query.encode(), method="POST"
    )
    with opener.open(request, timeout=30) as source:
        return source.read().decode()


def normalized_ip_address(value):
    """Normalize IPv4-mapped IPv6 addresses to their IPv4 address."""
    address = ipaddress.ip_address(value)
    return getattr(address, "ipv4_mapped", None) or address


def assert_client_info_query_log(opener, host, port, query_id):
    """Verify sent ClientInfo fields through the server's independent log."""
    http_query(opener, host, port, "SYSTEM FLUSH LOGS")
    fields = [
        "query_id", "initial_query_id", "initial_user",
        "toString(initial_address) AS initial_address",
        "toString(initial_port) AS initial_port", "toString(interface) AS interface",
        "os_user", "client_hostname", "client_name",
        "toString(client_revision) AS client_revision",
        "toString(client_version_major) AS client_version_major",
        "toString(client_version_minor) AS client_version_minor",
        "toString(client_version_patch) AS client_version_patch", "quota_key",
        "toString(distributed_depth) AS distributed_depth",
        "toString(toUnixTimestamp64Micro(initial_query_start_time_microseconds)) "
        "AS initial_query_start_time_microseconds",
    ]
    sql = (
        f"SELECT {', '.join(fields)} FROM system.query_log "
        f"WHERE query_id = '{query_id}' AND type = 'QueryFinish' "
        "ORDER BY event_time_microseconds DESC LIMIT 1 FORMAT JSONEachRow"
    )
    rows = [json.loads(line) for line in http_query(opener, host, port, sql).splitlines()]
    if len(rows) != 1:
        raise ValueError(f"expected one query_log row for {query_id}, got {len(rows)}")
    row = rows[0]
    expected = {
        "query_id": query_id,
        "initial_query_id": query_id,
        "initial_user": "default",
        "interface": "1",
        "os_user": OS_USER,
        "client_hostname": CLIENT_HOSTNAME,
        "client_name": CLIENT_NAME,
        "client_revision": str(REVISION),
        "client_version_major": str(CLIENT_VERSION_MAJOR),
        "client_version_minor": str(CLIENT_VERSION_MINOR),
        "client_version_patch": "0",
        "quota_key": QUOTA_KEY,
        "distributed_depth": "0",
        "initial_query_start_time_microseconds": str(INITIAL_QUERY_START_TIME_MICROSECONDS),
    }
    mismatches = {key: (row.get(key), value) for key, value in expected.items()
                  if row.get(key) != value}
    # INITIAL_QUERY fields are deliberately not oracle sentinels: the server
    # replaces the supplied address with the real peer before logging it.
    sent_address = normalized_ip_address(INITIAL_ADDRESS.rsplit(":", 1)[0])
    try:
        logged_address = normalized_ip_address(row.get("initial_address", ""))
    except ValueError:
        mismatches["initial_address"] = (row.get("initial_address"), "valid peer address")
    else:
        if logged_address == sent_address:
            mismatches["initial_address"] = (
                row.get("initial_address"), "real peer address"
            )
    if int(row.get("initial_port", "0")) <= 0:
        mismatches["initial_port"] = (row.get("initial_port"), "positive peer port")
    if mismatches:
        raise ValueError(f"ClientInfo query_log mismatch: {mismatches}")
    return {
        **expected,
        "initial_address_overwritten": True,
        "initial_port_positive": True,
    }


def assert_connection_closed(sock):
    """Require EOF, treating reset and broken pipe as closed transport."""
    try:
        sock.sendall(varuint(4))
        sock.settimeout(5)
        closed = not sock.recv(1)
    except OSError as error:
        if error.errno not in (errno.ECONNRESET, errno.EPIPE):
            raise
        closed = True
    if not closed:
        raise ValueError("bad-setting socket remained readable after Exception")


def assert_exception_lifecycle(host, port):
    """Live-probe the terminal settings error and reusable SQL error split."""
    bad = query_packet(
        "SELECT 13", "ch-core-fixture-bad-setting",
        settings=[("max_threads", "abc")],
    )
    with socket.create_connection((host, port), timeout=30) as sock:
        sock.sendall(client_hello())
        with sock.makefile("rb") as source:
            reader = Reader(source)
            read_hello(reader)
            sock.sendall(string(QUOTA_KEY) + bad)
            _, _, packets = response(reader)
            if len(packets) != 1 or packets[0].get("exception_code") != 27:
                raise ValueError(f"expected only code 27 bad-setting Exception: {packets}")
            assert_connection_closed(sock)

    syntax = query_packet("SELEC 13", "ch-core-fixture-syntax-error", settings=[])
    recovery = query_packet("SELECT 13", "ch-core-fixture-syntax-recovery", settings=[])
    with socket.create_connection((host, port), timeout=30) as sock:
        sock.sendall(client_hello())
        with sock.makefile("rb") as source:
            reader = Reader(source)
            read_hello(reader)
            sock.sendall(string(QUOTA_KEY) + syntax)
            _, _, packets = response(reader)
            if len(packets) != 1 or packets[0].get("exception_code") != 62:
                raise ValueError(f"expected only code 62 syntax Exception: {packets}")
            sock.sendall(recovery)
            _, _, packets = response(reader)
            if packets[-1]["code"] != 5:
                raise ValueError(f"syntax-error socket was not reusable: {packets[-1]}")
    return {
        "bad_known_setting_code": 27,
        "bad_known_setting_connection_closed": True,
        "syntax_error_code": 62,
        "syntax_error_connection_reused": True,
    }


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
    version = http_query(opener, args.host, args.http_port, "SELECT version()").strip()
    reference = (Path(__file__).resolve().parents[1] / ".server-ref").read_text().strip()
    if version != VERSION or reference != f"v{VERSION}-lts":
        raise ValueError(f"capture server/source pin mismatch: {version}/{reference}")
    captures = {}
    manifest = {
        "captured_on": date.today().isoformat(), "server_version": version,
        "source_tag": reference, "declared_revision": REVISION,
        "client_name": CLIENT_NAME, "user": "default", "password": "",
        "client_info": {
            "version_major": CLIENT_VERSION_MAJOR,
            "version_minor": CLIENT_VERSION_MINOR,
            "quota_key": QUOTA_KEY,
            "initial_address": INITIAL_ADDRESS,
            "initial_query_start_time_microseconds": INITIAL_QUERY_START_TIME_MICROSECONDS,
            "os_user": OS_USER,
            "client_hostname": CLIENT_HOSTNAME,
        },
        "settings": [
            ["send_profile_events", 1, "1"], ["send_logs_level", 1, "debug"],
            ["log_queries", 1, "1"],
        ],
        "queries": {}, "files": {},
    }
    manifest["exception_lifecycle_probe"] = assert_exception_lifecycle(
        args.host, args.tcp_port
    )
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
                        raise ValueError(
                            f"exception arrived before positive-row Data: {boundaries}"
                        )
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
                        "packets": public_packets(recovered),
                    }
                elif boundaries[-1]["code"] != 5:
                    raise ValueError(f"query {label} failed: {boundaries[-1]}")
                # Check exact parsed column bodies. This cannot accidentally
                # match a value substring or the next column's length byte.
                result_blocks = [
                    p for p in boundaries if p["code"] == 1 and p.get("rows") == 1
                ]
                if label == "escaped_parameter":
                    expected = [string(ESCAPED_PARAMETER)]
                    actual = [
                        bytes.fromhex(body) for body in result_blocks[-1]["_column_bodies"]
                    ]
                    if actual != expected:
                        raise ValueError(f"query {label} column bodies mismatch")
                elif label == "container_parameter":
                    expected = [
                        struct.pack("<Q", 2) + string("user_1") + string("it's"),
                        b"\x01" + string(""),
                    ]
                    actual = [
                        bytes.fromhex(body) for body in result_blocks[-1]["_column_bodies"]
                    ]
                    if actual != expected:
                        raise ValueError(f"query {label} column bodies mismatch")
        captures[f"tcp_query_{label}_client_rev54460.packet"] = request
        captures[f"tcp_query_{label}_server_rev54460.packets"] = packets
        captures[f"tcp_query_{label}_rev54460.native"] = native
        manifest["queries"][label] = {
            "sql": sql, "query_id": query_id, "parameters": parameters,
            "packets": public_packets(boundaries),
        }
    all_codes = {p["code"] for q in manifest["queries"].values() for p in q["packets"]}
    if not {1, 2, 3, 5, 6, 7, 8, 10, 14}.issubset(all_codes):
        raise ValueError(f"missing packet coverage: {all_codes}")
    manifest["client_info_query_log"] = assert_client_info_query_log(
        opener, args.host, args.http_port, "ch-core-fixture-select"
    )
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
