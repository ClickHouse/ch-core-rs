# ch-core-rs

A shared Rust core for the ClickHouse `FORMAT Native` wire format: it decodes
Native bytes into typed, Arrow-compatible columnar buffers, and encodes those
buffers back to Native bytes for `INSERT`. Pure Rust, zero runtime dependencies.
The codec is implemented once here. Each language client wraps it with a thin
binding.

Every ClickHouse client reimplements the same work today: type string parsing,
Native block decoding, null maps, and the long tail of types. Each client
redoes that work on every server release. This crate does it once. A
wire-format bug produces silently corrupt columns, so one audited decoder
beats one per client. The result is a single implementation that is correct,
maintained in one place, and built for zero-copy, streaming delivery into
native containers.

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

The diagram shows the decode (read) path. The same columnar buffers also encode
back to `FORMAT Native` bytes for the `INSERT` path.

## Division of labor

The core owns binary decoding and encoding, the ClickHouse logical type model,
and the shared Arrow-compatible buffer layout.

Bindings live in the client repos and own everything runtime-specific: Python
`int` versus JavaScript `BigInt`, null handling, stream and backpressure
integration, and the public client API. Bindings are thin adapters over
buffers the core has already filled.

Implement a ClickHouse type once in the core and every client gets it. The
buffers follow Arrow layout conventions, so results can export zero-copy through
the Arrow C Data or C Stream interfaces in runtimes that can import those
interfaces in-process.

## Why this exists

1. One correct implementation, not N. Wire decoding is the highest-risk code
   in a client, and today it is duplicated in every language.
2. ClickHouse type fidelity. The core's type model preserves ClickHouse
   semantics that host or Arrow types can blur, such as `DateTime64` precision
   and timezone, `FixedString` width, and exact signed/unsigned integer width.
   Presentation policy stays in the bindings.
3. Speed. Decoding once in Rust, with zero-copy delivery into native containers
   and no per-cell work, is built to be fast. End-to-end speed also depends on
   the binding and transport, so the measured numbers stay a binding concern.
4. Streaming. The decoder accepts transport byte chunks as they arrive and emits
   decoded column chunks as each block completes. Decode overlaps the network
   instead of waiting for the full response.

## How it works

The decoder reads `FORMAT Native` bytes, from a complete buffer or
incrementally from streamed chunks, and produces:

```text
ChunkedBatch
  schema                      ClickHouse logical types (ChType)
  chunks: Vec<Arc<ColBatch>>  one chunk per non-empty Native block, never merged
    columns: Vec<Column>      typed values / offsets / data / bitmaps
```

Non-empty Native blocks remain separate chunks. Zero-row blocks contribute the
schema but are dropped from `chunks`. This avoids merging and repacking buffers
and maps directly onto Arrow record batches. Buffer layouts follow Arrow
conventions throughout: fixed-width columns are one contiguous typed buffer,
strings are offsets plus a data buffer, booleans and validity are bit-packed
bitmaps.

Streaming uses a push API:

```text
StreamDecoder::feed(bytes) -> complete decoded blocks
StreamDecoder::finish()    -> final blocks or truncated-stream error
```

The streaming decoder retains incomplete trailing bytes between calls and
emits complete `ColBatch` values as soon as enough data has arrived.
Transport-level backpressure stays a binding or client responsibility.

`ARCHITECTURE.md` covers the decode path, streaming machinery, and the Arrow
C Data export in detail. `CODEC_CONTRACT.md` is the definitive per-type
contract for both directions: wire payload, decoded buffers, and Arrow export
for every supported type, plus the encode-side contract (preconditions, choices,
and round-trip guarantees) for turning those buffers back into Native bytes.

## Current scope

Implemented:

- Decode ClickHouse Native blocks from a complete byte buffer.
- Incrementally decode Native blocks from streamed byte chunks.
- Preserve ClickHouse blocks as separate columnar chunks.
- Store primitive values, strings, booleans, temporal values, and nullability
  in Arrow-compatible layouts.
- Export decoded chunks as an Arrow C Data stream.
- Malformed-input hardening: untrusted wire bytes return errors, never panic.
- Encode every supported column back to Native block bytes for `INSERT`
  (`native::encode`); accepted by a live server over HTTP.

Not implemented yet:

- TCP/native protocol packet framing.
- Compression framing.
- Language-specific materialization policy (bindings own this, by design).

## Supported types

