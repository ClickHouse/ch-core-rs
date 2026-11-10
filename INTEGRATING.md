# Integrating ch-core-rs

This is a high-level guide for maintainers of the ClickHouse language clients who want to evaluate or wrap `ch-core-rs`.

This crate decodes ClickHouse `FORMAT Native` result bytes into owned, typed,
columnar buffers, and encodes those buffers back into `FORMAT Native` bytes for
`INSERT`. It also reads ClickHouse LZ4/NONE compression frames and writes LZ4
frames. Transport, TCP packets, query APIs, and host object materialization stay
outside core.

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
| strip packet framing, use     |
| core compression codec,       |
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

The client owns HTTP responses and native TCP packets. It can use core's
compression codec to obtain raw Native block bytes, then use the Native codec
to obtain typed columnar buffers.

## What This Is

`ch-core-rs` is a pure Rust codec for the ClickHouse `FORMAT Native` wire format. On the read side a client asks the server for Native output, obtains the response bytes, and passes those bytes to the core, which parses column names and type strings, decodes one Native block at a time, and stores the values in Arrow-shaped buffers. On the write side the core encodes those same buffers back into Native block bytes for `INSERT ... FORMAT Native`.

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

### Depending On The Crate

Pin the crate by release tag, for example
`ch-core-rs = { git = "https://github.com/ClickHouse/ch-core-rs.git", tag = "v0.1.0" }`.
Releases are semver git tags, each documented in `CHANGELOG.md`; watch the
changelog when you repin. Commit `Cargo.lock` in the binding repo so wheel and
native-extension builds are reproducible. The crate is not on crates.io by
design. See `README.md` for the local-path and commit-pin forms used during
development.

## Getting Native Bytes

The core decodes the `Native` format.

A client can get bytes for this core by requesting `FORMAT Native` over the HTTP transport. The binding must hand the core the raw Native payload after transport concerns have been handled:

- HTTP transport compression such as gzip must be decompressed first. For
  ClickHouse's separate `compress=1` envelope, use `compression` to recover the
  Native bytes.
- Server exceptions, retries, cancellation, and connection lifecycle remain client responsibilities.
- The default decode APIs expect string-encoded column type headers. If the
  request enables `output_format_native_encode_types_in_binary_format`, use the
  corresponding `decode_*_binary_types` API or
  `StreamDecoder::new_binary_types`; do not mix the two modes.

For INSERT, the default encode APIs write textual headers. If a binding uses an
`encode_*_binary_types` API, it must set
`input_format_native_decode_types_in_binary_format=1` on the ClickHouse request.

The same model applies over TCP. The sans-I/O engine in `crates/ch-tcp-proto`
owns the handshake, revision gating, packet framing, and server packet
dispatch. It reports Progress, ProfileInfo, Log, ProfileEvents, Totals,
Extremes, Exception, and EndOfStream as typed events. It does not own sockets,
TLS, timeouts, pooling, or cancellation: there is no Cancel packet yet, so a
driver that abandons a query mid-stream must discard the connection. The first
engine handles uncompressed SELECT only. See `crates/ch-tcp-proto/README.md`
for the driver loop and a tested `run_select` example.

The engine conservatively fails a query when its first response is an
Exception and that query sent wire settings. This safely covers settings parse
errors raised before the server creates a query context, but can also classify
a reusable syntax error as terminal. A driver must close its socket whenever
the engine enters `State::Failed`, even if the server has not closed its side.
Where a binding can safely construct it, an SQL `SETTINGS` clause avoids the
wire-settings flag and this conservative false positive.

Each `Event::Data` payload is exactly one complete Native block at the
connection's `effective_revision()`, and the payloads concatenate into one
Native stream. Either feed that stream to `StreamDecoder`, or decode each
payload as it arrives with `ByteReader` plus `decode_next_block` into a
`ch_core_rs::batch::ColBatch`. Prefer the per-payload form: `StreamDecoder`
scans for block boundaries again before decoding, while the engine has already
found them. The engine README has a tested `decode_payload` snippet.

A TCP SELECT result with columns starts with a zero-row Data block that carries
only the schema, which HTTP `FORMAT Native` does not send. Both decode paths
return it as a zero-row `ColBatch` first, so bindings must tolerate zero-row
batches on TCP even where their HTTP path never saw one. A result with no
columns need not send a schema block.

Not all clients have a ready-to-use `FORMAT Native` result path today. Part of the integration task for those clients is to add or reuse a path that can request `FORMAT Native` and expose the resulting decompressed byte stream to the Rust decoder.

### Protocol Revision

Native block framing is partly controlled by the negotiated protocol revision, and that revision is not carried in the block bytes. The binding must pass the right `DecodeOptions.protocol_revision`:

- Use `0` for bare HTTP `FORMAT Native` responses with no `client_protocol_version` setting.
- For native TCP, advertise at most `DBMS_TCP_PROTOCOL_VERSION`, then pass the
  effective negotiated revision for Native payloads received through that
  connection.
