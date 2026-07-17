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
- **Encode (insert path)** - active since 2026-07-01 and currently at parity
  with decode: every type the crate decodes it also encodes into `FORMAT Native`
  block bytes for `INSERT`. Keep parity as decode grows, preferring to land each
  new type's encode in the same change.

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

- **Last updated:** 2026-07-17. `QBit(T, N)` is complete at decode/encode parity
  against `v26.6.1.1193-stable`, completing the registered type set at the pin.
  `T` is `BFloat16`, `Float32`, or `Float64`; `N` is 1 through 134,217,720.
  Native stores one MSB-first, bit-transposed FixedString plane per scalar bit,
  while `QBitColumn` materializes one row-major primitive child and exports it
  as Arrow FixedSizeList. Decode allocates only the final scalar buffer. Encode
  writes the transposed planes directly into the final output allocation and
  canonicalizes unused padding bits to zero. Nullable validity stays at the
  vector level. Text and binary type headers, streaming scan, zero rows,
  RowBinary single values, all legal scalar widths, malformed inputs, Arrow
  export, both Native protocol revisions, refreshed real-server fixtures, and a
  live Native INSERT round trip are covered.
- **AggregateFunction checkpoint:** decode, encode, streaming, Arrow LargeBinary
  export, real-server fixtures, and live INSERT coverage are complete for exact
  base `count` with zero or one argument, canonical
  `nothingUInt64(Nullable(Nothing))`, exact canonical
  `nothingNull(Nullable(Nothing))`, and exact base `sum` over the supported plain
  or Nullable numeric, Decimal, and Enum types. Unknown signatures remain
  rejected even in zero-row blocks.
- **AggregateFunction work left paused:** broader direct `nothingUInt64`
  argument lists, every `nothingNull` argument family beyond the one canonical
  `Nullable(Nothing)` signature, parameterized internal `nothingNull`, and every
  other function-specific or combinator-specific state layout. This includes
  their parser gates, row-boundary scanners, encode validation and writers,
  Arrow contract decisions, synthetic tests, captured fixtures, and live-server
  semantic checks. Resume only for a concrete binding or workload requirement,
  not as an open-ended completeness exercise.
- **Scope:** frame compression and its unwired files also remain untouched and
  out of scope. Completeness still means uncompressed HTTP `FORMAT Native`.
- **Sequencing:** type completeness remains ahead of encode-API polish. The
  sink-based encode API stays deferred until a binding measures the owned
  `encode_block` allocation as material. Further open-ended
  `AggregateFunction` work also remains paused because each signature needs a
  separately confirmed unframed state-boundary codec.
- **Recommended next:** resolve sparse column serialization. Its marker can
  reach ordinary queries at negotiated revisions >= 54454 and the decoder
  currently rejects it.
- **After that:** wire the existing compression work into an explicitly scoped
  transport path, or begin the binding POC against real workloads.
- **Key references:** QBit's wire, Arrow, binary-header, RowBinary, and encode
  contract is in `CODEC_CONTRACT.md`. The logical and column models are in
  `src/schema.rs` and `src/column.rs`; parser, binary descriptor, decode/scan,
  RowBinary, and encode paths are under `src/native/`; Arrow export is in
  `src/ffi/`; real-server coverage is in `scripts/gen_fixtures.sh`,
  `tests/integration.rs`, and `tests/live_insert.rs`.

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
4. `parse_ch_type` in `src/native/type_parser.rs` parses the type string.
5. Wire bytes decode into a `Column` variant in `src/column.rs`, Arrow-shaped.
6. Arrow format string and buffer export added in `src/ffi/mod.rs`.
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
- [x] `BFloat16`
- [x] `QBit(BFloat16|Float32|Float64, N)`
- [x] `String`
- [x] `FixedString(N)`
- [x] `UUID`, `IPv4`, `IPv6`
- [x] `Date`, `Date32`, `DateTime`, `DateTime64(P[, tz])`
- [x] `Time`, `Time64(P)`
- [x] `IntervalYear`, `IntervalQuarter`, `IntervalMonth`, `IntervalWeek`,
      `IntervalDay`, `IntervalHour`, `IntervalMinute`, `IntervalSecond`,
      `IntervalMillisecond`, `IntervalMicrosecond`, `IntervalNanosecond`
- [x] `Nullable(T)` over every supported inner type
- [x] Per-column bulk-state prefix (generalized; `LowCardinality` is the first
      non-empty prefix)
- [x] `LowCardinality(T)` for every allowed inner type this crate decodes:
      `String`, `FixedString(N)`, the fixed-width numerics, `Bool`, `Date`,
      `Date32`, `DateTime`, `Time`, every `Interval*`, `UUID`, `IPv4`, `IPv6`,
      each also in the inner `Nullable` form. `DateTime64`, `Time64`, `Decimal`,
      and `Enum8`/`Enum16` are excluded:
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
- [x] `Array(T)` (decode and encode)
      - the first nested/recursive type. Wire: the element type's state prefix,
        then `num_rows` cumulative little-endian `UInt64` end-offsets, then the
        flattened element column of length = the last offset (server
        `SerializationArray`). Decoded into `ArrayColumn { offsets: Vec<i64> with
        Arrow's leading 0, values: Box<Column> }`; the element recurses through the
        shared body path, so `Array(Nullable(T))`, `Array(LowCardinality(T))`, and
        `Array(Array(T))` all compose. `Nullable(Array(T))` is forbidden by the
        server (`canBeInsideNullable()` is false). Exported as an Arrow LargeList
        (`+L`, 64-bit offsets). `read_state_prefix` recurses into the element so an
        `Array(LowCardinality(T))` consumes the LC key version before the offsets;
        `decode_column` was split into a prefix step plus a `decode_values` step so
        the element body decodes without re-reading a prefix, and the completeness
        scan mirrors this (both walk the offsets through the shared
        `read_array_offsets`). A `MAX_TYPE_DEPTH` cap bounds recursion against a
        hostile deeply-nested header, and the encoder enforces the same cap
        iteratively on caller-constructed types. A zero-length `LowCardinality`
        element run (all arrays empty) has no LC body at all on the wire; both
        directions honor the server's `limit == 0` early return. Confirmed against
        the server source and verified with the
        `arr`/`arr_s`/`arr_n`/`arr_lc`/`arr_arr`/`arr_lc_empty` live-server fixture
        columns; encode is verified by round-trip and exact-byte unit tests plus
        the live INSERT test.
