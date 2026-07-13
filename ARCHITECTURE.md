# ch-core-rs: How the Core Works

## The Big Picture

Every ClickHouse language client today reimplements the same hard, error-prone
work: parsing ClickHouse type strings, decoding the Native wire format,
handling block framing, null maps, and the long tail of types, and redoing all
of it on every server release. `ch-core-rs` exists so that work is done once,
correctly, in one place.

The idea in one sentence: **turn ClickHouse `FORMAT Native` wire bytes into
typed, Arrow-shaped columnar buffers and back again, as fast and as safely as
possible, with zero dependencies, and let each language binding decide how to
expose those buffers to its own runtime.**

```
ClickHouse server
      |
      |  FORMAT Native bytes (compact columnar wire format)
      v
+---------------------------+
|        ch-core-rs         |   pure Rust, zero dependencies
|                           |
|  parse type strings       |
|  decode blocks -> columns |   one correct implementation,
|  typed columnar buffers   |   shared by every client
+---------------------------+
      |
      |  typed buffers: Vec<i64>, offsets + data, packed bitmaps
      v
+----------+  +----------+  +-----------------+
| Python   |  | Node     |  | anything else   |   thin bindings, one per
| (PyO3)   |  | (napi-rs)|  | (native wrapper)|   runtime, each repo owns
+----------+  +----------+  +-----------------+   its own materialization
      |             |               |
   NumPy /       TypedArray /    Arrow C Data
   Arrow capsule  ArrayBuffer    stream
```

The diagram is the decode (read) path. The same buffers run the inverse
direction too: `native/encode/mod.rs` encodes them back into Native block bytes for
`INSERT ... FORMAT Native`.

The division of labor is strict:

- **The core owns** binary decoding and encoding, the ClickHouse type model, and
  the shared columnar buffer layout. This is the hot path, and it is written once.
- **The bindings own** everything runtime-specific: whether a `UInt64` becomes
  a Python `int` or a JS `BigInt`, how nulls surface, stream backpressure, and
  the public client API. Bindings are thin adapters over buffers the core has
  already filled.

Because the output buffers are deliberately Arrow-shaped (contiguous typed
arrays, offsets-plus-data strings, bit-packed validity), the core can also hand
the data across an FFI boundary with **zero copies** through the standard Arrow
C Data and C Stream interfaces in runtimes that can import those interfaces
in-process.

Why this is worth a dedicated crate rather than each client doing its own
thing:

1. **One correct protocol and type implementation, not N.** Wire-format bugs
   produce silently corrupt columns. Auditing one decoder beats auditing five.
2. **ClickHouse type fidelity.** The core preserves ClickHouse semantics in its
   type model, such as `DateTime64` precision and timezone, `FixedString` width,
   and exact signed/unsigned integer width. Presentation policy stays in the
   bindings.
3. **Non-Arrow zero-copy delivery.** For consumers that want native containers
   (JS `TypedArray`s, NumPy arrays) rather than Arrow, the core's typed
   buffers map directly, with no row-by-row object churn.
4. **Streaming.** Bytes can be fed in as they arrive off the socket and
   decoded blocks come out as soon as they complete, instead of buffering a
   whole response first.

End-to-end throughput is mostly a property of the binding and transport, not
this core, so benchmark numbers belong with the bindings. A few design
characteristics are worth calling out because they shape what a binding can
expect:

- Streaming with overlapped decode is where end-to-end wins come from: the core
  emits decoded blocks as they complete, while a client that buffers the whole
  response (`wait_end_of_query=1`) waits for the last byte first. In a
  decode-isolated comparison the server's `ArrowStream` still beats
  Native -> Arrow, since the server already emits Arrow memory layout; the
  advantage is pipeline overlap, not a faster transform.
- Under heavy server-side compression the Arrow advantage can invert, bounded by
  server compression throughput rather than client code. Native's smaller wire
  size is an additional advantage over a real network that a localhost
  comparison cannot show.

---

## Details

### Design priorities and invariants

