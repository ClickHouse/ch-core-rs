use ch_core_rs::batch::ChunkedBatch;
use ch_core_rs::column::{Column, DynamicChild, FixedBinaryColumn, Utf8Column};
use ch_core_rs::native::decode::{decode_all_bytes, DecodeOptions, DBMS_TCP_PROTOCOL_VERSION};
use ch_core_rs::schema::{ChType, GeoKind, IntervalKind};

/// Declare one `#[test]` per committed Native fixture. Each generated test
/// decodes the fixture bytes through the public API and runs its asserter, so
/// libtest reports each fixture separately, a failure names the specific
/// fixture, and one bad fixture does not abort the others.
macro_rules! fixture_tests {
    ($($name:ident: {
        file: $file:literal,
        protocol_revision: $revision:expr,
        assert: $assert:path $(,)?
    }),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                let options = DecodeOptions {
                    protocol_revision: $revision,
                };
                let bytes = include_bytes!(concat!("fixtures/", $file));
                let batch = decode_all_bytes(bytes, &options)
                    .unwrap_or_else(|err| panic!("failed to decode {}: {err}", $file));
                $assert(&batch);
            }
        )*
    };
}

fixture_tests! {
    all_types_rev0: {
        file: "all_types_rev0.native",
        protocol_revision: 0,
        assert: assert_all_types,
    },
    all_types_rev54485: {
        file: "all_types_rev54485.native",
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
        assert: assert_all_types,
    },
    multi_block_rev0: {
        file: "multi_block_rev0.native",
        protocol_revision: 0,
        assert: assert_multi_block,
    },
}

/// Expected schema entry. Most columns pin an exact `ChType`. The `dt_utc`
/// column is the one exception: the server emits a different type string for it
/// depending on the negotiated protocol revision (see `assert_all_types`), so
/// it is matched against a set of acceptable types instead of one.
enum Expected<'a> {
    Exact(&'a str, ChType),
    AnyOf(&'a str, &'a [ChType]),
}

fn assert_schema(batch: &ChunkedBatch, expected: &[Expected]) {
    assert_eq!(batch.num_columns(), expected.len());
    for (field, exp) in batch.schema.fields.iter().zip(expected) {
        match exp {
            Expected::Exact(name, ch_type) => {
                assert_eq!(field.name, *name);
                assert_eq!(&field.ch_type, ch_type);
            }
            Expected::AnyOf(name, types) => {
                assert_eq!(field.name, *name);
                assert!(
                    types.contains(&field.ch_type),
                    "column {name}: type {:?} not in {types:?}",
                    field.ch_type
                );
            }
        }
    }
}