- For HTTP, request a `client_protocol_version` no greater than
  `DBMS_TCP_PROTOCOL_VERSION`, then pass the effective revision the response was
  actually produced with. The server can cap the requested value further.

Do not negotiate a higher revision and merely clamp the value passed to the
core afterward. The server would already have serialized the bytes using the
newer layout. Decode and encode reject revisions above
`DBMS_TCP_PROTOCOL_VERSION` so an unsupported negotiation fails explicitly
instead of silently misframing String columns.

This matters because framed Native blocks can include a `BlockInfo` preamble, and modern revisions include a per-column custom-serialization marker. Passing the wrong revision can shift the decoder by one or more bytes and corrupt the whole block.

The repo's `all_types_rev54485.native` fixture is an HTTP `FORMAT Native` capture
with `client_protocol_version=54485` against the pinned
`v26.8.1.2041-lts` server. It exercises both the `BlockInfo` preamble and the
modern per-column marker while staying below revision 54492's unsupported
String size-stream layout. That is the concrete shape bindings should reproduce
when they request protocol-framed Native over HTTP at that revision.

### One Block At A Time For TCP

To decode one complete block, use `native::varint::ByteReader` plus
`native::decode::decode_next_block`. To find its boundary without decoding,
use the scanner below. Set `DecodeOptions.protocol_revision` to the
connection's effective revision. The first engine advertises 54460; core's
decode ceiling remains 54485.

For an uncompressed body, construct
`native::stream_decoder::BlockScanner` with `DecodeOptions.protocol_revision`
set to the effective connection revision. Append packet bytes to a bounded
engine-owned buffer and call `scan` with the whole accumulated block prefix.
`BlockBoundary::NeedMore` means retain that prefix and wait for more bytes.
`BlockBoundary::Complete` reports the exact `consumed` prefix plus the declared
`num_columns` and `num_rows`; leave any suffix for packet dispatch. The shape
identifies the zero-column, zero-row TCP end marker without decoding a
`ColBatch`. The scanner copies no payload bytes and creates no decoded column
buffers. It checkpoints parsed column headers and complete top-level columns,
and resumes inside bodies composed from String, fixed-width values, Nullable, Array, Map, Tuple,
LowCardinality, Variant, Dynamic, JSON, and AggregateFunction, including
top-level binary descriptors, runtime type-table prefixes and nested containers.
Progress survives each feed, so scanning remains linear across transport read sizes. Call `reset`
before reusing a scanner after abandoning an incomplete block.

Scan and decode agree on acceptance or rejection and complete-block boundaries.
Error wording and the point at which corruption is reported can differ. On a
truncated stream, header corruption, an invalid Dynamic index, or an
AggregateFunction zero-state byte can produce invalid data instead of a
truncation error.

The decoder and scanner reject repeated known `BlockInfo` fields. The server
writer emits each field once; rejecting repeats bounds preamble work even
though the server reader accepts them.

For a compressed body, keep one `BlockScanner` and one plaintext buffer
`plain` that holds only the current block. For each frame:

1. Save `prior = plain.len()`.
2. Call
   `compression::decompress_frame_into(input, ChecksumMode::Verify, &mut plain)`
   to append the frame directly to the block buffer. It reuses the buffer's
   capacity and restores its contents on error. Advance the compressed input
   by `FrameInfo::consumed()`; `plaintext_len()` is the number of bytes
   appended.
3. Call `scanner.scan(&plain)`. The scanner resumes from its checkpoint, so
   each frame costs roughly the bytes it added rather than a rescan of the
   whole block. Do not call `block_end` after each frame: it restarts from the
   start of the block every time, which is quadratic in the frame count for a
   large block.
4. On `BlockBoundary::NeedMore`, load the next frame. On
   `BlockBoundary::Complete { consumed, .. }`, call
   `info.finish_block(consumed - prior)` to check that all plaintext in the
   final frame belongs to that block, then decode `&plain[..consumed]`. The
   count passed to `finish_block` is relative to the final frame, not the
   accumulated block. Return to packet dispatch without reading the next frame
   or packet.

Core rejects zero-plaintext frames; they are not end-of-input markers. Bound
the accumulated body and transport buffers according to the engine's resource
policy, and honor core's per-frame size checks.

Confirmed at `v26.8.1.2041-lts`, `TCPHandler::sendData`, `sendTotals`, and
`sendExtremes` in `src/Server/TCPHandler.cpp`, and `Connection::sendData` in
`src/Client/Connection.cpp`, flush the compression buffer after writing each
Native block. A large block can occupy multiple frames; a frame never spans
two TCP blocks. Requiring zero leftover plaintext at block end is the derived
reader check for this server behavior. HTTP `compress=1` enables output
auto-flush after each chunk, so default HTTP output also aligns frames with
blocks. Confirmed `HTTPHandler::Output::pushDelayedResults` in
`src/Server/HTTPHandler.cpp` copies buffered chunks together into the compressor
with `wait_end_of_query=1` or `http_response_buffer_size>0`. HTTP readers must
therefore accept a continuous compressed stream with frames crossing Native
block boundaries and must not apply the TCP check.

