# Codec Contract: Supported Types, Decoded Output, and Encode Input

This is the definitive reference for what `ch-core-rs` decodes and encodes, and
for the exact shapes on both sides of each path. The crate runs two inverse
paths over one shared columnar model and one shared wire format:

- **Decode** (`src/native/decode/mod.rs`): ClickHouse `FORMAT Native` bytes ->
  `Column` buffers -> Arrow C Data export (`src/ffi/mod.rs`).
- **Encode** (`src/native/encode/mod.rs`): `Column` buffers -> ClickHouse
  `FORMAT Native` bytes the server accepts for `INSERT`.

For every supported ClickHouse type this doc records three views, all shared by
both paths:

1. The **wire payload**: the Native bytes decode reads and encode writes. This
   is one description of one format; the two paths are exact inverses over it.
2. The raw Rust `Column` buffers defined in `src/column.rs`: decode's output and
   encode's input.
3. The Arrow C Data export produced by `src/ffi/mod.rs`: the format string, the
   buffer count, and the buffer order. This is the primary decode contract
   surface. Encode has no Arrow export; it consumes the `Column` buffers directly.

The per-type sections below describe these shared shapes. Everything specific to
the encode direction (its API, trust and error model, input preconditions, the
choices it makes where the wire format allows more than one valid encoding, and
the round-trip guarantees) lives in the "Encoding" section, which does not
repeat the per-type wire layouts.

This document describes the core's own input and output only. It says nothing
about how any particular consumer should map decoded output to a host language,
or build the `Column` buffers it hands to encode. Host value policy is out of
scope by design.

## Source of truth and how to keep this current

The code is authoritative. If this file and the source disagree, the source
wins and this file is stale. When a new type is added to the decoder, update the
support matrix and add a type section here in the same change; when its encoder
lands, update the "Encoding" coverage list in the same change. See the "Adding A
New ClickHouse Type" workflow in `AGENTS.md`.

Baseline wire-layout claims below were confirmed against ClickHouse
`v26.6.1.1193-stable`; the Geometry and MultiPoint contract was reconfirmed
against the tag currently pinned in `.server-ref`, `v26.8.1.2041-lts`, via the
`clickhouse-server-reader` sub-agent. Per-type payloads are verified by the
crate's round-trip decode and encode tests. Each type section cites the server
serialization class and method it was confirmed against. When the pinned tag
moves, reconfirm the affected layouts and update their citations as `AGENTS.md`
describes.

## How to read a type section

Every type section uses the same fields, in the same order:

- **Type string(s)**: the exact ClickHouse type name(s) that `parse_ch_type`
  in `src/native/type_parser.rs` accepts for this type.
- **Logical type**: the `ChType` variant in `src/schema.rs`.
- **Wire payload**: the bytes decode reads and encode writes for this column, for
  a block of `num_rows` rows. The two paths are exact inverses over these bytes.
  This is the per-column payload only. Block framing and the nullable null map are
  described once below, not repeated per type.
- **Arrow export**: the Arrow format string and the buffers emitted by
  `export_column_array` in `src/ffi/mod.rs`, in order. Decode only.
- **Rust buffer**: the `Column` variant and the fields of its backing struct in
  `src/column.rs`. This is decode's output and encode's input.
- **Notes**: edge cases, range limits, and anything a consumer can get wrong.
- **Server reference**: the server serialization class and method that defines
  the layout, confirmed at the exact server version named in that section.

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

Variable-length columns use Arrow's offset layout:

- `offsets` has length `num_rows + 1` with `offsets[0] == 0`.
- Row `i` occupies `data[offsets[i] .. offsets[i + 1]]`.
- Offsets are monotonically non-decreasing.
- `String` uses 32-bit signed offsets and Arrow Utf8 (`u`).
- `AggregateState` uses 64-bit signed offsets and Arrow LargeBinary (`Z`).

Because String offsets are `i32`, a single chunk's string data buffer is limited
to about 2 GiB. Blocks stay separate, so this is a per-chunk limit, not a
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
TCP or protocol-framed HTTP payloads. `DBMS_TCP_PROTOCOL_VERSION` remains 54485,
the highest revision this crate fully supports. The pinned 26.8 server's current
revision is 54492, but revision 54492 enables the separate String size-stream
layout, which this crate does not yet implement. Bindings must cap negotiation
at 54485, not clamp only the option passed to the core after higher-revision
bytes have been produced. Decode and encode reject any revision above 54485.
Fixtures therefore negotiate 54485 explicitly.

This section describes the server layout at `v26.8.1.2041-lts` while negotiating
the supported revision 54485
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
   At negotiated revision 54485 all three fields are written. The
   standard block (not overflows, `bucket_num` -1, empty `out_of_order_buckets`)
   serializes to 10 bytes: `01 00 02 FF FF FF FF 03 00 00`. At revisions below
   54480 the same preamble was 8 bytes.
2. `num_columns` as a varint.
3. `num_rows` as a varint.
4. For each column, in order:
   - column name, a varint-length-prefixed string,
   - type header, either a varint-length-prefixed name in the default mode or a
     `DataTypesBinaryEncoding` descriptor when a `*_binary_types` API is used,
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

ClickHouse selects binary type headers with
`output_format_native_encode_types_in_binary_format`. Callers must select the
matching explicit `decode_*_binary_types` API for that stream. Binary output
from an `encode_*_binary_types` API requires the receiving INSERT to set
`input_format_native_decode_types_in_binary_format=1`; the default APIs retain
the textual header contract. The binary descriptor parser is bounded by nesting
depth, node count, and container count, applies the same type-construction rules
as textual headers, and rejects unsupported or reserved tags before decoding a
payload.

### How the decoder reads this

`decode_next_block` in `src/native/decode/mod.rs`, driven by
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

| ClickHouse type                                                                        | ChType                                 | Column variant    | Arrow format                                                        | Arrow buffers (in order)                   | Nullable                                                 |
|----------------------------------------------------------------------------------------|----------------------------------------|-------------------|---------------------------------------------------------------------|--------------------------------------------|----------------------------------------------------------|
| `Bool`, `Boolean`                                                                      | `Bool`                                 | `Bool`            | `b`                                                                 | validity, data bits                        | yes                                                      |
| `Int8`                                                                                 | `Int8`                                 | `Int8`            | `c`                                                                 | validity, values                           | yes                                                      |
| `Int16`                                                                                | `Int16`                                | `Int16`           | `s`                                                                 | validity, values                           | yes                                                      |
| `Int32`                                                                                | `Int32`                                | `Int32`           | `i`                                                                 | validity, values                           | yes                                                      |
| `Int64`                                                                                | `Int64`                                | `Int64`           | `l`                                                                 | validity, values                           | yes                                                      |
| `UInt8`                                                                                | `UInt8`                                | `UInt8`           | `C`                                                                 | validity, values                           | yes                                                      |
| `UInt16`                                                                               | `UInt16`                               | `UInt16`          | `S`                                                                 | validity, values                           | yes                                                      |
| `UInt32`                                                                               | `UInt32`                               | `UInt32`          | `I`                                                                 | validity, values                           | yes                                                      |
| `UInt64`                                                                               | `UInt64`                               | `UInt64`          | `L`                                                                 | validity, values                           | yes                                                      |
| `Float32`                                                                              | `Float32`                              | `Float32`         | `f`                                                                 | validity, values                           | yes                                                      |
| `Float64`                                                                              | `Float64`                              | `Float64`         | `g`                                                                 | validity, values                           | yes                                                      |
| `BFloat16`                                                                             | `BFloat16`                             | `BFloat16`        | `w:2`                                                               | validity, data                             | yes                                                      |
| `QBit(T, N)` for `T` = `BFloat16`/`Float32`/`Float64`                                  | `QBit { element_type, dimension }`     | `QBit`            | `+w:N` (FixedSizeList; scalar child format follows `T`)              | validity (+ one row-major scalar child)    | yes                                                      |
| `Nothing`                                                                              | `Nothing`                              | `Nothing`         | `n`                                                                 | none                                       | yes                                                      |
| `String`                                                                               | `String`                               | `Utf8`            | `u`                                                                 | validity, offsets, data                    | yes                                                      |
| `FixedString(N)`                                                                       | `FixedString(N)`                       | `FixedBinary`     | `w:N`                                                               | validity, data                             | yes                                                      |
| `UUID`                                                                                 | `Uuid`                                 | `Uuid`            | `w:16`                                                              | validity, data                             | yes                                                      |
| `IPv4`                                                                                 | `Ipv4`                                 | `Ipv4`            | `I`                                                                 | validity, values                           | yes                                                      |
| `IPv6`                                                                                 | `Ipv6`                                 | `Ipv6`            | `w:16`                                                              | validity, data                             | yes                                                      |
| `Enum8(...)`                                                                           | `Enum8 { variants }`                   | `Enum8`           | `c`                                                                 | validity, values                           | yes                                                      |
| `Enum16(...)`                                                                          | `Enum16 { variants }`                  | `Enum16`          | `s`                                                                 | validity, values                           | yes                                                      |
| `Decimal(P, S)`                                                                        | `Decimal { precision, scale, bits }`   | `Decimal`         | `d:P,S` (128-bit) or `d:P,S,bits` (32/64/256-bit)                   | validity, data                             | yes                                                      |
| `Int128`                                                                               | `Int128`                               | `Int128`          | `w:16`                                                              | validity, data                             | yes                                                      |
| `UInt128`                                                                              | `UInt128`                              | `UInt128`         | `w:16`                                                              | validity, data                             | yes                                                      |
| `Int256`                                                                               | `Int256`                               | `Int256`          | `w:32`                                                              | validity, data                             | yes                                                      |
| `UInt256`                                                                              | `UInt256`                              | `UInt256`         | `w:32`                                                              | validity, data                             | yes                                                      |
| `Date`                                                                                 | `Date`                                 | `Date`            | `S`                                                                 | validity, values                           | yes                                                      |
| `Date32`                                                                               | `Date32`                               | `Date32`          | `tdD`                                                               | validity, values                           | yes                                                      |
| `DateTime`, `DateTime('<tz>')`                                                         | `DateTime { timezone }`                | `DateTime`        | `I`                                                                 | validity, values                           | yes                                                      |
| `DateTime64(P)`, `DateTime64(P, '<tz>')`                                               | `DateTime64 { precision, timezone }`   | `DateTime64`      | `ts{unit}:{tz}` for P in {0,3,6,9}, else `l`                        | validity, values                           | yes                                                      |
| `Time`                                                                                 | `Time`                                 | `Time`            | `i`                                                                 | validity, values                           | yes                                                      |
| `Time64(P)`                                                                            | `Time64 { precision }`                 | `Time64`          | `l`                                                                 | validity, values                           | yes                                                      |
| `IntervalYear` ... `IntervalNanosecond`                                                 | `Interval(IntervalKind)`               | `Interval`        | `tDs`/`tDm`/`tDu`/`tDn` for s/ms/us/ns, else `l`                    | validity, values                           | yes                                                      |
| `Nullable(T)`                                                                          | `Nullable(T)`                          | inner T's variant | inner's                                                             | inner's, validity populated                | n/a                                                      |
| `LowCardinality(T)` for an allowed inner `T` (see the type section)                    | `LowCardinality(Box<ChType>)`          | `Dictionary`      | `i` (index type; values type in the dictionary child)               | validity, i32 indices (+ dictionary child) | via inner `Nullable`                                     |
| `Array(T)` for any supported element `T`                                               | `Array(Box<ChType>)`                   | `Array`           | `+L` (LargeList; element type in the item child)                    | validity, i64 offsets (+ item child)       | no (array level); element nulls via `Array(Nullable(T))` |
| `Tuple(T1, ...)` / `Tuple(name1 T1, ...)` for supported element types, incl. `Tuple()` | `Tuple(Vec<(Option<String>, ChType)>)` | `Tuple`           | `+s` (struct; element types in the children)                        | validity (one child per element)           | yes (`Nullable(Tuple(...))` is legal)                    |
| `Map(K, V)` for a legal key type and supported `K`/`V`                                 | `Map(Box<ChType>, Box<ChType>)`        | `Map`             | `+L` (LargeList of an `entries` struct with `key`/`value` children) | validity, i64 offsets (+ entries child)    | no (map level); value nulls via `Map(K, Nullable(V))`    |
| `SimpleAggregateFunction(func, T)` for a supported inner `T`                            | `SimpleAggregateFunction { func, inner }` | inner `T`'s    | inner `T`'s                                                         | inner `T`'s                                | via inner `Nullable` iff `Nullable(T)` is legal          |
| Registered exact `AggregateFunction` codecs: `count`, canonical `nothingUInt64` and `nothingNull`, and base `sum` over plain or Nullable numeric/Enum arguments | `AggregateFunction { function, arguments }` | `AggregateState` | `Z` (LargeBinary) | validity, i64 offsets, state data | no |
| `Point`                                                                                | `Geo(GeoKind::Point)`                  | `Tuple`           | `+s` (struct of two `g` Float64 children)                          | validity (two Float64 children)            | yes (`Nullable(Point)` is legal)                         |
| `Ring`, `LineString`, `MultiLineString`, `Polygon`, `MultiPolygon`, `MultiPoint`       | `Geo(GeoKind::*)`                       | `Array`           | `+L` (LargeList chain over a Point `+s` struct)                     | validity, i64 offsets (+ item child)       | no (they expand to `Array`)                              |
| `Geometry`                                                                             | `Geometry`                              | `Variant`         | `+ud:0,1,2,3,4,5,6,7` Dense Union                                 | i8 type ids, i32 offsets (+ seven geo children and NULL) | intrinsic NULL child; no top-level validity |
| `Nested(name1 T1, ...)` for supported field types                                      | `Nested(Vec<(String, ChType)>)`        | `Array`           | `+L` (LargeList of a `+s` struct with the field names)             | validity, i64 offsets (+ item struct child) | no (it is an `Array`)                                    |
| `Variant(T1, ...)` for 1 through 255 legal alternatives                               | `Variant(Vec<ChType>)`                 | `Variant`         | `+ud:...` Dense Union, nested for 128+ alternatives                | i8 type ids, i32 offsets (+ dense children) | intrinsic NULL child; no top-level validity              |
| `Dynamic`, `Dynamic(max_types=N)`                                                     | `Dynamic { max_types }`                | `Dynamic`         | `+ud:...` result-wide Dense Union; SharedVariant child is `z`      | remapped i8 type ids, i32 offsets (+ dense children) | intrinsic NULL child; no top-level validity              |
| `JSON`, `JSON(...)` (typed paths, `max_dynamic_paths`/`max_dynamic_types`, `SKIP`)     | `Json { max_dynamic_paths, max_dynamic_types, typed_paths, skip_paths, skip_regexps }` | `Json` | `+s` (struct of typed paths, dynamic-path Dense Unions, and a `_shared_data` LargeList); `u` for a STRING-mode body | validity (one child per typed path, per dynamic path, plus `_shared_data`) | yes (`Nullable(JSON)` is legal) |

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

### Nothing

**Type string(s):** `Nothing`. This is the exact case-sensitive canonical name
registered by `registerDataTypeNothing` and written in Native text headers.

**Logical type:** `ChType::Nothing`.

**Wire payload:** exactly `num_rows` bytes. The server writes one canonical
ASCII `0` byte, `0x30`, per row. Its bulk reader consumes one byte per row and
does not validate the byte value. There is no type-specific prefix, suffix,
length, state, or endianness concern. `Nothing` is zero-width only in the
server's in-memory column model; it is not zero-width on the Native wire. A
zero-row block carries no body, under the ordinary Native `rows != 0` gate.

For `Nullable(Nothing)`, the ordinary Nullable framing still applies: first
`num_rows` null-map bytes, then the complete `num_rows`-byte nested Nothing
body. A legitimately constructed column has `0x01` in every null-map slot and
`0x30` in every nested slot. The server's Native reader structurally accepts
other mask and placeholder bytes, so decode retains the mask and ignores the
placeholder values. Encode writes the retained mask and canonicalizes every
nested placeholder to `0x30`.

**Arrow export:** Arrow Null, format `n`, with `length == num_rows`,
`null_count == num_rows`, and zero buffers. `Nullable(Nothing)` has the same
Arrow array body because Arrow Null is intrinsically all-null; the ClickHouse
null map remains in the Rust column only for Native decode/encode fidelity.
Both bare `Nothing` and `Nullable(Nothing)` set `ARROW_FLAG_NULLABLE` (2):
Arrow requires a Null-type field to be nullable (pyarrow rejects a
non-nullable Null field by construction), so the ClickHouse wrapper
distinction is not preserved in the field flags. It survives in the ClickHouse
type name carried alongside the schema.

**Rust buffer:** `Column::Nothing(NothingColumn { len, validity })`. The explicit
length is required because there is no value buffer from which to recover the
row count. `validity` is `None` for bare Nothing and retains the packed
ClickHouse null map for `Nullable(Nothing)`. `null_count()` counts that
structural mask like every other column; the Arrow rule that a Null array
reports every row null is applied at the FFI export site.
Decode performs no allocation or copy for the ignored body bytes beyond
the optional Nullable bitmap that every nullable type already needs.

**Wrappers, containers, and keys:** `Nullable(Nothing)` is legal.
`LowCardinality(Nothing)` and `LowCardinality(Nullable(Nothing))` are illegal
because Nothing does not opt into `canBeInsideLowCardinality`. `Array(Nothing)`,
Tuple elements, Map values, and a bare Nothing Map key are constructible. A
plain Nothing value cannot be created for a non-empty SQL result, so
`Array(Nothing)` and maps with a Nothing key or value are semantically empty.
Nullable Nothing Map keys remain illegal under the ordinary Map key rule.
Top-level Nothing and Nullable(Nothing) cannot be stored in tables, though the
type occurs in query results and can be nested in storable Tuple and Map types.

