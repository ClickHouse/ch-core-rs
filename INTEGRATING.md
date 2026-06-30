# Integrating ch-core-rs

This is a high-level guide for maintainers of the ClickHouse language clients who want to evaluate or wrap `ch-core-rs`.

In short, this crate decodes ClickHouse `FORMAT Native` result bytes into owned, typed, columnar buffers. It does not own transport, compression, query APIs, or host object materialization.

## Where It Fits

```text
Client query API
      |
      |  SELECT ... FORMAT Native
      v
+-------------------------------+
| Language client transport     |
| HTTP or TCP, auth, settings,  |
| retries, cancellation         |
+-------------------------------+
      |
      |  response bytes or TCP packets
      v
+-------------------------------+
| Client-owned byte handling    |
| decompress, strip framing,    |
| handle progress/errors/EOF    |
+-------------------------------+
      |
      |  raw Native block bytes
      |  + effective protocol_revision
      v
+-------------------------------+
| ch-core-rs                    |
| parse names/types, decode     |
| blocks, validate schema       |
+-------------------------------+
      |
      |  ChunkedBatch
      |  one chunk per non-empty block
      |  Vec<T>, offsets + data,
      |  validity and bool bitmaps
      v
+-------------------------------+
| Language-owned wrapper        |
| Arrow stream, columns, rows,  |
| scalar/time/null policy       |
+-------------------------------+
      |
      v
Public client result API
```

The important boundary is the middle one: the client owns everything needed to
turn an HTTP response or native TCP packet stream into raw Native block bytes.
`ch-core-rs` starts at those block bytes and stops at typed columnar buffers.

## What This Is

`ch-core-rs` is a pure Rust decoder for the ClickHouse `FORMAT Native` wire format. A client asks the server for Native output, obtains the response bytes, and passes those bytes to the core. The core parses column names and type strings, decodes one Native block at a time, and stores the values in Arrow-shaped buffers.

### Features

- It has zero runtime dependencies. i.e. `Cargo.toml` has no `[dependencies]`.
- It is downstream agnostic. The core owns binary decoding and the shared columnar model. It does not build host-language objects or make language-specific policy decisions.
- The decoder accepts either a complete byte buffer or streamed byte chunks. The streaming API lets a binding overlap network reads with decode and emit decoded blocks as they complete.

## Current Integration Surface

The crate exposes Rust APIs today. Non-Rust clients should build a small, language-owned wrapper around the Rust core.

That wrapper is where each client should integrate its runtime, packaging, public result API, async model, fallback behavior, and host value policies. Keeping that layer client-owned is the practical path right now because those concerns differ substantially across Python, JavaScript, Java, C#, Go, and C++.

The Python POC uses a PyO3 extension crate that depends on `ch-core-rs`, decodes Native bytes, exposes Python rows/columns, and implements `__arrow_c_stream__` by wrapping the core's Arrow stream export.

One important caveat: the Python POC predates the current numeric `DecodeOptions.protocol_revision` model. Its public `has_block_info` boolean is useful as POC context, but downstream bindings should copy the core's numeric revision model instead.

Candidate bridge choices:

| Client  | Practical bridge                                          |
|---------|-----------------------------------------------------------|
| Rust    | Depend on `ch-core-rs` directly                           |
| Python  | PyO3 + maturin                                            |
| Node.js | napi-rs                                                   |
| C++     | Client-owned Rust interop layer, for example `cxx`        |
| Java    | Client-owned JNI extension                                |
| C#      | Client-owned native component plus managed facade         |
| Go      | Client-owned native component with Go packaging decisions |

## Getting Native Bytes

The core decodes the `Native` format.

A client can get bytes for this core by requesting `FORMAT Native` over the HTTP transport. The binding must hand the core the raw Native payload after transport concerns have been handled:

- HTTP response compression must be decompressed first.
- Server exceptions, retries, cancellation, and connection lifecycle remain client responsibilities.
- Do not enable `output_format_native_encode_types_in_binary_format`. The current core expects string-encoded column type headers, which is the default Native output mode.

The same model applies over TCP. A TCP client must do the native protocol work first: handshake, revision negotiation, packet framing, compression framing, server packet dispatch, progress and exception handling, cancellation, and end-of-stream handling. Once it has the Native block payload bytes from server `Data` packets, it can feed those bytes to the core.

Not all clients have a ready-to-use `FORMAT Native` result path today. Part of the integration task for those clients is to add or reuse a path that can request `FORMAT Native` and expose the resulting decompressed byte stream to the Rust decoder.

### Protocol Revision

Native block framing is partly controlled by the negotiated protocol revision, and that revision is not carried in the block bytes. The binding must pass the right `DecodeOptions.protocol_revision`:

- Use `0` for bare HTTP `FORMAT Native` responses with no `client_protocol_version` setting.
- Use the negotiated native TCP revision for Native payloads received through the native TCP protocol.
- Use the effective `client_protocol_version` for HTTP responses where the client sets that ClickHouse setting. The server can cap the revision, so use the revision the response was actually produced with.

This matters because framed Native blocks can include a `BlockInfo` preamble, and modern revisions include a per-column custom-serialization marker. Passing the wrong revision can shift the decoder by one or more bytes and corrupt the whole block.

The repo's `all_types_rev54485.native` fixture is an HTTP `FORMAT Native` capture with `client_protocol_version=54485` against the pinned `v26.6.1.1193-stable` server; it exercises both the `BlockInfo` preamble and the modern per-column marker. That is the concrete shape bindings should reproduce when they request protocol-framed Native over HTTP at that revision.

