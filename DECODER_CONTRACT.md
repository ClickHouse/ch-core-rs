# Decoder Contract: Supported Types and Decoded Output

This is the definitive reference for what `ch-core-rs` decodes and the exact
shape of the decoded output. For every supported ClickHouse type it records two
views:

1. The Arrow C Data export produced by `src/ffi.rs`: the format string, the
   buffer count, and the buffer order. This is the primary contract surface.
2. The raw Rust `Column` buffers produced by `src/native/decode.rs` and defined
   in `src/column.rs`, for readers that consume the decoded buffers directly
   rather than through the Arrow C Data Interface.

This document describes the core's output only. It says nothing about how any
particular consumer should map that output to a host language. Host value policy
is out of scope by design.

## Source of truth and how to keep this current

The code is authoritative. If this file and the source disagree, the source
wins and this file is stale. When a new type is added to the decoder, update the
support matrix and add a type section here in the same change. See the "Adding A
New ClickHouse Type" workflow in `AGENTS.md`.

Wire-layout claims below were confirmed against the ClickHouse server source at
tag `v26.5.1.882-stable` (the tag pinned in `.server-ref`, protocol revision
54484) via the `clickhouse-server-reader` sub-agent, and the per-type payloads
are verified by the crate's round-trip decode tests. Each type section cites the
server serialization class and method it was confirmed against. When you change
the pinned tag, reconfirm the layouts and update the citations, as `AGENTS.md`
describes.

## How to read a type section

Every type section uses the same fields, in the same order:

- **Type string(s)**: the exact ClickHouse type name(s) that `parse_ch_type`
  in `src/native/decode.rs` accepts for this type.
- **Logical type**: the `ChType` variant in `src/schema.rs`.
- **Wire payload**: the bytes the decoder reads for this column, for a block of
  `num_rows` rows. This is the per-column payload only. Block framing and the
  nullable null map are described once below, not repeated per type.
- **Arrow export**: the Arrow format string and the buffers emitted by
  `export_column_array` in `src/ffi.rs`, in order.
- **Rust buffer**: the `Column` variant and the fields of its backing struct in
  `src/column.rs`.
- **Notes**: edge cases, range limits, and anything a consumer can get wrong.
- **Server reference**: the server serialization class and method that defines
  the layout, confirmed at `v26.5.1.882-stable`.

---

## The columnar model

Decoding produces a `ChunkedBatch` (`src/batch.rs`):

```text
ChunkedBatch
  schema: Schema                 // Vec<Field>, each Field is { name, ch_type }
  chunks: Vec<Arc<ColBatch>>     // one per non-empty Native block
    ColBatch
      schema: Schema
      columns: Vec<Column>       // one Column per field, same order as schema
      num_rows: usize            // every column in the chunk has this length
```

Key invariants a consumer can rely on:

- **Blocks stay separate.** Each Native block becomes its own `ColBatch` chunk.
  Chunks are never concatenated or repacked. The chunk stream maps directly onto
  Arrow record batches, one batch per chunk.
- **Schema is shared and stable.** All chunks of one result share the schema.
  The schema is taken from the first decoded block.
- **Zero-row blocks contribute only the schema.** A block with `num_rows == 0`
  is dropped from `chunks` but still establishes the schema. A result that is
  entirely empty therefore has a valid schema and no chunks. See "Zero-row
  output" below for the exact empty-buffer shapes.

### Validity and nulls

A nullable column carries a validity bitmap (`src/bitmap.rs`). The convention is
the Arrow convention: bit set to 1 means valid, bit set to 0 means null. Bits
are packed LSB-first within each byte, so row `i` is `byte[i / 8]` bit
`i % 8`. This is converted at decode time from ClickHouse's null map, which uses
one byte per row with 0x00 for present and a nonzero byte for null.

A non-nullable column has no validity bitmap. In the Arrow export its validity
buffer pointer is null, which Arrow reads as "all values valid".

`null_count` is always reported correctly, including 0 for non-nullable columns.

### Offsets for variable-length data

Variable-length columns (currently `String`) use Arrow's offset layout with
32-bit signed offsets:

- `offsets` has length `num_rows + 1` with `offsets[0] == 0`.
- Row `i` occupies `data[offsets[i] .. offsets[i + 1]]`.
- Offsets are monotonically non-decreasing.

Because offsets are `i32`, a single chunk's string data buffer is limited to
about 2 GiB. Blocks stay separate, so this is a per-chunk limit, not a
per-result limit.

### Endianness

ClickHouse Native is little-endian on the wire. Decoded fixed-width buffers are
native-endian typed buffers (`Vec<T>`). On little-endian targets the wire bytes
are read straight into the destination buffer with no per-element work. On
big-endian targets each element is byte-swapped during decode. Either way the
resulting `Vec<T>` holds correct host-native values.

### Arrow C Data export shape

`export_chunks_to_stream` emits one Arrow `ArrowArrayStream`. Each chunk is one
record batch, exported as a struct array:

- The top-level struct array has format `+s`, `n_buffers == 1` with a single
  null buffer pointer (the struct validity, all valid), and one child array per
  column.
- Each column child array has the per-type format string and buffers described
  in its type section. For every column the validity buffer is buffer index 0,
  null when the column is non-nullable.
- The schema is available before any batch via `get_schema` and stays valid even
  when there are zero chunks.

Exported buffers are borrowed from the owning `ColBatch`. The export keeps the
`Arc<ColBatch>` alive in private data, so the buffers stay valid until the
consumer calls the Arrow `release` callback. Do not read exported buffers after
release.

---

## Native block framing

Native block framing is gated on the server protocol revision, and the revision
is negotiated out of band, not carried in the Native bytes. The decoder must be
told it via `DecodeOptions.protocol_revision`. Use `DBMS_TCP_PROTOCOL_VERSION`
(54484, the revision this crate is validated against) for a stream from a current
server over native TCP, or 0 for a bare Native stream with no protocol framing,
for example HTTP `FORMAT Native` with no `client_protocol_version` set.

This section describes the server layout at `v26.5.1.882-stable`
(`NativeWriter::write` / `NativeReader::read` in `src/Formats/`, with `BlockInfo`
in `src/Core/BlockInfo.{h,cpp}`), then how the decoder reads it. "varint"
throughout means LEB128 unsigned (`src/native/varint.rs`).

Server layout of one block, in order:

1. Block info preamble. Present whenever the producer's protocol revision is
   nonzero, which is every native TCP connection. `BlockInfo` is a field-tagged,
   zero-terminated structure, not a fixed-size header. For each field it writes a
   varint field number then the field value; a varint field number of 0
   terminates. The fields, and the protocol revision at which each first appears:
   - field 1, `is_overflows`: 1 byte. Always present.
   - field 2, `bucket_num`: Int32 little-endian. Always present.
   - field 3, `out_of_order_buckets`: a varint count then that many Int32 values.
     Present at protocol revision >= 54480.
   At v26.5.1 the revision is 54484, so all three fields are written. The
   standard block (not overflows, `bucket_num` -1, empty `out_of_order_buckets`)
   serializes to 10 bytes: `01 00 02 FF FF FF FF 03 00 00`. At revisions below
   54480 the same preamble was 8 bytes.
2. `num_columns` as a varint.
3. `num_rows` as a varint.
4. For each column, in order:
   - column name, a varint-length-prefixed string,
   - type name, a varint-length-prefixed string,
   - a custom-serialization marker, 1 byte: 0 for default, nonzero for custom.
     Present at protocol revision >= 54454, for every column regardless of row
     count. When nonzero, serialization-kind bytes follow before the payload. At
     v26.5.1 this byte is always present.
   - the column payload, as described in that type's section.

The per-column payload is preceded by no other framing: the server's
`deserializeBinaryBulkStatePrefix` step reads zero bytes for every type this
crate supports, including `String` (always the single-stream variant on the
Native wire at this tag). For a `Nullable(T)` column the payload is the null map
first, then the inner type's payload. The null map is `num_rows` bytes, one per
row, 0x00 for present and nonzero for null. See the `Nullable(T)` section.

### How the decoder reads this

`decode_next_block` in `src/native/decode.rs`, driven by
`DecodeOptions.protocol_revision`:

- When `protocol_revision > 0`, it parses the block info preamble by field
  number until the 0 terminator (`read_block_info`), reading the known fields and
  discarding their values. This is revision independent: the 8-byte two-field
  preamble and the 10-byte three-field preamble both decode. An unknown field
  number is rejected with `DecodeError::InvalidBlockInfo`, matching the server,
  which throws.
- When `protocol_revision >= DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION`
  (54454), it reads the per-column custom-serialization marker byte for every
  column, including in zero-row blocks. A 0 means default serialization and
  decoding proceeds. A nonzero value selects a custom serialization (sparse,
  detached, and so on) whose layout this crate does not decode, so it is rejected
  with `DecodeError::UnsupportedSerialization` rather than misreading the column.

The first byte of the block info preamble is also where a clean end-of-stream
boundary falls, so a stream that ends there decodes as "no more blocks" rather
than an error.

---

## Supported types matrix

| ClickHouse type   | ChType           | Column variant    | Arrow format | Arrow buffers (in order)    | Nullable |
|-------------------|------------------|-------------------|--------------|-----------------------------|----------|
| `Bool`, `Boolean` | `Bool`           | `Bool`            | `b`          | validity, data bits         | yes      |
| `Int8`            | `Int8`           | `Int8`            | `c`          | validity, values            | yes      |
| `Int16`           | `Int16`          | `Int16`           | `s`          | validity, values            | yes      |
| `Int32`           | `Int32`          | `Int32`           | `i`          | validity, values            | yes      |
| `Int64`           | `Int64`          | `Int64`           | `l`          | validity, values            | yes      |
| `UInt8`           | `UInt8`          | `UInt8`           | `C`          | validity, values            | yes      |
| `UInt16`          | `UInt16`         | `UInt16`          | `S`          | validity, values            | yes      |
| `UInt32`          | `UInt32`         | `UInt32`          | `I`          | validity, values            | yes      |
| `UInt64`          | `UInt64`         | `UInt64`          | `L`          | validity, values            | yes      |
| `Float32`         | `Float32`        | `Float32`         | `f`          | validity, values            | yes      |
| `Float64`         | `Float64`        | `Float64`         | `g`          | validity, values            | yes      |
| `String`          | `String`         | `Utf8`            | `u`          | validity, offsets, data     | yes      |
| `FixedString(N)`  | `FixedString(N)` | `FixedBinary`     | `w:N`        | validity, data              | yes      |
| `Nullable(T)`     | `Nullable(T)`    | inner T's variant | inner's      | inner's, validity populated | n/a      |

Any type not in this matrix is rejected. See "Unsupported types" below.

---

## Type sections

### Fixed-width numerics

This covers `Int8`, `Int16`, `Int32`, `Int64`, `UInt8`, `UInt16`, `UInt32`,
`UInt64`, `Float32`, and `Float64`. They share one layout and differ only in
element width, signedness, and Arrow format character.

**Type string(s) and per-type details:**

| Type string | Logical type      | Rust element | Column variant | Arrow format | Bytes/row |
|-------------|-------------------|--------------|----------------|--------------|-----------|
| `Int8`      | `ChType::Int8`    | `i8`         | `Int8`         | `c`          | 1         |
| `Int16`     | `ChType::Int16`   | `i16`        | `Int16`        | `s`          | 2         |
| `Int32`     | `ChType::Int32`   | `i32`        | `Int32`        | `i`          | 4         |
| `Int64`     | `ChType::Int64`   | `i64`        | `Int64`        | `l`          | 8         |
| `UInt8`     | `ChType::UInt8`   | `u8`         | `UInt8`        | `C`          | 1         |
| `UInt16`    | `ChType::UInt16`  | `u16`        | `UInt16`       | `S`          | 2         |
| `UInt32`    | `ChType::UInt32`  | `u32`        | `UInt32`       | `I`          | 4         |
| `UInt64`    | `ChType::UInt64`  | `u64`        | `UInt64`       | `L`          | 8         |
| `Float32`   | `ChType::Float32` | `f32`        | `Float32`      | `f`          | 4         |
| `Float64`   | `ChType::Float64` | `f64`        | `Float64`      | `g`          | 8         |