fn assert_all_types(batch: &ChunkedBatch) {
    assert_eq!(batch.num_chunks(), 1);
    assert_eq!(batch.num_rows(), 4);
    assert_schema(
        batch,
        &[
            Expected::Exact("i8", ChType::Int8),
            Expected::Exact("i16", ChType::Int16),
            Expected::Exact("i32", ChType::Int32),
            Expected::Exact("i64", ChType::Int64),
            Expected::Exact("u8", ChType::UInt8),
            Expected::Exact("u16", ChType::UInt16),
            Expected::Exact("u32", ChType::UInt32),
            Expected::Exact("u64", ChType::UInt64),
            Expected::Exact("f32", ChType::Float32),
            Expected::Exact("f64", ChType::Float64),
            Expected::Exact("b", ChType::Bool),
            Expected::Exact("s", ChType::String),
            Expected::Exact("fs", ChType::FixedString(4)),
            Expected::Exact("ni32", ChType::Nullable(Box::new(ChType::Int32))),
            Expected::Exact("ns", ChType::Nullable(Box::new(ChType::String))),
            // Temporal columns, with the exact type strings this server
            // (v26.6.1.1193-stable) emits. A bare DateTime stays bare. The
            // DateTime64 columns keep their precision and timezone in both
            // captures.
            Expected::Exact("d", ChType::Date),
            Expected::Exact("d32", ChType::Date32),
            Expected::Exact("dt", ChType::DateTime { timezone: None }),
            // dt_utc is declared DateTime('UTC') in the query, but the emitted
            // type string depends on the negotiated protocol revision: the
            // rev54485 capture keeps DateTime('UTC'), while the rev0 capture
            // (HTTP FORMAT Native with no client_protocol_version) drops the
            // timezone and emits a bare DateTime. Both are what the server
            // actually wrote, so accept either. The raw seconds are identical
            // either way, and are asserted below.
            Expected::AnyOf(
                "dt_utc",
                &[
                    ChType::DateTime {
                        timezone: Some(String::from("UTC")),
                    },
                    ChType::DateTime { timezone: None },
                ],
            ),
            Expected::Exact(
                "dt64",
                ChType::DateTime64 {
                    precision: 3,
                    timezone: None,
                },
            ),
            Expected::Exact(
                "dt64_utc",
                ChType::DateTime64 {
                    precision: 3,
                    timezone: Some("UTC".to_string()),
                },
            ),
            Expected::Exact("lc", ChType::LowCardinality(Box::new(ChType::String))),
            Expected::Exact(
                "lcn",
                ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String)))),
            ),
            // LowCardinality over non-String inners, captured with
            // allow_suspicious_low_cardinality_types=1.
            Expected::Exact("lc_u32", ChType::LowCardinality(Box::new(ChType::UInt32))),
            Expected::Exact("lc_date", ChType::LowCardinality(Box::new(ChType::Date))),
            Expected::Exact(
                "lcn_u32",
                ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::UInt32)))),
            ),
            // UUID/IPv4/IPv6 and a LowCardinality(UUID).
            Expected::Exact("uuid", ChType::Uuid),
            Expected::Exact("ipv4", ChType::Ipv4),
            Expected::Exact("ipv6", ChType::Ipv6),
            Expected::Exact("lc_uuid", ChType::LowCardinality(Box::new(ChType::Uuid))),
            // Enum8/Enum16. The server emits the variants SORTED ASCENDING BY
            // VALUE in the type string, regardless of the order given at CREATE,
            // so the parsed ChType carries west=-1, north=1, south=2 in that
            // order. The name->value map lives here in the ChType; the per-row
            // wire data is the raw underlying Int8/Int16 only.
            Expected::Exact(
                "e8",
                ChType::Enum8 {
                    variants: vec![
                        ("west".to_string(), -1),
                        ("north".to_string(), 1),
                        ("south".to_string(), 2),
                    ],
                },
            ),
            Expected::Exact(
                "e16",
                ChType::Enum16 {
                    variants: vec![
                        ("west".to_string(), -1),
                        ("north".to_string(), 1),
                        ("south".to_string(), 2),
                    ],
                },
            ),
            // Decimal(P, S): the server always emits the canonical Decimal(P, S)
            // type string, so the creation-time Decimal32(4)/Decimal64(9)/
            // Decimal128(20)/Decimal256(50) come back normalized to
            // Decimal(9, 4)/Decimal(18, 9)/Decimal(38, 20)/Decimal(76, 50), with
            // the bit width derived from the precision (32/64/128/256).
            Expected::Exact(
                "dec32",
                ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 32,
                },
            ),
            Expected::Exact(
                "dec64",
                ChType::Decimal {
                    precision: 18,
                    scale: 9,
                    bits: 64,
                },
            ),
            Expected::Exact(
                "dec128",
                ChType::Decimal {
                    precision: 38,
                    scale: 20,
                    bits: 128,
                },
            ),
            Expected::Exact(
                "dec256",
                ChType::Decimal {
                    precision: 76,
                    scale: 50,
                    bits: 256,
                },
            ),
            // LowCardinality over IPv4/IPv6, captured with
            // allow_suspicious_low_cardinality_types=1. IPv4's dictionary body is a
            // plain UInt32 column body; IPv6's is raw 16-byte rows.
            Expected::Exact("lc_ipv4", ChType::LowCardinality(Box::new(ChType::Ipv4))),
            Expected::Exact("lc_ipv6", ChType::LowCardinality(Box::new(ChType::Ipv6))),
            // Array(T): Arrow list layout. The element type may itself be
            // Nullable, LowCardinality, or a further Array; the array is never
            // nullable at the array level. Order matches the SELECT list.
            Expected::Exact("arr", ChType::Array(Box::new(ChType::Int32))),
            Expected::Exact("arr_s", ChType::Array(Box::new(ChType::String))),
            Expected::Exact(
                "arr_n",
                ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::Int32)))),
            ),
            Expected::Exact(
                "arr_lc",
                ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
            ),
            Expected::Exact(
                "arr_arr",
                ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
            ),
            Expected::Exact(
                "arr_lc_empty",
                ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
            ),
            // Tuple(T1, ...): unnamed, named (names live in the type string
            // only), inside Array, and inside Nullable (legal:
            // DataTypeTuple::canBeInsideNullable() is true).
            Expected::Exact(
                "tup",
                ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
            ),
            Expected::Exact(
                "tup_named",
                ChType::Tuple(vec![
                    (Some("a".to_string()), ChType::Int32),
                    (
                        Some("b".to_string()),
                        ChType::Nullable(Box::new(ChType::String)),
                    ),
                ]),
            ),
            Expected::Exact(
                "arr_tup",
                ChType::Array(Box::new(ChType::Tuple(vec![
                    (None, ChType::Int32),
                    (None, ChType::Int32),
                ]))),
            ),
            Expected::Exact(
                "ntup",
                ChType::Nullable(Box::new(ChType::Tuple(vec![
                    (None, ChType::Int32),
                    (None, ChType::String),
                ]))),
            ),
            // Map(K, V): the Array(Tuple(keys, values)) wire layout. Keys may
            // be LowCardinality (but never Nullable); values are unrestricted.
            Expected::Exact(
                "m",
                ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
            ),
            Expected::Exact(
                "m_lc",
                ChType::Map(
                    Box::new(ChType::LowCardinality(Box::new(ChType::String))),
                    Box::new(ChType::UInt8),
                ),
            ),
            Expected::Exact(
                "m_nv",
                ChType::Map(
                    Box::new(ChType::String),
                    Box::new(ChType::Nullable(Box::new(ChType::String))),
                ),
            ),
            Expected::Exact(
                "m_arr",
                ChType::Map(
                    Box::new(ChType::String),
                    Box::new(ChType::Array(Box::new(ChType::Int32))),
                ),
            ),
            Expected::Exact(
                "arr_m",
                ChType::Array(Box::new(ChType::Map(
                    Box::new(ChType::String),
                    Box::new(ChType::Int32),
                ))),
            ),
            Expected::Exact(
                "m_empty",
                ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
            ),
            // Wide integers: the four exact spellings, plus a Nullable and a
            // LowCardinality inner (the wide ints are legal LC inners, unlike
            // Decimal/Enum). No parameters, no aliases.
            Expected::Exact("i128", ChType::Int128),
            Expected::Exact("u128", ChType::UInt128),
            Expected::Exact("i256", ChType::Int256),
            Expected::Exact("u256", ChType::UInt256),
            Expected::Exact("ni128", ChType::Nullable(Box::new(ChType::Int128))),
            Expected::Exact("lc_i256", ChType::LowCardinality(Box::new(ChType::Int256))),
            // Time/Time64 are signed primitive-backed temporals with no timezone.
            // Time is a legal LowCardinality inner; Time64 is not.
            Expected::Exact("t", ChType::Time),
            Expected::Exact("t64", ChType::Time64 { precision: 6 }),
            Expected::Exact("nt", ChType::Nullable(Box::new(ChType::Time))),
            Expected::Exact(
                "nt64",
                ChType::Nullable(Box::new(ChType::Time64 { precision: 6 })),
            ),
            Expected::Exact("lc_time", ChType::LowCardinality(Box::new(ChType::Time))),
            // SimpleAggregateFunction(func, T): a name-decoration alias over the
            // inner type T. The header carries the alias spelling (never the
            // expanded T), so the parsed ChType keeps func plus the inner type,
            // and the decoded buffer IS T's buffer. func preserves any
            // parenthesized literal params verbatim (groupArrayLastArray(5)).
            Expected::Exact(
                "saf_sum",
                ChType::SimpleAggregateFunction {
                    func: "sum".to_string(),
                    inner: Box::new(ChType::Float64),
                },
            ),
            Expected::Exact(
                "saf_lc",
                ChType::SimpleAggregateFunction {
                    func: "anyLast".to_string(),
                    inner: Box::new(ChType::LowCardinality(Box::new(ChType::Nullable(
                        Box::new(ChType::String),
                    )))),
                },
            ),
            Expected::Exact(
                "saf_grp",
                ChType::SimpleAggregateFunction {
                    func: "groupArrayLastArray(5)".to_string(),
                    inner: Box::new(ChType::Array(Box::new(ChType::UInt64))),
                },
            ),
            // Geo aliases: the bare alias spelling reaches the header and the
            // parsed ChType is Geo(kind); the decoded buffer is the underlying
            // Tuple/Array-of-Float64 nesting. Point = Tuple(Float64, Float64),
            // Ring = Array(Point), MultiPolygon = Array(Array(Array(Point))).
            // Nullable(Point) is legal (Point is a Tuple); the array-based kinds
            // are not nullable, so npoint uses Point.
            Expected::Exact("point", ChType::Geo(GeoKind::Point)),
            Expected::Exact(
                "npoint",
                ChType::Nullable(Box::new(ChType::Geo(GeoKind::Point))),
            ),
            Expected::Exact("ring", ChType::Geo(GeoKind::Ring)),
            Expected::Exact("mpoly", ChType::Geo(GeoKind::MultiPolygon)),
            // Nested(x UInt32, y String): the literal Nested spelling reaches the
            // header, and the body is byte-identical to Array(Tuple(named x, y)),
            // so it decodes as an Array of a named two-field Tuple.
            Expected::Exact(
                "nst",
                ChType::Nested(vec![
                    ("x".to_string(), ChType::UInt32),
                    ("y".to_string(), ChType::String),
                ]),
            ),
            // SimpleAggregateFunction inside wrappers. The server emits the alias
            // spelling verbatim inside the wrapper in the Native header (confirmed
            // live at v26.6.1.1193-stable via toTypeName + hexdump), and the
            // decoded buffer IS the physical delegate's. nsaf delegates to
            // Nullable(UInt64); lc_saf delegates to LowCardinality(String).
            Expected::Exact(
                "nsaf",
                ChType::Nullable(Box::new(ChType::SimpleAggregateFunction {
                    func: "sum".to_string(),
                    inner: Box::new(ChType::UInt64),
                })),
            ),
            Expected::Exact(
                "lc_saf",
                ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
                    func: "anyLast".to_string(),
                    inner: Box::new(ChType::String),
                })),
            ),
            // LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String))):
            // the SAF name decoration sits between the LowCardinality and its
            // removeNullable Nullable. The decoder resolves the full SAF chain, so
            // the column is nullable at the index level and its dictionary body is
            // the bare String inner. Confirmed a real server header live at
            // v26.6.1.1193-stable.
            Expected::Exact(
                "lc_nsaf",
                ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
                    func: "anyLast".to_string(),
                    inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
                })),
            ),
            // The 11 Interval* types preserve their exact logical unit over one
            // shared signed Int64 physical body. Nullable and LowCardinality
            // representatives exercise the two legal wrappers.
            Expected::Exact("iy", ChType::Interval(IntervalKind::Year)),
            Expected::Exact("iq", ChType::Interval(IntervalKind::Quarter)),
            Expected::Exact("imo", ChType::Interval(IntervalKind::Month)),
            Expected::Exact("iw", ChType::Interval(IntervalKind::Week)),
            Expected::Exact("id", ChType::Interval(IntervalKind::Day)),
            Expected::Exact("ih", ChType::Interval(IntervalKind::Hour)),
            Expected::Exact("imi", ChType::Interval(IntervalKind::Minute)),
            Expected::Exact("isecond", ChType::Interval(IntervalKind::Second)),
            Expected::Exact("ims", ChType::Interval(IntervalKind::Millisecond)),
            Expected::Exact("ius", ChType::Interval(IntervalKind::Microsecond)),
            Expected::Exact("ins", ChType::Interval(IntervalKind::Nanosecond)),
            Expected::Exact(
                "nid",
                ChType::Nullable(Box::new(ChType::Interval(IntervalKind::Day))),
            ),
            Expected::Exact(
                "lc_ih",
                ChType::LowCardinality(Box::new(ChType::Interval(IntervalKind::Hour))),
            ),
            Expected::Exact(
                "m_id",
                ChType::Map(
                    Box::new(ChType::Interval(IntervalKind::Day)),
                    Box::new(ChType::String),
                ),
            ),
            Expected::Exact("bf", ChType::BFloat16),
            Expected::Exact("nbf", ChType::Nullable(Box::new(ChType::BFloat16))),
            Expected::Exact("lc_bf", ChType::LowCardinality(Box::new(ChType::BFloat16))),
            Expected::Exact("nothing", ChType::Nullable(Box::new(ChType::Nothing))),
            Expected::Exact("arr_nothing", ChType::Array(Box::new(ChType::Nothing))),
            Expected::Exact(
                "agg_count",
                ChType::AggregateFunction {
                    function: "count".to_string(),
                    arguments: vec![ChType::UInt64],
                },
            ),
            Expected::Exact(
                "agg_nothing",
                ChType::AggregateFunction {
                    function: "nothingUInt64".to_string(),
                    arguments: vec![ChType::Nullable(Box::new(ChType::Nothing))],
                },
            ),
            Expected::Exact(
                "agg_sum_u8",
                ChType::AggregateFunction {
                    function: "sum".to_string(),
                    arguments: vec![ChType::UInt8],
                },
            ),
            Expected::Exact(
                "agg_sum_bf",
                ChType::AggregateFunction {
                    function: "sum".to_string(),
                    arguments: vec![ChType::BFloat16],
                },
            ),
            Expected::Exact(
                "agg_sum_d32",
                ChType::AggregateFunction {
                    function: "sum".to_string(),
                    arguments: vec![ChType::Decimal {
                        precision: 9,
                        scale: 2,
                        bits: 32,
                    }],
                },
            ),
            Expected::Exact(
                "agg_sum_u256",
                ChType::AggregateFunction {
                    function: "sum".to_string(),
                    arguments: vec![ChType::UInt256],
                },
            ),
            Expected::Exact(
                "agg_sum_nu8",
                ChType::AggregateFunction {
                    function: "sum".to_string(),
                    arguments: vec![ChType::Nullable(Box::new(ChType::UInt8))],
                },
            ),
            Expected::Exact(
                "agg_sum_e8",
                ChType::AggregateFunction {
                    function: "sum".to_string(),
                    arguments: vec![ChType::Enum8 {
                        variants: vec![
                            ("zero".to_string(), 0),
                            ("one".to_string(), 1),
                            ("two".to_string(), 2),
                        ],
                    }],
                },
            ),
            Expected::Exact(
                "agg_nothing_null",
                ChType::AggregateFunction {
                    function: "nothingNull".to_string(),
                    arguments: vec![ChType::Nullable(Box::new(ChType::Nothing))],
                },
            ),
            Expected::Exact(
                "variant",
                ChType::Variant(vec![ChType::String, ChType::UInt64]),
            ),
            Expected::Exact("dynamic", ChType::Dynamic { max_types: 1 }),
        ],
    );

    let block = &batch.chunks[0];
    assert_eq!(block.num_rows, 4);

    match block.column(0) {
        Column::Int8(c) => assert_eq!(c.values.as_slice(), &[-128, -1, 0, 127]),
        other => panic!("expected Int8, got {other:?}"),
    }
    match block.column(1) {
        Column::Int16(c) => assert_eq!(c.values.as_slice(), &[-32768, -13, 0, 32767]),
        other => panic!("expected Int16, got {other:?}"),
    }
    match block.column(2) {
        Column::Int32(c) => {
            assert_eq!(c.values.as_slice(), &[i32::MIN, -79, 0, i32::MAX]);
        }
        other => panic!("expected Int32, got {other:?}"),
    }
    match block.column(3) {
        Column::Int64(c) => {
            assert_eq!(c.values.as_slice(), &[i64::MIN, -79, 0, i64::MAX]);
        }
        other => panic!("expected Int64, got {other:?}"),
    }
    match block.column(4) {
        Column::UInt8(c) => assert_eq!(c.values.as_slice(), &[0, 13, 79, u8::MAX]),
        other => panic!("expected UInt8, got {other:?}"),
    }
    match block.column(5) {
        Column::UInt16(c) => assert_eq!(c.values.as_slice(), &[0, 13, 79, u16::MAX]),
        other => panic!("expected UInt16, got {other:?}"),
    }
    match block.column(6) {
        Column::UInt32(c) => assert_eq!(c.values.as_slice(), &[0, 13, 79, u32::MAX]),
        other => panic!("expected UInt32, got {other:?}"),
    }
    match block.column(7) {
        Column::UInt64(c) => assert_eq!(c.values.as_slice(), &[0, 13, 79, u64::MAX]),
        other => panic!("expected UInt64, got {other:?}"),
    }
    match block.column(8) {
        Column::Float32(c) => assert_eq!(c.values.as_slice(), &[-1.25, 0.0, 3.5, 79.125]),
        other => panic!("expected Float32, got {other:?}"),
    }
    match block.column(9) {
        Column::Float64(c) => assert_eq!(c.values.as_slice(), &[-1.25, 0.0, 3.5, 79.125]),
        other => panic!("expected Float64, got {other:?}"),
    }
    match block.column(10) {
        Column::Bool(c) => {
            assert!(!c.get(0));
            assert!(c.get(1));
            assert!(!c.get(2));
            assert!(c.get(3));
        }
        other => panic!("expected Bool, got {other:?}"),
    }

    assert_utf8_values(
        block.column(11),
        &[b"" as &[u8], b"user_1", &[0xff, 0x00], b"user_2"],
    );
    assert_fixed_binary_values(
        block.column(12),
        &[
            b"x\0\0\0" as &[u8],
            b"ABCD",
            b"\0\0\0\0",
            &[0xff, 0x00, 0x00, 0x00],
        ],
    );

    match block.column(13) {
        Column::Int32(c) => {
            assert_eq!(c.values.as_slice(), &[-7, 0, -5, 0]);
            assert_eq!(c.null_count(), 2);
        }
        other => panic!("expected nullable Int32 storage, got {other:?}"),
    }
    assert_validity(block.column(13), &[true, false, true, false]);

    assert_utf8_values(block.column(14), &[b"user_0" as &[u8], b"", b"user_2", b""]);
    assert_validity(block.column(14), &[true, false, true, false]);

    // Temporal columns. Raw integers were derived independently from the server
    // (toUInt16/toInt32/toUInt32 of the values, and reinterpretAsInt64 of the
    // DateTime64 ticks), not by decoding with this crate.
    match block.column(15) {
        Column::Date(c) => assert_eq!(c.values.as_slice(), &[0u16, 19737, 49710, 65535]),
        other => panic!("expected Date, got {other:?}"),
    }
    match block.column(16) {
        Column::Date32(c) => assert_eq!(c.values.as_slice(), &[-7227i32, 0, 19737, 84370]),
        other => panic!("expected Date32, got {other:?}"),
    }
    // The bare DateTime and DateTime('UTC') columns carry identical raw seconds;
    // timezone is type metadata only, with no effect on the wire bytes.
    let expected_seconds = &[0u32, 1705322096, 961056000, 4294967295];
    match block.column(17) {
        Column::DateTime(c) => assert_eq!(c.values.as_slice(), expected_seconds),
        other => panic!("expected DateTime, got {other:?}"),
    }
    match block.column(18) {
        Column::DateTime(c) => assert_eq!(c.values.as_slice(), expected_seconds),
        other => panic!("expected DateTime, got {other:?}"),
    }
    // DateTime64(3) ticks are milliseconds since epoch, including a pre-epoch
    // negative tick. The bare and 'UTC' variants share the same raw ticks.
    let expected_ticks = &[-877i64, 0, 1705322096789, 4102444799999];
    match block.column(19) {
        Column::DateTime64(c) => assert_eq!(c.values.as_slice(), expected_ticks),
        other => panic!("expected DateTime64, got {other:?}"),
    }
    match block.column(20) {
        Column::DateTime64(c) => assert_eq!(c.values.as_slice(), expected_ticks),
        other => panic!("expected DateTime64, got {other:?}"),
    }

    // LowCardinality(String): rows resolve to user_1, user_2, user_1, user_3
    // against the per-block dictionary. No nulls.
    assert_dictionary_string_values(
        block.column(21),
        &[
            Some(b"user_1" as &[u8]),
            Some(b"user_2"),
            Some(b"user_1"),
            Some(b"user_3"),
        ],
    );
    // LowCardinality(Nullable(String)): rows 1 and 3 are NULL (wire index 0 maps
    // to the null sentinel), rows 0 and 2 are real values.
    assert_dictionary_string_values(
        block.column(22),
        &[Some(b"user_0" as &[u8]), None, Some(b"user_2"), None],
    );
    assert_validity(block.column(22), &[true, false, true, false]);

    // LowCardinality(UInt32): a primitive dictionary body (raw 4-byte LE), with a
    // repeated value so the dictionary is smaller than the row count. Rows resolve
    // to 13, 79, 13, 4294967295.
    assert_dictionary_u32_values(
        block.column(23),
        &[Some(13), Some(79), Some(13), Some(u32::MAX)],
    );
    // LowCardinality(Date): a UInt16 dictionary body. Rows resolve to the raw days
    // 19737, 49710, 19737, 0.
    assert_dictionary_date_values(
        block.column(24),
        &[Some(19737), Some(49710), Some(19737), Some(0)],
    );
    // LowCardinality(Nullable(UInt32)): rows 1 and 3 NULL via the index-0
    // sentinel, rows 0 and 2 the values 13 and 79.
    assert_dictionary_u32_values(block.column(25), &[Some(13), None, Some(79), None]);
    assert_validity(block.column(25), &[true, false, true, false]);

    // UUID: 16 raw wire bytes per row, a POD dump of the UInt128 (NOT RFC-4122
    // byte order). Row 1 is the documented 00112233-4455-6677-8899-aabbccddeeff,
    // whose exact wire bytes the server emits are asserted below. The decoder
    // does no reordering; the wire->RFC mapping (rfc[i] = wire[7-i] for i in
    // 0..7, rfc[i] = wire[23-i] for i in 8..15) is a binding concern. These
    // wire bytes were probed directly from the server, not produced by this
    // crate.
    let uuid_00112233: [u8; 16] = [
        0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99,
        0x88,
    ];
    let uuid_10203040: [u8; 16] = [
        0x80, 0x70, 0x60, 0x50, 0x40, 0x30, 0x20, 0x10, 0x00, 0xf0, 0xe0, 0xd0, 0xc0, 0xb0, 0xa0,
        0x90,
    ];
    match block.column(26) {
        Column::Uuid(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.len(), 4);
            assert_eq!(c.value(0), [0u8; 16]); // nil UUID
            assert_eq!(c.value(1), uuid_00112233);
            assert_eq!(c.value(2), uuid_10203040);
            assert_eq!(c.value(3), [0xffu8; 16]);
        }
        other => panic!("expected Uuid, got {other:?}"),
    }

    // IPv4: the standard UInt32 numeric value. 192.0.2.235 = 3221226219,
    // 10.20.30.40 = 169090600. Numbers probed from the server.
    match block.column(27) {
        Column::Ipv4(c) => {
            assert_eq!(c.values.as_slice(), &[0, 3221226219, 169090600, u32::MAX]);
        }
        other => panic!("expected Ipv4, got {other:?}"),
    }

    // IPv6: 16 raw bytes in network byte order, verbatim. Bytes probed from the
    // server (the unspecified ::, 2001:db8::68, fe80::1, ::ffff:192.0.2.235).
    let ipv6_db8: [u8; 16] = [
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x68,
    ];
    let ipv6_fe80: [u8; 16] = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01];
    let ipv6_v4mapped: [u8; 16] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xc0, 0x00, 0x02, 0xeb,
    ];
    match block.column(28) {
        Column::Ipv6(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.len(), 4);
            assert_eq!(c.value(0), [0u8; 16]);
            assert_eq!(c.value(1), ipv6_db8);
            assert_eq!(c.value(2), ipv6_fe80);
            assert_eq!(c.value(3), ipv6_v4mapped);
        }
        other => panic!("expected Ipv6, got {other:?}"),
    }

    // LowCardinality(UUID): a 16-byte fixed-binary dictionary body, with a repeat
    // so the per-block dictionary is smaller than the row count. Rows resolve to
    // the wire bytes 00112233..., 10203040..., 00112233..., ffffffff... in wire
    // order (the core never reorders).
    assert_dictionary_uuid_values(
        block.column(29),
        &[
            Some(&uuid_00112233),
            Some(&uuid_10203040),
            Some(&uuid_00112233),
            Some(&[0xffu8; 16]),
        ],
    );

    // Enum8/Enum16: the underlying signed int per row. Rows north, south, west,
    // north map to 1, 2, -1, 1 via the value map (west = -1). The raw ints were
    // probed directly from the server (CAST(enum AS Int8/Int16)), not produced
    // by decoding with this crate.
    match block.column(30) {
        Column::Enum8(c) => {
            assert_eq!(c.values.as_slice(), &[1i8, 2, -1, 1]);
            assert!(c.validity.is_none());
        }
        other => panic!("expected Enum8, got {other:?}"),
    }
    match block.column(31) {
        Column::Enum16(c) => assert_eq!(c.values.as_slice(), &[1i16, 2, -1, 1]),
        other => panic!("expected Enum16, got {other:?}"),
    }

    // Decimal columns: a raw little-endian two's-complement fixed-width integer
    // per row, byte width derived from the precision. The unscaled integers and
    // the wide-width raw byte patterns were probed directly from the server
    // (reinterpretAsInt32/Int64 and hex(reinterpretAsFixedString)), not produced
    // by decoding with this crate.
    //
    // Decimal32(4) -> Decimal(9, 4): 4-byte LE Int32. Unscaled ints from the
    // values 0.0013, -0.0001, 0, 1.2345 are 13, -1, 0, 12345. -1 is all-0xFF
    // bytes of the width.
    match block.column(32) {
        Column::Decimal(c) => {
            assert_eq!(c.width, 4);
            assert_eq!(c.precision, 9);
            assert_eq!(c.scale, 4);
            assert_eq!(c.len(), 4);
            assert_eq!(decimal_le_i32(c, 0), 13);
            assert_eq!(decimal_le_i32(c, 1), -1);
            assert_eq!(c.value(1), &[0xFF, 0xFF, 0xFF, 0xFF]);
            assert_eq!(decimal_le_i32(c, 2), 0);
            assert_eq!(decimal_le_i32(c, 3), 12345);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
    // Decimal64(9) -> Decimal(18, 9): 8-byte LE Int64. Unscaled ints from
    // 0.000000079, -0.000000001, 0, 1.5 are 79, -1, 0, 1500000000.
    match block.column(33) {
        Column::Decimal(c) => {
            assert_eq!(c.width, 8);
            assert_eq!(c.precision, 18);
            assert_eq!(c.scale, 9);
            assert_eq!(decimal_le_i64(c, 0), 79);
            assert_eq!(decimal_le_i64(c, 1), -1);
            assert_eq!(c.value(1), &[0xFF; 8]);
            assert_eq!(decimal_le_i64(c, 2), 0);
            assert_eq!(decimal_le_i64(c, 3), 1_500_000_000);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
    // Decimal128(20) -> Decimal(38, 20): 16-byte LE Int128. The core has no
    // native i128, so assert the raw little-endian byte pattern. Unscaled ints
    // are 1, -1, 0, 79: row 0 is 0x01 then zeros, row 1 is all 0xFF, row 2 is all
    // zero, row 3 is 0x4F (79) then zeros.
    {
        let mut one128 = [0u8; 16];
        one128[0] = 0x01;
        let mut seventy_nine128 = [0u8; 16];
        seventy_nine128[0] = 0x4F;
        match block.column(34) {
            Column::Decimal(c) => {
                assert_eq!(c.width, 16);
                assert_eq!(c.precision, 38);
                assert_eq!(c.scale, 20);
                assert_eq!(c.value(0), one128);
                assert_eq!(c.value(1), [0xFFu8; 16]);
                assert_eq!(c.value(2), [0u8; 16]);
                assert_eq!(c.value(3), seventy_nine128);
            }
            other => panic!("expected Decimal, got {other:?}"),
        }
    }
    // Decimal256(50) -> Decimal(76, 50): 32-byte LE Int256. Assert the raw
    // little-endian byte pattern. Unscaled ints are 1, -1, 0, 258: row 0 is 0x01
    // then zeros, row 1 is all 0xFF, row 2 is all zero, row 3 is 0x02 0x01 (0x0102
    // = 258) then zeros.
    {
        let mut one256 = [0u8; 32];
        one256[0] = 0x01;
        let mut two_fifty_eight256 = [0u8; 32];
        two_fifty_eight256[0] = 0x02;
        two_fifty_eight256[1] = 0x01;
        match block.column(35) {
            Column::Decimal(c) => {
                assert_eq!(c.width, 32);
                assert_eq!(c.precision, 76);
                assert_eq!(c.scale, 50);
                assert_eq!(c.value(0), one256);
                assert_eq!(c.value(1), [0xFFu8; 32]);
                assert_eq!(c.value(2), [0u8; 32]);
                assert_eq!(c.value(3), two_fifty_eight256);
            }
            other => panic!("expected Decimal, got {other:?}"),
        }
    }

    // LowCardinality(IPv4): a UInt32 dictionary body, with a repeat so the
    // per-block dictionary is smaller than the row count. Rows resolve to the
    // standard IPv4 numeric values 3221226219 (192.0.2.235), 169090600
    // (10.20.30.40), 3221226219, 4294967295. Numbers match the plain `ipv4` column.
    assert_dictionary_ipv4_values(
        block.column(36),
        &[
            Some(3221226219),
            Some(169090600),
            Some(3221226219),
            Some(u32::MAX),
        ],
    );

    // LowCardinality(IPv6): a 16-byte fixed-binary dictionary body, network byte
    // order, with a repeat. Rows resolve to the same wire bytes as the plain `ipv6`
    // column: 2001:db8::68, fe80::1, 2001:db8::68, ::ffff:192.0.2.235.
    assert_dictionary_ipv6_values(
        block.column(37),
        &[
            Some(&ipv6_db8),
            Some(&ipv6_fe80),
            Some(&ipv6_db8),
            Some(&ipv6_v4mapped),
        ],
    );

    // Array(Int32): rows [] / [13] / [79, -13] / [1, 2, 3]. The decoded offsets
    // carry Arrow's leading 0 and are i64; the empty row 0 shows as equal adjacent
    // offsets (0, 0). The flattened element column is a plain Int32 buffer.
    {
        let arr = as_array(block.column(38));
        assert_eq!(arr.offsets, vec![0i64, 0, 1, 3, 6]);
        assert_eq!(arr.null_count(), 0); // never nullable at the array level
        match arr.values.as_ref() {
            Column::Int32(c) => {
                assert!(c.validity.is_none());
                assert_eq!(c.values.as_slice(), &[13, 79, -13, 1, 2, 3]);
            }
            other => panic!("expected Int32 array elements, got {other:?}"),
        }
    }

    // Array(String): rows [] / ['user_1'] / ['a', 'user_2'] / ['x']. Variable-length
    // element body after the offsets.
    {
        let arr = as_array(block.column(39));
        assert_eq!(arr.offsets, vec![0i64, 0, 1, 3, 4]);
        match arr.values.as_ref() {
            Column::Utf8(c) => assert_utf8_column(c, &[b"user_1" as &[u8], b"a", b"user_2", b"x"]),
            other => panic!("expected Utf8 array elements, got {other:?}"),
        }
    }

    // Array(Nullable(Int32)): rows [] / [13, NULL] / [NULL] / [79, -1, NULL]. The
    // per-element null map follows the offsets, so element-level nulls live on the
    // element column's validity, never on the array. The null slots decode to the
    // inner default 0 in the buffer.
    {
        let arr = as_array(block.column(40));
        assert_eq!(arr.offsets, vec![0i64, 0, 2, 3, 6]);
        assert_eq!(arr.null_count(), 0);
        match arr.values.as_ref() {
            Column::Int32(c) => {
                assert_eq!(c.values.as_slice(), &[13, 0, 0, 79, -1, 0]);
                assert_eq!(c.null_count(), 3);
                let bm = c.validity.as_ref().expect("nullable element validity");
                let got: Vec<bool> = (0..c.values.len()).map(|i| bm.is_valid(i)).collect();
                assert_eq!(got, vec![true, false, false, true, true, false]);
            }
            other => panic!("expected nullable Int32 array elements, got {other:?}"),
        }
    }

    // Array(LowCardinality(String)): rows [] / ['red', 'red'] / ['green'] /
    // ['red', 'blue']. The element column is a per-block dictionary; resolving each
    // flattened element through it gives red, red, green, red, blue.
    {
        let arr = as_array(block.column(41));
        assert_eq!(arr.offsets, vec![0i64, 0, 2, 3, 5]);
        assert_dictionary_string_values(
            arr.values.as_ref(),
            &[
                Some(b"red" as &[u8]),
                Some(b"red"),
                Some(b"green"),
                Some(b"red"),
                Some(b"blue"),
            ],
        );
    }

    // Array(Array(Int32)): rows [] / [[13]] / [[79, 13], []] / [[1], [2, 3]]. Two
    // offset levels then the leaf. The outer offsets count inner arrays; the inner
    // offsets count leaf ints (with an empty inner array as equal adjacent offsets
    // 3, 3).
    {
        let outer = as_array(block.column(42));
        assert_eq!(outer.offsets, vec![0i64, 0, 1, 3, 5]);
        let inner = as_array(outer.values.as_ref());
        assert_eq!(inner.offsets, vec![0i64, 1, 3, 3, 4, 6]);
        match inner.values.as_ref() {
            Column::Int32(c) => assert_eq!(c.values.as_slice(), &[13, 79, 13, 1, 2, 3]),
            other => panic!("expected Int32 leaf elements, got {other:?}"),
        }
    }

    // Array(LowCardinality(String)) with EVERY row empty: the server writes only
    // the hoisted LC key version and the four all-zero offsets, and NOTHING for
    // the LC element run (`SerializationLowCardinality::
    // serializeBinaryBulkWithMultipleStreams` early-returns at limit == 0), so
    // the element column decodes to an empty dictionary.
    {
        let arr = as_array(block.column(43));
        assert_eq!(arr.offsets, vec![0i64, 0, 0, 0, 0]);
        match arr.values.as_ref() {
            Column::Dictionary(d) => {
                assert!(d.indices.is_empty());
                assert!(d.validity.is_none());
                assert_eq!(d.values.len(), 0);
            }
            other => panic!("expected empty Dictionary elements, got {other:?}"),
        }
    }

    // Tuple(Int32, String): each element's full run in declaration order
    // (column-of-columns). Rows (-13, user_0), (-6, user_1), (1, user_2),
    // (8, user_3).
    {
        let t = as_tuple(block.column(44));
        assert_eq!(t.len, 4);
        assert!(t.validity.is_none());
        match &t.fields[0] {
            Column::Int32(c) => assert_eq!(c.values.as_slice(), &[-13, -6, 1, 8]),
            other => panic!("expected Int32 tuple element, got {other:?}"),
        }
        match &t.fields[1] {
            Column::Utf8(c) => {
                assert_utf8_column(c, &[b"user_0" as &[u8], b"user_1", b"user_2", b"user_3"])
            }
            other => panic!("expected Utf8 tuple element, got {other:?}"),
        }
    }

    // Tuple(a Int32, b Nullable(String)): element b carries its own null map
    // inside its element body; rows 1 and 3 are NULL there. The names are type
    // metadata only.
    {
        let t = as_tuple(block.column(45));
        assert_eq!(t.len, 4);
        match &t.fields[0] {
            Column::Int32(c) => assert_eq!(c.values.as_slice(), &[0, 1, 2, 3]),
            other => panic!("expected Int32 tuple element, got {other:?}"),
        }
        match &t.fields[1] {
            Column::Utf8(c) => {
                assert_eq!(c.null_count(), 2);
                let bm = c.validity.as_ref().expect("element validity");
                let got: Vec<bool> = (0..4).map(|i| bm.is_valid(i)).collect();
                assert_eq!(got, vec![true, false, true, false]);
                assert_eq!(c.value(0), b"user_0");
                assert_eq!(c.value(2), b"user_2");
            }
            other => panic!("expected Utf8 tuple element, got {other:?}"),
        }
    }

    // Array(Tuple(Int32, Int32)): rows [] / [(13, 79)] / [(1, 2), (3, 4)] /
    // [(-1, -2)]. The flattened tuple column holds 4 rows; the offsets carry
    // Arrow's leading 0.
    {
        let arr = as_array(block.column(46));
        assert_eq!(arr.offsets, vec![0i64, 0, 1, 3, 4]);
        let t = as_tuple(arr.values.as_ref());
        assert_eq!(t.len, 4);
        match (&t.fields[0], &t.fields[1]) {
            (Column::Int32(a), Column::Int32(b)) => {
                assert_eq!(a.values.as_slice(), &[13, 1, 3, -1]);
                assert_eq!(b.values.as_slice(), &[79, 2, 4, -2]);
            }
            other => panic!("expected Int32 tuple elements, got {other:?}"),
        }
    }

    // Nullable(Tuple(Int32, String)): the tuple-level null map precedes the
    // tuple body; rows 1 and 3 are NULL and carry element defaults (0, '') in
    // the element bodies.
    {
        let t = as_tuple(block.column(47));
        assert_eq!(t.len, 4);
        assert_eq!(t.null_count(), 2);
        let bm = t.validity.as_ref().expect("tuple-level validity");
        let got: Vec<bool> = (0..4).map(|i| bm.is_valid(i)).collect();
        assert_eq!(got, vec![true, false, true, false]);
        match &t.fields[0] {
            Column::Int32(c) => assert_eq!(c.values.as_slice(), &[0, 0, 2, 0]),
            other => panic!("expected Int32 tuple element, got {other:?}"),
        }
        match &t.fields[1] {
            Column::Utf8(c) => {
                assert_eq!(c.value(0), b"user_0");
                assert_eq!(c.value(1), b"");
                assert_eq!(c.value(2), b"user_2");
                assert_eq!(c.value(3), b"");
            }
            other => panic!("expected Utf8 tuple element, got {other:?}"),
        }
    }

    // Map(String, Int32): rows {} / {a: 13} / {a: 1, b: 2} / {k: -7}. The
    // offsets carry Arrow's leading 0; the entries are the flattened keys run
    // then the flattened values run.
    {
        let m = as_map(block.column(48));
        assert_eq!(m.offsets, vec![0i64, 0, 1, 3, 4]);
        let (keys, values) = map_entries(m);
        match (keys, values) {
            (Column::Utf8(k), Column::Int32(v)) => {
                assert_utf8_column(k, &[b"a" as &[u8], b"a", b"b", b"k"]);
                assert_eq!(v.values.as_slice(), &[13, 1, 2, -7]);
            }
            other => panic!("expected (Utf8, Int32) entries, got {other:?}"),
        }
    }

    // Map(LowCardinality(String), UInt8): rows {red: 1} / {} / {red: 2,
    // blue: 3} / {green: 4}. The LC key version is hoisted ahead of the
    // offsets; the flattened keys resolve through the per-block dictionary.
    {
        let m = as_map(block.column(49));
        assert_eq!(m.offsets, vec![0i64, 1, 1, 3, 4]);
        let (keys, values) = map_entries(m);
        assert_dictionary_string_values(
            keys,
            &[
                Some(b"red" as &[u8]),
                Some(b"red"),
                Some(b"blue"),
                Some(b"green"),
            ],
        );
        match values {
            Column::UInt8(v) => assert_eq!(v.values.as_slice(), &[1, 2, 3, 4]),
            other => panic!("expected UInt8 values, got {other:?}"),
        }
    }

    // Map(String, Nullable(String)): rows {a: user_1} / {b: NULL} / {} /
    // {c: user_2, d: NULL}. The flattened value run carries its own null map.
    {
        let m = as_map(block.column(50));
        assert_eq!(m.offsets, vec![0i64, 1, 2, 2, 4]);
        let (keys, values) = map_entries(m);
        match keys {
            Column::Utf8(k) => assert_utf8_column(k, &[b"a" as &[u8], b"b", b"c", b"d"]),
            other => panic!("expected Utf8 keys, got {other:?}"),
        }
        match values {
            Column::Utf8(v) => {
                assert_eq!(v.null_count(), 2);
                let bm = v.validity.as_ref().expect("value validity");
                let got: Vec<bool> = (0..4).map(|i| bm.is_valid(i)).collect();
                assert_eq!(got, vec![true, false, true, false]);
                assert_eq!(v.value(0), b"user_1");
                assert_eq!(v.value(2), b"user_2");
            }
            other => panic!("expected Utf8 values, got {other:?}"),
        }
    }

    // Map(String, Array(Int32)): rows {a: [13]} / {} / {b: [], c: [1, 2]} /
    // {d: [79]}. The flattened value run is itself an Array column over the
    // entries.
    {
        let m = as_map(block.column(51));
        assert_eq!(m.offsets, vec![0i64, 1, 1, 3, 4]);
        let (keys, values) = map_entries(m);
        match keys {
            Column::Utf8(k) => assert_utf8_column(k, &[b"a" as &[u8], b"b", b"c", b"d"]),
            other => panic!("expected Utf8 keys, got {other:?}"),
        }
        match values {
            Column::Array(arr) => {
                assert_eq!(arr.offsets, vec![0i64, 1, 1, 3, 4]);
                match arr.values.as_ref() {
                    Column::Int32(c) => assert_eq!(c.values.as_slice(), &[13, 1, 2, 79]),
                    other => panic!("expected Int32 leaf, got {other:?}"),
                }
            }
            other => panic!("expected Array values, got {other:?}"),
        }
    }

    // Array(Map(String, Int32)): rows [] / [{a: 1}] / [{b: 2}, {}] / [{c: 3}].
    {
        let arr = as_array(block.column(52));
        assert_eq!(arr.offsets, vec![0i64, 0, 1, 3, 4]);
        let m = as_map(arr.values.as_ref());
        assert_eq!(m.offsets, vec![0i64, 1, 2, 2, 3]);
        let (keys, values) = map_entries(m);
        match (keys, values) {
            (Column::Utf8(k), Column::Int32(v)) => {
                assert_utf8_column(k, &[b"a" as &[u8], b"b", b"c"]);
                assert_eq!(v.values.as_slice(), &[1, 2, 3]);
            }
            other => panic!("expected (Utf8, Int32) entries, got {other:?}"),
        }
    }

    // Map(String, Int32) with EVERY row empty: the server writes the all-zero
    // offsets and NOTHING for the key/value runs.
    {
        let m = as_map(block.column(53));
        assert_eq!(m.offsets, vec![0i64, 0, 0, 0, 0]);
        let (keys, values) = map_entries(m);
        assert_eq!(keys.len(), 0);
        assert_eq!(values.len(), 0);
    }

    // Wide integers: raw little-endian fixed-width byte patterns, verbatim
    // passthrough (no host byteswap, no native i128/i256). `w16`/`w32` build a
    // little-endian buffer whose only nonzero byte is the least-significant.
    let w16 = |low: u8| {
        let mut b = [0u8; 16];
        b[0] = low;
        b
    };
    let w32 = |low: u8| {
        let mut b = [0u8; 32];
        b[0] = low;
        b
    };
    // i128 (col 54): -1 (all 0xFF), 0, 79, i128::MAX (0xFF.. then 0x7F MSB).
    match block.column(54) {
        Column::Int128(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.value(0), [0xFFu8; 16]);
            assert_eq!(c.value(1), [0u8; 16]);
            assert_eq!(c.value(2), w16(79));
            let mut i128_max = [0xFFu8; 16];
            i128_max[15] = 0x7F;
            assert_eq!(c.value(3), i128_max);
        }
        other => panic!("expected Int128, got {other:?}"),
    }
    // u128 (col 55): 0, 13, 2^127 (high bit set, still positive), u128::MAX.
    match block.column(55) {
        Column::UInt128(c) => {
            assert_eq!(c.value(0), [0u8; 16]);
            assert_eq!(c.value(1), w16(13));
            let mut two_pow_127 = [0u8; 16];
            two_pow_127[15] = 0x80;
            assert_eq!(c.value(2), two_pow_127);
            assert_eq!(c.value(3), [0xFFu8; 16]);
        }
        other => panic!("expected UInt128, got {other:?}"),
    }
    // i256 (col 56): -1, 0, 79, i256::MAX.
    match block.column(56) {
        Column::Int256(c) => {
            assert_eq!(c.width, 32);
            assert_eq!(c.value(0), [0xFFu8; 32]);
            assert_eq!(c.value(1), [0u8; 32]);
            assert_eq!(c.value(2), w32(79));
            let mut i256_max = [0xFFu8; 32];
            i256_max[31] = 0x7F;
            assert_eq!(c.value(3), i256_max);
        }
        other => panic!("expected Int256, got {other:?}"),
    }
    // u256 (col 57): 0, 13, 2^255 (high bit set), u256::MAX.
    match block.column(57) {
        Column::UInt256(c) => {
            assert_eq!(c.value(0), [0u8; 32]);
            assert_eq!(c.value(1), w32(13));
            let mut two_pow_255 = [0u8; 32];
            two_pow_255[31] = 0x80;
            assert_eq!(c.value(2), two_pow_255);
            assert_eq!(c.value(3), [0xFFu8; 32]);
        }
        other => panic!("expected UInt256, got {other:?}"),
    }
    // ni128 (col 58): Nullable(Int128), rows 13, NULL, -1, NULL. Null rows carry
    // the server's placeholder; validity marks rows 1 and 3 null.
    match block.column(58) {
        Column::Int128(c) => {
            assert_eq!(c.value(0), w16(13));
            assert_eq!(c.value(2), [0xFFu8; 16]);
        }
        other => panic!("expected Int128, got {other:?}"),
    }
    assert_validity(block.column(58), &[true, false, true, false]);
    // lc_i256 (col 59): LowCardinality(Int256), row values 13, 79, 13, 258 via a
    // block-local dictionary. Resolve each row's index into the Int256 values.
    match block.column(59) {
        Column::Dictionary(dict) => match dict.values.as_ref() {
            Column::Int256(vals) => {
                let resolved = |row: usize| vals.value(dict.indices[row] as usize).to_vec();
                let mut two_fifty_eight = [0u8; 32];
                two_fifty_eight[0] = 0x02; // 258 = 0x0102, little-endian
                two_fifty_eight[1] = 0x01;
                assert_eq!(resolved(0), w32(13));
                assert_eq!(resolved(1), w32(79));
                assert_eq!(resolved(2), w32(13));
                assert_eq!(resolved(3), two_fifty_eight.to_vec());
            }
            other => panic!("expected Int256 dictionary values, got {other:?}"),
        },
        other => panic!("expected Dictionary, got {other:?}"),
    }

    // Time (col 60): signed Int32 seconds, including the documented text
    // extrema. Time64(6) (col 61): signed Int64 microsecond ticks. Neither has
    // an epoch or timezone, and the wire carries no metadata beyond the type
    // string.
    match block.column(60) {
        Column::Time(c) => {
            assert_eq!(c.values.as_slice(), &[-3_599_999i32, -3_600, 13, 3_599_999]);
        }
        other => panic!("expected Time, got {other:?}"),
    }
    match block.column(61) {
        Column::Time64(c) => assert_eq!(
            c.values.as_slice(),
            &[-3_599_999_999_999i64, -1, 13_000_079, 3_599_999_999_999]
        ),
        other => panic!("expected Time64, got {other:?}"),
    }

    // Nullable Time/Time64 (cols 62/63): valid, NULL, valid, NULL. The null-row
    // primitive placeholders are deliberately not asserted.
    match block.column(62) {
        Column::Time(c) => {
            assert_eq!(c.values[0], -13);
            assert_eq!(c.values[2], 79);
        }
        other => panic!("expected nullable Time storage, got {other:?}"),
    }
    assert_validity(block.column(62), &[true, false, true, false]);
    match block.column(63) {
        Column::Time64(c) => {
            assert_eq!(c.values[0], -13);
            assert_eq!(c.values[2], 79);
        }
        other => panic!("expected nullable Time64 storage, got {other:?}"),
    }
    assert_validity(block.column(63), &[true, false, true, false]);

    // LowCardinality(Time) (col 64): rows -13, 79, -13, 258 resolved through
    // the block-local dictionary.
    match block.column(64) {
        Column::Dictionary(dict) => match dict.values.as_ref() {
            Column::Time(values) => {
                let resolved: Vec<i32> = dict
                    .indices
                    .iter()
                    .map(|&index| values.values[index as usize])
                    .collect();
                assert_eq!(resolved, vec![-13, 79, -13, 258]);
            }
            other => panic!("expected Time dictionary values, got {other:?}"),
        },
        other => panic!("expected Dictionary, got {other:?}"),
    }

    // SimpleAggregateFunction(sum, Float64) (col 65): a name-decoration alias
    // whose body is byte-identical to the inner Float64, so it decodes to a plain
    // Float64 buffer with no extra framing. Values -1.25, 0, 13, 79.125.
    match block.column(65) {
        Column::Float64(c) => assert_eq!(c.values.as_slice(), &[-1.25, 0.0, 13.0, 79.125]),
        other => panic!("expected Float64 (SimpleAggregateFunction delegate), got {other:?}"),
    }
    // SimpleAggregateFunction(anyLast, LowCardinality(Nullable(String))) (col 66):
    // delegates to the LC inner, so it decodes to a Dictionary exactly like a bare
    // LowCardinality(Nullable(String)). Rows resolve to user_1, NULL, user_2, NULL.
    assert_dictionary_string_values(
        block.column(66),
        &[Some(b"user_1" as &[u8]), None, Some(b"user_2"), None],
    );
    assert_validity(block.column(66), &[true, false, true, false]);
    // SimpleAggregateFunction(groupArrayLastArray(5), Array(UInt64)) (col 67):
    // delegates to Array(UInt64). The parametrized function name is metadata only;
    // the body is a plain array. Rows [] / [13] / [79, 13] / [1, 2, 3].
    {
        let arr = as_array(block.column(67));
        assert_eq!(arr.offsets, vec![0i64, 0, 1, 3, 6]);
        assert_eq!(arr.null_count(), 0);
        match arr.values.as_ref() {
            Column::UInt64(c) => assert_eq!(c.values.as_slice(), &[13, 79, 13, 1, 2, 3]),
            other => panic!("expected UInt64 array elements, got {other:?}"),
        }
    }

    // Point (col 68): Geo alias over Tuple(Float64, Float64) (unnamed), so it
    // decodes to a two-field Float64 Tuple with no tuple-level validity. field 0
    // is x, field 1 is y. Rows (13, 79), (-1.5, 2.5), (0, 0), (79.125, -13.25).
    {
        let t = as_tuple(block.column(68));
        assert_eq!(t.len, 4);
        assert!(t.validity.is_none());
        match (&t.fields[0], &t.fields[1]) {
            (Column::Float64(x), Column::Float64(y)) => {
                assert_eq!(x.values.as_slice(), &[13.0, -1.5, 0.0, 79.125]);
                assert_eq!(y.values.as_slice(), &[79.0, 2.5, 0.0, -13.25]);
            }
            other => panic!("expected (Float64, Float64) Point elements, got {other:?}"),
        }
    }
    // Nullable(Point) (col 69): Point is a Tuple, so Nullable(Point) decodes to a
    // Tuple with a tuple-level null map; null rows carry element placeholders that
    // are deliberately not asserted. Valid rows 0 and 2 are (13, 79), (1.25, -2.5).
    {
        let t = as_tuple(block.column(69));
        assert_eq!(t.len, 4);
        assert_eq!(t.null_count(), 2);
        let bm = t
            .validity
            .as_ref()
            .expect("Nullable(Point) tuple-level validity");
        let got: Vec<bool> = (0..4).map(|i| bm.is_valid(i)).collect();
        assert_eq!(got, vec![true, false, true, false]);
        match (&t.fields[0], &t.fields[1]) {
            (Column::Float64(x), Column::Float64(y)) => {
                assert_eq!(x.values[0], 13.0);
                assert_eq!(y.values[0], 79.0);
                assert_eq!(x.values[2], 1.25);
                assert_eq!(y.values[2], -2.5);
            }
            other => panic!("expected (Float64, Float64) Point elements, got {other:?}"),
        }
    }
    // Ring (col 70): Geo alias over Array(Point) = Array(Tuple(Float64, Float64)),
    // so it decodes to an Array of a two-field Float64 Tuple. Rows [] / [(13, 79)] /
    // [(1, 2), (3, 4)] / [(-1.5, -2.5)] -> offsets [0, 0, 1, 3, 4].
    {
        let arr = as_array(block.column(70));
        assert_eq!(arr.offsets, vec![0i64, 0, 1, 3, 4]);
        let t = as_tuple(arr.values.as_ref());
        assert_eq!(t.len, 4);
        match (&t.fields[0], &t.fields[1]) {
            (Column::Float64(x), Column::Float64(y)) => {
                assert_eq!(x.values.as_slice(), &[13.0, 1.0, 3.0, -1.5]);
                assert_eq!(y.values.as_slice(), &[79.0, 2.0, 4.0, -2.5]);
            }
            other => panic!("expected (Float64, Float64) Ring point elements, got {other:?}"),
        }
    }
    // MultiPolygon (col 71): Geo alias over Array(Array(Array(Point))), three
    // Array levels above the leaf Point tuple. Rows [] / [[[(13, 79)]]] /
    // [[[(1, 2), (3, 4)], [(5, 6)]]] / [[[(-1, -2)]], [[(7, 8)]]].
    {
        let outer = as_array(block.column(71));
        assert_eq!(outer.offsets, vec![0i64, 0, 1, 2, 4]);
        let mid = as_array(outer.values.as_ref());
        assert_eq!(mid.offsets, vec![0i64, 1, 3, 4, 5]);
        let inner = as_array(mid.values.as_ref());
        assert_eq!(inner.offsets, vec![0i64, 1, 3, 4, 5, 6]);
        let t = as_tuple(inner.values.as_ref());
        assert_eq!(t.len, 6);
        match (&t.fields[0], &t.fields[1]) {
            (Column::Float64(x), Column::Float64(y)) => {
                assert_eq!(x.values.as_slice(), &[13.0, 1.0, 3.0, 5.0, -1.0, 7.0]);
                assert_eq!(y.values.as_slice(), &[79.0, 2.0, 4.0, 6.0, -2.0, 8.0]);
            }
            other => panic!("expected (Float64, Float64) MultiPolygon leaf, got {other:?}"),
        }
    }
    // Nested(x UInt32, y String) (col 72): the body is byte-identical to
    // Array(Tuple(named x, y)), so it decodes as an Array of a two-field Tuple.
    // The field names live in the ChType (asserted above); the Column stores the
    // element buffers by position. Rows [] / [(13, user_1)] / [(79, a), (1, user_2)]
    // / [(2, x)] -> offsets [0, 0, 1, 3, 4].
    {
        let arr = as_array(block.column(72));
        assert_eq!(arr.offsets, vec![0i64, 0, 1, 3, 4]);
        let t = as_tuple(arr.values.as_ref());
        assert_eq!(t.len, 4);
        match &t.fields[0] {
            Column::UInt32(c) => assert_eq!(c.values.as_slice(), &[13, 79, 1, 2]),
            other => panic!("expected UInt32 Nested field, got {other:?}"),
        }
        match &t.fields[1] {
            Column::Utf8(c) => assert_utf8_column(c, &[b"user_1" as &[u8], b"a", b"user_2", b"x"]),
            other => panic!("expected Utf8 Nested field, got {other:?}"),
        }
    }

    // Nullable(SimpleAggregateFunction(sum, UInt64)) (col 73): the alias is name
    // decoration inside the Nullable, so the decoded buffer is a plain
    // Nullable(UInt64): the null map then the UInt64 run. Rows 1 and 3 NULL,
    // rows 0 and 2 real (13, 79); null rows carry the server's placeholder 0.
    match block.column(73) {
        Column::UInt64(c) => {
            assert_eq!(c.values.as_slice(), &[13, 0, 79, 0]);
            assert_eq!(c.null_count(), 2);
        }
        other => panic!("expected UInt64 SAF delegate, got {other:?}"),
    }
    assert_validity(block.column(73), &[true, false, true, false]);

    // LowCardinality(SimpleAggregateFunction(anyLast, String)) (col 74): the LC
    // body delegates to the physical String inner. Rows user_1, user_2, user_1,
    // user_3 resolve through the per-block dictionary (three distinct entries).
    assert_dictionary_string_values(
        block.column(74),
        &[
            Some(b"user_1" as &[u8]),
            Some(b"user_2"),
            Some(b"user_1"),
            Some(b"user_3"),
        ],
    );

    // LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String))) (col 75):
    // the SAF name decoration sits between the LowCardinality and its
    // removeNullable Nullable. The decoder resolves the full SAF chain, so this
    // decodes exactly like LowCardinality(Nullable(String)): a nullable dictionary
    // whose index 0 is the NULL sentinel and whose body is the bare String inner.
    // Rows resolve to user_1, NULL, user_2, NULL.
    assert_dictionary_string_values(
        block.column(75),
        &[Some(b"user_1" as &[u8]), None, Some(b"user_2"), None],
    );
    assert_validity(block.column(75), &[true, false, true, false]);

    // Interval* (cols 76..86): all kinds carry the same raw signed Int64 count
    // shape. The exact logical kinds are pinned in the schema assertions above.
    for col in 76..=86 {
        match block.column(col) {
            Column::Interval(c) => assert_eq!(c.values.as_slice(), &[-13, 0, 79, 258]),
            other => panic!("expected Interval at column {col}, got {other:?}"),
        }
    }

    // Nullable(IntervalDay) (col 87): valid, NULL, valid, NULL. Placeholder
    // values for NULL rows are not semantically meaningful.
    match block.column(87) {
        Column::Interval(c) => {
            assert_eq!(c.values[0], 13);
            assert_eq!(c.values[2], 79);
            assert_eq!(c.null_count(), 2);
        }
        other => panic!("expected nullable Interval, got {other:?}"),
    }
    assert_validity(block.column(87), &[true, false, true, false]);

    // LowCardinality(IntervalHour) (col 88): rows 13, 79, 13, 258 resolve
    // through an Interval-valued per-block dictionary.
    match block.column(88) {
        Column::Dictionary(d) => {
            let values = match d.values.as_ref() {
                Column::Interval(values) => values,
                other => panic!("expected Interval dictionary values, got {other:?}"),
            };
            let resolved: Vec<i64> = d
                .indices
                .iter()
                .map(|index| values.values[*index as usize])
                .collect();
            assert_eq!(resolved, vec![13, 79, 13, 258]);
        }
        other => panic!("expected Interval dictionary, got {other:?}"),
    }

    // Map(IntervalDay, String) (col 89): rows {} / {13: user_1} /
    // {-79: a, 13: user_2} / {258: x}. The flattened key run must retain the
    // Interval physical tag rather than collapsing to a plain Int64 column.
    {
        let m = as_map(block.column(89));
        assert_eq!(m.offsets, vec![0i64, 0, 1, 3, 4]);
        let (keys, values) = map_entries(m);
        match (keys, values) {
            (Column::Interval(k), Column::Utf8(v)) => {
                assert_eq!(k.values.as_slice(), &[13, -79, 13, 258]);
                assert_utf8_column(v, &[b"user_1" as &[u8], b"a", b"user_2", b"x"]);
            }
            other => panic!("expected (Interval, Utf8) entries, got {other:?}"),
        }
    }

    // BFloat16 (col 90): raw two-byte words, kept verbatim rather than widened
    // to Float32. These correspond to -1.25, 0, 3.5, and 79.
    assert_bfloat16_bits(block.column(90), &[0xbfa0, 0x0000, 0x4060, 0x429e]);

    // Nullable(BFloat16) (col 91): rows 13, NULL, 79, NULL. The nested bits on
    // null rows are placeholders; validity is the logical contract.
    match block.column(91) {
        Column::BFloat16(c) => {
            assert_eq!(u16::from_le_bytes(c.values[0]), 0x4150);
            assert_eq!(u16::from_le_bytes(c.values[2]), 0x429e);
            assert_eq!(c.null_count(), 2);
        }
        other => panic!("expected nullable BFloat16, got {other:?}"),
    }
    assert_validity(block.column(91), &[true, false, true, false]);

    // LowCardinality(BFloat16) (col 92): resolve the per-block dictionary and
    // compare the exact raw words for 13, 79, 13, and 258.
    match block.column(92) {
        Column::Dictionary(d) => {
            let values = match d.values.as_ref() {
                Column::BFloat16(values) => values,
                other => panic!("expected BFloat16 dictionary values, got {other:?}"),
            };
            let resolved: Vec<u16> = d
                .indices
                .iter()
                .map(|index| u16::from_le_bytes(values.values[*index as usize]))
                .collect();
            assert_eq!(resolved, vec![0x4150, 0x429e, 0x4150, 0x4381]);
        }
        other => panic!("expected BFloat16 dictionary, got {other:?}"),
    }

    // Nullable(Nothing) (col 93): every row is NULL. The physical column keeps
    // only its logical length and the decoded ClickHouse null map; the nested
    // ASCII '0' placeholder bytes carry no value.
    match block.column(93) {
        Column::Nothing(c) => {
            assert_eq!(c.len(), 4);
            assert_eq!(c.null_count(), 4);
        }
        other => panic!("expected Nothing, got {other:?}"),
    }
    assert_validity(block.column(93), &[false, false, false, false]);

    // Array(Nothing) (col 94): Nothing cannot supply a real element, so every
    // array is empty and the flattened child has length zero.
    match block.column(94) {
        Column::Array(c) => {
            assert_eq!(c.offsets, vec![0i64, 0, 0, 0, 0]);
            match c.values.as_ref() {
                Column::Nothing(values) => assert_eq!(values.len(), 0),
                other => panic!("expected Nothing array values, got {other:?}"),
            }
        }
        other => panic!("expected Array(Nothing), got {other:?}"),
    }

    // AggregateFunction(count, UInt64) (col 95): one raw VarUInt64 state per
    // row, produced by arrayReduce over arrays of lengths 0, 1, 2, and 3. The
    // state bytes are exposed as Arrow LargeBinary offsets plus data.
    match block.column(95) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0i64, 1, 2, 3, 4]);
            assert_eq!(c.data, vec![0x00, 0x01, 0x02, 0x03]);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }

    // AggregateFunction(nothingUInt64, Nullable(Nothing)) (col 96): the canonical
    // name for the count(Nullable(Nothing)) collapse, one 0x00 byte per row. The
    // fixed-width state exports as LargeBinary offsets i -> i over an all-zero
    // data run.
    match block.column(96) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0i64, 1, 2, 3, 4]);
            assert_eq!(c.data, vec![0x00, 0x00, 0x00, 0x00]);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }

    let sum_values = [0u64, 0, 1, 3];

    // AggregateFunction(sum, UInt8) (col 97): UInt8 promotes to one UInt64
    // accumulator per row, serialized as 8 little-endian bytes.
    match block.column(97) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0i64, 8, 16, 24, 32]);
            let expected: Vec<u8> = sum_values.into_iter().flat_map(u64::to_le_bytes).collect();
            assert_eq!(c.data, expected);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }

    // AggregateFunction(sum, BFloat16) (col 98): BFloat16 promotes to Float64.
    match block.column(98) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0i64, 8, 16, 24, 32]);
            let expected: Vec<u8> = [0.0f64, 0.0, 1.0, 3.0]
                .into_iter()
                .flat_map(f64::to_le_bytes)
                .collect();
            assert_eq!(c.data, expected);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }

    // AggregateFunction(sum, Decimal(9, 2)) (col 99): Decimal32 promotes to a
    // 16-byte Decimal128 state containing the raw scaled integer.
    match block.column(99) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0i64, 16, 32, 48, 64]);
            let expected: Vec<u8> = [0i128, 0, 100, 300]
                .into_iter()
                .flat_map(i128::to_le_bytes)
                .collect();
            assert_eq!(c.data, expected);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }

    // AggregateFunction(sum, UInt256) (col 100): the accumulator stays 32 bytes.
    match block.column(100) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0i64, 32, 64, 96, 128]);
            let expected: Vec<u8> = sum_values
                .into_iter()
                .flat_map(|value| {
                    let mut bytes = [0u8; 32];
                    bytes[..8].copy_from_slice(&value.to_le_bytes());
                    bytes
                })
                .collect();
            assert_eq!(c.data, expected);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }

    // AggregateFunction(sum, Nullable(UInt8)) (col 101): empty and all-null
    // inputs are one false flag byte each. Present states are one true flag plus
    // the UInt64 accumulator, here 13 and 92.
    match block.column(101) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0i64, 1, 2, 11, 20]);
            let mut expected = vec![0x00, 0x00, 0x01];
            expected.extend_from_slice(&13u64.to_le_bytes());
            expected.push(0x01);
            expected.extend_from_slice(&92u64.to_le_bytes());
            assert_eq!(c.data, expected);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }

    // AggregateFunction(sum, Enum8(...)) (col 102): Enum8 promotes to Int64.
    match block.column(102) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0i64, 8, 16, 24, 32]);
            let expected: Vec<u8> = [0i64, 0, 1, 3]
                .into_iter()
                .flat_map(i64::to_le_bytes)
                .collect();
            assert_eq!(c.data, expected);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }

    // AggregateFunction(nothingNull, Nullable(Nothing)) (col 103): the canonical
    // name for sum over an only-null argument. Each opaque state is one strict
    // 0x00 byte; the function's Null suffix does not create Arrow nullability.
    match block.column(103) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0i64, 1, 2, 3, 4]);
            assert_eq!(c.data, vec![0x00, 0x00, 0x00, 0x00]);
            assert_eq!(c.null_count(), 0);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }

    // Variant(String, UInt64) (col 104): the server canonicalizes alternatives
    // by type name, writes BASIC mode 0, then discriminators [NULL, String,
    // UInt64, String] and dense child bodies. The Arrow-shaped routing buffers
    // preserve that selection without row materialization.
    match block.column(104) {
        Column::Variant(c) => {
            assert_eq!(c.len(), 4);
            assert_eq!(c.null_count(), 1);
            assert_eq!(c.value_position(0), Some((u8::MAX, 0)));
            assert_eq!(c.value_position(1), Some((0, 0)));
            assert_eq!(c.value_position(2), Some((1, 0)));
            assert_eq!(c.value_position(3), Some((0, 1)));
            match (&c.variants[0], &c.variants[1]) {
                (Column::Utf8(strings), Column::UInt64(integers)) => {
                    assert_utf8_column(strings, &[b"user_1" as &[u8], b"user_2"]);
                    assert_eq!(integers.values, vec![13]);
                }
                other => panic!("expected (Utf8, UInt64) Variant children, got {other:?}"),
            }
        }
        other => panic!("expected Variant, got {other:?}"),
    }

    // Dynamic(max_types=1) (col 105): repeated String is the one direct type;
    // UInt64 and Array(Int32) overflow into SharedVariant. Shared cells retain
    // the exact binary type descriptor plus one serializeBinary value payload.
    match block.column(105) {
        Column::Dynamic(c) => {
            assert_eq!(c.type_ids, vec![1, 1, 0, 0]);
            assert_eq!(c.offsets, vec![0, 1, 0, 1]);
            assert_eq!(c.null_count(), 0);
            assert_eq!(c.children.len(), 2);
            match (&c.children[0], &c.children[1]) {
                (DynamicChild::Shared(shared), DynamicChild::Typed { ch_type, values }) => {
                    assert_eq!(ch_type, &ChType::String);
                    assert_utf8_values(values, &[b"user_1", b"user_2"]);

                    let mut uint64_blob = vec![0x04];
                    uint64_blob.extend_from_slice(&13u64.to_le_bytes());
                    let mut array_blob = vec![0x1e, 0x09, 0x02];
                    array_blob.extend_from_slice(&79i32.to_le_bytes());
                    array_blob.extend_from_slice(&(-13i32).to_le_bytes());
                    assert_eq!(shared.value(0), uint64_blob);
                    assert_eq!(shared.value(1), array_blob);
                }
                other => panic!("expected (SharedVariant, String) Dynamic children, got {other:?}"),
            }
        }
        other => panic!("expected Dynamic, got {other:?}"),
    }
}

