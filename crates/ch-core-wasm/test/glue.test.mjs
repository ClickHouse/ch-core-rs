// End-to-end tests of the JS glue over the real .wasm, against committed
// server fixtures. Run `npm run build` first. No ClickHouse server needed.
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { bindExports, ABI_VERSION } from '../js/core.mjs'
import {
  decodeNativeColumns,
  describeType,
  encodeNative,
  NativeStreamDecoder,
  CodecError,
} from '../js/node.mjs'

const fixture = (name) => new Uint8Array(readFileSync(new URL(`../../../tests/fixtures/${name}`, import.meta.url)))
const ALL_TYPES = fixture('all_types_rev0.native')
const MULTI_BLOCK = fixture('multi_block_rev0.native')
// Rev-0 fixtures in the server's default serialization. json_flattened is
// left out: the core decodes that shape but does not write it.
const REV0 = [
  'all_types_rev0.native',
  'multi_block_rev0.native',
  'json_mixed_rows_rev0.native',
  'json_multipart_rev0.native',
  'json_shared_rev0.native',
  'json_string_rev0.native',
  'zero_width_dynamic_rev0.native',
]

const concat = (parts) => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0))
  let o = 0
  for (const p of parts) {
    out.set(p, o)
    o += p.length
  }
  return out
}

/** Every TypedArray reachable from `v`. */
function* typedArrays(v) {
  if (ArrayBuffer.isView(v)) yield v
  else if (Array.isArray(v)) for (const i of v) yield* typedArrays(i)
  else if (v && typeof v === 'object') for (const i of Object.values(v)) yield* typedArrays(i)
}

function instance() {
  const bytes = readFileSync(new URL('../dist/ch_core_wasm.wasm', import.meta.url))
  const { exports } = new WebAssembly.Instance(new WebAssembly.Module(bytes), {})
  return { exports, api: bindExports(exports) }
}

test('the .wasm speaks the ABI version the glue expects', () => {
  const { exports } = instance()
  assert.equal(exports.chw_abi_version(), ABI_VERSION)
  assert.throws(() => bindExports({ ...exports, chw_abi_version: () => ABI_VERSION + 1 }), /ABI/)
})

for (const name of REV0) {
  test(`decode -> encode is byte-identical: ${name}`, () => {
    const bytes = fixture(name)
    const decoded = decodeNativeColumns(bytes)
    assert.ok(decoded.chunks.length > 0)
    assert.deepEqual(concat(decoded.chunks.map(encodeNative)), bytes)
  })
}

test('results are JS-owned copies that survive memory growth', () => {
  const { exports, api } = instance()
  const decoded = api.decodeNativeColumns(ALL_TYPES)
  const arrays = [...typedArrays(decoded)]
  assert.ok(arrays.length > 100)
  const before = arrays.map((a) => Buffer.from(a.buffer, a.byteOffset, a.byteLength).toString('hex'))
  for (const a of arrays) assert.notEqual(a.buffer, exports.memory.buffer, 'no views into wasm memory')

  // Force memory.grow(), which detaches the old ArrayBuffer, then reuse the
  // freed memory by decoding again.
  const old = exports.memory.buffer
  const big = 64 * 1024 * 1024
  const p = exports.chw_alloc(big) >>> 0
  assert.notEqual(p, 0)
  exports.chw_free(p, big)
  assert.equal(old.byteLength, 0, 'growth detached the old buffer')
  api.decodeNativeColumns(ALL_TYPES)

  const after = arrays.map((a) => Buffer.from(a.buffer, a.byteOffset, a.byteLength).toString('hex'))
  assert.deepEqual(after, before)
  assert.deepEqual(concat(decoded.chunks.map(api.encodeNative)), ALL_TYPES)
})

test('typed buffers use the matching TypedArray per kind', () => {
  const cols = Object.fromEntries(decodeNativeColumns(ALL_TYPES).chunks[0].columns.map((c) => [c.name, c]))
  const byKind = {}
  for (const c of Object.values(cols)) byKind[c.kind] ??= c
  assert.ok(byKind.Int64.values instanceof BigInt64Array)
  assert.ok(byKind.UInt64.values instanceof BigUint64Array)
  assert.ok(byKind.Float64.values instanceof Float64Array)
  assert.ok(byKind.String.offsets instanceof Int32Array)
  assert.ok(byKind.String.data instanceof Uint8Array)
  assert.ok(byKind.Array.offsets instanceof BigInt64Array)
})

