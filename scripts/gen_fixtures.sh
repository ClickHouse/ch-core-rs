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
    fromUnixTimestamp64Milli(multiIf(n = 0, toInt64(-877), n = 1, toInt64(0), n = 2, toInt64(1705322096789), toInt64(4102444799999)), 'UTC') AS dt64_utc
FROM numbers(4)
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
capture "all_types_rev54483.native" "${all_types_query}" \
  --data-urlencode "client_protocol_version=54483"
capture "multi_block_rev0.native" "${multi_block_query}"

echo "Wrote fixtures to ${fixture_dir}"
