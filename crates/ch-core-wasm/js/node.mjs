// Node.js entry: the module is compiled and instantiated synchronously on
// import, from dist/ch_core_wasm.wasm next to this package.
import { readFileSync } from 'node:fs'
import { bindExports } from './core.mjs'

const bytes = readFileSync(new URL('../dist/ch_core_wasm.wasm', import.meta.url))
const { exports } = new WebAssembly.Instance(new WebAssembly.Module(bytes), {})
const api = bindExports(exports)

export { ABI_VERSION, CodecError } from './core.mjs'
export const { decodeNativeColumns, decodeBinaryValue, describeType, encodeNative, NativeStreamDecoder } = api

/** No-op on Node, where the module is ready on import. Present for parity with the web entry. */
export async function init() {}