- [x] `Tuple(T1, ...)` / named tuples (decode and encode)
      - the first multi-child container. Wire: no framing of its own; each
        element column is written sequentially in declaration order, the full
        `num_rows` run each (server `SerializationTuple` loops the elements);
        the state prefix/suffix recurse per element in order. Named tuples
        carry names only in the type string (`backQuoteIfNeed` quoting; parse
        accepts both the doubled-backtick and backslash escape forms, `Display`
        renders the server's backslash convention, quoting any-case `null`).
        `Nullable(Tuple)` is wire-legal and supported; `LowCardinality(Tuple)`
        is illegal. A zero-element `Tuple()` writes one 0x30 byte per row.
        Decoded into `TupleColumn { fields, len, validity }`; exported as Arrow
        `+s` struct (children named per element, 1-based indexes when unnamed).
        Decode enforces equal element lengths; encode mirrors
        `checkTupleNames`. Confirmed against the server source and verified
        with the `tup`/`tup_named`/`arr_tup`/`ntup` live-server fixture
        columns; encode runs green in the live INSERT test.
- [x] `Map(K, V)` (decode and encode)
      - always the plain `Array(Tuple(keys, values))` layout on the Native wire
        (cumulative `UInt64` end-offsets, then the flattened key run, then the
        value run; the bucketed WITH_BUCKETS MergeTree mode never reaches the
        wire). State prefix recurses key then value, before the offsets. Keys
        reject `Nullable`/`LowCardinality(Nullable)` (`isValidKeyType`); plain
        `LowCardinality(K)` is legal. `Nullable(Map)`/`LowCardinality(Map)`
        are illegal. Decoded into `MapColumn { offsets, entries }` (entries a
        two-field Tuple column, never with its own validity); exported as
        Arrow `+L` LargeList of non-nullable `entries: +s {key, value}` (never
        `+m`, which mandates i32 offsets). Confirmed against the server source
        and verified with the `m`/`m_lc`/`m_nv`/`m_arr`/`arr_m`/`m_empty`
        live-server fixture columns; encode runs green in the live INSERT
        test.
- [x] `Int128`, `UInt128`, `Int256`, `UInt256` (decode and encode)
      - Tier 2's first entry. Raw contiguous fixed-width `SerializationNumber<T>`
        dump: 16 bytes (128-bit) / 32 bytes (256-bit), straight little-endian
        two's-complement (signed for `Int128`/`Int256`, unsigned for
        `UInt128`/`UInt256`), byte-identical to the integer body under
        `Decimal128`/`Decimal256`. Decoded as a host-agnostic verbatim byte
        passthrough into four distinct `Column` variants over the shared
        `FixedBinaryColumn` (width 16 or 32); no native `i128`/`i256`. Legal
        `Nullable` AND `LowCardinality` inners (added to the allowlist), unlike
        `Decimal`/`Enum`. Exported as Arrow FixedSizeBinary `w:16`/`w:32`,
        zero-copy; signedness rides the `ChType`/type-name channel, not the Arrow
        format string. Type strings are the exact case-sensitive `Int128` etc.,
        no params, no aliases. Confirmed against the server source
        (`SerializationNumber`/`wide::integer`, v26.6.1.1193-stable) and verified
        with the `i128`/`u128`/`i256`/`u256`/`ni128`/`lc_i256` live-server
        fixture columns; encode runs green in the live INSERT test.
- [x] `Time`, `Time64(P)` (decode and encode)
      - `Time` is a raw little-endian signed `Int32` of seconds;
        `Time64(P)` is a raw little-endian signed `Int64` of `10^-P`-second ticks
        with `P in 0..=9`. Both use distinct logical and `Column` tags over the
        existing primitive buffers, with no new physical layout or copy.
        `Nullable` composes for both. `Time` is a legal `LowCardinality` inner;
        `Time64` inherits the server's false capability and is rejected there.
        Arrow exports raw `i`/`l` because ClickHouse's negative and
        beyond-one-day values cannot be represented by Arrow's one-day
        nonnegative Time types.
        Confirmed at `v26.6.1.1193-stable` against `DataTypeTime`,
        `SerializationTime`, `DataTypeTime64`, `SerializationTime64`, and
        `SerializationDecimalBase`; verified by the live `all_types` fixtures,
        unit round-trips/scanner tests, and the live INSERT test.
- [x] `IntervalYear` through `IntervalNanosecond` (decode and encode)
      - The 11 exact case-sensitive logical types share one
        `ChType::Interval(IntervalKind)` family and one
        `Column::Interval(PrimitiveColumn<i64>)` physical buffer. Every body is a
        contiguous little-endian signed Int64 count, so decode/encode reuse the
        primitive bulk hot path with no new allocation, copy, or per-row work.
        All kinds compose with `Nullable` and `LowCardinality`; bare and plain-LC
        forms are legal Map keys. Arrow exports Second/Millisecond/Microsecond/
        Nanosecond as the exact zero-copy Duration formats `tDs`/`tDm`/`tDu`/
        `tDn`; the other seven kinds stay raw `l` because Arrow's calendar
        interval layout is physically incompatible. Confirmed, not inferred, at
        `v26.6.1.1193-stable` against `DataTypeInterval`, `IntervalKind`,
        `SerializationInterval`, and `SerializationNumber<Int64>`. Unit coverage
        includes all 11 names, plain extrema, Nullable, LowCardinality, zero-row,
        multi-block, exact bytes, encode round-trips, and Arrow zero-copy export;
        the all-types fixture query/assertions and live INSERT gate include all 11
        plus Nullable/LowCardinality representatives.