fn assert_bfloat16_bits(column: &Column, expected: &[u16]) {
    match column {
        Column::BFloat16(c) => {
            let actual: Vec<u16> = c.values.iter().copied().map(u16::from_le_bytes).collect();
            assert_eq!(actual, expected);
        }
        other => panic!("expected BFloat16, got {other:?}"),
    }
}

/// Borrow the inner `MapColumn` of a decoded `Map` column, panicking with a
/// useful message on any other variant.
fn as_map(column: &Column) -> &ch_core_rs::column::MapColumn {
    match column {
        Column::Map(m) => m,
        other => panic!("expected Map column, got {other:?}"),
    }
}

/// Borrow a `MapColumn`'s keys and values columns out of its two-field entries
/// tuple.
fn map_entries(map: &ch_core_rs::column::MapColumn) -> (&Column, &Column) {
    let t = as_tuple(map.entries.as_ref());
    assert_eq!(t.fields.len(), 2, "entries must be the (keys, values) pair");
    (&t.fields[0], &t.fields[1])
}

/// Borrow the inner `TupleColumn` of a decoded `Tuple` column, panicking with a
/// useful message on any other variant.
fn as_tuple(column: &Column) -> &ch_core_rs::column::TupleColumn {
    match column {
        Column::Tuple(t) => t,
        other => panic!("expected Tuple column, got {other:?}"),
    }
}

