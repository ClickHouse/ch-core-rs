# Feature Completeness Tracker

The goal this document tracks: make `ch-core-rs` the complete, authoritative
decoder for ClickHouse `FORMAT Native`. It should decode every type the
ClickHouse server can emit, across server versions. The authority is the server's
own type system, not any one client; `clickhouse-connect` and other clients are
useful cross-checks, but the bar is "every type the server emits," not "what a
given client happens to support." There are two tracks:

- **Decode (read path)** - the primary, near-term track. Support every ClickHouse
  type and the Native wire features needed to read real results. "Done" for a
  type means its wire layout is confirmed against the server source and its
  decoded values are verified against a live server.
- **Encode (insert path)** - a confirmed goal, not yet started. Encode columnar
  input into `FORMAT Native` block bytes for `INSERT`. Sequenced after the decode
  type model is exercised by more types, so the encoder mirrors a stable buffer
  model instead of a moving one.

Server version is a first-class concern. New types are added in specific
ClickHouse releases, and a few types changed wire layout across releases, so what
the decoder must handle depends on the version it is decoding against. See
"Version awareness" below.

The native TCP protocol engine and per-runtime binding adapters are separate and
out of scope here; see "Out of scope" at the bottom and the README roadmap.

This is a living document. Work proceeds as a loop:

1. Read the Context Handoff block. If the user named a target, do that. Otherwise
   take the handoff's recommended next item.
2. Implement it following the per-item definition of done below.
3. Check it off.
4. Rewrite the Context Handoff block so the next agent can continue without
   re-deriving state. This includes recommending the next item and saying why,
   so whoever picks up next has a default even if the user gives no direction.

Keep the handoff block honest and current; it is the first thing a fresh agent
should read. The recommendation is a default, never a constraint: the user can
redirect to any item at any time.

---

## Context Handoff

Rewrite this whole block every time you complete and check off an item. It is a
point-in-time snapshot for the next agent, not a changelog. Keep it short. Always
include a "Recommended next" with a one-line reason, so the next agent has a
default; the user may override it.

- **Last updated:** 2026-07-02 (encode/insert path: **`LowCardinality(T)` encode**
  landed on top of `Decimal(P, S)`, `Enum8`/`Enum16`, `UUID`/`IPv4`/`IPv6`,
  temporal, `Bool` + `Nullable(T)` null map, `String`/`FixedString(N)`, and
  fixed-width numeric encode slices; unit round-trips are green at rev 0 and rev
  54485, plain and nullable; the live INSERT tests cover
  `LowCardinality(String)`, `LowCardinality(Nullable(String))`, and a 260-entry
  `LowCardinality(FixedString(4))` dictionary that exercises the UInt16 index
  path, and **run green** against a 26.6.1.1193 server)