For inserts, pass each encoded Native block separately to
`compression::compress_lz4_frames(&native, 1_048_576)`. This bounds each
frame's plaintext to 1 MiB and flushes at the block boundary. For a caller
that already splits its input, `write_lz4_frame` appends a single frame to an
existing output buffer. Empty end markers are encoded Native blocks with
nonempty bytes; an empty byte slice is not a valid compression frame.

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

The core covers the full ClickHouse scalar set plus containers (`Array`, `Tuple`, `Map`, `Nested`), `Variant`, `Dynamic`, `JSON`, wide integers, `LowCardinality(T)`, the geo types, `QBit`, a set of `AggregateFunction`/`SimpleAggregateFunction` states, and `Nullable(T)` over the supported inner types. The "Supported types" list in `README.md` is the current enumeration, and `CODEC_CONTRACT.md` is the per-type wire and buffer contract; this guide does not repeat the list so it cannot drift. Unsupported types fail with `DecodeError::UnsupportedType` on decode (and `EncodeError::UnsupportedType` on encode).

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

// `DecodeOptions` is `#[non_exhaustive]`, so construct it with `default()` and
// then assign fields rather than using a struct literal.
// `max_synthetic_allocation_bytes` is the per-block cumulative ceiling shared
// by synthetic allocations across all columns. It resets for every block and
// defaults to 256 MiB; lower it only for a memory-constrained binding.
let mut options = DecodeOptions::default();
options.protocol_revision = protocol_revision;

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

For an outer protocol that interleaves Native blocks with packet headers,
`BlockScanner` finds one exact block boundary without materializing its typed
columns. Advance the outer input by the `consumed` field from
`BlockBoundary::Complete`; any suffix belongs to the outer protocol. The caller
retains incomplete block bytes, supplies the decode options, and imposes its
own block byte limit.

`ch-tcp-proto` uses this scanner for uncompressed revision-54460 SELECTs.
Its `Event::Data` payloads concatenate into a framed Native result stream for
the ordinary streaming API above. The TCP empty end-of-data marker is consumed
by the engine; schema-only result blocks remain in the byte stream. Other
packet bodies arrive through separate event variants.

## Encoding For Insert

The core also runs the inverse direction: it encodes the same `Column` buffers back into `FORMAT Native` block bytes a server accepts for `INSERT ... FORMAT Native`. A binding can build an insert path on the same columnar model it already uses for results.

```rust
use ch_core_rs::native::encode::{encode_block, encode_chunked, EncodeOptions};

// `EncodeOptions` is `#[non_exhaustive]`, so construct it with `default()` and
// then assign fields rather than using a struct literal. Its `protocol_revision`
// defaults to 0, the value the HTTP INSERT body wants, so `default()` is enough
// here.
let options = EncodeOptions::default();

// One Native block from one ColBatch.
let bytes = encode_block(&col_batch, &options)?;

// A whole ChunkedBatch: one Native block per chunk, concatenated.
let body = encode_chunked(&chunked_batch, &options)?;
```

Send those bytes as the body of an `INSERT INTO t FORMAT Native` request. Notes for a binding:

- Protocol revision: use `0` for the HTTP `INSERT ... FORMAT Native` path. The
  server parses that body at revision 0, so encode writes no `BlockInfo`
  preamble and no per-column marker, and the stream ends at EOF. For TCP, use
  the connection's effective revision. Frame each encoded block separately
  when compression is enabled, including the empty end marker. Packet codes
  and block names stay outside the compressed envelope. Transport and packet
  framing remain the client's responsibility.
- Coverage: encode is at full type parity with decode. Every type the crate decodes it also encodes, so a batch the core produced round-trips back to Native bytes. Only a type the decoder itself does not support returns `EncodeError::UnsupportedType`.
- Validation: the input is your in-memory buffers, not wire bytes, but encode still validates the whole batch before writing anything and returns `EncodeError::InconsistentBatch` on a malformed column (wrong length, bad offsets, width mismatch, out-of-range `LowCardinality` index, a null in a non-nullable column). A rejected batch produces no partial bytes.
- Round trip: `decode(encode(x))` reproduces the buffers. The "Encoding" section of `CODEC_CONTRACT.md` has the precise preconditions, encoder choices, and round-trip guarantees.

## What Stays In The Binding

The core stops at typed buffers plus ClickHouse logical type metadata. The binding will need to own:

- Transport, HTTP transport decompression, streaming, cancellation, retries,
  and backpressure.
- Public client API shape.
- Host scalar mapping, for example `UInt64` as Python `int` or JavaScript `BigInt`.
- Temporal presentation, for example how to apply a `DateTime` timezone or whether `DateTime64` becomes a host datetime object.
- Null representation, for example `None`, `null`, optional values, masked arrays, or Arrow validity.
- Row vs column result shaping.
- Future type policy as the core grows.
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

- `README.md` for the data model and current scope.
- `ARCHITECTURE.md` for the decode path, streaming, Arrow C Data export, and the encode path.
- `CODEC_CONTRACT.md` for the per-type wire, decoded-buffer, and Arrow contract,
  plus the encode-side contract for the insert path.
