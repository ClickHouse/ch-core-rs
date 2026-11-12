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
| `all_types_rev54460.native` | HTTP `FORMAT Native` with `client_protocol_version=54460` | `54460` | `01 00 02 ff ff ff ff 00 74 04 02 69 38 04 49 6e` |
| `all_types_rev54460.lz4` | Same query and revision, HTTP `compress=1` | `54460` | `11 69 3d e2 dd b9 84 c9 90 98 d1 e6 d9 37 a0 65` |
| `multi_block_rev0.native` | HTTP `FORMAT Native`, `max_block_size=2` | `0` | `01 02 01 6e 05 49 6e 74 33 32 0d 00 00 00 0e 00` |
| `json_string_rev0.native` | HTTP `FORMAT Native`, `output_format_native_write_json_as_string=1` | `0` | `01 04 01 6a 26 4a 53 4f 4e 28 6d 61 78 5f 64 79` |
| `json_flattened_rev0.native` | HTTP `FORMAT Native`, `output_format_native_use_flattened_dynamic_and_json_serialization=1` | `0` | `01 04 01 6a 26 4a 53 4f 4e 28 6d 61 78 5f 64 79` |

The revision-54485 fixture starts with the 10-byte `BlockInfo` preamble:
`01 00 02 ff ff ff ff 03 00 00`.
At revision 54460 the `BlockInfo` preamble is eight bytes:
`01 00 02 ff ff ff ff 00`. Field 3 is absent before revision 54480.

All `all_types` fixtures carry 116 columns, so the leading column-count
varint is `0x74` (116): at the very start of `all_types_rev0.native`, and
immediately after the 10-byte `BlockInfo` preamble in
`all_types_rev54485.native`, or after the eight-byte preamble at revision 54460.
Next is the `0x04` row-count varint (4 rows), then
the first column: name length `0x02`, name `i8` (`69 38`), type length `0x04`,
type `Int8` (`49 6e 74 38`). At revision 54485 a per-column
custom-serialization marker byte follows each type string, also at revision
54460; at revision 0 it does
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

## Revision 54460 compression and TCP packets

The additional fixtures below were captured on 2026-11-05 from the same
`26.8.1.2041` server, source tag `v26.8.1.2041-lts`. They exercise the engine's
chosen revision 54460 independently of the core's revision ceiling.
`tests/tcp_compression.rs` reads these bytes without network I/O.

| File | Bytes | Content |
|------|-------|---------|
| `http_multi_rev54460.native` | 165 | Plain HTTP Native, three blocks with 2, 2, and 1 rows |
| `http_multi_rev54460_lz4.native` | 234 | Same HTTP query with `compress=1` |
| `tcp_multi_rev54460.packets` | 215 | Five plain server Data packets |
| `tcp_multi_rev54460_lz4.packets` | 337 | The same five Data packets with LZ4 bodies |
| `tcp_large_rev54460_lz4.packets` | 4522 | Schema packet, one large row, end packet |
| `tcp_rev54460.json` | variable | Queries, version pin, packet offsets and frame sizes |

Regenerate these six files using only Python's standard library:

```sh
python3 scripts/gen_tcp_fixtures.py
```

The capture defaults to `127.0.0.1:8123` for HTTP and `127.0.0.1:9000` for TCP,
user `default` with no password. It bypasses HTTP proxies. Override `--host`,
`--http-port`, `--tcp-port`, or `--output` for a matching local test server.
It checks the exact HTTP version, checks the TCP Hello version and revision,
and checks `.server-ref` before capturing. The existing
`scripts/gen_fixtures.sh` also refreshes the two all-types revision-54460 files.

