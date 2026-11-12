#!/usr/bin/env python3
"""Capture uncompressed revision-54460 INSERT exchanges on pinned loopback server.

Independent Python packet parsing supplies the replay oracle. No third-party
packages are used and every successful insert is checked independently by HTTP.
"""

import argparse
import io
from datetime import date
import hashlib
import json
import os
from pathlib import Path
import select
import socket
import struct
import sys
import urllib.parse
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gen_tcp_query_fixtures as query
from gen_tcp_fixtures import Reader, string, varuint
from gen_tcp_handshake_fixtures import read_hello

TABLE = "ch_core_tcp_insert_fixture"
ORACLE = "tcp_insert_oracle_rev54460.rs"
FINISH = varuint(2) + string("") + query.EMPTY_BLOCK
REVISION = 54460
VERSION = "26.8.1.2041"
CLIENT_NAME = "ch-core-insert-fixture"
CLIENT_VERSION_MAJOR = 26
CLIENT_VERSION_MINOR = 8
INITIAL_ADDRESS = "127.0.0.1:0"
CAPTURE_SETTINGS = [
    ("send_profile_events", "1"),
    ("send_logs_level", "debug"),
]


def client_hello():
    return (
        varuint(0) + string(CLIENT_NAME) + varuint(CLIENT_VERSION_MAJOR)
        + varuint(CLIENT_VERSION_MINOR) + varuint(REVISION)
        + string("") + string("default") + string("")
    )


def query_packet(sql, query_id):
    """Build the stable INSERT fixture Query independently of SELECT fixtures."""
    client_info = (
        b"\x01" + string("default") + string(query_id) + string(INITIAL_ADDRESS)
        + struct.pack("<Q", 0) + b"\x01" + string("") + string("")
        + string(CLIENT_NAME) + varuint(CLIENT_VERSION_MAJOR)
        + varuint(CLIENT_VERSION_MINOR) + varuint(REVISION)
        + string("") + varuint(0) + varuint(0) + b"\0"
        + varuint(0) + varuint(0) + varuint(0)
    )
    settings = (
        b"".join(string(name) + varuint(1) + string(value)
                 for name, value in CAPTURE_SETTINGS)
        + string("")
    )
    return (
        varuint(1) + string(query_id) + client_info + settings + string("")
        + varuint(2) + varuint(0) + string(sql) + string("")
        + varuint(2) + string("") + query.EMPTY_BLOCK
    )


def block(rows):
    result = bytes.fromhex("010002ffffffff00") + varuint(2) + varuint(len(rows))
    result += string("n") + string("UInt64") + b"\0"
    result += b"".join(struct.pack("<Q", n) for n, _ in rows)
    result += string("s") + string("String") + b"\0"
    result += b"".join(string(s) for _, s in rows)
    return result


def response(reader, schema_only=False, one_packet=False):
    """Parse packet boundaries, stopping at schema or terminal packet."""
    packets, boundaries = bytearray(), []
    while True:
        reader.record.clear()
        code = reader.uint()
        entry = {"code": code, "offset": len(packets)}
        if code in (1, 10, 14):
            entry["name"] = reader.string().decode()
            offset = len(reader.record)
            rows, columns, schema, _bodies, body = query.native_block(reader)
            entry.update(rows=rows, columns=columns, schema=schema,
                         block_offset=entry["offset"] + offset,
                         block_length=len(body))
        elif code == 11:
            # TableColumns fields are raw bytes on the wire (writeStringBinary)
            # and may hold non-UTF-8 literals. The hex keys are lossless and
            # authoritative; the text keys are lossy, for readability only.
            for key in ("external_table", "columns_description"):
                raw = reader.string()
                entry[key] = raw.decode(errors="replace")
                entry[key + "_hex"] = raw.hex()
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
        if code in (2, 5) or (schema_only and code == 1) or one_packet:
            return bytes(packets), boundaries


def rust_bytes(raw):
    """Render bytes as a Rust byte-string literal, hex-escaping the rest."""
    escapes = {0x22: '\\"', 0x5C: "\\\\", 0x0A: "\\n", 0x0D: "\\r", 0x09: "\\t"}
    return 'b"' + "".join(
        escapes.get(b) or (chr(b) if 0x20 <= b < 0x7F else f"\\x{b:02x}")
        for b in raw
    ) + '"'