- [x] `SimpleAggregateFunction(func, T)` (decode and encode)
      - Tier 2 name-decoration alias. Pure `getName()` decoration over the inner
        `T` (`DataTypeCustomSimpleAggregateFunction` attaches only a custom name;
        the serialization slot is null), so wire bytes, state prefix (including a
        `LowCardinality` key version when `T` is LC), and Arrow export are exactly
        `T`'s. No new `Column` variant: the decoded column IS the inner type's
        column, reached through `ChType::physical_delegate`. The type string
        appears VERBATIM in Native headers, including parametrized function
        spellings like `SimpleAggregateFunction(groupArrayLastArray(5),
        Array(UInt64))`; registration is case-sensitive with no aliases. Legal at
        ANY nesting position, wrapper legality delegating to `T`: confirmed live
        at 26.6.1.1193 (CREATE + Native header hexdump) for `Nullable(SAF)`,
        `LowCardinality(SAF)`, `Tuple(v SAF)`, `Array(SAF)`, and
        `Map(String, SAF)`, corroborated by server test
        `04329_tuple_element_aggregation_reject_nullable_tuple`. Decode is lenient
        on the function name (the server's 21-function whitelist is documented but
        NOT enforced; a server-authored header is trusted and the list grows
        across versions); encode validates the func string SYNTACTICALLY only
        (identifier + optional balanced literal params) to prevent type-string
        injection, and also does not enforce the whitelist (the same trusted-input
        precedent as `DateTime64` precision and `Enum` values). Multi-type-arg
        forms `SimpleAggregateFunction(f, T1, T2)` are rejected as
        `UnsupportedType`. Each SAF level charges +1 depth on both the parse and
        encode sides, and SAF chains are legal to any depth (live-constructible
        `SimpleAggregateFunction(anyLast, SimpleAggregateFunction(sum, UInt64))`).
        Inside `LowCardinality`, the alias may sit BETWEEN the LC and its
        removeNullable `Nullable`; every LC site resolves the inner through one
        shared full-chain helper `low_cardinality_dict_value_type`
        (`src/native/type_parser.rs`) so header validation, the nullability decision,
        the body paths, and the Arrow export cannot drift. Confirmed against the
        server source (`DataTypeCustomSimpleAggregateFunction`,
        v26.6.1.1193-stable) and verified with the
        `saf_sum`/`saf_lc`/`saf_grp`/`nsaf`/`lc_saf`/`lc_nsaf` live-server fixture
        columns; encode runs green in the live INSERT test.
- [x] Geo types: `Point`, `Ring`, `LineString`, `MultiLineString`, `Polygon`,
      `MultiPolygon` (decode and encode)
      - Tier 2 name-decoration aliases (`DataTypeCustomGeo`), registered
        case-sensitive with no aliases over: `Point` = unnamed
        `Tuple(Float64, Float64)`; `Ring`/`LineString` = `Array(Point)`;
        `Polygon`/`MultiLineString` = `Array(Array(Point))`; `MultiPolygon` =
        `Array(Array(Array(Point)))`. Wire bytes are byte-identical to the
        underlying nesting (no custom serialization, no extra prefix); the Native
        header carries the bare alias spelling, and the mapping is
        one-directional (a structural `Array(Tuple(Float64, Float64))` header
        stays plain Array/Tuple). GA at v26.6.1.1193-stable (the
        `allow_experimental_geo_types` gate is an obsolete no-op). `Nullable(Point)`
        is legal (Tuple is nullable-able); `Nullable` of the five Array-based
        kinds and `LowCardinality` of all six are illegal; all six are legal as
        `Array`/`Tuple` elements and `Map` keys/values (the key case leniently,
        through the delegate). Arrow export = the underlying export: `Point` as a
        `+s` struct of two `g` (Float64) children, the others as `+L` LargeList
        chains above it, zero-copy with no new buffers. Each kind charges its
        physical expansion depth (`GeoKind::expansion_depth`, `Point` 1 through
        `MultiPolygon` 4) on both sides. Confirmed against the server source
        (`DataTypeCustomGeo`, v26.6.1.1193-stable) and verified with the
        `point`/`npoint`/`ring`/`mpoly` live-server fixture columns; encode runs
        green in the live INSERT test.
- [x] `Nested(name1 T1, ...)` (decode and encode)
      - Tier 2 name-decoration alias (`DataTypeNested`) over
        `Array(Tuple(named fields))`; there is no `SerializationNested` and the
        body is byte-identical to `Array(Tuple(...))` (element state prefixes
        recurse per field, cumulative `UInt64` LE end-offsets, flattened
        field-major tuple body). It reaches the wire when a table is created with
        `flatten_nested = 0` (default `flatten_nested = 1` expands to sibling
        `n.a Array(T)` columns at CREATE time), AND in any SELECT projection that
        CASTs to `Nested` regardless of the setting (fixture-confirmed). Element
        names are MANDATORY (`Nested(UInt32)` is a server parse error), validated
        by the same `checkTupleNames` rules as a named `Tuple` (empty,
        lowercase-`null`, duplicates) with `backQuoteIfNeed` quoting; encode
        enforces these via the Tuple delegation. `Nullable(Nested)` and
        `LowCardinality(Nested)` are illegal. `Nested` inside a container
        (`Array(Nested(...))`) is accepted leniently on decode; that layout is
        INFERRED from the delegation architecture, not test-confirmed. Arrow
        export is `+L` LargeList of a `+s` struct with the declared field names.
        Charges +2 physical levels (Array + Tuple) on both sides. Binary-encoded
        type headers give `Nested` a distinct `0x2F` tag, supported by the
        explicit `*_binary_types` APIs. Confirmed against the server source (`DataTypeNested`,
        v26.6.1.1193-stable) and verified with the `nst` live-server fixture
        column; encode runs green in the live INSERT test.
