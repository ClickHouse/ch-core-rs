# Vision

This file states what `ch-core-rs` is for and where it is going, so the
direction survives across contributors and does not drift back to the original
(wrong) pitch. `README.md` is the authoritative "what is implemented today"
list. `DECODER_CONTRACT.md` is the per-type output reference. `AGENTS.md` is how
to work in the repo. This file is the why and the where-next.

## One sentence

A shared, zero-dependency Rust core that implements the ClickHouse wire protocol
and type system once, so every language client reuses one correct, maintained
implementation instead of growing its own.

## What we learned, and why the pitch changed

The original pitch was speed: decode `FORMAT Native` into typed column buffers
faster than the alternatives. We ran that experiment with real Python and JS
bindings against a live server. The broad speed claim does not hold:

- For Arrow consumers, server `FORMAT Arrow` / `ArrowStream` beats
  Native -> Arrow, in both Python and JS. This is structural, not a bug we can
  fix. The server already lays bytes out in Arrow's memory format, and the
  ClickHouse Native string, null, and bool layouts are not Arrow layouts, so the
  client side transform is mandatory and is strictly more work than copying two
  contiguous Arrow buffers. Caveat: this was measured on localhost, where
  transfer is free. Native is the most compact wire format, so a bandwidth bound
  or high latency link may shift the end to end story. That has not been
  remeasured yet.
- Row and object materialization is dominated by per-runtime object allocation
  (Python objects, JS objects), which the core cannot own.
- The core clearly wins where it avoids row/object materialization and hands
  back typed column buffers that are not Arrow: JS `TypedArray`s today, Python
  NumPy columns plausibly.

So the durable reason for this crate is not raw decode speed. It is
consolidation: one correct implementation, ClickHouse type fidelity that Arrow
does not carry faithfully, the read/write and protocol plumbing no Arrow library
gives you, and zero-copy delivery into each runtime's native container.

## Positioning

- Arrow consumers -> use server `Arrow` / `ArrowStream`. The core does not
  compete here.
- Non-Arrow column consumers (JS `TypedArray`, NumPy) -> the core is the fast,
  convenient path.
- ClickHouse type fidelity -> the core is the faithful path. The server's
  CH -> Arrow mapping normalizes or drops information (Enum names, IPv4/IPv6
  semantics, `AggregateFunction` state, exact CH type identity).
- Inserts and protocol -> the core is shared plumbing nothing else provides.

Always frame a Native column API as `query_columns` / `query_native_columns`,
never as a replacement for `query_arrow`.

## Value pillars (why this exists)

1. One correct protocol and type implementation, not N. Every client today
   reimplements type parsing, block framing, compression, and the type long
   tail, and redoes it on every ClickHouse release. Do it once, audited, here.
2. Type fidelity Arrow does not carry faithfully. This is the real moat over
   `ArrowStream`.
3. Read/write symmetry: Native decode and Native encode (inserts), plus the
   protocol around them.
4. Native zero-copy containers per runtime: `TypedArray` in JS, NumPy or an
   Arrow capsule in Python, with no extra copy across the binding boundary.
5. Streaming overlap with backpressure: feed socket bytes, get decoded column
   chunks as they arrive, instead of buffering the whole response then decoding.

## The seam

The core is a streaming ClickHouse protocol and type engine, not a one-shot
buffer decoder. The layering:

```
transport bytes
  -> protocol framing (HTTP / native TCP, compression)
    -> Native block decode into typed columnar chunks   <- exists today
      -> thin per-runtime adapters (TypedArray, NumPy, Arrow C Data, CH-typed values)
```

Decode-to-Arrow stays as one optional adapter, explicitly deprioritized against
server `ArrowStream`.

## Current state

See `README.md` for the authoritative list. In short:

- Read path: Native block decode from a complete buffer and from streamed
  chunks. Blocks preserved as separate chunks.
- Types: `Bool`, `Int8..64`, `UInt8..64`, `Float32/64`, `String`,
  `FixedString(N)`, `Nullable(T)`. See `DECODER_CONTRACT.md`.
- Export: Arrow C Data Interface (schema, array, stream).
- Hardening: slice-cursor decode, zero-copy string path, allocation-free
  streaming completeness scan, malformed-header guards so untrusted bytes never
  panic.
- Bindings (live in the client repos, experimental): JS via napi-rs exposing
  Native columns as `TypedArray`s; Python via PyO3.

