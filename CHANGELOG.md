# Changelog

All notable changes to ch-core-rs are recorded here, one section per release tag. Downstream language bindings pin this crate by git tag and rely on this file when repinning.

Release tags follow semver. Before 1.0, breaking changes increment the minor
version; patch releases remain backward compatible.

## v0.2.0, Unreleased

- Add standalone `MultiPoint` decode, encode, and Arrow export for ClickHouse
  26.8. Extend `Geometry` with the server's appended discriminator 6 while
  preserving the existing discriminator assignments 0 through 5. Downstream
  matches on the public `GeoKind` enum must handle its new `#[non_exhaustive]`
  contract, and Geometry Arrow consumers now see seven geo children plus NULL.
  These public API and Arrow schema changes make this a breaking release from
  the 0.1 line.
- Reject decode or encode options above protocol revision 54485, the highest
  revision whose Native layout this crate supports. ClickHouse 26.8 advertises
  revision 54492, which enables a newer String size-stream layout, so bindings
  must cap negotiation at the exported `DBMS_TCP_PROTOCOL_VERSION`.

## v0.1.1, 2026-08-12

- StreamDecoder's block completeness scan is now resumable. Previously it restarted from the start of the partial block on every feed, costing O(block bytes x chunks) on large String blocks fed in transport-sized chunks. Scan progress is checkpointed per column for all types and per row for String and Nullable(String). No API or behavior change.

## v0.1.0, 2026-07-23

Initial release.

- Zero-dependency decoder and encoder for the ClickHouse FORMAT Native wire format as sent over HTTP.
- Streaming block decode into typed columnar buffers with Arrow C Data Interface export.
- Type coverage across the production type system, including numerics, strings, temporal types, Decimal, UUID, IP types, Enum, Array, Tuple, Map, Nullable, LowCardinality, Variant, Dynamic, JSON, QBit, geometry types, interval types, and supported AggregateFunction and SimpleAggregateFunction states.
- Encode support for INSERT bodies over the same type surface.
- Decode hardening: per-block cumulative allocation budgets, capacity caps bounded by remaining input, and an abort-on-unwind FFI contract.
