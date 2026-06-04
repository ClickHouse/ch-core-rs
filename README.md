# ch-core-rs

Experimental shared Rust core for ClickHouse client internals.

The current POC decodes ClickHouse `FORMAT Native` bytes into columnar Rust
buffers and can export those buffers through the Arrow C Data Interface. The
goal is to implement low-level ClickHouse type decoding once, then let each
language client expose the decoded columnar data in the way that fits that
runtime.

This crate is pure Rust with zero dependencies and no Python or JavaScript
binding code.

## Current Scope

Implemented:

- Decode ClickHouse Native blocks from a complete byte buffer.
- Incrementally decode Native blocks from streamed byte chunks.
- Preserve ClickHouse blocks as separate columnar chunks.
- Store primitive values, strings, booleans, and nullability in Arrow-compatible
  layouts.
- Export decoded chunks as an Arrow C Data stream.

Not implemented yet:

- Native encoding for inserts.
- TCP/native protocol packet framing.
- Compression framing.
- Decimal, temporal, LowCardinality, Enum, UUID/IP, Array, Tuple, or Map types.
- Language-specific materialization policy.

## Binding Model

Bindings live in the language client repos and depend on this crate.

- Python: a PyO3 binding can expose decoded data as Python rows/columns or as an
  Arrow C Data stream for PyArrow/Pandas/Polars.
- JavaScript/Node: a napi-rs binding can expose decoded data as typed arrays,
  validity bitmaps, and streamed columnar chunks.

The core owns ClickHouse binary decoding and the shared columnar model. Bindings
own runtime-specific behavior such as Python `int` versus JavaScript `BigInt`,
native null handling, stream/backpressure integration, and public client APIs.

## Consuming

Local development:

```toml
[dependencies]
ch-core-rs = { path = "/path/to/ch-core-rs" }
```

Pinned git dependency:

```toml
[dependencies]
ch-core-rs = { git = "ssh://git@github.com/ORG/ch-core-rs.git", rev = "<commit>" }
```

Use a local `[patch]` in `.cargo/config.toml` when you want to override a pinned
git dependency with a local checkout during development.

## Layout

- `src/schema.rs` - ClickHouse logical type model.
- `src/column.rs` - Arrow-compatible physical column buffers.
- `src/batch.rs` - `ColBatch` and `ChunkedBatch` result model.
- `src/bitmap.rs` - validity bitmap conversion and storage.
- `src/native/` - Native-format varints, block decode, and stream decode.
- `src/ffi.rs` - Arrow C Data Interface export.

## Data Model

Decoded results are represented as:

```text
ChunkedBatch
  schema
  chunks: Vec<Arc<ColBatch>>
    columns: Vec<Column>
      typed values / offsets / data / bitmaps
```

ClickHouse Native blocks remain separate chunks. This avoids merging and
repacking buffers, and maps naturally to Arrow record batches.

## Supported Types

Current decoder support:

- `Bool`
- `Int8`, `Int16`, `Int32`, `Int64`
- `UInt8`, `UInt16`, `UInt32`, `UInt64`
- `Float32`, `Float64`
- `String`
- `FixedString(N)`
- `Nullable(T)` where `T` is one of the supported inner types

Unsupported types raise a decode error.

See `DECODER_CONTRACT.md` for the definitive per-type reference: the wire
payload, the decoded `Column` buffers, and the Arrow C Data export for every
supported type.

## Testing

Run the crate checks with:

```sh
cargo test
```

The integration suite in `tests/integration.rs` decodes committed
`FORMAT Native` fixture bytes captured from a live ClickHouse server, so CI does
not need a server. Refresh those fixtures with:

```sh
scripts/gen_fixtures.sh
```

The script follows the `clickhouse-connect` local test convention:
`CLICKHOUSE_CONNECT_TEST_HOST`, `CLICKHOUSE_CONNECT_TEST_PORT`,
`CLICKHOUSE_CONNECT_TEST_USER`, and `CLICKHOUSE_CONNECT_TEST_PASSWORD`, defaulting
to `localhost:8123` as `default` with no password. When `.server-ref` is moved
or server framing behavior is being reconciled, regenerate the fixtures and
update `tests/fixtures/README.md` with the capture version and first-byte
hexdumps.

## Streaming

`native::stream_decoder::StreamDecoder` accepts arbitrary byte chunks:

```text
feed(bytes) -> complete decoded blocks
finish()    -> final blocks or truncated-stream error
```

It retains incomplete trailing bytes between calls and emits complete
`ColBatch` values as soon as enough data has arrived. Transport-level
backpressure is still a binding or client responsibility.

## Status

This is a POC, not a production-ready public API. The crate is intended for
review and experimentation with Python and Node binding branches.
