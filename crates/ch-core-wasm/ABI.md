# ch-core-wasm C ABI

This is the public contract of `ch_core_wasm.wasm`. The JS glue in `js/` is
one host. Any WebAssembly host can call the same exports. Treat every name,
signature, error code, descriptor field and ownership rule below as API.

ABI version: **1**, returned by `chw_abi_version()`. A host must check it at
load time and refuse a version it does not know. Any breaking change to this
document bumps the version. Adding a new export or a new optional descriptor
field does not.

## Module

- Target `wasm32-unknown-unknown`, built with
  `cargo build -p ch-core-wasm --target wasm32-unknown-unknown`. A workspace
  wide wasm32 build is not supported: other members turn core's C `zstd`
  feature back on. Pointers and lengths are 32-bit. A host
  that reads them as signed `i32`, as JS does, must convert them to unsigned.
- No imports. The module exports `memory` plus the functions below.
- Single threaded. Calls must not overlap.
- The module does not panic on malformed input. A bug that does panic traps
  with `unreachable`, after which the instance is unusable and must be
  replaced.

## Memory rules

These rules are the reason the ABI looks the way it does.

1. **Linear memory can grow during any call.** Growth replaces the host's
   view of memory. In JS it detaches the old `memory.buffer`, and every
   TypedArray over it becomes empty. Addresses stay the same. A host must
   re-read the memory buffer after every call before it touches memory.
2. **Input is borrowed.** The host allocates with `chw_alloc`, writes its
   bytes, passes `(ptr, len)`, and frees with `chw_free` after the call
   returns. No call keeps a pointer to its input. Input must not point into
   memory the module owns, such as a result buffer or the last-error message,
   because the call may change or free it.
3. **Output is owned by a result handle.** A result's JSON descriptor and every
   buffer it references stay valid until `chw_result_free`. After that the
   memory is reused.
4. **Copy out, then free.** A host copies whatever it needs out of linear
   memory before calling `chw_result_free`, and before any call that could
   reuse the memory. The JS glue copies every buffer into a JS-owned TypedArray
   and never returns a view into linear memory, so results survive growth,
   frees and later calls.

## Errors

A call that fails returns `0` (or a null pointer) and records an error. A call
that returns a result handle clears it on success.

| Export | Returns |
| --- | --- |
| `chw_last_error_code() -> u32` | `0` none, `1` InvalidArg, `2` GenericFailure, `3` OutOfMemory |
| `chw_last_error_ptr() -> *const u8` | UTF-8 message, valid until the next call that records or clears an error |
| `chw_last_error_len() -> usize` | message byte length |

- **InvalidArg**: the input can never succeed, however many bytes arrive. This
  covers corrupt framing, unsupported types, invalid encode input, a bad
  handle, and lifecycle misuse such as pushing after finish.
- **GenericFailure**: an I/O-shaped failure, such as a truncated final block in
  `chw_stream_finish`.
- **OutOfMemory**: `chw_alloc` could not allocate.

## Handles

Results and streams are `u32` handles, never raw pointers. `0` is never a
valid handle. A handle carries a slot index in its low 20 bits and a
generation in its high 12 bits. Freeing bumps the generation, so a freed
handle stays invalid after its slot is reused, until that one slot has been
reused 4096 times. A stale, freed, forged or wrong-kind handle is an
`InvalidArg` error, never undefined behavior. At most about one million
handles can be live at once.

## Exports

### Version and memory

| Export | Meaning |
| --- | --- |
| `chw_abi_version() -> u32` | The ABI version. |
| `chw_alloc(len) -> *mut u8` | `len` bytes, 8-byte aligned. Null and `OutOfMemory` on failure. `len = 0` returns a dangling non-null pointer that must not be read or written. |
| `chw_free(ptr, len)` | Free a `chw_alloc(len)` block. `len` must match. Freeing twice or freeing a pointer `chw_alloc` did not return is undefined behavior, as with C `free`. `len = 0` is a no-op. |

### Results

| Export | Meaning |
| --- | --- |
| `chw_result_json_ptr(result) -> *const u8` | The result's UTF-8 JSON descriptor. Null for a bad handle. |
| `chw_result_json_len(result) -> usize` | Descriptor byte length. `0` for a bad handle. |
| `chw_result_free(result) -> u32` | Free the result and every buffer it references. `1`, or `0` for a bad handle. |

### Decode

| Export | Descriptor |
| --- | --- |
| `chw_decode(ptr, len) -> result` | A complete HTTP `FORMAT Native` body -> `{ rowCount, columnNames, columnTypes, chunks: [{ rowCount, columns }] }`. An empty body is an empty result with no schema. |
| `chw_decode_binary_value(ptr, len) -> result` | One SharedVariant or JSON shared-data cell -> `{ type, column }` with a one-row column. |
| `chw_describe_type(ptr, len) -> result` | A UTF-8 ClickHouse type string -> `{ type, kind, nullable, ...metadata, ...children }`. Children are nested descriptions under `element`, `key`, `value`, `dictionary`, `fields` or `variants`. JSON `typedPaths` entries are descriptions with an added `path`. |

