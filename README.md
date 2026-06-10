# ch-core-rs

A shared Rust core that decodes the ClickHouse `FORMAT Native` wire format
into typed, Arrow-compatible columnar buffers. Pure Rust, zero runtime
dependencies. The decoder is implemented once here. Each language client wraps
it with a thin binding.

Every ClickHouse client reimplements the same work today: type string parsing,
Native block decoding, null maps, and the long tail of types. Each client
redoes that work on every server release. This crate does it once. A
wire-format bug produces silently corrupt columns, so one audited decoder
beats one per client. The result is a single implementation that is correct,
maintained in one place, and faster end to end than the existing client query
paths.

```
ClickHouse server
      |
      |  FORMAT Native bytes
      v
+---------------------------+
|        ch-core-rs         |  pure Rust, zero dependencies
|  type parsing, block      |  decode happens once, here
|  decode, columnar buffers |
+---------------------------+
      |
      |  typed buffers: Vec<i64>, offsets + data, packed bitmaps
      v
  thin per-language bindings (live in the client repos)
      |
      v
  NumPy / Arrow capsule (Python), TypedArray (JS), ...
```

## Division of labor

The core owns binary decoding, the ClickHouse logical type model, and the
shared Arrow-compatible buffer layout.

Bindings live in the client repos and own everything runtime-specific: Python
`int` versus JavaScript `BigInt`, null handling, stream and backpressure
integration, and the public client API. Bindings are thin adapters over
buffers the core has already filled.

Implement a ClickHouse type once in the core and every client gets it. The
buffers follow Arrow layout conventions, so results also export zero-copy
through the Arrow C Data Interface to anything that speaks Arrow (PyArrow,
Pandas, Polars, Arrow JS).

## Why this exists

1. One correct implementation, not N. Wire decoding is the highest-risk code
   in a client, and today it is duplicated in every language.
2. ClickHouse type fidelity. The server's own CH -> Arrow mapping normalizes
   or drops information such as Enum names, IP semantics, and exact type
   identity. The core's type model preserves ClickHouse semantics and leaves
   presentation policy to the bindings.
3. Speed. The Rust decode path plus zero-copy delivery into native containers
   outperforms the existing client paths end to end. Numbers below.
4. Streaming. The decoder accepts socket bytes as they arrive and emits
   decoded column chunks as each block completes. Decode overlaps the network
   instead of waiting for the full response.

## Performance

End-to-end POCs in the Node and Python clients route real queries through
each client's full transport stack and decode with this core. Localhost
medians against each client's existing query paths:

| Client                  | Destination          | Speedup                      |
|-------------------------|----------------------|------------------------------|
| Node (1M rows x 6 cols) | columns              | 8.7x vs JSON (21.1M rows/s)  |
| Node                    | row arrays / objects | 2.3x / 2.5x vs JSON          |
| Python                  | rows                 | 1.3-2.7x vs `client.query()` |
| Python                  | columns              | 1.7-3.1x                     |
| Python                  | NumPy                | 3.6-5.8x                     |
| Python                  | pandas               | 1.4-7.2x                     |
| Python                  | Arrow                | 2.7-7.7x vs `query_arrow`    |

The Python Arrow path also used 25-42% less peak memory than `query_arrow`.

Scope on these numbers: they are localhost measurements against the clients'
current paths. Part of the Arrow gain comes from streaming with overlapped
decode, where the existing clients buffer the full response before decoding.
Under heavy server-side compression on localhost the Arrow lead can invert,
bounded by server compression throughput. Native is also the most compact
ClickHouse wire format, which favors it further over a real network. The full
analysis is in `ARCHITECTURE.md`.

## How it works

The decoder reads `FORMAT Native` bytes, from a complete buffer or
incrementally from streamed chunks, and produces:

```text
ChunkedBatch
  schema                      ClickHouse logical types (ChType)
  chunks: Vec<Arc<ColBatch>>  one chunk per Native block, never merged
    columns: Vec<Column>      typed values / offsets / data / bitmaps
```

Native blocks remain separate chunks. This avoids merging and repacking
buffers and maps directly onto Arrow record batches. Buffer layouts follow
Arrow conventions throughout: fixed-width columns are one contiguous typed
buffer, strings are offsets plus a data buffer, booleans and validity are
bit-packed bitmaps.

Streaming uses a push API:

```text
StreamDecoder::feed(bytes) -> complete decoded blocks
StreamDecoder::finish()    -> final blocks or truncated-stream error
```