Priorities, in order: correctness against the real wire format, safety (no
undefined behavior, no panics on malformed input), speed, leanness.

Non-negotiable invariants:

- **Zero runtime dependencies.** `Cargo.toml` has no `[dependencies]`. No
  Arrow crate, no Python, no Node, nothing.
- **No binding code in the core.** No PyO3, no napi-rs, no language-specific
  materialization.
- **Columnar, not row-major.** Row materialization was measured to throw away
  the core's advantage; it stays a binding concern.
- **Blocks stay separate chunks.** Native blocks are never concatenated or
  repacked (more below).
- **Arrow-compatible buffer layout** for everything, so FFI export is
  zero-copy.

### Module map

```
src/schema.rs            ClickHouse logical type model (ChType, Field, Schema)
src/column.rs            physical columnar buffers (Column and its variants)
src/batch.rs             ColBatch (one block) and ChunkedBatch (one result)
src/bitmap.rs            bit-packed validity bitmaps, CH null map conversion
src/native/protocol.rs   shared wire-protocol constants
src/native/type_parser.rs  type-string parser + type-shape predicates
src/native/varint.rs     ByteReader slice cursor + LEB128 varints
src/native/decode/mod.rs block framing, header validation, per-type decode
src/native/encode/mod.rs block framing + per-type Native encode (insert path)
src/native/encode/validate.rs encode precondition validation
src/native/stream_decoder.rs  push-based incremental decoding
src/ffi/mod.rs           Arrow C Data Interface export (schema/array/stream)
```

### The data model

**Logical layer (`schema.rs`).** `ChType` models ClickHouse types faithfully,
preserving semantics that Arrow or host types would lose: `DateTime64` keeps
its precision and timezone, `FixedString` keeps its width, `Nullable(T)` is an
explicit wrapper. `Display` renders the canonical ClickHouse type name, which
round-trips through the parser; that string is the contract bindings use to
report column types. A `Schema` is an ordered list of named `Field`s.

**Physical layer (`column.rs`).** A `Column` is an enum over a small set of
Arrow-shaped buffer containers:

- `PrimitiveColumn<T>`: one contiguous `Vec<T>` plus optional validity. Used
  for all fixed-width numerics and the temporal types at their faithful native
  width (`Date` is `u16` days, `DateTime` is `u32` seconds, `DateTime64` is
  `i64` ticks; no widening or rescaling ever happens in the core).
- `Utf8Column`: Arrow string layout, `offsets: Vec<i32>` of length rows+1 plus
  one `data: Vec<u8>` buffer. Row `i` is `data[offsets[i]..offsets[i+1]]`.
- `FixedBinaryColumn`: one contiguous `width * num_rows` byte buffer.
- `BoolColumn`: bit-packed LSB-first bitmap, exactly Arrow's boolean layout.

**Nullability (`bitmap.rs`).** ClickHouse sends nulls as one byte per row
(0x01 = null) before the values. Arrow wants a bit-packed validity bitmap
(bit 1 = valid). `Bitmap::from_ch_null_map` converts in a branchless loop that
packs 8 wire bytes into one output byte per iteration, so it vectorizes and
avoids per-row divide/modulo/store. Nullable columns still carry a value slot
for every row (ClickHouse writes placeholder values for nulls), which is also
exactly what Arrow expects.

**Results (`batch.rs`).** One decoded non-empty Native block becomes one
`ColBatch` (schema + columns + row count). A whole result is a `ChunkedBatch`:
the schema plus `Vec<Arc<ColBatch>>`. Blocks are kept as separate chunks on
purpose:
merging would cost O(n) buffer copies and O(n^2)-ish re-packing of bit-aligned
bool/validity bitmaps, and chunks map one-to-one onto Arrow record batches
anyway. The chunks are `Arc`ed because the FFI layer hands out shared
ownership of them (see below). Zero-row blocks (the server commonly sends a
zero-row trailer) contribute schema validation but are dropped from the chunk
list.

### Decoding the Native format (`native/decode/mod.rs`)

