// Browser, Deno and Bun entry. Call `await init()` once before anything else.
// By default `init()` fetches dist/ch_core_wasm.wasm relative to this module;
// pass a URL, Response, BufferSource or WebAssembly.Module to load it from
// elsewhere.
import { bindExports, CodecError } from './core.mjs'

export { ABI_VERSION, CodecError } from './core.mjs'

let api
let ready

async function instantiate(source) {
  if (source instanceof WebAssembly.Module) return WebAssembly.instantiate(source, {})
  if (source instanceof ArrayBuffer || ArrayBuffer.isView(source)) {
    return (await WebAssembly.instantiate(source, {})).instance
  }
  const response = await (typeof source === 'string' || source instanceof URL ? fetch(source) : source)
  if (!response.ok) throw new Error(`ch-core-wasm: fetching the .wasm failed with HTTP ${response.status}`)
  if (typeof WebAssembly.instantiateStreaming === 'function') {
    try {
      return (await WebAssembly.instantiateStreaming(response.clone(), {})).instance
    } catch {
      // Served without `application/wasm`: fall back to a buffered compile.
    }
  }
  return (await WebAssembly.instantiate(await response.arrayBuffer(), {})).instance
}

export function init(source = new URL('../dist/ch_core_wasm.wasm', import.meta.url)) {
  ready ??= instantiate(source).then(
    (instance) => {
      api = bindExports(instance.exports)
    },
    (err) => {
      // Let a later init() retry instead of keeping the failure forever.
      ready = undefined
      throw err
    },
  )
  return ready
}

const loaded = () => {
  if (!api) throw new CodecError('InvalidArg', 'ch-core-wasm is not initialized; await init() first')
  return api
}

export const decodeNativeColumns = (bytes) => loaded().decodeNativeColumns(bytes)
export const decodeBinaryValue = (bytes) => loaded().decodeBinaryValue(bytes)
export const describeType = (type) => loaded().describeType(type)
export const encodeNative = (block) => loaded().encodeNative(block)

/** See `NativeStreamDecoder` in core.mjs. Requires `init()` first. */
export class NativeStreamDecoder {
  constructor() {
    return new (loaded().NativeStreamDecoder)()
  }

  // The constructor returns the bound class's instance, so make instanceof
  // recognize those.
  static [Symbol.hasInstance](value) {
    return api !== undefined && value instanceof api.NativeStreamDecoder
  }
}