test('streaming matches the buffered decode at any split', () => {
  const whole = decodeNativeColumns(MULTI_BLOCK)
  for (const step of [1, 7, MULTI_BLOCK.length]) {
    const d = new NativeStreamDecoder()
    const chunks = []
    for (let i = 0; i < MULTI_BLOCK.length; i += step) {
      chunks.push(...d.push(MULTI_BLOCK.subarray(i, i + step)).chunks)
    }
    const last = d.finish()
    chunks.push(...last.chunks)
    assert.deepEqual(chunks, whole.chunks)
    assert.deepEqual(last.columnNames, whole.columnNames)
    assert.equal(d.rowCount, whole.rowCount)
    assert.equal(d.bufferedBytes, 0)
    d.free()
    d.free()
    assert.throws(() => d.push(MULTI_BLOCK), /freed/)
  }
})

test('empty body is an empty result', () => {
  const r = decodeNativeColumns(new Uint8Array())
  assert.deepEqual(r, { rowCount: 0, columnNames: [], columnTypes: [], chunks: [] })
  const d = new NativeStreamDecoder()
  assert.equal(d.push(new Uint8Array()).rowCount, 0)
  assert.deepEqual(d.finish().chunks, [])
  d.free()
})

test('errors carry codes and the module keeps working', () => {
  assert.throws(
    () => decodeNativeColumns(new Uint8Array([1, 1, 1, 120, 3, 70, 111, 111])),
    (e) => e instanceof CodecError && e.code === 'InvalidArg',
  )
  const d = new NativeStreamDecoder()
  d.push(MULTI_BLOCK.subarray(0, MULTI_BLOCK.length - 3))
  assert.throws(() => d.finish(), { code: 'GenericFailure' })
  assert.throws(() => d.push(MULTI_BLOCK), /create a new stream/)
  d.free()

  const once = new NativeStreamDecoder()
  once.push(new Uint8Array([1, 1, 1, 120, 5, 85, 73, 110, 116, 56, 7])) // x UInt8 = 7
  once.finish()
  assert.throws(() => once.push(new Uint8Array([0])), /after finish/)
  assert.throws(() => once.finish(), /more than once/)
  once.free()

  assert.throws(() => encodeNative({ rowCount: 2, columns: [{ name: 'x', type: 'UInt8', values: new Uint8Array([7]) }] }), {
    code: 'InvalidArg',
  })
  assert.equal(decodeNativeColumns(ALL_TYPES).chunks[0].columns.length, 116)
})

test('zero-width rows past the 32-bit address space fail instead of wrapping', () => {
  // On wasm32 the encoder's output size once wrapped here, returning a
  // truncated block. Native tests cannot see this: they run on 64 bits.
  for (const type of ['Nothing', 'Tuple()']) {
    const column = type === 'Nothing' ? { name: 'z', type } : { name: 'z', type, fields: [] }
    assert.throws(() => encodeNative({ rowCount: 2 ** 32 - 1, columns: [column] }), { code: 'InvalidArg' })
  }
  const arr = { name: 'a', type: 'Array(Nothing)', offsets: new BigInt64Array([0n, 3_000_000_000n]), values: {} }
  assert.throws(() => encodeNative({ rowCount: 1, columns: [arr] }), { code: 'InvalidArg' })
  assert.equal(decodeNativeColumns(ALL_TYPES).chunks[0].columns.length, 116, 'module still works')
})

test('encode accepts Node Buffers like any other TypedArray', () => {
  const bytes = encodeNative({ rowCount: 2, columns: [{ name: 'x', type: 'UInt8', values: Buffer.from([13, 79]) }] })
  assert.deepEqual(decodeNativeColumns(bytes).chunks[0].columns[0].values, new Uint8Array([13, 79]))
})

test('encode takes plain TypedArrays', () => {
  const bytes = encodeNative({
    rowCount: 2,
    columns: [
      { name: 'id', type: 'UInt64', values: new BigUint64Array([13n, 79n]) },
      {
        name: 'user',
        type: 'Nullable(String)',
        offsets: new Int32Array([0, 6, 6]),
        data: new TextEncoder().encode('user_1'),
        validity: new Uint8Array([0b01]),
      },
    ],
  })
  const [chunk] = decodeNativeColumns(bytes).chunks
  assert.deepEqual(chunk.columns[0].values, new BigUint64Array([13n, 79n]))
  assert.deepEqual(chunk.columns[1].validity, new Uint8Array([0b01]))
  assert.equal(new TextDecoder().decode(chunk.columns[1].data), 'user_1')
})

test('describeType exposes the core type parser', () => {
  const t = describeType("Map(LowCardinality(String), Array(Nullable(Decimal(18, 4))))")
  assert.equal(t.kind, 'Map')
  assert.equal(t.key.kind, 'LowCardinality')
  assert.equal(t.value.element.kind, 'Decimal')
  assert.equal(t.value.element.nullable, true)
  assert.equal(t.value.element.scale, 4)
  assert.throws(() => describeType('NotAType('), { code: 'InvalidArg' })
})