## Output Model

The Rust output is:

```text
ChunkedBatch
  schema: Schema
  chunks: Vec<Arc<ColBatch>>
    ColBatch
      schema: Schema
      columns: Vec<Column>
      num_rows: usize
```

Each non-empty ClickHouse Native block becomes one `ColBatch` chunk. Zero-row blocks establish or validate the schema but are dropped from `chunks`. Chunks are not merged. That maps cleanly onto Arrow record batches and avoids repacking large buffers, especially bit-packed bool and validity buffers.

The physical column buffers are Arrow-shaped:

- Fixed-width numerics and temporal values: one contiguous `Vec<T>`.
- `Bool`: bit-packed data bitmap.
- `Nullable(T)`: bit-packed validity bitmap, where bit 1 means valid.
- `String`: `i32` offsets plus one data buffer.
- `FixedString(N)`: one contiguous fixed-width byte buffer.

Supported types today:

- `Bool`
- `Int8`, `Int16`, `Int32`, `Int64`
- `UInt8`, `UInt16`, `UInt32`, `UInt64`
- `Float32`, `Float64`
- `String`
- `FixedString(N)`
- `Date`, `Date32`, `DateTime`, `DateTime64(P[, tz])`
- `Nullable(T)` where `T` is one of the supported inner types

Unsupported types fail with `DecodeError::UnsupportedType`.

ClickHouse `String` is arbitrary bytes, not guaranteed UTF-8. The core stores the raw bytes. The current Arrow export uses Arrow utf8 format because the physical layout is offsets plus data, but strict Arrow consumers may reject invalid UTF-8 when importing or later validating the array. A binding that needs byte-faithful behavior should validate first, choose a bytes/binary fallback, or return a clear unsupported-for-Arrow error for invalid strings.

## Consumption Strategies

There are three main consumption strategies. Pick the one that best fits the client API.

1. Arrow stream export: If the target runtime can import an Arrow C Stream in-process, expose `ArrowArrayStream` and let the Arrow implementation consume the decoded buffers without copying values. This is the best fit for PyArrow-style APIs. It still requires binding glue in each runtime.
2. Columnar materialization: Walk each decoded column once and build a host array, typed array, vector, or column object. This keeps work at column or block granularity and is usually the best non-Arrow path.
3. Row materialization: Build tuples, records, or objects from the decoded buffers. This supports row-oriented client APIs, but it is the most expensive path because host object allocation happens per row or per cell.

Across all three paths you'll want to cross the language boundary at block, stream, or column granularity, never per cell. If a binding needs host rows, build them inside the native extension and return a whole batch of rows.

The Rust call shape is intentionally small:

```rust
use ch_core_rs::native::decode::{decode_all_bytes, DecodeOptions};
use ch_core_rs::native::stream_decoder::StreamDecoder;

let options = DecodeOptions { protocol_revision };

// Complete buffer.
let result = decode_all_bytes(native_bytes, &options)?;

// Streaming.
let mut decoder = StreamDecoder::new(options);
for chunk in decompressed_native_chunks {
    for block in decoder.feed(chunk.as_ref())? {
        // Hand the completed block to the binding.
    }
}
for block in decoder.finish()? {
    // Flush final completed blocks, or surface truncated-stream errors.
}
```

## What Stays In The Binding

The core stops at typed buffers plus ClickHouse logical type metadata. The binding will need to own:

- Transport, decompression, streaming, cancellation, retries, and backpressure.
- Public client API shape.
- Host scalar mapping, for example `UInt64` as Python `int` or JavaScript `BigInt`.
- Temporal presentation, for example how to apply a `DateTime` timezone or whether `DateTime64` becomes a host datetime object.
- Null representation, for example `None`, `null`, optional values, masked arrays, or Arrow validity.
- Row vs column result shaping.
- Future type policy as the core grows, for example UUID, Enum, Decimal, LowCardinality, containers, and wide integers.
- Unsupported-type fallback policy, for example returning a clear error or retrying through the existing client path.

The Python POC may be a useful reference for this split. The core decodes `DateTime64` as `i64` ticks plus schema metadata. The PyO3 binding decides how to turn that into Python `datetime` objects, how to handle UTC-equivalent timezones, how to expose rows and columns, and how to package the Arrow C Stream as a Python capsule.

## Suggested Way Forward

For each client, if you decide to try consuming this and build a POC, a reasonable integration plan would be:

1. If it doesn't exist, add an internal query path that requests `FORMAT Native` and yields raw, decompressed response chunks.
2. Build a language-owned Rust wrapper or native extension.
3. Feed chunks into `StreamDecoder` with the correct protocol revision.
4. Start with one output surface: Arrow stream for Arrow-first clients, or columnar host arrays for clients with typed-array/vector APIs.
5. Add row materialization only as an adapter over the decoded column buffers.
6. Gate rollout by type coverage. Unsupported types should fall back to the existing client path or return a clear unsupported-type error.

## Where To Read Next

The following docs were agent generated and mostly intended to be read by agents if you decide to pursue a POC bind in your client.

- `README.md` for the data model, current scope, and measured results.
- `ARCHITECTURE.md` for the decode path, streaming, and Arrow C Data export.
- `DECODER_CONTRACT.md` for the per-type wire and Arrow contract.