- [x] `Nothing` (decode and encode)
      - the type of a bare `NULL` literal (canonical query-result type
        `Nullable(Nothing)`) and the inferred element type of an empty array
        literal. Zero-width in memory but NOT on the Native wire:
        `SerializationNothing::serializeBinaryBulk` writes one ASCII `'0'`
        (0x30) placeholder byte per row and `deserializeBinaryBulk` consumes one
        arbitrary byte per row without validating its value, so decode accepts
        any placeholder byte and encode emits the server's canonical 0x30.
        `Nullable(Nothing)` uses the ordinary framing: `num_rows` null-map bytes
        first, then the complete one-byte-per-row nested Nothing body.
        `LowCardinality(Nothing)` and `LowCardinality(Nullable(Nothing))` are
        illegal (`canBeInsideLowCardinality()` is false); `Array(Nothing)`,
        Tuple elements, Map values, and a bare Nothing Map key are
        constructible (the flattened runs are semantically empty). Decoded into
        `Column::Nothing(NothingColumn { len, validity })` - no value buffer;
        `validity` retains the structural null map of `Nullable(Nothing)` for
        Native re-encoding, and `null_count()` counts that mask like every
        other column. Arrow export is the Null type (`n`) with zero buffers,
        `null_count == len`, and a nullable field flag for both forms (Arrow
        forbids a non-nullable Null field).
        Confirmed against the server source (`DataTypeNothing`,
        `SerializationNothing`, v26.6.1.1193-stable; no revision or setting
        gate; introduction version undetermined - do not guess it) and verified
        with the `nothing` (`Nullable(Nothing)`) and `arr_nothing`
        (`Array(Nothing)`) live-server fixture columns; encode runs green in
        the live INSERT test.

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
      fixed-width numerics, `Bool`, `Date`, `Date32`, `DateTime`, `Time`, each also in the
      inner `Nullable` form. Exported as Arrow `dictionary(i32, V)` where `V` is
      the inner value type's format. The dictionary values defer to the shared
      per-type body decoder (`decode_column_body`), so the allowlist is just
      `IDataType::canBeInsideLowCardinality()` intersected with the decoded types.
      `DateTime64`, `Time64`, `Decimal`, and `Enum8`/`Enum16` reject because the server
      forbids them as LC inners (`canBeInsideLowCardinality()` is false), so they
      never appear in that position on the wire; this is independent of decode
      support, and the crate decodes all four as ordinary columns while still
      rejecting them as LC inners. `UUID`/`IPv4`/`IPv6` are decoded so their LC
      forms decode too. Confirmed against the server source and verified with
      live-server fixtures (`lc`, `lcn`, `lc_u32`, `lc_date`, `lcn_u32`,
      `lc_uuid`, `lc_time`).
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
- [x] `Array(T)` - decode AND encode done. The first nested/recursive type.
      Wire layout (server `SerializationArray`, confirmed at v26.6.1.1193-stable):
      the element type's state prefix first (so `Array(LowCardinality(T))` reads the
      LC 8-byte key version before the offsets), then `num_rows` cumulative absolute
      little-endian `UInt64` end-offsets (monotonically non-decreasing; equal
      adjacent = an empty-array row), then the flattened element body of length =
      the last offset. Decoded into `Column::Array(ArrayColumn { offsets: Vec<i64>
      with Arrow's leading 0, values: Box<Column> })`; the element recurses through
      the shared per-type body path, so `Array(Nullable(T))` (per-element null map
      after the offsets), `Array(LowCardinality(T))`, and nested `Array(Array(T))`
      all compose. `Nullable(Array(T))` is illegal (`DataTypeArray::canBeInsideNullable()`
      is false). Exported as an Arrow LargeList `+L` (64-bit offsets, one `item`
      child), zero-copy. Recursion across parse/decode/scan/export is bounded by
      `MAX_TYPE_DEPTH`. Verified with the `arr`, `arr_s`, `arr_n`, `arr_lc`,
      `arr_arr`, `arr_lc_empty` live-server fixture columns. Encode is the exact
      inverse (see "Encode / insert path") and runs green in the live INSERT test.
- [x] `Map(K, V)` - decode AND encode done. Serialized as the nested
      `Array(Tuple(keys, values))` on the Native wire, always in the BASIC
      (non-bucketed) mode. See "Implemented" for the full summary; type
      section in `CODEC_CONTRACT.md`.
- [x] `Tuple(T1, ...)` / named tuples - decode AND encode done. Sequential
      element columns, per-element prefix recursion, `Nullable(Tuple)`
      supported, `Tuple()` one-byte-per-row placeholder handled. See
      "Implemented" for the full summary; type section in `CODEC_CONTRACT.md`.

### Tier 2 - less common, still in scope

- [x] `Int128`, `UInt128`, `Int256`, `UInt256` - decode AND encode done.
      16/32-byte little-endian `SerializationNumber<T>` dump (byte-identical to
      the `Decimal128`/`Decimal256` integer body), host-agnostic verbatim
      passthrough, legal `Nullable`/`LowCardinality` inners, Arrow FixedSizeBinary
      `w:16`/`w:32`. Host representation is decided at the binding. See
      "Implemented" and the `CODEC_CONTRACT.md` type section. Introduction version
      undetermined from the shallow pin.