The streaming decoder retains incomplete trailing bytes between calls and
emits complete `ColBatch` values as soon as enough data has arrived.
Transport-level backpressure stays a binding or client responsibility.

`ARCHITECTURE.md` covers the decode path, streaming machinery, and the Arrow
C Data export in detail. `DECODER_CONTRACT.md` is the definitive per-type
contract: wire payload, decoded buffers, and Arrow export for every supported
type.

## Current scope

Implemented:

- Decode ClickHouse Native blocks from a complete byte buffer.
- Incrementally decode Native blocks from streamed byte chunks.
- Preserve ClickHouse blocks as separate columnar chunks.
- Store primitive values, strings, booleans, temporal values, and nullability
  in Arrow-compatible layouts.
- Export decoded chunks as an Arrow C Data stream.
- Malformed-input hardening: untrusted wire bytes return errors, never panic.

Not implemented yet:

- Native encoding for inserts.
- TCP/native protocol packet framing.
- Compression framing.
- Decimal, LowCardinality, Enum, UUID/IP, Array, Tuple, or Map types.
- Language-specific materialization policy (bindings own this, by design).

## Supported types

- `Bool`
- `Int8`, `Int16`, `Int32`, `Int64`
- `UInt8`, `UInt16`, `UInt32`, `UInt64`
- `Float32`, `Float64`
- `String`
- `FixedString(N)`
- `Date`, `Date32`, `DateTime`, `DateTime64(P[, tz])`
- `Nullable(T)` where `T` is one of the supported inner types

Unsupported types raise a decode error rather than guessing.

## Roadmap

In rough priority order:

1. Type coverage: `LowCardinality`, `Decimal`, `UUID`, `IPv4`/`IPv6`,
   `Enum8`/`Enum16`, `Array`, `Tuple`, `Map`, `Int128`/`Int256` and unsigned
   variants.
2. Compression framing: LZ4, then ZSTD.
3. Per-runtime zero-copy adapters: JS `TypedArray` over an external
   `ArrayBuffer`, NumPy export that does not route through Arrow.
4. Insert path: columnar input encoded to Native block bytes.
5. Native TCP protocol engine: handshake, query/data/progress/exception
   packets, revision negotiation.

## Consuming

Bindings live in the language client repos and depend on this crate.

Local development:

```toml
[dependencies]
ch-core-rs = { path = "/path/to/ch-core-rs" }
```

Pinned git dependency:

```toml
[dependencies]
ch-core-rs = { git = "ssh://git@github.com/ClickHouse/ch-core-rs.git", rev = "<commit>" }
```

Use a local `[patch]` in `.cargo/config.toml` to override a pinned git
dependency with a local checkout during development.

Decode a complete buffer with `native::decode::decode_all_bytes`, or stream
with `native::stream_decoder::StreamDecoder`.

## Repo layout

- `src/schema.rs` - ClickHouse logical type model.
- `src/column.rs` - Arrow-compatible physical column buffers.
- `src/batch.rs` - `ColBatch` and `ChunkedBatch` result model.
- `src/bitmap.rs` - validity bitmap conversion and storage.
- `src/native/` - Native-format varints, block decode, and stream decode.
- `src/ffi.rs` - Arrow C Data Interface export.

## Testing

```sh
cargo test
```

Decode logic is tested two ways: synthesized wire bytes in unit tests, and
committed `FORMAT Native` fixture bytes captured from a live ClickHouse server
in `tests/integration.rs`, so CI does not need a server. Refresh fixtures
with:

```sh
scripts/gen_fixtures.sh
```

The script follows the `clickhouse-connect` local test convention:
`CLICKHOUSE_CONNECT_TEST_HOST`, `CLICKHOUSE_CONNECT_TEST_PORT`,
`CLICKHOUSE_CONNECT_TEST_USER`, and `CLICKHOUSE_CONNECT_TEST_PASSWORD`,
defaulting to `localhost:8123` as `default` with no password. When
`.server-ref` is moved or server framing behavior is being reconciled,
regenerate the fixtures and update `tests/fixtures/README.md` with the capture
version and first-byte hexdumps.

## Status

A working core under active development. The API is not yet stable. The read
path (Native decode, streaming, Arrow export) is implemented and verified
against live-server fixtures. Type coverage, compression, and the insert path
are in progress. `ARCHITECTURE.md` describes how the pieces fit together.