**Introduction version:** undetermined from the shallow pinned checkout. No
Nothing-specific setting, protocol-revision branch, or experimental gate exists
at the pin.

**Server reference:** all wire and legality claims above are **confirmed** at
`v26.6.1.1193-stable`. The implementation relies on
`DataTypeNothing` and `registerDataTypeNothing` in
`src/DataTypes/DataTypeNothing.{h,cpp}`;
`SerializationNothing::serializeBinaryBulk` and
`SerializationNothing::deserializeBinaryBulk` in
`src/DataTypes/Serializations/SerializationNothing.cpp`;
`SerializationNullable::serializeBinaryBulkWithMultipleStreams` and its
deserialize counterpart in
`src/DataTypes/Serializations/SerializationNullable.cpp`;
`NativeWriter::write` and `NativeReader::read` in
`src/Formats/NativeWriter.cpp` and `src/Formats/NativeReader.cpp`; and the
wrapper/container predicates in `DataTypeNullable`, `DataTypeLowCardinality`,
`DataTypeArray`, `DataTypeTuple`, and `DataTypeMap`. Server tests
`00570_empty_array_is_const`, `02294_nothing_arguments_in_functions`,
`03034_ddls_and_merges_with_unusual_maps`,
`03173_row_binary_and_native_with_binary_encoded_types`, and
`03243_check_for_nullable_nothing_in_alter` corroborate the behavior. The
header comment in `SerializationNothing.h` that says the bulk methods read and
write zero bytes is stale and contradicted by both function bodies and live
Native bytes. Only the introduction release is **inferred as unknown**, rather
than guessed.

### BFloat16

**Type string(s):** `BFloat16`. This is the exact case-sensitive canonical name
written in Native text headers. The server's general numeric factory tolerates
up to two creation-time arguments and normalizes them away, but `getName()`
always emits the bare `BFloat16`, so the Native decoder accepts only that
canonical spelling.

**Logical type:** `ChType::BFloat16`.

**Wire payload:** exactly `num_rows * 2` contiguous bytes, one raw BFloat16 word
per row in little-endian order, with no per-row framing and no BFloat-specific
bulk-state prefix or suffix. The 16-bit word is the top half of an IEEE-754
Float32: 1 sign bit, 8 exponent bits, and 7 mantissa bits. Server conversion
from Float32 truncates the low 16 bits rather than rounding. Every raw pattern,
including signed zero, infinities, subnormals, and NaN payloads, passes through
unchanged.

**Arrow export:** format `w:2` (FixedSizeBinary(2)), with 2 buffers in order:
validity, then the raw data bytes. Arrow's `e` format is IEEE binary16, whose
exponent and mantissa layout is incompatible with BFloat16. Exporting as `S`
would preserve the bits but falsely advertise UInt16 numeric semantics. The
opaque width-2 export is lossless, host-independent, and zero-copy; a binding
uses the accompanying `ChType::BFloat16` to materialize host BFloat16 or Float32
values.

**Rust buffer:** `Column::BFloat16(PrimitiveColumn<[u8; 2]>)` with
`values.len() == num_rows` and an optional validity bitmap. Each array element
is one exact little-endian wire word, so the width-2 invariant is structural and
cannot disagree with the Arrow `w:2` schema. Decode performs one column
allocation and one contiguous copy, with no widening, conversion, or per-value
allocation. Encode writes the same bytes verbatim after validating the row
count.

**Wrappers and keys:** `Nullable(BFloat16)`, `LowCardinality(BFloat16)`, and
`LowCardinality(Nullable(BFloat16))` are legal. BFloat16 is a numeric
LowCardinality inner, so persisted schema declarations and explicit CAST targets
need `allow_suspicious_low_cardinality_types = 1`; that setting has no effect on
wire bytes. Bare and non-nullable-LowCardinality BFloat16 Map keys are legal.
Nullable and `LowCardinality(Nullable(BFloat16))` Map keys are illegal under the
generic Map key rule. A null row's nested two bytes are an unspecified
placeholder; validity is authoritative.

**Introduction version:** the server settings history confirms an experimental
BFloat16 gate was added in compatibility version 24.11, defaulted on in 25.1,
and is obsolete and always true at the pin. The exact first shipped release is
**inferred** to be 24.11, not confirmed from this shallow checkout.

**Server reference:** `registerDataTypeNumbers` and
`createNumericDataType<BFloat16>` in `src/DataTypes/DataTypesNumber.cpp`;
`DataTypeNumber<BFloat16>` in `src/DataTypes/DataTypesNumber.h`;
`SerializationNumber<BFloat16>::serializeBinaryBulk` and
`deserializeBinaryBulk` in
`src/DataTypes/Serializations/SerializationNumber.cpp`; the raw word and
Float32 conversion in `base/base/BFloat16.h`; Nullable/LowCardinality/Map
legality in `src/DataTypes/DataTypeNullable.cpp`,
`src/DataTypes/DataTypeNumberBase.h`, `src/DataTypes/DataTypeLowCardinality.cpp`,
and `src/DataTypes/DataTypeMap.cpp`. The raw width and byte order are also
covered by server tests `03269_bf16` and `03733_sparse_negative_zero`. All wire,
wrapper, and key claims above are confirmed at `v26.6.1.1193-stable`; only the
exact introduction release is inferred.

### QBit

**Type string(s):** canonical `QBit(T, N)`, where `T` is `BFloat16`,
`Float32`, or `Float64`, and `N` is an unsigned dimension in
`1..=134217720`. The server also accepts the case-insensitive scalar aliases
`FLOAT`, `REAL`, and `SINGLE` for `Float32`, and `DOUBLE` and
`DOUBLE PRECISION` for `Float64`; `ChType::Display` always emits the canonical
scalar name and a comma followed by one space. The dimension limit follows the
server's internal `FixedString(ceil(N / 8))` representation and its maximum
FixedString width of `0x00ffffff` bytes.

**Logical type:** `ChType::QBit { element_type: QBitElementType, dimension }`.

**Native bulk wire payload:** let `R = num_rows`, `B` be the scalar bit width
(16, 32, or 64), and `K = ceil(N / 8)`. The body is exactly `B * R * K` bytes
with no QBit-specific prefix, suffix, per-row dimension, or offsets. It is
ordered as B complete bit planes. Within plane `p`, each row contributes one
K-byte FixedString. Plane `p` carries scalar word bit `B - 1 - p`, so the sign
bit is first. Logical element `i` is stored at physical byte
`K - 1 - floor(i / 8)`, bit `i % 8`. Thus the last physical byte holds logical
elements 0 through 7. Each Native block transposes independently, and a zero-row
block has no body.

Decode performs the inverse transpose once into exact row-major scalar words.
All bit patterns are preserved, including signed zero, subnormals, infinities,
and NaN payloads. Unused padding bits in the first physical byte are ignored.
This tolerance is part of the decode contract: Native bulk readers must accept
nonzero padding even though every confirmed server write path and this crate's
encoder produce canonical zero padding. A valid but noncanonical input therefore
decodes normally and re-encodes to the canonical zero-padded body.

**Nullable:** `Nullable(QBit(T, N))` writes the ordinary `R`-byte ClickHouse
null map first, then the complete QBit body for all R rows, including null rows.
The validity bitmap belongs to the whole vector. Nested scalar values for a null
row are placeholders and must not be interpreted.

**Rust buffer:** `Column::QBit(QBitColumn)` contains `dimension`, optional
vector-level validity, and one boxed row-major scalar child with exactly
`R * N` values. The child is `Column::BFloat16`, `Column::Float32`, or
`Column::Float64` and has no child validity. Decode allocates only that final
scalar vector, then reconstructs up to eight rows from one plane byte at a time;
there are no intermediate planes and no per-value heap allocations. Encode
reserves the final Native body in the destination and writes the transposed
bytes directly, also without an intermediate plane buffer.

**Arrow export:** format `+w:N` (FixedSizeList(N)), with one parent validity
buffer and one child named `item`. The child length is `R * N`; its format is
`w:2` for BFloat16, `f` for Float32, or `g` for Float64, with the usual child
validity and values buffers. Child validity is always null. Parent and child
buffers borrow the decoded allocations through the existing Arrow C Data
ownership model, so export is zero-copy and does not transpose again. A
zero-row QBit still exports its one empty scalar child.

**Binary type descriptor and single values:** the binary Native type header is
tag `0x36`, followed by the recursive scalar descriptor and `N` as VarUInt.
For example, `QBit(Float32, 9)` is `36 0d 09`, and
`QBit(Float64, 300)` is `36 0e ac 02`. The single-value binary form used by
RowBinary and SharedVariant is different from Native bulk: it writes `N` as
VarUInt followed by N ordinary little-endian scalar values. The decoder requires
that in-band dimension to equal the declared type. Because a null single value
has no nested payload bytes but Arrow-shaped output still needs placeholders,
`decode_binary_value` caps cumulative null-default materialization at 16 MiB per
call. This preserves a maximum-width `FixedString` placeholder. It intentionally
rejects a null QBit when its dimension exceeds 2,097,152 for `Float64`,
4,194,304 for `Float32`, or 8,388,608 for `BFloat16`, even when that dimension
and value are otherwise well-formed. Top-level `Nullable(QBit(...))` cannot be a
SharedVariant cell because Variant forbids an outer Nullable type and Dynamic
uses its null discriminator instead. Nested forms remain reachable, for example
an `Array(Nullable(QBit(...)))` cell, so their cumulative placeholder expansion
is subject to the same cap and returns `Invalid` when exceeded.

**Wrappers and containers:** `Nullable(QBit(...))` is legal.
`LowCardinality(QBit(...))` is forbidden because QBit cannot be inside
LowCardinality. QBit composes directly in `Array`, `Tuple`, `Variant`, Dynamic,
and as a non-null Map key or value. The generic Map rule still forbids a
nullable QBit key.

**Introduction and compatibility:** QBit was introduced as experimental in
ClickHouse 25.10, promoted to Beta and enabled by default in 26.1, and became GA
in 26.2. ClickHouse 26.4 records a backward-incompatible change making QBit
single-value binary serialization explicitly little-endian. The pinned call
graph confirms that Native bulk continues to use the FixedString plane path with
no protocol-revision or server-version branch; the 26.4 endian helpers apply
only to single-value and text paths, not Native bulk.

**Server reference:** all Native bulk, type grammar, descriptor, range, and
wrapper claims above are **confirmed** at `v26.6.1.1193-stable` from
`DataTypeQBit::create`, `DataTypeQBit::doGetName`, and
`registerDataTypeQBit` in `src/DataTypes/DataTypeQBit.{h,cpp}`;
`SerializationQBit::serializeBinaryBulkWithMultipleStreams`,
`deserializeBinaryBulkWithMultipleStreams`, `transposeBits`,
`serializeFloatsFromQBit`, `serializeBinary`, and `deserializeBinary` in
`src/DataTypes/Serializations/SerializationQBit.{h,cpp}`;
`SerializationTuple` and `SerializationFixedString` bulk methods in the same
directory; `SerializationNullable` for null-map order; `ColumnQBit` in
`src/Columns/ColumnQBit.{h,cpp}`; `NativeWriter` and `NativeReader` in
`src/Formats/`; and `DataTypesBinaryEncoding.{h,cpp}`. Release milestones are
confirmed by `docs/changelogs/v25.10.1.3832-stable.md`,
`v26.1.1.912-stable.md`, `v26.2.1.1139-stable.md`, and
`v26.4.1.1141-stable.md`, with setting transitions in
`src/Core/SettingsChangesHistory.cpp` and obsolete settings at the pin in
`src/Core/Settings.cpp`.

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

### Int128 / UInt128 / Int256 / UInt256

**Type string(s):** `Int128`, `UInt128`, `Int256`, and `UInt256`, the exact
case-sensitive spellings the server emits via `DataTypeNumber<T>::doGetName`. No
parameters, no aliases; `parse_ch_type` matches only these.

**Logical type:** `ChType::Int128`, `ChType::UInt128`, `ChType::Int256`, and
`ChType::UInt256`.

**Wire payload:** `num_rows * width` bytes, contiguous, no per-row framing, where
`width` is 16 for the 128-bit pair and 32 for the 256-bit pair. Each row is one
fixed-width integer in straight little-endian byte order: two's-complement signed
for `Int128`/`Int256`, unsigned for `UInt128`/`UInt256`. Byte 0 is the
least-significant byte and the last byte is the most-significant (the sign byte
for the signed types). This is BYTE-IDENTICAL to how `Decimal128`/`Decimal256`
serialize their underlying integer, which the crate already treats as a raw
little-endian passthrough. The per-column bulk-state prefix reads zero bytes and
the custom-serialization marker is 0x00, same as a plain numeric. These are legal
`LowCardinality` inners (see the `LowCardinality(T)` section) and legal
`Nullable` inners.

**Arrow export:** Arrow FixedSizeBinary, `w:16` for `Int128`/`UInt128` and `w:32`
for `Int256`/`UInt256`. 2 buffers in order: validity, then the contiguous data
buffer, zero-copy. There is no offsets buffer; row `i` is
`data[i * width .. (i + 1) * width]`. This is NOT the Arrow decimal format
`d:P,0,bits`: Arrow caps `decimal128` at precision 38 and `decimal256` at 76,
neither of which can represent the full 128/256-bit range (`2^127 - 1` is 39
digits), and Arrow decimal is signed so an unsigned high-bit value would read as
negative. FixedSizeBinary is opaque bytes, universally supported, and a
zero-copy handoff of the decoded `Vec<u8>`; the `Arc<ColBatch>` keeps it alive.
No endianness handling happens in the export (the bytes are opaque). Signedness
and integer-vs-blob are NOT carried by the format string: `w:16` is shared by
`Int128`/`UInt128`/`UUID`/`IPv6` and `w:32` by `Int256`/`UInt256`. That matches
the existing contract; the binding disambiguates via the `ChType`/type-name
channel, exactly as it already separates `UUID` from `IPv6`.

**Rust buffer:** `Column::Int128`, `Column::UInt128`, `Column::Int256`, and
`Column::UInt256`, each a `FixedBinaryColumn`
(`{ data: Vec<u8>, width: usize, validity: Option<Bitmap> }`) with `width` 16 or
32. Four distinct `Column` variants back the shared physical shape, mirroring the
UUID/IPv6 precedent (both width-16 `FixedBinaryColumn`s under distinct variants).
`data` is the wire bytes verbatim.

**Notes:**

- **Decode is a host-agnostic raw passthrough.** The fixed-width bytes are stored
  unchanged, with no reinterpretation into a native integer, so the buffer stays
  correct on big-endian hosts and the core needs no native `i128`/`i256`. This is
  deliberately the `Decimal` path, NOT the `decode_primitive!` numeric path
  (which byte-swaps into a native `Vec<T>` on big-endian hosts); wide ints are
  never byte-swapped on decode.
- **Binding recovery.** Read the `width` bytes of a row as a LITTLE-ENDIAN integer
  of that width. Signedness comes from the ClickHouse type name (the `ChType`
  variant), not from the Arrow format string: `Int128`/`Int256` are
  two's-complement signed, `UInt128`/`UInt256` are unsigned. There is no scale,
  unlike `Decimal`. The host representation (a Python `int`, a JS `BigInt`, and so
  on) is a binding concern.
- **Sign / high bit.** A signed `-1` is all-`0xFF` bytes of the width. An unsigned
  value with the top bit set (for example `2^127` for `UInt128`) is a positive
  value with its most-significant byte's high bit set, never a negative.

**Introduction version:** undetermined at this pin (the local shallow
`.server-src` checkout's `CHANGELOG.md` only reaches 26.1 and the git history is
shallow); not guessed from memory. Stable at `v26.6.1.1193-stable`.

**Server reference:** `SerializationNumber<T>::serializeBinaryBulk` /
`deserializeBinaryBulk` in
`src/DataTypes/Serializations/SerializationNumber.cpp` (the same template as
`Int8`..`Int64`), over `DataTypeNumber<T>` in `src/DataTypes/DataTypesNumber.cpp`;
the underlying `wide::integer` stores `items[0]` as the least-significant 64-bit
limb, so the on-wire blob is canonical little-endian on little-endian server
builds. `canBeInsideLowCardinality()` is final-true on `DataTypeNumberBase`.
Confirmed at `v26.6.1.1193-stable`.

### Temporal types

This covers `Date`, `Date32`, `DateTime`, `DateTime64`, `Time`, and `Time64`.
All six are plain bulk integers on the wire, identical in layout to the matching
fixed-width primitive. Timezone and precision are type metadata only and have
zero effect on the wire bytes.

**Type string(s) and per-type details:**

| Type string                              | Logical type                                 | Wire element | Column variant | Bytes/row |
|------------------------------------------|----------------------------------------------|--------------|----------------|-----------|
| `Date`                                   | `ChType::Date`                               | `u16`        | `Date`         | 2         |
| `Date32`                                 | `ChType::Date32`                             | `i32`        | `Date32`       | 4         |
| `DateTime`, `DateTime('<tz>')`           | `ChType::DateTime { timezone }`              | `u32`        | `DateTime`     | 4         |
| `DateTime64(P)`, `DateTime64(P, '<tz>')` | `ChType::DateTime64 { precision, timezone }` | `i64`        | `DateTime64`   | 8         |
| `Time`                                   | `ChType::Time`                               | `i32`        | `Time`         | 4         |
| `Time64(P)`                              | `ChType::Time64 { precision }`               | `i64`        | `Time64`       | 8         |

`parse_ch_type` reads the optional timezone as the single-quoted contents of the
type string (`None` when absent), and the `DateTime64` precision `P` as the
integer in `DateTime64(P[, '<tz>')`. A precision outside `0..=9` is rejected as
`UnsupportedType`. `Time` has no parameter. `Time64(P)` carries a required
canonical precision `P` in `0..=9` and has no timezone. Although the server's
input parser accepts compatibility aliases such as bare `Time64` (defaulting to
precision 3), it always emits `Time64(P)` in a Native header, so
`parse_ch_type` accepts only that canonical wire spelling.