def rust_str(text):
    """Render text as a Rust string literal; non-ASCII uses \\u{...}."""
    escapes = {'"': '\\"', "\\": "\\\\", "\n": "\\n", "\r": "\\r", "\t": "\\t"}
    return '"' + "".join(
        escapes.get(c) or (c if " " <= c < "\x7f" else
                           f"\\x{ord(c):02x}" if ord(c) < 0x80 else f"\\u{{{ord(c):x}}}")
        for c in text
    ) + '"'


def render_oracle(captures):
    lines = ["// Generated by scripts/gen_tcp_insert_fixtures.py. Do not edit.",
             "const ORACLES: &[CaptureOracle] = &["]
    for label, capture in captures.items():
        lines += [f'    CaptureOracle {{ label: "{label}", sql: {rust_str(capture["sql"])},', "        schema: &["]
        for stage in ("schema", "midupload", "completion"):
            if stage != "schema":
                lines += [f"        ], {stage}: &["]
            for p in capture.get(stage + "_packets", []):
                code = p["code"]
                if code in (1, 10, 14):
                    kind = {1: "InsertSchema", 10: "Log", 14: "ProfileEvents"}[code]
                    lines.append(f"            Expected::Block({code}, {p['block_offset']}, {p['block_length']}), // {kind}")
                elif code == 11:
                    lines.append(
                        "            Expected::TableColumns("
                        f"{rust_bytes(bytes.fromhex(p['external_table_hex']))}, "
                        f"{rust_bytes(bytes.fromhex(p['columns_description_hex']))}),"
                    )
                elif code == 3:
                    lines.append(f"            Expected::Progress({p['progress']}),")
                elif code == 6:
                    lines.append(f"            Expected::ProfileInfo({p['profile']}, {str(bool(p['applied_limit'])).lower()}, {p['rows_before_limit']}, {str(bool(p['obsolete'])).lower()}),")
                elif code == 2:
                    lines.append(f"            Expected::Exception({p['exception_code']}, {rust_str(p['exception_name'])}, {rust_str(p['message'])}, {str(p['has_nested']).lower()}),")
                elif code == 5:
                    lines.append("            Expected::EndOfStream,")
                else:
                    raise ValueError(f"no oracle mapping {code}")
        lines += ["        ],", "    },"]
    lines += ["];", ""]
    return "\n".join(lines)


def write_oracle(output, captures):
    (output / ORACLE).write_text(render_oracle(captures))


