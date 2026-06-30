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
    CAST(multiIf(n = 0, 'north', n = 1, 'south', n = 2, 'west', 'north'), 'Enum16(\'north\' = 1, \'south\' = 2, \'west\' = -1)') AS e16
FROM numbers(4)
SETTINGS allow_suspicious_low_cardinality_types = 1
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

echo "Wrote fixtures to ${fixture_dir}"