- **Active track:** the **encode/insert path** is now the priority (shifted
  2026-07-01, at the user's direction). Decode type coverage is paused with
  `Array(T)` as its next item. See the "Encode / insert path" section for the
  encode checklist and "Type coverage" for the decode backlog.
- **Pinned server tag (`.server-ref`):** v26.6.1.1193-stable, protocol revision
  **54485**. The crate, the committed fixtures, and the `CODEC_CONTRACT.md`
  citations are all aligned to this pin. The local `.server-src` checkout and the
  running capture server are both 26.6.1.1193.
- **Scope (current completeness bar):** "complete" means decoding `FORMAT Native`
  delivered over **HTTP**, not TCP. The path **assumes uncompressed Native bytes**:
  HTTP `FORMAT Native` does not enable native block-frame compression by default.
  The LZ4/NONE + CityHash128 compressed-block framing in `src/compression/` is
  built and tested but intentionally **unwired** (no caller). See "Out of scope".
- **Last completed:** **`LowCardinality(T)` encode** (`src/native/encode.rs`).
  The encoder now accepts `Column::Dictionary` for every decoded
  `LowCardinality` inner the server permits (`String`, `FixedString`, numeric,
  `Bool`, `Date`, `Date32`, `DateTime`, `UUID`, `IPv4`, `IPv6`, with optional
  inner `Nullable`). For nonzero rows it writes the server-confirmed key-version
  prefix `1`, then index words `0x600..0x603` (`HasAdditionalKeysBit` +
  `NeedUpdateDictionary` + width tag), dictionary size, removeNullable inner body,
  row count, and raw fixed-width indexes. The width tag is self-described and
  server-accepted as long as indexes are in range; this encoder uses UInt8
  through 255 dictionary entries, UInt16 through 65535, then UInt32/UInt64.
  Zero-row LC writes only the column header, no LC prefix or body.
  `LowCardinality(Nullable(T))` preserves the decoded contract:
  dictionary/index 0 is the NULL sentinel, valid rows may not use index 0, and
  NULL rows must use index 0.
- **Build/test status:** Tree builds clean; `cargo test` green (242 unit + 3
  integration, ignored tests skipped); clippy clean
  (`cargo clippy --all-targets -- -D warnings`); fmt clean
  (`cargo fmt --check`). The live INSERT tests in `tests/live_insert.rs` are
  `#[ignore]` and server-gated; they cover `lc LowCardinality(String)`,
  `lcn LowCardinality(Nullable(String))`, and a 260-entry
  `LowCardinality(FixedString(4))` dictionary that exercises UInt16 indexes, and
  **run green** against the localhost 26.6.1.1193 ClickHouse server.
- **Recommended next (encode track):** **Sink-based encode**, exposing the
  existing private `encode_block_into` shape so bindings can write directly into
  a transport buffer and skip an owned `Vec` handoff. If the decode track resumes
  instead, its next item is **`Array(T)`** (see "Type coverage"). Compression
  framing stays implemented but unwired and out of the current HTTP scope.
- **Active gotchas / context:**
  - `Decimal` is NOT a legal `LowCardinality` inner: `DataTypeDecimal` is a
    `DataTypeDecimalBase` subclass whose `canBeInsideLowCardinality()` is false, so
    `LowCardinality(Decimal...)` can never appear on the wire. The crate decodes
    `Decimal` as an ordinary column but `is_low_cardinality_inner` still rejects
    it. Do not add `Decimal` to the LC allowlist.
  - Only the canonical `Decimal(P, S)` spelling is parsed. The creation-time-only
    `Decimal32(S)`..`Decimal256(S)` spellings never appear in a Native header (the
    server normalizes them), so they are NOT parsed and surface as
    `UnsupportedType` if ever seen, mirroring the `Enum8(`/`Enum16(`-only decision.
  - Arrow decimal export emits the native-width format string `d:P,S` (128-bit) /
    `d:P,S,bits` (32/64/256-bit), deliberately NOT widening narrow decimals to 128
    (that would cost a per-value copy in the decode loop). `decimal32`/`decimal64`/
    `decimal256` are newer in the Arrow C Data Interface than `decimal128`, so
    consumer support for the non-128 widths varies; the `arrow-ffi-specialist`
    should confirm the exact format-string spelling and downstream consumer
    compatibility before any binding relies on the narrow/256-bit exports.
  - `Enum` is NOT a legal `LowCardinality` inner: `DataTypeEnum` does not inherit
    `DataTypeNumberBase`, so `canBeInsideLowCardinality()` is false and the server
    throws `ILLEGAL_TYPE_OF_ARGUMENT` on `LowCardinality(Enum...)` at construction.
    The crate decodes `Enum8`/`Enum16` as ordinary columns but `is_low_cardinality_inner`
    still rejects them, so a (never-emitted) `LowCardinality(Enum...)` surfaces as
    `UnsupportedType`. Do not add `Enum` to the LC allowlist.
  - V1 done (v26.6.1.1193-stable): the Tier lists are authoritative against
    `DataTypeFactory`. `Geometry` and `QBit(T, N)` are registered and GA in Tier 3;
    `QBit`'s `SerializationQBit` wire layout is still unexamined - read it via the
    `clickhouse-server-reader` sub-agent before implementing `QBit`. Legacy
    `Object('json')` does not exist at this pin; do not implement it.
  - Per-type introduction versions are mostly undetermined from the local shallow
    `.server-src` checkout (its `CHANGELOG.md` records behavior changes, not
    original introduction). Record a type's introduction version only when it is
    determinable from the source; do not guess from memory.
  - `LowCardinality` inner support is gated by `is_low_cardinality_inner`
    (`src/native/decode.rs`), the `canBeInsideLowCardinality()` allowlist
    intersected with decoded types. It now includes `UUID`/`IPv4`/`IPv6`. Both
    `decode_low_cardinality_dictionary` and the scan's `skip_low_cardinality_data`
    consult it, so decode and `block_end` agree on which columns are accepted. The
    `allow_suspicious_low_cardinality_types` setting is a server-side creation
    guard only (needed at creation for the numerics, temporals, and `IPv4`/`IPv6`,
    not for `UUID`) and does not affect decoding.
  - The per-column bulk-state prefix is generalized (`read_state_prefix`).
    `LowCardinality` reads its 8-byte key version through it; every other type
    reads zero bytes. Later types with a real prefix (Array, Map) declare it
    there. Note the prefix is emitted per column per block in Native and only
    when the block has rows, not once per column overall.
- **Key references:** per-type workflow is in `AGENTS.md` ("Adding A New
  ClickHouse Type"). Output contract is `CODEC_CONTRACT.md`. Deferred
  decisions are in `FINDINGS.md`. Type enum and planned placeholders are in
  `src/schema.rs`.

---

## Version awareness

The ClickHouse type system grew across releases, so a complete decoder spans many
versions. This has concrete consequences for how work is done here:

- **The pinned tag bounds what is confirmable now.** `.server-ref` pins the
  reference version (currently v26.6.1.1193-stable) and the local `.server-src/`
  checkout is at that tag. A type that exists at the pin can have its layout
  confirmed against the source today. A type introduced in a *later* release than
  the pin cannot be confirmed against the current checkout. Bump `.server-ref`
  first (switch in place per `AGENTS.md`), then confirm. Do not implement a newer
  type's layout from memory or blog posts.
- **Record each type's introduction version.** As a type is implemented, record
  the ClickHouse version it first appeared in, confirmed against the server
  source or changelog, not asserted from memory. This goes in the type's
  `CODEC_CONTRACT.md` section and lets bindings reason about what a given server
  can emit.
- **Some layouts are version dependent.** A few types changed wire serialization
  across releases, and the newest self-describing types are still evolving. Where
  layout depends on version or negotiated protocol revision, the decoder branches
  on `DecodeOptions.protocol_revision` (already plumbed) or refuses a revision it
  has not confirmed, rather than guessing. Type availability (which version added
  a type) and `protocol_revision` (which gates block framing and serialization
  markers) are separate axes; both are "version" concerns and both matter.
- **Experimental vs stable.** Newer types may be experimental at the pin. Note
  the status, since experimental wire formats are the ones most likely to change
  under you on a later tag.

---

## Definition of done for one type

This mirrors the "Adding A New ClickHouse Type" workflow in `AGENTS.md`. A type
is not done, and must not be checked off, until all of these hold:

1. Wire layout confirmed against the server source at the pinned tag, via the
   `clickhouse-server-reader` sub-agent. Confirmed vs inferred is recorded. If the
   type does not exist at the pin, bump `.server-ref` first (see "Version
   awareness").
2. Introduction version recorded (the ClickHouse release the type first appeared
   in), plus whether its layout is version dependent or experimental at the pin.
3. `ChType` variant added or enabled in `src/schema.rs`, with `Display`
   round-tripping the canonical type name.
4. `parse_ch_type` in `src/native/decode.rs` parses the type string.
5. Wire bytes decode into a `Column` variant in `src/column.rs`, Arrow-shaped.
6. Arrow format string and buffer export added in `src/ffi.rs`.
7. Unit tests using the `BlockBuilder` pattern: plain, `Nullable`, zero-row, and
   at least one multi-block case.
8. Live-server coverage: extend the `all_types` query in
   `scripts/gen_fixtures.sh`, recapture committed `.native` bytes against a
   server matching `.server-ref`, and assert decoded values in
   `tests/integration.rs`.
9. `CODEC_CONTRACT.md` updated: move the type from "Unsupported types" into the
   support matrix and add its type section (wire payload, Arrow export, Rust
   buffer, server reference, introduction version).
10. `cargo test` and `cargo clippy --all-targets` clean.

---

## Implemented

- [x] Native block decode from a complete buffer (`decode_all_bytes`).
- [x] Incremental/streaming decode from byte chunks (`StreamDecoder`).
- [x] Block framing: `BlockInfo` preamble (field-tagged), revision-gated.
- [x] Per-column custom-serialization marker detection (default accepted,
      nonzero rejected rather than misread).
- [x] Cross-block schema consistency enforcement.
- [x] Malformed-input hardening (no panics on untrusted bytes; bounded
      allocations).
- [x] Arrow C Data Interface export (schema, array, stream).
- [x] `Bool` / `Boolean`
- [x] `Int8`, `Int16`, `Int32`, `Int64`
- [x] `UInt8`, `UInt16`, `UInt32`, `UInt64`
- [x] `Float32`, `Float64`
- [x] `String`
- [x] `FixedString(N)`
- [x] `UUID`, `IPv4`, `IPv6`
- [x] `Date`, `Date32`, `DateTime`, `DateTime64(P[, tz])`
- [x] `Nullable(T)` over every supported inner type
- [x] Per-column bulk-state prefix (generalized; `LowCardinality` is the first
      non-empty prefix)
- [x] `LowCardinality(T)` for every allowed inner type this crate decodes:
      `String`, `FixedString(N)`, the fixed-width numerics, `Bool`, `Date`,
      `Date32`, `DateTime`, `UUID`, `IPv4`, `IPv6`, each also in the inner
      `Nullable` form. `DateTime64`, `Decimal`, and `Enum8`/`Enum16` are excluded:
      the server forbids all of them as LC inners (`canBeInsideLowCardinality()`
      is false), so they never appear in that position on the wire. This holds for
      `Enum` independent of decode support; the crate now decodes `Enum8`/`Enum16`
      as ordinary columns but still rejects them as LC inners.
- [x] `Enum8`, `Enum16`
      - raw `Int8`/`Int16` on the wire; the name->value map is carried in the
        `ChType` (`variants`), not in the Column or the per-row data. Exported as
        Arrow `c`/`s` (the underlying signed int), zero-copy. Forbidden as a
        `LowCardinality` inner by the server. Confirmed against the server source
        and verified with the `e8`/`e16` live-server fixture columns.
- [x] `Decimal(P, S)` (`Decimal32`/`Decimal64`/`Decimal128`/`Decimal256`)
      - raw little-endian two's-complement fixed-width integer per row (4/8/16/32
        bytes by precision); precision and scale are in the `ChType`
        (`Decimal { precision, scale, bits }`), the byte width derived from P, not
        in the per-row data. The server always emits the canonical `Decimal(P, S)`
        spelling, so only that is parsed. Decode is a host-agnostic passthrough
        into a contiguous `DecimalColumn` (no native `i128`/`i256`). Exported as
        Arrow `d:P,S` (128-bit) / `d:P,S,bits` (32/64/256-bit), zero-copy, native
        width. Forbidden as a `LowCardinality` inner. Confirmed against the server
        source (`SerializationDecimalBase`/`DataTypesDecimal`) and verified with
        the `dec32`/`dec64`/`dec128`/`dec256` live-server fixture columns.

---

## Type coverage

The target is every type the server can emit. Ordering below is by how common the
type is in real schemas and by dependency (wrappers and containers unlock later
entries), not by whether it is in scope; everything here is in scope. Follow the
per-type definition of done for each, including recording its introduction
version. V1 (done at v26.6.1.1193-stable, via the `clickhouse-server-reader`
sub-agent reading `DataTypeFactory`) made this list authoritative against the
server source: every entry below is confirmed registered at the pin unless noted
otherwise. Introduction versions are mostly undetermined from the local shallow
checkout (the bundled `CHANGELOG.md` records behavior changes, not original
introduction), so record them per type only when determinable.

### Tier 1 - common, decode these first

- [x] `LowCardinality(T)` - dictionary + index framing; has a real bulk-state
      prefix. Decoded for every inner type this crate already decodes that
      ClickHouse permits inside `LowCardinality`: `String`, `FixedString(N)`, the
      fixed-width numerics, `Bool`, `Date`, `Date32`, `DateTime`, each also in the
      inner `Nullable` form. Exported as Arrow `dictionary(i32, V)` where `V` is
      the inner value type's format. The dictionary values defer to the shared
      per-type body decoder (`decode_column_body`), so the allowlist is just
      `IDataType::canBeInsideLowCardinality()` intersected with the decoded types.
      `DateTime64`, `Decimal`, and `Enum8`/`Enum16` reject because the server
      forbids them as LC inners (`canBeInsideLowCardinality()` is false), so they
      never appear in that position on the wire; this is independent of decode
      support, and the crate decodes all three as ordinary columns while still
      rejecting them as LC inners. `UUID`/`IPv4`/`IPv6` are decoded so their LC
      forms decode too. Confirmed against the server source and verified with
      live-server fixtures (`lc`, `lcn`, `lc_u32`, `lc_date`, `lcn_u32`,
      `lc_uuid`).
- [x] `Enum8(...)`, `Enum16(...)` - raw `Int8`/`Int16` on the wire (the name
      list is in the type string only, never in per-row data). The core carries
      the name->value map in `ChType` (`variants`, in the server's emitted
      ascending-by-value order); the Column holds only the physical signed-int
      buffer. Exported as Arrow `c`/`s`, zero-copy. The type-string parser is
      quote-aware (a name can contain `,`/`=` unescaped) and unescapes the
      server's set. Forbidden as a `LowCardinality` inner. Confirmed against the
      server source (`SerializationEnum`/`DataTypeEnum`) and verified with the
      `e8`/`e16` live-server fixture columns.
- [x] `UUID` - 16 raw bytes, a POD dump of the UInt128 (NOT RFC-4122 order).
      Decode is raw passthrough into a width-16 `FixedBinary`; the wire->RFC
      mapping is documented for bindings. Arrow `w:16`. Also a legal
      `LowCardinality` inner (decoded through the dictionary path). Confirmed at
      v26.6.1.1193-stable; verified with the `uuid` and `lc_uuid` fixtures.
- [x] `IPv4` - UInt32 on the wire (standard IPv4 numeric value), decoded as
      `Column::Ipv4(PrimitiveColumn<u32>)`. Arrow `I` (uint32), zero-copy. Legal
      `LowCardinality` inner. Confirmed at v26.6.1.1193-stable; verified with the
      `ipv4` fixture.
- [x] `IPv6` - 16 raw bytes in network byte order, decoded as a width-16
      `FixedBinary` (`Column::Ipv6`), passthrough. Arrow `w:16`. Legal
      `LowCardinality` inner. Confirmed at v26.6.1.1193-stable; verified with the
      `ipv6` fixture.
- [x] `Decimal(P, S)` / `Decimal32`/`Decimal64`/`Decimal128`/`Decimal256` -
      raw little-endian two's-complement fixed-width signed integers (4/8/16/32
      bytes by precision) with precision and scale as `ChType` metadata. The
      server always emits the canonical `Decimal(P, S)` type string (never
      `Decimal32(S)` etc.), so only that spelling is parsed and the byte width is
      derived from P (1..=9 -> 32, 10..=18 -> 64, 19..=38 -> 128, 39..=76 -> 256).
      Decode is a host-agnostic passthrough into a contiguous `DecimalColumn`
      (the same physical buffer as a `FixedSizeBinary` of width `bits/8`), so the
      core needs no native `i128`/`i256`. Exported as Arrow `d:P,S` (128-bit) or
      `d:P,S,bits` (32/64/256-bit), zero-copy, native width (no widening).
      Forbidden as a `LowCardinality` inner (`canBeInsideLowCardinality()` is
      false on `DataTypeDecimalBase`). Confirmed against the server source
      (`SerializationDecimalBase`/`DataTypesDecimal`) and verified with the
      `dec32`/`dec64`/`dec128`/`dec256` live-server fixture columns.
- [ ] `Array(T)` - offsets stream (cumulative `UInt64`) plus the inner column;
      first nested type, recurse into `T`.
- [ ] `Map(K, V)` - serialized as `Array(Tuple(K, V))`; depends on `Array` and
      `Tuple`.
- [ ] `Tuple(T1, ...)` / named tuples - one nested column per element.

### Tier 2 - less common, still in scope

- [ ] `Int128`, `UInt128`, `Int256`, `UInt256` - 16/32-byte little-endian
      integers; decide host representation policy at the binding, not here.
- [ ] `Nested(...)` - sugar over `Array(Tuple(...))`; confirm whether the server
      ever emits the `Nested` type string on the wire or always the expanded
      form.
- [ ] `SimpleAggregateFunction(func, T)` - decodes as the inner `T` on the wire;
      mostly a type-string parsing concern.
- [ ] Geo types: `Point`, `Ring`, `LineString`, `MultiLineString`, `Polygon`,
      `MultiPolygon` - custom-serialization aliases over `Tuple`/`Array` of
      `Float64` (`Point` = `Tuple(Float64, Float64)`, the rest nest `Array` over
      it); come almost for free once containers land, but need type-string parsing.
      Confirmed registered and stable at v26.6.1.1193-stable (the
      `allow_experimental_geo_types` gate is now an obsolete no-op). The umbrella
      `Geometry` type (= `Variant(...)` of the six, alias `GEOMETRY`) is in Tier 3
      because it depends on `Variant`.
- [ ] `Interval*` (`IntervalYear` ... `IntervalNanosecond`) - Int64 on the wire;
      11 distinct simple types, one per kind. Confirmed at v26.6.1.1193-stable.
- [ ] `Nothing` - the type of a bare `NULL`; zero-width, edge case.
- [ ] `BFloat16` - 2-byte float, the top 16 bits of an IEEE-754 `Float32` (sign +
      8-bit exponent + 7-bit truncated mantissa), serialized raw little-endian via
      `SerializationNumber<BFloat16>` with no per-row framing. Confirmed registered
      and stable at v26.6.1.1193-stable (`registerDataTypeNumbers`; the
      `allow_experimental_bfloat16_type` gate is now an obsolete no-op).
- [ ] `Time` - 4-byte little-endian signed `Int32` of seconds, can be negative
      (range [-999:59:59, 999:59:59]); no timezone (the type rejects a tz arg).
      `SerializationTime` extends `SerializationNumber<Int32>` with no binary-bulk
      override. Confirmed registered and stable at v26.6.1.1193-stable.
- [ ] `Time64(P)` - 8-byte little-endian signed `Int64` of ticks scaled 10^-P,
      P in 0..=9 (default 3); no timezone. Wire layout identical to `DateTime64`
      (`SerializationTime64` extends `SerializationDecimalBase<Time64>`). Confirmed
      registered and stable at v26.6.1.1193-stable. The `enable_time_time64_type`
      setting (default true) gates only CREATE TABLE column creation, not
      query-result Native streams, so a `Time`/`Time64` column can appear on the
      wire regardless of the setting.

### Tier 3 - advanced / newest type system; hardest, do last

In scope, but high effort and the most version-sensitive: these are the newest
types and their wire formats are still evolving across releases, so confirm the
exact layout at the pinned tag before implementing. V1 found all of these are GA
(not experimental) at v26.6.1.1193-stable: every `allow_experimental_*` gate that
once covered them is now an obsolete no-op. None of their `Serialization*` classes
branch on the protocol revision, but the self-describing types (`Variant`,
`Dynamic`, `JSON`) carry their own in-band version/structure headers, which is
where the across-release churn lives.

- [ ] `AggregateFunction(...)` - opaque aggregation state; large surface, decode
      fidelity is hard.
- [ ] `Variant(...)` - discriminator stream plus per-variant columns.
- [ ] `Dynamic` - self-describing, carries its own type info; optional
      `Dynamic(max_types=N)` form.
- [ ] `JSON` (new object type) - dynamic subcolumns, carries its own structure
      header; highest effort. Confirmed registered (case-insensitive) and GA at
      v26.6.1.1193-stable. The legacy `Object('json')` spelling is **not registered
      at this pin** (only the obsolete `allow_experimental_object_type` setting
      remains as a vestige), so there is nothing to decode against here; do not
      implement it.
- [ ] `Geometry` - `Variant(Point, LineString, MultiLineString, Polygon,
      MultiPolygon, Ring)`, alias `GEOMETRY`. Depends on `Variant` plus the geo
      aliases. Confirmed registered and GA at v26.6.1.1193-stable.
- [ ] `QBit(T, N)` - quantized bit-packed vector type, parametric over a
      `BFloat16`/`Float32`/`Float64` element type and a dimension count. GA at
      v26.6.1.1193-stable (the `allow_experimental_qbit_type` gate is now an
      obsolete no-op; CHANGELOG confirms the GA transition). Wire layout
      (`SerializationQBit`) is NOT yet examined - needs a dedicated
      `clickhouse-server-reader` read of `SerializationQBit.cpp` before tiering it
      for implementation.

---

## Wire / protocol features for the decode path

- [~] Native block compression framing - LZ4 + NONE codecs and the CityHash128
      checksum are implemented and tested in `src/compression/`
      (`decompress_native_stream`), hand-rolled to keep the crate zero-dependency.
      They are **not wired into the decode path**: the current scope is HTTP
      `FORMAT Native`, which does not enable native block-frame compression by
      default (see "Scope" in the handoff and "Out of scope" at the bottom), so
      the decoder assumes uncompressed input. ZSTD (0x90) is rejected pending a
      dependency discussion (raise with the user, do not add silently). Wire this
      in only when the `compress=1` / TCP path enters scope. Distinct from HTTP
      transport compression (gzip/zstd), which stays in the bindings.
- [ ] Sparse column serialization - the nonzero custom-serialization marker the
      decoder currently rejects. Needed wherever the server emits sparse columns.
- [x] Multiple-stream bulk-state prefix - the per-column read is generalized via
      `read_state_prefix` in `src/native/decode.rs`, so a type with a real
      `deserializeBinaryBulkStatePrefix` declares its prefix in one place instead
      of the old "prefix reads zero bytes" assumption. `LowCardinality` reads its
      8-byte key version through it; every other type reads zero bytes. The
      prefix runs per column per block, gated on the block having rows, matching
      `NativeReader::readData`.
- [ ] Binary-encoded type headers (`DataTypesBinaryEncoding`) - only emitted when
      `output_format_native_encode_types_in_binary_format` is set. Conditional on
      whether any target binding needs it; see `FINDINGS.md`. May stay a
      documented constraint rather than a feature.

---

## Encode / insert path

Now in active development (priority shifted here 2026-07-01). The aim is to turn
columnar input, the same `Column`/`ColBatch` model the decoder produces, into
`FORMAT Native` block bytes the server accepts for `INSERT`. Lives in
`src/native/encode.rs` (`encode_block`, `encode_chunked`, `EncodeOptions`,
`EncodeError`), the mirror of `src/native/decode.rs`. Framing is the exact inverse
of the decoder, confirmed against `NativeWriter::write`, `BlockInfo::write`, and
`NativeInputFormat` at v26.6.1.1193-stable via the `clickhouse-server-reader`
sub-agent. Encode coverage is kept a subset of decode coverage: any type or
`Nullable` wrapper the encoder does not handle yet returns
`EncodeError::UnsupportedType` rather than emitting wrong bytes. The encode-side
contract (API, trust and error model, input preconditions, encoder choices,
round-trip guarantees) is documented in the "Encoding" section of
`CODEC_CONTRACT.md`; keep it current as encode coverage grows.

- [x] Native block framing for writes: optional `BlockInfo` preamble (rev > 0),
      column/row counts, per-column name + type string (`ChType::Display`) +
      custom-serialization marker byte (rev >= 54454, 0 = default), revision gated
      to match what `decode_next_block` reads. HTTP `INSERT ... FORMAT Native` is
      parsed at server_revision 0, so encode at `protocol_revision = 0` for HTTP;
      the stream ends at EOF, no trailing empty block (that terminator is
      TCP-only, out of the current HTTP scope).
- [x] Encode fixed-width primitives (bulk little-endian write, big-endian
      fallback), the inverse of the `decode_primitive!` path: `Int8`..`Int64`,
      `UInt8`..`UInt64`, `Float32`, `Float64`.

Remaining types, in dependency/difficulty order. This is the current worklist:
bring encode to parity with what the decoder already supports.

- [x] `String` (per-value varint length prefix + raw bytes, walked from the
      offsets+data buffer) and `FixedString(N)` (contiguous `width * num_rows`
      bytes straight from the `FixedBinaryColumn` data buffer; the buffer `width`
      must equal the declared `N` or it is an `InconsistentBatch`). Both are the
      exact inverse of `decode_string_data`/`decode_fixed_binary_data`, no per-row
      allocation. Verified by in-crate round-trip tests (rev 0 and rev 54485, plus
      exact-byte framing pins) and the live-server INSERT test.
- [x] `Bool` (one byte per row, 0/1, the inverse of `BoolColumn::from_wire_bytes`
      unpacking the Arrow bitmap) and the `Nullable(T)` null map (one byte per row
      from the validity bitmap, the inverse of `Bitmap::from_ch_null_map`, written
      before the inner values). `Nullable` is a wrapper (an unwrap in
      `encode_column_data` that writes the null map, then defers to
      `encode_column_body` for the inner type), so it composes with every inner
      type already encodable. A `Nullable` whose validity bitmap does not cover
      `num_rows` is rejected as `InconsistentBatch` in the pre-write validation. A
      `Nullable` over a not-yet-encodable inner still returns
      `EncodeError::UnsupportedType`. Verified by round-trip unit tests (rev 0 and
      rev 54485), exact-byte framing pins (`rev0_frames_bool_bytes`,
      `rev0_frames_nullable_bytes`), and the live-server INSERT test.
- [x] Temporal: `Date` (u16), `Date32` (i32), `DateTime` (u32), `DateTime64`
      (i64). Each is a new `(ch_type, column)` arm in `encode_column_body` running
      the existing `encode_primitive!` over the matching
      `Column::Date`/`Date32`/`DateTime`/`DateTime64` buffer at its native
      little-endian width, the exact inverse of the `decode_primitive!` arms.
      Timezone and precision live only in the type string (`ChType::Display`),
      never in the per-row data, so nothing else is needed; `is_encodable` gained
      all four. The `Nullable(T)` wrapper composes for free via `encode_null_map`,
      so `Nullable(DateTime64(3, 'UTC'))` and friends encode with no extra arm.
      Verified by round-trip unit tests (rev 0 and rev 54485, plain and nullable,
      hitting each width's boundaries plus a negative pre-epoch value for the signed
      `Date32`/`DateTime64`), by `assert_batches_eq` extended to compare the four
      temporal columns, and by the live-server INSERT test (`d Date`, `d32 Date32`,
      `dt DateTime('UTC')`, `dt64 DateTime64(3, 'UTC')` columns; the current full
      live INSERT test now runs green against a 26.6.1.1193 server). Reviewed clean
      by the `rust-reviewer` and `codex-reviewer`; the one non-blocking note is that
      a `DateTime64` precision > 9 is constructible in memory and would render a
      type string the parser and server reject (pre-existing, encode input is
      trusted in-memory data, left out of scope).
- [x] `UUID`/`IPv6` (raw 16-byte-per-row passthrough straight from the
      `FixedBinaryColumn` data buffer, one `extend_from_slice`, NO reordering:
      UUID stays in its wire UInt128 POD order, IPv6 in network byte order; the
      RFC-4122 / host-address mapping is a binding concern on both directions)
      and `IPv4` (u32 LE primitive through `encode_primitive!`, exactly like
      `UInt32`). The exact inverse of the decoder's `Uuid`/`Ipv6`
      `decode_fixed_binary_data` passthrough and `Ipv4` `decode_primitive!` arms.
      `validate_column` guards `UUID`/`IPv6` through the shared
      `validate_fixed_binary` helper (also used by `FixedString`, width fixed at
      16 here): a stored width other than 16 or a data buffer that is not
      exactly `16 * num_rows` bytes is `InconsistentBatch` before any bytes are
      written. `is_encodable` gained all three; the `Nullable(T)` wrapper
      composes for free via `encode_null_map`. Verified by round-trip unit tests
      (rev 0 and rev 54485, plain and nullable), exact-byte framing pins for all
      three (`rev0_frames_uuid_bytes` and `rev0_frames_ipv6_bytes` with 16
      distinct bytes so any reordering breaks them, `rev0_frames_ipv4_bytes`
      proving the LE u32 byte order), a zero-row block and a two-block
      `encode_chunked` round-trip, rejection tests for the width/ragged guards,
      `assert_batches_eq`
      extended to the three variants, and the live-server INSERT test extended
      with `u UUID`, `ip4 IPv4`, `ip6 IPv6`, and `nu Nullable(UUID)` columns
      (the current full live INSERT test now runs green against a 26.6.1.1193
      server).
      The two "unsupported type" unit tests that used `UUID` as the
      decoded-but-not-encodable example were swapped to `Enum8`.
- [x] `Enum8`/`Enum16` (raw underlying `Int8`/`Int16`; the name->value map lives
      in the type string, which `ChType::Display` already renders). Two
      `encode_primitive!` arms in `encode_column_body` over
      `Column::Enum8`/`Column::Enum16` at i8/i16 LE, the exact inverse of the
      decode arms; `column_variant_matches` and `is_encodable` gained both, so
      `Nullable(Enum8/16)` composes for free via `encode_null_map`. No server
      read or new fixtures needed (byte-identical to the confirmed `Int8`/`Int16`
      layout). The enum map is not semantically validated on encode (an empty or
      duplicate variant list, or a per-row value outside the declared set, is the
      server's call on INSERT, the same trusted-input boundary as a `DateTime64`
      precision above 9); documented at the arms. Verified by round-trip unit
      tests (rev 0 and rev 54485, plain and nullable, sign and width boundaries),
      the then-swapped unsupported-type tests, and a passing live-server INSERT
      of `e8 Enum8(...)` / `e16 Enum16(...)` columns against 26.6.1.1193.
      Reviewed clean by
      `rust-reviewer` and `codex-reviewer`.
- [x] `Decimal(P, S)` (contiguous fixed-width LE two's-complement bytes straight
      from the `DecimalColumn` data buffer, the same shape as `FixedString`).
      `column_variant_matches` and `is_encodable` include Decimal, and
      `encode_column_body` writes the whole data buffer verbatim. Validation
      rejects precision/scale metadata mismatches, width mismatches, and ragged
      `data.len() != width * num_rows` bodies before writing; the same guard runs
      inside `Nullable(Decimal(...))`. Verified by rev 0 and rev 54485
      round-trips across 32/64/128/256-bit widths, nullable Decimal round-trips,
      exact-byte framing with a negative Decimal32 value, zero-row schema and
      multi-block `encode_chunked` round-trips, rejection tests for all Decimal
      guards, and the live INSERT test extended with all four widths plus
      `Nullable(Decimal(18, 9))` and run green against a 26.6.1.1193 server.
- [x] `LowCardinality(T)` (write-side bulk-state prefix + dictionary/index
      framing). Writes key version 1, server-native index words
      `0x600..0x603`, dictionary size, removeNullable inner body through the
      existing inner encoder, row count, and raw UInt8/16/32/64 indexes. Zero-row
      LC writes no prefix/body. Validation rejects forbidden inners
      (`Decimal`, `DateTime64`, `Enum`), wrong dictionary value variants, negative
      or out-of-range indexes, non-empty dictionaries on zero-row blocks, and
      nullable-LC rows that violate the index-0 NULL sentinel rule. Covered by
      rev 0 / rev 54485 round-trips, exact-byte pins, zero-row and multi-block
      tests, rejection tests, and live INSERT tests against localhost, including
      String, nullable String, and a 260-entry FixedString dictionary that
      exercises UInt16 indexes.
- [x] Round-trip tests (encode then decode equals the original buffers) for the
      numerics, plus a live-server `INSERT` acceptance test
      (`tests/live_insert.rs`, `#[ignore]`, curl over HTTP). Extend both as each
      new type lands.

**Encode per-type workflow (differs from the decode "Adding A New ClickHouse Type"
flow in `AGENTS.md`).** The wire layout is already confirmed for the decode
direction and encode is its exact inverse, so no `clickhouse-server-reader` read
and no new committed fixtures are needed (the exceptions are the wrappers, where
the framing detail matters: see `LowCardinality`). For a plain, non-wrapper type:

1. Add a `(ChType, Column)` arm in `encode_column_data` (`src/native/encode.rs`)
   that writes the inverse of that type's decoder in `src/native/decode.rs`. Keep
   the pair-match so the on-wire type string (from `ChType::Display`) and the body
   (from the `Column` buffer) can never diverge.
2. Add a round-trip unit test in `encode.rs`: build a `ColBatch`, `encode_block`,
   `decode_all_bytes`, assert equality, at rev 0 and rev 54485, plus the nullable
   form once the null map lands.
3. Add a column to `tests/live_insert.rs` so the live server confirms it accepts
   and round-trips the bytes (`cargo test --test live_insert -- --ignored`).
4. `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`.

Wrappers (`Nullable`, `LowCardinality`) touch the framing (the null map, the
bulk-state prefix, the dictionary/index streams), not just a `Column` arm. Keep
encode coverage a subset of decode coverage: a type or wrapper the encoder cannot
yet write must keep returning `EncodeError::UnsupportedType` (or
`InconsistentBatch` for a type-string/buffer mismatch), never emit wrong bytes.

### Streaming encode and the encode-push overlap (assessment 2026-07-01, nothing to build yet)

The overlap a binding wants on the insert path: encode block N+1 while pushing
block N's bytes to the server, the mirror of how the query-side binding overlaps
decoding block N with reading block N+1 off the socket.

**This overlap is already achievable today with `encode_block`; no new core API is
required to enable it.** `encode_block(&ColBatch)` is a synchronous, stateless,
per-block call that returns owned bytes. The binding gets the overlap by running
the two stages concurrently on its own threads/tasks (threading stays a binding
concern; the core is zero-dep, synchronous, and does no I/O):

```
encode(batch_1)  encode(batch_2)  encode(batch_3)   <- CPU
                 push(bytes_1)    push(bytes_2)      <- network
```

It double-buffers: encode N+1 while pushing N. `encode_block` returning an owned
`Vec` is convenient here, since the binding can hand that ownership straight to the
send task.

**Why no streaming encoder is needed for the overlap (the decode asymmetry).**
`StreamDecoder` is load-bearing on the read side ONLY because input arrives as an
opaque byte stream whose block boundaries are unknown until parsed, so the binding
needs the accumulation buffer plus the `block_end` completeness scan to be handed
complete blocks as bytes dribble in. The encoder is the producer: it already holds
whole `ColBatch`es, so the pipelining unit is in hand and `encode_block` per block
is directly usable. There is no fragmentation, no boundary detection, and no
partial block to reassemble, so there is nothing on the encode side to mirror.

**`encode_chunked` is the anti-pattern for streaming.** It materializes every
block into one buffer, so all encoding must finish before any bytes can be sent.
Use `encode_block` per block for the streaming/overlap path; keep `encode_chunked`
for the "I already hold the whole result and just want the bytes" case.

**Block granularity is the binding's lever.** `encode_block` does not split a large
batch internally, by design: the producer chooses block size by how it slices its
source into `ColBatch`es, exactly as the server chooses it on the query side via
`max_block_size`. Overlap granularity equals block size.

Potential future work, in value order. None of it is required for the overlap:

- [ ] **Sink-based encode** (the one modest, real win for the overlap path): write
      a block straight into a caller-provided `&mut Vec<u8>` / `impl io::Write`
      instead of returning an owned `Vec`, so a binding can encode directly into
      its transport send buffer and skip an allocation plus copy per block. The
      private `encode_block_into(&mut Vec<u8>, ...)` already does exactly this
      internally; exposing it (or an `io::Write` variant) is a few lines.
- [ ] **`StreamEncoder` (ergonomics and safety, NOT overlap).** A thin feed/finish
      state machine over `encode_block`: pin the first fed batch's schema and
      reject a later mismatch (the mirror of `StreamDecoder`'s
      `BlockSchemaMismatch`), use `finish()` as the hook for the TCP trailing
      empty-block terminator (a no-op for HTTP), and give it a feed/finish shape
      symmetric with `StreamDecoder` so bindings use one mental model both
      directions. Note the fragmentation asymmetry: encode `feed(batch)` yields
      exactly one block's bytes, whereas decode `feed(chunk)` can yield several
      batches. Sequence it after type coverage catches up, or whenever a binding
      needs the schema guard or the TCP terminator.

## Policy decisions to resolve

These are decisions, not implementations. Resolve with the user, then record the
outcome in `CODEC_CONTRACT.md`.

- [ ] `String` export as Arrow `u` (Utf8) vs `z` (Binary), or a binding-selected
      option. ClickHouse `String` is arbitrary bytes; current export is `u`. See
      `FINDINGS.md`.
- [ ] Numeric effective-protocol-revision API. The Python POC used a stale
      `has_block_info` boolean; the core now takes `protocol_revision: u64`. Any
      production binding must compute/expose the numeric revision. See
      `FINDINGS.md`.
- [ ] Server version as a decode input. `protocol_revision` gates block framing,
      but a few types' layouts are tied to the ClickHouse release, not the
      revision. Decide whether `DecodeOptions` should also carry the negotiated
      server version (or a per-type capability set) so version-dependent layouts
      can branch, and how a binding supplies it. Resolve when the first
      version-dependent type lands.

---

## Validation / meta

- [x] **V1: Enumerate the authoritative type set from the server source** and
      reconcile this tracker against it. Done at v26.6.1.1193-stable via the
      `clickhouse-server-reader` sub-agent reading `DataTypeFactory` (25
      `registerDataType*` functions). Outcome: every ALREADY DECODED and Tier 1/2/3
      entry is confirmed registered at the pin and not mis-stated, with these
      corrections folded into the tiers above:
      (1) the "(confirm at pin)" flags are resolved - `BFloat16`, `Time`, and
      `Time64` all exist and are stable, with wire layouts recorded;
      (2) two registered types were missing from the tiers and have been added to
      Tier 3 - `Geometry` (= `Variant(...)`, alias `GEOMETRY`) and `QBit(T, N)`
      (GA; its `SerializationQBit` wire layout is still unexamined, flagged before
      tiering for implementation);
      (3) legacy `Object('json')` is NOT registered at this pin (only the obsolete
      `allow_experimental_object_type` setting remains) - marked do-not-implement;
      (4) every `allow_experimental_*` gate for the Tier 2/3 newest types
      (`BFloat16`, `Variant`, `Dynamic`, `JSON`, geo, `QBit`) is now an obsolete
      no-op, so none are experimental at the pin;
      (5) no Tier `Serialization*` class branches on the protocol revision; the one
      revision-gated wire detail is the per-column custom-serialization marker
      (already implemented, gated at `DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION`
      = 54454).
      Introduction versions are mostly undetermined from the local shallow checkout
      and were not guessed. No type newer than the pin was found. A
      `clickhouse-connect` cross-check remains optional and was not done.
- [ ] **V2: End-to-end coverage harness.** A query whose result mixes every
      supported type, decoded by this core, with values asserted against the live
      server (the server is the oracle; the captured fixtures are ground truth).
      Optionally cross-check against `clickhouse-connect`. Grows as types are
      checked off; it is the real "feature complete" gate.
- [~] **V3: Re-confirm on server-tag bumps.** Ongoing item, re-run on every
      `.server-ref` move: re-confirm the per-type server-source layouts, re-check
      introduction versions and version-dependent layouts, and recapture fixtures,
      per `AGENTS.md`. Last executed for the v26.2.4.23-stable / rev 54483 ->
      v26.6.1.1193-stable / rev 54485 bump (framing and per-type layout confirmed
      unchanged; constant bumped, fixtures recaptured at 54485, contract re-cited).

---

## Out of scope for this tracker

**Current completeness bar (2026-06-29 decision):** "implementation complete"
means decoding `FORMAT Native` delivered over **HTTP**. Native delivery over the
**TCP** protocol is deferred and is not part of the completeness bar right now.
The decode path also **assumes uncompressed Native bytes**: HTTP `FORMAT Native`
does not enable native block-frame compression by default, so the compressed-block
path is not exercised. The LZ4/NONE compressed-block framing (with the hand-rolled
CityHash128 checksum) lives in `src/compression/`, is tested, but is **deliberately
left unwired** - nothing in the decode path calls it. Keep the files; wire them in
only when the HTTP `compress=1` (or TCP) path enters scope.

Tracked elsewhere (README roadmap), intentionally not part of this tracker:

- Native TCP protocol engine (handshake, packet framing, revision negotiation).
- Per-runtime zero-copy adapters in the bindings (those live in the client
  repos).
- Row-major materialization (a binding concern by design).
