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
tag `v26.6.1.1193-stable` (the tag pinned in `.server-ref`, protocol revision
54485) via the `clickhouse-server-reader` sub-agent, and the per-type payloads
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
  the layout, confirmed at `v26.6.1.1193-stable`.

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
told it via `DecodeOptions.protocol_revision`. Use 0 for a bare Native stream
with no protocol framing, for example HTTP `FORMAT Native` with no
`client_protocol_version` set. Use the effective negotiated revision for native
TCP or protocol-framed HTTP payloads. `DBMS_TCP_PROTOCOL_VERSION` is 54485, the
revision this crate is validated against at the pinned server tag.

This section describes the server layout at `v26.6.1.1193-stable`
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
   - field 2, `bucket_num`: Int32 written with the server's native POD binary
     helper. On the usual little-endian server builds this is little-endian.
     Always present.
   - field 3, `out_of_order_buckets`: a varint count then that many Int32 values,
     written with the same native POD helper.
     Present at protocol revision >= 54480.
   At v26.6.1 the revision is 54485, so all three fields are written. The
   standard block (not overflows, `bucket_num` -1, empty `out_of_order_buckets`)
   serializes to 10 bytes: `01 00 02 FF FF FF FF 03 00 00`. At revisions below
   54480 the same preamble was 8 bytes.
2. `num_columns` as a varint.
3. `num_rows` as a varint.
4. For each column, in order:
   - column name, a varint-length-prefixed string,
   - type name, a varint-length-prefixed string in the default Native type-header
     mode,
   - a custom-serialization marker, 1 byte: 0 for default, nonzero for custom.
     Present at protocol revision >= 54454, for every column regardless of row
     count. When nonzero, serialization-kind bytes follow before the payload. At
     v26.6.1 this byte is always present.
   - the column payload, as described in that type's section.

The per-column payload is preceded by a per-column bulk-state prefix, the
server's `deserializeBinaryBulkStatePrefix` step. For every type except
`LowCardinality` this prefix reads zero bytes, including `String` (always the
single-stream variant on the Native wire at this tag). `LowCardinality` reads a
real prefix, an 8-byte little-endian key version; see the `LowCardinality(T)`
section. The server runs this step once per column per block, immediately before
that column's payload, and only when the block has rows
(`NativeReader::readData`, gated by `if (rows)`), so a zero-row block reads no
prefix and no data for any column. For a `Nullable(T)` column the payload is the
null map first, then the inner type's payload. The null map is `num_rows` bytes,
one per row, 0x00 for present and nonzero for null. See the `Nullable(T)`
section.

This contract describes the default string-encoded type-header mode. ClickHouse
also has an `output_format_native_encode_types_in_binary_format` setting that
causes the server to write binary type tags instead of the varint-length-prefixed
type name string. The current core does not decode that header mode. Bindings
must not enable that setting when routing results through this decoder unless
binary type-header support is added.

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
| `UUID`            | `Uuid`           | `Uuid`            | `w:16`       | validity, data              | yes      |
| `IPv4`            | `Ipv4`           | `Ipv4`            | `I`          | validity, values            | yes      |
| `IPv6`            | `Ipv6`           | `Ipv6`            | `w:16`       | validity, data              | yes      |
| `Enum8(...)`      | `Enum8 { variants }`  | `Enum8`      | `c`          | validity, values            | yes      |
| `Enum16(...)`     | `Enum16 { variants }` | `Enum16`     | `s`          | validity, values            | yes      |
| `Decimal(P, S)`   | `Decimal { precision, scale, bits }` | `Decimal` | `d:P,S` (128-bit) or `d:P,S,bits` (32/64/256-bit) | validity, data | yes |
| `Date`            | `Date`           | `Date`            | `S`          | validity, values            | yes      |
| `Date32`          | `Date32`         | `Date32`          | `tdD`        | validity, values            | yes      |
| `DateTime`, `DateTime('<tz>')` | `DateTime { timezone }` | `DateTime` | `I` | validity, values         | yes      |
| `DateTime64(P)`, `DateTime64(P, '<tz>')` | `DateTime64 { precision, timezone }` | `DateTime64` | `ts{unit}:{tz}` for P in {0,3,6,9}, else `l` | validity, values | yes |
| `Nullable(T)`     | `Nullable(T)`    | inner T's variant | inner's      | inner's, validity populated | n/a      |
| `LowCardinality(T)` for an allowed inner `T` (see the type section) | `LowCardinality(Box<ChType>)` | `Dictionary` | `i` (index type; values type in the dictionary child) | validity, i32 indices (+ dictionary child) | via inner `Nullable` |

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
element. Confirmed at `v26.6.1.1193-stable`.

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
server writes 0 or 1 in practice. Confirmed at `v26.6.1.1193-stable`.

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
  itself. A strict Arrow consumer may reject the import or fail later validation
  when invalid UTF-8 is present. The bytes can contain embedded NULs and invalid
  UTF-8.
