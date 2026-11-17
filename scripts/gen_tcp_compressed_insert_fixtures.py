#!/usr/bin/env python3
"""Capture compressed revision-54460 INSERT and same-socket SELECT recovery."""

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
import tempfile
import urllib.parse
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gen_tcp_query_fixtures as query
import gen_tcp_compressed_query_fixtures as compressed
from gen_tcp_fixtures import Reader, string, varuint
from gen_tcp_handshake_fixtures import read_hello

TABLE = "ch_core_tcp_compressed_insert_fixture"
SETTINGS = [("send_profile_events", "0"), ("send_logs_level", "none")]
FINISH = varuint(2) + string("") + query.EMPTY_COMPRESSED_LZ4
LARGE_STRING = "x" * 1_100_000
CASES = ("multi", "rejected", "before_schema", "plain_before_schema")


def request(sql, label):
    before_schema = "before_schema" in label
    return query.query_packet(
        sql, f"ch-core-compressed-insert-{label}",
        settings=() if before_schema and not label.endswith("_recovery") else SETTINGS,
        compression=not label.startswith("plain_"),
    )


def block(reader, compression):
    if compression:
        result = compressed.compressed_native_block(reader, "LZ4")
        native = result.pop("native")
        result.pop("_column_bodies")
    else:
        rows, columns, schema, _, native = query.native_block(reader)
        result = {"rows": rows, "columns": columns, "schema": schema, "frames": []}
    result.update(
        native_length=len(native),
        native_sha256=hashlib.sha256(native).hexdigest(),
    )
    return result


def client_block(data, compression):
    reader = Reader(io.BytesIO(data))
    assert reader.uint() == 2 and reader.string() == b""
    result = block(reader, compression)
    assert not reader.source.read(1)
    return result


def response(reader, compression=True, schema_only=False):
    packets, boundaries = bytearray(), []
    while True:
        reader.record.clear()
        code = reader.uint()
        entry = {"code": code, "offset": len(packets)}
        if code == 1:
            entry["name"] = reader.string().decode()
            entry.update(block(reader, compression))
        elif code == 11:
            entry["table_hex"] = reader.string().hex()
            entry["description_hex"] = reader.string().hex()
        elif code in (10, 14):
            entry["name"] = reader.string().decode()
            rows, columns, schema, _, native = query.native_block(reader)
            entry.update(rows=rows, columns=columns, schema=schema,
                         native_length=len(native),
                         native_sha256=hashlib.sha256(native).hexdigest())
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
        if code in (2, 5) or (schema_only and code == 1):
            return bytes(packets), boundaries