/// Borrow the inner `ArrayColumn` of a decoded `Array` column, panicking with a
/// useful message on any other variant.
fn as_array(column: &Column) -> &ch_core_rs::column::ArrayColumn {
    match column {
        Column::Array(a) => a,
        other => panic!("expected Array, got {other:?}"),
    }
}

/// Read row `index` of a 4-byte (Decimal32-backed) decimal column as the raw
/// little-endian unscaled `i32`.
fn decimal_le_i32(c: &ch_core_rs::column::DecimalColumn, index: usize) -> i32 {
    i32::from_le_bytes(c.value(index).try_into().unwrap())
}

/// Read row `index` of an 8-byte (Decimal64-backed) decimal column as the raw
/// little-endian unscaled `i64`.
fn decimal_le_i64(c: &ch_core_rs::column::DecimalColumn, index: usize) -> i64 {
    i64::from_le_bytes(c.value(index).try_into().unwrap())
}

/// Assert the per-row resolved UUID (16-byte wire) values of a dictionary column.
fn assert_dictionary_uuid_values(column: &Column, expected: &[Option<&[u8; 16]>]) {
    match column {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), expected.len());
            let values = match d.values.as_ref() {
                Column::Uuid(v) => v,
                other => panic!("expected Uuid dictionary values, got {other:?}"),
            };
            for (row, want) in expected.iter().enumerate() {
                let is_null = d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row));
                match want {
                    None => assert!(is_null, "row {row} expected null"),
                    Some(bytes) => {
                        assert!(!is_null, "row {row} expected a value, got null");
                        let idx = d.indices[row] as usize;
                        assert_eq!(values.value(idx), bytes.as_slice(), "row {row}");
                    }
                }
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