The small query yields UInt64 values 13 through 17 and String values `user_0`
through `user_4`, with `max_block_size=2` and `max_threads=1`. TCP adds an initial
schema-only Data packet and a final zero-column, zero-row Data packet. These
are preserved. Each `.packets` file contains only complete server Data packets:
raw varUInt code 1, raw binary String block name, then the Native block body.
All captured names are empty, so every packet starts with `01 00`.
Hello, Progress, ProfileInfo, and EndOfStream are parsed and excluded by the
capture script. ProfileEvents and logs are disabled through query settings.
No packet is inferred by searching for byte patterns. Plain body lengths come
from parsing the query's UInt64/String Native schema; compressed body lengths
come from frame headers and parsing their decompressed plaintext. Checksums are
verified by the Rust tests.

The manifest records every packet's exact offset and length. Plain small packet
offsets are `0, 32, 94, 156, 203`, lengths `32, 62, 62, 47, 12`.
Compressed small packet offsets are `0, 59, 143, 227, 299`, lengths
`59, 84, 84, 72, 38`. Their frame plaintext sizes are respectively
`30, 60, 60, 45, 10` bytes.

The large query returns UInt64 79 and a String of 1,100,000 `x` bytes.
Its middle Data packet starts at byte 59 and occupies 4425 bytes. That one
Native block spans two frames with plaintext lengths 1,048,576 and 51,465.
The final empty packet starts at byte 4484 and occupies 38 bytes. Tests use
these captured sizes to verify the frame loop and preservation of the next
raw packet header.

Server-source behavior confirmed at `v26.8.1.2041-lts`:
`src/Server/TCPHandler.cpp`, `TCPHandler::sendData`, writes the code and name
to the raw stream and flushes `maybe_compressed_out` after each Native block.
`src/Client/Connection.cpp`, `Connection::sendData`, does the same for inserts.
`src/Server/HTTPHandler.cpp` installs the HTTP compression buffer, and
`src/Interpreters/executeQuery.cpp`, `executeQuery`, enables `setAutoFlush` on
both output format branches. `IOutputFormat::work` and `flushImpl` in
`src/Processors/Formats/IOutputFormat.cpp`, and
`ParallelFormattingOutputFormat::collectorThreadFunction` in
`src/Processors/Formats/Impl/ParallelFormattingOutputFormat.cpp`, flush after
each chunk or formatted unit. The captured HTTP frame plaintext lengths
`60, 60, 45` therefore align with its three Native blocks as expected.
Confirmed `HTTPHandler::Output::pushDelayedResults` copies buffered chunks
together into the compressor when `wait_end_of_query=1` or
`http_response_buffer_size>0`, so HTTP readers still accept arbitrary
frame/block boundaries. The fixture tests apply the end-of-block plaintext check only
to TCP. A separate hermetic test reframes
the real server plaintext into one compressed frame spanning all three blocks,
then verifies decoding preserves all three chunks.

The ignored writer acceptance test sends core-encoded revision-0 Native
blocks through `write_lz4_frame` and HTTP `decompress=1`. It covers the large
repetitive String split across two frames and a deterministic noisy String
with a repeated prefix at offset 49,152. It reads both inserted rows back
from a unique Memory table, compares all values, and drops the table:

```sh
cargo test --test tcp_compression live_server_accepts_compressed_native_insert -- --ignored
```

It passed against the pinned local server on 2026-11-05. The test defaults to
`127.0.0.1:8123`, bypasses proxies, and reads the same
`CLICKHOUSE_CONNECT_TEST_HOST`, `CLICKHOUSE_CONNECT_TEST_PORT`,
`CLICKHOUSE_CONNECT_TEST_USER`, and `CLICKHOUSE_CONNECT_TEST_PASSWORD`
variables as the other live acceptance tests.

## Revision 54460 handshake and ping

These bytes were captured on 2026-11-06 from `127.0.0.1`, with `no_proxy`
set and HTTP proxies bypassed, against `26.8.1.2041` and source tag
`v26.8.1.2041-lts`. The `ch-tcp-proto` replay tests use them without sockets.

