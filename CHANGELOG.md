# Changelog

All notable changes to ch-core-rs are recorded here, one section per release tag. Downstream language bindings pin this crate by git tag and rely on this file when repinning.

Release tags follow semver. Before 1.0, breaking changes increment the minor
version; patch releases remain backward compatible.

## Unreleased

- Add a resumable `BlockScanner` for outer protocols to find one exact Native
  block boundary and inspect its declared row and column counts without
  materializing decoded column buffers or copying wire bytes. Partial String,
  Nullable, Array, Map, Tuple, LowCardinality, and Variant bodies resume within
  the current column instead of re-walking it after each transport read, so
  incremental feeds scan in linear time. The LowCardinality scan rejects the
  same malformed dictionaries and indexes that decode rejects, and the scan
  applies decode's 2 GiB String chunk limit and the Variant and Dynamic i32
  row guards.
- Treat a partial first VarUInt in either `BlockInfo` or the unframed
  revision-zero column count as `UnexpectedEof`. An entirely empty input
  remains a clean block boundary.
- Add uncompressed revision-54460 SELECT support to `ch-tcp-proto`, including
  Query parameters and settings, Data, Totals, Extremes, Progress,
  ProfileInfo, Log, ProfileEvents, Exception, and EndOfStream events, plus
  replay fixtures captured from ClickHouse 26.8. The driver contract is feed
  any amount, then drain `poll_event` until `None`. `feed` parses every byte
  it is given, so there is no backpressure or resume call, and event memory is
  proportional to the bytes fed since the last drain. `query` and `ping` are
  refused until events are drained.
- `ch-tcp-proto`: a `Query::with_parameter` value is exactly the text an HTTP
  `param_<name>` value would carry, the server's escaped text form, such as
  `\N` for NULL, `['a','b']` for an array, or `a\tb` for a String with a tab.
  The engine adds the single quoting layer that `readQueryParameters` undoes,
  so Array, Tuple, Map, and NULL parameters resolve as they do over HTTP. A
  raw tab or newline in a String value is rejected by the server, as over
  HTTP. `query` rejects a repeated parameter name with
  `ProtocolError::DuplicateName`, because TCP keeps the last value and HTTP
  the first.
- `ch-tcp-proto`: cap each Native block at `DEFAULT_MAX_BLOCK_BYTES` (1 GiB),
  configurable with `Connection::with_max_block_bytes`; values below
  `MIN_MAX_BLOCK_BYTES` (1 KiB) are raised to it. The cap covers Data, the
  end-of-data marker, Totals, Extremes, Log, and ProfileEvents blocks. Block
  buffers grow only with bytes received and never past the cap.
- `ch-tcp-proto`: a query Exception closes the connection
  (`State::Failed(FailureKind::ServerException)`) for codes 99, 101, 720, and
  749, and for code 115 only when its message is `Unknown setting '<name>'`
  for a name sent with `Query::with_setting`. Code 115 from a SQL `SETTINGS`
  clause leaves the connection `Ready`.
- `ch-tcp-proto`: setting values and parameter names and values are no longer
  limited to 64 KiB each; only the 64 MiB Query packet bound applies. Setting
  names keep a 64 KiB cap so a code 115 message naming one is never
  truncated. The packet bound is exact: a packet of exactly 64 MiB is
  accepted, and `QueryOutputTooLarge.length` is the true packet length.
- `ch-tcp-proto`: `Event::ServerHello` and `Event::Exception` now hold
  `Box<ServerHello>` and `Box<ServerException>`, keeping `Event` at 56 bytes.
- `ch-tcp-proto`: `finish` after a protocol failure returns the error that
  failed the connection instead of `InvalidState`. Add
  `ProtocolError::InconsistentBlockBoundary` for an internal scanner
  inconsistency.
- `ch-tcp-proto`: add `Connection::effective_revision()` for decoding block
  payloads.
- `ch-tcp-proto`: the crate README documents and tests `run_select` (with a
  typed error), a per-payload `decode_payload`, and a Ping `health_check`,
  each run against the recorded captures over a fake stream.

## v0.2.1

- Expose ClickHouse compression framing with CityHash128 1.0.2 verification,
  LZ4/NONE decode, and a pure Rust LZ4 writer. The frame reader reports bytes
  consumed so TCP engines can stop at the next packet and check that a block
  consumed all plaintext in its final frame. An into-buffer reader appends
  directly to caller-owned storage and returns frame metadata for that check.
  ZSTD remains unsupported.
- Add real-server HTTP and TCP compression fixtures at revision 54460,
  including a TCP Native block spanning multiple compression frames. Document
  the existing one-block decode entry for the planned packet engine.

## v0.2.0, 2026-09-01

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