**Logical meaning of the integer:**

- `Date`: days since 1970-01-01 (Unix epoch), unsigned. Range
  1970-01-01 .. 2149-06-06.
- `Date32`: days since 1970-01-01, signed (negative is before the epoch). Wider
  range than `Date`.
- `DateTime`: seconds since the Unix epoch, unsigned. Range 1970 .. 2106.
- `DateTime64(P)`: ticks where one tick is `10^-P` seconds, signed (negative is
  before the epoch). For example `DateTime64(3)` ticks are milliseconds.
- `Time`: signed whole seconds with no date, epoch, or timezone. Text parsing and
  component extraction document `-999:59:59 .. 999:59:59`, but the Native bulk
  path performs no range validation and decodes any `i32` payload verbatim.
- `Time64(P)`: signed ticks where one tick is `10^-P` seconds, with precision
  `P` in `0..=9` and no date, epoch, or timezone. Its text/component behavior
  uses the same documented 999-hour range, but Native bulk decoding accepts any
  `i64` payload verbatim.

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
- `Time` -> `i` (Arrow int32, raw seconds).
- `Time64(P)` -> `l` (Arrow int64, raw ticks) for every precision.

The two Time types deliberately do not use Arrow Time32/Time64 formats. Arrow
Time values are restricted to one nonnegative 24-hour day, while ClickHouse
`Time`/`Time64` allow negative values and values beyond 24 hours. Advertising
the physical buffers as Arrow Time would therefore make valid ClickHouse values
invalid Arrow arrays. Keeping the raw integers is zero-copy and preserves every
wire value; `ChType::Time64` retains the tick precision.

**Rust buffer:** `Column::Date(PrimitiveColumn<u16>)`,
`Column::Date32(PrimitiveColumn<i32>)`,
`Column::DateTime(PrimitiveColumn<u32>)`, and
`Column::DateTime64(PrimitiveColumn<i64>)`,
`Column::Time(PrimitiveColumn<i32>)`, and
`Column::Time64(PrimitiveColumn<i64>)`. Each is `{ values, validity }` at the
faithful native width. The Time variants add only distinct logical tags, not a
new physical column concept. `values` has length `num_rows` and, on
little-endian targets, is the wire bytes verbatim.

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

`Time` and `Time64` always export their raw signed integer buffers because of
the Arrow Time domain mismatch described above. Their precision and type
identity remain available in `ChType`, and neither type carries timezone
metadata.

**Server reference:** `SerializationDate` (inherits `SerializationNumber<UInt16>`),
`SerializationDate32` (inherits `SerializationNumber<Int32>`),
`SerializationDateTime` (inherits `SerializationNumber<UInt32>`), and
`SerializationDateTime64` (inherits `SerializationDecimalBase<DateTime64>`, native
type `Int64`), plus `DataTypeTime`/`SerializationTime` and
`DataTypeTime64`/`SerializationTime64`, in `src/DataTypes/` and
`src/DataTypes/Serializations/`. `SerializationTime` inherits
`SerializationNumber<Int32>`; `SerializationTime64` inherits
`SerializationDecimalBase<Time64>`, whose native type is `Int64`. None override
the binary bulk path, so each is exactly its underlying integer: a single bulk
raw read on little-endian hosts, byte-swapped per element on big-endian hosts.
Confirmed at `v26.6.1.1193-stable` from
`src/DataTypes/DataTypeTime.{h,cpp}`,
`src/DataTypes/Serializations/SerializationDateTime.{h,cpp}` for
`SerializationTime`, `src/DataTypes/DataTypeTime64.{h,cpp}`,
`src/DataTypes/Serializations/SerializationTime64.{h,cpp}`, and
`src/DataTypes/Serializations/SerializationDecimalBase.cpp`.

### Interval types

**Type string(s):** the 11 exact, case-sensitive canonical names
`IntervalYear`, `IntervalQuarter`, `IntervalMonth`, `IntervalWeek`,
`IntervalDay`, `IntervalHour`, `IntervalMinute`, `IntervalSecond`,
`IntervalMillisecond`, `IntervalMicrosecond`, and `IntervalNanosecond`. There
are no parameters or aliases in a Native header.

**Logical type:** `ChType::Interval(IntervalKind)`, where `IntervalKind` has one
variant for each unit above. `Display` emits the matching canonical name, so
`parse(display(t)) == t` for every kind.

**Wire payload:** `num_rows * 8` bytes, one contiguous little-endian signed
`Int64` count per row, with no per-row framing and no in-band unit tag. The unit
is carried only by the type string. The per-column bulk-state prefix reads zero
bytes and the custom-serialization marker is 0x00. Decode and encode use the
same primitive bulk path as `Int64`.

**Arrow export:** 2 buffers in order, validity then values, zero-copy. The four
units whose physical `i64` count exactly matches an Arrow Duration unit export
as Duration: `IntervalSecond` -> `tDs`, `IntervalMillisecond` -> `tDm`,
`IntervalMicrosecond` -> `tDu`, and `IntervalNanosecond` -> `tDn`. The other
seven kinds export as `l` (Arrow int64, raw counts). Arrow's calendar interval
layouts are physically incompatible with ClickHouse's one-`Int64` count, and
Arrow has no exact duration unit for year, quarter, month, week, day, hour, or
minute. Converting those would require per-value work and a new buffer, so the
core preserves the raw i64 and the exact unit remains in `ChType`.

**Rust buffer:** `Column::Interval(PrimitiveColumn<i64>)`, `{ values, validity }`,
length `num_rows`. All 11 kinds share this physical column variant; the exact
unit lives in the schema's `ChType::Interval`, the same Column-vs-ChType split
used for `Time64` precision and `DateTime64` timezone.

**Notes:** all 11 kinds are legal inside `Nullable` and `LowCardinality` at the
pinned tag. A `LowCardinality(Interval*)` dictionary body is a plain contiguous
Interval i64 run and uses the ordinary dictionary/index framing. Bare Interval
and `LowCardinality(Interval*)` are legal Map keys; only the generic Map ban on
nullable keys applies. Persisted `LowCardinality(Interval*)` schema declarations
and explicit CAST targets require
`allow_suspicious_low_cardinality_types = 1`, but that validation gate does not
change the type's legality or wire bytes. It is the generic fixed-width numeric
LowCardinality guard, not an Interval-specific restriction. Server expressions
can still produce `LowCardinality(Interval*)` results without the setting, and
clients need no setting to decode them. Intervals are leaf types for
`MAX_TYPE_DEPTH`; wrappers and containers charge their ordinary levels, with no
Interval-specific depth.

**Introduction version:** undetermined from the shallow `.server-src` checkout;
not guessed. All 11 are registered and stable at `v26.6.1.1193-stable`.

**Server reference:** `DataTypeInterval::doGetName`,
`DataTypeInterval::doGetSerialization`, and `registerDataTypeInterval` in
`src/DataTypes/DataTypeInterval.{h,cpp}`; the unit names in
`src/Common/IntervalKind.{h,cpp}`; `SerializationInterval` in
`src/DataTypes/Serializations/SerializationInterval.h`; and the contiguous
little-endian bulk methods in
`src/DataTypes/Serializations/SerializationNumber.cpp`. Nullable legality was
confirmed through `src/DataTypes/DataTypeNullable.cpp`, LowCardinality legality
through `DataTypeNumberBase::canBeInsideLowCardinality` and
`src/DataTypes/DataTypeLowCardinality.cpp`, and Map-key legality through
`src/DataTypes/DataTypeMap.cpp`. All claims in this section are confirmed at
`v26.6.1.1193-stable`; none are inferred.

### Nullable(T)

**Type string(s):** `Nullable(T)` where `T` is any supported non-wrapper type
above, for example `Nullable(Int64)` or `Nullable(String)`, plus the one legal
container nesting `Nullable(Tuple(...))` (see the `Tuple` section).

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
  type must be a concrete supported type, never `Nullable`, `LowCardinality`,
  or `Array`. `Tuple` is the one container ClickHouse permits inside `Nullable`
  (`DataTypeTuple::canBeInsideNullable()` is true); it keeps the ordinary
  framing here, with the tuple body as the inner payload.
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
`UInt8`/`UInt16`/`UInt32`/`UInt64`, `Float32`/`Float64`, `BFloat16`), the wide
integers
(`Int128`/`UInt128`/`Int256`/`UInt256`), `Bool`, the number-backed temporals
`Date`, `Date32`, `DateTime`, `Time`, every `Interval*`, and
`UUID`/`IPv4`/`IPv6`. The
dictionary values are that inner type serialized as a plain column body:
varint-length strings for `String`, raw fixed-width bytes otherwise (4 bytes per
`IPv4` entry, 2 bytes per `BFloat16` entry, 16 bytes per
`UUID`/`IPv6`/`Int128`/`UInt128` entry, 32 bytes per
`Int256`/`UInt256` entry). Support follows directly from the per-type body
decoder.

This allowlist is exactly `IDataType::canBeInsideLowCardinality()` intersected
with the types this crate decodes, confirmed against the server source at
`v26.6.1.1193-stable` (the `DataTypeLowCardinality` constructor checks it after
`removeNullable`). Three consequences worth calling out:

- `DateTime64`, `Time64`, and every `Decimal` are **not** allowed: they are
  `DataTypeDecimalBase` subclasses whose `canBeInsideLowCardinality()` is false,
  so the server never emits `LowCardinality(DateTime64(...))` or
  `LowCardinality(Time64(...))`. The crate decodes both as ordinary columns but
  rejects them as `LowCardinality` inners. `Time`, by contrast, inherits the
  number-backed true capability and is allowed.
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
- The wide integers (`Int128`/`UInt128`/`Int256`/`UInt256`) **are** permitted,
  unlike `Decimal`/`Enum`: they are `DataTypeNumberBase` subclasses whose
  `canBeInsideLowCardinality()` is final-true (the server ships tests
  `02125_low_cardinality_int256` and `02459_low_cardinality_uint128_aggregator`).
  A `LowCardinality` over them decodes through the dictionary path with a
  width-16/32 `FixedBinary`-backed dictionary value column
  (`Int128`/`UInt128`/`Int256`/`UInt256`).

The fixed-width numeric, temporal, and Interval inners, and `IPv4`/`IPv6`,
require the server setting `allow_suspicious_low_cardinality_types=1` in
persisted schema declarations and explicit `CAST` targets. That is a server-side
type-use guard only, shared by fixed-width numeric types rather than specific to
Interval. It does not prevent server expressions from returning these
LowCardinality types without the setting, has no effect on the wire bytes, and
is not needed to decode a column the server already produced. `String`,
`FixedString`, and `UUID` are allowed unconditionally.

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

A `SimpleAggregateFunction` name decoration may wrap the inner, including
BETWEEN the `LowCardinality` and its `Nullable`: `LowCardinality(SAF(anyLast,
Nullable(String)))` is a real server header (live-confirmed at
`v26.6.1.1193-stable`), and chained SAF is legal. All `LowCardinality` sites
resolve `(nullable, dict_value_type)` through one shared helper,
`low_cardinality_dict_value_type` in `src/native/type_parser.rs`, which strips the
full SAF chain, unwraps the optional `Nullable`, then strips any further SAF
chain, so the aliased forms decode, encode, and export exactly as the
equivalent plain `LowCardinality(Nullable(T))`. See the
`SimpleAggregateFunction(func, T)` section.

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

### Array(T)

**Type string(s):** `Array(T)` for any element type `T` this crate supports. The
element may itself be `Nullable(T)`, `LowCardinality(T)`, or a further `Array(T)`,
so containers nest. The array is never wrapped in an outer `Nullable`:
`Nullable(Array(T))` is not constructible on the server
(`DataTypeArray::canBeInsideNullable()` is false), and the parser rejects it. The
parser caps wrapper/container nesting depth (`MAX_TYPE_DEPTH = 100`) so a hostile
`Array(Array(...))` header returns `UnsupportedType` instead of overflowing the
stack; that cap bounds decode, the completeness scan, and the Arrow export too.

**Logical type:** `ChType::Array(Box<ChType>)`.

**Introduction version:** `Array` is a foundational ClickHouse type, stable at the
pinned tag `v26.6.1.1193-stable`.

**Wire payload:** per Array column per block, only when the block has rows
(`NativeReader::readData` gates on `if (rows)`), the bytes are, in order:

```text
[state prefix]      // SerializationArray writes NOTHING of its own; it recurses
                    // into the element type's deserializeBinaryBulkStatePrefix.
                    // Zero bytes for String / Nullable / plain Array elements. For
                    // Array(LowCardinality(T)) the LC 8-byte key version is emitted
                    // HERE, at the very front, BEFORE the offsets.
[num_rows * 8]      offsets   // raw LE u64, cumulative ABSOLUTE end-offsets (the
                    //   element index one past this row's last element). NO leading
                    //   zero, no count, no per-row framing. Monotonically
                    //   non-decreasing (equal adjacent = an empty array row); a
                    //   decrease is INCORRECT_DATA on the server and rejected here.
[element body]      // the flattened element column of length total_elements (= the
                    //   last offset value), the element type's normal bulk body
                    //   WITHOUT its state prefix (already consumed above).
```

For `Array(Nullable(T))` the element body is the per-element null map
(`total_elements` bytes, one per element) followed by the `total_elements` element
values, so element-level nulls live on the element column, never on the array. For
`Array(Array(T))` the element body is the inner array's offsets (`total_elements`
values) then the leaf body, recursively. For `Array(LowCardinality(T))` the element
body is the LowCardinality index word / dictionary / row count / index array (its
key version was consumed by the state prefix at the front).

A zero-row block reads no state prefix, no offsets, and no element body.

When the block has rows but every array is empty (the flattened element run has
zero length), the element body is entirely absent for a `LowCardinality`
element: `SerializationLowCardinality::serializeBinaryBulkWithMultipleStreams`
early-returns whenever `limit == 0`, writing neither the index word nor the
dictionary, row count, or indexes, so the on-wire column is exactly
`[LC key version][num_rows zero offsets]` and nothing else (verified against a
live 26.6.1.1193 capture of `SELECT []::Array(LowCardinality(String))`). Other
element types degrade naturally to zero body bytes at count 0 (an empty null map
plus no values, an empty offsets run, and so on).

**Arrow export:** Arrow `large_list(item: T)`, format string `+L`. The array
carries 2 buffers in order: validity (always null here, arrays are never nullable
at the array level) then the i64 offset buffer. It has one child, the flattened
element array, exported recursively as a column of `T` (so a `Nullable`,
`LowCardinality`, or nested `Array` element composes through the child). 64-bit
(LargeList) offsets are used, not 32-bit, because ClickHouse offsets are `UInt64`
element counts and a per-block element count can legitimately exceed `i32::MAX`.

**Rust buffer:** `Column::Array(ArrayColumn)` where `ArrayColumn` is
`{ offsets: Vec<i64>, values: Box<Column> }`. `offsets` has length `num_rows + 1`
and carries Arrow's leading `0` (so row `i`'s elements are
`values[offsets[i]..offsets[i + 1]]`), widened from the wire `UInt64` run. An
empty array row is equal adjacent offsets. `values` is the flattened element
column of length `offsets[num_rows]`, its own `Column` (recursively any supported
element type). There is no array-level validity bitmap; a nullable element type
keeps its nulls on `values`' own validity.

**Notes:**

- The element data is local to each chunk, like every other column; blocks stay
  separate chunks and are never concatenated.
- An absolute offset above `i64::MAX`, or offsets that decrease, fail with
  `DecodeError::InvalidArray` (not `UnsupportedType`), on both the allocating
  decode and the streaming completeness scan, so `StreamDecoder` surfaces the same
  error rather than stalling.

**Server reference:** `SerializationArray::deserializeBinaryBulkStatePrefix` (the
recurse-into-element prefix) and
`SerializationArray::deserializeBinaryBulkWithMultipleStreams` (the offset run then
the flattened element body) in
`src/DataTypes/Serializations/SerializationArray.cpp`, with the `if (rows)` gate in
`NativeReader::readData` (`src/Formats/NativeReader.cpp`) and the
non-decreasing-offsets enforcement (`INCORRECT_DATA`) in the same serialization
file. `DataTypeArray::canBeInsideNullable()` (false) is in
`src/DataTypes/DataTypeArray.cpp`. Confirmed at `v26.6.1.1193-stable`.

### Tuple(T1, ...)

**Type string(s):** `Tuple(T1, T2, ...)` for unnamed elements and
`Tuple(name1 T1, name2 T2, ...)` when explicit names exist
(`DataTypeTuple::doGetName` renders names all-or-nothing; the parser stores them
per element so any received header is preserved exactly). Element types are any
supported types, including `Nullable(T)`, `LowCardinality(T)`, `Array(T)`, and a
nested `Tuple`. The zero-element `Tuple()` is constructible and emittable.
`Nullable(Tuple(...))` is legal (`DataTypeTuple::canBeInsideNullable()` is true;
the `enable_nullable_tuple_type` setting is a DDL creation gate with no wire
effect). `LowCardinality(Tuple(...))` is illegal (`canBeInsideLowCardinality()`
is false) and rejected at header time.

