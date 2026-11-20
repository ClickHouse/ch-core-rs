// Optional live-server coverage, skipped unless CLICKHOUSE_URL is set
// (CLICKHOUSE_USER and CLICKHOUSE_PASSWORD optional). Every type is read from
// a real server as FORMAT Native and must re-encode to the same bytes.
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { decodeNativeColumns, encodeNative, NativeStreamDecoder } from '../js/node.mjs'

const URL_ = process.env.CLICKHOUSE_URL
const skip = URL_ ? false : 'set CLICKHOUSE_URL to run against a live server'
const AUTH = `Basic ${btoa(`${process.env.CLICKHOUSE_USER ?? 'default'}:${process.env.CLICKHOUSE_PASSWORD ?? ''}`)}`

const TYPE_SETTINGS = {
  allow_experimental_variant_type: '1',
  allow_experimental_dynamic_type: '1',
  allow_experimental_json_type: '1',
  allow_experimental_qbit_type: '1',
  allow_experimental_bfloat16_type: '1',
  enable_time_time64_type: '1',
  allow_suspicious_low_cardinality_types: '1',
}

async function query(sql, settings = {}) {
  const url = new URL(URL_)
  for (const [k, v] of Object.entries({ ...TYPE_SETTINGS, ...settings })) url.searchParams.set(k, v)
  const res = await fetch(url, { method: 'POST', body: `${sql}\nFORMAT Native`, headers: { Authorization: AUTH } })
  if (!res.ok) throw new Error(`ClickHouse ${res.status}: ${await res.text()}`)
  return res
}

const nativeBytes = async (sql, settings) => new Uint8Array(await (await query(sql, settings)).arrayBuffer())

const concat = (parts) => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0))
  let o = 0
  for (const p of parts) (out.set(p, o), (o += p.length))
  return out
}