| File | Bytes | First bytes | Content |
|------|-------|-------------|---------|
| `tcp_handshake_client_hello_rev54460.packet` | 42 | `00 19 63 68 2d 63 6f 72` | Client Hello, user `default`, empty password |
| `tcp_handshake_server_hello_rev54460.packet` | 35 | `00 0a 43 6c 69 63 6b 48` | Server Hello, advertised revision 54492, effective revision 54460 |
| `tcp_handshake_addendum_rev54460.bytes` | 1 | `00` | Empty quota-key string, no packet tag |
| `tcp_ping_client_rev54460.packet` | 1 | `04` | Ping after the addendum |
| `tcp_ping_server_rev54460.packet` | 1 | `04` | Pong |
| `tcp_handshake_exception_client_rev54460.packet` | 73 | `00 19 63 68 2d 63 6f 72` | Client Hello with a deliberately nonexistent user |
| `tcp_handshake_exception_server_rev54460.packet` | 1411 | `02 04 02 00 00 0d 44 42` | Authentication Exception, code 516 |
| `tcp_handshake_rev54460.json` | variable | JSON | Capture metadata, decoded fields, sizes and SHA-256 hashes |

Regenerate using Python's standard library:

```sh
no_proxy=127.0.0.1 NO_PROXY=127.0.0.1 python3 scripts/gen_tcp_handshake_fixtures.py
```

The script checks the exact HTTP server version, TCP Hello version and
`.server-ref` before writing. It uses `default` with an empty password for
success and `ch_core_handshake_fixture_missing_user` with an empty password
for the failure. It records the bytes as received, including the server's
display name and exception stack trace. These can change between captures,
so the replay tests assert only that the display name is non-empty and that the
stack trace mentions `receiveHello`. The JSON manifest records the captured
values for reference.

The matching server source confirms these layouts in
`src/Server/TCPHandler.cpp`, `TCPHandler::receiveHello`, `sendHello`,
`receiveAddendum`, and `sendException`; `src/Core/Protocol.h`; and
`src/IO/WriteHelpers.cpp`, `writeException`. All Hello fields after the raw
server revision are parsed using effective revision 54460. Fields introduced
at 54461 and above are absent even though the server advertises 54492.

## Revision 54460 SELECT capture workflow

The uncompressed query fixture generator requires the same local
`26.8.1.2041` server and `.server-ref` pin:

```sh
no_proxy=127.0.0.1 NO_PROXY=127.0.0.1 python3 -B scripts/gen_tcp_query_fixtures.py
```

It uses only the Python standard library, loopback, and the `default` user with
an empty password. It checks both HTTP and TCP versions and bypasses proxies.
The generated `tcp_query_*_client_rev54460.packet` files hold the Query packet
and empty client Data block. The corresponding `*_server_rev54460.packets`
files retain complete responses, including logs, profile events, progress,
profiles, totals, extremes, and the final EOS or Exception.
`tcp_query_rev54460.json` records queries, packet boundaries, schemas, and file
hashes. `tcp_query_oracle_rev54460.rs` is generated from those independently
parsed boundaries. The replay test uses it to check the exact event sequence,
Progress and ProfileInfo fields, and every block event payload without adding a
JSON dependency to the crate.

The queries cover SELECT 1, multiple result blocks, a mid-stream division
error, totals and extremes, and typed parameters. Every parameter value is the
text an HTTP `param_<name>` value would carry, the server's escaped text form.
The Query packet wraps that text in exactly one String Field dump, which
`readQueryParameters` undoes with one `readQuoted`. The server then parses the
text with `deserializeTextEscaped` for the declared type, as it does for HTTP.
The escaped parameter sends the escaped text of a String holding quote,
backslash, NUL, newline, carriage return, tab, backspace, and form feed, and
the replay checks the decoded bytes. The container parameter sends
`['user_1','it\'s']` for `Array(String)` and `\N` for `Nullable(String)`, and
the replay checks both element offsets, the two values, and the NULL. At
capture time the generator parses and compares each complete result column
body, so a value substring or an adjacent column's length byte cannot satisfy
the oracle accidentally.
The capture asserts that the error follows positive-row Data and immediately
executes SELECT 1 again on the same socket, with no intervening EOS or Ping.
It also live-probes two first-response Exceptions: `max_threads=abc` returns
code 27 and closes the socket, while a code 62 syntax error from a query with
no wire settings leaves the same socket reusable.