Element names are rendered with the server's `backQuoteIfNeed`: a name stays
bare only if it is a valid ASCII identifier (`[A-Za-z_][A-Za-z0-9_]*`), not
any-case `null` (excluded by `isValidIdentifier` itself, since a bare NULL
would read back as the NULL keyword), and not one of the case-insensitive
keywords `distinct`/`all`/`table`/`select`/`from`/`values`; otherwise it is
backtick-quoted, with a backtick inside escaping as `` \` `` and a backslash
as `\\` (`writeBackQuotedString` -> `writeAnyEscapedString<'`'>`, plus the
two-char C0 letter escapes). The parser accepts a superset on read, mirroring
the server's own reader (`Lexer.cpp`, `readBackQuotedStringWithSQLStyle`,
`parseEscapeSequence`): the doubled `` `` `` backtick escape, `\xAA` hex
bytes, `\N` (empty), the control escapes `\a \b \e \f \n \r \t \v \0`, and any
other `\c` as the literal `c`. `Display` re-renders in the canonical server
form, so `parse(display(x)) == x` always holds. Names can contain spaces and
commas (they just force quoting). The server rejects empty names, the
exact-lowercase reserved name `null`, duplicates, and mixed named/unnamed
elements at creation (`checkTupleNames` and the type factory), so they never
appear in an honest header; the DECODE parser accepts them as received rather
than second-guessing, while ENCODE rejects them pre-write (see the encoding
preconditions), since a rendered header carrying them is one the server cannot
parse back.

**Logical type:** `ChType::Tuple(Vec<(Option<String>, ChType)>)`.

**Introduction version:** undetermined from the shallow checkout at the pinned
tag; `Tuple` is a foundational ClickHouse type, stable at `v26.6.1.1193-stable`.

**Wire payload:** per Tuple column per block, only when the block has rows
(`NativeReader::readData` gates on `if (rows)`), the bytes are, in order:

```text
[state prefix]      // SerializationTuple writes NOTHING of its own; its
                    // (de)serializeBinaryBulkStatePrefix loops over the
                    // elements in declaration order and delegates. So for
                    // Tuple(LowCardinality(String), Int32) the LC 8-byte key
                    // version sits HERE, at the very front of the whole
                    // column, before ANY element body; the Int32 contributes
                    // nothing. SerializationNullable delegates its prefix the
                    // same way, so Nullable(Tuple(...)) hoists identically.
[element 0 body]    // element 0's FULL run of num_rows rows: the element
[element 1 body]    // type's normal bulk body WITHOUT its state prefix, then
...                 // element 1's full run, and so on, in declaration order.
                    // Column-of-columns: no interleaving, no offsets, and no
                    // Tuple-level length framing.
```

The server asserts all element columns come out the same size (`INCORRECT_DATA`
in Native mode); the decoder mirrors the check, though its element decodes are
all driven by the block row count so it cannot fire in practice.

The zero-element `Tuple()` has a special layout: exactly ONE literal ASCII '0'
byte (0x30) per row and no other bytes. The reader ignores the byte values
(`tryIgnore`), so the decoder skips `num_rows` bytes without validating them;
truncation is still `UnexpectedEof`. Encode writes the canonical 0x30 per row.

A zero-row block reads no state prefix, no element bodies, and no `Tuple()`
placeholder bytes. A zero-length run with rows in the block (e.g.
`Array(Tuple(...))` whose arrays are all empty) passes `limit == 0` down to
every element, so each element's own zero-limit behavior applies: in
particular, a `LowCardinality` element's body is entirely absent (the server's
`limit == 0` early return), and a `Tuple()` in that position writes nothing.

For `Nullable(Tuple(...))` the ordinary Nullable framing applies: the per-row
null map first, then the tuple body as above. Null rows still carry
placeholder (default) element values in every element body.

**Arrow export:** Arrow struct, format string `+s`. The element types are NOT
in the format string; each element is a child `ArrowSchema`/`ArrowArray`
described recursively, so `Nullable`, `LowCardinality`, `Array`, and nested
`Tuple` elements compose exactly like the `Array` `item` child does. Child
names are the ClickHouse element names verbatim for a named tuple (flowing
through the same lossy-NUL C-string path as every wire-origin name) and the
1-based decimal strings "1", "2", ... for unnamed elements. The struct node
carries 1 buffer, the validity slot: null with `null_count` 0 for a plain
Tuple, and the tuple-level validity bitmap for a `Nullable(Tuple(...))`
(whose schema also sets the nullable flag). Struct validity is independent of
the children per the C Data spec; consumers AND them. `Tuple()` exports as
`+s` with `n_children == 0`.

**Rust buffer:** `Column::Tuple(TupleColumn)` where `TupleColumn` is
`{ fields: Vec<Column>, len: usize, validity: Option<Bitmap> }`. `fields` holds
one child column per element in declaration order, each of length `len`; the
element names live in the schema's `ChType::Tuple`, not on the column. `len` is
explicit so the zero-field `Tuple()` cannot desync from the row count.
`validity` is `Some` only for `Nullable(Tuple(...))`.

**Notes:**

- Ragged element columns cannot be produced by decode (every element run is
  driven by the same row count); the defensive mirror of the server's check is
  `DecodeError::InvalidTuple`. On the encode side a ragged `TupleColumn` or a
  field-count mismatch against the declared type is
  `EncodeError::InconsistentBatch`, rejected before any bytes are written.
- `Map(K, V)` is wire-serialized as `Array(Tuple(key, value))` and builds on
  this element-decode path; see its own type section below.

**Server reference:** `SerializationTuple` (the delegate-per-element state
prefix and the sequential element bodies; the equal-sizes `INCORRECT_DATA`
check) in `src/DataTypes/Serializations/SerializationTuple.cpp`, with
`DataTypeTuple::doGetName` and `canBeInsideNullable()` in
`src/DataTypes/DataTypeTuple.cpp`, name quoting in
`src/Common/quoteString.cpp` (`backQuoteIfNeed` ->
`writeProbablyBackQuotedString`) and `src/IO/WriteHelpers.h`
(`writeAnyEscapedString`), and the permissive read side in
`src/Parsers/Lexer.cpp` and `src/IO/ReadHelpers.cpp`
(`readBackQuotedStringWithSQLStyle` / `parseEscapeSequence`).
`SerializationNullable`'s prefix delegation is in
`src/DataTypes/Serializations/SerializationNullable.cpp`. Confirmed at
`v26.6.1.1193-stable`.

### Map(K, V)

**Type string(s):** `Map(K, V)`, always exactly the two type arguments
(`DataTypeMap::doGetName`); the nested tuple's "keys"/"values" names never
appear in the type string or as wire bytes (`SerializationNamed` is a pure
forwarder). The key type must satisfy `DataTypeMap::isValidKeyType`
(`!isNullableOrLowCardinalityNullable`): `Nullable(K)` and
`LowCardinality(Nullable(K))` keys are forbidden and rejected at header time
(`UnsupportedType`); a plain `LowCardinality(K)` key is legal. The value type
is unrestricted among supported types, including `Nullable(V)`,
`LowCardinality(V)`, `Array(V)`, `Tuple(...)`, and a nested `Map`. The map
itself is never inside `Nullable` (`canBeInsideNullable()` is false; the
parser rejects `Nullable(Map(...))` like `Nullable(Array(...))`) or
`LowCardinality`; `Map` inside `Array` and inside `Tuple` composes.

**Logical type:** `ChType::Map(Box<ChType>, Box<ChType>)`.

**Introduction version:** undetermined from the shallow checkout at the pinned
tag; stable at `v26.6.1.1193-stable`.

**Wire payload:** on the Native wire a Map is ALWAYS the plain
`Array(Tuple(keys, values))` layout. The server's newer bucketed `WITH_BUCKETS`
on-disk serialization never reaches the Native wire in either direction:
`NativeReader` builds serializations via `enableAllSupportedSerializations`,
which leaves `map_serialization_version` at `BASIC`, and `NativeWriter` goes
through `IDataType::getSerializationInfo`'s default, also `BASIC`. Per Map
column per block, only when the block has rows, the bytes are, in order:

```text
[state prefix]      // SerializationMap writes NOTHING of its own; the chain is
                    // Map -> Array (nothing) -> Tuple -> K's prefix then V's
                    // prefix, in that order. So Map(LowCardinality(String), V)
                    // has the LC 8-byte key version HERE, at the very front of
                    // the column, before the offsets.
[num_rows * 8]      offsets   // raw LE u64, cumulative ABSOLUTE end-offsets in
                    //   ENTRIES, no leading zero, monotonically non-decreasing;
                    //   identical framing to the Array offsets and validated by
                    //   the same shared walk (a decrease is INCORRECT_DATA).
[key run]           // the flattened keys column of length total_entries (= the
                    //   last offset), K's normal bulk body WITHOUT its prefix.
[value run]         // the flattened values column of length total_entries, V's
                    //   normal bulk body WITHOUT its prefix.
