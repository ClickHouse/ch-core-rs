# ch-core-rs

Reusable Rust core for decoding the ClickHouse **Native** binary format into a
columnar in-memory layout, with a zero-copy **Arrow C Data Interface** export.

This crate is **pure Rust with zero dependencies** and no language bindings. It is
the single shared core consumed by language-specific binding crates that each live
in their own client repo:

- **Python** — a PyO3 binding (`_chc`) in the `clickhouse-connect` repo.
- **JavaScript/Node** — a napi-rs binding (`.node` addon) in the `clickhouse-js` repo.

Each binding depends on this crate as a Cargo dependency and adds its own
language-specific materialization layer (read formats, null handling, encoding,
timezone, number-vs-bigint policy, etc.). Only this core is shared; the glue is not.

## Consuming it

Local development (path dependency):

```toml
[dependencies]
ch-core-rs = { path = "/path/to/ch-core-rs" }
```

Production edge (pin by git tag):

```toml
[dependencies]
ch-core-rs = { git = "https://…/ch-core-rs", tag = "v0.1.0" }
```

Use a local `[patch]` in `.cargo/config.toml` to override the git dependency with a
path checkout during development.

## Layout

- `src/native/` — Native-format decode (varint, block/column decoders, push StreamDecoder)
- `src/column.rs`, `src/batch.rs`, `src/schema.rs`, `src/bitmap.rs` — columnar model
- `src/ffi.rs` — Arrow C Data Interface export (`ArrowSchema`/`ArrowArray`/`ArrowArrayStream`)

## Status

Decodes Bool, Int/UInt 8–64, Float32/64, String, FixedString, and Nullable wrappers.
Temporal/Decimal/LowCardinality/containers are not yet implemented.
