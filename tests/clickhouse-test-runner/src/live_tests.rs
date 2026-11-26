//! Tests that need a ClickHouse server on `localhost` (HTTP 8123, native
//! 9000), such as the server of "Run the tests locally" in README.md. Set
//! `CLICKHOUSE_TEST_RUNNER_LIVE=1` to run them; without it they pass without
//! doing anything. Each test runs over both protocols of the shim, unless it
//! says otherwise.

use crate::run_with_log;
use std::io::Read;

const LIVE_ENV: &str = "CLICKHOUSE_TEST_RUNNER_LIVE";
const URL: &str = "http://localhost:8123/";
const PROTOCOLS: [&str; 2] = ["http", "tcp"];

fn live() -> bool {
    if std::env::var(LIVE_ENV).as_deref() == Ok("1") {
        return true;
    }
    eprintln!(
        "skipped: set {LIVE_ENV}=1 to run the tests against a ClickHouse server on localhost"
    );
    false
}

/// The server's own TabSeparated text for a query.
fn server_tsv(query: &str) -> Vec<u8> {
    server_output(query, "TabSeparated")
}

/// The server's own text for a query without a FORMAT clause, in `format`.
fn server_output(query: &str, format: &str) -> Vec<u8> {
    let agent: ureq::Agent = ureq::Agent::config_builder().proxy(None).build().into();
    let mut resp = agent
        .post(URL)
        .query("default_format", format)
        .query("enable_json_type", "1")
        .query("allow_experimental_qbit_type", "1")
        .send(query.as_bytes())
        .unwrap_or_else(|e| panic!("server error for {query}: {e}"));
    let mut body = Vec::new();
    resp.body_mut().as_reader().read_to_end(&mut body).unwrap();
    body
}

/// Runs the shim with `--query` over `protocol` and returns stdout, stderr
/// and the exit code.
fn shim(protocol: &str, query: &str) -> (String, String, i32) {
    let (out, err, code) = shim_bytes(protocol, query);
    (String::from_utf8_lossy(&out).into_owned(), err, code)
}

fn shim_bytes(protocol: &str, query: &str) -> (Vec<u8>, String, i32) {
    let (out, err, code, _) = shim_with(protocol, &[], query);
    (out, err, code)
}

/// Runs the shim with more arguments before `--query`, and returns stdout,
/// stderr, the exit code and the invocation log.
fn shim_with(protocol: &str, args: &[&str], query: &str) -> (Vec<u8>, String, i32, String) {
    let mut argv = vec!["--host".to_owned(), "localhost".to_owned()];
    argv.extend(args.iter().map(|a| a.to_string()));
    argv.extend(["--query".to_owned(), query.to_owned()]);
    let path = temp_log_path();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_with_log(
        &argv,
        Some(path.to_str().unwrap().to_owned()),
        Some(protocol),
        &mut std::io::empty(),
        &mut out,
        &mut err,
    );
    let log = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    (out, String::from_utf8_lossy(&err).into_owned(), code, log)
}

/// A file name for an invocation log that no other test uses.
fn temp_log_path() -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("shim-live-{}-{nanos}-{n}.log", std::process::id()))
}