**The cursor.** All decoding runs over `ByteReader`, a plain slice cursor
(`&[u8]` + position). The input is always already in memory, so there is no
generic `io::Read`: no virtual calls, one bounds check per read, and
`read_slice` can *borrow* a sub-slice of the input rather than copying through
a temporary. Every read that can run off the end reports
`io::ErrorKind::UnexpectedEof` and nothing else; that error kind is
load-bearing, it is how the streaming layer distinguishes "need more bytes"
from "corrupt data".

**Block framing.** A Native block is: optional `BlockInfo` preamble, varint
column count, varint row count, then per column: varint-prefixed name,
varint-prefixed type string, optional custom-serialization marker byte, then
the column data. Two pieces of framing are protocol-revision gated, and the
revision is negotiated out of band (TCP handshake), so the caller must pass it
in via `DecodeOptions.protocol_revision`:

- revision > 0: each block is preceded by a `BlockInfo` preamble, a
  field-tagged structure terminated by field number 0. The decoder parses it
  by field number (revision independent) and discards the values; unknown
  field numbers are rejected, matching the server.
- revision >= 54454 (`DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION`): every
  column header carries one marker byte. 0 means default serialization;
  anything else (sparse, etc.) is a layout this crate does not decode, so it
  is rejected explicitly rather than misread.

Use revision 0 for bare HTTP `FORMAT Native` responses and the effective
negotiated protocol revision for protocol-framed HTTP or native TCP payloads.
`DBMS_TCP_PROTOCOL_VERSION` is the revision this crate has validated against at
the pinned server tag, not a substitute for negotiation. Wire-format behavior is
confirmed against the actual server C++ source at a pinned tag (cited in the doc
comments), per the repo's "server behavior is authoritative" rule.

**Type strings.** `parse_ch_type` parses the server's type name string
(`Nullable(DateTime64(3, 'UTC'))`) into a `ChType`. Unsupported or malformed
type names produce a clean `DecodeError::UnsupportedType`, never a wrong
decode. This assumes the default string-encoded Native type-header mode;
binary-encoded type headers are not implemented. One fidelity caveat: the type
string the server emits is itself revision gated in one known case. Over
revision 0 (bare HTTP `FORMAT Native`) a `DateTime('tz')` column arrives as
plain `DateTime`; at the negotiated TCP revision it keeps its timezone. The data
bytes (UInt32 seconds) are identical either way, so decode is correct
regardless, but a client that needs timezone fidelity must negotiate a protocol
revision.

**The primitive hot path.** Fixed-width columns are pure little-endian value
runs on the wire. The `decode_primitive!` macro reads them by allocating the
destination `Vec<T>` and doing a single `copy_nonoverlapping` of the whole
run into it: no per-element loop, no temporary buffer, one allocation per
column. The unsafe block carries a full safety argument; the destination is
allocated *as* `Vec<T>` so alignment and deallocation layout are correct. A
`#[cfg(target_endian = "big")]` fallback byte-swaps per element so the crate
stays correct off little-endian targets. Temporal types ride this same path
since their payloads are plain integer runs (timezone and precision are type
metadata only and never appear in the data bytes).

**Strings.** Each value is a varint length plus raw bytes. The decoder builds
the Arrow offsets and the single data buffer in one pass: the value bytes are
borrowed from the input slice and appended with one `extend_from_slice`, so
the cost is one copy per string and zero per-row heap allocations.
ClickHouse `String` is arbitrary bytes, so the current Arrow `utf8` export is
layout-compatible but not UTF-8-validating. Bindings that expose Arrow should
decide whether to validate, fall back to bytes, or return a clear error for
invalid strings.

**Hostile input hardening.** The wire bytes are untrusted, and the rule is
that malformed input returns `Err`, never panics, and never drives a huge
allocation:

- Row and column counts are sanity-checked against the bytes actually
  remaining before any `Vec::with_capacity` (every header and every row costs
  at least one byte on the wire, so a count larger than the remaining input is
  impossible).
