# Native Fixtures

These `.native` files are raw `FORMAT Native` responses captured from a real
ClickHouse server and committed so `cargo test` stays hermetic. They are
decoded by `tests/integration.rs` with expected values kept in Rust code.

The current bytes were captured from a live local server reporting
ClickHouse `26.6.1.1193`, using the same localhost default convention as the
`clickhouse-connect` integration suite. Server-source framing behavior was
cross-checked against the matching `.server-ref` tag `v26.6.1.1193-stable`.

## Fixtures

| File | Capture path | `protocol_revision` | First bytes |
|------|--------------|---------------------|-------------|
| `all_types_rev0.native` | HTTP `FORMAT Native` | `0` | `26 04 02 69 38 04 49 6e 74 38 80 ff 00 7f 03 69` |
| `all_types_rev54485.native` | HTTP `FORMAT Native` with `client_protocol_version=54485` | `54485` | `01 00 02 ff ff ff ff 03 00 00 26 04 02 69 38 04` |
| `multi_block_rev0.native` | HTTP `FORMAT Native`, `max_block_size=2` | `0` | `01 02 01 6e 05 49 6e 74 33 32 0d 00 00 00 0e 00` |

The framed fixture starts with the standard 10-byte `BlockInfo` preamble:
`01 00 02 ff ff ff ff 03 00 00`.

Zero-row HTTP and `clickhouse-client` Native queries on the current local
server emit an empty response, not a schema-bearing zero-row Native block. The
decoder's zero-row block behavior remains covered by unit tests in
`src/native/decode.rs`.

## Refreshing

Run:

```sh
scripts/gen_fixtures.sh
```

The script defaults to:

- `CLICKHOUSE_CONNECT_TEST_HOST=localhost`
- `CLICKHOUSE_CONNECT_TEST_PORT=8123`
- `CLICKHOUSE_CONNECT_TEST_USER=default`
- `CLICKHOUSE_CONNECT_TEST_PASSWORD=`

Override those variables to point at a different local test server. After
refreshing, run `cargo test` and update this README if the server version,
fixture list, protocol revisions, or first-byte hexdumps change.

The framed fixture is captured with `client_protocol_version` set to the pinned
version's `DBMS_TCP_PROTOCOL_VERSION` (`54485` for `v26.6.1.1193-stable`, see
`src/native/decode.rs`). The server caps the negotiated revision at its own
maximum, so if you point the script at a different server version, update that
number in `scripts/gen_fixtures.sh`, the fixture file name, and
`tests/integration.rs` to the new negotiated revision. The full type-adding
workflow lives in `AGENTS.md` under "Adding A New ClickHouse Type".