- Empty strings are represented by equal adjacent offsets, not by null. Null and
  empty are distinct.
- The 32-bit offsets cap a single chunk's data buffer at about 2 GiB. See
  "Offsets for variable-length data" above. A chunk whose running string data
  would exceed `i32::MAX` is rejected with `DecodeError::Io` of kind
  `InvalidData` rather than overflowing the offset to a negative value. Blocks
  stay separate chunks, so this is a per-chunk limit, not a per-result limit.

**Server reference:** `SerializationString::deserializeBinaryBulk` in
`src/DataTypes/Serializations/SerializationString.cpp`: per row a VarUInt length
then that many raw bytes, no UTF-8 validation. Confirmed at `v26.6.1.1193-stable`.

### FixedString(N)

**Type string(s):** `FixedString(N)` where `N` is the byte width, for example
`FixedString(16)`. `N` must be positive: `FixedString(0)` is not a valid
ClickHouse type and is rejected as `UnsupportedType`.

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
`v26.6.1.1193-stable`.

### UUID

**Type string(s):** `UUID`.

**Logical type:** `ChType::Uuid`.

**Wire payload:** `num_rows * 16` raw bytes, contiguous, no per-row framing. Each
row is a 16-byte POD dump of the server's `UInt128` (`items[0]` then `items[1]`,
each little-endian on the usual little-endian server builds). This is **not**
RFC-4122 byte order.

**Arrow export:** format `w:16` (Arrow fixed-size binary of width 16). 2 buffers
in order: validity, then data. There is no offsets buffer; row `i` is
`data[i * 16 .. (i + 1) * 16]`. The core emits plain `w:16`; it does **not** claim
the `arrow.uuid` extension type.

**Rust buffer:** `Column::Uuid(FixedBinaryColumn)` with `width == 16`, the same
backing struct as `FixedString`. The bytes are the wire bytes verbatim.

**Notes:**

- **Decode is raw passthrough.** The 16 wire bytes are stored unchanged; the
  decoder does no reordering and does no per-cell work. Any conversion to a host
  `uuid.UUID` (or to RFC-4122 byte order) is a binding concern, out of scope here.
- **Wire -> RFC byte mapping (for bindings).** The wire order is the reverse of
  RFC-4122 within each 8-byte half: `rfc[i] = wire[7 - i]` for `i` in `0..=7`, and
  `rfc[i] = wire[23 - i]` for `i` in `8..=15` (reverse the first 8 bytes, reverse
  the last 8). Concrete example: RFC UUID
  `00112233-4455-6677-8899-aabbccddeeff` serializes on the wire as the 16 bytes
  `77 66 55 44 33 22 11 00 ff ee dd cc bb aa 99 88`. The crate's round-trip and
  live-server tests assert exactly these bytes.

**Introduction version:** first-class since approximately v21.1 (inferred from
release notes; predates the pinned tag). Stable at `v26.6.1.1193-stable`.

**Server reference:** `SerializationUUID::serializeBinaryBulk` /
`deserializeBinaryBulk` in `src/DataTypes/Serializations/SerializationUUID.cpp`: a
POD dump of the `UInt128`, 16 contiguous bytes per row, no per-row framing, with
the half-reversed (non-RFC) byte order above. `deserializeBinaryBulkStatePrefix`
reads zero bytes and the custom-serialization marker is 0x00. Confirmed at
`v26.6.1.1193-stable`.

### IPv4

**Type string(s):** `IPv4`.

**Logical type:** `ChType::Ipv4`.

**Wire payload:** `num_rows * 4` bytes, little-endian, contiguous, no per-row
framing. Identical to `SerializationNumber<UInt32>` in bulk. Reading 4 bytes as a
little-endian `u32` yields the standard IPv4 numeric value
(`a<<24 | b<<16 | c<<8 | d`); for example `192.0.2.235` decodes to `3221226219`.

**Arrow export:** format `I` (Arrow uint32). 2 buffers in order: validity, then
values. Zero-copy, exactly like `DateTime` and `UInt32`.

**Rust buffer:** `Column::Ipv4(PrimitiveColumn<u32>)`, `{ values, validity }`,
length `num_rows`. On little-endian targets `values` is the wire bytes verbatim.