/// For each expression, the text that the shim writes from the columns that
/// ch-core-rs decoded must be the same as the server's text.
#[test]
fn render_matches_server() {
    if !live() {
        return;
    }
    let exprs = [
        // Integers.
        "toInt8(-1), toUInt8(255), toInt64(-9223372036854775808), toUInt64(18446744073709551615)",
        "toInt128(-170141183460469231731687303715884105728), toUInt128(340282366920938463463374607431768211455)",
        "toInt256(-57896044618658097711785492504343953926634992332820282019728792003956564819968), toUInt256(115792089237316195423570985008687907853269984665640564039457584007913129639935)",
        "toInt256(-1), toInt256(0), toUInt256(10000000000000000000), toInt256('-12345678901234567890123')",
        // Floats.
        "arrayJoin([1e15, 1e21, 1.5e-7, 0.1 + 0.2, nan, inf, -inf, -0., 5e-324, 1e-6, 123456789012345680000])",
        "toFloat32(0.1), toFloat32(3.4028235e38), toFloat32(16777216), toFloat32(1e-7), toFloat32(-0.)",
        "toBFloat16(1.5), toBFloat16(0.1), toBFloat16(1e20), toBFloat16(-3.14159)",
        // `0/0` and `sqrt(-1)` are NaN with the sign bit set; the server writes `nan`.
        "0/0, -(0/0), sqrt(-1), [0/0], tuple(0/0), toFloat32(0/0), toBFloat16(0/0), toNullable(0/0)",
        // Decimals.
        "toDecimal32(1.50, 2), toDecimal64(-0.001, 5), toDecimal128(100, 3), toDecimal256('1.5', 10)",
        "toDecimal32(0, 4), toDecimal64(-12345.6789, 4), toDecimal128('-0.00000000000000000001', 20)",
        "toDecimal256('-123456789012345678901234567890123456789.123456789', 30), toDecimal256(0, 0)",
        // Strings.
        "'a\\tb\\\\c\\nd''e\\0\\b\\f\\v\\x01\\r', toFixedString('ab', 4), '', '\u{fc}\u{20ac}\u{1f600}'",
        "unhex('FF00C3'), [unhex('FF')], toFixedString(unhex('FF'), 2)",
        // NULL and Bool.
        "true, false, CAST(NULL AS Nullable(String)), toNullable(1), CAST(NULL AS Nullable(Int32))",
        // Dates and times.
        "toDate('2020-01-02'), toDate32('1900-01-01'), toDate32('2299-12-31'), toDate('2149-06-06'), toDate('1970-01-01')",
        // Date32 values outside 0000-01-01 .. 9999-12-31: the server writes the nearest end.
        "fromDaysSinceYearZero32(2147483648), fromDaysSinceYearZero32(toUInt64(2932897) + 719528), [fromDaysSinceYearZero32(toUInt64(2932896) + 719528)], toDate32(0) - 800000, toNullable(toDate32(0) - 719529)",
        "toDateTime('2020-01-01 10:00:00', 'UTC'), toDateTime(0, 'UTC'), toDateTime(4294967295, 'UTC')",
        // At `client_protocol_version` 54485 a top-level `DateTime('tz')` keeps its time zone.
        "toDateTime('2020-01-01 10:00:00', 'Asia/Tokyo'), [toDateTime('2020-01-01 10:00:00', 'Asia/Tokyo')], toNullable(toDateTime('2020-06-01 10:00:00', 'Europe/Amsterdam')), tuple(toDateTime('2020-01-01 10:00:00', 'America/New_York'))",
        "toDateTime64('2020-01-01 00:00:00.120', 3), toDateTime64('2020-01-01 00:00:00', 9, 'Asia/Tokyo'), toDateTime64('1969-12-31 23:59:59.99', 2, 'UTC')",
        "toDateTime64('1900-01-01 00:00:00', 3, 'UTC'), toDateTime64('2299-12-31 23:59:59.999999', 6, 'UTC'), toDateTime64('1677-09-21 00:12:44', 0, 'UTC'), toDateTime64('2262-04-11 23:47:16.854775807', 9, 'UTC'), toDateTime64('1677-09-21 00:12:44', 9, 'UTC')",
        "toDateTime64('1600-01-01 00:00:00', 3, 'UTC'), toDateTime64('2500-01-01 00:00:00', 3, 'UTC')",
        // Daylight saving time after 2099 (chrono-tz has no transitions there).
        "[toDateTime('2105-07-28 04:08:00', 'Europe/Amsterdam'), toDateTime('2101-03-01 04:08:00', 'Europe/Amsterdam')], toDateTime64('2200-07-28 04:08:00', 0, 'Europe/Amsterdam'), toNullable(toDateTime64('2299-07-28 04:08:00', 3, 'America/New_York'))",
        // Fixed-offset time zones (`Fixed/UTC+HH:MM:SS`, `Fixed/UTC-HH:MM:SS`; `-` is west of UTC).
        "toDateTime64(0, 3, 'Fixed/UTC+14:00:00'), toDateTime64(0, 3, 'Fixed/UTC-14:00:00'), [toDateTime(0, 'Fixed/UTC+05:30:00')], tuple(toDateTime(1700000000, 'Fixed/UTC-08:00:00')), toNullable(toDateTime64(1700000000.5, 1, 'Fixed/UTC+05:45:00'))",
        // Values outside 0000-01-01 00:00:00 .. 9999-12-31 23:59:59 in the time zone of the
        // type: the server writes the nearest end of that range, with the fraction of the value.
        "reinterpret(toInt64(9223372036854775806), 'DateTime64(6, \\'UTC\\')'), reinterpret(toInt64(-9223372036854775808), 'DateTime64(3, \\'UTC\\')'), reinterpret(toInt64(9223372036854775807), 'DateTime64(0, \\'Asia/Tokyo\\')'), reinterpret(toInt64(-9223372036854775807), 'DateTime64(9, \\'America/New_York\\')')",
        "reinterpret(toInt64(253402304399), 'DateTime64(0, \\'Asia/Tokyo\\')'), reinterpret(toInt64(253402304399), 'DateTime64(0, \\'America/New_York\\')'), reinterpret(toInt64(-62167222800), 'DateTime64(0, \\'America/New_York\\')'), reinterpret(toInt64(-62167219200123), 'DateTime64(3, \\'UTC\\')'), reinterpret(toInt64(253402300800500), 'DateTime64(3, \\'UTC\\')')",
        "[reinterpret(toInt64(9223372036854775806), 'DateTime64(6, \\'Asia/Kolkata\\')')], toNullable(reinterpret(toInt64(-9223372036854775807), 'DateTime64(2, \\'Europe/Moscow\\')'))",
        "toTime('12:34:56'), toTime('-1:02:03'), toTime64('12:00:00.5', 3), toTime64('-00:00:01.1234567', 7), toTime64('00:00:01.12345678', 8), toTime64('100:00:00.123456789', 9)",
        "toTime(3600000), toTime64(3600000.5, 2), toTime(-3600000)",
        // UUID and IP addresses.
        "toUUID('61f0c404-5cb3-11e7-907b-a6006ad3dba0'), toUUID('00000000-0000-0000-0000-000000000000')",
        "toIPv4('1.2.3.4'), toIPv4('255.255.255.255'), toIPv6('::ffff:1.2.3.4'), toIPv6('::1.2.3.4'), toIPv6('2001:db8::1'), toIPv6('::'), toIPv6('1:0:0:2:0:0:0:3')",
        // Arrays, tuples and maps.
        "[1, 2], ['a''b', NULL], [[1], []], [toDate('2020-01-01')], [toDateTime64('2020-01-01 00:00:00.5', 2)]",
        "(1, 'x', CAST(NULL AS Nullable(UInt8))), [(1, 'a')], (toInt128(1), toUInt256(2)), tuple(1)",
        "CAST((1, NULL) AS Tuple(Int32, LowCardinality(Nullable(String)))), [toLowCardinality(toNullable('x')), NULL]",
        "CAST((1, 'x') AS Nullable(Tuple(UInt8, String))), CAST(NULL AS Nullable(Tuple(UInt8, String)))",
        "[CAST((1, NULL) AS Tuple(UInt8, Nullable(String)))], tuple(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16)",
        "map('k', 1, 'a', 2, 'z', 3, 'b', 4), map(1, [2]), map('x', map('y', 1, 'a', 2)), map([1], 'a'), map(map(1, 2), 'b')",
        "[map('b', 1, 'a', 2)], (map('z', 1, 'y', 2), 1), map('k', NULL, 'j', 2)",
        "CAST('a' AS Enum8('a' = 1, 'b' = 2)), [CAST('b' AS Enum16('a' = -1000, 'b' = 2))], CAST('x''y' AS Enum8('x''y' = -128))",
        "toLowCardinality('x'), toLowCardinality(toNullable('y')), [toLowCardinality('z')], toLowCardinality(toNullable(1))",
        "INTERVAL 3 DAY, toIntervalSecond(-5), toIntervalMonth(1)",
        "(1.5, 2.5)::Point",
        "sumSimpleState(number), anySimpleState(toString(number)) FROM numbers(3)",
        // A tuple in a `SimpleAggregateFunction` has the encoding of the tuple.
        "CAST(([1, 2], [3, 4]) AS SimpleAggregateFunction(sumMap, Tuple(Array(UInt8), Array(UInt64))))",
        "CAST([(1, 'a'), (2, NULL)] AS SimpleAggregateFunction(groupArrayArray, Array(Tuple(UInt8, Nullable(String)))))",
        // Nullable with a null-map byte other than 1 (the server can send 2, 3, ...).
        "if(number % 5, NULL, toInt8OrZero(toString(number))) FROM numbers(6)",
        "number, toString(number), if(number % 2, NULL, number) FROM numbers(3)",
        "number, repeat('x', number % 7) FROM numbers(200000) WHERE number % 49999 = 0",
        "toLowCardinality(toString(number % 3)) FROM numbers(10)",
        // Types that only some clients decode.
        "NULL, [], map(), [NULL], tuple(), [tuple()], CAST(NULL AS Nullable(Nothing))",
        "CAST((1, 'x') AS Tuple(a Int32, `b c` String)), CAST([(1, 'y')] AS Array(Tuple(`x.y` UInt8, `1` String)))",
        "(1, (2, 'x'), [(3, (4, 5))]), CAST((1, NULL) AS Nullable(Tuple(UInt8, Nullable(String)))), map((1, 'a'), [2]), map([1], map(2, 3))",
        "1::Variant(UInt64, String), 'a\\tb'::Variant(UInt64, String), [1::Variant(UInt64, String), 'x', NULL], NULL::Variant(UInt64, String), (1, 'a')::Variant(Tuple(UInt8, String), UInt8)",
        "1::Dynamic, 'a\\tb'::Dynamic, ['a']::Dynamic, [['a']::Dynamic], NULL::Dynamic, (1, 'x''y')::Dynamic, toDate('2020-01-01')::Dynamic, [toDate('2020-01-01')::Dynamic], map('k', 'v')::Dynamic",
        "CAST(number, 'Dynamic(max_types=1)') FROM numbers(3)",
        "if(number % 2, number::Dynamic(max_types=1), toString(number)::Dynamic(max_types=1)), if(number = 2, [1.5]::Dynamic(max_types=0), NULL) FROM numbers(4)",
        "[1, 2, 3]::QBit(Float32, 3), [0.5, -2]::QBit(Float64, 2), [1.5]::QBit(BFloat16, 1), [[1, 2]::QBit(Float32, 2)]",
        "countState(1), sumState(toUInt8(3)), [countState(2)] FROM numbers(1)",
        "(1, 2)::Point, [(1, 2), (3, 4)]::Ring, [[(1, 2)]]::Polygon, [[[(1, 2)]]]::MultiPolygon, [(1, 2)]::LineString, [[(1, 2)]]::MultiLineString",
        "(1, 2)::Point::Geometry, [(1, 2)]::Ring::Geometry, NULL::Geometry",
        "CAST([(1, 'a')] AS Nested(x UInt8, y String))",
        "toLowCardinality(toNullable('a')), [toLowCardinality(toNullable('b')), NULL], map(toLowCardinality('k'), toLowCardinality(toNullable('v'))), CAST((NULL, 1) AS Tuple(a LowCardinality(Nullable(String)), b UInt8))",
        "if(number = 1, NULL, number) AS n, toNullable(toString(number)), [if(number = 0, NULL, number)] FROM numbers(3)",
        // JSON: sorted paths, nested objects, typed paths always (NULL as null), the
        // JSON text escaped (or quoted) as one string.
        r#"'{"a":1,"b":{"c":"x/y\\u0001\\"z","d":[1,null,2]},"e":null,"f":1.5,"g":true,"h":"2020-01-01","i":-9223372036854775808,"j":18446744073709551615,"k":[{"x":1}],"l":{},"m":[[1,2],[3]],"n":1e300,"o":"","p":["a",1]}'::JSON"#,
        r#"'{"a":{"b":1},"a.c":2}'::JSON, '{}'::JSON, ['{"a":"q''s"}'::JSON], tuple('{"a":13}'::JSON), CAST(NULL AS Nullable(JSON))"#,
        r#"CAST('{"a":1,"b":2.5,"d":{"x":1,"y":"s"},"e":"x"}' AS JSON(a Int128, b Float64, d Tuple(x UInt8, y String), e LowCardinality(String)))"#,
        r#"CAST('{"z":{"k":1},"t":[1,"s"],"v":[1.5,null]}' AS JSON(z Map(String, UInt8), t Tuple(UInt8, String), v Array(Nullable(Float32))))"#,
        r#"CAST('{"a":1}' AS JSON(b UInt256, c Decimal(38, 5), d Date32, e Time, f Array(Array(String)), w Nullable(String), u UUID, ip IPv4, fs FixedString(2), dt DateTime64(3, 'Asia/Tokyo')))"#,
        r#"CAST(map('s', concat('a', char(31), char(127), unhex('E280A8'), unhex('FF'))) AS JSON), CAST(map('x.y', 1, '', 2) AS JSON)"#,
        r#"'{"a":[{"b":[{"c":1}]}]}'::JSON, '{"a":[]}'::JSON, '{"x":[1,2.5]}'::JSON, '{"y":[1,"a"]}'::JSON"#,
        // Paths over max_dynamic_paths go to the shared data.
        r#"CAST('{"a":[1,2],"b":{"c":1},"d":"x"}' AS JSON(max_dynamic_paths=1)), CAST('{"q":1}' AS JSON(max_dynamic_paths=0))"#,
        r#"CAST(concat('{"k":', toString(number), ',"s":"v', toString(number), '"}') AS JSON) FROM numbers(3)"#,
        // A column or alias named `format` is not the FORMAT clause.
        "1 AS format ORDER BY format DESC",
        "1 AS format ORDER BY format ASC",
        "format FROM (SELECT 1 AS format)",
        "1 AS format FROM numbers(1)",
        // `?` is not a bind placeholder.
        "'?', 1 ? 2 : 3",
    ];
    let mut failures = Vec::new();
    for expr in exprs {
        let query = format!(
            "SELECT {expr} SETTINGS enable_json_type = 1, allow_experimental_qbit_type = 1, \
             allow_suspicious_variant_types = 1"
        );
        let want = server_tsv(&query);
        for protocol in PROTOCOLS {
            let (got, err, code) = shim_bytes(protocol, &query);
            if code != 0 || got != want {
                failures.push(format!(
                    "{protocol}: {query}\n  server: {:?}\n  shim:   {:?} (exit code {code}, stderr {err:?})",
                    String::from_utf8_lossy(&want),
                    String::from_utf8_lossy(&got),
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The TSV variants of the output format.
#[test]
fn output_formats() {
    if !live() {
        return;
    }
    let cases = [
        (
            "SELECT 1 AS a, 'x\\ty' AS `b c` FORMAT TSVWithNames",
            "a\tb c\n1\tx\\ty\n",
        ),
        (
            "SELECT 1 AS a, 'x\\ty' AS b FORMAT TabSeparatedWithNamesAndTypes",
            "a\tb\nUInt8\tString\n1\tx\\ty\n",
        ),
        ("SELECT 'x\\ty' FORMAT TSVRaw", "x\ty\n"),
        ("SELECT ['x\\ty'] FORMAT TSVRaw", "['x\\ty']\n"),
        ("SELECT 1 FORMAT Null", ""),
        ("SELECT 1 FORMAT `Null`", ""),
        ("SELECT 1 FORMAT tsv SETTINGS max_threads = 1", "1\n"),
    ];
    for protocol in PROTOCOLS {
        for (query, want) in cases {
            let (got, err, code) = shim(protocol, query);
            assert_eq!(
                (got.as_str(), code),
                (want, 0),
                "{protocol}: {query}: {err}"
            );
        }
        let (_, err, code) = shim(protocol, "SELECT 1 FORMAT JSONEachRow");
        assert_eq!(code, 1, "{protocol}");
        assert!(
            err.contains("output format JSONEachRow is not supported"),
            "{protocol}: {err}"
        );
    }
}

/// The header lines (names, types): a Raw format writes them without
/// escapes, as its rows. Contrast: TSVWithNames escapes the header.
#[test]
fn header_of_format() {
    if !live() {
        return;
    }
    let select = "SELECT 1 AS `a\\tb\\\\c'd`, 'x' AS `\u{e9}\\nf`, \
                  CAST('a' AS Enum8('a\\tb' = 1, 'a' = 2)) AS e";
    let formats = [
        "TSVWithNames",
        "TSVWithNamesAndTypes",
        "TSVRawWithNames",
        "TabSeparatedRawWithNamesAndTypes",
        "RawWithNames",
    ];
    let mut failures = Vec::new();
    for format in formats {
        let query = format!("{select} FORMAT {format}");
        let want = server_output(select, format);
        for protocol in PROTOCOLS {
            let (got, err, code) = shim_bytes(protocol, &query);
            if code != 0 || got != want {
                failures.push(format!(
                    "{protocol}: {format}\n  server: {:?}\n  shim:   {:?} (exit code {code}, stderr {err:?})",
                    String::from_utf8_lossy(&want),
                    String::from_utf8_lossy(&got),
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// `SET`, `USE` and temporary tables last for the whole invocation, also
/// after an error that a test hint expects.
#[test]
fn session_state() {
    if !live() {
        return;
    }
    let script = [
        "SET max_threads = 3;",
        "SELECT throwIf(1); -- { serverError FUNCTION_THROW_IF_VALUE_IS_NON_ZERO }",
        "SELECT getSetting('max_threads');",
        "CREATE TEMPORARY TABLE t (x UInt8);",
        "INSERT INTO t VALUES (1), (2);",
        "INSERT INTO t FORMAT TSV",
        "3",
        "",
        "INSERT INTO t FORMAT CSV",
        "4",
        "5",
        "",
        "SELECT sum(x) FROM t;",
        "USE system;",
        "SELECT currentDatabase();",
        "SELECT 1 FROM one;",
    ]
    .join("\n");
    for protocol in PROTOCOLS {
        let (got, err, code) = shim(protocol, &script);
        assert_eq!(
            (got.as_str(), code),
            ("3\n15\nsystem\n1\n", 0),
            "{protocol}: {err}"
        );
    }
}

/// The settings that `tests/clickhouse-test` gives for the tags
/// `no-async-insert` and `no-parallel-replicas` go to the server with each
/// query. With `--async_insert 0`, an INSERT into a MergeTree table does not go
/// through the asynchronous insert queue; without it, it does (the server's
/// default of `async_insert` is 1).
#[test]
fn feature_tag_settings() {
    if !live() {
        return;
    }
    let query = "SELECT getSetting('async_insert'), getSetting('enable_parallel_replicas')";
    let want = server_tsv(&format!(
        "{query} SETTINGS async_insert = 0, enable_parallel_replicas = 0"
    ));
    let args = ["--async_insert", "0", "--enable_parallel_replicas", "0"];
    for protocol in PROTOCOLS {
        let table = format!("shim_live_feature_tags_{protocol}");
        let script = [
            format!("DROP TABLE IF EXISTS {table};"),
            format!("CREATE TABLE {table} (x UInt8) ENGINE = MergeTree ORDER BY x;"),
            format!("EXPLAIN PIPELINE INSERT INTO {table} SELECT 1;"),
            format!("DROP TABLE {table};"),
        ]
        .join("\n");
        let (got, err, code, _) = shim_with(protocol, &args, &format!("{script}\n{query};"));
        let got = String::from_utf8_lossy(&got);
        assert!(
            code == 0
                && got.contains("MergeTreeSink")
                && !got.contains("AsyncInsertQueueTransform")
                && got.ends_with(std::str::from_utf8(&want).unwrap()),
            "{protocol}: {got:?} {err}"
        );
        // Contrast: without the setting, the server's default.
        let (got, err, code) = shim(protocol, &script);
        assert!(
            code == 0 && got.contains("AsyncInsertQueueTransform"),
            "{protocol} (no settings): {got:?} {err}"
        );
    }
}

/// Column types in the binary encoding
/// (`output_format_native_encode_types_in_binary_format = 1`): over HTTP the
/// shim decodes in that mode, after a `SET` of the session and with the
/// `SETTINGS` clause of the statement, and in the text mode again after the
/// setting is off. Over tcp the engine frames each block in the text mode
/// (`crates/ch-tcp-proto/README.md`), and the error names its source.
#[test]
fn binary_type_headers() {
    if !live() {
        return;
    }
    let row = server_tsv("SELECT toUInt8(7) AS x, 'ab' AS s");
    let row = String::from_utf8(row).unwrap();
    let off = String::from_utf8(server_tsv("SELECT toInt16(-5)")).unwrap();
    let session = "SET output_format_native_encode_types_in_binary_format = 1;\n\
                   SELECT toUInt8(7) AS x, 'ab' AS s;\n\
                   SET output_format_native_encode_types_in_binary_format = 0;\n\
                   SELECT toInt16(-5);";
    let clause = "SELECT toUInt8(7) AS x, 'ab' AS s \
                  SETTINGS output_format_native_encode_types_in_binary_format = 1;\n\
                  SELECT toInt16(-5);";
    for script in [session, clause] {
        let (got, err, code) = shim("http", script);
        assert_eq!((got, code), (format!("{row}{off}"), 0), "http: {err}");
        let (_, err, code) = shim("tcp", script);
        assert_ne!(code, 0, "tcp");
        assert!(
            err.contains("ch-tcp-proto: Native block decode failed: "),
            "tcp: {err}"
        );
    }
}

#[test]
fn errors() {
    if !live() {
        return;
    }
    for protocol in PROTOCOLS {
        let shim = |query: &str| shim(protocol, query);
        // An unexpected error stops the script, writes the error name to
        // stderr, and sets the exit code to the error code.
        let (got, err, code) = shim("SELECT 1; SELECT * FROM no_such_table_xyz; SELECT 2;");
        assert_eq!(got, "1\n", "{protocol}");
        assert_eq!(code, 60, "{protocol}");
        assert!(err.contains("UNKNOWN_TABLE"), "{protocol}: {err}");

        // A query that succeeds although the hint expects an error.
        let (_, err, code) = shim("SELECT 1; -- { serverError UNKNOWN_TABLE }");
        assert_ne!(code, 0, "{protocol}");
        assert!(
            err.contains("expected serverError UNKNOWN_TABLE"),
            "{protocol}: {err}"
        );

        // A different error than the hint expects.
        let (_, err, code) =
            shim("SELECT * FROM no_such_table_xyz; -- { serverError SYNTAX_ERROR }");
        assert_eq!(code, 60, "{protocol}: stderr: {err}");

        // A syntax error satisfies a clientError hint, and is not echoed.
        let (got, err, code) =
            shim("-- { echo }\nSELECT 1;\nSELEC 2; -- { clientError SYNTAX_ERROR }\nSELECT 3;");
        assert_eq!(
            (got.as_str(), code),
            ("-- { echo }\nSELECT 1;\n1\nSELECT 3;\n3\n", 0),
            "{protocol}: {err}"
        );

        // A syntax error in the argument of `formatQuery` comes after the
        // statement parsed, so clickhouse-client echoes the statement.
        let script = "-- { echo }\nSELECT formatQuery('SELECT 1 +'); -- { serverError SYNTAX_ERROR }\nSELECT 3;";
        let (got, err, code) = shim(script);
        assert_eq!(
            (got.as_str(), code),
            (format!("{script}\n3\n").as_str(), 0),
            "{protocol}: {err}"
        );

        // A Poco exception has no `Code: N.` prefix in its text
        // (`Poco::Exception. Code: 1000, ...`), but satisfies its hint.
        let (got, err, code) = shim(
            "SELECT * FROM url('http://127.0.0.1:1337/? HTTP/1.1\\r\\nTest: test', CSV, 'c String'); -- { serverError POCO_EXCEPTION }\nSELECT 1;",
        );
        assert_eq!((got.as_str(), code), ("1\n", 0), "{protocol}: {err}");

        // An exception in the middle of the result.
        let (_, err, code) = shim(
            "SELECT throwIf(number = 100000) FROM numbers(200000) SETTINGS max_block_size = 1000",
        );
        assert_eq!(code, 395 % 256, "{protocol}: stderr: {err}");
    }
}

/// Statement forms that the ClickHouse lexer accepts: `//` comments, Unicode
/// whitespace before the keyword, and `KILL`, which returns rows.
#[test]
fn statement_forms() {
    if !live() {
        return;
    }
    for protocol in PROTOCOLS {
        let (got, err, code) = shim(protocol, "SELECT 1; // \u{3bc}s\nSELECT 2 // x;\n;");
        assert_eq!((got.as_str(), code), ("1\n2\n", 0), "{protocol}: {err}");

        let (got, err, code) = shim(
            protocol,
            "\u{0085}\u{00A0} \u{2000}\u{2028}SELECT\u{2029}\u{3000}3\u{FEFF};",
        );
        assert_eq!((got.as_str(), code), ("3\n", 0), "{protocol}: {err}");

        // The mutation cannot finish ('x' is not a number), so it waits until
        // KILL MUTATION stops it, and KILL MUTATION writes it as a row.
        let script = [
            "DROP TABLE IF EXISTS shim_live_kill;",
            "CREATE TABLE shim_live_kill (v String) ENGINE = MergeTree ORDER BY tuple();",
            "INSERT INTO shim_live_kill VALUES ('x');",
            "ALTER TABLE shim_live_kill MODIFY COLUMN v UInt64 SETTINGS mutations_sync = 2; -- { serverError UNFINISHED }",
            "KILL MUTATION WHERE database = currentDatabase() AND table = 'shim_live_kill';",
            "DROP TABLE shim_live_kill;",
        ]
        .join("\n");
        let (got, err, code) = shim(protocol, &script);
        assert_eq!(code, 0, "{protocol}: {err}");
        assert!(
            got.starts_with("waiting\tdefault\tshim_live_kill\tmutation_2.txt\t")
                && got.lines().count() == 1,
            "{protocol}: {got:?}"
        );
    }
}

/// After `SET dialect`, every statement runs as a query, without its
/// trailing semicolon.
#[test]
fn other_dialect() {
    if !live() {
        return;
    }
    let script = "SET allow_experimental_kusto_dialect = 1;\nSET dialect = 'kusto';\nprint x = 1 + 2;\nSET dialect = 'clickhouse';\nSELECT 4;";
    for protocol in PROTOCOLS {
        let (got, err, code) = shim(protocol, script);
        assert_eq!((got.as_str(), code), ("3\n4\n", 0), "{protocol}: {err}");
    }
}

/// The invocation log must not hold the password, because CI keeps the log as
/// an artifact. Only the log is checked: the server can refuse the password.
#[test]
fn log_hides_the_password() {
    if !live() {
        return;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let secret = format!("pw{}x{nanos}", std::process::id());
    let forms = [
        (
            vec!["--password".to_owned(), secret.clone()],
            "--password ***",
        ),
        (vec![format!("--password={secret}")], "--password=***"),
    ];
    for protocol in PROTOCOLS {
        for (password, shown) in &forms {
            let args: Vec<&str> = password.iter().map(String::as_str).collect();
            let (_, _, _, log) = shim_with(protocol, &args, "SELECT 1");
            assert!(!log.contains(&secret), "{protocol}: {log}");
            assert!(
                log.contains(&format!("argv=--host localhost {shown} --query SELECT 1\n")),
                "{protocol}: {log}"
            );
            // The run got past the arguments, so the log also has the
            // records of the runner: over http the query (the server checks
            // the password with the request), over tcp the error of the
            // handshake when the server refuses the password.
            let record = if protocol == "http" {
                "\nquery=SELECT 1\n"
            } else {
                "\naddress=localhost:9000\n"
            };
            assert!(log.contains(record), "{protocol}: {log}");
        }
    }
}

/// `WITH TOTALS` and `extremes = 1` over tcp: an empty line, then the totals
/// row, and an empty line, then the minimum and maximum rows, as the server's
/// own TabSeparated output. Only tcp: HTTP `Native` has no totals and no
/// extremes (the `only=tcp` tests of the allowlist).
#[test]
fn totals_and_extremes() {
    if !live() {
        return;
    }
    let cases = [
        (
            "SELECT number % 3 AS k, count() AS c FROM numbers(10) GROUP BY k WITH TOTALS ORDER BY k",
            "TabSeparated",
        ),
        (
            "SELECT number, toString(number) AS s FROM numbers(3) SETTINGS extremes = 1",
            "TabSeparated",
        ),
        (
            "SELECT number % 2 AS k, sum(number) AS s FROM numbers(7) GROUP BY k WITH TOTALS ORDER BY k SETTINGS extremes = 1",
            "TSVWithNamesAndTypes",
        ),
        (
            "SELECT toNullable(number) AS n, [number] AS a FROM numbers(2) GROUP BY number WITH TOTALS ORDER BY number",
            "TSVWithNames",
        ),
    ];
    for (select, format) in cases {
        let want = server_output(select, format);
        let (got, err, code) = shim_bytes("tcp", &format!("{select} FORMAT {format}"));
        assert_eq!(
            (String::from_utf8_lossy(&got), code),
            (String::from_utf8_lossy(&want), 0),
            "{select}: {err}"
        );
    }
    // FORMAT Null writes neither.
    let (got, err, code) = shim(
        "tcp",
        "SELECT number % 2 AS k, count() FROM numbers(4) GROUP BY k WITH TOTALS FORMAT Null SETTINGS extremes = 1",
    );
    assert_eq!((got.as_str(), code), ("", 0), "{err}");
}

/// Over tcp the first block of a result has no rows and gives the header, so
/// an empty result has its header lines, as the server writes them. Over
/// HTTP an empty `Native` result has no block.
#[test]
fn header_of_empty_result() {
    if !live() {
        return;
    }
    let select = "SELECT 1 AS a, 'x' AS `b c`, [toDate(0)] AS d WHERE 0";
    for format in ["TSVWithNames", "TSVWithNamesAndTypes", "TSVRawWithNames"] {
        let want = server_output(select, format);
        let (got, err, code) = shim_bytes("tcp", &format!("{select} FORMAT {format}"));
        assert_eq!(
            (String::from_utf8_lossy(&got), code),
            (String::from_utf8_lossy(&want), 0),
            "{format}: {err}"
        );
    }
}

/// With command-line settings, the engine fails the connection after an
/// exception that is the first packet of the response. The shim then opens a
/// new connection with the database of the last `USE`, and runs the `SET`
/// statements again, so the session keeps them. Over http nothing changes.
#[test]
fn session_after_failed_connection() {
    if !live() {
        return;
    }
    let script = [
        "SET max_threads = 3;",
        "USE system;",
        "SELECT * FROM no_such_table_xyz; -- { serverError UNKNOWN_TABLE }",
        "SELECT getSetting('max_threads'), currentDatabase();",
        "SELEC 1; -- { clientError SYNTAX_ERROR }",
        "SELECT getSetting('max_threads'), getSetting('log_comment');",
    ]
    .join("\n");
    for protocol in PROTOCOLS {
        let (got, err, code, log) = shim_with(protocol, &["--log_comment=shim-live"], &script);
        assert_eq!(
            (String::from_utf8_lossy(&got).as_ref(), code),
            ("3\tsystem\n3\tshim-live\n", 0),
            "{protocol}: {err}"
        );
        let reconnects = log.matches("\ntcp_reconnect=").count();
        let want = if protocol == "tcp" { 2 } else { 0 };
        assert_eq!(reconnects, want, "{protocol}: {log}");
    }
}

/// An INSERT for which the server asks the client for the data
/// (`InsertSchema`): the shim has no input-format parser, so it reports
/// that. Inline data in the query text works (see `session_state`).
#[test]
fn insert_with_data_from_the_client() {
    if !live() {
        return;
    }
    let scripts = [
        // No data at all.
        "CREATE TEMPORARY TABLE t (x UInt8);\nINSERT INTO t VALUES;",
        // `input()`: clickhouse-client parses the data and sends blocks.
        "CREATE TEMPORARY TABLE t (x UInt8);\nINSERT INTO t SELECT * FROM input('x UInt8') FORMAT TSV\n7\n",
    ];
    for script in scripts {
        let (got, err, code) = shim("tcp", script);
        assert_eq!((got.as_str(), code), ("", 1), "{script}");
        assert!(
            err.contains("the server asks the client for the data of this INSERT"),
            "{script}: {err}"
        );
    }
}

/// A script with bytes that are not UTF-8 (in a heredoc, in a string literal,
/// in an `echo`ed statement), on stdin as the test runner gives it. Over HTTP
/// the shim sends the bytes as they are, and the result is the server's own
/// text for the same bytes. Over tcp `ch-tcp-proto` takes the SQL only as a
/// `String`: the statement fails with that error, and the connection stays
/// usable.
#[test]
fn input_not_utf8() {
    if !live() {
        return;
    }
    let query: &[u8] =
        b"SELECT hex($$\xff\xfe\x80\xbb$$) AS h, length('\xe9t\xe9') AS n, 'a\xffb' AS s";
    let agent: ureq::Agent = ureq::Agent::config_builder().proxy(None).build().into();
    let mut resp = agent
        .post(URL)
        .query("default_format", "TabSeparated")
        .send(query)
        .unwrap();
    let mut want = Vec::new();
    resp.body_mut().as_reader().read_to_end(&mut want).unwrap();
    assert_eq!(want, b"FFFE80BB\t3\ta\xffb\n");

    let mut script = b"SELECT 1 AS x; -- { echoOn }\n".to_vec();
    script.extend_from_slice(query);
    script.extend_from_slice(b";\nSELECT 2 AS y;\n");
    for protocol in PROTOCOLS {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_with_log(
            &["--host".to_owned(), "localhost".to_owned()],
            None,
            Some(protocol),
            &mut script.as_slice(),
            &mut out,
            &mut err,
        );
        let err = String::from_utf8_lossy(&err);
        // `echo` writes the statement with its own bytes.
        let mut echoed = b"SELECT 1 AS x; -- { echoOn }\n1\n".to_vec();
        echoed.extend_from_slice(query);
        echoed.extend_from_slice(b";\n");
        if protocol == "http" {
            let mut expected = echoed;
            expected.extend_from_slice(&want);
            expected.extend_from_slice(b"SELECT 2 AS y;\n2\n");
            assert_eq!((code, err.as_ref()), (0, ""), "{protocol}");
            assert_eq!(
                out,
                expected,
                "{protocol}: {}",
                String::from_utf8_lossy(&out)
            );
        } else {
            assert_eq!(code, 1, "{protocol}: {err}");
            assert!(
                err.contains("Query::new takes the SQL as a String (UTF-8)"),
                "{protocol}: {err}"
            );
            assert_eq!(out, echoed, "{protocol}: {}", String::from_utf8_lossy(&out));
        }
    }
}