- [x] `Nested(...)` - decode AND encode done. A `DataTypeNested` name-decoration
      alias over `Array(Tuple(named fields))`; the server DOES emit the literal
      `Nested(...)` type string on the wire, under `flatten_nested = 0` and in any
      SELECT that CASTs to `Nested`. See "Implemented" for the full summary; type
      section in `CODEC_CONTRACT.md`.
- [x] `SimpleAggregateFunction(func, T)` - decode AND encode done. Pure name
      decoration; decodes/encodes as the inner `T`, legal at any nesting position,
      the func string validated syntactically on encode but the whitelist not
      enforced on either side. See "Implemented" for the full summary; type
      section in `CODEC_CONTRACT.md`.
- [x] Geo types: `Point`, `Ring`, `LineString`, `MultiLineString`, `Polygon`,
      `MultiPolygon` - decode AND encode done. `DataTypeCustomGeo` name-decoration
      aliases over `Tuple`/`Array` of `Float64` (`Point` = `Tuple(Float64,
      Float64)`, the rest nest `Array` over it), GA and stable at
      v26.6.1.1193-stable (the `allow_experimental_geo_types` gate is an obsolete
      no-op). See "Implemented" for the full summary; type section in
      `CODEC_CONTRACT.md`.
- [x] `Geometry` - decode AND encode done. A custom fixed name over the
      canonical six-child geo Variant, sharing its BASIC wire body and Arrow
      Dense Union buffers. See the Tier 3 checkpoint and `CODEC_CONTRACT.md`.
- [x] `Interval*` (`IntervalYear` ... `IntervalNanosecond`) - decode AND encode
      done. One signed Int64 body per row, with exact logical unit preservation;
      legal Nullable/LowCardinality inners and Map keys. See "Implemented" and
      the `CODEC_CONTRACT.md` type section.
- [x] `Nothing` - decode AND encode done. The type of a bare `NULL`; zero-width
      in memory but NOT on the Native wire: `SerializationNothing` writes one
      ASCII `'0'` (0x30) placeholder byte per row and decode consumes one
      arbitrary byte per row without validating it. `Nullable(Nothing)` is legal
      (null map first, then the full one-byte-per-row body);
      `LowCardinality(Nothing)` is illegal. Arrow export is the Null type (`n`,
      zero buffers). See "Implemented" and the `CODEC_CONTRACT.md` type section.
      Introduction version undetermined; do not guess it.
- [x] `BFloat16` - 2-byte float, the top 16 bits of an IEEE-754 `Float32` (sign +
      8-bit exponent + 7-bit truncated mantissa), serialized raw little-endian via
      `SerializationNumber<BFloat16>` with no per-row framing. Confirmed registered
      and stable at v26.6.1.1193-stable (`registerDataTypeNumbers`; the
      `allow_experimental_bfloat16_type` gate is now an obsolete no-op). Decode
      and encode preserve raw words in
      `Column::BFloat16(PrimitiveColumn<[u8; 2]>)`; Arrow exports `w:2` because
      `e` is incompatible IEEE binary16 and `S` would misstate integer semantics.
      Legal Nullable/LowCardinality inner
      and Map key, with the generic suspicious numeric-LC construction gate.
      Verified by plain/Nullable/LowCardinality live fixtures and live INSERT.
- [x] `Time` - decode AND encode done. 4-byte little-endian signed `Int32` of
      seconds, can be negative (documented text range
      [-999:59:59, 999:59:59], while Native accepts any i32 payload); no
      timezone. The type rejects non-empty timezone arguments
      but accepts and discards an empty timezone input alias; neither form is
      emitted in a canonical Native header.
      `SerializationTime` extends `SerializationNumber<Int32>` with no binary-bulk
      override. Confirmed registered and stable at v26.6.1.1193-stable.
- [x] `Time64(P)` - decode AND encode done. 8-byte little-endian signed `Int64`
      of ticks scaled 10^-P, P in 0..=9 (default 3); no timezone. Wire layout
      identical to `DateTime64`
      (`SerializationTime64` extends `SerializationDecimalBase<Time64>`). Confirmed
      registered and stable at v26.6.1.1193-stable. The
      `enable_time_time64_type` setting (default true) is a post-parse validation
      gate for CREATE, ALTER, table-function structure declarations, and
      user-facing CAST targets. It does not gate DataTypeFactory registration or
      Native serialization, so a `Time`/`Time64` column can still appear in a
      Native stream regardless of the setting.

### Tier 3 - advanced / newest type system; hardest, do last

In scope, but high effort and the most version-sensitive: these are the newest
types and their wire formats are still evolving across releases, so confirm the
exact layout at the pinned tag before implementing. V1 found all of these are GA
(not experimental) at v26.6.1.1193-stable: every `allow_experimental_*` gate that
once covered them is now an obsolete no-op. None of their `Serialization*` classes
branch on the protocol revision, but the self-describing types (`Variant`,
`Dynamic`, `JSON`) carry their own in-band version/structure headers, which is
where the across-release churn lives.