```

A zero-row block reads no state prefix, no offsets, and no runs. When the
block has rows but every map is empty, the offsets are still written (all
zeros) and the nested tuple gets `limit == 0`, so each run takes its own
zero-limit behavior: a `LowCardinality` key or value writes NO body at all
(the `limit == 0` early return), everything else degrades to zero bytes.
There are no protocol-revision branches.

**Arrow export:** LargeList-of-struct, NOT the Arrow map type: there is no
large-map format string and `+m` mandates i32 offsets, which would force a
per-offset copy of the i64 buffer (the same reasoning as the Array LargeList
note). The field format is `+L` with flags 0 (`Nullable(Map)` is impossible)
and one child named `entries`: a non-nullable `+s` struct (null validity,
`null_count` 0) whose children are named `key` (flags 0; naturally
non-nullable given the key constraint) and `value` (nullable flag per the
value type). The map array node is byte-identical in shape to the Array
export: 2 buffers (always-null validity, then the verbatim i64 offsets) and
the recursively exported entries child. `ARROW_FLAG_MAP_KEYS_SORTED` is never
set (it is `+m`-only). This naming makes the export shape-isomorphic to Arrow
Map minus the offset width, so bindings can cast cheaply.

**Rust buffer:** `Column::Map(MapColumn)` where `MapColumn` is
`{ offsets: Vec<i64>, entries: Box<Column> }`. `offsets` has length
`num_rows + 1` with Arrow's leading `0` (row `i`'s entries are
`entries[offsets[i]..offsets[i + 1]]`). `entries` is always a two-field
`Column::Tuple` holding the keys column then the values column, each of length
`offsets[num_rows]`. There is no map-level validity bitmap.

**Notes:**

- A malformed offsets run fails with `DecodeError::InvalidArray` (the shared
  offsets walk), on both the allocating decode and the streaming scan.
- On the encode side an illegal key type is `EncodeError::UnsupportedType`;
  ragged keys/values runs, an entries buffer that is not a two-field tuple, or
  offsets that disagree with the entries length are
  `EncodeError::InconsistentBatch`, all rejected before any bytes are written.

**Server reference:** `SerializationMap` (the delegate-through-nested prefix
and the `Array(Tuple(keys, values))` bulk body) in
`src/DataTypes/Serializations/SerializationMap.cpp`, with
`DataTypeMap::doGetName`, `isValidKeyType`, and `canBeInsideNullable()` in
`src/DataTypes/DataTypeMap.cpp`, and the BASIC-only Native wire mode via
`enableAllSupportedSerializations` in `src/Formats/NativeReader.cpp` and
`IDataType::getSerializationInfo`'s default on the write side. Confirmed at
`v26.6.1.1193-stable`.

---

### SimpleAggregateFunction(func, T)

**Type string(s):** `SimpleAggregateFunction(func, T)` where `func` is an
aggregate function name and `T` is any supported inner type. The function name
may carry parenthesized literal parameters, so parametrized spellings such as
`SimpleAggregateFunction(groupArrayLastArray(5), Array(UInt64))` appear VERBATIM
in the Native header. Registration is case-sensitive with no aliases.
`parse_ch_type` accepts the spelling at ANY nesting position, because the server
emits it verbatim inside wrappers and containers: confirmed live at
`v26.6.1.1193-stable` by CREATE plus a Native header hexdump for
`Nullable(SimpleAggregateFunction(...))`,
`LowCardinality(SimpleAggregateFunction(...))`,
`Tuple(v SimpleAggregateFunction(...))`, `Array(SimpleAggregateFunction(...))`,
and `Map(String, SimpleAggregateFunction(...))`, and corroborated by the server
test `04329_tuple_element_aggregation_reject_nullable_tuple.sql`.

**Logical type:**
`ChType::SimpleAggregateFunction { func: String, inner: Box<ChType> }`.

**Wire payload:** byte-identical to the inner `T`. `SimpleAggregateFunction` is
pure name decoration (`DataTypeCustomSimpleAggregateFunction` attaches only a
custom name and leaves the serialization slot null), so its state prefix
(including a `LowCardinality` key version when `T` is `LowCardinality`), body
bytes, and per-column custom-serialization marker are exactly `T`'s. There is no
SAF-specific framing.

**Arrow export:** exactly the inner `T`'s export, the same format string and
buffers. There is no new Column variant: the decoded column IS the inner type's
column, reached through `ChType::physical_delegate`.

**Rust buffer:** the inner `T`'s `Column` variant, with no wrapper. `func` and
the inner type live only in the schema's `ChType`, the same Column-vs-ChType
split the temporals and `Decimal` use for their metadata.

**Notes:**

- Legal at any nesting position and to any chain depth; wrapper legality
  delegates to `T`. `Nullable(SAF(T))` is legal iff `Nullable(T)` is, and
  `is_low_cardinality_inner` / `is_valid_map_key_type` see through the alias to
  the physical delegate, so a `LowCardinality(SAF(...))` or a
  `Map(SAF(...), V)` key resolves as if the SAF were its inner.
- Inside `LowCardinality`, the alias may sit BETWEEN the `LowCardinality` and its
  removeNullable `Nullable`:
  `LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))` is a real
  server header (live-confirmed at `v26.6.1.1193-stable`, fixture `lc_nsaf`).
  Every `LowCardinality` site (decode, the completeness scan, the zero-row empty
  column, header validation, encode validate/write, and the Arrow schema export)
  resolves its inner through one shared helper, `low_cardinality_dict_value_type`
  in `src/native/type_parser.rs`, which strips the full SAF chain, unwraps the optional
  `Nullable`, then strips any further SAF chain beneath it, returning
  `(nullable, dict_value_type)`. This is not a single-level see-through: chained
  SAF (`SimpleAggregateFunction(anyLast, SimpleAggregateFunction(sum, UInt64))`,
  and the same chain as a `LowCardinality` inner) resolves to its physical value
  type the same way.
- Decode is lenient on the function name. The server whitelists a fixed set (21
  functions at this pin: `any`, `any_respect_nulls`, `anyLast`,
  `anyLast_respect_nulls`, `min`, `max`, `sum`, `sumWithOverflow`, `groupBitAnd`,
  `groupBitOr`, `groupBitXor`, `sumMap`, `minMap`, `maxMap`, `groupArrayArray`,
  `groupArrayLastArray`, `groupUniqArrayArray`, `groupUniqArrayArrayMap`,
  `sumMappedArrays`, `minMappedArrays`, `maxMappedArrays`), but the decoder does
  NOT enforce it: a server-authored header is trusted and the list grows across
  versions.
- Multi-type-arg forms `SimpleAggregateFunction(f, T1, T2)` are rejected as
  `UnsupportedType`. They are grammar-parseable server-side, but only `T1` is
  physically load-bearing and the shape is unobserved in practice.
- Depth accounting: each SAF level charges +1 on both the parse and encode sides,
  so a hostile chain of nested SAFs stays bounded by `MAX_TYPE_DEPTH` and
  decode-accept implies encode-accept at the cap.

**Introduction version:** undetermined from the shallow `.server-src` checkout
(its `CHANGELOG.md` only reaches 26.1). Stable at `v26.6.1.1193-stable`.

**Server reference:** `DataTypeCustomSimpleAggregateFunction` in
`src/DataTypes/DataTypeCustomSimpleAggregateFunction.{h,cpp}` (the custom-name
attach with a null serialization slot). The wire layout, state prefix, and Arrow
export are the inner type's; see that type's section. Confirmed at
`v26.6.1.1193-stable`.

### AggregateFunction state codecs: count, nothingUInt64, nothingNull, and sum

**Type string(s):** `AggregateFunction(count)` and
`AggregateFunction(count, T)`, where `T` is one server-constructible type this
crate can parse, plus the canonical spellings
`AggregateFunction(nothingUInt64, Nullable(Nothing))` and
`AggregateFunction(nothingNull, Nullable(Nothing))`, and
`AggregateFunction(sum, T)` for exactly one plain or Nullable numeric or Enum
`T`. The supported sum value set is Bool, Int8..Int256, UInt8..UInt256,
BFloat16, Float32, Float64, Enum8, Enum16, and Decimal(P, S), each either bare or
wrapped once in `Nullable`. Grammatically valid but illegal shapes, such as a
forbidden `LowCardinality` inner or a nullable `Map` key, are rejected
recursively. The general server grammar is
`AggregateFunction([version, ]function[(parameters...)] [, argument types...])`,
but each function owns a different state serializer. These codecs accept only the
exact base function names `count` (with zero or one argument, but see the
canonicalization note below), `nothingUInt64`, and `nothingNull` (each with
exactly the single `Nullable(Nothing)` argument), plus parameterless unary `sum`
over the argument set above. `countDistinct`, `sumWithOverflow`, and other
functions or combinators are not aliases for these layouts.

**count(Nullable(Nothing)) canonicalization:** the `count, Nullable(Nothing)`
spelling never appears on the wire, so it is rejected on both paths. Parsing the
type string `AggregateFunction(count, Nullable(Nothing))` resolves through
`AggregateFunctionFactory::get` ->
`AggregateFunctionCombinatorNull::transformAggregateFunction`: because
`AggregateFunctionCount` registers `returns_default_when_only_null = true` and
`Nullable(Nothing).onlyNull()` is true, the server substitutes
`AggregateFunctionNothingUInt64`, and `DataTypeAggregateFunction::getNameImpl`
renders the canonical `AggregateFunction(nothingUInt64, Nullable(Nothing))`.
`NativeWriter` writes that canonical name. Admitting the VarUInt `count` state
under the `count, Nullable(Nothing)` header would be a wire-format mismatch,
because a server that parses the name resolves the one-zero-byte `nothingUInt64`
codec, so decode, parse, and encode all reject that spelling. Every other `count`
spelling (`count()`, `count(T)`, and `count(Nullable(T))` for `T != Nothing`,
served by `AggregateFunctionCountNotNullUnary` via
`AggregateFunctionCount::getOwnNullAdapter`) keeps the name `count` and the single
VarUInt64 state, so those remain the `count` codec unchanged. `count` is strictly
unary; the server throws for more than one argument.

The `nothingUInt64` codec is gated on the confirmed `Nullable(Nothing)` argument
shape only, not any argument list: that name is synthesized solely by the `count`
collapse at this tag, and only for `Nullable(Nothing)`, so it is the only spelling
the server emits. Accepting other argument lists would fabricate a header the
server never writes and could not parse back to `nothingUInt64`.

**sum(Nullable(Nothing)) canonicalization:** the `sum, Nullable(Nothing)`
spelling is accepted by the server but is not canonical, so decode and encode
reject it. `AggregateFunctionFactory::get` and the Null combinator replace the
embedded sum implementation with `AggregateFunctionNothingNull`, while
`DataTypeAggregateFunction::getNameImpl` retains the argument metadata and
renders `AggregateFunction(nothingNull, Nullable(Nothing))`. That canonical
header is the exact signature this crate registers. The server's public
`nothingNull` function can be constructed with other argument lists, but those
headers remain outside this exact tracker item and are still unsupported.

A direct `sumState(NULL)` expression is not a state constructor for this header.
The outer internal Null combinator replaces the complete State wrapper and the
SQL expression returns finalized `Nullable(Nothing)`. Real Native fixtures and
live INSERT tests therefore construct the exact state with
`CAST(unhex('00'), 'AggregateFunction(nothingNull, Nullable(Nothing))')`.

No state version is parsed. The server omits version 0 from canonical type names
and emits no other version at the pin, so the first token is always the function
name. A versioned spelling like `AggregateFunction(2, sum, UInt64)` treats `2` as
an unknown function name and parse-rejects cleanly (the full type string appears
in the `UnsupportedType` error); the explicit `AggregateFunction(0, count)`
spelling, which the server never emits, is likewise rejected rather than
accepted. Versioning is reintroduced with the first confirmed versioned codec,
and may key off the negotiated protocol revision rather than the type string.

**Logical type:**
`ChType::AggregateFunction { function, arguments }`, with `function` being
`"count"`, `"nothingUInt64"`, `"nothingNull"`, or `"sum"`. The function spelling
(with any literal parameters) and all argument types are retained so later
function-specific codecs can preserve them.

**Wire payload:**

- `count`: one unsigned VarUInt64 count state per row. Each value uses base-128
  low 7-bit groups first, with bit `0x80` marking continuation, so one state
  occupies 1 through 10 bytes. `AggregateFunction(count, Nullable(T))` (for
  `T != Nothing`) uses the same VarUInt state.
- `nothingUInt64` and `nothingNull`: exactly one `0x00` byte per row
  (`AggregateFunctionNothingImpl::serialize` writes one `'\0'`). Decode rejects
  any nonzero placeholder byte as `DecodeError::Io` of kind `InvalidData`,
  mirroring the server's `INCORRECT_DATA` throw in `deserialize`. The
  `nothingNull` name is aggregate function semantics only: the state row is not
  nullable and carries no null mask.
- `sum`: one fixed-width little-endian accumulator per row. Bool and
  UInt8..UInt64 promote to UInt64; Int8..Int64 and Enum8/Enum16 promote to
  Int64; BFloat16/Float32/Float64 promote to Float64. Those states are 8 bytes.
  UInt128/Int128 remain 16 bytes, UInt256/Int256 remain 32 bytes.
  Decimal32/64/128 promote to a 16-byte Decimal128 scaled integer, while
  Decimal256 remains a 32-byte Decimal256 scaled integer. Deserialize performs
  no semantic value validation: any complete accumulator bit pattern, including
  a Float64 NaN payload, is valid.
- `sum` with `Nullable(T)`: one flag byte first. Canonical serialization writes
  `0x00` when no non-NULL input was accumulated and stops the row state there.
  It writes `0x01` when a value is present, followed immediately by the ordinary
  8-, 16-, or 32-byte sum accumulator selected above. The server deserializer's
  `readBinary(bool)` accepts any nonzero flag as present, so decode and encode
  validation do too and preserve the exact flag byte. A nonzero flag without a
  complete accumulator is truncation. The flag is aggregate-state data, not an
  outer ClickHouse null map or Arrow validity bitmap.

In all cases states are adjacent with no row delimiter, per-state length,
aggregate-column length, prefix, suffix, or in-body version word. The argument
type is metadata and adds no nested body.

**Arrow export:** LargeBinary format `Z`, with a null validity buffer, i64
offsets, and the exact serialized state bytes. LargeBinary is used instead of
Utf8 or Binary because states are arbitrary bytes and the aggregate data run has
no 2 GiB contract. It is exported zero-copy from the decoded buffers. The
`nothingUInt64`, `nothingNull`, and `sum` states share this shape, so the FFI
export needs no type-specific handling.

**Rust buffer:**
`Column::AggregateState(AggregateStateColumn { offsets: Vec<i64>, data: Vec<u8> })`.
Decode walks one function-specific state per row, records each end offset, then
copies the complete contiguous wire run once. For both canonical `nothing*`
functions the state is fixed-width, so the walk bounds-checks the
`num_rows`-byte run once, rejects any nonzero byte, and fills offsets `i -> i`.
There is one offsets allocation and one data allocation per column, with no
per-row allocation. Fixed-width sum decode bounds-checks
`num_rows * state_width` once and computes each offset
arithmetically. Nullable sum walks the borrowed input once, branches on each
flag, records the variable row ends, then makes the same single contiguous data
copy. Encode validates every offset and row slice with the same codec: each
`count` slice is exactly one VarUInt64, each fixed-zero `nothing*` slice is
exactly `[0x00]`, each plain sum slice has the selected accumulator width, and each
nullable sum slice is either `[0x00]` or one nonzero byte plus exactly that
accumulator width. It then writes `data` in one copy.

**Wrappers and containers:** an outer `Nullable(AggregateFunction(...))` and
`LowCardinality(AggregateFunction(...))` are illegal and rejected.
`Array(AggregateFunction(...))`, Tuple elements, and Map values compose
through the ordinary container paths. A zero-row column has offsets `[0]` and
empty data, and Native writes no state body.

**Scope:** other aggregate functions remain `UnsupportedType`, including on a
zero-row block. This is required for safe framing: Native provides no generic
way to locate the next state or next column. New functions must register a
boundary codec shared by materializing decode, `block_end`, and encode
validation. Every unregistered aggregate function signature remains deferred
until its exact boundary codec is added.

**Introduction version:** the `count` function documentation records ClickHouse
1.1, but the original introduction release of the `AggregateFunction` data type
is undetermined from the shallow source checkout. The distinct `nothingNull`
naming is **INFERRED** to have entered the stable series in
`v24.1.1.2048-stable`: that release changelog records the result-type fix that
introduced separate `nothing*` names, but the shallow checkout cannot establish
the exact first commit or whether the unnamed implementation predates it. At the
pinned tag the state is **CONFIRMED** unversioned at version 0.

**Server reference:** `SerializationAggregateFunction::serializeBinaryBulk` and
`deserializeBinaryBulk` in
`src/DataTypes/Serializations/SerializationAggregateFunction.cpp`,
`IAggregateFunction::serializeBatch` and `createAndDeserializeBatch` in
`src/AggregateFunctions/IAggregateFunction.h`, the count state in
`src/AggregateFunctions/AggregateFunctionCount.{h,cpp}`, both fixed-zero states
and their `INCORRECT_DATA` deserialize guard in
`AggregateFunctionNothingImpl::serialize`/`deserialize`
(`src/AggregateFunctions/AggregateFunctionNothing.h`), aggregate resolution in
`AggregateFunctionFactory::get`/`getImpl`
(`src/AggregateFunctions/AggregateFunctionFactory.cpp`), the count and sum
collapses in
`AggregateFunctionCombinatorNull::transformAggregateFunction`
(`src/AggregateFunctions/Combinators/AggregateFunctionNull.cpp`), the canonical
name in `DataTypeAggregateFunction::create`/`getNameImpl`, and the direct
`sumState(NULL)` replacement in
`AggregateFunctionCombinatorState::transformAggregateFunction`
(`src/AggregateFunctions/Combinators/AggregateFunctionState.cpp`). VarUInt
encoding is in
`src/IO/VarInt.h`, sum dispatch in
`src/AggregateFunctions/AggregateFunctionSum.cpp`
(`createAggregateFunctionSum`, `SumSimple`), the numeric/Decimal dispatch helpers
in `src/AggregateFunctions/Helpers.h`, accumulator promotion in
`src/Core/Field.h` (`NearestFieldTypeImpl`), sum state serialization in
`src/AggregateFunctions/AggregateFunctionSum.h`
(`AggregateFunctionSumData::write`/`read`), and Native column dispatch in
`src/Formats/NativeReader.cpp` / `NativeWriter.cpp`. Nullable sum's adapter is
confirmed in `AggregateFunctionNullBase::serialize`/`deserialize` and
`AggregateFunctionNullUnary` in
`src/AggregateFunctions/Combinators/AggregateFunctionNull.h`, with construction
in `AggregateFunctionCombinatorNull::transformAggregateFunction` and
`transformArguments` in `AggregateFunctionNull.cpp`; bool byte behavior is in
`src/IO/ReadHelpers.h` and `WriteHelpers.h`. The no-op bulk state prefix/suffix is
confirmed in `src/DataTypes/Serializations/ISerialization.h`, and the unversioned
state defaults are in `src/AggregateFunctions/IAggregateFunction.h`. All wire,
canonicalization, and version claims above are **CONFIRMED** at
`v26.6.1.1193-stable`; only the historical introduction release is marked
**INFERRED**.

### Geo types: Point, Ring, LineString, MultiLineString, Polygon, MultiPolygon, MultiPoint

**Type string(s):** the seven bare alias spellings `Point`, `Ring`, `LineString`,
`MultiLineString`, `Polygon`, `MultiPolygon`, and `MultiPoint`, registered
case-sensitive with no aliases (`DataTypeCustomGeo`). The Native header carries
the bare alias, never the expanded form, and the mapping is one-directional: a structural
`Array(Tuple(Float64, Float64))` header stays spelled that way and decodes as a
plain `Array`/`Tuple`.

**Logical type:** `ChType::Geo(GeoKind)` over the seven `GeoKind` variants. Each
expands to a fixed nesting over `Float64` (`GeoKind::underlying_type`):

- `Point` = unnamed `Tuple(Float64, Float64)`
- `Ring`, `LineString` = `Array(Point)`
- `Polygon`, `MultiLineString` = `Array(Array(Point))`
- `MultiPolygon` = `Array(Array(Array(Point)))`
- `MultiPoint` = `Array(Point)`

**Wire payload:** byte-identical to the underlying nesting. There is no custom
geo serialization and no extra prefix, so decode, the state prefix, and the
custom-serialization marker are exactly the underlying
`Tuple`/`Array`-of-`Float64` column's.

**Arrow export:** the underlying export. `Point` is a `+s` struct of two `g`
(Float64) children; the six `Array`-based kinds are `+L` LargeList chains above
that struct. Zero-copy, no new buffers.

**Rust buffer:** the underlying `Tuple`/`Array` `Column`, reached through
`ChType::physical_delegate`. `Point` is a two-field `TupleColumn` of `Float64`
columns; the others are `ArrayColumn` chains over it. No new Column variant.

**Notes:**

- The original six aliases are GA at `v26.6.1.1193-stable`. `MultiPoint` is
  present at `v26.8.1.2041-lts`, where it is registered as `Array(Point)`.
  The `allow_experimental_geo_types` gate is an obsolete no-op.
- `Nullable(Point)` is legal (`DataTypeTuple::canBeInsideNullable()` is true).
  `Nullable` of the six `Array`-based kinds is illegal, and `LowCardinality` is
  illegal for all seven. All seven are legal as `Array`/`Tuple` elements and as `Map`
  keys and values; the key case is accepted leniently, resolving through the
  delegate to the underlying `Tuple`/`Array`.
- Custom-serialization marker: the six `Array`-based kinds are always `0`.
  `Point` could in principle carry a nonzero marker via the generic `Tuple`
  sparse path, which the decoder rejects as `UnsupportedSerialization`
  (pre-existing behavior) rather than misreads.
- Depth accounting: each kind charges its physical expansion depth
  (`GeoKind::expansion_depth`, `Point` 1 through `MultiPolygon` 4) on both the
  parse and encode sides, so a geo-tipped header that decodes is always
  re-encodable.

**Introduction version:** undetermined for the original six aliases from the
shallow `.server-src` checkout. `MultiPoint` was added in ClickHouse 26.8 and is
confirmed at `v26.8.1.2041-lts`.

**Server reference:** `DataTypeCustomGeo` in
`src/DataTypes/DataTypeCustomGeo.{h,cpp}` (the custom-name attach over the
`Tuple`/`Array`-of-`Float64` nesting). The wire layout and Arrow export are the
underlying types'; see the `Tuple`, `Array`, and fixed-width numeric sections.
The original aliases are confirmed at `v26.6.1.1193-stable`; `MultiPoint` and
its `Array(Point)` delegate are confirmed at `v26.8.1.2041-lts`.

### Geometry

**Introduction version:** undetermined from the shallow `.server-src` checkout.
The earliest tag at which this project confirmed the type unconditionally
registered and GA is `v26.6.1.1193-stable`; that status was reconfirmed at
`v26.8.1.2041-lts`. The shallow checkouts do not establish its first release.
Do not infer an earlier version from the obsolete fixed-true geo feature gate.

**Type string(s):** canonical `Geometry`. The exact uppercase `GEOMETRY` is a
case-sensitive MySQL-compatible input alias and normalizes to `Geometry`;
lowercase `geometry` is not registered. Native text headers always carry the
canonical name, never the expanded Variant spelling.

**Logical type:** `ChType::Geometry`, a custom fixed name over this exact
canonical Variant and global discriminator order:

```text
0   LineString
1   MultiLineString
2   MultiPolygon
3   Point
4   Polygon
5   Ring
6   MultiPoint
255 NULL
```

`ChType::physical_delegate()` returns
`Variant(LineString, MultiLineString, MultiPolygon, Point, Polygon, Ring,
MultiPoint)`. The server uses `FixedDiscriminatorOrder` and requires new
Geometry alternatives to be appended, preserving the existing persisted
discriminators 0 through 5.

**Wire payload:** byte-identical to that seven-child Variant. Direct `FORMAT
Native` writes one little-endian `UInt64` BASIC mode word `0`, one global
`UInt8` discriminator per row, then one dense bulk body per alternative in the
order above. A child contains only the rows that selected it. Discriminator
`255` contributes no child value. The seven child bodies use the ordinary geo
layouts documented above. Geometry adds no framing, marker, version, setting,
or remapping, and its custom-serialization marker is the generic default `0`.
Like direct Variant, this core rejects COMPACT mode `1` because NativeWriter
does not emit it.

**Binary type descriptor:** the generic Custom tag and canonical name, exactly
`2c 08 47 65 6f 6d 65 74 72 79`. It is not structural Variant tag `0x2a` and
has no Geometry-specific tag.

**Arrow export:** one Arrow Dense Union with format `+ud:0,1,2,3,4,5,6,7`.
Children 0 through 6 retain the geo names and their zero-copy struct/LargeList
shapes; child 7 is Arrow Null named `NULL`. The union uses the existing i8 type
ids and i32 dense offsets, has no top-level validity buffer, and is flagged
nullable because NULL is intrinsic.

**Rust buffer:** `Column::Variant(VariantColumn)`, with seven dense physical geo
children plus the existing Null child and routing buffers. There is no Geometry
column wrapper and no per-row Geometry branch, allocation, remap, or copy.
The fixed Geometry Variant and seven geo type trees are initialized once per
process and borrowed by internal prefix, body, suffix, validation, and FFI
dispatch, so repeated small blocks do not rebuild temporary type trees.

**Notes:**

- `Nullable(Geometry)` and `LowCardinality(Geometry)` are illegal. Geometry
  already owns intrinsic NULL through its Variant discriminator.
- `Array(Geometry)`, Tuple elements, and Map keys or values are legal. A direct
  `Variant(Geometry, ...)` is illegal because Geometry's physical TypeIndex is
  Variant.
- Casting Geometry into Dynamic flattens its seven alternatives; Dynamic does not
  preserve Geometry as one nested child. A typed JSON path of Geometry is legal
  and delegates to this Variant serialization.
- Geometry charges five physical depth levels, one Variant plus the deepest
  four-level MultiPolygon child, during text-header parsing, complete binary
  descriptor validation, and encode validation. At the generic binary Custom
  `0x2c` boundary, the name itself is reparsed with a fresh text-depth budget,
  matching ClickHouse. The core then validates the complete constructed type
  under its aggregate `MAX_TYPE_DEPTH` cap before body traversal.
- It is unconditionally registered and GA at `v26.6.1.1193-stable`, and that
  status is reconfirmed at `v26.8.1.2041-lts`.
  `allow_experimental_geo_types` is an obsolete fixed-true setting.

**Server reference:** `DataTypeGeometryName` and
`registerDataTypeDomainGeo` in `src/DataTypes/DataTypeCustomGeo.cpp`;
`DataTypeVariant::DataTypeVariant` and `doGetSerialization` in
`src/DataTypes/DataTypeVariant.cpp`; the bulk prefix/body methods in
`src/DataTypes/Serializations/SerializationVariant.cpp`; `IDataType::getSerialization`
in `src/DataTypes/IDataType.cpp`; and Custom descriptor encode/decode in
`src/DataTypes/DataTypesBinaryEncoding.cpp`. The fresh Custom-name parse is
confirmed through `DataTypeFactory::getImpl` in
`src/DataTypes/DataTypeFactory.cpp`, `tryParseQuery` in
`src/Parsers/parseQuery.cpp`, and `IParser::Pos` in
`src/Parsers/IParser.h`. Native framing and BASIC-mode selection are in
`src/Formats/NativeWriter.cpp`. The baseline Geometry contract is confirmed at
`v26.6.1.1193-stable`; the appended `MultiPoint` discriminator and fixed order
are confirmed at `v26.8.1.2041-lts`.

### Nested(name1 T1, ...)

**Type string(s):** `Nested(name1 T1, name2 T2, ...)` with at least one field.
Element names are MANDATORY (`Nested(UInt32)` is a server parse error) and follow
the same `checkTupleNames` rules as a named `Tuple` (no empty name, no
exact-lowercase `null`, no duplicates), quoted with `backQuoteIfNeed`. This
spelling reaches the Native wire when a table is created with
`flatten_nested = 0` (the default `flatten_nested = 1` expands `Nested` into
sibling `n.a Array(T)` columns at CREATE time), AND in any SELECT projection that
CASTs to `Nested` regardless of the setting (the setting governs table DDL, not
projections; the fixture capture confirmed a plain `SELECT` CAST keeps the
literal `Nested(...)` header).

**Logical type:** `ChType::Nested(Vec<(String, ChType)>)`, expanding to
`Array(Tuple(named fields))` (`nested_underlying_type`).

**Wire payload:** byte-identical to `Array(Tuple(named fields))`: the element
state prefixes recurse per field, then cumulative `UInt64` LE end-offsets, then
the flattened field-major tuple body. There is no `SerializationNested`; the
runtime object is a `DataTypeArray` over a `DataTypeTuple` with a custom name.

**Arrow export:** `+L` LargeList of a `+s` struct whose children carry the
declared field names verbatim (the same names the flattened `n.a` sibling columns
would use, without the `n.` prefix). Reached through `ChType::physical_delegate`.

**Rust buffer:** the underlying `ArrayColumn` over a two-or-more-field
`TupleColumn`; no new Column variant. The field names live in the schema's
`ChType::Nested`.

**Notes:**

- `Nullable(Nested)` and `LowCardinality(Nested)` are both illegal (it is an
  `Array`).
- `Nested` inside a container (`Array(Nested(...))`) is accepted leniently on
  decode. That layout is INFERRED from the delegation architecture, not
  test-confirmed against the server. Server-side flattening is not recursive, so
  deep `Nested`-in-`Nested` under `flatten_nested = 0` is real.
- Depth accounting: `Nested` charges +2 physical levels (`Array` + `Tuple`) on
  both the parse and encode sides.
- Binary-encoded type headers give `Nested` a distinct `0x2F` tag
  (`DataTypesBinaryEncoding`), which the explicit `*_binary_types` APIs preserve
  as `ChType::Nested`.

**Introduction version:** undetermined from the shallow `.server-src` checkout.
Stable at `v26.6.1.1193-stable`.

**Server reference:** `DataTypeNested` in
`src/DataTypes/DataTypeNested.{h,cpp}` (`DataTypeNested.cpp` renders each field
name with `backQuoteIfNeed` exactly like `DataTypeTuple`, and the runtime type is
`Array(Tuple(...))`). The wire layout and Arrow export are the
`Array(Tuple(...))` sections'. Confirmed at `v26.6.1.1193-stable`.

---

### Variant(T1, ...)

**Type string(s):** `Variant(T1, ...)` with 1 through 255 distinct legal
alternatives after normalization. `DataTypeVariant` canonicalizes alternatives
lexicographically by their complete canonical `getName()`, removes duplicate
names, and drops any alternative whose PHYSICAL type is `Nothing` (via
`isNothing`), so a name-decorated `SimpleAggregateFunction(..., Nothing)` is
dropped too even though its `getName()` is not literally `Nothing`. The parser
also rejects a header spelling more than 255 top-level alternatives before
parsing any part, since a canonical `getName()` never exceeds that count. A
direct alternative cannot be `Nullable`,
`LowCardinality(Nullable(...))`, `Variant`, or `Dynamic`; a legal nested type
such as `Array(Nullable(String))` remains valid. `Nullable(Variant(...))` and
`LowCardinality(Variant(...))` are illegal because Variant already has intrinsic
NULL.

**Logical type:** `ChType::Variant(Vec<ChType>)`, with alternatives stored in
the server's canonical order. The alternative index in this vector is the
global ClickHouse discriminator.

**Wire payload:** direct `FORMAT Native` always uses BASIC Variant
serialization at the pinned server tag. For a block with rows, the complete
column payload is:

```text
UInt64 LE mode = 0
state prefix for T0, then T1, ...
num_rows UInt8 discriminators
dense body for T0, then T1, ...
state suffix for T0, then T1, ...
```

Discriminators `0..alternatives.len()-1` select the corresponding canonical
alternative. `255` is intrinsic NULL and consumes no alternative value. Each
alternative body contains exactly the number of values selected by that
discriminator, in row occurrence order. There are no child counts, byte
lengths, or offset arrays on the wire. The decoder derives counts and dense
offsets in one pass over the discriminator bytes, then decodes each compact
child body. Mode 1 is the COMPACT on-disk form supported by
`SerializationVariant`, but `NativeWriter` does not emit it; this core rejects
it as `DecodeError::InvalidVariant` instead of guessing.

**Arrow export:** Arrow Dense Union with sequential type codes. A Variant with
at most 127 alternatives exports as one `+ud:0,1,...` node whose children are
the alternative columns followed by an Arrow Null child. Its two buffers are
signed Int8 type ids and i32 dense child offsets; unions have no validity
buffer or top-level null count. The ClickHouse NULL discriminator routes to the
final Null child. Arrow permits only 128 type codes in one union node, while
ClickHouse permits 255 alternatives, so 128 through 255 alternatives export as
an outer Dense Union over inner groups of at most 128 alternatives, followed by
the outer Null child. This preserves the full ClickHouse range without copying
the decoded buffers.

**Rust buffer:** `Column::Variant(VariantColumn)`. `VariantColumn::variants`
contains one dense `Column` per canonical alternative. `VariantLayout::Flat`
stores the top-level `{ type_ids: Vec<i8>, offsets: Vec<i32> }`; the nested form
adds deterministic `VariantGroup` routing buffers.
`nulls: NothingColumn` provides the explicit Arrow Null child length.
`value_position(row)` resolves a row to its global discriminator and child
offset; discriminator 255 denotes NULL.

**Notes:**

- Native writes no Variant payload at all for a zero-row block, including no
  mode word or alternative prefixes, because `NativeWriter` gates the complete
  data step on `rows > 0`.
- An out-of-range non-255 discriminator is malformed. The server bulk path does
  not provide a clean range error at this point. Its exact process behavior is
  therefore INFERRED to be undefined for such corrupt input; this core
  deliberately returns `DecodeError::InvalidVariant` before indexing a child.
- Variant first appeared experimentally in `v24.1.1.2048-stable`; the mode word
  was added in `v24.7.1.2915-stable`, it became beta in
  `v24.12.1.1614-stable`, production-ready in 25.3, and GA at the pinned 26.6
  release. This implementation targets the mode-bearing layout at
  `v26.6.1.1193-stable`.

**Server reference:** `NativeWriter::writeData` and `NativeWriter::write` in
`src/Formats/NativeWriter.cpp`; `SerializationVariant` state prefix, bulk body,
and state suffix methods in
`src/DataTypes/Serializations/SerializationVariant.cpp`;
`DataTypeVariant::DataTypeVariant`, `doGetName`, and `canBeInsideNullable` in
`src/DataTypes/DataTypeVariant.{h,cpp}`; and `ColumnVariant` discriminator
constants and validation in `src/Columns/ColumnVariant.{h,cpp}`. The BASIC
layout, canonical ordering, legality rules, NULL discriminator, and direct
Native mode choice are CONFIRMED at `v26.6.1.1193-stable`.

### Dynamic

**Type string(s):** `Dynamic` is the canonical spelling for the default
`max_types=32`; `Dynamic(max_types=N)` is canonical for every other `N` in
`0..=254`. The server also accepts `Dynamic()` and
`Dynamic(max_types=32)` but renders both back as `Dynamic`. Parameter and type
names are case-sensitive. `Nullable(Dynamic)`, `LowCardinality(Dynamic)`, and a
direct `Variant(Dynamic, ...)` alternative are illegal because Dynamic already
has intrinsic NULL and owns a Variant-like runtime type set. Dynamic is legal
inside Array, Tuple, and Map key/value positions.

**Logical type:** `ChType::Dynamic { max_types: u8 }`. The logical schema does
not contain the value types. Those are discovered separately in every Native
block and can differ across chunks of one result.

**Wire payload:** a block with rows starts with one little-endian UInt64
structure word. Direct NativeWriter selects V1 or V2 from the negotiated client
revision; its opt-in flattened output setting selects word 3:

```text
V1, revision < 54473:
  UInt64 LE 1
  VarUInt M                 legacy ignored slot
  VarUInt M                 direct type count
  M type entries
  UInt64 LE Variant mode 0
  state prefixes for global children
  num_rows UInt8 discriminators
  dense child bodies in global order
  state suffixes in global order