**Wire payload:** `num_rows * bytes_per_row` bytes, little-endian, contiguous,
with no per-row framing. Floats are IEEE 754 in little-endian byte order.

**Arrow export:** 2 buffers in order: validity, then values. The values buffer
points at the contiguous typed value buffer. Format is the per-type character
above.

**Rust buffer:** `Column::<Variant>(PrimitiveColumn<T>)` where
`PrimitiveColumn<T>` is `{ values: Vec<T>, validity: Option<Bitmap> }`. `values`
has length `num_rows`. On little-endian targets `values` is the wire bytes
verbatim.

**Notes:** `UInt64` and `Int64` carry the full 64-bit range. The core does not
narrow or reinterpret them. Float NaN and infinity pass through unchanged as
their wire bit patterns.

**Server reference:** `SerializationNumber<T>::deserializeBinaryBulk` in
`src/DataTypes/Serializations/SerializationNumber.cpp`. On little-endian hosts it
is a single bulk raw read into the column buffer; big-endian hosts byte-swap per
element. Confirmed at `v26.5.1.882-stable`.

### Bool

**Type string(s):** `Bool`, `Boolean`.

**Logical type:** `ChType::Bool`.

**Wire payload:** `num_rows` bytes, one per row. The decoder treats a zero byte
as false and any nonzero byte as true.

**Arrow export:** format `b`. 2 buffers in order: validity, then the data bit
buffer. The data buffer is a packed bitmap, one bit per row, LSB-first within
each byte. Note this differs from the wire, which uses one byte per row. The
decoder repacks the wire bytes into a bitmap so the layout is Arrow boolean.

**Rust buffer:** `Column::Bool(BoolColumn)` where `BoolColumn` is
`{ bitmap: Vec<u8>, len: usize, validity: Option<Bitmap> }`. `bitmap` is the
packed data bits, `len` is the row count.

**Notes:** the data bit buffer is distinct from the validity buffer. A nullable
`Bool` has both: a validity bitmap saying which rows are present, and a data
bitmap giving each present row's true or false value. Both use the same
LSB-first packing.

**Server reference:** `SerializationBool` in
`src/DataTypes/Serializations/SerializationBool.cpp`. It is a `SerializationWrapper`
around `SerializationNumber<UInt8>` and does not override the binary bulk path, so
the wire form is exactly `UInt8`: one byte per row. The bulk read does no
clamping to 0/1, so the decoder's "any nonzero byte is true" reading is safe; the
server writes 0 or 1 in practice. Confirmed at `v26.5.1.882-stable`.

### String

**Type string(s):** `String`.

**Logical type:** `ChType::String`.

**Wire payload:** for each row, a varint length prefix followed by exactly that
many raw bytes. Lengths and bytes are read in row order.

**Arrow export:** format `u` (Arrow utf8, 32-bit offsets). 3 buffers in order:
validity, offsets, data. Offsets are `i32` with `num_rows + 1` entries starting
at 0. Data is the concatenated bytes.

**Rust buffer:** `Column::Utf8(Utf8Column)` where `Utf8Column` is
`{ offsets: Vec<i32>, data: Vec<u8>, validity: Option<Bitmap> }`.

**Notes:**

- ClickHouse `String` is arbitrary bytes, not guaranteed UTF-8. The decoder does
  not validate or transcode the bytes. They are exported under the Arrow utf8
  format `u` as-is. A consumer that requires valid UTF-8 must validate the bytes
  itself. The bytes can contain embedded NULs and invalid UTF-8.
- Empty strings are represented by equal adjacent offsets, not by null. Null and
  empty are distinct.
- The 32-bit offsets cap a single chunk's data buffer at about 2 GiB. See
  "Offsets for variable-length data" above.

**Server reference:** `SerializationString::deserializeBinaryBulk` in
`src/DataTypes/Serializations/SerializationString.cpp`: per row a VarUInt length
then that many raw bytes, no UTF-8 validation. Confirmed at `v26.5.1.882-stable`.

### FixedString(N)

**Type string(s):** `FixedString(N)` where `N` is the byte width, for example
`FixedString(16)`.

**Logical type:** `ChType::FixedString(N)`.

