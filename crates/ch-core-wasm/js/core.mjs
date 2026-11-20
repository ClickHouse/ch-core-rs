// JS glue over the ch-core-wasm C ABI (see ../ABI.md). Runtime-agnostic: the
// Node and web entries instantiate the module and pass its exports here.
//
// Memory rule: nothing this module returns is a view into wasm memory. Every
// buffer in a result is copied into a JS-owned TypedArray, then the result is
// freed on the Rust side. Any call into the module may grow its memory, which
// detaches the old `memory.buffer`, so the buffer is re-read after every call.

/** The C ABI version this glue speaks. */
export const ABI_VERSION = 1

const CODES = { 1: 'InvalidArg', 2: 'GenericFailure', 3: 'OutOfMemory' }

const CTORS = {
  i8: Int8Array,
  u8: Uint8Array,
  i16: Int16Array,
  u16: Uint16Array,
  i32: Int32Array,
  u32: Uint32Array,
  i64: BigInt64Array,
  u64: BigUint64Array,
  f32: Float32Array,
  f64: Float64Array,
}

const utf8Decoder = new TextDecoder('utf-8')
const utf8Encoder = new TextEncoder()

/** An error raised by the codec. `code` is 'InvalidArg', 'GenericFailure' or 'OutOfMemory'. */
export class CodecError extends Error {
  constructor(code, message) {
    super(message)
    this.name = 'CodecError'
    this.code = code
  }
}

const asBytes = (input) => {
  if (input instanceof Uint8Array) return input
  if (ArrayBuffer.isView(input)) return new Uint8Array(input.buffer, input.byteOffset, input.byteLength)
  if (input instanceof ArrayBuffer) return new Uint8Array(input)
  throw new TypeError('expected a Uint8Array, ArrayBuffer or ArrayBuffer view')
}

/**
 * Wrap the exports of an instantiated ch-core-wasm module.
 * Throws if the module speaks a different ABI version than this glue.
 */