- `Bool`
- `Int8`, `Int16`, `Int32`, `Int64`
- `UInt8`, `UInt16`, `UInt32`, `UInt64`
- `Float32`, `Float64`, `BFloat16`
- `QBit(BFloat16|Float32|Float64, N)`
- `Nothing`
- `String`
- `FixedString(N)`
- `Date`, `Date32`, `DateTime`, `DateTime64(P[, tz])`, `Time`, `Time64(P)`
- `IntervalYear`, `IntervalQuarter`, `IntervalMonth`, `IntervalWeek`,
  `IntervalDay`, `IntervalHour`, `IntervalMinute`, `IntervalSecond`,
  `IntervalMillisecond`, `IntervalMicrosecond`, `IntervalNanosecond`
- `Nullable(T)` where `T` is one of the supported inner types
- `Decimal(P, S)`
- `UUID`, `IPv4`, `IPv6`
- `Enum8(...)`, `Enum16(...)`
- `Int128`, `UInt128`, `Int256`, `UInt256`
- `Array(T)`, `Tuple(T1, ...)`, `Map(K, V)`, `Variant(T1, ...)`, and `Dynamic`
- `JSON`, `SimpleAggregateFunction(func, T)`, the seven geo aliases, `Geometry`,
  and `Nested(...)`
- Exact `AggregateFunction` state codecs for `count`, canonical
  `nothingUInt64` and `nothingNull`, and base `sum` over plain or Nullable
  numeric and Enum arguments
- `LowCardinality(T)` for the allowed inner types above, excluding server-forbidden
  combinations such as `Nothing`, `Decimal`, `DateTime64`, `Time64`, and `Enum`

Unsupported types raise a decode error rather than guessing.

## Roadmap

In rough priority order:

1. Sparse column serialization.
2. Compression framing: LZ4, then ZSTD.
3. Per-runtime zero-copy adapters: JS `TypedArray` over an external
   `ArrayBuffer`, NumPy export that does not route through Arrow.
4. Insert-path completion: a streaming/sink encode API. Type parity is kept as
   each decoder type lands.
5. Native TCP protocol engine: handshake, query/data/progress/exception
   packets, revision negotiation.

## Consuming

Bindings live in the language client repos and depend on this crate.
See `INTEGRATING.md` for guidance aimed at downstream client maintainers.

The canonical way to depend on the crate is a git dependency pinned to a
release tag:

```toml
[dependencies]
ch-core-rs = { git = "https://github.com/ClickHouse/ch-core-rs.git", tag = "v0.1.0" }
```

Releases are semver git tags: patch and minor tags carry no breaking changes,
and every release is documented in `CHANGELOG.md`. The crate is not published to
crates.io by design, so pin the tag directly. The MSRV (`rust-version = 1.81`)
is enforced for git dependencies too, so a consuming crate must build on Rust
1.81 or newer.

For development you can point at a local checkout or pin an exact commit:

```toml
[dependencies]
# Local checkout.
ch-core-rs = { path = "/path/to/ch-core-rs" }

# Exact commit, for bisecting or tracking an unreleased fix.
ch-core-rs = { git = "https://github.com/ClickHouse/ch-core-rs.git", rev = "<commit>" }
```

Use a local `[patch]` in `.cargo/config.toml` to override a pinned git
dependency with a local checkout during development.

Decode a complete buffer with `native::decode::decode_all_bytes`, or stream
with `native::stream_decoder::StreamDecoder`. Encode a batch back to Native
block bytes for `INSERT` with `native::encode::encode_block` or
`native::encode::encode_chunked` (every type the crate decodes it also encodes;
`COMPLETENESS.md` is the per-type tracker). Server output configured with
`output_format_native_encode_types_in_binary_format=1` uses the explicit
`decode_*_binary_types` entry points. Bytes from `encode_*_binary_types` require
`input_format_native_decode_types_in_binary_format=1` on the receiving INSERT.

## Repo layout

- `src/schema.rs` - ClickHouse logical type model.
- `src/column.rs` - Arrow-compatible physical column buffers.
- `src/batch.rs` - `ColBatch` and `ChunkedBatch` result model.
- `src/bitmap.rs` - validity bitmap conversion and storage.
- `src/native/` - Native-format varints, block decode, stream decode, and block encode.
- `src/ffi/mod.rs` - Arrow C Data Interface export.

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

A working core under active development. The API is not yet stable. Decode and
encode are at type parity, both verified against a live ClickHouse server. Not
yet implemented: compression framing and the native TCP protocol; a
streaming/sink encode API is future work. Releases are tagged and semver'd, each
recorded in `CHANGELOG.md`. `ARCHITECTURE.md` describes how the pieces fit
together.

## License

Apache-2.0. See `LICENSE`.