ClientInfo uses sentinel OS user, hostname, client version, quota key, initial
address, and initial start time values. After `SYSTEM FLUSH LOGS`, the generator
queries the `QueryFinish` row in `system.query_log` by query ID and independently
checks the fields the server preserves. It separately verifies that the server
replaced the supplied initial address with a real peer address and positive
port, as it does for every initial query.

Each matching `.native` file concatenates the result Data bodies while omitting
the zero-column, zero-row TCP end marker. Schema-only result blocks remain.
`crates/ch-tcp-proto/tests/query_replay.rs` compares those bytes with engine
output, decodes the result with core, and replays every split, bytewise input,
and seeded random chunking without sockets. The new captures must be generated
and these replays must pass before accepting the SELECT slice.

Confirmed source at `v26.8.1.2041-lts`:
`src/Client/Connection.cpp::sendQuery`,
`src/Interpreters/ClientInfo.cpp::write/read`,
`src/Core/BaseSettings.cpp::BaseSettingsHelpers::writeFlags`,
`src/Core/Settings.cpp::writeQueryParameters/readQueryParameters`,
`src/Interpreters/ReplaceQueryParameterVisitor.cpp::resolveParameterValueAsField`,
`src/Server/HTTPHandler.cpp::DynamicQueryHandler::customizeQueryParam`,
`src/DataTypes/Serializations/SerializationString.cpp::deserializeTextEscaped`,
`src/Server/TCPHandler.cpp::sendData/sendTotals/sendExtremes/initLogsBlockOutput/initProfileEventsBlockOutput`,
`src/IO/Progress.cpp::ProgressValues::write`, and
`src/QueryPipeline/ProfileInfo.cpp::write`.
Settings flags are VarUInt IMPORTANT, value 1, and parameter flags are CUSTOM,
value 2. Log and ProfileEvents use revision-54460 Native framing, including
BlockInfo.

## Revision 54460 INSERT capture workflow

The INSERT fixtures were captured on 2026-11-10 from local ClickHouse
`26.8.1.2041`, matched to source tag `v26.8.1.2041-lts`, at revision 54460.
Regenerate using only Python's standard library:

```sh
no_proxy=127.0.0.1 NO_PROXY=127.0.0.1 python3 -B scripts/gen_tcp_insert_fixtures.py
```

Run the offline parser, request-byte, and oracle regression check without a
server. It also fails if `tcp_insert_oracle_rev54460.rs` differs from the
oracle regenerated from the manifest:

```sh
python3 -B scripts/gen_tcp_insert_fixtures.py --check
```

Use `--cases <case> ...` to refresh selected cases while preserving the other
captures. Unknown labels are rejected before connecting, and a partial refresh
updates the capture date and server version. The generator restricts the host
to `127.0.0.1`, bypasses proxies, checks HTTP and TCP versions and
`.server-ref`, and uses `default` with an empty password. Its Query packet has
fixed INSERT-specific ClientInfo values and settings, independent of the
SELECT fixture generator's sentinels. It creates
`ch_core_tcp_insert_fixture`, a Memory table with UInt64 `n`, String `s`, and a
constraint `n < 100`, then drops it in cleanup. It independently checks stored
rows through HTTP JSONEachRow after every applicable exchange. The
`before_schema` missing-table case records HTTP verification as not applicable.

Each `tcp_insert_<case>_*_rev54460.bytes` set separates the client Query request
plus external-table terminator, server initial response, client upload blocks,
server telemetry received between upload blocks, client upload terminator, and
server completion response. The manifest builds `exchange_order` from the
stages actually sent or received. The initial response retains the historical
`schema` filename even when it ends in EOS without a schema. Empty files mean
that stage did not occur. Nine cases cover:

- `multi`: two upload blocks containing `(13, user_1)`, `(79, user_2)`, and
  `(17, empty String)`. HTTP verifies all three rows.
- `empty`: no upload blocks, followed by the upload terminator. HTTP verifies
  the table remains empty.
- `zero_rows`: one schema-bearing upload block with zero rows, followed by the
  zero-column upload terminator. HTTP verifies the table remains empty. This
  distinguishes valid zero-row data from the terminator.
- `before_schema`: an unknown table raises Exception code 60 before a schema
  is sent. No upload or terminator is sent.
- `completion_exception`: UInt64 179 violates the constraint, raising
  Exception code 469. Before sending the terminator, the generator waits 250
  ms and records any complete telemetry or Exception packet. The manifest
  records the observed phase. HTTP verifies no rows were stored.
- `midupload`: the client sends block 1 with `(13, user_1)`, reads complete
  Log and ProfileEvents packets from the live socket, then sends block 2
  with `(79, user_2)` and the terminator. HTTP verifies both rows. These are
  actual packets received before the second block was sent.
- `inline_values`: SQL contains both `(13, user_1)` and `(79, user_2)`, and
  completes with EOS without requesting an upload schema.
- `insert_select`: SQL inserts `(13, user_1)` from SELECT and completes with
  EOS without requesting an upload schema.
- `inline_async`: SQL enables `async_insert=1, wait_for_async_insert=1` and
  contains `(79, user_2)`. It completes with EOS without requesting an upload
  schema. HTTP verifies the completed insertion.

The streaming schema responses include Log, TableColumns, and a zero-row
schema-bearing Data packet. Inline and SELECT responses include actual
Progress, Log, and ProfileEvents before EOS. The manifest
`tcp_insert_rev54460.json` records packet boundaries, schema, uploads, HTTP
results, exception phase, file hashes, and the ordered full-duplex exchange.
For `midupload`, the order is request, schema, first upload block, server
telemetry, second upload block, client terminator, then completion.
`tcp_insert_oracle_rev54460.rs` is generated from the independent Python
parser. It compares exact block payloads, raw TableColumns bytes, Progress and
ProfileInfo fields, and Exception fields. TableColumns fields are raw bytes:
the manifest's `external_table_hex` and `columns_description_hex` keys are
lossless and authoritative, the matching text keys are lossy UTF-8 for
reading, and the oracle emits them as Rust byte strings with hex escapes for
any non-printable or non-ASCII byte. No boundaries are inferred by
searching byte patterns.

`crates/ch-tcp-proto/tests/insert_replay.rs` checks every two-chunk split of
each initial and completion response, bytewise reads, seeded random chunking,
and every truncated prefix. A separate test feeds every prefix of the real
mid-upload telemetry without draining completed events, sends the second
block and terminator, then feeds the suffix and completion. This covers both
partial packets and complete queued Log/ProfileEvents while writing. Query
and terminator bytes must match the captures. Upload blocks are constructed
through core's `encode_block` and their complete Data packets must match the
accepted uploads. The schema is also decoded through core. INSERT Exceptions
conservatively fail the engine connection.

The constraint capture's `completion_probe` and `exception_phase` fields state
whether the server produced the failure before or after the upload terminator.
Synthetic engine tests cover Exceptions while uploading. The mid-upload
telemetry fixture makes no claim about connection reuse after an Exception.

Confirmed source at `v26.8.1.2041-lts`:
`src/Client/Connection.cpp::sendData`,
`src/Server/TCPHandler.cpp::startInsertQuery/processInsertQuery/receivePacketsExpectData/processData/sendData/sendTableColumns/sendEndOfStream/skipData`,
`src/Server/TCPHandler.cpp::sendLogs/sendInsertProfileEvents`, and
`src/Formats/NativeReader.cpp::NativeReader::read` describe schema delivery,
client Data framing, per-block telemetry, the zero-column upload terminator,
and error drain behavior. The server-reader findings distinguish that
confirmed framing from the conservative engine policy of retiring failed
INSERT connections.