/// Assert the per-row resolved IPv6 (16-byte wire) values of a dictionary column.
fn assert_dictionary_ipv6_values(column: &Column, expected: &[Option<&[u8; 16]>]) {
    match column {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), expected.len());
            let values = match d.values.as_ref() {
                Column::Ipv6(v) => v,
                other => panic!("expected Ipv6 dictionary values, got {other:?}"),
            };
            for (row, want) in expected.iter().enumerate() {
                let is_null = d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row));
                match want {
                    None => assert!(is_null, "row {row} expected null"),
                    Some(bytes) => {
                        assert!(!is_null, "row {row} expected a value, got null");
                        let idx = d.indices[row] as usize;
                        assert_eq!(values.value(idx), bytes.as_slice(), "row {row}");
                    }
                }
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

/// Assert the per-row resolved IPv4 (UInt32 numeric) values of a dictionary column.
fn assert_dictionary_ipv4_values(column: &Column, expected: &[Option<u32>]) {
    match column {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), expected.len());
            let values = match d.values.as_ref() {
                Column::Ipv4(v) => v,
                other => panic!("expected Ipv4 dictionary values, got {other:?}"),
            };
            for (row, want) in expected.iter().enumerate() {
                let is_null = d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row));
                match want {
                    None => assert!(is_null, "row {row} expected null"),
                    Some(v) => {
                        assert!(!is_null, "row {row} expected a value, got null");
                        let idx = d.indices[row] as usize;
                        assert_eq!(values.values[idx], *v, "row {row}");
                    }
                }
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