- Byte-length computations from untrusted counts use `checked_mul`.
- String offsets are checked against `i32` overflow (Arrow's 32-bit offset
  cap, ~2 GiB per chunk) *before* the payload is copied, and surface as
  `InvalidData`, a fatal error the streaming layer will not mistake for
  "need more bytes".
- Varints reject a 10th continuation byte; header strings reject invalid
  UTF-8.

**Cross-block schema enforcement.** Every block of one query result must share
one schema. `decode_all_bytes` (and the streaming decoder) take the schema
from the first block and reject any later block that disagrees with
`DecodeError::BlockSchemaMismatch`, so a corrupt or mixed payload cannot
silently produce misaligned chunks.

### Streaming (`native/stream_decoder.rs`)

`StreamDecoder` is the push-based wrapper for real transports: feed it byte
chunks as they arrive, get complete `ColBatch`es back as soon as they can be
decoded.

```
let mut d = StreamDecoder::new(options);
for chunk in socket_chunks {
    for block in d.feed(chunk)? { /* hand to binding */ }
}
let tail = d.finish()?;   // errors if a truncated block remains
```

The interesting machinery is how it avoids wasted work on partial data:

- **Completeness scan first.** Before running the allocating decode, it runs
  `block_end`, an allocation-free walk of the block framing that shares the
  exact header-reading code with the real decoder (so the two cannot drift)
  and skips past column data by computed length (fixed-width types) or by
  walking the varint length prefixes (strings). Only when the scan confirms a
  whole block is buffered does the real decode run, so a block arriving over
  many small feeds allocates its column buffers exactly once, when the last
  byte lands.
- **High-water mark.** If a scan found the next block incomplete after
  consuming all N buffered bytes, no block can complete until the buffer grows
  past N, so redundant re-scans are skipped.
- **Buffer compaction.** Consumed bytes are drained after each feed so the
  internal buffer does not grow without bound.
- The `UnexpectedEof`-vs-fatal-error distinction from the decode layer is what
  makes this work: EOF means wait for more bytes, anything else aborts the
  stream.

Transport-level backpressure stays a binding/client responsibility; the core
just decodes what it is given.

### Arrow C Data Interface export (`src/ffi/mod.rs`)

This is how the buffers cross a language boundary with zero copies and zero
shared dependencies. The Arrow C Data Interface is a tiny C-compatible
interchange standard:
three `repr(C)` structs (`ArrowSchema`, `ArrowArray`, `ArrowArrayStream`),
format strings, and release callbacks. The core implements the producer side
by hand, no Arrow library involved:

- `export_schema` renders each column's `ChType` to an Arrow format string
  (`l` for Int64, `u` for Utf8, `w:16` for FixedString(16), `tsm:UTC` for
  DateTime64(3, 'UTC'), and so on) and sets the NULLABLE flag for
  `Nullable(T)`.
- `export_batch_array` exposes each column's buffers (validity bitmap, values,
  offsets/data) as raw pointers in Arrow's prescribed buffer order. No bytes
  are copied; the pointers point straight into the decoded `Vec`s.
- `export_chunks_to_stream` wraps a whole `ChunkedBatch` as an
  `ArrowArrayStream`: `get_schema` plus a `get_next` that yields one record
  batch per chunk. This is why chunks-as-blocks maps so cleanly onto Arrow.

Ownership is the subtle part. Each exported array's private data holds an
`Arc<ColBatch>`, so the decoded buffers stay alive for exactly as long as any
consumer still holds them, regardless of what
the Rust side does next. The consumer eventually invokes the `release`
callback, which drops the `Arc` and frees the bookkeeping. Release callbacks
are idempotent and null-safe per the spec. Export is also panic-free by
construction: for example, wire-derived strings containing interior NUL bytes
(which `CString` cannot represent) are sanitized rather than unwrapped,
because a panic unwinding across an `extern "C"` boundary would be undefined
behavior.

Temporal export follows a strict zero-copy rule: a column maps to a real Arrow
temporal type only when the physical width matches exactly (`Date32` ->
`date32`, `DateTime64` with precision 0/3/6/9 -> `timestamp`), otherwise the
raw integer type is exposed and interpretation is left to the binding. Buffers
are never widened or rescaled at the FFI layer.

