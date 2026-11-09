# Changelog

All notable changes to ch-core-rs are recorded here, one section per release tag. Downstream language bindings pin this crate by git tag and rely on this file when repinning.

Release tags follow semver. Before 1.0, breaking changes increment the minor
version; patch releases remain backward compatible.

## Unreleased

- Add a resumable `BlockScanner` for outer protocols to find one exact Native
  block boundary and inspect its declared row and column counts without
  materializing decoded column buffers or copying payload bytes. Partial String,
  Nullable, Array, Map, Tuple, LowCardinality, Variant, Dynamic, JSON, and
  AggregateFunction bodies and their runtime type-table prefixes resume within
  the current column instead of re-walking it after each transport read, so
  incremental feeds scan in linear time. Parsed top-level headers are
  checkpointed for fixed-width bodies too, and incomplete top-level binary
  descriptors retain grammar progress. The LowCardinality scan rejects the
  same malformed dictionaries and indexes that decode rejects, and the scan
  applies decode's 2 GiB String chunk limit and the Variant and Dynamic i32
  row guards. Flattened JSON scanning also enforces decode's cumulative
  synthetic-allocation budget across nested paths and columns. JSON paths use
  a counted prefix task rather than retaining one large task per path, and
  scanned paths retain only the count and the last name needed for validation.
  Binary runtime descriptor parsing must match the scanned boundary exactly; parser
  errors after a complete descriptor are fatal rather than resumable EOF.
- Reject repeated known `BlockInfo` fields in decode and scan. The server
  writer never emits repeats; this bounds incremental preamble scanning.
- Make `DecodeOptions` cloneable and copyable and allow future fields in
  `BlockBoundary::Complete` without breaking downstream matches using `..`.
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
  (`State::Failed(FailureKind::ServerException)`) for codes 99, 101, 702,
  720, and 749, and whenever Exception is the first server response to a query
  that sent settings. This conservative policy covers failures while parsing
  setting values before the server creates a query context. Other query
  exceptions return to `Ready`; transport EOF still requires discarding the
  connection.
- `ch-tcp-proto`: setting values and parameter names and values are no longer
  limited to 64 KiB each; only the 64 MiB Query packet bound applies. Setting
  names retain their existing 64 KiB cap. The packet bound is exact: a packet of exactly 64 MiB is
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
- `ch-tcp-proto`: add uncompressed revision-54460 INSERT upload.
  `Connection::insert` queues the Query and moves to `AwaitingInsertSchema`;
  the server's zero-row sample block arrives as `Event::InsertSchema`,
  optionally preceded by raw-byte `Event::TableColumns` metadata. Then
  `insert_data` queues one validated client Native block per call and
  `finish_insert` queues the end-of-data marker before `Event::EndOfStream`.
  New states `AwaitingInsertSchema`, `Inserting`, and `FinishingInsert`.
  Rejected blocks return `ProtocolError::InvalidInsertBlock` with an
  `InsertBlockError` reason (`TooLarge`, `Decode`, `Truncated`,
  `TrailingBytes`, `NoColumns`) and leave state and output unchanged;
  `InsertOutputAllocationFailed` reports a failed output reservation.
  `NativeBlockTooLarge` now means only an inbound block and is always fatal.
  Every Exception in an `insert` exchange is terminal. TableColumns fields
  share one `max_block_bytes` retention budget, and a declared field length
  above 64 times `max_block_bytes` fails as corruption with `StringTooLong`.
  Replay fixtures are captured from ClickHouse 26.8.
- `ch-tcp-proto`: a protocol failure, from `feed` or a failing `finish`, now
  discards queued output not yet returned by `take_output` in every state,
  including the handshake. Take output before feeding if it must be written
  regardless.

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