- [~] `AggregateFunction(...)` - PAUSED by project decision on 2026-07-15. Exact
      unversioned base `count` with zero or one argument type is done at
      decode/encode parity, as are canonical `nothingUInt64`, exact canonical
      `nothingNull` for `Nullable(Nothing)`, and exact base `sum` for every plain
      or Nullable numeric and Enum argument. The two canonical `nothing*` states
      use the strict fixed-zero codec.
      States are stored as raw bytes with i64 row offsets and exported as Arrow
      LargeBinary. The generic item stays open because Native has no state/column
      length framing; each additional signature needs a confirmed boundary
      codec. Work still left includes the broader direct `nothingUInt64`
      argument family, every `nothingNull` argument family beyond the canonical
      `Nullable(Nothing)` signature, parameterized internal `nothingNull`, and
      all other function-specific and combinator-specific state layouts, with
      matching parser, scanner, decode, encode, fixture, FFI-contract, and
      live-server coverage. Do not resume this as an open-ended parity effort.
      Resume only when a concrete binding or workload requires a specific
      missing signature.
- [x] `Variant(...)` - complete at decode/encode parity. Direct Native BASIC
      mode 0, canonical alternative ordering, intrinsic NULL discriminator 255,
      dense child columns, streaming boundary scans, malformed discriminator
      rejection, flat and two-level Arrow Dense Union export through all 255
      alternatives, synthetic plain/zero-row/multi-block/round-trip tests,
      real-server fixture capture, and live INSERT are covered at
      `v26.6.1.1193-stable`. COMPACT mode 1 remains intentionally rejected
      because direct `NativeWriter` never emits it.
- [x] `Dynamic` - complete at decode/encode parity for direct V1/V2 and
      FLATTENED word 3, including SharedVariant binary cells, textual and binary
      type tables, recursive containers, result-wide Arrow Dense Union stream
      schemas, malformed-input rejection, synthetic tests, and real-server
      fixtures at `v26.6.1.1193-stable`. V3 word 4 is intentionally rejected
      because direct NativeWriter does not emit it.
- [x] `JSON` (new object type) - complete at decode/encode parity at
      v26.6.1.1193-stable. Carries its own LE u64 structure word: V1 (0, with the
      legacy ignored count slot), STRING (1, one document string per row), V2 (2),
      and opt-in FLATTENED (3); V3 word 4 is rejected because direct NativeWriter
      never emits it. A structured body decodes into typed paths (sorted), one
      block-local `Dynamic` per dynamic/flattened path, and the shared-data
      `Array(Tuple(String, String))` overflow (V1/V2 only) whose values are opaque
      binary descriptor+payload blobs kept unmaterialized. The V1/V2 direct path
      count is bounded by `max_dynamic_paths`, while the FLATTENED count is
      confirmed UNBOUNDED (`unflattenAndInsertPaths`, `SerializationObjectHelpers`),
      so a pathless FLATTENED block with zero body bytes per row still decodes
      under the type-aware row-count guard. `Nullable(JSON)` is legal (top-level
      null map), `LowCardinality(JSON)` illegal, and JSON composes in
      Array/Tuple/Map/Variant/Dynamic and as its own typed path (nested), bounded
      by `MAX_TYPE_DEPTH`. Arrow export is a struct of the typed paths, the
      result-wide dynamic-path Dense Unions, and a `_shared_data` LargeList of
      `{paths utf8, values binary}`, with a Text body exporting as utf8; binary
      type descriptor tag 0x30. Legacy `Object('json')` is not registered at the
      pin and is intentionally not implemented. Unit (plain, Nullable, zero-row,
      multi-block, encode round-trip, Arrow), the `j_typed`/`j_bare`/`j_null`
      all_types fixture columns, the STRING and FLATTENED single-column auxiliary
      fixtures, and the live JSON INSERT and binary-type-header round-trips are all
      present and green. Introduction: production ready in 25.3; the STRING setting
      `output_format_native_write_json_as_string` shipped in 24.10 and the
      FLATTENED setting `output_format_native_use_flattened_dynamic_and_json_serialization`
      in 25.6 (per `SettingsChangesHistory.cpp`).
- [x] `Geometry` - complete at decode/encode parity at
      v26.6.1.1193-stable. The canonical physical type is
      `Variant(LineString, MultiLineString, MultiPolygon, Point, Polygon, Ring)`
      after the server sorts custom names, with UInt8 discriminators 0..=5 and
      255 for intrinsic NULL. `ChType::Geometry` preserves the custom header and
      delegates every physical path to the existing `VariantColumn`: BASIC LE
      UInt64 mode 0, one discriminator per row, then six dense geo bodies.
      Arrow is one seven-child Dense Union (six named geo children plus NULL),
      with no new buffers, remap, or per-row Geometry work. Text output is
      canonical `Geometry` (`GEOMETRY` is an accepted input alias), and binary
      headers use Custom tag 0x2c plus the name. Nullable, LowCardinality, and a
      direct outer Variant are illegal; Array/Tuple/Map and typed JSON paths
      compose. Unit plain/NULL/zero-row/multi-block/encode/Arrow/binary-depth
      coverage, all_types real fixtures (including all alternatives through
      `Array(Geometry)`), and live INSERT are green.