The input is a bare HTTP Native body at protocol revision 0, with no TCP
framing and no compression frames. This build of core leaves out the C
`zstd` feature.

### Streaming decode

| Export | Meaning |
| --- | --- |
| `chw_stream_new() -> stream` | A decoder for one HTTP response body. |
| `chw_stream_push(stream, ptr, len) -> result` | Append bytes. The result describes every block now complete, in the decode shape plus `bufferedBytes`. |
| `chw_stream_finish(stream) -> result` | Drain the tail. Fails with GenericFailure on a truncated final block. Callable once. |
| `chw_stream_row_count(stream) -> u64` | Rows emitted so far. A `u64`, an `i64` in wasm, a BigInt in JS, because a long export can pass `u32::MAX`. |
| `chw_stream_buffered_bytes(stream) -> usize` | Bytes held for an incomplete block. |
| `chw_stream_free(stream) -> u32` | `1`, or `0` for a bad handle. |

Zero-row blocks contribute the schema but are left out of `chunks`. A failed
decode step poisons the stream, and every later push or finish fails with
InvalidArg. Lifecycle misuse, such as a push after finish, fails without
poisoning. The two lookups return `0` for a bad handle and record
InvalidArg. On success they clear the last error, so `chw_last_error_code`
tells a real `0` from a failure.

### Encode

| Export | Meaning |
| --- | --- |
| `chw_encode(desc_ptr, desc_len, data_ptr, data_len) -> result` | Encode one Native block for `INSERT ... FORMAT Native`. The result descriptor is `{ bytes }`. |

`desc` is a UTF-8 JSON block `{ rowCount, columns: [...] }` in the shape of
one decoded chunk, so a decoded chunk can be passed straight back. Its buffers
are references into `data` (see below). Each column needs `name` and `type`.
The `type` string is authoritative and `kind` is ignored. The core encoder
validates every structural invariant before writing a byte. A block too large
to allocate, for example billions of zero-width `Nothing` rows, fails with
InvalidArg.

## Descriptors

Descriptors are UTF-8 JSON. They carry per-column metadata only, never
per-row values, so parsing them costs little next to the buffers.

### Buffer references

- **In results**: `{"$b": [ptr, byteLength, dtype]}`, an absolute address in
  linear memory owned by the result. `dtype` is one of `i8 u8 i16 u16 i32 u32
  i64 u64 f32 f64` and names the element type. Buffers are little-endian.
- **In encode input**: `{"$b": [offset, byteLength]}`, a range of the `data`
  argument. The element type follows from the column's `type`, and
  `byteLength` must be a multiple of its width.

No other object in a descriptor has a `$b` key.

### Column objects

Every column has `kind` (the physical layout), `type` (the ClickHouse type
string) where known, and `name` at the top level of a chunk. A nullable column
also has `validity`, an Arrow-style bitmap with bit 1 = valid.

| `kind` | Fields |
| --- | --- |
| `Int8` ... `UInt64`, `Float32`, `Float64` | `values` |
| `Date`, `Date32`, `DateTime`, `DateTime64`, `Time`, `Time64`, `Interval` | `values`; `timezone` and `precision` where the type has them |
| `Enum8`, `Enum16` | `values`, `enumValues: [[name, value], ...]` |
| `IPv4` | `values` (u32) |
| `BFloat16` | `values` (u8, two bytes per row) |
| `Bool` | `bitmap`, `length` |
| `Nothing` | `length` |
| `String` | `offsets` (i32), `data` (u8) |
| `FixedString`, `UUID`, `IPv6`, `Int128`, `UInt128`, `Int256`, `UInt256` | `data`, `width` |
| `Decimal` | `data`, `width`, `precision`, `scale` |
| `AggregateFunction` | `offsets` (i64), `data` |
| `LowCardinality` | `indices` (i32), `dictionary` (column) |
| `Array` | `offsets` (i64), `values` (column) |
| `Tuple` | `length`, `names` (string or null each), `fields` (columns) |
| `Map` | `offsets` (i64), `keys`, `values` (columns) |
| `QBit` | `dimension`, `values` (column) |
| `Variant` | `discriminators` (u8, 255 = NULL), `offsets` (i32, row index within its alternative), `variants` (columns) |
| `Dynamic` | `typeIds` (u32, 0xFFFFFFFF = NULL), `offsets` (i32), `children` (typed columns, or a `SharedVariant` String-shaped column of binary cells) |
| `JSON` | `format: "text"` with `text` (String column), or `format: "structured"` with `length`, `typedPaths`, `dynamicPaths` (`[{ path, column }]`), `sharedOffsets` (i64), `sharedPaths`, `sharedValues` (String columns) |

Name-only types (Nullable, SimpleAggregateFunction, the geo types, Nested)
use the layout of the type they stand for. `type` keeps the original name.

## Out of scope

The ABI is columnar on purpose. Turning columns into rows, mapping values to
host types (BigInt, Date, Decimal strings), and client methods such as
`insertNative` or `queryNativeRows` belong in the client, not here.