**Wire payload:** `num_rows * N` bytes, contiguous, no length prefixes. Each row
is exactly `N` bytes. Values shorter than `N` are right-padded with zero bytes
by the server, and that padding is part of the bytes the decoder returns.

**Arrow export:** format `w:N` (Arrow fixed-size binary of width `N`). 2 buffers
in order: validity, then data. There is no offsets buffer; row `i` is
`data[i * N .. (i + 1) * N]`.

**Rust buffer:** `Column::FixedBinary(FixedBinaryColumn)` where
`FixedBinaryColumn` is `{ data: Vec<u8>, width: usize, validity: Option<Bitmap> }`.

**Notes:** the bytes are raw and include any trailing zero padding. The decoder
does not strip padding or interpret the bytes as text.

**Server reference:** `SerializationFixedString::deserializeBinaryBulk` in
`src/DataTypes/Serializations/SerializationFixedString.cpp`: exactly `N * num_rows`
contiguous bytes, no length prefixes. Short values are zero-padded to `N` at
insert time, so the wire bytes are always `N` per row. Confirmed at
`v26.5.1.882-stable`.

### Nullable(T)

**Type string(s):** `Nullable(T)` where `T` is any supported non-wrapper type
above, for example `Nullable(Int64)` or `Nullable(String)`.

**Logical type:** `ChType::Nullable(Box<ChType>)`.

**Wire payload:** the null map first, then the inner type `T`'s full payload.
The null map is `num_rows` bytes, one per row, 0x00 for present and nonzero for
null. The inner payload then carries all `num_rows` values, including
placeholder values for the null rows. Null rows still occupy space in the inner
column; their stored value is unspecified and must not be read as meaningful.

**Arrow export:** identical to the inner type `T`'s export, except the validity
buffer (buffer index 0) is populated from the null map instead of being null,
and the child schema sets the Arrow nullable flag.

**Rust buffer:** the same `Column` variant as `T`, with its `validity` field set
to `Some(Bitmap)`. There is no separate `Nullable` column wrapper. Nullability
is carried by the inner column's `validity`.

**Notes:**

- Only one level of `Nullable` is meaningful, matching ClickHouse. The inner
  type must be a concrete supported type, not another wrapper.
- For a null row the inner buffer still holds a value. Always consult validity
  before reading a value. The placeholder is whatever the server wrote, commonly
  zero, but do not rely on that.

**Server reference:** `SerializationNullable::deserializeBinaryBulkWithMultipleStreams`
in `src/DataTypes/Serializations/SerializationNullable.cpp`: the null map stream
(one `UInt8` per row, 0 present, 1 NULL) is read first, then the nested column
with all `num_rows` values. The decode verifies the null map and nested column
have equal length. Confirmed at `v26.5.1.882-stable`.

---

## Zero-row output

When a block has `num_rows == 0` the chunk is dropped from `chunks`, but the
schema is still established. If a consumer constructs or inspects an empty column
directly (`empty_column` in `src/native/decode.rs`), the empty shapes are:

- Numerics and `Bool`: empty value or bit buffer, length 0.
- `String`: `offsets == [0]` (length 1, the required leading zero) and empty
  data.
- `FixedString(N)`: empty data, width preserved.
- `Nullable(T)`: as above with an empty validity bitmap.

In all cases length is 0 and `null_count` is 0.

---

## Unsupported types

`parse_ch_type` returns no match for any type not listed in the support matrix.
Decoding such a column fails with `DecodeError::UnsupportedType { column,
type_name }` rather than producing a wrong or partial column. There is no silent
fallback. A consumer can treat an unsupported type as a hard decode error.

Not yet supported, tracked as planned phases in `src/schema.rs`:

- Temporal: `Date`, `Date32`, `DateTime`, `DateTime64`.
- `Decimal`.
- `UUID`, `IPv4`, `IPv6`.
- `Enum8`, `Enum16`.
- `LowCardinality(T)`.
- Containers: `Array(T)`, `Tuple(...)`, `Map(K, V)`.
- Wide integers: `Int128`, `UInt128`, `Int256`, `UInt256`.

When one of these is implemented, move it into the support matrix and add a type
section here.
