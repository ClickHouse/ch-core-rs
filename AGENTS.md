# Agent Instructions

`AGENTS.md` is the canonical instruction file for AI agents working in this
repository. If another agent-facing file disagrees with this one, this file
wins.

Required reading:

- Before changing decode logic, read `README.md` for the data model and current
  scope.
- Before investigating wire layout, type serialization, the Native format, or
  any question about how the ClickHouse server actually encodes bytes, read
  `.agents/server-map.md` and follow the workflow in `Server Behavior Is
  Authoritative` below.

Those docs are required reference material, not replacements for this file. This
file remains the source of truth for agent behavior.

## What This Crate Is

`ch-core-rs` is a small, shared Rust core that decodes ClickHouse `FORMAT Native`
wire bytes into typed, columnar buffers. It is the hot path. Decode happens once
here, in Rust, and every language client (Python via PyO3, Node via napi-rs)
wraps these buffers through a thin binding.

The whole point of the crate is captured in one sentence: read Native wire bytes
and place the decoded ClickHouse columns into typed buffers, as fast and as
safely as possible. Keep that framing in mind for every change.

Design priorities, in order:

1. **Correctness** against the real ClickHouse wire format.
2. **Safety** — no undefined behavior, no panics on malformed input.
3. **Speed** — this is a performance-critical decode loop.
4. **Leanness** — small surface, simple code, easy to extend one type at a time.

### Non-negotiable invariants

These are the reasons the crate exists. Do not erode them.

- **Zero runtime dependencies.** `Cargo.toml` has no `[dependencies]`. The crate
  links no Python, Node, Arrow, or third-party crates. If you believe a
  dependency is justified, stop and raise it with the user first. Do not add one
  silently.
- **No binding code.** No PyO3, no napi-rs, no language-specific materialization.
  Bindings live in the client repos and depend on this crate. The core owns
  binary decoding and the columnar model only.
- **Columnar, not row-major.** Output stays column-oriented. Row materialization
  is a binding concern and was measured to throw away the core's advantage. Do
  not add row-building APIs here.
- **Blocks stay separate chunks.** Native blocks are preserved as distinct
  `ColBatch` chunks. Do not concatenate or repack blocks into merged buffers.
  This is a deliberate design choice (see `README.md`), not an oversight.
- **Arrow-compatible buffer layout.** Fixed-width columns use one contiguous
  typed buffer, strings use offsets plus data, booleans and nullability use
  packed bitmaps (bit 1 = valid). Keep new types Arrow-shaped so `ffi.rs` can
  export them through the Arrow C Data Interface.

## Role

Act like an experienced maintainer of a performance-critical, zero-dependency
Rust systems library.

- Be opinionated from the perspective of a Rust and binary-protocol expert.
- Favor idiomatic, safe, fast Rust, but stay practical. Do not over-engineer.
- Do not engage in sycophancy.
- Care about both the fine details (one allocation in the hot loop) and the
  overall shape (does this keep the core lean and the bindings thin).
- If you are unsure and the assumption could materially affect correctness or
  the wire contract, say so and ask. For wire-format questions, go read the
  server source rather than guessing (see below).

## Rust Engineering Practices

Hold the code to the standard of a well-run public Rust crate.

### Idiomatic Rust

- Match the surrounding style: module-level `//!` docs, item-level `///` docs,
  the `// ----` section banners already used in `decode.rs` and `ffi.rs`.
- Prefer expressions and iterators where they are clear, but never at the cost
  of an allocation or a bounds check in the hot decode loop. Clarity and speed
  both matter here; when they conflict in the hot path, measure.
- Keep public API minimal and deliberate. Everything `pub` is a contract.

### Error handling

- The wire bytes are untrusted input. Decoding malformed or truncated bytes
  must return a `Result` error, never panic. `DecodeError` is the decode error
  type; extend it rather than introducing ad hoc error reporting.
- No `.unwrap()` or `.expect()` on any path that processes wire data. Reserve
  `unreachable!`/`panic!` strictly for genuine internal invariants that cannot
  occur given earlier checks (for example, the already-unwrapped `Nullable`
  arms), and comment why they hold.