/// Assert the per-row resolved `UInt32` values of a dictionary column, treating a
/// null index as `None`, the way a consumer reads them.
fn assert_dictionary_u32_values(column: &Column, expected: &[Option<u32>]) {
    match column {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), expected.len());
            let values = match d.values.as_ref() {
                Column::UInt32(v) => v,
                other => panic!("expected UInt32 dictionary values, got {other:?}"),
            };
            for (row, want) in expected.iter().enumerate() {
                let is_null = d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row));
                match want {
                    None => assert!(is_null, "row {row} expected null"),
                    Some(v) => {
                        assert!(!is_null, "row {row} expected a value, got null");
                        let idx = d.indices[row] as usize;
                        assert_eq!(values.values[idx], *v, "row {row}");
                    }
                }
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

/// Assert the per-row resolved `Date` (UInt16 days) values of a dictionary column.
fn assert_dictionary_date_values(column: &Column, expected: &[Option<u16>]) {
    match column {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), expected.len());
            let values = match d.values.as_ref() {
                Column::Date(v) => v,
                other => panic!("expected Date dictionary values, got {other:?}"),
            };
            for (row, want) in expected.iter().enumerate() {
                let is_null = d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row));
                match want {
                    None => assert!(is_null, "row {row} expected null"),
                    Some(v) => {
                        assert!(!is_null, "row {row} expected a value, got null");
                        let idx = d.indices[row] as usize;
                        assert_eq!(values.values[idx], *v, "row {row}");
                    }
                }
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

