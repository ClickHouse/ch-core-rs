#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture_dir="${repo_root}/tests/fixtures"

host="${CLICKHOUSE_CONNECT_TEST_HOST:-localhost}"
port="${CLICKHOUSE_CONNECT_TEST_PORT:-8123}"
user="${CLICKHOUSE_CONNECT_TEST_USER:-default}"
password="${CLICKHOUSE_CONNECT_TEST_PASSWORD:-}"
scheme="${CLICKHOUSE_CONNECT_TEST_SCHEME:-http}"
url="${CLICKHOUSE_CONNECT_TEST_URL:-${scheme}://${host}:${port}/}"

mkdir -p "${fixture_dir}"

curl_common=(
  curl
  -sS
  --fail
  --get
  "${url}"
  --user
  "${user}:${password}"
)

server_version="$("${curl_common[@]}" \
  --data-urlencode "query=SELECT version() FORMAT TabSeparatedRaw")"

echo "Capturing fixtures from ClickHouse ${server_version} at ${url}"

all_types_query=$(
  cat <<'SQL'
WITH number AS n
SELECT
    CAST(multiIf(n = 0, '-128', n = 1, '-1', n = 2, '0', '127'), 'Int8') AS i8,
    CAST(multiIf(n = 0, '-32768', n = 1, '-13', n = 2, '0', '32767'), 'Int16') AS i16,
    CAST(multiIf(n = 0, '-2147483648', n = 1, '-79', n = 2, '0', '2147483647'), 'Int32') AS i32,
    CAST(multiIf(n = 0, '-9223372036854775808', n = 1, '-79', n = 2, '0', '9223372036854775807'), 'Int64') AS i64,
    CAST(multiIf(n = 0, '0', n = 1, '13', n = 2, '79', '255'), 'UInt8') AS u8,
    CAST(multiIf(n = 0, '0', n = 1, '13', n = 2, '79', '65535'), 'UInt16') AS u16,
    CAST(multiIf(n = 0, '0', n = 1, '13', n = 2, '79', '4294967295'), 'UInt32') AS u32,
    CAST(multiIf(n = 0, '0', n = 1, '13', n = 2, '79', '18446744073709551615'), 'UInt64') AS u64,
    CAST(multiIf(n = 0, '-1.25', n = 1, '0', n = 2, '3.5', '79.125'), 'Float32') AS f32,
    CAST(multiIf(n = 0, '-1.25', n = 1, '0', n = 2, '3.5', '79.125'), 'Float64') AS f64,
    CAST(n % 2, 'Bool') AS b,
    multiIf(n = 0, '', n = 1, 'user_1', n = 2, unhex('FF00'), 'user_2') AS s,
    CAST(multiIf(n = 0, 'x', n = 1, 'ABCD', n = 2, '', unhex('FF')), 'FixedString(4)') AS fs,
    CAST(multiIf(n = 1, NULL, n = 3, NULL, CAST(toInt32(n) - 7, 'Nullable(Int32)')), 'Nullable(Int32)') AS ni32,
    CAST(multiIf(n = 1, NULL, n = 3, NULL, CAST(concat('user_', toString(n)), 'Nullable(String)')), 'Nullable(String)') AS ns,
    -- Temporal columns. Values are chosen inside each type's representable
    -- range; Date32 and DateTime64 each include a pre-epoch value. Timezone and
    -- precision are type metadata only, with no effect on the wire bytes, so the
    -- bare and tz-carrying variants share the same raw integers.
    -- Date: UInt16 days since 1970-01-01 (0 .. 65535 = 1970-01-01 .. 2149-06-06).
    CAST(multiIf(n = 0, toUInt16(0), n = 1, toUInt16(19737), n = 2, toUInt16(49710), toUInt16(65535)), 'Date') AS d,
    -- Date32: Int32 days since 1970-01-01, signed, wider range.
    CAST(multiIf(n = 0, toInt32(-7227), n = 1, toInt32(0), n = 2, toInt32(19737), toInt32(84370)), 'Date32') AS d32,
    -- DateTime (bare): UInt32 seconds since epoch. CAST of a number is the raw
    -- seconds, independent of session timezone.
    CAST(multiIf(n = 0, toUInt32(0), n = 1, toUInt32(1705322096), n = 2, toUInt32(961056000), toUInt32(4294967295)), 'DateTime') AS dt,
    -- DateTime('UTC'): same UInt32 seconds, timezone is metadata only.
    CAST(multiIf(n = 0, toUInt32(0), n = 1, toUInt32(1705322096), n = 2, toUInt32(961056000), toUInt32(4294967295)), 'DateTime(\'UTC\')') AS dt_utc,
    -- DateTime64(3): Int64 ticks at 10^-3 s. fromUnixTimestamp64Milli sets the
    -- raw ticks directly, so the committed bytes match these integers exactly.
    multiIf(n = 0, fromUnixTimestamp64Milli(toInt64(-877)), n = 1, fromUnixTimestamp64Milli(toInt64(0)), n = 2, fromUnixTimestamp64Milli(toInt64(1705322096789)), fromUnixTimestamp64Milli(toInt64(4102444799999))) AS dt64,
    -- DateTime64(3, 'UTC'): same Int64 ticks, timezone is metadata only.
    fromUnixTimestamp64Milli(multiIf(n = 0, toInt64(-877), n = 1, toInt64(0), n = 2, toInt64(1705322096789), toInt64(4102444799999)), 'UTC') AS dt64_utc,
    -- LowCardinality(String): dictionary-encoded strings with repeats so the
    -- per-block dictionary has fewer entries than rows. Values picked so the
    -- four rows reference three distinct dictionary entries.
    CAST(multiIf(n = 0, 'user_1', n = 1, 'user_2', n = 2, 'user_1', 'user_3'), 'LowCardinality(String)') AS lc,
    -- LowCardinality(Nullable(String)): the dictionary's index 0 is the NULL
    -- sentinel; rows 1 and 3 are NULL, rows 0 and 2 are real values.
    CAST(multiIf(n = 1, NULL, n = 3, NULL, CAST(concat('user_', toString(n)), 'Nullable(String)')), 'LowCardinality(Nullable(String))') AS lcn,
    -- LowCardinality over non-String inners. ClickHouse only allows these with
    -- allow_suspicious_low_cardinality_types=1 (set in SETTINGS below), a
    -- server-side creation guard; the wire bytes are unaffected. The dictionary
    -- values are the inner type serialized as a plain column body (raw LE
    -- primitives), not varint strings.
    -- LowCardinality(UInt32): repeats so the per-block dictionary is smaller than
    -- the row count. Values: 13, 79, 13, 4294967295.
    CAST(multiIf(n = 0, 13, n = 1, 79, n = 2, 13, 4294967295), 'LowCardinality(UInt32)') AS lc_u32,
    -- LowCardinality(Date): UInt16 days, with a repeat. Days: 19737, 49710,
    -- 19737, 0.
    CAST(multiIf(n = 0, 19737, n = 1, 49710, n = 2, 19737, 0), 'LowCardinality(Date)') AS lc_date,
    -- LowCardinality(Nullable(UInt32)): rows 1 and 3 NULL (wire index 0), rows 0
    -- and 2 real values 13 and 79.
    CAST(multiIf(n = 1, NULL, n = 3, NULL, n = 0, 13, 79), 'LowCardinality(Nullable(UInt32))') AS lcn_u32,
    -- UUID: 16 raw bytes, a POD dump of the UInt128 (NOT RFC-4122 byte order).
    -- Row 1 uses the documented 00112233-4455-6677-8899-aabbccddeeff so the
    -- committed bytes can be asserted exactly: on the wire that is
    -- 77 66 55 44 33 22 11 00 ff ee dd cc bb aa 99 88. Row 0 is the nil UUID.
    CAST(multiIf(n = 0, '00000000-0000-0000-0000-000000000000', n = 1, '00112233-4455-6677-8899-aabbccddeeff', n = 2, '10203040-5060-7080-90a0-b0c0d0e0f000', 'ffffffff-ffff-ffff-ffff-ffffffffffff'), 'UUID') AS uuid,
    -- IPv4: a UInt32 on the wire (standard IPv4 numeric value). Decoded numbers:
    -- 0, 3221226219 (192.0.2.235), 169090600 (10.20.30.40), 4294967295.
    CAST(multiIf(n = 0, '0.0.0.0', n = 1, '192.0.2.235', n = 2, '10.20.30.40', '255.255.255.255'), 'IPv4') AS ipv4,
    -- IPv6: 16 raw bytes in network byte order, passed through verbatim.
    CAST(multiIf(n = 0, '::', n = 1, '2001:db8::68', n = 2, 'fe80::1', '::ffff:192.0.2.235'), 'IPv6') AS ipv6,
    -- LowCardinality(UUID): UUID is an unconditional LowCardinality inner (the
    -- allow_suspicious setting is not required for it, but is harmless). The
    -- dictionary body is raw 16-byte UUID rows. Values repeat so the per-block
    -- dictionary is smaller than the row count.
    CAST(multiIf(n = 0, '00112233-4455-6677-8899-aabbccddeeff', n = 1, '10203040-5060-7080-90a0-b0c0d0e0f000', n = 2, '00112233-4455-6677-8899-aabbccddeeff', 'ffffffff-ffff-ffff-ffff-ffffffffffff'), 'LowCardinality(UUID)') AS lc_uuid,
    -- Enum8: raw Int8 on the wire (1 byte/row); the name->value map is in the
    -- type string only. Values include a negative (-1) so the signed decode is
    -- exercised. Rows resolve to 1, 2, -1, 1.
    CAST(multiIf(n = 0, 'north', n = 1, 'south', n = 2, 'west', 'north'), 'Enum8(\'north\' = 1, \'south\' = 2, \'west\' = -1)') AS e8,
    -- Enum16: raw Int16 on the wire (2 bytes/row), same name->value-in-type-string
    -- shape, wider value range. Rows resolve to 1, 2, -1, 1.
    CAST(multiIf(n = 0, 'north', n = 1, 'south', n = 2, 'west', 'north'), 'Enum16(\'north\' = 1, \'south\' = 2, \'west\' = -1)') AS e16,
    -- Decimal(P, S): a raw little-endian two's-complement fixed-width integer per
    -- row, byte width derived from P (4/8/16/32 bytes). The server always emits
    -- the canonical Decimal(P, S) type string, so Decimal32(4) becomes
    -- Decimal(9, 4), Decimal64(9) becomes Decimal(18, 9), Decimal128(20) becomes
    -- Decimal(38, 20), and Decimal256(50) becomes Decimal(76, 50). Each column
    -- includes a negative value to exercise the signed two's-complement decode.
    -- The CAST string is the unscaled value; the stored integer is value*10^S.
    -- Decimal32(4) -> Decimal(9, 4): unscaled bytes are value*10^4. Rows:
    -- 0.0013 -> 13, -0.0001 -> -1, 0 -> 0, 1.2345 -> 12345.
    CAST(multiIf(n = 0, '0.0013', n = 1, '-0.0001', n = 2, '0', '1.2345'), 'Decimal32(4)') AS dec32,
    -- Decimal64(9) -> Decimal(18, 9): unscaled is value*10^9. Rows:
    -- 0.000000079 -> 79, -0.000000001 -> -1, 0 -> 0, 1.5 -> 1500000000.
    CAST(multiIf(n = 0, '0.000000079', n = 1, '-0.000000001', n = 2, '0', '1.5'), 'Decimal64(9)') AS dec64,
    -- Decimal128(20) -> Decimal(38, 20): unscaled is value*10^20. Rows:
    -- 1e-20 -> 1, -1e-20 -> -1, 0 -> 0, 7.9e-19 -> 79.
    CAST(multiIf(n = 0, '0.00000000000000000001', n = 1, '-0.00000000000000000001', n = 2, '0', '0.00000000000000000079'), 'Decimal128(20)') AS dec128,
    -- Decimal256(50) -> Decimal(76, 50): unscaled is value*10^50. Rows:
    -- 1e-50 -> 1, -1e-50 -> -1, 0 -> 0, 2.58e-48 -> 258.
    CAST(multiIf(n = 0, '0.00000000000000000000000000000000000000000000000001', n = 1, '-0.00000000000000000000000000000000000000000000000001', n = 2, '0', '0.00000000000000000000000000000000000000000000000258'), 'Decimal256(50)') AS dec256,
    -- LowCardinality(IPv4): IPv4 is a UInt32 on the wire, so the dictionary body
    -- is a plain UInt32 column body (raw 4-byte LE). Needs
    -- allow_suspicious_low_cardinality_types at creation (set in SETTINGS below);
    -- the wire bytes are unaffected. Values repeat so the per-block dictionary is
    -- smaller than the row count. Decoded numbers: 3221226219 (192.0.2.235),
    -- 169090600 (10.20.30.40), 3221226219, 4294967295.
    CAST(multiIf(n = 0, '192.0.2.235', n = 1, '10.20.30.40', n = 2, '192.0.2.235', '255.255.255.255'), 'LowCardinality(IPv4)') AS lc_ipv4,
    -- LowCardinality(IPv6): 16 raw bytes per dictionary entry in network byte
    -- order, the same body shape as LowCardinality(UUID). Values repeat. Rows
    -- resolve to 2001:db8::68, fe80::1, 2001:db8::68, ::ffff:192.0.2.235.
    CAST(multiIf(n = 0, '2001:db8::68', n = 1, 'fe80::1', n = 2, '2001:db8::68', '::ffff:192.0.2.235'), 'LowCardinality(IPv6)') AS lc_ipv6,
    -- Array(T): SerializationArray writes num_rows cumulative LE UInt64 end-offsets
    -- (no leading zero), then the flattened element body of length = the last
    -- offset. The decoder prepends Arrow's leading 0 and widens to i64. Rows cover
    -- an empty array (n=0), varying lengths, and a negative element:
    -- [] / [13] / [79, -13] / [1, 2, 3], so the decoded offsets are [0, 0, 1, 3, 6]
    -- and the flattened Int32 values are 13, 79, -13, 1, 2, 3.
    CAST(multiIf(n = 0, [], n = 1, [13], n = 2, [79, -13], [1, 2, 3]), 'Array(Int32)') AS arr,
    -- Array(String): variable-length element body after the offsets.
    -- [] / ['user_1'] / ['a', 'user_2'] / ['x'] -> offsets [0, 0, 1, 3, 4],
    -- flattened strings user_1, a, user_2, x.
    CAST(multiIf(n = 0, [], n = 1, ['user_1'], n = 2, ['a', 'user_2'], ['x']), 'Array(String)') AS arr_s,
    -- Array(Nullable(Int32)): after the offsets the element body is a per-element
    -- null map then the element values; element-level nulls live on the element
    -- column. [] / [13, NULL] / [NULL] / [79, -1, NULL] -> offsets [0, 0, 2, 3, 6],
    -- element validity [T, F, F, T, T, F].
    CAST(multiIf(n = 0, [], n = 1, [13, NULL], n = 2, [NULL], [79, -1, NULL]), 'Array(Nullable(Int32))') AS arr_n,
    -- Array(LowCardinality(String)): the element type's state prefix recurses, so
    -- the LC 8-byte key version is written BEFORE the array offsets, and the LC body
    -- (index word / dictionary / indexes) comes AFTER. [] / ['red', 'red'] /
    -- ['green'] / ['red', 'blue'] -> offsets [0, 0, 2, 3, 5], flattened elements
    -- red, red, green, red, blue resolved through the per-block dictionary.
    CAST(multiIf(n = 0, [], n = 1, ['red', 'red'], n = 2, ['green'], ['red', 'blue']), 'Array(LowCardinality(String))') AS arr_lc,
    -- Array(Array(Int32)): two offset levels then the leaf. [] / [[13]] /
    -- [[79, 13], []] / [[1], [2, 3]] -> outer offsets [0, 0, 1, 3, 5], inner offsets
    -- [0, 1, 3, 3, 4, 6], leaf Int32 values 13, 79, 13, 1, 2, 3.
    CAST(multiIf(n = 0, [], n = 1, [[13]], n = 2, [[79, 13], []], [[1], [2, 3]]), 'Array(Array(Int32))') AS arr_arr,
    -- Array(LowCardinality(String)) with EVERY row empty: the flattened element
    -- run has zero length, so the server writes only the hoisted LC 8-byte key
    -- version and the four all-zero offsets, and NOTHING for the LC element body
    -- (SerializationLowCardinality::serializeBinaryBulkWithMultipleStreams
    -- early-returns at limit == 0). Decodes to offsets [0, 0, 0, 0, 0] over an
    -- empty dictionary element column.
    CAST([], 'Array(LowCardinality(String))') AS arr_lc_empty,
    -- Tuple(T1, ...): SerializationTuple writes each element's FULL run one
    -- after another in declaration order (column-of-columns), no interleaving
    -- and no tuple-level framing. Rows: (-13, 'user_0'), (-6, 'user_1'),
    -- (1, 'user_2'), (8, 'user_3').
    CAST(tuple(toInt32(n) * 7 - 13, concat('user_', toString(n))), 'Tuple(Int32, String)') AS tup,
    -- Named tuple with a Nullable element: the names live only in the type
    -- string (Tuple(a Int32, b Nullable(String))); element b's body is its own
    -- null map then the strings. Rows: (0, 'user_0'), (1, NULL), (2, 'user_2'),
    -- (3, NULL).
    CAST(tuple(toInt32(n), multiIf(n = 1, NULL, n = 3, NULL, concat('user_', toString(n)))), 'Tuple(a Int32, b Nullable(String))') AS tup_named,
    -- Array(Tuple(Int32, Int32)): offsets first (the tuple elements write no
    -- state prefix), then the flattened tuple body (element 0's full run, then
    -- element 1's). [] / [(13, 79)] / [(1, 2), (3, 4)] / [(-1, -2)] -> offsets
    -- [0, 0, 1, 3, 4], element 0 values 13, 1, 3, -1, element 1 values
    -- 79, 2, 4, -2.
    CAST(multiIf(n = 0, [], n = 1, [(13, 79)], n = 2, [(1, 2), (3, 4)], [(-1, -2)]), 'Array(Tuple(Int32, Int32))') AS arr_tup,
    -- Nullable(Tuple(Int32, String)): legal on the wire
    -- (DataTypeTuple::canBeInsideNullable() is true; the DDL gate
    -- enable_nullable_tuple_type is set in SETTINGS below and has no wire
    -- effect). Ordinary Nullable framing: the per-row null map first, then the
    -- tuple body; null rows carry element defaults (0, ''). Rows:
    -- (0, 'user_0'), NULL, (2, 'user_2'), NULL.
    CAST(multiIf(n = 1, NULL, n = 3, NULL, tuple(toInt32(n), concat('user_', toString(n)))), 'Nullable(Tuple(Int32, String))') AS ntup,
    -- Map(K, V): on the Native wire always the plain Array(Tuple(keys, values))
    -- layout, cumulative UInt64 end-offsets then the flattened key run then the
    -- flattened value run. Rows: {} / {a: 13} / {a: 1, b: 2} / {k: -7} ->
    -- offsets [0, 0, 1, 3, 4], keys a, a, b, k, values 13, 1, 2, -7.
    CAST(multiIf(n = 0, map(), n = 1, map('a', 13), n = 2, map('a', 1, 'b', 2), map('k', -7)), 'Map(String, Int32)') AS m,
    -- Map(LowCardinality(String), UInt8): a plain LC key is legal
    -- (DataTypeMap::isValidKeyType forbids only Nullable and
    -- LowCardinality(Nullable)); the LC 8-byte key version is hoisted to the
    -- very front of the column, before the offsets. Rows: {red: 1} / {} /
    -- {red: 2, blue: 3} / {green: 4} -> offsets [0, 1, 1, 3, 4].
    CAST(multiIf(n = 0, map('red', 1), n = 1, map(), n = 2, map('red', 2, 'blue', 3), map('green', 4)), 'Map(LowCardinality(String), UInt8)') AS m_lc,
    -- Map(String, Nullable(String)): the flattened value run carries its own
    -- per-entry null map. Rows: {a: user_1} / {b: NULL} / {} /
    -- {c: user_2, d: NULL} -> offsets [0, 1, 2, 2, 4], value validity T,F,T,F.
    CAST(multiIf(n = 0, map('a', 'user_1'), n = 1, map('b', NULL), n = 2, map(), map('c', 'user_2', 'd', NULL)), 'Map(String, Nullable(String))') AS m_nv,
    -- Map(String, Array(Int32)): the flattened value run is itself an Array
    -- column over the entries. Rows: {a: [13]} / {} / {b: [], c: [1, 2]} /
    -- {d: [79]} -> map offsets [0, 1, 1, 3, 4], value-array offsets
    -- [0, 1, 1, 3, 4], leaf 13, 1, 2, 79.
    CAST(multiIf(n = 0, map('a', [13]), n = 1, map(), n = 2, map('b', [], 'c', [1, 2]), map('d', [79])), 'Map(String, Array(Int32))') AS m_arr,
    -- Array(Map(String, Int32)): maps compose inside Array. Rows: [] /
    -- [{a: 1}] / [{b: 2}, {}] / [{c: 3}] -> outer offsets [0, 0, 1, 3, 4],
    -- inner map offsets [0, 1, 2, 2, 3], keys a, b, c, values 1, 2, 3.
    CAST(multiIf(n = 0, [], n = 1, [map('a', 1)], n = 2, [map('b', 2), map()], [map('c', 3)]), 'Array(Map(String, Int32))') AS arr_m,
    -- Map(String, Int32) with EVERY row empty: the offsets are still written
    -- (all zeros), and the key/value runs are entirely absent (limit == 0
    -- passes to the nested tuple).
    CAST(map(), 'Map(String, Int32)') AS m_empty,
    -- Wide integers: Int128/UInt128/Int256/UInt256 are raw contiguous
    -- little-endian fixed-width integers (16 bytes for the 128-bit pair, 32 for
    -- the 256-bit pair), the same SerializationNumber template as Int8..Int64,
    -- byte-identical to a Decimal128/256 integer body. Signed rows include -1
    -- (all 0xFF two's-complement) and the type MAX; unsigned rows include a
    -- high-bit-set value (2^(width-1)) that must stay positive, and the type MAX.
    -- i128 rows: -1, 0, 79, 2^127-1.
    CAST(multiIf(n = 0, '-1', n = 1, '0', n = 2, '79', '170141183460469231731687303715884105727'), 'Int128') AS i128,
    -- u128 rows: 0, 13, 2^127, 2^128-1.
    CAST(multiIf(n = 0, '0', n = 1, '13', n = 2, '170141183460469231731687303715884105728', '340282366920938463463374607431768211455'), 'UInt128') AS u128,
    -- i256 rows: -1, 0, 79, 2^255-1.
    CAST(multiIf(n = 0, '-1', n = 1, '0', n = 2, '79', '57896044618658097711785492504343953926634992332820282019728792003956564819967'), 'Int256') AS i256,
    -- u256 rows: 0, 13, 2^255, 2^256-1.
    CAST(multiIf(n = 0, '0', n = 1, '13', n = 2, '57896044618658097711785492504343953926634992332820282019728792003956564819968', '115792089237316195423570985008687907853269984665640564039457584007913129639935'), 'UInt256') AS u256,
    -- Nullable(Int128): rows 1 and 3 NULL, rows 0 and 2 real (13, -1). The
    -- Nullable null map precedes the 16-byte body; null rows carry the server's
    -- placeholder (0).
    CAST(multiIf(n = 1, NULL, n = 3, NULL, n = 0, '13', '-1'), 'Nullable(Int128)') AS ni128,
    -- LowCardinality(Int256): a wide-int LC inner (canBeInsideLowCardinality is
    -- true), gated at creation by allow_suspicious_low_cardinality_types (set in
    -- SETTINGS below; no wire effect). Values repeat (13, 79, 13, 258) so the
    -- per-block dictionary is smaller than the row count. The dictionary body is
    -- the plain 32-byte-per-entry Int256 run.
    CAST(multiIf(n = 0, '13', n = 1, '79', n = 2, '13', '258'), 'LowCardinality(Int256)') AS lc_i256,
    -- Time: raw signed Int32 seconds, including both documented text extrema.
    -- Unlike DateTime, this has no epoch or timezone.
    CAST(multiIf(n = 0, toInt32(-3599999), n = 1, toInt32(-3600), n = 2, toInt32(13), toInt32(3599999)), 'Time') AS t,
    -- Time64(6): raw signed Int64 microsecond ticks. The string forms pin exact
    -- fractional values without a numeric cast rescaling seconds by 10^6.
    CAST(multiIf(n = 0, '-999:59:59.999999', n = 1, '-00:00:00.000001', n = 2, '00:00:13.000079', '999:59:59.999999'), 'Time64(6)') AS t64,
    -- Both Time types are legal Nullable inners. Null rows carry the server's
    -- placeholder after the null map; only the valid rows are asserted.
    CAST(multiIf(n = 1, NULL, n = 3, NULL, n = 0, toInt32(-13), toInt32(79)), 'Nullable(Time)') AS nt,
    CAST(multiIf(n = 1, NULL, n = 3, NULL, n = 0, '-00:00:00.000013', '00:00:00.000079'), 'Nullable(Time64(6))') AS nt64,
    -- Time is number-backed and legal inside LowCardinality (Time64 is not).
    -- Repeats keep the per-block dictionary smaller than the row count.
    CAST(multiIf(n = 0, toInt32(-13), n = 1, toInt32(79), n = 2, toInt32(-13), toInt32(258)), 'LowCardinality(Time)') AS lc_time,
    -- SimpleAggregateFunction(func, T): pure name decoration over the inner type
    -- T (DataTypeCustomSimpleAggregateFunction). The runtime object IS the inner
    -- T instance with a custom name and a null serialization slot, so the wire
    -- bytes, state prefix, and Arrow shape are byte-identical to T; the alias
    -- spelling (never the expanded T) reaches the Native header. Decode delegates
    -- to T, so no new Column variant appears. CAST(x AS SimpleAggregateFunction)
    -- is expressible directly in a SELECT and the header keeps the alias, so no
    -- temporary table is needed.
    -- Over a scalar Float64: decodes as a plain Float64 buffer. Values -1.25, 0,
    -- 13, 79.125 are all exactly representable.
    CAST(multiIf(n = 0, -1.25, n = 1, 0., n = 2, 13., 79.125), 'SimpleAggregateFunction(sum, Float64)') AS saf_sum,
    -- Over LowCardinality(Nullable(String)): decodes as a Dictionary, exactly the
    -- inner LC. Rows resolve to user_1, NULL, user_2, NULL.
    CAST(multiIf(n = 1, NULL, n = 3, NULL, n = 0, 'user_1', 'user_2'), 'SimpleAggregateFunction(anyLast, LowCardinality(Nullable(String)))') AS saf_lc,
    -- Parametrized function name (groupArrayLastArray(5)) over Array(UInt64):
    -- exercises the parenthesized-function-name parse path. The (5) is metadata
    -- only; the stored value is a plain Array(UInt64). Rows [] / [13] / [79, 13] /
    -- [1, 2, 3] -> offsets [0, 0, 1, 3, 6], flattened 13, 79, 13, 1, 2, 3.
    CAST(multiIf(n = 0, [], n = 1, [13], n = 2, [79, 13], [1, 2, 3]), 'SimpleAggregateFunction(groupArrayLastArray(5), Array(UInt64))') AS saf_grp,
    -- Geo aliases (DataTypeCustomGeo): a custom name over a fixed Tuple/Array-of-
    -- Float64 nesting whose serialization slot is null, so wire bytes and Arrow
    -- shape are byte-identical to that nesting and the bare alias spelling reaches
    -- the header. CAST(... AS Point/Ring/MultiPolygon/MultiPoint) is expressible
    -- in a SELECT.
    -- Point = Tuple(Float64, Float64) (unnamed): decodes as a two-field Float64
    -- Tuple. field0 (x) 13, -1.5, 0, 79.125; field1 (y) 79, 2.5, 0, -13.25.
    CAST(multiIf(n = 0, (13., 79.), n = 1, (-1.5, 2.5), n = 2, (0., 0.), (79.125, -13.25)), 'Point') AS point,
    -- Nullable(Point): Point is a Tuple, and DataTypeTuple::canBeInsideNullable()
    -- is true, so Nullable(Point) is legal (enable_nullable_tuple_type gates only
    -- CREATE, no wire effect). Decodes as a Tuple with a tuple-level null map;
    -- null rows carry element placeholders. Valid rows 0 and 2 are (13, 79) and
    -- (1.25, -2.5).
    CAST(multiIf(n = 1, NULL, n = 3, NULL, n = 0, (13., 79.), (1.25, -2.5)), 'Nullable(Point)') AS npoint,
    -- Ring = Array(Point) = Array(Tuple(Float64, Float64)): decodes as an Array of
    -- a two-field Float64 Tuple. Rows [] / [(13, 79)] / [(1, 2), (3, 4)] /
    -- [(-1.5, -2.5)] -> offsets [0, 0, 1, 3, 4], leaf x 13, 1, 3, -1.5, leaf y
    -- 79, 2, 4, -2.5.
    CAST(multiIf(n = 0, [], n = 1, [(13., 79.)], n = 2, [(1., 2.), (3., 4.)], [(-1.5, -2.5)]), 'Ring') AS ring,
    -- MultiPolygon = Array(Array(Array(Point))): three Array levels over the leaf
    -- Point tuple, the deepest geo nesting. Rows [] / [[[(13, 79)]]] /
    -- [[[(1, 2), (3, 4)], [(5, 6)]]] / [[[(-1, -2)]], [[(7, 8)]]]. Outer offsets
    -- [0, 0, 1, 2, 4], mid [0, 1, 3, 4, 5], inner [0, 1, 3, 4, 5, 6], leaf x
    -- 13, 1, 3, 5, -1, 7, leaf y 79, 2, 4, 6, -2, 8.
    CAST(multiIf(n = 0, [], n = 1, [[[(13., 79.)]]], n = 2, [[[(1., 2.), (3., 4.)], [(5., 6.)]]], [[[(-1., -2.)]], [[(7., 8.)]]]), 'MultiPolygon') AS mpoly,
    -- Nested(x UInt32, y String): with the CAST form the alias reaches the header
    -- regardless of flatten_nested (that setting flattens table DDL, not a SELECT
    -- projection). The body is byte-identical to Array(Tuple(named x, y)), so it
    -- decodes as an Array of a named two-field Tuple. Rows [] / [(13, user_1)] /
    -- [(79, a), (1, user_2)] / [(2, x)] -> offsets [0, 0, 1, 3, 4], field x
    -- 13, 79, 1, 2, field y user_1, a, user_2, x.
    CAST(multiIf(n = 0, [], n = 1, [(13, 'user_1')], n = 2, [(79, 'a'), (1, 'user_2')], [(2, 'x')]), 'Nested(x UInt32, y String)') AS nst,
    -- SimpleAggregateFunction inside wrappers. The server emits the alias
    -- spelling VERBATIM inside the wrapper in the Native header (confirmed live at
    -- v26.6.1.1193-stable via toTypeName + hexdump), and wrapper legality
    -- delegates to the physical inner. These prove the decoder accepts the
    -- server's real headers for the newly legal wrapper forms.
    -- Nullable(SimpleAggregateFunction(sum, UInt64)): decodes exactly as
    -- Nullable(UInt64), the null map then the UInt64 run. Rows 1 and 3 NULL,
    -- rows 0 and 2 real (13, 79).
    CAST(multiIf(n = 1, NULL, n = 3, NULL, n = 0, toUInt64(13), toUInt64(79)), 'Nullable(SimpleAggregateFunction(sum, UInt64))') AS nsaf,
    -- LowCardinality(SimpleAggregateFunction(anyLast, String)): the LC body
    -- delegates to the physical String inner. Values repeat so the per-block
    -- dictionary is smaller than the row count. Rows user_1, user_2, user_1,
    -- user_3 -> three distinct dictionary entries.
    CAST(multiIf(n = 0, 'user_1', n = 1, 'user_2', n = 2, 'user_1', 'user_3'), 'LowCardinality(SimpleAggregateFunction(anyLast, String))') AS lc_saf,
    -- LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String))): the SAF
    -- name decoration sits BETWEEN the LowCardinality and its removeNullable
    -- Nullable, so nullability and the dictionary value type must be resolved
    -- through the full SAF chain, not a single-level see-through. Confirmed a real
    -- server header live at v26.6.1.1193-stable. Rows resolve to user_1, NULL,
    -- user_2, NULL; index 0 is the NULL sentinel and the dictionary body is the
    -- bare non-nullable String inner.
    CAST(multiIf(n = 1, NULL, n = 3, NULL, n = 0, 'user_1', 'user_2'), 'LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))') AS lc_nsaf,
    -- The 11 Interval* types are distinct logical units over the same raw signed
    -- Int64 body. Small signed counts keep the SQL conversion safely in range
    -- while pinning negative, zero, and positive wire values for every kind.
    CAST(multiIf(n = 0, toInt64(-13), n = 1, toInt64(0), n = 2, toInt64(79), toInt64(258)), 'IntervalYear') AS iy,
    CAST(multiIf(n = 0, toInt64(-13), n = 1, toInt64(0), n = 2, toInt64(79), toInt64(258)), 'IntervalQuarter') AS iq,
    CAST(multiIf(n = 0, toInt64(-13), n = 1, toInt64(0), n = 2, toInt64(79), toInt64(258)), 'IntervalMonth') AS imo,
    CAST(multiIf(n = 0, toInt64(-13), n = 1, toInt64(0), n = 2, toInt64(79), toInt64(258)), 'IntervalWeek') AS iw,
    CAST(multiIf(n = 0, toInt64(-13), n = 1, toInt64(0), n = 2, toInt64(79), toInt64(258)), 'IntervalDay') AS id,
    CAST(multiIf(n = 0, toInt64(-13), n = 1, toInt64(0), n = 2, toInt64(79), toInt64(258)), 'IntervalHour') AS ih,
    CAST(multiIf(n = 0, toInt64(-13), n = 1, toInt64(0), n = 2, toInt64(79), toInt64(258)), 'IntervalMinute') AS imi,
    CAST(multiIf(n = 0, toInt64(-13), n = 1, toInt64(0), n = 2, toInt64(79), toInt64(258)), 'IntervalSecond') AS isecond,
    CAST(multiIf(n = 0, toInt64(-13), n = 1, toInt64(0), n = 2, toInt64(79), toInt64(258)), 'IntervalMillisecond') AS ims,
    CAST(multiIf(n = 0, toInt64(-13), n = 1, toInt64(0), n = 2, toInt64(79), toInt64(258)), 'IntervalMicrosecond') AS ius,
    CAST(multiIf(n = 0, toInt64(-13), n = 1, toInt64(0), n = 2, toInt64(79), toInt64(258)), 'IntervalNanosecond') AS ins,
    -- Wrapper representatives: both Nullable and LowCardinality are legal for
    -- every Interval kind. The LC values repeat so a real dictionary body and
    -- indexes are captured.
    CAST(multiIf(n = 1, NULL, n = 3, NULL, n = 0, toInt64(13), toInt64(79)), 'Nullable(IntervalDay)') AS nid,
    CAST(multiIf(n = 0, toInt64(13), n = 1, toInt64(79), n = 2, toInt64(13), toInt64(258)), 'LowCardinality(IntervalHour)') AS lc_ih,
    -- Map(IntervalDay, String): bare Interval keys are legal. This captures the
    -- ordinary Array(Tuple(keys, values)) Map body with an Interval-backed key
    -- run, including an empty row and multiple entries in one row.
    CAST(multiIf(n = 0, map(), n = 1, map(toIntervalDay(13), 'user_1'), n = 2, map(toIntervalDay(-79), 'a', toIntervalDay(13), 'user_2'), map(toIntervalDay(258), 'x')), 'Map(IntervalDay, String)') AS m_id,
    -- BFloat16: raw 16-bit words containing the top half of IEEE-754 Float32.
    -- These finite values are exactly representable, so the committed words
    -- are deterministic: BFA0, 0000, 4060, 429E.
    CAST(multiIf(n = 0, '-1.25', n = 1, '0', n = 2, '3.5', '79'), 'BFloat16') AS bf,
    -- Nullable uses the ordinary null-map then the complete 2-byte nested run.
    CAST(multiIf(n = 1, NULL, n = 3, NULL, n = 0, toFloat32(13), toFloat32(79)), 'Nullable(BFloat16)') AS nbf,
    -- BFloat16 is a legal numeric LowCardinality inner. Repeated 13 proves a
    -- real width-2 dictionary body plus indexes; the suspicious-LC setting
    -- below is a construction-time gate only.
    CAST(multiIf(n = 0, toFloat32(13), n = 1, toFloat32(79), n = 2, toFloat32(13), toFloat32(258)), 'LowCardinality(BFloat16)') AS lc_bf,
    -- A NULL literal has the canonical query-result type Nullable(Nothing).
    -- Native writes the four-byte null map first, then one ASCII '0'
    -- placeholder byte per row for the nested Nothing body.
    NULL AS nothing,
    -- Bare Nothing cannot hold a non-null value, but it is the inferred element
    -- type of an empty array. All four rows are empty, so the flattened Nothing
    -- run has length zero while the literal Array(Nothing) header reaches Native.
    CAST([], 'Array(Nothing)') AS arr_nothing,
    -- AggregateFunction(count, UInt64): each row is a concrete count state,
    -- serialized as one VarUInt64 with no outer row/column length. arrayReduce
    -- produces counts 0, 1, 2, 3 from arrays of those lengths, so the committed
    -- fixture confirms both the canonical header and the function-specific body.
    arrayReduce('countState', range(n)) AS agg_count,
    -- AggregateFunction(nothingUInt64, Nullable(Nothing)): the canonical name the
    -- server assigns when count collapses over an only-null argument
    -- (count(Nullable(Nothing)) -> AggregateFunctionNothingUInt64). Its state is a
    -- single 0x00 byte per row (AggregateFunctionNothingImpl::serialize), and
    -- finalizeAggregation resolves it to a count of 0. CAST to the canonical type
    -- string is the authentic construction: countState(toNullable(NULL)) constant-
    -- folds away, but CAST(unhex('00'), '...') emits the canonical header with the
    -- one-zero-byte state (live-confirmed at v26.6.1.1193-stable).
    CAST(unhex('00'), 'AggregateFunction(nothingUInt64, Nullable(Nothing))') AS agg_nothing,
    -- Exact base sum has one fixed-width accumulator per row and no generic
    -- state framing. These representatives cover every accumulator width and
    -- the special numeric/Enum promotion families. range(n) yields sums
    -- 0, 0, 1, and 3 across the four rows.
    arrayReduce('sumState', arrayMap(x -> toUInt8(x), range(n))) AS agg_sum_u8,
    -- BFloat16 accumulates and serializes as one Float64 (8-byte) state.
    arrayReduce('sumState', arrayMap(x -> toBFloat16(x), range(n))) AS agg_sum_bf,
    -- Decimal32/64/128 sum into a 16-byte Decimal128 scaled integer. Scale 2
    -- makes the four unscaled states 0, 0, 100, and 300.
    arrayReduce('sumState', arrayMap(x -> toDecimal32(x, 2), range(n))) AS agg_sum_d32,
    -- UInt256 keeps its native 32-byte accumulator width.
    arrayReduce('sumState', arrayMap(x -> toUInt256(x), range(n))) AS agg_sum_u256,
    -- Nullable sum adds one presence byte per state and writes the nested
    -- accumulator only after a true flag. These rows yield absent, absent,
    -- present(13), and present(92), so the following Enum column grounds the
    -- variable state boundary against real server bytes.
    arrayReduce('sumState', multiIf(n = 0, CAST([], 'Array(Nullable(UInt8))'), n = 1, CAST([NULL], 'Array(Nullable(UInt8))'), n = 2, CAST([13, NULL], 'Array(Nullable(UInt8))'), CAST([NULL, 79, 13], 'Array(Nullable(UInt8))'))) AS agg_sum_nu8,
    -- Enum8 and Enum16 both sum into signed Int64 states; one Enum8 fixture is
    -- enough to ground the explicit Enum dispatch against real server bytes.
    arrayReduce('sumState', arrayMap(x -> CAST(x, 'Enum8(\'zero\' = 0, \'one\' = 1, \'two\' = 2)'), range(n))) AS agg_sum_e8,
    -- AggregateFunction(nothingNull, Nullable(Nothing)): the canonical function
    -- name when sum collapses over an only-null argument. Direct sumState(NULL)
    -- collapses to a finalized Nullable(Nothing), so CAST from the exact one-zero-
    -- byte state is the reliable construction for a Native fixture.
    CAST(unhex('00'), 'AggregateFunction(nothingNull, Nullable(Nothing))') AS agg_nothing_null,
    -- Variant BASIC serialization: one UInt64 mode prefix, then the complete
    -- UInt8 discriminator run, then one dense body per canonical alternative.
    -- Discriminator 255 is intrinsic NULL and consumes no alternative value.
    -- Canonical alternative order is String = 0, UInt64 = 1. Rows are
    -- NULL, user_1, 13, user_2.
    multiIf(
        n = 0, CAST(CAST(NULL, 'Nullable(Nothing)'), 'Variant(String, UInt64)'),
        n = 1, CAST(CAST('user_1', 'String'), 'Variant(String, UInt64)'),
        n = 2, CAST(toUInt64(13), 'Variant(String, UInt64)'),
        CAST(CAST('user_2', 'String'), 'Variant(String, UInt64)')
    ) AS variant,
    -- Dynamic(max_types=1): block-local runtime types use String as the one
    -- direct dense child because it occurs twice. UInt64 and Array(Int32)
    -- overflow into SharedVariant as binary descriptor+single-value blobs.
    -- Rows are user_1, user_2, 13, [79, -13].
    multiIf(
        n < 2, CAST(concat('user_', toString(n + 1)), 'Dynamic(max_types=1)'),
        n = 2, CAST(toUInt64(13), 'Dynamic(max_types=1)'),
        CAST([toInt32(79), toInt32(-13)], 'Dynamic(max_types=1)')
    ) AS dynamic,
    -- JSON (DataTypeObject, GA at this server version; needs no special setting).
    -- Three shapes exercise the whole structured wire form. This first one has a
    -- typed path, exactly ONE dynamic path, and shared-data spill.
    -- JSON(max_dynamic_paths=1, `a.b` Int64): the typed path `a.b` never counts
    -- against max_dynamic_paths, so it is always a typed child (rows 13, 79, 0, 0;
    -- absent objects read back as the type default 0). The first distinct
    -- non-typed path encountered, `x`, becomes THE one direct dynamic path (rows 0
    -- and 2 -> "user_1"/"user_2", rows 1 and 3 -> the path's intrinsic NULL). Every
    -- later distinct path overflows into shared data: `y` on row 1 and `z` on row
    -- 2. Shared values are opaque binary blobs (a binary type descriptor then one
    -- serializeBinary payload): `y` is [0x0a] (Int64 tag) + the i64 7, and `z` is
    -- [0x1e 0x23 0x0a] (Array(Nullable(Int64))) + the array [1, 2]. Confirmed live
    -- against v26.6.1.1193-stable via JSONDynamicPaths / JSONSharedDataPaths.
    CAST(multiIf(n = 0, '{"a":{"b":13}, "x":"user_1"}', n = 1, '{"a":{"b":79}, "y":7}', n = 2, '{"x":"user_2", "z":[1,2]}', '{}'), 'JSON(max_dynamic_paths=1, `a.b` Int64)') AS j_typed,
    -- Bare JSON (default max_dynamic_paths=1024): two dynamic paths and NO shared
    -- spill. `p` is a String path (rows 0, 1) and `q` an Int64 path (rows 0, 3);
    -- row 2 is an empty object, so every path reads NULL there. The two dynamic
    -- paths decode sorted by name, each a block-local Dynamic whose child order is
    -- the canonical global-discriminator (sorted type-name) order, so `p`'s
    -- SharedVariant child sorts before String and `q`'s Int64 child sorts before
    -- SharedVariant.
    CAST(multiIf(n = 0, '{"p":"user_1", "q":13}', n = 1, '{"p":"user_2"}', n = 2, '{}', '{"q":79}'), 'JSON') AS j_bare,
    -- Nullable(JSON): the top-level null map precedes the full JSON body. Rows 1
    -- and 3 are NULL; rows 0 and 2 are `{"m": 13}` / `{"m": 79}` with the single
    -- Int64 dynamic path `m`.
    CAST(multiIf(n = 1, NULL, n = 3, NULL, n = 0, '{"m":13}', '{"m":79}'), 'Nullable(JSON)') AS j_null,
    -- Geometry is a custom fixed name over the canonical Variant alternatives
    -- LineString(0), MultiLineString(1), MultiPolygon(2), Point(3), Polygon(4),
    -- Ring(5), MultiPoint(6), with 255 for intrinsic NULL. The plain column covers
    -- three different shapes plus NULL without changing this fixture's four-row
    -- size.
    multiIf(
        n = 0, [(13., 79.)]::LineString::Geometry,
        n = 1, [[[(21., 31.)]]]::MultiPolygon::Geometry,
        n = 2, (51., 61.)::Point::Geometry,
        CAST(NULL, 'Geometry')
    ) AS geometry,
    -- One Array(Geometry) per row covers every Geometry alternative and NULL
    -- against real server bytes while keeping the all_types query row-aligned.
    -- Each row's flattened discriminator sequence is 0,1,2,3,4,5,6,255.
    [
        [(1., 2.)]::LineString::Geometry,
        [[(3., 4.)]]::MultiLineString::Geometry,
        [[[(5., 6.)]]]::MultiPolygon::Geometry,
        (7., 8.)::Point::Geometry,
        [[(9., 10.)]]::Polygon::Geometry,
        [(11., 12.)]::Ring::Geometry,
        [(13., 14.)]::MultiPoint::Geometry,
        CAST(NULL, 'Geometry')
    ] AS geometry_all,
    -- MultiPoint = Array(Point), appended as a standalone column as well as the
    -- Geometry discriminator-6 child above. Rows [] / [(13, 79)] /
    -- [(1, 2), (3, 4)] / [(-1.5, -2.5)].
    CAST(multiIf(n = 0, [], n = 1, [(13., 79.)], n = 2, [(1., 2.), (3., 4.)], [(-1.5, -2.5)]), 'MultiPoint') AS multipoint,
    -- QBit(T, N): fixed-size logical vectors whose Native body is transposed
    -- into one FixedString(ceil(N/8)) plane per scalar bit. Cover all three
    -- scalar widths, a dimension crossing the 8-element byte boundary, and an
    -- outer Nullable whose null map precedes the complete nested plane body.
    CAST([1.5, -2.5, toFloat64(n + 13)], 'QBit(BFloat16, 3)') AS qbit_bf,
    CAST([
        toFloat32(n), toFloat32(-1.25), toFloat32(0),
        toFloat32(3.5), toFloat32(79.125), toFloat32(-0.0),
        toFloat32(13), toFloat32(-2.5), toFloat32(n + 1)
    ], 'QBit(Float32, 9)') AS qbit_f32,
    CAST([toFloat64(n) + 0.5, -toFloat64(n + 13)], 'QBit(Float64, 2)') AS qbit_f64,
    CAST(
        multiIf(
            n = 1, NULL,
            n = 3, NULL,
            [toFloat32(n * 33 + 13), toFloat32(-toInt64(n) - 0.25)]
        ),
        'Nullable(QBit(Float32, 2))'
    ) AS qbit_nullable
FROM numbers(4)
SETTINGS allow_suspicious_low_cardinality_types = 1, enable_nullable_tuple_type = 1, enable_time_time64_type = 1, flatten_nested = 0
FORMAT Native
SQL
)