V2, revision >= 54473:
  UInt64 LE 2
  VarUInt M                 direct type count
  M type entries
  UInt64 LE Variant mode 0
  state prefixes, discriminators, dense bodies, state suffixes

FLATTENED:
  UInt64 LE 3
  VarUInt K                 emitted typed child count
  K type entries
  state prefixes for the K children
  num_rows fixed-width indexes
  K sparse typed bodies in table order
  state suffixes in table order
```

The V1/V2 table contains direct typed children only. The physical Variant also
has one implicit `SharedVariant` String child; direct types plus SharedVariant
are globally sorted by canonical type name before discriminators are assigned.
`255` is NULL. SharedVariant cells are String-framed opaque blobs containing one
binary data-type descriptor followed by exactly one value's `serializeBinary`
payload. The core keeps each blob opaque instead of introducing a per-row type
parse and value materialization into the bulk path.

FLATTENED has no SharedVariant child and no Variant mode word. Index `K` is
NULL; `0..K-1` select the table entries and values above K are malformed. The
index width is the smallest of UInt8/UInt16/UInt32/UInt64 that can represent
the K children plus NULL. FLATTENED may expose more types than the logical
`max_types`, since it expands types previously stored in SharedVariant.

Type entries are StringBinary canonical type names by default. With
`output_format_native_encode_types_in_binary_format=1`, both outer Native type
headers and Dynamic tables use `DataTypesBinaryEncoding` descriptors instead.
The public `decode_*_binary_types` and `encode_*_binary_types` APIs select that
out-of-band form while keeping `DecodeOptions` and `EncodeOptions` source
compatible. The descriptor reader is bounded by `MAX_TYPE_DEPTH`, complexity
1000, and list count 1,000,000, and rejects illegal wrappers, reserved tags, and
unsupported descriptor forms before payload decode.

A zero-row Native block carries only the logical column header. NativeWriter
suppresses the structure word, type table, child prefixes, routing, and bodies.

**Arrow export:** Dynamic exports as Arrow Dense Union with typed children, an
Arrow Binary (`z`) SharedVariant child when present, and a final Arrow Null
child. The block-local `u32` ids are remapped once into Arrow signed Int8 union
codes; typed child value buffers stay zero-copy. Child sets of 128 or more use
the same union-of-unions strategy as Variant. `export_chunks_to_stream`
pre-scans the already-supplied chunks once, recursively through containers,
and fixes one result-wide canonical child set before exposing the stream
schema. Each record batch supplies zero-length arrays for children absent from
that block and remaps routing to the result-wide ids. Child sets are capped at
16,256 (127 groups of 128; the top union reserves one code for NULL) because a
wider set cannot be represented in Arrow's signed Int8 type-code space. A
FLATTENED block's type table is bounded only by its row count, so the cap is
enforced at export, never at decode: the standalone batch entry points return
`ExportError::DynamicUnionTooWide`, and the stream's `get_schema`/`get_next`
return nonzero with the message available through `get_last_error`. For a
standalone record
batch, use `export_batch`, or pair `export_batch_schema` with
`export_batch_array` (all three return `Result` for this reason); logical
`export_schema` alone cannot invent block-local Dynamic children and therefore
describes only the intrinsic Null child.

**Rust buffer:** `Column::Dynamic(DynamicColumn)`. `type_ids: Vec<u32>` uses
`u32::MAX` for NULL and a block-local child index otherwise.
`offsets: Vec<i32>` stores the occurrence ordinal in the selected dense child.
`children: Vec<DynamicChild>` contains either
`Typed { ch_type, values: Column }` or `Shared(Utf8Column)`. Shared uses the
existing offsets+bytes storage implementation, but its semantics and Arrow
schema are Binary, never Utf8. `nulls: NothingColumn` provides the explicit
Arrow Null child length.

**Validation and performance:** the discriminator/index run is scanned once to
derive child counts and offsets, then every typed child is decoded in bulk
through the existing recursive column path. Allocation is per routing buffer
and child column, never per value; direct typed payloads keep the primitive
read-into-Vec fast path. Encode validation proves occurrence-ordinal offsets,
child lengths, direct-type legality and uniqueness, V1/V2 canonical child
order, the single-SharedVariant invariant, and `max_types` before writing.
Unknown structure words, word 4 V3, direct Variant modes other than BASIC 0,
out-of-range routing, duplicate/illegal types, and direct counts above 254 or
`max_types` return `DecodeError::InvalidDynamic` or a precise unsupported-type
error. Word 4 is rejected because NativeWriter does not emit it at the pin.

**Version:** Dynamic first appeared experimentally in 24.5, became
production-ready in 25.3, and is GA/stable at `v26.6.1.1193-stable`.

**Server reference:** `NativeWriter::writeData`/`write` in
`src/Formats/NativeWriter.cpp`, `NativeReader` in
`src/Formats/NativeReader.cpp`, Native settings in
`src/Processors/Formats/Impl/NativeFormat.cpp`, revision negotiation in
`src/Server/TCPHandler.cpp` and `HTTPHandler.cpp`, canonical naming in
`DataTypeDynamic::doGetName` (`src/DataTypes/DataTypeDynamic.cpp`), runtime
children in `ColumnDynamic` and `ColumnVariant` (`src/Columns/`), the state and
body layouts in `SerializationDynamic`, `SerializationDynamicHelpers`, and
`SerializationVariant` (`src/DataTypes/Serializations/`), and descriptor tags
in `DataTypesBinaryEncoding` (`src/DataTypes/`). The structure selection,
tables, ordering, SharedVariant shape, NULL values, flattened indexes, and
version history above are **CONFIRMED** at `v26.6.1.1193-stable`. Stricter Rust
rejection of overlong/nonterminating VarUInt input, canonical enforcement of a
V1/V2 table, and retaining SharedVariant blobs without parsing their trailing
payload are deliberate core policies, not claims about additional server
validation.

### JSON

**Type string(s):** `JSON` is the canonical spelling when every parameter is
default and no paths are declared. Otherwise the server emits `JSON(...)` joining
these in order: `max_dynamic_types=M` (only when != 32), `max_dynamic_paths=N`
(only when != 1024), the typed paths sorted lexicographically (each
`<path> <TypeName>`), the `SKIP <path>` entries sorted, then the
`SKIP REGEXP '<regex>'` entries. Each path renders through the server's
`backQuoteIfNeed` on the WHOLE path, so a dotted path such as `a.b` always
backtick-quotes to `` `a.b` `` and a path literally named `SKIP`
(case-insensitive) always quotes; the parser strips the quoting back to the bare
path string. `parse_ch_type` canonicalizes an unsorted or out-of-order header
into this form, so `parse(display(t)) == t`. Parameter bounds are
`max_dynamic_paths` <= 10000, `max_dynamic_types` <= 254, and typed paths <=
1000; a header past any bound, a duplicate parameter, or the legacy
`Object('json')` spelling (unregistered at this tag) is `UnsupportedType`. A
typed-path type may itself be `JSON` (nested), bounded by `MAX_TYPE_DEPTH`.

**Logical type:** `ChType::Json { max_dynamic_paths: u32, max_dynamic_types: u8,
typed_paths: Vec<(String, ChType)>, skip_paths: Vec<String>, skip_regexps:
Vec<String> }`. Typed paths are stored sorted by path. The runtime dynamic paths
and shared-data overflow are NOT in the logical type: like Dynamic they are
discovered per Native block and can differ across chunks of one result.

**Wire payload:** a block with rows starts with one little-endian UInt64
structure version word. Direct NativeWriter selects V1/V2 from the negotiated
revision; its opt-in flattened setting selects word 3:

```text
one LE u64 structure word:
  0 = V1, 1 = STRING, 2 = V2, 3 = FLATTENED (4 = V3 is MergeTree-only, rejected)

prefix by form:
  V1 (word 0):     VarUInt M legacy ignored slot (equals the path count),
                   VarUInt M dynamic-path count, M sorted path strings
  STRING (word 1): nothing else in the prefix
  V2 (word 2):     VarUInt M dynamic-path count, M sorted path strings
  FLATTENED (3):   VarUInt K flattened-path count (UNBOUNDED), K sorted paths

  then, for every non-STRING form:
    typed-path state prefixes, in sorted path order
    one full SerializationDynamic prefix per dynamic/flattened path, sorted
    (shared data contributes no prefix bytes)

