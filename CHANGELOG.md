# Changelog

All notable changes to ch-core-rs are recorded here, one section per release tag. Downstream language bindings pin this crate by git tag and rely on this file when repinning.

Release tags follow semver. No breaking changes on patch or minor releases.

## Unreleased

- StreamDecoder's block completeness scan is now resumable. Previously it restarted from the start of the partial block on every feed, costing O(block bytes x chunks) on large String blocks fed in transport-sized chunks. Scan progress is checkpointed per column for all types and per row for String and Nullable(String). No API or behavior change.

## v0.1.0

Initial release.

- Zero-dependency decoder and encoder for the ClickHouse FORMAT Native wire format as sent over HTTP.
- Streaming block decode into typed columnar buffers with Arrow C Data Interface export.
- Type coverage across the production type system, including numerics, strings, temporal types, Decimal, UUID, IP types, Enum, Array, Tuple, Map, Nullable, LowCardinality, Variant, Dynamic, JSON, QBit, geometry types, interval types, and supported AggregateFunction and SimpleAggregateFunction states.
- Encode support for INSERT bodies over the same type surface.
- Decode hardening: per-block cumulative allocation budgets, capacity caps bounded by remaining input, and an abort-on-unwind FFI contract.