**Notes:** the stored integer is the canonical IPv4 numeric value, not a
dotted-quad string. Rendering it as `a.b.c.d` (or to a host address object) is a
binding concern.

**Introduction version:** first-class since approximately v21.1 (inferred from
release notes; predates the pinned tag). Stable at `v26.6.1.1193-stable`.

**Server reference:** `SerializationIP<IPv4>` in
`src/DataTypes/Serializations/SerializationIPv4andIPv6.cpp`, which serializes
identically to `SerializationNumber<UInt32>` in bulk: a single bulk raw read into
the column buffer on little-endian hosts, byte-swapped per element on big-endian
hosts. `deserializeBinaryBulkStatePrefix` reads zero bytes and the
custom-serialization marker is 0x00. Confirmed at `v26.6.1.1193-stable`.

### IPv6

**Type string(s):** `IPv6`.

**Logical type:** `ChType::Ipv6`.

**Wire payload:** `num_rows * 16` raw bytes, contiguous, no per-row framing. Each
row is the 16-byte `in6_addr` in network byte order (big-endian), exactly as a
standard IPv6 address is stored on the wire.

**Arrow export:** format `w:16` (Arrow fixed-size binary of width 16). 2 buffers
in order: validity, then data. Row `i` is `data[i * 16 .. (i + 1) * 16]`.

**Rust buffer:** `Column::Ipv6(FixedBinaryColumn)` with `width == 16`, the same
backing struct as `FixedString`. The bytes are the wire bytes verbatim.

**Notes:** the bytes are network-order `in6_addr` bytes, passed through unchanged.
Decode does no reordering and no per-cell work. Converting to a host IPv6 address
object (or to a textual form) is a binding concern.

**Introduction version:** first-class since approximately v21.1 (inferred from
release notes; predates the pinned tag). Stable at `v26.6.1.1193-stable`.

**Server reference:** `SerializationIP<IPv6>` in
`src/DataTypes/Serializations/SerializationIPv4andIPv6.cpp`: 16 contiguous bytes
per row in network byte order, no per-row framing.
`deserializeBinaryBulkStatePrefix` reads zero bytes and the custom-serialization
marker is 0x00. Confirmed at `v26.6.1.1193-stable`.

### Enum8 / Enum16

**Type string(s):** `Enum8('name' = N, ...)` and `Enum16('name' = N, ...)`. The
server always emits the concrete keyword with explicit values, never a bare
`Enum(...)` (that spelling is creation-time parser sugar only), so those are the
only two spellings `parse_ch_type` accepts.

**Logical type:** `ChType::Enum8 { variants: Vec<(String, i8)> }` and
`ChType::Enum16 { variants: Vec<(String, i16)> }`. The `variants` carry the
name->value mapping in the server's emitted order, which is ascending by value.

**Wire payload:** byte-identical to the underlying integer: `Enum8` is raw
little-endian `Int8` (1 byte/row), `Enum16` is raw little-endian `Int16` (2
bytes/row), contiguous, no per-row framing. The name->value mapping is ONLY in
the type string, never in the per-row data. The per-column bulk-state prefix
reads zero bytes and the custom-serialization marker is 0x00, same as a plain
numeric.

**Arrow export:** `c` (Arrow int8) for `Enum8`, `s` (Arrow int16) for `Enum16`.
2 buffers in order: validity, then values. Zero-copy, exactly like `Int8` /
`Int16`. Arrow has no native enum type, and ClickHouse enum values are arbitrary
signed integers (not `0..N-1` dictionary indices), so the export is the raw
underlying int rather than a dictionary array; a dictionary export would require
forbidden per-cell remapping. The name->value mapping is carried in the `ChType`
for bindings; surfacing it as Arrow field metadata is out of scope.

**Rust buffer:** `Column::Enum8(PrimitiveColumn<i8>)` and
`Column::Enum16(PrimitiveColumn<i16>)`, `{ values, validity }`, length
`num_rows`. On little-endian targets `values` is the wire bytes verbatim. The
Column carries only the physical int buffer; the name->value map stays in the
schema's `ChType`, the same Column-vs-ChType split the temporals use for
timezone and precision.