Not yet: most ClickHouse types, compression framing, native TCP protocol,
inserts, and per-runtime non-Arrow adapters beyond the JS POC.

## Roadmap

Big paths, with rationale. The recommended next step is marked; the ordering is
a proposal, not settled.

### P1 - Type coverage, the long tail  [in progress]

The core cannot decode most real result sets today, and type fidelity is the
moat (pillar 2). The repo already has a defined per-type workflow in `AGENTS.md`
("Adding A New ClickHouse Type"). Suggested order by how common the type is in
real schemas, with effort flagged:

- `DateTime`, `DateTime64(p, tz)`, `Date`, `Date32`. DONE (2026-06-05).
- `LowCardinality(T)` (very common as a wrapper; higher effort, has its own
  dictionary and index framing).
- `Decimal(P, S)`.
- `UUID`, `IPv4`, `IPv6`.
- `Enum8`, `Enum16` (carry the named mapping; Arrow does not).
- Containers: `Array(T)`, then `Tuple(...)`, then `Map(K, V)` (highest effort,
  nested serialization).
- Wide ints: `Int128/256`, `UInt128/256`.

### P2 - Compression framing (LZ4, then ZSTD)

Real Native responses are commonly compressed. Without this the read path is
limited to uncompressed streams, which may already constrain the bindings
depending on how they fetch. Small, self-contained, large unlock. Could run
alongside P1.

### P3 - Per-runtime zero-copy adapters

JS `TypedArray` over an external `ArrayBuffer` with the N-API copy removed (in
flight in the JS repo). Python NumPy column export that does not route through
Arrow. This is pillar 4 and where the core measurably wins.

### P4 - Insert / write path (Native encode)

Columnar input -> Native block bytes for `INSERT`. Read/write symmetry, pillar
3. Sequenced after the type model and buffer layout are exercised by more read
types, so the encoder mirrors a stable model instead of a moving one.

### P5 - Protocol engine (native TCP)

Handshake, query / data / progress / profile / exception packets, revision
negotiation. The largest path. Turns the core from a codec into a client
engine. Worth it once types, compression, and inserts justify owning transport.

## Non-goals

- Beating server `Arrow` / `ArrowStream` for Arrow consumers.
- Row-major materialization in the core. It stays a binding concern and was
  measured to throw away the core's advantage.
- Runtime dependencies in the core. Zero dependencies stays.
- Language-specific host value policy (Python `int` vs JS `BigInt`, `uuid.UUID`
  vs raw bytes). Bindings own that, never the core.

## Decision log

- 2026-06-05: Binding benchmarks (Python and JS) showed Native -> Arrow loses to
  server `ArrowStream` for Arrow consumers. Reframed the crate from "faster
  decode" to "shared correctness, type fidelity, non-Arrow delivery, and
  protocol core." Recorded the localhost caveat as an open item.
- 2026-06-05: Hardened the decode hot path: slice cursor replacing
  `R: Read` + `io::Cursor`, zero-copy string decode, allocation-free streaming
  completeness scan, and malformed-header overflow guards.
- 2026-06-05: Added temporal types (`Date`, `Date32`, `DateTime`,
  `DateTime64`), confirmed against v26.2.4.23-stable and verified with
  live-server fixtures. Found a fidelity caveat: a `DateTime('tz')` column's
  emitted type string is protocol-revision gated. Over HTTP `FORMAT Native`
  with no `client_protocol_version` (revision 0) the server drops the timezone
  and emits a bare `DateTime`; at the negotiated TCP revision it keeps
  `DateTime('UTC')`. The wire data (UInt32 seconds) is identical either way, so
  decode is correct, but a client that needs timezone fidelity must negotiate a
  protocol revision. Not yet confirmed against the server source why, see open
  questions.

## Open questions

- Remeasure Native vs `ArrowStream` end to end over a realistic network (remote
  endpoint or injected latency and bandwidth), since the decision so far rests
  on localhost numbers.
- Decide whether the core should own transport (P5) or stay a codec that the
  bindings feed. This gates how far pillars 1 and 3 can go.
- Confirm against the server source why a `DateTime('tz')` column's emitted type
  string is protocol-revision gated (see the 2026-06-05 decision log entry), and
  whether any analogous gate affects a type's data layout rather than only its
  name. The temporal data layouts are revision independent and confirmed, so
  this is documentation rigor, not a known correctness gap.
</content>
</invoke>