### Encoding the Native format (`native/encode/mod.rs`)

The inverse of the decode path: `encode_block` and `encode_chunked` turn a
`ColBatch` (or each chunk of a `ChunkedBatch`) back into Native block bytes the
server accepts for `INSERT ... FORMAT Native`. Framing is the exact inverse of
the decoder (optional `BlockInfo` preamble at revision > 0, column and row
counts, per-column name/type-string/marker, then the body), gated by
`EncodeOptions.protocol_revision` the same way decode is, so bytes written at a
revision decode back at that revision. It is confirmed against
`NativeWriter::write` at the pinned tag.

Two things differ from decode by nature:

- **Input is validated, not trusted blindly.** The `Column` buffers are public,
  so a binding can build an insert column by hand. Encode validates the whole
  batch before writing a byte (column and row counts, type-string round-trip,
  string offset and fixed-width invariants, `LowCardinality` index bounds,
  nullability) and returns `EncodeError` rather than emit corrupt bytes or
  panic. A rejected batch leaves no partial stream.
- **Encode is canonical where decode is lenient.** It writes `0`/`1` for `Bool`,
  the canonical type string, and a minimal `LowCardinality` index width, so
  `decode(encode(x))` reproduces `x` and re-encoding canonical server bytes is
  byte-for-byte, though byte-identity is not guaranteed for non-canonical input.

Encode coverage is a subset of decode coverage (the full scalar and
`LowCardinality` set today) and targets the HTTP `INSERT` path at
`protocol_revision = 0`; there is no TCP engine or compression framing yet. The
per-type encode contract is the "Encoding" section of `CODEC_CONTRACT.md`.

### Error handling

`DecodeError` is the single decode error type: `Io` (with `UnexpectedEof`
reserved for "need more bytes"), `UnsupportedType`,
`UnsupportedSerialization`, `InvalidBlockInfo`, and `BlockSchemaMismatch`.
Anything the decoder does not understand fails loudly and precisely; the worst
failure mode for a wire decoder is a silently corrupt column, and the design
consistently chooses an explicit error over a guess. Encode has its own
`EncodeError` with two variants: `UnsupportedType` (a column type encode cannot
yet write) and `InconsistentBatch` (a structurally invalid batch, caught during
pre-write validation), so a bad batch fails cleanly instead of emitting partial
or corrupt bytes.

### Testing strategy

Two complementary layers:

- **Synthesized unit tests**: a `BlockBuilder` helper assembles wire bytes in
  process, covering each type plain, `Nullable`, zero-row, and multi-block.
- **Captured fixtures** (`tests/integration.rs`): committed `.native` bytes
  captured from a live ClickHouse server at the pinned version in
  `.server-ref`. This is the ground truth a synthesized test cannot give: a
  shared wrong assumption about the format would let an encode/decode
  round-trip pass while a real server fixture fails.

`CODEC_CONTRACT.md` is the definitive per-type reference (wire payload,
decoded buffers, Arrow export) and covers the encode direction too (input
preconditions, encoder choices, round-trip guarantees). It is kept in lockstep
with the code.

### Current scope and direction

Supported today: `Bool`, `Int8..64`, `UInt8..64`, `Float32/64`, `String`,
`FixedString(N)`, `Date`, `Date32`, `DateTime`, `DateTime64`, `Decimal(P, S)`,
`UUID`, `IPv4`, `IPv6`, `Enum8`/`Enum16`, and `LowCardinality(T)` for the
allowed inner types, plus `Nullable(T)` over any of them; whole-buffer and
streaming decode; Arrow C Data export; and encode back to Native block bytes for
that same scalar and `LowCardinality` set (the HTTP `INSERT` path). Not yet:
compression framing, the native TCP protocol, `Array`/`Tuple`/`Map`, and wide
ints. The growth model is fixed: implement a type once here, following the
documented confirm-against-server-source workflow in `AGENTS.md`, and every
binding gets it for free. The roadmap lives in `README.md`.