**Type-string format and name escaping:** the type string is `Enum8(` followed
by the `'<name>' = <int>` pairs joined by exactly `, `, with exactly ` = `
around each integer, then `)`. Pairs are sorted ascending by value in the
emitted string. Values are explicit and may be negative (`Enum8` is the Int8
range -128..=127, `Enum16` is the Int16 range -32768..=32767). Inside the single
quotes the server uses `writeQuotedString` with
`escape_quote_with_quote=false` and `escape_backslash_with_backslash=true`:
`'` -> `\'`, `\` -> `\\`, backspace -> `\b`, formfeed -> `\f`, newline -> `\n`,
CR -> `\r`, tab -> `\t`, NUL -> `\0`, and every other byte (including `,` and
`=`) passes through unescaped. So the parser cannot split on `,` or `=`; it
walks the quote-delimited names and unescapes that exact set. `Display`
round-trips the type string: `parse(display(t)) == t`. An out-of-range value, a
malformed escape, or any syntax error in this untrusted string surfaces as
`UnsupportedType`, never a panic.

**Notes:** `LowCardinality(Enum8/16)` is illegal: `DataTypeEnum` does not
inherit `DataTypeNumberBase`, so `canBeInsideLowCardinality()` is false and the
server throws `ILLEGAL_TYPE_OF_ARGUMENT` at construction. A
`LowCardinality(Enum...)` column can therefore never appear on the wire, and the
decoder keeps rejecting `Enum` as a `LowCardinality` inner.

**Introduction version:** first-class long before the pinned tag (inferred from
release history); stable at `v26.6.1.1193-stable`.

**Server reference:** `SerializationEnum` (inherits `SerializationNumber<Int8>`
/ `SerializationNumber<Int16>` and overrides no bulk method; the bulk methods are
`final`) and `DataTypeEnum` in `src/DataTypes/`. The type-string emission and
name escaping are `DataTypeEnum::doGetName` / `writeQuotedString`, and
`canBeInsideLowCardinality()` is false on `DataTypeEnum`. Confirmed at
`v26.6.1.1193-stable`.

### Decimal(P, S)

**Type string(s):** `Decimal(P, S)` where `P` is the precision (1..=76) and `S`
is the scale (0..=P). The server always emits this canonical, comma-space form
on the wire via `DataTypeDecimal::doGetName`, with both fields always present.
It never emits the creation-time spellings `Decimal32(S)`, `Decimal64(S)`,
`Decimal128(S)`, or `Decimal256(S)`, so `parse_ch_type` accepts only
`Decimal(P, S)` (mirroring the decision to accept only the concrete `Enum8(` /
`Enum16(` forms). The byte width is derived from `P`, not carried on the wire.

**Logical type:** `ChType::Decimal { precision: u8, scale: u8, bits: u16 }`.
`bits` is in `{32, 64, 128, 256}`, derived from `P` at parse time:

- `P` in  1..=9  -> 32 bits (backed by Int32, 4 bytes/row)
- `P` in 10..=18 -> 64 bits (backed by Int64, 8 bytes/row)
- `P` in 19..=38 -> 128 bits (backed by Int128, 16 bytes/row)
- `P` in 39..=76 -> 256 bits (backed by Int256, 32 bytes/row)

`Display` renders the canonical `Decimal(P, S)` (not `bits`), so
`parse(display(t)) == t`.

**Wire payload:** `num_rows * (bits / 8)` bytes, little-endian two's-complement,
contiguous, no per-row framing. Each row is one fixed-width signed integer of
4/8/16/32 bytes by precision: the unscaled value, equal to the decimal value
times `10^S`. Precision and scale are type metadata only and never appear in the
per-row data. The per-column bulk-state prefix reads zero bytes and the
custom-serialization marker is 0x00, same as a plain numeric.

**Arrow export:** the Arrow C Data decimal format string over the contiguous
fixed-width buffer, zero-copy. The 128-bit case is the bare `d:P,S` (the Arrow
spec default width); the other widths carry the bit width as a third field,
`d:P,S,bits` (for example `d:9,4,32`, `d:18,9,64`, `d:50,10,256`). 2 buffers in
order: validity, then the data buffer. There is no offsets buffer; row `i` is
`data[i * (bits / 8) .. (i + 1) * (bits / 8)]`. The buffer is exported verbatim
at its native width; the core deliberately does NOT widen a narrow decimal to
128 bits, which would cost a forbidden per-value copy in the decode loop.

Note: Arrow `decimal32` / `decimal64` / `decimal256` are newer in the Arrow C
Data Interface than `decimal128`. The `d:P,S,bits` spelling is the documented
form, but consumer support for the non-128 widths varies by Arrow
implementation and version. A binding that needs broad consumer compatibility
for the narrow or 256-bit decimals should confirm the consumer accepts the
native-width format string, or widen on its own side.

**Rust buffer:** `Column::Decimal(DecimalColumn)` where `DecimalColumn` is
`{ data: Vec<u8>, width: usize, precision: u8, scale: u8, validity: Option<Bitmap> }`.
`width` is `bits / 8`. The physical buffer is identical to a `FixedBinaryColumn`
of that width; `precision` and `scale` are carried on the column for bindings
that read the buffers directly, the same Column-vs-ChType split the temporals
use. `data` is the wire bytes verbatim.

**Notes:**

- **Decode is a host-agnostic raw passthrough.** The fixed-width bytes are
  stored unchanged, with no reinterpretation into a native integer, so the
  buffer stays correct on big-endian hosts and the core needs no native `i128`
  or `i256`. The host representation (a Python `Decimal`, a JS `BigInt`, and so
  on) is a binding concern, out of scope here. The unscaled value is `data` for
  a row read as a little-endian signed integer of `width` bytes; the decimal
  value is that divided by `10^scale`.
- A negative unscaled value is two's-complement at the full width: an unscaled
  `-1` is all-`0xFF` bytes of the width.
- `LowCardinality(Decimal(...))` is illegal: `DataTypeDecimal` is a
  `DataTypeDecimalBase` subclass whose `canBeInsideLowCardinality()` is false, so
  the server throws at construction and such a column never appears on the wire.
  The decoder keeps rejecting `Decimal` as a `LowCardinality` inner.

**Introduction version:** first-class long before the pinned tag (inferred from
release history); stable at `v26.6.1.1193-stable`.

**Server reference:** `SerializationDecimalBase` (its `final`
`serializeBinaryBulk` / `deserializeBinaryBulk` do a single contiguous read of
`sizeof(FieldType) * num_rows` bytes, byte-swapping only on big-endian hosts) and
`DataTypesDecimal` / `DataTypeDecimal::doGetName` (the canonical `Decimal(P, S)`
type string and the precision-to-width mapping), in `src/DataTypes/`.
`deserializeBinaryBulkStatePrefix` reads zero bytes and the custom-serialization
marker is 0x00. Confirmed at `v26.6.1.1193-stable`.

### Temporal types

This covers `Date`, `Date32`, `DateTime`, and `DateTime64`. All four are plain
bulk integers on the wire, identical in layout to the corresponding
`SerializationNumber<T>`. Timezone and precision are type metadata only and have
zero effect on the wire bytes.

**Type string(s) and per-type details:**

| Type string                              | Logical type                          | Wire element | Column variant | Bytes/row |
|------------------------------------------|---------------------------------------|--------------|----------------|-----------|
| `Date`                                   | `ChType::Date`                        | `u16`        | `Date`         | 2         |
| `Date32`                                 | `ChType::Date32`                      | `i32`        | `Date32`       | 4         |
| `DateTime`, `DateTime('<tz>')`           | `ChType::DateTime { timezone }`       | `u32`        | `DateTime`     | 4         |
| `DateTime64(P)`, `DateTime64(P, '<tz>')` | `ChType::DateTime64 { precision, timezone }` | `i64` | `DateTime64`   | 8         |

`parse_ch_type` reads the optional timezone as the single-quoted contents of the
type string (`None` when absent), and the `DateTime64` precision `P` as the
integer in `DateTime64(P[, '<tz>')`. A precision outside `0..=9` is rejected as
`UnsupportedType`.

**Logical meaning of the integer:**

- `Date`: days since 1970-01-01 (Unix epoch), unsigned. Range
  1970-01-01 .. 2149-06-06.
- `Date32`: days since 1970-01-01, signed (negative is before the epoch). Wider
  range than `Date`.
- `DateTime`: seconds since the Unix epoch, unsigned. Range 1970 .. 2106.
- `DateTime64(P)`: ticks where one tick is `10^-P` seconds, signed (negative is
  before the epoch). For example `DateTime64(3)` ticks are milliseconds.

**Wire payload:** `num_rows * bytes_per_row` bytes, little-endian, contiguous,
no per-row framing. Identical to the matching fixed-width numeric.

**Arrow export:** 2 buffers in order, validity then values, exactly like the
numerics. The export is zero-copy and never widens or rescales a buffer, so the
format string maps to a real Arrow temporal type only on an exact same-width
match, otherwise it exposes the raw integer:

- `Date` -> `S` (Arrow uint16). There is no 16-bit Arrow date, so this is raw
  days.
- `Date32` -> `tdD` (Arrow date32, i32 days). Exact match.
- `DateTime` -> `I` (Arrow uint32). There is no u32-seconds Arrow timestamp, so
  this is raw seconds.
- `DateTime64(P)` -> Arrow timestamp `ts{unit}:{tz}` only when `P` is in
  `{0, 3, 6, 9}`, where `unit` is `s`/`m`/`u`/`n` and `tz` is the timezone or the
  empty string. Examples: `DateTime64(3, 'UTC')` -> `tsm:UTC`,
  `DateTime64(0)` -> `tss:`, `DateTime64(6)` -> `tsu:`,
  `DateTime64(9, 'America/New_York')` -> `tsn:America/New_York`. For any other
  precision it falls back to `l` (Arrow int64, raw ticks), for example
  `DateTime64(2)` -> `l`.

**Rust buffer:** `Column::Date(PrimitiveColumn<u16>)`,
`Column::Date32(PrimitiveColumn<i32>)`,
`Column::DateTime(PrimitiveColumn<u32>)`, and
`Column::DateTime64(PrimitiveColumn<i64>)`. Each is `{ values, validity }` at the
faithful native width. `values` has length `num_rows` and, on little-endian
targets, is the wire bytes verbatim.

**Notes:** the Arrow export is zero-copy and never widens or rescales. `Date`
exports as Arrow uint16 (raw days), `DateTime` as uint32 (raw seconds),
`DateTime64(P)` as an Arrow timestamp only for `P` in `{0, 3, 6, 9}` and as int64
(raw ticks) otherwise, and `Date32` as Arrow date32. A consumer that wants full
Arrow temporal semantics for the integer-exported cases (`Date`, `DateTime`, and
the non-`{0,3,6,9}` `DateTime64` precisions) should request the server's
`ArrowStream` format instead. The timezone, when present, is preserved in the
`ChType` and in the `DateTime64` Arrow timestamp format string; it does not
change the stored integers. The emitted type string for a column declared with an
explicit timezone can depend on the negotiated protocol revision: at protocol
revision 0 (for example HTTP `FORMAT Native` with no `client_protocol_version`)
the server may drop the timezone and emit a bare `DateTime`, while at revision
54485 it emits `DateTime('<tz>')`. The decoder trusts and reflects whatever type
string the server actually wrote.

**Server reference:** `SerializationDate` (inherits `SerializationNumber<UInt16>`),
`SerializationDate32` (inherits `SerializationNumber<Int32>`),
`SerializationDateTime` (inherits `SerializationNumber<UInt32>`), and
`SerializationDateTime64` (inherits `SerializationDecimalBase<DateTime64>`, native
type `Int64`), in `src/DataTypes/Serializations/`. None override the binary bulk
path, so each is exactly its underlying integer: a single bulk raw read on
little-endian hosts, byte-swapped per element on big-endian hosts. Confirmed at
`v26.6.1.1193-stable`.

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
have equal length. Confirmed at `v26.6.1.1193-stable`.

### LowCardinality(T)

**Type string(s):** `LowCardinality(T)` and `LowCardinality(Nullable(T))` for
any inner type `T` in the allowlist below. The parser accepts any
`LowCardinality(<inner>)` and records it; decode then accepts the inner types
ClickHouse permits inside `LowCardinality` that this crate already decodes, and
rejects any other inner as `UnsupportedType`.

**Allowed inner types (after `removeNullable`):** `String`, `FixedString(N)`,
the fixed-width numerics (`Int8`/`Int16`/`Int32`/`Int64`,
`UInt8`/`UInt16`/`UInt32`/`UInt64`, `Float32`/`Float64`), `Bool`, the
number-backed temporals `Date`, `Date32`, and `DateTime`, and `UUID`/`IPv4`/`IPv6`.
The dictionary values are that inner type serialized as a plain column body
(varint-length strings for `String`, raw fixed-width bytes otherwise: 4 bytes per
`IPv4` entry, 16 bytes per `UUID`/`IPv6` entry), so support follows directly from
the per-type body decoder.

This allowlist is exactly `IDataType::canBeInsideLowCardinality()` intersected
with the types this crate decodes, confirmed against the server source at
`v26.6.1.1193-stable` (the `DataTypeLowCardinality` constructor checks it after
`removeNullable`). Three consequences worth calling out:

- `DateTime64` and every `Decimal` are **not** allowed: they are
  `DataTypeDecimalBase` subclasses whose `canBeInsideLowCardinality()` is false,
  so the server never emits `LowCardinality(DateTime64(...))`. The crate decodes
  `DateTime64` as an ordinary column but rejects it as a `LowCardinality` inner.
- `Enum8` and `Enum16` are **not** allowed either: `DataTypeEnum` does not
  inherit `DataTypeNumberBase`, so its `canBeInsideLowCardinality()` is false and
  the server throws `ILLEGAL_TYPE_OF_ARGUMENT` on `LowCardinality(Enum...)` at
  construction. The crate decodes `Enum8`/`Enum16` as ordinary columns but
  rejects them as `LowCardinality` inners. This is independent of `Enum` decode
  support; it is the server forbidding the combination.
- `UUID`, `IPv4`, and `IPv6` **are** permitted by the server and are now decoded
  by this crate, so a `LowCardinality` over them decodes through the dictionary
  path: a `UUID`/`IPv6` dictionary value column is a `FixedBinary` of width 16 and
  an `IPv4` dictionary value column is a `UInt32`-backed column.

The fixed-width numeric and temporal inners, and `IPv4`/`IPv6`, require the server
setting `allow_suspicious_low_cardinality_types=1` at table-creation time. That is
a server-side creation guard only: it has no effect on the wire bytes and is not
needed to decode a column the server already produced. `String`, `FixedString`,
and `UUID` are allowed unconditionally.

**Logical type:** `ChType::LowCardinality(Box<ChType>)`.

**Introduction version:** `LowCardinality` has been a stable ClickHouse type
since 19.x (it left experimental in 19.11). It exists and is stable at the
pinned tag `v26.6.1.1193-stable`.

**Wire payload:** the Native wire uses a single flat buffer, so every substream
(`DictionaryKeys`, `DictionaryIndexes`, the state prefix) resolves to the same
read buffer and the bytes below are exactly the serializer call order. All the
multi-byte words here are fixed 8-byte little-endian `u64`, written with the
server's `writeBinaryLittleEndian`, not varints.

A per-column bulk-state prefix precedes the per-block payload:

```text
[8 bytes LE u64]  key_version   // must be 1 (SharedDictionariesWithAdditionalKeys); else rejected
```

Then, per block (when the block has rows):

```text
[8 bytes LE u64]  index_type_word
                  //   bits 1:0 = index width: 0=u8 1=u16 2=u32 3=u64
                  //   bit 8 (0x100) = NeedGlobalDictionaryBit -> must be CLEAR in Native; rejected if set
                  //   bit 9 (0x200) = HasAdditionalKeysBit    -> set in Native
                  //   higher bits (e.g. bit 10 = NeedUpdateDictionary) may be set; the decoder masks only the bits it acts on and ignores the rest
[8 bytes LE u64]  num_keys      // dictionary entry count for THIS block
[num_keys values] dictionary    // inner type's plain serializeBinaryBulk body: varint len + raw bytes for String, raw fixed-width LE bytes otherwise
[8 bytes LE u64]  num_rows      // re-stated; must equal the block row count
[num_rows * w]    indexes       // raw LE array (NOT varint), w = index width, each an index into this block's dictionary
```

The dictionary is per block (additional keys). This core never concatenates
blocks, so each chunk gets its own dictionary and indexes resolve against that
chunk's dictionary, never a shared global one. `NeedGlobalDictionaryBit` is
never set in Native (the server rejects it for `native_format`), so the decoder
rejects it as well.

Important divergence from the abstract serialization model: although
`deserializeBinaryBulkStatePrefix` is conceptually a once-per-column step, the
Native reader creates a fresh deserialize state for every column in every block
(`NativeReader::readData`), so the `key_version` prefix is emitted per block per
column, immediately before that block's index payload. A zero-row block reads no
prefix and no data for the column.

For `LowCardinality(Nullable(T))` the dictionary value type after
removeNullable is the bare `T`. The wire transmits `num_keys` values of `T`;
dictionary index 0 is the NULL sentinel and its on-wire value is the inner
default (an empty string for `String`, a zero for the fixed-width inners). Rows
whose index is 0 are NULL.

**Arrow export:** Arrow `dictionary(i32, V)` where `V` is the inner value type's
Arrow format. The schema field's own format string is the index type `i` (int32)
and the value type lives in the schema's `dictionary` child (`u` for a `String`
inner, `I` for `UInt32`, `S` for `Date`, `w:N` for `FixedString(N)`, and so on:
the same per-type format the bare inner exports). The nullable flag is set on the
field when the inner type is `Nullable`. The array carries 2 index buffers in
order: validity then the i32 index data, and its `dictionary` child array holds
the dictionary values exported as a column of the inner type.

Index-width decision: the Native payload self-describes the index width
(u8..u64) per block, and the decoder normalizes every index to a single signed
`i32`, widening the native width during decode. This matches the index type
pyarrow accepts for a dictionary array and keeps the `Column` model and the
Arrow export single-shaped rather than branching the index format per chunk. A
per-block dictionary large enough to overflow `i32` is not a real Native payload
and is rejected.

Null-representation decision: in Arrow a null in a dictionary array lives in the
indices validity bitmap, not as a dictionary entry. The decoder maps each
wire-index-0 row (the ClickHouse NULL sentinel) to a null bit in the index
validity bitmap and leaves that row's i32 index at 0 (pointing at the harmless
sentinel entry, never read). The dictionary `values` column still contains the
`num_keys` entries the wire carried, including the inner-default sentinel at slot
0. A plain (non-nullable) `LowCardinality(T)` has no validity bitmap; the server
still reserves dictionary slot 0 with the inner default, but it is never
referenced (the indexes start at 1), so it carries no null sentinel.

**Rust buffer:** `Column::Dictionary(DictionaryColumn)` where `DictionaryColumn`
is `{ indices: Vec<i32>, validity: Option<Bitmap>, values: Box<Column> }`.
`indices` has length `num_rows`. `validity` is `Some` only for a nullable inner
type. `values` is the per-block dictionary as its own `Column` of the inner type:
a `Utf8Column` for `String`, a `FixedBinaryColumn` for `FixedString(N)`, or the
matching `PrimitiveColumn<T>` (for example `Column::UInt32` or `Column::Date`)
for a numeric or temporal inner.

**Notes:**

- The dictionary is local to each chunk. Two chunks of one result can have
  different dictionaries and different (normalized away) native index widths.
- A null row's i32 index is 0; always consult the validity bitmap before reading
  the value, exactly as for `Nullable(T)`.

**Server reference:** `SerializationLowCardinality::deserializeBinaryBulkStatePrefix`
(reads the `key_version`) and
`SerializationLowCardinality::deserializeBinaryBulkWithMultipleStreams` (the
per-block index word, additional-keys dictionary, row count, and index array) in
`src/DataTypes/Serializations/SerializationLowCardinality.cpp`, with the
per-column-per-block state and the `if (rows)` gate in
`NativeReader::readData` (`src/Formats/NativeReader.cpp`). The index word layout
and the index-0 NULL sentinel are in `IndexesSerializationType` and
`read_additional_keys` in the same serialization file. Confirmed at
`v26.6.1.1193-stable`.

---

## Zero-row output

When a block has `num_rows == 0` the chunk is dropped from `chunks`, but the
schema is still established. If a consumer constructs or inspects an empty column
directly (`empty_column` in `src/native/decode.rs`), the empty shapes are:

- Numerics and `Bool`: empty value or bit buffer, length 0. `IPv4` (a `u32`
  primitive) is the same.
- `String`: `offsets == [0]` (length 1, the required leading zero) and empty
  data.
- `FixedString(N)`: empty data, width preserved. `UUID` and `IPv6` are the same
  with width 16.
- `Decimal(P, S)`: empty data, width (`bits / 8`), precision, and scale
  preserved, like `FixedString(N)`.
- `Nullable(T)`: as above with an empty validity bitmap.
- `LowCardinality(T)`: empty indices, an empty values dictionary column, and (for
  a nullable inner type) an empty index validity bitmap.

In all cases length is 0 and `null_count` is 0.

---

## Unsupported types

`parse_ch_type` returns no match for any type not listed in the support matrix.
Decoding such a column fails with `DecodeError::UnsupportedType { column,
type_name }` rather than producing a wrong or partial column. There is no silent
fallback. A consumer can treat an unsupported type as a hard decode error.

Not yet supported, tracked as planned phases in `src/schema.rs`:

- `LowCardinality(T)` for an inner type outside the allowlist in the
  `LowCardinality(T)` section. The wrapper and its allowed inners (String,
  FixedString, the fixed-width numerics, Bool, Date, Date32, DateTime,
  UUID/IPv4/IPv6, with or without an inner `Nullable`) are supported; any other
  inner is rejected as `UnsupportedType`. This includes `DateTime64`, every
  `Decimal`, and `Enum8`/`Enum16`, all of which the server itself forbids as LC
  inners (`canBeInsideLowCardinality()` is false), so they never appear in that
  position on the wire.
- Containers: `Array(T)`, `Tuple(...)`, `Map(K, V)`.
- Wide integers: `Int128`, `UInt128`, `Int256`, `UInt256`.

A malformed `LowCardinality` payload (a bad key version, the
`NeedGlobalDictionaryBit` set, an index width tag outside `0..=3`, an out-of-range
index, or a row count that disagrees with the block header) fails with
`DecodeError::InvalidLowCardinality` rather than `UnsupportedType`.

When one of these is implemented, move it into the support matrix and add a type
section here.
