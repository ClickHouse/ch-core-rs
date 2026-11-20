# ch-core-wasm

`ch-core-rs` compiled to WebAssembly: one `.wasm` file with a small C ABI and
no imports, plus hand-written JS glue for Node.js, browsers, Deno and Bun.

- **Columnar only.** Decode Native bytes into typed column buffers, and encode
  column buffers into Native bytes. Turning columns into rows and the client
  methods (`insertNative`, `queryNativeRows`, `queryNativeColumns`) live in
  clickhouse-js.
- **Results are copies.** Every buffer is copied once out of wasm memory into a
  JS-owned TypedArray, then freed on the Rust side. Nothing returned is a view
  into wasm memory, so memory growth cannot empty or corrupt a result.
- **The C ABI is a contract**, versioned and documented in [ABI.md](ABI.md).
  The glue checks the version at load time.
- **No dependencies** beyond core. Core is built without its C `zstd` feature,
  so the build needs only the Rust `wasm32-unknown-unknown` target.

## Build and test

```sh
rustup target add wasm32-unknown-unknown
npm run build      # -> dist/ch_core_wasm.wasm (runs wasm-opt if it is on PATH)
npm test           # JS glue against the committed server fixtures
cargo test -p ch-core-wasm   # the ABI, natively on the host
```

`npm test` also runs a live-server suite when `CLICKHOUSE_URL` is set, for
example `CLICKHOUSE_URL=http://localhost:8123 npm test`. It reads about 55
types from the server and checks that each re-encodes to identical bytes.

## Use

```js
import { NativeStreamDecoder, encodeNative } from 'ch-core-wasm' // web: await init() first

const res = await fetch(url, { method: 'POST', body: 'SELECT number AS n FROM numbers(1e6) FORMAT Native' })
const decoder = new NativeStreamDecoder()
for await (const piece of res.body) {
  for (const chunk of decoder.push(piece).chunks) {
    chunk.columns[0].values // BigUint64Array, one per Native block
  }
}
decoder.finish()
decoder.free()

// A decoded chunk, or any block in the same shape, encodes back to Native.
const body = encodeNative({
  rowCount: 2,
  columns: [{ name: 'id', type: 'UInt64', values: new BigUint64Array([13n, 79n]) }],
})
```

The entry points are `decodeNativeColumns`, `NativeStreamDecoder`,
`encodeNative`, `describeType` (the core's type parser) and
`decodeBinaryValue` (one Dynamic or JSON shared cell). Column shapes are listed
in [ABI.md](ABI.md#column-objects) and typed in `js/index.d.ts`. Errors are
`CodecError`s with `code` `'InvalidArg'`, `'GenericFailure'` or `'OutOfMemory'`.

## Layout

```text
src/lib.rs        C ABI exports, handle table, errors
src/decode.rs     decoded columns -> descriptor JSON with buffer references
src/encode.rs     descriptor JSON + data blob -> columns -> Native bytes
src/describe.rs   chw_describe_type
src/json.rs       minimal JSON writer and parser for descriptors
js/core.mjs       glue: copy in, copy out, errors, stream decoder
js/node.mjs       Node entry (ready on import)
js/web.mjs        browser entry (await init())
test/             node:test suites
```
