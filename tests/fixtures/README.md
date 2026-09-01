# Native Fixtures

These `.native` files are raw `FORMAT Native` responses captured from a real
ClickHouse server and committed so `cargo test` stays hermetic. They are
decoded by `tests/integration.rs` with expected values kept in Rust code.

The current bytes were captured from a live local server reporting ClickHouse
`26.8.1.2041`, using the same localhost default convention as the
`clickhouse-connect` integration suite. Server-source framing behavior was
cross-checked against the matching `.server-ref` tag `v26.8.1.2041-lts`.

## Fixtures

| File | Capture path | `protocol_revision` | First bytes |
|------|--------------|---------------------|-------------|
| `all_types_rev0.native` | HTTP `FORMAT Native` | `0` | `74 04 02 69 38 04 49 6e 74 38 80 ff 00 7f 03 69` |
| `all_types_rev54485.native` | HTTP `FORMAT Native` with `client_protocol_version=54485` | `54485` | `01 00 02 ff ff ff ff 03 00 00 74 04 02 69 38 04` |
| `multi_block_rev0.native` | HTTP `FORMAT Native`, `max_block_size=2` | `0` | `01 02 01 6e 05 49 6e 74 33 32 0d 00 00 00 0e 00` |
| `json_string_rev0.native` | HTTP `FORMAT Native`, `output_format_native_write_json_as_string=1` | `0` | `01 04 01 6a 26 4a 53 4f 4e 28 6d 61 78 5f 64 79` |
| `json_flattened_rev0.native` | HTTP `FORMAT Native`, `output_format_native_use_flattened_dynamic_and_json_serialization=1` | `0` | `01 04 01 6a 26 4a 53 4f 4e 28 6d 61 78 5f 64 79` |

The framed fixture starts with the standard 10-byte `BlockInfo` preamble:
`01 00 02 ff ff ff ff 03 00 00`.

Both `all_types` fixtures now carry 116 columns, so the leading column-count
varint is `0x74` (116): at the very start of `all_types_rev0.native`, and
immediately after the 10-byte `BlockInfo` preamble in
`all_types_rev54485.native`. Next is the `0x04` row-count varint (4 rows), then
the first column: name length `0x02`, name `i8` (`69 38`), type length `0x04`,
type `Int8` (`49 6e 74 38`). At revision 54485 a per-column
custom-serialization marker byte follows each type string; at revision 0 it does
not.

The six columns immediately before the trailing QBit group are three `JSON`
values, two Geometry columns, and standalone `MultiPoint`. For JSON
(`DataTypeObject`, GA at this
server version),
`j_typed` is
`JSON(max_dynamic_paths=1, `a.b` Int64)` exercising a typed path, exactly one
direct dynamic path, and shared-data spill (paths `y` and `z` overflow into the
shared stream as opaque binary descriptor + payload blobs); `j_bare` is a bare
`JSON` with two dynamic paths and an empty-object row; and `j_null` is a
`Nullable(JSON)` with NULL rows. The JSON body bytes are the same at both
protocol revisions, so both `all_types` fixtures assert the same decoded values.
`geometry` covers LineString, MultiPolygon, Point, and intrinsic NULL in a plain
`Geometry` column. `geometry_all` is `Array(Geometry)` and carries every
canonical alternative in discriminator order (`LineString`, `MultiLineString`,
`MultiPolygon`, `Point`, `Polygon`, `Ring`, `MultiPoint`) followed by NULL in
each row. The standalone `multipoint` column independently grounds
`MultiPoint = Array(Point)`.
The four trailing QBit columns cover every legal element width, a dimension of
9 that crosses the physical byte boundary, signed zero and nontrivial float bit
patterns, and whole-vector nullability.

## Setting-gated JSON wire shapes

`json_string_rev0.native` and `json_flattened_rev0.native` capture the SAME
single `JSON(max_dynamic_paths=1, `a.b` Int64)` column and data as `all_types`'
`j_typed`, but under one extra output-format setting each, so the two
setting-gated Native serializations are covered against the real server:

- `json_string_rev0.native` uses `output_format_native_write_json_as_string=1`
  (STRING mode, structure word 1): one re-serialized JSON document string per
  row. The typed path is materialized in the text (as `0` where omitted); the
  declared typed/dynamic split does not appear on the wire.
- `json_flattened_rev0.native` uses
  `output_format_native_use_flattened_dynamic_and_json_serialization=1`
  (FLATTENED mode, structure word 3): the typed path stays typed, the dynamic
  and shared-data paths are written as the union of shared-less per-path
  Dynamics (so `y` and `z` become flattened dynamic paths), and there is NO
  shared-data stream.

A preceding group of nine columns contains exact base `AggregateFunction(sum,
T)` states for
UInt8, BFloat16, Decimal32, UInt256, Nullable(UInt8), and Enum8, followed by
canonical `AggregateFunction(nothingNull, Nullable(Nothing))`, then
`Variant(String, UInt64)` with intrinsic NULL and both alternatives, and
`Dynamic(max_types=1)` with String direct and UInt64/Array(Int32) values in
SharedVariant. Together they
capture the 8-, 16-, and 32-byte accumulator layouts, the special float,
Decimal, and Enum promotion paths, nullable sum's flag plus conditional
accumulator, and the strict one-zero-byte nothingNull state against the real
server.

Zero-row HTTP and `clickhouse-client` Native queries on the current local
server emit an empty response, not a schema-bearing zero-row Native block. The
decoder's zero-row block behavior remains covered by unit tests in
`src/native/decode/tests/`.

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

The framed fixture is captured with `client_protocol_version=54485`, the highest
revision this crate fully supports. ClickHouse `v26.8.1.2041-lts` advertises
revision 54492, but revision 54492 enables the separate String size-stream
serialization. Until that layout is implemented, do not bump the fixture or
`DBMS_TCP_PROTOCOL_VERSION`; negotiate 54485 explicitly. The full type-adding
workflow lives in `AGENTS.md` under "Adding A New ClickHouse Type".