// [label, SQL expression, (optional) query settings]
const CASES = [
  ['Bool', 'true'],
  ['Int8', 'toInt8(-128)'],
  ['Int16', 'toInt16(-32768)'],
  ['Int32', 'toInt32(-2147483648)'],
  ['Int64', 'toInt64(-9223372036854775808)'],
  ['UInt8', 'toUInt8(255)'],
  ['UInt16', 'toUInt16(65535)'],
  ['UInt32', 'toUInt32(4294967295)'],
  ['UInt64', 'toUInt64(18446744073709551615)'],
  ['Int128', "toInt128('-170141183460469231731687303715884105728')"],
  ['UInt128', "toUInt128('340282366920938463463374607431768211455')"],
  ['Int256', "toInt256('-12345678901234567890123456789012345678901234567890')"],
  ['UInt256', "toUInt256('115792089237316195423570985008687907853269984665640564039457584007913129639935')"],
  ['Float32', 'toFloat32(1.5)'],
  ['Float64', 'toFloat64(-0.125)'],
  ['BFloat16', 'toBFloat16(1.5)'],
  ['String', "'héllo, 世界'"],
  ['FixedString', "toFixedString('ab', 4)"],
  ['Date', "toDate('2024-02-29')"],
  ['Date32', "toDate32('1900-01-01')"],
  ['DateTime', "toDateTime('2024-01-02 03:04:05', 'UTC')"],
  ['DateTime64(9)', "toDateTime64('2024-01-02 03:04:05.123456789', 9, 'UTC')"],
  ['Time', "toTime('12:34:56')"],
  ['Time64(3)', "toTime64('12:34:56.789', 3)"],
  ['IntervalDay', 'toIntervalDay(5)'],
  ['Decimal32', 'toDecimal32(-3.14, 2)'],
  ['Decimal256', "toDecimal256('12345678901234567890.123456789', 9)"],
  ['UUID', "toUUID('61f0c404-5cb3-11e7-907b-a6006ad3dba0')"],
  ['IPv4', "toIPv4('192.168.1.254')"],
  ['IPv6', "toIPv6('2001:db8:0:0:1:0:0:1')"],
  ['Enum8', "CAST('b', 'Enum8(\\'a\\' = -1, \\'b\\' = 2)')"],
  ['Enum16', "CAST('z', 'Enum16(\\'y\\' = 1000, \\'z\\' = -1000)')"],
  ['Nullable', "if(number = 1, NULL, toNullable('x'))"],
  ['Nothing', 'NULL'],
  ['LowCardinality', "toLowCardinality(toString(number % 2))"],
  ['LowCardinality(Nullable)', "CAST(if(number = 1, NULL, 'lc'), 'LowCardinality(Nullable(String))')"],
  ['Array(Nullable)', '[1, NULL, 3]'],
  ['Array(Array)', "[['a'], [], ['b', 'c']]"],
  ['Tuple named', "CAST((1, 'x'), 'Tuple(id UInt8, name String)')"],
  ['Nullable(Tuple)', "CAST(if(number = 1, NULL, tuple(7)), 'Nullable(Tuple(a UInt8))')"],
  ['Map(String, Nullable)', "map('x', NULL, 'y', toNullable(7))"],
  ['Variant', "CAST(if(number = 0, 'hi', if(number = 1, NULL, toUInt64(13))), 'Variant(String, UInt64)')"],
  ['Dynamic', "CAST(if(number = 0, toInt32(7), if(number = 1, NULL, 'str')), 'Dynamic')"],
  ['Dynamic array', "CAST([1, 2], 'Dynamic')"],
  ['Dynamic shared', "CAST(toInt16(5), 'Dynamic(max_types=0)')"],
  ['JSON', `CAST('{"a": 1, "b": {"c": "x", "d": [1, 2]}}', 'JSON')`],
  ['JSON typed path', `CAST('{"a": 1, "s": "str"}', 'JSON(a UInt8)')`],
  ['JSON shared data', `CAST('{"a": 1, "b": 2}', 'JSON(max_dynamic_paths=0)')`],
  ['JSON as string', `CAST('{"a": 1}', 'JSON')`, { output_format_native_write_json_as_string: '1' }],
  ['QBit', "CAST([1., 2., 3.], 'QBit(Float32, 3)')"],
  ['Point', "CAST((1.5, 2.5), 'Point')"],
  ['Ring', "CAST([(0, 0), (1, 1)], 'Ring')"],
  ['SimpleAggregateFunction', "CAST(5, 'SimpleAggregateFunction(sum, UInt64)')"],
  ['Nested', "CAST([(1, 'a')], 'Nested(id UInt8, s String)')"],
]

for (const [label, expr, settings] of CASES) {
  test(`live re-encode: ${label}`, { skip }, async () => {
    const bytes = await nativeBytes(`SELECT ${expr} AS v FROM numbers(3)`, settings)
    const decoded = decodeNativeColumns(bytes)
    assert.equal(decoded.rowCount, 3)
    assert.deepEqual(concat(decoded.chunks.map(encodeNative)), bytes)
  })
}

test('live stream: 1M rows across many blocks and pushes', { skip }, async () => {
  const res = await query('SELECT number AS n, toString(number) AS s FROM numbers(1000000)')
  const d = new NativeStreamDecoder()
  let sum = 0n
  let blocks = 0
  const consume = (out) => {
    blocks += out.chunks.length
    for (const c of out.chunks) for (const v of c.columns[0].values) sum += v
  }
  for await (const piece of res.body) consume(d.push(piece))
  consume(d.finish())
  assert.equal(d.rowCount, 1_000_000)
  assert.equal(sum, (999_999n * 1_000_000n) / 2n)
  assert.ok(blocks > 1)
  d.free()
})

test('live stream: a mid-stream server exception fails the decode', { skip }, async () => {
  const res = await query("SELECT throwIf(number = 300000, 'boom') AS x FROM numbers(1000000)", {
    max_block_size: '10000',
    http_write_exception_in_output_format: '0',
  })
  const d = new NativeStreamDecoder()
  let before = 0
  await assert.rejects(async () => {
    for await (const piece of res.body) before += d.push(piece).rowCount
    d.finish()
  })
  assert.ok(before > 0)
  d.free()
})