def check_fixtures(output):
    """Parse every committed server stage with the independent parser."""
    manifest = json.loads((output / "tcp_insert_rev54460.json").read_text())
    for label, capture in manifest["queries"].items():
        request = (output / f"tcp_insert_{label}_request_rev54460.bytes").read_bytes()
        regenerated = query_packet(capture["sql"], capture["query_id"])
        if request != regenerated:
            raise ValueError(f"{label}/request: INSERT packet builder changed")
        for stage in ("schema", "midupload", "completion"):
            data = (output / f"tcp_insert_{label}_{stage}_rev54460.bytes").read_bytes()
            expected = capture[f"{stage}_packets"]
            reader = Reader(io.BytesIO(data))
            parsed = []
            for _ in expected:
                _, packets = response(reader, one_packet=True)
                parsed.extend(packets)
            if reader.source.read(1) or len(parsed) != len(expected):
                raise ValueError(f"{label}/{stage}: packet count or trailing bytes")
            semantic_keys = (
                "code", "length", "name", "rows", "columns", "schema",
                "external_table", "external_table_hex", "columns_description",
                "columns_description_hex", "progress", "profile",
                "applied_limit", "rows_before_limit", "obsolete",
                "exception_code", "exception_name", "message", "has_nested",
            )
            for actual, oracle in zip(parsed, expected):
                for key in semantic_keys:
                    if actual.get(key) != oracle.get(key):
                        raise ValueError(
                            f"{label}/{stage}: {key} changed: "
                            f"{actual.get(key)!r} != {oracle.get(key)!r}"
                        )
    if (output / ORACLE).read_text() != render_oracle(manifest["queries"]):
        raise ValueError(f"{ORACLE} does not match the manifest; regenerate it")
    print(f"parsed {len(manifest['queries'])} committed INSERT captures; oracle matches")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", choices=["127.0.0.1"], default="127.0.0.1")
    parser.add_argument("--http-port", type=int, default=8123)
    parser.add_argument("--tcp-port", type=int, default=9000)
    parser.add_argument("--cases", nargs="*", help="Capture selected cases and preserve other fixtures")
    parser.add_argument("--output", type=Path, default=Path(__file__).resolve().parents[1] / "tests/fixtures")
    parser.add_argument("--check", action="store_true",
                        help="Parse committed response bytes without contacting a server")
    args = parser.parse_args()
    if args.check:
        check_fixtures(args.output)
        return
    cases = {
        "multi": [[(13, "user_1"), (79, "user_2")], [(17, "")]],
        "empty": [], "zero_rows": [[]], "before_schema": [],
        "completion_exception": [[(179, "rejected")]],
        "midupload": [[(13, "user_1")], [(79, "user_2")]],
        "inline_values": [], "insert_select": [], "inline_async": [],
    }
    if args.cases:
        unknown = sorted(set(args.cases) - set(cases))
        if unknown:
            raise ValueError(f"unknown cases: {', '.join(unknown)}")
    os.environ["no_proxy"] = os.environ["NO_PROXY"] = args.host
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def http(sql):
        url = f"http://{args.host}:{args.http_port}/?" + urllib.parse.urlencode({"query": sql})
        with opener.open(urllib.request.Request(url, data=b""), timeout=30) as source:
            return source.read().decode().strip()

    version = http("SELECT version()")
    reference = (Path(__file__).resolve().parents[1] / ".server-ref").read_text().strip()
    if version != VERSION or reference != f"v{VERSION}-lts":
        raise ValueError(f"capture pin mismatch: {version}/{reference}")
    manifest_path = args.output / "tcp_insert_rev54460.json"
    manifest = json.loads(manifest_path.read_text()) if args.cases and manifest_path.exists() else {
        "captured_on": date.today().isoformat(), "server_version": version,
        "source_tag": reference, "declared_revision": REVISION,
        "host": args.host, "queries": {}, "files": {}}
    manifest.update(captured_on=date.today().isoformat(), server_version=version,
                    source_tag=reference, declared_revision=REVISION, host=args.host)
    files = {}
    http(f"DROP TABLE IF EXISTS {TABLE}")
    try:
        http(f"CREATE TABLE {TABLE} (n UInt64, s String, CONSTRAINT small CHECK n < 100) ENGINE=Memory")
        for label, chunks in cases.items():
            if args.cases and label not in args.cases:
                continue
            http(f"TRUNCATE TABLE {TABLE}")
            sql = f"INSERT INTO {TABLE} (n, s) VALUES"
            expected_rows = sorted(row for chunk in chunks for row in chunk)
            if label == "before_schema":
                sql = "INSERT INTO ch_core_tcp_insert_fixture_missing (n, s) VALUES"
            elif label == "midupload":
                sql = f"INSERT INTO {TABLE} (n, s) SETTINGS interactive_delay=1000 VALUES"
            elif label == "inline_values":
                sql += " (13, 'user_1'), (79, 'user_2')"
                expected_rows = [(13, "user_1"), (79, "user_2")]
            elif label == "insert_select":
                sql = f"INSERT INTO {TABLE} (n, s) SELECT toUInt64(13), 'user_1'"
                expected_rows = [(13, "user_1")]
            elif label == "inline_async":
                sql = (f"INSERT INTO {TABLE} (n, s) SETTINGS async_insert=1, "
                       "wait_for_async_insert=1 VALUES (79, 'user_2')")
                expected_rows = [(79, "user_2")]
            if label == "completion_exception":
                expected_rows = []
            request = query_packet(sql, f"ch-core-insert-fixture-{label}")
            upload_blocks = [block(rows) for rows in chunks]
            upload = b"".join(varuint(2) + string("") + body for body in upload_blocks)
            completion, completion_packets = b"", []
            midupload, midupload_packets = b"", []
            finish = b""
            exchange_order = ["request", "schema"]
            exception_phase = None
            completion_probe = None
            with socket.create_connection((args.host, args.tcp_port), timeout=10) as sock:
                sock.sendall(client_hello())
                with sock.makefile("rb", buffering=0) as source:
                    reader = Reader(source)
                    read_hello(reader)
                    sock.sendall(string("") + request)
                    schema, schema_packets = response(reader, schema_only=True)
                    if label == "before_schema":
                        assert schema_packets[-1]["code"] == 2
                        exception_phase = "before_schema"
                    elif label in ("inline_values", "insert_select", "inline_async"):
                        assert schema_packets[-1]["code"] == 5, schema_packets[-1]
                        assert not any(p["code"] == 1 for p in schema_packets)
                    else:
                        assert schema_packets[-1]["code"] == 1
                        assert schema_packets[-1]["schema"] == [["n", "UInt64"], ["s", "String"]]
                        if label == "midupload":
                            first = varuint(2) + string("") + upload_blocks[0]
                            sock.sendall(first)
                            exchange_order.append("upload[0]")
                            # This read occurs before block 2 or the terminator
                            # has been sent, proving full-duplex upload telemetry.
                            while not {10, 14}.issubset({p["code"] for p in midupload_packets}):
                                packet, parsed = response(reader, one_packet=True)
                                assert parsed[0]["code"] in (10, 14)
                                for entry in parsed:
                                    entry["offset"] += len(midupload)
                                    entry["block_offset"] += len(midupload)
                                midupload += packet
                                midupload_packets.extend(parsed)
                            exchange_order.append("midupload")
                            sock.sendall(upload[len(first):] + FINISH)
                            finish = FINISH
                            exchange_order.extend(["upload[1]", "finish"])
                        else:
                            if upload:
                                sock.sendall(upload)
                                exchange_order.append("upload")
                            if label == "completion_exception":
                                # Prove whether rejection occurs before the
                                # zero-column terminator instead of assuming.
                                while select.select([sock], [], [], 0.25)[0]:
                                    packet, parsed = response(reader, one_packet=True)
                                    for entry in parsed:
                                        entry["offset"] += len(midupload)
                                        if "block_offset" in entry:
                                            entry["block_offset"] += len(midupload)
                                    midupload += packet
                                    midupload_packets.extend(parsed)
                                    if parsed[-1]["code"] == 2:
                                        exception_phase = "before_upload_terminator"
                                        break
                                if exception_phase == "before_upload_terminator":
                                    completion_probe = {
                                        "result": "exception_before_terminator",
                                        "packet_codes": [p["code"] for p in midupload_packets],
                                    }
                                else:
                                    completion_probe = {
                                        "result": (
                                            "telemetry_then_timeout"
                                            if midupload_packets
                                            else "timeout_no_packet"
                                        ),
                                        "timeout_seconds": 0.25,
                                        "packet_codes": [p["code"] for p in midupload_packets],
                                    }
                                if midupload:
                                    exchange_order.append("midupload")
                            if exception_phase != "before_upload_terminator":
                                sock.sendall(FINISH)
                                finish = FINISH
                                exchange_order.append("finish")
                        if exception_phase == "before_upload_terminator":
                            completion, completion_packets = midupload, midupload_packets
                            midupload, midupload_packets = b"", []
                            exchange_order[-1] = "completion"
                        else:
                            completion, completion_packets = response(reader)
                            exchange_order.append("completion")
                        assert completion_packets[-1]["code"] == (2 if label == "completion_exception" else 5)
                        if label == "completion_exception" and exception_phase is None:
                            exception_phase = "after_upload_terminator"
            actual = http(f"SELECT n, s FROM {TABLE} ORDER BY n FORMAT JSONEachRow")
            expected = "\n".join(json.dumps({"n": n, "s": text}, separators=(",", ":")) for n, text in expected_rows)
            assert actual == expected, (label, actual, expected)
            entry = {"sql": sql, "query_id": f"ch-core-insert-fixture-{label}",
                     "schema_packets": schema_packets, "midupload_packets": midupload_packets,
                     "completion_packets": completion_packets,
                     "exchange_order": exchange_order,
                     "upload_blocks": [{"offset": sum(len(b) + 2 for b in upload_blocks[:i]),
                                        "length": len(body) + 2, "rows": chunks[i]} for i, body in enumerate(upload_blocks)],
                     "verified_http_rows": actual if label != "before_schema" else None,
                     "http_verification": (
                         {"status": "not_applicable",
                          "reason": "query targets a missing table"}
                         if label == "before_schema"
                         else {"status": "verified", "rows": actual}
                     ),
                     "exception_phase": exception_phase,
                     "completion_probe": completion_probe}
            manifest["queries"][label] = entry
            for stage, data in [("request", request), ("schema", schema), ("upload", upload),
                                ("finish", finish), ("midupload", midupload), ("completion", completion)]:
                files[f"tcp_insert_{label}_{stage}_rev54460.bytes"] = data
    finally:
        http(f"DROP TABLE IF EXISTS {TABLE}")
    args.output.mkdir(parents=True, exist_ok=True)
    for name, data in files.items():
        (args.output / name).write_bytes(data)
        manifest["files"][name] = {"length": len(data), "sha256": hashlib.sha256(data).hexdigest()}
        print(f"{name}: {len(data)} bytes")
    (args.output / "tcp_insert_rev54460.json").write_text(json.dumps(manifest, indent=2) + "\n")
    write_oracle(args.output, manifest["queries"])


if __name__ == "__main__":
    main()