- [x] `QBit(T, N)` - complete at decode/encode parity at
      v26.6.1.1193-stable for `T` in BFloat16/Float32/Float64 and dimension
      `N` in 1..=134,217,720. Native bulk is `bit_width(T)` bit-transposed
      FixedString planes, MSB first; the public `QBitColumn` is one row-major
      primitive child with N values per row and optional vector-level validity,
      exported as Arrow FixedSizeList. Decode and encode each allocate only the
      final destination buffer and perform no per-value heap allocation.
      Text/binary type headers, RowBinary single values, zero rows, Nullable,
      multi-block streaming, exact transpose bytes, all legal scalar widths,
      malformed public buffers, Arrow schema/buffers, recaptured real-server
      fixtures, and live INSERT are covered. The server introduced QBit as
      experimental in 25.10, promoted it to Beta and enabled it by default in
      26.1, and made it GA in 26.2. ClickHouse 26.4 changed single-value binary
      serialization to explicit little-endian order; the pinned Native bulk
      call graph has no corresponding version branch.

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
      Resolve before opening the binding POC to outside workloads (2026-07-15
      decision). Server-side sparse encoding has been default-on since 23.7
      (`ratio_of_defaults_for_sparse_serialization = 0.9375`, introduced
      experimental in 22.1), so real MergeTree tables commonly hold sparse
      parts, and the per-part serialization kind can differ within one table.
      Reach analysis: the custom-serialization marker byte only exists at
      revision >= 54454, and plain HTTP `FORMAT Native` responds with rev 0
      framing unless the client sends `client_protocol_version` (this is
      exactly how `scripts/gen_fixtures.sh` captures the rev 54485 fixture), so
      a rev 0 client presumably always receives full columns - INFERRED, since
      without the marker the server has no way to signal sparse; confirm the
      NativeWriter conversion at the pin. Wire layout per the official Native
      format spec docs (docs-sourced 2026-07-15, NOT yet source-confirmed; run
      the `clickhouse-server-reader` workflow before implementing): the marker
      carries a kind_stack byte of 0x01 for SPARSE, then the column data is two
      back-to-back streams - first a VarUInt offset stream where each value is
      the number of default positions before the next non-default value and a
      value with bit 62 set (`END_OF_GRANULE_FLAG`) terminates the stream, then
      the non-default values densely packed in the inner type. Special case:
      for `Nullable(T)` the null map is dropped entirely; the offset stream
      identifies the non-NULL positions and every other position reconstructs
      as NULL.
- [x] Multiple-stream bulk-state prefix - the per-column read is generalized via
      `read_state_prefix` in `src/native/decode/mod.rs`, so a type with a real
      `deserializeBinaryBulkStatePrefix` declares its prefix in one place instead
      of the old "prefix reads zero bytes" assumption. `LowCardinality` reads its
      8-byte key version through it. `Variant` and `Dynamic` also read their mode
      or block-local structure and type-table prefixes through the same
      traversal. The prefix runs per column per block, gated on the block having
      rows, matching `NativeReader::readData`.
- [x] Binary-encoded type headers (`DataTypesBinaryEncoding`) - explicit
      `*_binary_types` decode/encode APIs cover outer Native headers and Dynamic
      runtime type tables without changing the existing options structs. The
      zero-dependency descriptor parser is depth/complexity/count bounded and
      rejects illegal semantic wrappers and unsupported/reserved tags rather
      than partially consuming them. A live HTTP INSERT/SELECT test verifies the
      paired server input/output settings against the pinned server.

---

## Encode / insert path

Now in active development (priority shifted here 2026-07-01). The aim is to turn
columnar input, the same `Column`/`ColBatch` model the decoder produces, into
`FORMAT Native` block bytes the server accepts for `INSERT`. Lives in
`src/native/encode/mod.rs` (`encode_block`, `encode_chunked`, `EncodeOptions`,
`EncodeError`), the mirror of `src/native/decode/mod.rs`. Framing is the exact inverse
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
- [x] `Time` (i32 seconds) / `Time64(P)` (i64 ticks) through the same primitive
      little-endian encoder, with distinct `Column::Time`/`Time64` tag matching.
      Precision remains type-string metadata only. `Nullable` composes for both;
      the shared dictionary path encodes `LowCardinality(Time)`, while
      `LowCardinality(Time64)` remains `UnsupportedType` to match the server.
      Verified at revisions 0 and 54485, including an exact signed-byte framing
      pin, zero-row and multi-block round-trips, invalid precision rejection,
      live fixture capture, and live INSERT against 26.6.1.1193.
- [x] All 11 `Interval*` types through one signed-i64 primitive encoder arm over
      `Column::Interval`, the exact inverse of the decoder. The unit stays in
      `ChType::Interval(IntervalKind)` and the type string. `Nullable` and the
      shared `LowCardinality` writer compose without Interval-specific framing.
      Covered at revisions 0 and 54485, including all kinds, exact signed bytes,
      zero-row, multi-block, Nullable, LowCardinality, all-types fixtures, and
      the live INSERT batch.
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
      (`Decimal`, `DateTime64`, `Time64`, `Enum`), wrong dictionary value variants, negative
      or out-of-range indexes, non-empty dictionaries on zero-row blocks, and
      nullable-LC rows that violate the index-0 NULL sentinel rule. Covered by
      rev 0 / rev 54485 round-trips, exact-byte pins, zero-row and multi-block
      tests, rejection tests, and live INSERT tests against localhost, including
      String, nullable String, and a 260-entry FixedString dictionary that
      exercises UInt16 indexes.