/// Assert the per-row resolved string values of a dictionary (`LowCardinality`)
/// column, treating a null index as `None`, the way a consumer reads them.
fn assert_dictionary_string_values(column: &Column, expected: &[Option<&[u8]>]) {
    match column {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), expected.len());
            let values = match d.values.as_ref() {
                Column::Utf8(v) => v,
                other => panic!("expected Utf8 dictionary values, got {other:?}"),
            };
            for (row, want) in expected.iter().enumerate() {
                let is_null = d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row));
                match want {
                    None => assert!(is_null, "row {row} expected null"),
                    Some(bytes) => {
                        assert!(!is_null, "row {row} expected a value, got null");
                        let idx = d.indices[row] as usize;
                        assert_eq!(values.value(idx), *bytes, "row {row}");
                    }
                }
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

fn assert_multi_block(batch: &ChunkedBatch) {
    assert_eq!(batch.num_chunks(), 3);
    assert_eq!(batch.num_rows(), 5);
    assert_schema(batch, &[Expected::Exact("n", ChType::Int32)]);

    assert_int32_chunk(batch, 0, &[13, 14]);
    assert_int32_chunk(batch, 1, &[15, 16]);
    assert_int32_chunk(batch, 2, &[17]);
}

fn assert_int32_chunk(batch: &ChunkedBatch, chunk: usize, expected: &[i32]) {
    let block = &batch.chunks[chunk];
    assert_eq!(block.num_rows, expected.len());
    match block.column(0) {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), expected),
        other => panic!("expected Int32, got {other:?}"),
    }
}

