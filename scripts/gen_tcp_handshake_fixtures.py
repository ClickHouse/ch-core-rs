#!/usr/bin/env python3
"""Capture revision-54460 TCP Hello, addendum, Ping/Pong, and auth failure.

Uses Python's standard library and only empty fixture passwords. Captured
server bytes are not normalized. See tests/fixtures/README.md.
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

# `python3 -I` drops the script directory from sys.path. Add it explicitly so
# the sibling helper module still imports.
sys.path.insert(0, str(Path(__file__).resolve().parent))

from gen_tcp_fixtures import Reader, string, varuint

REVISION = 54460
VERSION = "26.8.1.2041"
CLIENT_NAME = "ch-core-handshake-fixture"
MISSING_USER = "ch_core_handshake_fixture_missing_user"


def client_hello(user):
    return (
        varuint(0) + string(CLIENT_NAME) + varuint(26) + varuint(8)
        + varuint(REVISION) + string("") + string(user) + string("")
    )


def read_hello(reader):
    if reader.uint() != 0:
        raise ValueError("expected Server Hello")
    name = reader.string().decode()
    major, minor, revision = reader.uint(), reader.uint(), reader.uint()
    timezone, display, patch = (
        reader.string().decode(), reader.string().decode(), reader.uint()
    )
    if (major, minor, patch, revision) != (26, 8, 1, 54492):
        raise ValueError(f"unexpected TCP server: {major}.{minor}.{patch}/{revision}")
    return {
        "name": name, "major": major, "minor": minor, "patch": patch,
        "revision": revision, "effective_revision": min(REVISION, revision),
        "timezone": timezone, "display_name": display,
    }


def read_exception(reader):
    if reader.uint() != 2:
        raise ValueError("expected authentication Exception")
    code = struct.unpack("<i", reader.read(4))[0]
    name, message, stack = (
        reader.string().decode(), reader.string().decode(), reader.string().decode()
    )
    nested = reader.read(1)[0]
    if code != 516 or name != "DB::Exception" or nested != 0:
        raise ValueError(f"unexpected authentication Exception: {code}/{name}/{nested}")
    if "Authentication failed" not in message or MISSING_USER not in message:
        raise ValueError(f"unexpected authentication failure message: {message}")
    return {
        "code": code, "name": name, "message": message,
        "stack_trace_bytes": len(stack.encode()), "has_nested": False,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--http-port", type=int, default=8123)
    parser.add_argument("--tcp-port", type=int, default=9000)
    parser.add_argument(
        "--output", type=Path,
        default=Path(__file__).resolve().parents[1] / "tests/fixtures",
    )
    args = parser.parse_args()
    # Explicitly bypass proxies; this tool is for a matching local server.
    os.environ["no_proxy"] = args.host
    os.environ["NO_PROXY"] = args.host
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    query = urllib.parse.urlencode({"query": "SELECT version()"})
    with opener.open(f"http://{args.host}:{args.http_port}/?{query}", timeout=30) as response:
        version = response.read().decode().strip()
    reference = (Path(__file__).resolve().parents[1] / ".server-ref").read_text().strip()
    if version != VERSION or reference != f"v{VERSION}-lts":
        raise ValueError(f"capture server/source pin mismatch: {version}/{reference}")
    captures = {}
    hello = client_hello("default")
    addendum, ping = string(""), varuint(4)
    with socket.create_connection((args.host, args.tcp_port), timeout=30) as sock:
        sock.sendall(hello)
        with sock.makefile("rb") as source:
            reader = Reader(source)
            server_hello = read_hello(reader)
            captures["tcp_handshake_client_hello_rev54460.packet"] = hello
            captures["tcp_handshake_server_hello_rev54460.packet"] = bytes(reader.record)
            sock.sendall(addendum)
            captures["tcp_handshake_addendum_rev54460.bytes"] = addendum
            sock.sendall(ping)
            captures["tcp_ping_client_rev54460.packet"] = ping
            reader.record.clear()
            if reader.uint() != 4:
                raise ValueError("expected Pong")
            captures["tcp_ping_server_rev54460.packet"] = bytes(reader.record)
    failed_hello = client_hello(MISSING_USER)
    with socket.create_connection((args.host, args.tcp_port), timeout=30) as sock:
        sock.sendall(failed_hello)
        with sock.makefile("rb") as source:
            reader = Reader(source)
            exception = read_exception(reader)
            captures["tcp_handshake_exception_client_rev54460.packet"] = failed_hello
            captures["tcp_handshake_exception_server_rev54460.packet"] = bytes(reader.record)
            if source.read(1):
                raise ValueError("expected connection close after authentication failure")
    manifest = {
        "captured_on": date.today().isoformat(),
        "server_version": version, "source_tag": reference,
        "declared_revision": REVISION, "client_name": CLIENT_NAME,
        "client_version": {"major": 26, "minor": 8},
        "database": "", "success_user": "default", "password": "",
        "missing_user": MISSING_USER, "quota_key": "",
        "server_hello": server_hello, "exception": exception, "files": {},
    }
    args.output.mkdir(parents=True, exist_ok=True)
    for name, data in captures.items():
        (args.output / name).write_bytes(data)
        manifest["files"][name] = {
            "length": len(data), "sha256": hashlib.sha256(data).hexdigest(),
            "first_bytes": data[:16].hex(" "),
        }
        print(f"{name}: {len(data)} bytes")
    (args.output / "tcp_handshake_rev54460.json").write_text(
        json.dumps(manifest, indent=2) + "\n"
    )


if __name__ == "__main__":
    main()