- [x] `Array(T)` (offsets + flattened element body, the write-side of the
      prefix/values split). `write_state_prefix` mirrors decode's
      `read_state_prefix`: `Array` writes nothing of its own and recurses, so a
      leaf `LowCardinality` key version is hoisted once to the very front of the
      column, before the offsets, across all nesting levels. `encode_array_data`
      writes `offsets[1..]` as raw LE `u64` (the Arrow leading 0 is model-only;
      validation proved non-negativity, so the i64 bytes are the wire u64's) via
      the `encode_primitive!` bulk-copy fast path, then the element body through
      the shared `encode_column_values` path without re-emitting a prefix, so
      `Nullable`, `LowCardinality`, and nested `Array` elements compose.
      Zero-row blocks write no data section at all; a zero-length nested LC run
      writes no LC body (the server's `limit == 0` early return, which also
      required the matching decode-side gate in `decode_values`/`skip_values`).
      Pre-write `validate_array` rejects wrong offset counts, a missing leading
      zero, non-monotonic or negative offsets, and a last offset that disagrees
      with the flattened element count, then validates the element column
      recursively; `validate_column` also caps caller-constructed type nesting
      at `MAX_TYPE_DEPTH` iteratively (encode input never passes through
      `parse_ch_type`). Verified by rev 0 / rev 54485 round-trips (plain,
      nullable-element, LC-element, all-empty LC-element, nested), exact-byte
      pins (`rev0_frames_array_int32_bytes`,
      `rev0_frames_array_low_cardinality_all_empty_bytes`), zero-row and
      multi-block `encode_chunked` round-trips, rejection tests, the recaptured
      `arr_lc_empty` fixture column, and the live INSERT test
      (`arr_i32`/`arr_ns`/`arr_lc`/`arr_arr`/`arr_lc_empty`) green against
      26.6.1.1193. With this, encode coverage equals decode coverage.
- [x] `Tuple(T1, ...)` (sequential element bodies through the shared
      `encode_column_values` path, per-element `write_state_prefix` recursion,
      `Tuple()` writes 0x30 per row). Pre-write `validate_tuple` rejects
      field-count mismatches and ragged element lengths (`InconsistentBatch`)
      and server-unconstructible element names per `checkTupleNames` plus the
      factory's all-or-nothing rule (empty, exact-lowercase `null`, duplicate,
      or mixed named/unnamed -> `UnsupportedType`); a plain (non-Nullable)
      Tuple with null-marked validity is rejected by the generic
      `null_count() > 0` check. Verified by round-trips at rev 0 and 54485,
      exact-byte pins, rejection tests, and the live INSERT test.
- [x] `Map(K, V)` (offsets `[1..]` via the bulk path, then the key and value
      runs through `encode_column_values`; `write_state_prefix` recurses key
      then value so an LC key's version hoists before the offsets). Pre-write
      `validate_map` enforces the Array offset invariants, key-type legality
      (shared `is_valid_map_key_type` -> `UnsupportedType`), a two-field
      entries tuple, and rejects entries validity (`InconsistentBatch`).
      Verified by round-trips at rev 0 and 54485, exact-byte pins (including
      the all-empty LC-key hoisted-prefix pin), rejection tests, and the live
      INSERT test.
- [x] `SimpleAggregateFunction(func, T)` (encode as the inner `T` via
      `ChType::physical_delegate`; no new body writer). `write_state_prefix`,
      `encode_column_values`, and `is_encodable` all see through the alias, so a
      SAF at any nesting position encodes as its inner. Pre-write validation runs
      `validate_saf_func_spellings` over every SAF in the declared type (an
      identifier plus an optional balanced literal-param suffix, the shared
      `is_simple_aggregate_func_spelling`) so a caller-constructed `func` cannot
      inject type-string tokens; the server whitelist is not enforced. Each SAF
      level charges +1 in `type_depth`. Verified by round-trips (plain,
      `Nullable(SAF)`, `Array(SAF)`, `LowCardinality(SAF)`, `Tuple(v SAF)`, and
      the `groupArrayLastArray(5)` param form), exact-byte header pins,
      func-spelling rejection tests, and the live INSERT test.
- [x] Geo types (encode as the underlying `Tuple`/`Array`-of-`Float64` nesting via
      `ChType::physical_delegate`; no new body writer). Always encodable (the
      `Float64` nesting always is). Each kind charges its `expansion_depth` in
      `type_depth`, so a geo-tipped type that decodes is re-encodable. Verified by
      round-trips (`Point`, `Nullable(Point)`, `MultiPolygon`), exact-byte pins,
      and the live INSERT test.
- [x] `Nested(name1 T1, ...)` (encode as `Array(Tuple(named fields))` via
      `ChType::physical_delegate`; no new body writer). Field names are validated
      through the Tuple delegation (`checkTupleNames`: an empty name, the
      exact-lowercase reserved `null`, or a duplicate -> `UnsupportedType`).
      Charges +2 in `type_depth`. Verified by round-trips, an exact-byte
      header/body pin, name-rejection tests, and the live INSERT test.
- [x] Round-trip tests (encode then decode equals the original buffers) for the
      numerics, plus a live-server `INSERT` acceptance test
      (`tests/live_insert.rs`, `#[ignore]`, curl over HTTP). Extend both as each
      new type lands.

**Encode per-type workflow (differs from the decode "Adding A New ClickHouse Type"
flow in `AGENTS.md`).** The wire layout is already confirmed for the decode
direction and encode is its exact inverse, so no `clickhouse-server-reader` read
and no new committed fixtures are needed (the exceptions are the wrappers, where
the framing detail matters: see `LowCardinality`). For a plain, non-wrapper type:

1. Add a `(ChType, Column)` arm in `encode_column_data` (`src/native/encode/mod.rs`)
   that writes the inverse of that type's decoder in `src/native/decode/mod.rs`. Keep
   the pair-match so the on-wire type string (from `ChType::Display`) and the body
   (from the `Column` buffer) can never diverge.
2. Add a round-trip unit test in `encode/tests/`: build a `ColBatch`, `encode_block`,
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

- [ ] **Sink-based encode (deferred 2026-07-15; polish, not a blocker)** (the
      one modest, real win for the overlap path): write
      a block straight into a caller-provided `&mut Vec<u8>` / `impl io::Write`
      instead of returning an owned `Vec`, so a binding can encode directly into
      its transport send buffer and skip an allocation plus copy per block. The
      private `encode_block_into(&mut Vec<u8>, ...)` already does exactly this
      internally; exposing it (or an `io::Write` variant) is a few lines. Pick
      it up when a binding measures the per-block allocation as mattering, not
      before; Tier 3 type coverage comes first.
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
      (2) two registered types were missing from the tiers and were added to
      Tier 3 - `Geometry` (= `Variant(...)`, alias `GEOMETRY`) and `QBit(T, N)`;
      both are now complete at decode/encode parity;
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