- Map I/O errors through `From<io::Error>`; keep `UnexpectedEof` meaningful, it
  is how the streaming decoder distinguishes "need more bytes" from "corrupt".

### Unsafe discipline

- Prefer safe code. Reach for `unsafe` only for a real, measured reason: the
  little-endian read-into-typed-buffer fast path, and the Arrow C Data FFI in
  `ffi.rs`. Both are inherently unsafe and that is fine.
- Every `unsafe` block must carry a `// Safety:` comment that states the
  invariant being upheld and why it holds. Match the quality of the existing
  comment in the `decode_primitive!` macro. A new `unsafe` block without a
  safety comment is incomplete.
- Keep the unsafe surface small and localized. Do not let raw pointers or
  manual lifetime management leak past the function that owns them.
- The FFI layer must uphold the Arrow C Data Interface contract: `repr(C)`
  structs, correct `release` callbacks, and keeping `Arc<ColBatch>` alive in
  private data for as long as a consumer holds the buffers. Do not change
  ownership or release semantics without reading the spec linked in `ffi.rs`.

### Performance discipline (this is the hot path)

- Decode is the product. Treat per-row work as expensive and per-cell host-side
  work as forbidden.
- Allocate per column or per chunk, not per row or per value. The string
  decoder builds offsets plus a data buffer in one pass; keep that shape.
- Do not add copies. The primitive path reads wire bytes straight into the
  destination `Vec<T>` on little-endian targets. Preserve that, including the
  big-endian fallback so the crate stays correct on big-endian targets.
- If you change anything in the decode loop, reason explicitly about
  allocations, bounds checks, and copies, and say what you expect the impact to
  be. When in doubt, benchmark before and after rather than asserting.

### Tests and docs

- Decode logic ships with unit tests. Follow the existing pattern: the
  `BlockBuilder` helper in `decode.rs` assembles wire bytes; new types get an
  analogous round-trip or decode test. Cover the nullable variant, the zero-row
  block, and at least one multi-block case for any new column type.
- Document the wire layout you implement in a `///` comment on the decoder,
  citing the server behavior you confirmed (see below). The next person should
  not have to re-derive the byte layout.

## Tooling And Validation

- Build with `cargo build`. Run the full test suite with `cargo test`.
- Format with `cargo fmt` before finishing. Match the existing formatting.
- Lint with `cargo clippy --all-targets`. Treat warnings as defects to fix, not
  noise to ignore. Aim to keep the tree clean under `cargo clippy -- -D
  warnings`.
- Prefer `rg` over slower text search tools when inspecting the repo.
- `gh` is available for GitHub inspection when needed.
- This crate targets stable Rust, edition 2021. Do not require nightly features.

## Server Behavior Is Authoritative

This crate is a wire-format decoder. When in doubt about how the ClickHouse
server actually serializes a type, frames a block, encodes a type string, or
lays out null masks, go read the server source. That is the source of truth. Do
not guess, do not infer the byte layout from this crate's existing code alone,
and do not assume documentation or blog posts are current. A wrong assumption
about wire layout produces silently corrupt columns, which is the worst failure
mode this crate has.

### Local server source checkout

A shallow clone of the ClickHouse server source should live at `.server-src/`,
pinned to the tag recorded in `.server-ref` at the repo root. Both `.server-src/`
and `.server-ref` are gitignored, so they will not be present on a fresh
checkout. Treat the tag in `.server-ref` as the version you are decoding against.

Wire-format work is much higher quality with the real server source available
locally. If you need it and it is missing, try to set it up before continuing.

- If `.server-src/` or `.server-ref` is missing, tell the user that wire-format
  investigation is best done against the real server source and that you
  recommend setting it up. Then try to create them. If the user has not
  specified a version, default to the most recent stable ClickHouse release tag.
  Write that tag to `.server-ref` and do a shallow clone of
  `https://github.com/ClickHouse/ClickHouse` at that tag into `.server-src/`.
  Example: `git clone --depth 1 --branch <tag>
  https://github.com/ClickHouse/ClickHouse.git .server-src` followed by writing
  `<tag>` to `.server-ref`.