def check(output):
    manifest = json.loads((output / "tcp_compressed_insert_rev54460.json").read_text())
    for name, expected in manifest["files"].items():
        data = (output / name).read_bytes()
        assert len(data) == expected["length"], name
        assert hashlib.sha256(data).hexdigest() == expected["sha256"], name
    for label, case in manifest["cases"].items():
        compression = case["compression"]
        assert (output / case["request"]).read_bytes() == request(case["sql"], label)
        for stage in ("schema", "completion", "recovery"):
            if stage not in case:
                continue
            data = (output / case[stage]).read_bytes()
            reader = Reader(io.BytesIO(data))
            parsed, packets = response(reader, compression, schema_only=stage == "schema")
            assert parsed == data and not reader.source.read(1), (label, stage)
            assert packets == case[stage + "_packets"], (label, stage)
        if "finish" in case:
            assert (output / case["finish"]).read_bytes() == FINISH
        for name, expected in zip(case["blocks"], case["block_oracles"]):
            assert client_block((output / name).read_bytes(), compression) == expected, name
        if "recovery_request" in case:
            assert (output / case["recovery_request"]).read_bytes() == request(
                case["recovery_sql"], label + "_recovery"
            )
    print(f"parsed {len(manifest['cases'])} INSERT captures with engine-written packets")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", choices=["127.0.0.1"], default="127.0.0.1")
    parser.add_argument("--http-port", type=int, default=8123)
    parser.add_argument("--tcp-port", type=int, default=9000)
    parser.add_argument("--output", type=Path,
                        default=Path(__file__).resolve().parents[1] / "tests/fixtures")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if args.check:
        check(args.output)
        return
    os.environ["no_proxy"] = os.environ["NO_PROXY"] = args.host
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def http(sql, **params):
        params.update(query=sql)
        url = f"http://{args.host}:{args.http_port}/?" + urllib.parse.urlencode(params)
        with opener.open(urllib.request.Request(url, data=b""), timeout=30) as source:
            return source.read()

    version = http("SELECT version()").decode().strip()
    reference = (Path(__file__).resolve().parents[1] / ".server-ref").read_text().strip()
    if version != query.VERSION or reference != f"v{query.VERSION}-lts":
        raise ValueError(f"capture server/source pin mismatch: {version}/{reference}")
    manifest = {
        "captured_on": date.today().isoformat(), "server_version": version,
        "source_tag": reference, "declared_revision": query.REVISION,
        "client_encoder": "ch-tcp-proto Connection and core write_lz4_frame",
        "cases": {}, "files": {},
    }
    files = {}

    def save(label, stage, data):
        name = f"tcp_compressed_insert_{label}_{stage}_rev54460.bytes"
        files[name] = data
        return name

    http(f"DROP TABLE IF EXISTS {TABLE}")
    try:
        http(f"CREATE TABLE {TABLE} "
             "(n UInt64, s String, CONSTRAINT small CHECK n < 100) ENGINE=Memory")
        with tempfile.TemporaryDirectory(prefix="ch-core-insert-client-") as temporary:
            generated = Path(temporary)
            root = Path(__file__).resolve().parents[1]
            subprocess.run(
                ["cargo", "run", "--quiet", "-p", "ch-tcp-proto", "--example",
                 "gen_compressed_insert_fixture_client", "--", str(generated)],
                cwd=root, check=True,
            )

            def client(label, stage):
                return (generated / f"tcp_compressed_insert_{label}_{stage}_rev54460.bytes").read_bytes()

            for label in CASES:
                http(f"TRUNCATE TABLE {TABLE}")
                compression = label != "plain_before_schema"
                before_schema = "before_schema" in label
                sql = (
                    "INSERT INTO ch_core_tcp_compressed_insert_missing (n, s) VALUES"
                    if before_schema else f"INSERT INTO {TABLE} (n, s) VALUES"
                )
                case = {"sql": sql, "compression": compression,
                        "blocks": [], "block_oracles": []}
                request_bytes = client(label, "request")
                assert request_bytes == request(sql, label)
                case["request"] = save(label, "request", request_bytes)
                with socket.create_connection((args.host, args.tcp_port), timeout=30) as sock:
                    sock.sendall((generated / "tcp_compressed_insert_client_hello.bytes").read_bytes())
                    with sock.makefile("rb", buffering=0) as source:
                        reader = Reader(source)
                        read_hello(reader)
                        sock.sendall((generated / "tcp_compressed_insert_addendum.bytes").read_bytes()
                                     + request_bytes)
                        data, packets = response(reader, compression, schema_only=True)
                        assert packets[-1]["code"] == (2 if before_schema else 1)
                        first_stage = "completion" if before_schema else "schema"
                        case[first_stage] = save(label, first_stage, data)
                        case[first_stage + "_packets"] = packets
                        if not before_schema:
                            for index in range(2 if label == "multi" else 1):
                                body = client(label, f"block{index}")
                                oracle = client_block(body, compression)
                                if label == "multi" and index == 1:
                                    assert oracle["native_length"] > 1024 * 1024
                                    assert len(oracle["frames"]) > 1
                                case["blocks"].append(save(label, f"block{index}", body))
                                case["block_oracles"].append(oracle)
                                sock.sendall(body)
                            finish = client(label, "finish")
                            assert finish == FINISH
                            sock.sendall(finish)
                            case["finish"] = save(label, "finish", finish)
                            case["exception_phase"] = (
                                "after_upload_terminator_sent" if label == "rejected" else None
                            )
                            data, packets = response(reader, compression)
                            assert packets[-1]["code"] == (2 if label == "rejected" else 5)
                            case["completion"] = save(label, "completion", data)
                            case["completion_packets"] = packets
                        if label != "rejected":
                            recovery_sql = (
                                "SELECT toUInt64(13) AS n, 'user_1' AS s"
                                if before_schema else f"SELECT n, s FROM {TABLE} ORDER BY n"
                            )
                            recovery_request = client(label, "recovery_request")
                            assert recovery_request == request(recovery_sql, label + "_recovery")
                            sock.sendall(recovery_request)
                            data, packets = response(reader, compression)
                            assert packets[-1]["code"] == 5, packets
                            case["recovery_sql"] = recovery_sql
                            case["recovery_request"] = save(label, "recovery_request", recovery_request)
                            case["recovery"] = save(label, "recovery", data)
                            case["recovery_packets"] = packets
                rows = http(f"SELECT n, s FROM {TABLE} ORDER BY n FORMAT JSONEachRow")
                expected = (
                    ('{"n":13,"s":"user_1"}\n{"n":79,"s":"' + LARGE_STRING + '"}\n').encode()
                    if label == "multi" else b""
                )
                assert rows == expected, (label, len(rows), len(expected))
                case["verified_http_rows"] = {
                    "count": 2 if label == "multi" else 0,
                    "length": len(rows), "sha256": hashlib.sha256(rows).hexdigest(),
                }
                manifest["cases"][label] = case
    finally:
        http(f"DROP TABLE IF EXISTS {TABLE}")
    args.output.mkdir(parents=True, exist_ok=True)
    for name, data in files.items():
        (args.output / name).write_bytes(data)
        manifest["files"][name] = {
            "length": len(data), "sha256": hashlib.sha256(data).hexdigest(),
        }
        print(f"{name}: {len(data)} bytes")
    (args.output / "tcp_compressed_insert_rev54460.json").write_text(
        json.dumps(manifest, indent=2) + "\n"
    )
    check(args.output)


if __name__ == "__main__":
    main()
