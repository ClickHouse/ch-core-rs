// Types for both entries (Node and web). Column objects are columnar: every
// buffer is a JS-owned TypedArray copied out of wasm memory, never a view.

/** The C ABI version this glue speaks. */
export const ABI_VERSION: number

export class CodecError extends Error {
  readonly code: 'InvalidArg' | 'GenericFailure' | 'OutOfMemory'
}

/**
 * One decoded column. `kind` is the physical layout and selects which
 * buffers are present (see README.md); `type` is the ClickHouse type string.
 * Nullable columns carry an Arrow-style `validity` bitmap (bit 1 = valid).
 */
export interface Column {
  kind: string
  type?: string
  name?: string
  validity?: Uint8Array
  values?: ArrayBufferView | Column
  offsets?: Int32Array | BigInt64Array
  data?: Uint8Array
  [field: string]: unknown
}

export interface Chunk {
  rowCount: number
  columns: Column[]
}

export interface DecodeResult {
  rowCount: number
  columnNames: string[]
  columnTypes: string[]
  chunks: Chunk[]
}

export interface StreamResult extends DecodeResult {
  bufferedBytes: number
}

export interface TypeDescription {
  type: string
  kind: string
  nullable: boolean
  [field: string]: unknown
}

type Bytes = Uint8Array | ArrayBuffer | ArrayBufferView

/** Resolves once the module is ready. A no-op on Node. */
export function init(source?: string | URL | Response | BufferSource | WebAssembly.Module): Promise<void>

/** Decode a complete `FORMAT Native` body. An empty body is an empty result. */
export function decodeNativeColumns(bytes: Bytes): DecodeResult

/** Decode one SharedVariant or JSON shared-data cell into a one-row column. */
export function decodeBinaryValue(bytes: Bytes): { type: string; column: Column }

/** Parse a ClickHouse type string with the core's parser. */
export function describeType(type: string): TypeDescription

/** Encode one block, in the shape of a decoded chunk, into Native bytes. */
export function encodeNative(block: { rowCount: number; columns: Column[] }): Uint8Array

/** Incremental decoder for HTTP response bodies. Poisoned after any error. */
export class NativeStreamDecoder {
  constructor()
  push(bytes: Bytes): StreamResult
  finish(): StreamResult
  readonly rowCount: number
  readonly bufferedBytes: number
  /** Release the decoder now instead of at garbage collection. */
  free(): void
}