multi_block_query=$(
  cat <<'SQL'
SELECT CAST(number + 13, 'Int32') AS n
FROM numbers(5)
SETTINGS max_block_size = 2
FORMAT Native
SQL
)

# A single JSON column reused for the two setting-gated wire shapes below. It is
# the same typed-path + dynamic + shared-spill data as `all_types`' j_typed, so
# the STRING and FLATTENED captures show how one identical column serializes
# under each setting. NativeWriter emits the structured V1/V2 form by default;
# these two settings switch it to the STRING and FLATTENED forms instead.
json_aux_query=$(
  cat <<'SQL'
WITH number AS n
SELECT CAST(multiIf(n = 0, '{"a":{"b":13}, "x":"user_1"}', n = 1, '{"a":{"b":79}, "y":7}', n = 2, '{"x":"user_2", "z":[1,2]}', '{}'), 'JSON(max_dynamic_paths=1, `a.b` Int64)') AS j
FROM numbers(4)
SETTINGS flatten_nested = 0
FORMAT Native
SQL
)

capture() {
  local name="$1"
  local query="$2"
  shift 2

  "${curl_common[@]}" \
    "$@" \
    --data-urlencode "query=${query}" \
    -o "${fixture_dir}/${name}"

  xxd -g1 -l 16 "${fixture_dir}/${name}"
}

capture "all_types_rev0.native" "${all_types_query}"
capture "all_types_rev54485.native" "${all_types_query}" \
  --data-urlencode "client_protocol_version=54485"
capture "multi_block_rev0.native" "${multi_block_query}"

# Setting-gated JSON wire shapes, captured at the default (rev 0) HTTP protocol
# like `all_types_rev0.native`. Only the extra output-format setting differs.
# STRING mode re-serializes one JSON document string per row (a Text body).
capture "json_string_rev0.native" "${json_aux_query}" \
  --data-urlencode "output_format_native_write_json_as_string=1"
# FLATTENED mode writes structure word 3: typed paths, then one shared-less
# Dynamic per flattened path (the union of the dynamic and shared-data paths),
# and NO shared-data stream.
capture "json_flattened_rev0.native" "${json_aux_query}" \
  --data-urlencode "output_format_native_use_flattened_dynamic_and_json_serialization=1"

echo "Wrote fixtures to ${fixture_dir}"