fn assert_utf8_values(column: &Column, expected: &[&[u8]]) {
    match column {
        Column::Utf8(c) => assert_utf8_column(c, expected),
        other => panic!("expected Utf8, got {other:?}"),
    }
}

fn assert_utf8_column(column: &Utf8Column, expected: &[&[u8]]) {
    assert_eq!(column.len(), expected.len());
    for (row, expected_value) in expected.iter().enumerate() {
        assert_eq!(column.value(row), *expected_value, "row {row}");
    }
}

fn assert_fixed_binary_values(column: &Column, expected: &[&[u8]]) {
    match column {
        Column::FixedBinary(c) => assert_fixed_binary_column(c, expected),
        other => panic!("expected FixedBinary, got {other:?}"),
    }
}

fn assert_fixed_binary_column(column: &FixedBinaryColumn, expected: &[&[u8]]) {
    assert_eq!(column.len(), expected.len());
    for (row, expected_value) in expected.iter().enumerate() {
        assert_eq!(column.value(row), *expected_value, "row {row}");
    }
}

fn assert_validity(column: &Column, expected: &[bool]) {
    let validity = column.validity().expect("expected validity bitmap");
    assert_eq!(validity.len(), expected.len());
    for (row, expected_valid) in expected.iter().enumerate() {
        assert_eq!(validity.is_valid(row), *expected_valid, "row {row}");
    }
}