export function bindExports(x) {
  const abi = x.chw_abi_version()
  if (abi !== ABI_VERSION) {
    throw new Error(`ch-core-wasm: the .wasm speaks ABI ${abi}, this glue expects ${ABI_VERSION}`)
  }
  // wasm returns i32; pointers and lengths are unsigned.
  const u32 = (n) => n >>> 0
  const memory = () => x.memory.buffer

  function lastError() {
    const code = x.chw_last_error_code()
    const ptr = u32(x.chw_last_error_ptr())
    const len = u32(x.chw_last_error_len())
    const message = len ? utf8Decoder.decode(new Uint8Array(memory(), ptr, len)) : 'unknown error'
    return new CodecError(CODES[code] ?? 'GenericFailure', message)
  }

  function alloc(len) {
    const ptr = u32(x.chw_alloc(len))
    if (len > 0 && ptr === 0) throw lastError()
    return ptr
  }

  /** Copy `input` into wasm memory for the duration of `fn(ptr, len)`. */
  function withInput(input, fn) {
    const bytes = asBytes(input)
    const ptr = alloc(bytes.byteLength)
    try {
      new Uint8Array(memory(), ptr, bytes.byteLength).set(bytes)
      return fn(ptr, bytes.byteLength)
    } finally {
      x.chw_free(ptr, bytes.byteLength)
    }
  }

  /** Read a result, copy every referenced buffer out, then free the result. */
  function take(handle) {
    if (handle === 0) throw lastError()
    try {
      const ptr = u32(x.chw_result_json_ptr(handle))
      const len = u32(x.chw_result_json_len(handle))
      // No calls into the module between here and the end of JSON.parse, so
      // `buf` stays attached for the whole copy.
      const buf = memory()
      const text = utf8Decoder.decode(new Uint8Array(buf, ptr, len))
      return JSON.parse(text, (_key, v) => {
        if (v !== null && typeof v === 'object' && Array.isArray(v.$b)) {
          const [p, n, dtype] = v.$b
          return new CTORS[dtype](buf.slice(p, p + n))
        }
        return v
      })
    } finally {
      x.chw_result_free(handle)
    }
  }

  /**
   * Decode a complete `FORMAT Native` body into
   * `{ rowCount, columnNames, columnTypes, chunks: [{ rowCount, columns }] }`.
   */
  function decodeNativeColumns(bytes) {
    return withInput(bytes, (ptr, len) => take(x.chw_decode(ptr, len)))
  }

  /** Decode one SharedVariant / JSON shared-data cell into `{ type, column }`. */
  function decodeBinaryValue(bytes) {
    return withInput(bytes, (ptr, len) => take(x.chw_decode_binary_value(ptr, len)))
  }

  /** Parse a ClickHouse type string with the core's parser. */
  function describeType(type) {
    return withInput(utf8Encoder.encode(type), (ptr, len) => take(x.chw_describe_type(ptr, len)))
  }

  /**
   * Encode one block `{ rowCount, columns: [{ name, type, ...buffers }] }`,
   * the shape of one decoded chunk, into Native bytes for
   * `INSERT ... FORMAT Native`. Concatenate blocks for a multi-block body.
   */
  function encodeNative(block) {
    // Pack every TypedArray into one data blob at 8-byte aligned offsets and
    // replace it in the descriptor with a reference to its range.
    const parts = []
    let size = 0
    const desc = JSON.stringify(block, function (key, v) {
      // Read the raw property: JSON.stringify has already applied toJSON, which
      // turns a Node Buffer into { type, data } before the replacer sees it.
      const raw = this[key]
      if (ArrayBuffer.isView(raw)) {
        const off = Math.ceil(size / 8) * 8
        parts.push([off, raw])
        size = off + raw.byteLength
        return { $b: [off, raw.byteLength] }
      }
      return v
    })
    const descBytes = utf8Encoder.encode(desc)
    const dataPtr = alloc(size)
    let descPtr = 0
    try {
      descPtr = alloc(descBytes.byteLength)
      const mem = new Uint8Array(memory())
      mem.set(descBytes, descPtr)
      for (const [off, v] of parts) {
        mem.set(new Uint8Array(v.buffer, v.byteOffset, v.byteLength), dataPtr + off)
      }
      return take(x.chw_encode(descPtr, descBytes.byteLength, dataPtr, size)).bytes
    } finally {
      x.chw_free(dataPtr, size)
      if (descPtr) x.chw_free(descPtr, descBytes.byteLength)
    }
  }

  // A decoder dropped without free() releases its Rust state when collected.
  const streams = new FinalizationRegistry((handle) => x.chw_stream_free(handle))

  /**
   * Incremental Native decoder for HTTP response streams. `push` returns
   * every block that is now complete; `finish` drains the tail and fails on a
   * truncated final block. After any error the decoder is poisoned.
   */
  class NativeStreamDecoder {
    #handle

    constructor() {
      this.#handle = x.chw_stream_new()
      streams.register(this, this.#handle, this)
    }

    #live() {
      if (!this.#handle) throw new CodecError('InvalidArg', 'NativeStreamDecoder was freed')
      return this.#handle
    }

    push(bytes) {
      const h = this.#live()
      return withInput(bytes, (ptr, len) => take(x.chw_stream_push(h, ptr, len)))
    }

    finish() {
      return take(x.chw_stream_finish(this.#live()))
    }

    /** Rows emitted so far. A u64 in the ABI, exact here up to 2^53. */
    get rowCount() {
      return Number(BigInt.asUintN(64, x.chw_stream_row_count(this.#live())))
    }

    get bufferedBytes() {
      return u32(x.chw_stream_buffered_bytes(this.#live()))
    }

    /** Release the decoder's state now instead of at garbage collection. */
    free() {
      if (!this.#handle) return
      streams.unregister(this)
      x.chw_stream_free(this.#handle)
      this.#handle = 0
    }
  }

  return {
    abiVersion: abi,
    decodeNativeColumns,
    decodeBinaryValue,
    describeType,
    encodeNative,
    NativeStreamDecoder,
  }
}