body with rows:
  STRING:  one VarUInt-length-prefixed document string per row
  others:  typed-path bodies, in sorted path order
           one full Dynamic body per dynamic/flattened path, sorted
           V1/V2 only: shared-data Array(Tuple(String, String)) body:
             num_rows LE u64 end-offsets, then the flattened path strings,
             then the flattened value strings

suffix: zero bytes (JSON owns no suffix; children walked for cursor alignment)
```

V1 and V2 differ only in V1's ignored legacy VarUInt slot; both carry the
shared-data stream. The dynamic-path count is strictly bounded by
`max_dynamic_paths` on V1/V2, where the writer routes any path past the limit
into shared data so a larger direct count is malformed
(`DecodeError::InvalidJson`). The FLATTENED count is UNBOUNDED and routinely
exceeds `max_dynamic_paths`: `unflattenAndInsertPaths`
(`SerializationObjectHelpers.cpp`) writes the union of the dynamic paths and
every distinct shared-data path, greedily assigns the first sorted paths that
fit `max_dynamic_paths`, and spills the rest back to shared data, so it is
protected by the read-before-allocate discipline rather than a count cap.
Dynamic paths must be strictly increasing (sorted, unique); anything else is
`InvalidJson`.

FLATTENED carries no shared-data stream (each path is a shared-less Dynamic) and
decodes to a structured column with empty shared columns (`shared_offsets` all
zero, one per row plus the Arrow leading 0). A PATHLESS FLATTENED column writes
ZERO body bytes per row, so a block can legitimately claim more rows than it has
bytes; the type-aware per-column row-count guard excludes JSON (and any container
that bottoms out in it) and bounds it by its own read-before-allocate decode.
SharedVariant and shared-data values are opaque binary blobs, one binary
data-type descriptor followed by one `serializeBinary` payload, the same shape as
a Dynamic SharedVariant cell, kept opaque and never materialized. A zero-row
Native block writes only the column header (no structure word, path list, or
bodies); `empty_column` builds the declared typed paths with empty children and
empty shared data.

**Arrow export:** a structured body exports as an Arrow struct (`+s`) whose
children are, in fixed order: one child per declared typed path (each the path
type's own schema and buffers), one Dense Union child per dynamic path (the same
result-wide Dynamic export as a standalone `Dynamic` column, SharedVariant child
`z`, final Null child), and a `_shared_data` child. `_shared_data` is a
non-nullable Arrow LargeList (`+L`) of a non-nullable struct (`+s`) pairing
`paths` (utf8 `u`, like a String column) with `values` (binary `z`, the opaque
descriptor+payload bytes, never utf8). A STRING-mode (`Text`) body exports as a
plain utf8 column (`u`), the same shape as `String`. `Nullable(JSON)` fills the
struct or utf8 validity from the top-level null map.

Because the dynamic-path child set is block-local column data,
`export_chunks_to_stream` pre-scans the chunks once and fixes one result-wide
dynamic-path set (BTreeMap name order) before exposing the stream schema, exactly
like Dynamic; each record batch supplies zero-length arrays for paths absent from
that block. The `DynamicUnionTooWide` cap applies per dynamic-path union. A pure
logical `export_schema` (no sample column) cannot invent block-local dynamic
paths, so it describes only the declared typed paths plus `_shared_data`; pair
`export_batch_schema`/`export_batch_array` (or `export_batch`) for the full child
set. All chunks of one JSON field must agree on body kind (all structured or all
Text); a mismatch returns `ExportError::JsonBodyKindMismatch`.

**Rust buffer:** `Column::Json(JsonColumn { body, validity })`. `body` is
`JsonBody::Structured(Box<StructuredJson>)` or `JsonBody::Text(Utf8Column)`;
`StructuredJson` is boxed because it is much larger than a `Utf8Column`.
`StructuredJson` is `{ typed: Vec<(String, Column)>, dynamic: Vec<(String,
DynamicColumn)>, shared_offsets: Vec<i64>, shared_paths: Utf8Column,
shared_values: Utf8Column, len: usize }`: typed paths in the declared sorted
order, dynamic paths sorted by name (each a block-local `DynamicColumn`), and the
shared-data overflow as an Arrow list-offset run (leading 0, length `len + 1`)
over the two flattened string columns. `shared_values` are opaque blobs, never
materialized. `validity` is populated only under a `Nullable(JSON)` wrapper (the
top-level null map), independent of the children, exactly as `TupleColumn`
carries `Nullable(Tuple(...))` validity. `StructuredJson::try_new` validates
child lengths, strictly sorted dynamic paths, and the shared-offset run.

**Wrappers, containers, and keys:** `Nullable(JSON)` is legal; the null map
precedes the full JSON body. `LowCardinality(JSON)` is illegal. JSON composes
inside `Array`, `Tuple`, and `Map` (as key or value), as a `Variant`
alternative, as a `Dynamic` runtime type, and as its own typed-path type (nested
JSON), all bounded by `MAX_TYPE_DEPTH`.

**Encode:** see the "Encoding" section. In brief: a `Text` body writes STRING
(word 1); a structured body writes FLATTENED (word 3) only via
`EncodeOptions.flattened_dynamic` when the column carries no shared pairs (pre
25.6 servers reject word 3), otherwise the revision-selected V1 (< 54473) or V2
(>= 54473). Validation proves the typed-path set and lengths match the declared
type, the dynamic paths are strictly sorted and within `max_dynamic_paths` on
V1/V2, and the shared-offset run is well formed.

**Binary type descriptor:** JSON's `DataTypesBinaryEncoding` tag is `0x30`: a
`TYPE_JSON_SERIALIZATION_VERSION` byte (must be 0), a VarUInt `max_dynamic_paths`,
a RAW u8 `max_dynamic_types`, then the typed-path list (VarUInt count, each a
StringBinary path then a nested descriptor), the `SKIP`-path list, and the
`SKIP REGEXP` list. The lists may arrive in arbitrary order and are canonicalized
on read (sorted typed paths, sorted skip sets), so the reconstructed
`ChType::Json` matches the textual-header form. The bounds above and a nonzero
version byte are rejected before any payload decode. The
`decode_*_binary_types` / `encode_*_binary_types` APIs select this out-of-band
form.

**Introduction version:** the JSON data type is marked production ready in
ClickHouse 25.3 per the `registerDataTypeJSON` documentation string
(`DataTypeObject.cpp`), and is GA/stable at `v26.6.1.1193-stable`. The default
Native serialization at the pin is the structured V1/V2 form; both output
settings default off. Per `SettingsChangesHistory.cpp` the two settings were
introduced in DIFFERENT releases: `output_format_native_write_json_as_string` in
24.10 and `output_format_native_use_flattened_dynamic_and_json_serialization` in
25.6.

**Server reference:** `SerializationObject` (structure state prefix, body, and
suffix; `deserializeObjectStructureStatePrefix`), `SerializationObjectSharedData`,
and `SerializationObjectHelpers` (`unflattenAndInsertPaths`, the unbounded
FLATTENED count) in `src/DataTypes/Serializations/`; `DataTypeObject`,
`registerDataTypeJSON`, and `DataTypeObject::doGetName` in
`src/DataTypes/DataTypeObject.{h,cpp}`; the `0x30` descriptor in
`DataTypesBinaryEncoding` (`src/DataTypes/`); and `NativeWriter`/`NativeReader`
(`src/Formats/`) for the zero-row gate and revision selection. The structure
words, V1 legacy slot, the V1/V2 path-count bound versus the confirmed unbounded
FLATTENED count, the body order, STRING and FLATTENED shapes, opaque shared
blobs, and the version history above are CONFIRMED at `v26.6.1.1193-stable`.
Stricter Rust rejection of a V3/unknown structure word and of a V1/V2 direct
count above `max_dynamic_paths`, and retaining shared blobs without parsing their
trailing payload, are deliberate core policies, not claims about additional
server validation.

---

## Encoding

`src/native/encode/mod.rs` is the inverse of the decode path: it turns a `ColBatch`
(or each chunk of a `ChunkedBatch`) back into the Native block bytes the server
accepts for `INSERT`. The wire it produces is exactly the wire the per-type
sections above describe and exactly what `decode_next_block` reads at the same
protocol revision, so the per-type "Wire payload" is not repeated here. This
section is the encode-only contract: the API, what encode trusts, what it
rejects, the choices it makes where the format allows more than one valid
encoding, and what a round trip guarantees.

Confirmed against the server source at `v26.6.1.1193-stable`: `NativeWriter::write`
in `src/Formats/`, `BlockInfo::write` in `src/Core/BlockInfo.cpp`, and the
revision-0 read path in `NativeFormat.cpp` in `src/Processors/Formats/Impl/`.

### API

- `encode_block(&ColBatch, &EncodeOptions) -> Result<Vec<u8>, EncodeError>`
  produces one standalone Native block.
- `encode_chunked(&ChunkedBatch, &EncodeOptions) -> Result<Vec<u8>, EncodeError>`
  produces one block per chunk, in chunk order, concatenated. It validates every
  chunk (including that each chunk's schema equals the batch schema) before
  writing any bytes, so a rejected `ChunkedBatch` leaves no partial stream.
- `encode_block_binary_types` and `encode_chunked_binary_types` are the same
  operations with outer headers and Dynamic tables written as binary data-type
  descriptors. Decode exposes the matching `decode_*_binary_types` functions.
- `EncodeOptions { protocol_revision: u64 }` is the only knob, the mirror of
  `DecodeOptions.protocol_revision`. See "Encode framing" below.

### Trust and error model

Encode's input is trusted in one sense and untrusted in another. The bytes are
in-memory `Column` buffers, not wire bytes from the network, so the failure
modes are structural, not adversarial. But `Column`, `ColBatch`, and `Bitmap`
have public fields and only debug-assert their invariants (`ColBatch::new`,
`Bitmap::from_raw`), so a binding that builds buffers by hand for the insert path
can hand encode a release-mode-inconsistent batch. Encode therefore validates
fully in release, and it does so before writing any bytes.

`EncodeError` has two variants:

- `UnsupportedType { column, ch_type }`: a column whose type encode cannot yet
  write (see coverage below), or a `LowCardinality` inner outside the allowlist.
- `InconsistentBatch { detail }`: a structurally invalid batch. Every
  precondition below maps to this variant.

Because `validate_block` runs to completion before any byte is emitted, a
rejected batch never produces a partial or truncated block. The write step is
structurally infallible once validation passes; its `Result` exists only for a
defensive fall-through that validation already rules out.

### Coverage

Encode coverage is kept a subset of decode coverage and grows the same
one-type-at-a-time way; the two are currently at parity.
Encodable today: `Nothing`, `Bool`, the fixed-width numerics (`Int8`..`Int64`,
`UInt8`..`UInt64`, `Float32`, `Float64`, `BFloat16`), `QBit(T, N)` over each
legal scalar type, the temporals (`Date`, `Date32`,
`DateTime`, `DateTime64`, `Time`, `Time64`, and every `Interval*`), `UUID`,
`IPv4`, `IPv6`, `String`,
`FixedString(N)`,
`Enum8`/`Enum16`, `Decimal(P, S)`, the wide integers
(`Int128`/`UInt128`/`Int256`/`UInt256`, a verbatim fixed-width body byte-identical
to a `Decimal128`/`256` body, the exact inverse of the decode passthrough),
`LowCardinality(T)` for the same allowed
inner types decode accepts, `Array(T)` over any encodable element type
(including a `Nullable`, `LowCardinality`, or nested `Array` element),
`Tuple(T1, ...)` over encodable element types (named or unnamed, the
zero-element `Tuple()` included, composing inside `Array` and inside
`Nullable`), and `Map(K, V)` for a legal key type over encodable key/value
types (composing inside `Array` and `Tuple`), `Variant(T1, ...)` when every
alternative is encodable, `Dynamic` with recursively encodable typed
children plus an optional opaque SharedVariant child, and `JSON` (structured
bodies with recursively encodable typed paths, block-local dynamic paths, and
opaque shared-data blobs, or a STRING-mode `Text` body), the non-wrapper types,
`Tuple`, and `JSON` each optionally wrapped in `Nullable`. The name-decoration aliases
`SimpleAggregateFunction(func, T)` (encodable when its inner `T` is, at any
nesting position), the seven geo types (`Point`, `Ring`, `LineString`,
`MultiLineString`, `Polygon`, `MultiPolygon`, `MultiPoint`, always encodable
since they expand to `Tuple`/`Array` of `Float64`), `Geometry` (the fixed
seven-child Variant), and `Nested(name1 T1, ...)`
(encodable when every field type is) each encode as their physical delegate,
with no new body writer: encode, like decode, recurses on
`ChType::physical_delegate`. The exact `AggregateFunction` state codecs for
`count`, canonical `nothingUInt64` and `nothingNull`, and base `sum` over plain
or Nullable numeric/Enum arguments are also encodable from an
`AggregateStateColumn` after every i64 offset and row state is validated (each
`count` slice exactly one VarUInt64, each fixed-zero `nothing*` slice exactly
`[0x00]`, each plain sum slice exactly its selected 8/16/32-byte accumulator
width, and each nullable sum slice one false flag or one nonzero flag plus that
width); the
`count, Nullable(Nothing)` spelling is rejected as
`UnsupportedType` since the server canonicalizes it to `nothingUInt64`, and the
noncanonical `sum, Nullable(Nothing)` spelling is rejected in favor of
`nothingNull`. Any other type is `UnsupportedType`, at every row count including
zero. This is deliberately stricter than decode, whose `empty_column` builds an
empty column for any decodable type in a zero-row block: encode fails fast rather
than write a header for a type it cannot write rows of.

### Encode framing

`EncodeOptions.protocol_revision` gates the same framing
`DecodeOptions.protocol_revision` gates on the read side, and must match the
revision the consumer reads with.

- **Revision 0**: no `BlockInfo` preamble and no per-column custom-serialization
  marker. This is the HTTP `INSERT ... FORMAT Native` shape: the server builds
  its `NativeReader` with revision 0, expects neither, and stops at EOF.
- **Revision > 0**: each block is preceded by the standard client `BlockInfo`
  preamble (`is_overflows` = false, `bucket_num` = -1, and at revision >= 54480
  an empty `out_of_order_buckets` vector, then the field-0 terminator: the same
  10-byte preamble the "Native block framing" section above documents).
- **Revision >= 54454** (`DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION`): a
  per-column custom-serialization marker byte, always `0x00` (default
  serialization), is written for every column including in a zero-row block.

Within a block, encode writes `num_columns` and `num_rows` as varints, then per
column the varint-length-prefixed name, the varint-length-prefixed type string,
the marker byte (when the revision calls for it), and the column body.

`encode_chunked` writes one Native block per chunk and no terminating empty
block: for HTTP the concatenated blocks are the whole request body and the
server stops at EOF. The native TCP protocol needs an explicit empty-block
terminator; that path is outside the current encode scope.

### Input preconditions

All of these are checked before any bytes are written and reported as
`InconsistentBatch` unless noted. They exist because the `Column` buffers are
public and a hand-built insert column can violate an invariant the decode path
would never produce.

- **Batch shape.** The column count equals the schema field count, and every
  column's `len()` equals the block's `num_rows`. For `encode_chunked`, every
  chunk's schema equals the batch schema.
- **Type nesting depth.** The declared type's wrapper/container nesting is
  capped at the decode parser's `MAX_TYPE_DEPTH` (100), checked iteratively
  before anything walks the type. Encode input never passes through
  `parse_ch_type`, so without this a caller-constructed pathologically deep
  type (say 10^6 nested `Array`s) would overflow the stack in the encoder's
  recursive walks. The rejection is `InconsistentBatch` rather than
  `UnsupportedType`, deliberately: `UnsupportedType` clones and `Display`s the
  `ChType`, both of which recurse to full depth, so the error itself would
  overflow on the input the check exists to reject.
- **Type/buffer match.** The `Column` variant matches the field's `ChType` (for
  a `Nullable(T)`, the inner `T`). A supported type paired with the wrong buffer
  variant (say `Int64` over a `Column::Int32`) is rejected here rather than
  written as wrong-width bytes. A not-yet-encodable type is reported as
  `UnsupportedType`.
- **Type string round-trips.** `field.ch_type.to_string()` must parse back to the
  same `ChType` through `parse_ch_type`, so encode never writes a header the
  server or this crate's own decoder would reject: `FixedString(0)`, a
  `DateTime64` or `Time64` precision above 9, an out-of-range `Decimal`
  precision, and so on.
  A `DateTime`/`DateTime64` timezone containing a single quote is rejected
  separately, since it renders a header that closes its quote early even though
  the crate's own lenient parser would recover it.
- **Validity bitmap backing.** Any present validity bitmap must have a backing
  buffer of at least `len.div_ceil(8)` bytes, and for a nullable field its bit
  length must equal `num_rows`.
- **Nullability match.** A non-`Nullable` field (and a `LowCardinality` over a
  non-`Nullable` inner) must have `null_count() == 0`. Encode writes no null map
  for a non-nullable column, so a null bit there would silently encode that row's
  placeholder value as a real value.
- **Fixed-width bodies.** For `FixedString(N)`, `UUID`, `IPv6`, and the wide
  integers (`Int128`/`UInt128` width 16, `Int256`/`UInt256` width 32), the stored
  buffer width must equal the declared or implied width and the data length must
  be exactly `width * num_rows`. `FixedBinaryColumn::len()` truncates, so a
  misframed buffer would otherwise pass the row-count check and put a different
  number of bytes on the wire. The four wide-int types map 1:1 to their `Column`
  variants, so an `Int128` type over a `UInt128` buffer (both width 16) is a
  mismatched-variant `InconsistentBatch`, caught before any bytes are written.
- **QBit.** The declared dimension is nonzero and within the server's
  FixedString-derived limit; the `QBitColumn` repeats that exact dimension; its
  child variant matches the declared scalar type, has no child validity, and
  contains exactly `num_rows * dimension` values; and all plane and body byte
  counts fit `usize`. The per-plane `num_rows * ceil(dimension / 8)` byte count
  may not exceed the server's 1 GiB FixedString bulk limit.
- **Decimal.** `scale <= precision`, `precision` in `1..=76`, the column's
  `precision`/`scale`/`width` agree with the type, and the width is derived from
  precision (not trusted from `ChType`'s `bits`), with data length exactly
  `width * num_rows`.
- **String offsets.** The Arrow offset invariants are checked (monotonic
  non-decreasing, in range, covering `data` exactly) so the per-row
  `data[offsets[i]..offsets[i+1]]` slice cannot panic or silently drop bytes.
- **LowCardinality.** The inner is in the allowlist and encodable; the dictionary
  has at most `i32::MAX` entries (the public index buffer is `i32`); a zero-row
  block carries an empty dictionary; every index is in `0..num_keys`; and for a
  `LowCardinality(Nullable(T))`, NULL rows use dictionary index 0 and valid rows
  do not. The dictionary column is validated recursively as its own inner-typed
  column.
- **Array offsets.** The Arrow LargeList invariants are checked: exactly
  `num_rows + 1` offsets, `offsets[0] == 0`, monotonically non-decreasing
  (which, from the zero start, also proves every offset non-negative), and a
  final offset equal to the flattened element column's length (a smaller value
  would silently drop trailing elements, a larger one would declare elements
  the body does not carry). The element column is then validated recursively as
  its own column of `offsets[num_rows]` rows, so every element-level guard (a
  `Nullable` element's validity length, `LowCardinality` invariants, string
  offsets, fixed-binary widths, a nested `Array`) applies to the flattened
  buffer too.
- **Tuple elements.** The buffer carries exactly one field column per declared
  element (a count mismatch is `InconsistentBatch`), and each element column is
  validated recursively as its own column of `num_rows` rows, so a ragged
  element length, a wrong element buffer variant, or any element-level
  invariant violation is rejected before any bytes are written (the server
  enforces equal element sizes as `INCORRECT_DATA`). `Tuple()` needs no
  per-element checks; its `TupleColumn::len` is the row count checked by the
  batch-shape rule.
- **Tuple element names.** The names must be a set the server can construct
  (`DataTypeTuple`'s factory and `checkTupleNames`): all-or-nothing named,
  never empty, never the exact-lowercase reserved `null`, never duplicated.
  Violations are `UnsupportedType` (the type cannot exist on the server), the
  same classification as an illegal Map key, applied through nesting via the
  recursive validation. The decode parser deliberately round-trips these
  shapes, so the type-string round-trip check alone cannot catch them.
- **Nested field names.** A `Nested(...)` renders its fields through the same
  named-`Tuple` machinery, so its names get the same `checkTupleNames`
  validation (all named, never empty, never the exact-lowercase reserved
  `null`, never duplicated) applied through the `Tuple` delegation. Violations
  are `UnsupportedType`, the type cannot exist on the server.
- **SimpleAggregateFunction function name.** Every `SimpleAggregateFunction` in
  the declared type (at any nesting position) must carry a SYNTACTICALLY valid
  function spelling: an ASCII identifier optionally followed by one balanced
  parenthesized literal-parameter suffix (`sum`, `anyLast`,
  `groupArrayLastArray(5)`), checked by the exact predicate the decode parser
  uses. This prevents a caller-constructed `func` from injecting extra type
  tokens into the header's type-string channel (the same injection class the
  Tuple element-name validation guards), and is reported as `UnsupportedType`.
  The server's function whitelist is deliberately NOT enforced on encode either:
  the list grows across versions and the server rejects an unknown function
  loudly on INSERT, the same trusted-input boundary as a `DateTime64` precision
  or an `Enum` value set. This is a syntactic guard, not a semantic one.
- **Map key legality and entries.** The key type must satisfy the server's
  `DataTypeMap::isValidKeyType` (never `Nullable` or
  `LowCardinality(Nullable(...))`), reported as `UnsupportedType` since the
  type itself cannot exist. The offsets get the same Arrow list checks as
  `Array` (count, zero start, monotonic, final offset equal to the entries
  length), and the entries buffer must be a two-field `Tuple` column, carrying
  NO validity bitmap (the wire has no null map at the entries level, so one
  would be silently dropped; `InconsistentBatch`), whose keys and values
  columns are validated recursively as their own columns of
  `offsets[num_rows]` rows.

### Encoder choices

Decoding one wire input yields exactly one output; encoding often has more than
one valid wire form, and encode commits to these:

- **Canonical type string.** Encode writes the canonical `ChType` `Display` form
  (`Decimal(P, S)`, `Enum8('a' = 1, ...)`, `DateTime64(3, 'UTC')`, and so on),
  which is the same string the server emits and the decoder parses.
- **Bool.** Encode writes `0x00`/`0x01` per row. The decoder accepts any nonzero
  byte as true, but the server emits 0/1, so encode does too.
- **Nothing.** Encode writes one canonical ASCII `0` byte, `0x30`, per row.
  Decode accepts any placeholder byte because the server reader ignores its
  value. For `Nullable(Nothing)`, the retained structural null map is written
  first through the ordinary Nullable path, then the canonical placeholders.
- **Nullable placeholder.** For a `Nullable(T)`, encode writes the null map, then
  the full inner body straight from the buffer, including whatever value sits in
  each null row's slot. It does not zero or otherwise rewrite null-row values, so
  the placeholder bytes a decode captured are written back verbatim.
- **LowCardinality.** Encode writes `key_version` = 1
  (`SharedDictionariesWithAdditionalKeys`) once per block per column, and an index
  word with `HasAdditionalKeysBit` and `NeedUpdateDictionary` set and
  `NeedGlobalDictionaryBit` clear (Native never uses a global dictionary). It
  picks the narrowest self-describing index width that addresses the dictionary
  (u8 through 255 entries, then u16, u32, u64). The dictionary is written exactly
  as the `DictionaryColumn` carries it, in slot order; encode does not re-dedup or
  reorder it. Indexes are written as a raw little-endian array at the chosen width.
- **Array.** The wire offsets are `offsets[1..]` written as raw little-endian
  `u64`: the model's leading 0 is Arrow-only and never hits the wire, and
  validation proved every offset non-negative, so each i64's little-endian bytes
  are exactly the wire `UInt64`'s. The element state prefix is hoisted by
  `write_state_prefix`, the mirror of decode's `read_state_prefix`: `Array`
  writes nothing of its own and recurses into the element, so a leaf
  `LowCardinality`'s 8-byte key version lands exactly once at the very front of
  the whole column, before any offsets, across every nesting level; the element
  body is then written through the shared value path without re-emitting a
  prefix. A zero-length element run (rows > 0 but every array empty) writes NO
  `LowCardinality` body at all, not even the index word, matching the server's
  `limit == 0` early return in
  `SerializationLowCardinality::serializeBinaryBulkWithMultipleStreams`; every
  other element writer degrades naturally to zero bytes at count 0.
- **Tuple.** The element bodies are written one after another in declaration
  order through the shared value path, with no tuple-level framing. Element
  state prefixes are hoisted by the same `write_state_prefix` recursion
  (`SerializationTuple` delegates per element, `SerializationNullable` to its
  nested type), so a `LowCardinality` element's key version lands once at the
  front of the whole column, before any element body. `Tuple()` writes the
  canonical ASCII '0' placeholder byte per row (the decoder accepts any byte
  values, matching the server's `tryIgnore`). A zero-length tuple run nested
  inside an all-empty `Array` writes nothing, each element taking its own
  `limit == 0` behavior.
- **Map.** Encode always writes the BASIC `Array(Tuple(keys, values))` wire
  form (the only form the Native wire carries; the bucketed on-disk mode never
  applies): the offsets exactly as the `Array` bullet above, then the
  flattened key run and the flattened value run through the shared value path.
  The key and value prefixes are hoisted by the same `write_state_prefix`
  recursion (key first, then value), so a `LowCardinality` key's version lands
  at the very front of the column, before the offsets. An all-empty-maps block
  with rows writes the all-zero offsets and nothing else.
- **Variant.** Encode always writes direct Native BASIC mode 0, then each
  alternative's state prefix, the complete global discriminator run, and each
  dense alternative body in canonical order. The input must use the exact flat
  or nested Arrow routing layout derived by `VariantColumn::try_new`; validation
  checks every type id, occurrence offset, child length, and NULL child before
  writing. Alternative state suffixes are traversed in the same order after the
  bodies. A zero-row block writes no mode word or child prefix.
- **Geometry.** Encode delegates to its canonical seven-child Variant and uses
  the exact Variant writer and validation above. The header remains `Geometry`
  or the Custom binary descriptor, while the body is BASIC mode 0,
  discriminators, then dense geo children in canonical order.
- **Dynamic.** A column with SharedVariant writes direct V1 below revision
  54473 or V2 at and above it, the direct type table, BASIC mode 0, the complete
  UInt8 discriminator run, then dense children in canonical global order. A
  column without SharedVariant writes FLATTENED word 3, its typed child table,
  the smallest fixed-width index run, then dense bodies. Validation checks the
  table/order/max-types and every routing/child invariant before either form is
  emitted. A zero-row block writes no Dynamic structure or child prefix.
- **JSON.** A `Text` body writes STRING (structure word 1) then one
  length-prefixed document per row. A structured body writes FLATTENED (word 3)
  ONLY when `EncodeOptions.flattened_dynamic` is set AND the column carries no
  shared pairs (word 3 is unknown to pre-25.6 servers, so it stays opt-in);
  otherwise it writes the revision-selected V1 (< 54473, with the legacy count
  slot) or V2 (>= 54473), including the shared-data `Array(Tuple(String,
  String))` stream. Either structured form writes the sorted dynamic-path list,
  the typed-path bodies in sorted order, then one full Dynamic body per path.
  Validation proves the typed-path set and lengths match the declared type, the
  dynamic paths are strictly sorted and (on V1/V2) within `max_dynamic_paths`,
  and the shared-offset run is well formed, before any bytes are written; a
  shared-bearing column with `flattened_dynamic` set still writes V1/V2 because
  FLATTENED cannot represent shared data. A zero-row block writes no JSON
  structure word or child prefix.

### Round-trip guarantees

- **`decode(encode(x))` reproduces `x`.** Encoding a `ColBatch` and decoding the
  result at the same protocol revision yields the same columns and buffers. This
  is the fidelity the encode tests assert.
- **`encode(decode(server_bytes))` reproduces canonical server bytes.** At the
  pinned revision the server emits canonical output (0/1 for `Bool`, the canonical
  type string, a minimal `LowCardinality` index width), and encode emits the same
  canonical form, so for real server blocks of the supported types the round trip
  is byte-for-byte.
- **Byte-identity is not guaranteed in general.** The decoder is deliberately
  lenient where the encoder is canonical: it accepts any nonzero byte as `Bool`
  true (encode writes `0x01`), accepts non-canonical type strings the server would
  not emit (encode re-renders the canonical string), and ignores unused bits in
  the `LowCardinality` index word (encode writes a fixed set). A non-canonical
  input that decodes correctly can therefore re-encode to different but
  semantically equal bytes. Null-row placeholder values under a `Nullable` are
  passed through verbatim, so they survive any round trip.

### Zero-row encoding

A zero-row block still writes its `BlockInfo` preamble (at revision > 0), the
column and row counts, and, for every column, the name, type string, and
custom-serialization marker (at revision >= 54454). It writes no column data
section at all: no state prefix (not a `LowCardinality` key version, even one
hoisted through an `Array`, a `Tuple`, or a `Map`), no `Array`/`Map` offsets,
no `Tuple()` placeholder bytes, and no body, matching
`NativeWriter::write`'s `rows > 0` gate around `writeData`. A not-yet-encodable
type is still rejected at zero rows (see coverage). This is the write-side
mirror of the "Zero-row output" section below.

---

## Zero-row output

When a block has `num_rows == 0` the chunk is dropped from `chunks`, but the
schema is still established. If a consumer constructs or inspects an empty column
directly (`empty_column` in `src/native/decode/mod.rs`), the empty shapes are:

- Numerics, `Interval*`, and `Bool`: empty value or bit buffer, length 0.
  `IPv4` (a `u32` primitive) is the same.
- `Nothing`: explicit length 0 with no value buffer. `Nullable(Nothing)` also
  retains an empty validity bitmap. Both have `null_count == 0`.
- `BFloat16`: empty `[u8; 2]` values buffer with its distinct BFloat16 logical
  and Column tags.
- `QBit(T, N)`: the dimension is preserved, the parent has length 0 and optional
  empty validity, and its one scalar child has length 0 with the variant selected
  by T.
- `String`: `offsets == [0]` (length 1, the required leading zero) and empty
  data.
- `FixedString(N)`: empty data, width preserved. `UUID` and `IPv6` are the same
  with width 16.
- `Decimal(P, S)`: empty data, width (`bits / 8`), precision, and scale
  preserved, like `FixedString(N)`.
- `Nullable(T)`: as above with an empty validity bitmap.
- `LowCardinality(T)`: empty indices, an empty values dictionary column, and (for
  a nullable inner type) an empty index validity bitmap.
- `Array(T)`: offsets `[0]` (the leading zero only) over an empty element column
  of `T`, built recursively. The same shape stands in for a zero-length
  `LowCardinality` element run nested inside an `Array` (the server's
  `limit == 0` early return writes no LC body at all).
- `Tuple(T1, ...)`: one empty element column per declared element, built
  recursively, with `len == 0`. `Tuple()` is no element columns at all.
  `Nullable(Tuple(...))` carries an empty tuple-level validity bitmap.
- `Map(K, V)`: offsets `[0]` (the leading zero only) over an empty two-field
  entries tuple (an empty keys column and an empty values column, built
  recursively).
- `Variant(T1, ...)`: an empty flat or nested Dense Union layout, one recursively
  empty dense child per alternative, and an empty Null child.
- `Geometry`: the same empty flat Dense Union with seven recursively empty geo
  children and an empty Null child; the logical `Geometry` name stays in schema.
- `Dynamic`: empty u32 type-id and i32 offset buffers, no discovered block-local
  children, and an empty Null child. The logical `max_types` remains in schema.
- Any registered `AggregateFunction` codec: offsets `[0]` and empty state data.

In all cases length is 0 and `null_count` is 0.

---

## Unsupported types

`parse_ch_type` returns no match for any type not listed in the support matrix.
Decoding such a column fails with `DecodeError::UnsupportedType { column,
type_name }` rather than producing a wrong or partial column. There is no silent
fallback. A consumer can treat an unsupported type as a hard decode error.

Not yet supported, tracked as planned phases in `src/schema.rs`:

- `AggregateFunction(...)` state serializers other than exact `count`, canonical
  `nothingUInt64` and `nothingNull`, and base `sum` over the listed plain or
  Nullable numeric/Enum arguments. Native does not length-prefix aggregate
  states or their column, so every additional function needs a confirmed
  boundary codec. Unknown functions are rejected even at zero rows rather than
  accepted with a body the streaming scanner cannot frame once rows appear.

- `LowCardinality(T)` for an inner type outside the allowlist in the
  `LowCardinality(T)` section. The wrapper and its allowed inners (String,
  FixedString, the fixed-width numerics including BFloat16, the wide integers,
  Bool, Date, Date32,
  DateTime, Time, every `Interval*`, UUID/IPv4/IPv6, with or without an inner
  `Nullable`) are
  supported; any other inner is rejected as `UnsupportedType`. This includes
  `Nothing`, `DateTime64`, `Time64`, every `Decimal`, and `Enum8`/`Enum16`, all of which the
  server itself forbids as LC inners (`canBeInsideLowCardinality()` is false), so
  they never appear in that position on the wire. Plain and nullable `Time64`
  columns remain supported.

The containers `Array(T)`, `Tuple(T1, ...)`, and `Map(K, V)` are all fully
supported, decode and encode (see their type sections and the "Encoding"
section). A `Map` header with an illegal key type (`Nullable` or
`LowCardinality(Nullable(...))`) is rejected as `UnsupportedType`, matching
the server's `DataTypeMap::isValidKeyType`.

The name-decoration aliases `SimpleAggregateFunction(func, T)`, the seven geo
types, `Geometry`, and `Nested(name1 T1, ...)` are all fully supported, decode and encode
(see their type sections). They carry no new Column variant or body writer:
every path resolves them to their physical delegate via
`ChType::physical_delegate`. `Nullable`/`LowCardinality` of an alias is legal
only when the physical delegate permits it: `Nullable(Point)` is legal (Point is
a `Tuple`), while `Nullable(Ring)`, `Nullable(Nested(...))`, and
`LowCardinality(Nested(...))` are rejected as `UnsupportedType` (they expand to
`Array`).

A malformed `LowCardinality` payload (a bad key version, the
`NeedGlobalDictionaryBit` set, an index width tag outside `0..=3`, an out-of-range
index, or a row count that disagrees with the block header) fails with
`DecodeError::InvalidLowCardinality` rather than `UnsupportedType`. A malformed
`Array` payload (offsets that decrease or exceed `i64::MAX`) fails with
`DecodeError::InvalidArray` rather than `UnsupportedType`. `Tuple` has the
analogous `DecodeError::InvalidTuple` (unequal element lengths), a defensive
mirror of the server's check that its row-count-driven decode cannot reach.
A malformed `Variant` payload (a nonzero direct-Native mode or a non-255
discriminator outside the alternative range) fails with
`DecodeError::InvalidVariant`.
A malformed `Dynamic` payload (unknown/V3 structure word, non-BASIC direct
mode, illegal/duplicate runtime type, invalid count, or out-of-range index)
fails with `DecodeError::InvalidDynamic` rather than indexing a child or
silently desynchronizing the following column.

When one of these is implemented, move it into the support matrix and add a type
section here.