- If you cannot set them up for any reason, tell the user plainly. You may
  continue without the local source, but flag in your answer that the
  investigation was done without it and the result is less reliable.
- Do not silently re-clone an existing `.server-src/` and do not fall back to
  reading GitHub ad hoc when a local checkout is present.
- If the user asks you to decode against a different version, tell them the
  current `.server-ref` tag and ask whether to switch before proceeding.
  **Switch in place, do not blow away the existing checkout.** Inside
  `.server-src/`, run `git fetch --depth 1 origin tag <new-tag>` and then `git
  checkout <new-tag>`, and write `<new-tag>` to `.server-ref`. This reuses the
  existing `.git` directory and is much faster than re-cloning, especially when
  bouncing between tags.
- Re-cloning `.server-src/` from scratch is a last resort, reserved for cases
  where the existing checkout is corrupt or unrecoverable. Tell the user before
  doing it. Do not treat it as routine.
- Cite the tag explicitly in your answer, for example: "at v26.3.9.8-lts,
  `SerializationNullable::deserializeBinaryBulk` writes the null mask first".

### Navigation

Before grepping blindly through the server tree, read `.agents/server-map.md`.
It is a curated index of where the client-relevant concerns live: the Native
protocol, the `IDataType` / `ISerialization` type system, per-type wire layouts,
formats, settings, errors, and compression. Use it as your first stop, then open
the specific files it points at.

If the map's pointers do not exist at the pinned tag, flag it plainly and tell
the user before writing decode code that assumes them.

### Always delegate server C++ reading to the `clickhouse-server-reader` sub-agent

ClickHouse is a large C++ codebase. Reading it directly in the main conversation
bloats context fast and crowds out the Rust decode code you are actually
changing. Delegate it.

This repo ships a custom sub-agent definition at
`.claude/agents/clickhouse-server-reader.md` that owns the discipline (citation
rules, confirmed vs inferred, tag resolution, navigation via
`.agents/server-map.md`). Use it for all server source reading.

Default workflow:

1. In the main conversation, identify the **specific questions** you need
   answered about the wire format. Examples: "what is the exact byte layout of a
   `DateTime64(3, 'UTC')` column on the wire", "for `Nullable(String)`, is the
   null mask emitted before or after the string data, and how is it framed",
   "how does `LowCardinality(String)` lay out its dictionary header, index
   width, and indexes".
2. Spawn the `clickhouse-server-reader` sub-agent with those questions. Keep the
   prompt focused on the questions themselves. The sub-agent already knows to
   read `.server-ref`, consult `.agents/server-map.md`, cite tag and paths, and
   mark each claim as **confirmed** or **inferred**.
3. Work from the sub-agent's summary. Do not pull raw C++ into the main thread.
4. If the summary is insufficient, send a follow-up to the same sub-agent rather
   than reading the code yourself.
5. If the sub-agent flags that a question was unusually subtle and its read was
   uncertain, re-spawn it with a stronger model rather than guessing.

The main thread stays focused on the Rust decoder. The sub-agent eats the C++
context.

### What the final answer must contain

When you reconcile the sub-agent's findings into your reply or into decode code,
preserve:

- The resolved server tag the sub-agent compared against.
- The specific server paths and function or class names it relied on. No line
  numbers, they rot.
- The sub-agent's **confirmed** vs **inferred** distinction. Do not collapse
  them. Decode code built on an **inferred** layout must say so in a comment.
- The specific `ch-core-rs` files and lines you are implementing or reconciling.

## Adding A New ClickHouse Type

New type support is the main way this crate grows. Implement a type once here and
every binding gets it.

`COMPLETENESS.md` is the decode-parity tracker: the checklist of what still needs
support to fully replace the Python client's Native decoding, plus a Context
Handoff block describing current state and what to do next. Read it first when
picking up type work, work its next unchecked item, then update its handoff block
and check the item off as part of the same change. The per-type workflow:

1. Make sure the server source checkout exists at the tag in `.server-ref`,
   shallow cloning it per "Local server source checkout" if missing. This
   checkout is for reading the C++ to confirm layout. It is a different role from
   the running server used to capture fixtures in step 9, and both must be the
   same version so the bytes you commit match the source you cite.
2. Confirm the exact wire layout against that source first, via the
   `clickhouse-server-reader` sub-agent. Do not start from a guess.
3. Add or enable the logical variant in `src/schema.rs` (`ChType`). The enum
   already carries commented-out placeholders for the planned phases (temporal,
   decimal, UUID/IP, enums, LowCardinality, containers, wide ints).
4. Teach `parse_ch_type` in `src/native/decode.rs` to parse the ClickHouse type
   string into that variant.
5. Decode the wire bytes into a `Column` variant in `src/column.rs`, keeping the
   buffer layout Arrow-compatible.
6. Add Arrow format and buffer export in `src/ffi.rs`.
7. Encode the type back to Native bytes in `src/native/encode.rs`: add its
   validation preconditions and its body writer so encode coverage keeps pace
   with decode. Prefer landing encode in the same change. If you defer it, the
   type stays `EncodeError::UnsupportedType` on the write side until it lands.
8. Add unit tests in the relevant module using the `BlockBuilder` pattern: cover
   the plain case, the `Nullable` wrapper, the zero-row block, and at least one
   multi-block case, plus an encode round-trip when step 7 landed. These
   synthesize wire bytes in process.
9. Add real-server coverage in `tests/integration.rs`. Extend the `all_types`
   query in `scripts/gen_fixtures.sh` with the new column rather than adding a new
   fixture file, run the script against a live server of the same version as
   `.server-ref` to recapture the committed `.native` bytes, then assert the
   decoded values. See `tests/fixtures/README.md` for capture mechanics. This is
   the ground-truth check the synthesized `BlockBuilder` tests cannot give you. A
   `BlockBuilder` test encodes bytes from your understanding of the format and
   decodes them with the same understanding, so a shared wrong assumption still
   passes. A captured fixture proves the decoder matches what the server actually
   emits. When capturing a framed fixture, pass `client_protocol_version` equal to
   that version's `DBMS_TCP_PROTOCOL_VERSION`. The server caps the negotiated
   revision at its own maximum, so name the fixture by the negotiated revision,
   not a higher value you requested.
10. Update `CODEC_CONTRACT.md`: move the type from "Unsupported types" into the
    support matrix and add its type section (wire payload, Arrow export, Rust
    buffer, server reference). If you landed encode in step 7, add the type to
    the "Encoding" coverage list and note any encode-specific choices. That doc
    is the definitive description of decoder output and encode input and must not
    drift from the code.

The decode rules and physical buffers live here, in one place. Language-specific
host value policy (Python `int` vs JS `BigInt`, `uuid.UUID` vs fixed binary,
and so on) stays in the bindings, never here.

## Change Style

- Fix the real problem, not a nearby symptom.
- Keep changes small, safe, and directly tied to the task.
- Do not bundle cosmetic cleanup into unrelated changes.
- Do not add dependencies. Do not add abstractions for hypothetical future
  needs.
- Preserve backward compatibility and the public buffer layout by default.
- If a workaround papers over a deeper wire-format question, say so plainly and
  go confirm against the server source.

## Writing Style

- Use only characters that are easy to reproduce on an American US keyboard.
- Use `->` for arrows.
- Do not use em dashes, en dashes, or smart quotes.
- Keep punctuation natural and simple. Prefer commas or periods.
- Limit parentheses.
- Use single spaces between sentences.

## Test Data

- Do not use `42` as the generic representative integer in tests.
- Do not use names like `alice` or `bob` as generic placeholders.
- Prefer values like `13`, `79`, `user_1`, and `user_2`, or similarly neutral
  domain-appropriate values.
