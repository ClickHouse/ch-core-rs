//! Live-server acceptance test for the Native encoder.
//!
//! This proves the encoder produces bytes a real ClickHouse server accepts on
//! `INSERT ... FORMAT Native`: it encodes a batch, POSTs it over HTTP, reads the
//! rows back as `FORMAT Native`, decodes them with this crate, and asserts the
//! values survived the round-trip through the server.
//!
//! It is `#[ignore]` so `cargo test` stays hermetic (CI has no server). Run it
//! against a ClickHouse server matching `.server-ref` with:
//!
//! ```sh
//! cargo test --test live_insert -- --ignored
//! ```
//!
//! It shells out to `curl` (no added Rust dependency, same as
//! `scripts/gen_fixtures.sh`) and reads the same environment convention,
//! defaulting to `localhost:8123`, user `default`, no password:
//!
//! - `CLICKHOUSE_CONNECT_TEST_HOST` (default `localhost`)
//! - `CLICKHOUSE_CONNECT_TEST_PORT` (default `8123`)
//! - `CLICKHOUSE_CONNECT_TEST_USER` (default `default`)
//! - `CLICKHOUSE_CONNECT_TEST_PASSWORD` (default empty)
//! - `CLICKHOUSE_CONNECT_TEST_SCHEME` (default `http`)

use std::env;
use std::io::Write;
use std::process::{Command, Stdio};

use ch_core_rs::batch::ColBatch;
use ch_core_rs::bitmap::Bitmap;
use ch_core_rs::column::{
    AggregateStateColumn, ArrayColumn, BoolColumn, Column, DecimalColumn, DictionaryColumn,
    DynamicChild, DynamicColumn, FixedBinaryColumn, MapColumn, NothingColumn, PrimitiveColumn,
    TupleColumn, Utf8Column, VariantColumn,
};
use ch_core_rs::native::decode::{decode_all_bytes, decode_all_bytes_binary_types, DecodeOptions};
use ch_core_rs::native::encode::{encode_block, encode_block_binary_types, EncodeOptions};
use ch_core_rs::schema::{ChType, Field, GeoKind, IntervalKind, Schema};

const TABLE: &str = "ch_core_rs_encode_test";
const LC_U16_TABLE: &str = "ch_core_rs_encode_lc_u16_test";
const GSN_TABLE: &str = "ch_core_rs_encode_gsn_test";
const AGG_COUNT_TABLE: &str = "ch_core_rs_encode_agg_count_test";
const AGG_NOTHING_TABLE: &str = "ch_core_rs_encode_agg_nothing_test";
const AGG_SUM_TABLE: &str = "ch_core_rs_encode_agg_sum_test";
const DYNAMIC_TABLE: &str = "ch_core_rs_encode_dynamic_test";
const DYNAMIC_BINARY_TABLE: &str = "ch_core_rs_encode_dynamic_binary_test";

/// Build a `Utf8Column` from raw byte values, computing Arrow offsets the same
/// way the decoder does.
fn utf8_column(values: &[&[u8]]) -> Utf8Column {
    let mut offsets = Vec::with_capacity(values.len() + 1);
    let mut data = Vec::new();
    offsets.push(0i32);
    for v in values {
        data.extend_from_slice(v);
        offsets.push(data.len() as i32);
    }
    Utf8Column::new(offsets, data)
}

fn dynamic_batch() -> ColBatch {
    let mut uint64_blob = vec![0x04];
    uint64_blob.extend_from_slice(&13u64.to_le_bytes());
    let mut array_blob = vec![0x1e, 0x09, 0x02];
    array_blob.extend_from_slice(&79i32.to_le_bytes());
    array_blob.extend_from_slice(&(-13i32).to_le_bytes());
    let dynamic = DynamicColumn::try_new(
        &[1, 1, 0, 0],
        vec![
            DynamicChild::Shared(utf8_column(&[&uint64_blob, &array_blob])),
            DynamicChild::Typed {
                ch_type: ChType::String,
                values: Column::Utf8(utf8_column(&[b"user_1", b"user_2"])),
            },
        ],
    )
    .unwrap();
    ColBatch::new(
        Schema::new(vec![
            Field {
                name: "id".into(),
                ch_type: ChType::UInt8,
            },
            Field {
                name: "v".into(),
                ch_type: ChType::Dynamic { max_types: 1 },
            },
        ]),
        vec![
            Column::UInt8(PrimitiveColumn::new(vec![0, 1, 2, 3])),
            Column::Dynamic(dynamic),
        ],
        4,
    )
}

/// Build a `FixedBinaryColumn` of the given width from equal-width byte values.
fn fixed_binary_column(width: usize, values: &[&[u8]]) -> FixedBinaryColumn {
    let mut data = Vec::with_capacity(width * values.len());
    for v in values {
        assert_eq!(v.len(), width, "fixed-string value must be {width} bytes");
        data.extend_from_slice(v);
    }
    FixedBinaryColumn::new(data, width)
}

/// Build a BFloat16 column from exact raw bit words, stored in Native
/// little-endian byte order.
fn bfloat16_column(bits: &[u16]) -> PrimitiveColumn<[u8; 2]> {
    PrimitiveColumn::new(bits.iter().map(|word| word.to_le_bytes()).collect())
}

/// Build a DecimalColumn from raw wire-order fixed-width byte values.
fn decimal_column(width: usize, precision: u8, scale: u8, values: &[&[u8]]) -> DecimalColumn {
    let mut data = Vec::with_capacity(width * values.len());
    for v in values {
        assert_eq!(v.len(), width, "decimal value must be {width} bytes");
        data.extend_from_slice(v);
    }
    DecimalColumn::new(data, width, precision, scale)
}

/// The batch to insert: every encodable type over four rows (the ten fixed-width
/// numerics, `String`, `FixedString(4)`, `Bool`, the six temporal types,
/// `UUID`, `IPv4`, `IPv6`, `Enum8`/`Enum16`, four Decimal widths, five
/// `Nullable` columns, and four `Array` shapes covering a plain, `Nullable`,
/// `LowCardinality`, and nested `Array` element, each with at least one empty
/// row).
/// The `i32` column is strictly ascending so `ORDER BY i32` on read-back is
/// deterministic and matches insertion order, which lets the other columns line up
/// row-for-row too. The `Nullable` columns use the null pattern valid, null, valid,
/// null so the server round-trips the null map, not just the values.
fn sample_batch() -> ColBatch {
    let fields = vec![
        ("i8", ChType::Int8),
        ("i16", ChType::Int16),
        ("i32", ChType::Int32),
        ("i64", ChType::Int64),
        ("u8", ChType::UInt8),
        ("u16", ChType::UInt16),
        ("u32", ChType::UInt32),
        ("u64", ChType::UInt64),
        ("f32", ChType::Float32),
        ("f64", ChType::Float64),
        ("s", ChType::String),
        ("lc", ChType::LowCardinality(Box::new(ChType::String))),
        (
            "lcn",
            ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String)))),
        ),
        ("fs", ChType::FixedString(4)),
        ("b", ChType::Bool),
        ("d", ChType::Date),
        ("d32", ChType::Date32),
        (
            "dt",
            ChType::DateTime {
                timezone: Some("UTC".into()),
            },
        ),
        (
            "dt64",
            ChType::DateTime64 {
                precision: 3,
                timezone: Some("UTC".into()),
            },
        ),
        ("u", ChType::Uuid),
        ("ip4", ChType::Ipv4),
        ("ip6", ChType::Ipv6),
        (
            "e8",
            ChType::Enum8 {
                variants: vec![("off".into(), -1), ("idle".into(), 0), ("busy".into(), 13)],
            },
        ),
        (
            "e16",
            ChType::Enum16 {
                variants: vec![("off".into(), -1), ("idle".into(), 0), ("busy".into(), 79)],
            },
        ),
        (
            "dec32",
            ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            },
        ),
        (
            "dec64",
            ChType::Decimal {
                precision: 18,
                scale: 9,
                bits: 64,
            },
        ),
        (
            "dec128",
            ChType::Decimal {
                precision: 38,
                scale: 10,
                bits: 128,
            },
        ),
        (
            "dec256",
            ChType::Decimal {
                precision: 76,
                scale: 20,
                bits: 256,
            },
        ),
        ("i128", ChType::Int128),
        ("u128", ChType::UInt128),
        ("i256", ChType::Int256),
        ("u256", ChType::UInt256),
        ("ni128", ChType::Nullable(Box::new(ChType::Int128))),
        ("lc_i256", ChType::LowCardinality(Box::new(ChType::Int256))),
        ("ni32", ChType::Nullable(Box::new(ChType::Int32))),
        ("ns", ChType::Nullable(Box::new(ChType::String))),
        ("nb", ChType::Nullable(Box::new(ChType::Bool))),
        ("nu", ChType::Nullable(Box::new(ChType::Uuid))),
        (
            "ndec",
            ChType::Nullable(Box::new(ChType::Decimal {
                precision: 18,
                scale: 9,
                bits: 64,
            })),
        ),
        ("arr_i32", ChType::Array(Box::new(ChType::Int32))),
        (
            "arr_ns",
            ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::String)))),
        ),
        (
            "arr_lc",
            ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
        ),
        (
            "arr_arr",
            ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
        ),
        (
            "arr_lc_empty",
            ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
        ),
        (
            "tup",
            ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        ),
        (
            "tup_named",
            ChType::Tuple(vec![
                (Some("a".to_string()), ChType::Int32),
                (
                    Some("b".to_string()),
                    ChType::Nullable(Box::new(ChType::String)),
                ),
            ]),
        ),
        (
            "arr_tup",
            ChType::Array(Box::new(ChType::Tuple(vec![
                (None, ChType::Int32),
                (None, ChType::Int32),
            ]))),
        ),
        (
            "ntup",
            ChType::Nullable(Box::new(ChType::Tuple(vec![
                (None, ChType::Int32),
                (None, ChType::String),
            ]))),
        ),
        (
            "m",
            ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        ),
        (
            "m_lc",
            ChType::Map(
                Box::new(ChType::LowCardinality(Box::new(ChType::String))),
                Box::new(ChType::UInt8),
            ),
        ),
        (
            "m_nv",
            ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Nullable(Box::new(ChType::String))),
            ),
        ),
        (
            "m_arr",
            ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Array(Box::new(ChType::Int32))),
            ),
        ),
        (
            "arr_m",
            ChType::Array(Box::new(ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Int32),
            ))),
        ),
        (
            "m_empty",
            ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        ),
        ("t", ChType::Time),
        ("t64", ChType::Time64 { precision: 6 }),
        ("nt", ChType::Nullable(Box::new(ChType::Time))),
        (
            "nt64",
            ChType::Nullable(Box::new(ChType::Time64 { precision: 6 })),
        ),
        ("lc_time", ChType::LowCardinality(Box::new(ChType::Time))),
        ("iy", ChType::Interval(IntervalKind::Year)),
        ("iq", ChType::Interval(IntervalKind::Quarter)),
        ("imo", ChType::Interval(IntervalKind::Month)),
        ("iw", ChType::Interval(IntervalKind::Week)),
        ("id", ChType::Interval(IntervalKind::Day)),
        ("ih", ChType::Interval(IntervalKind::Hour)),
        ("imi", ChType::Interval(IntervalKind::Minute)),
        ("isecond", ChType::Interval(IntervalKind::Second)),
        ("ims", ChType::Interval(IntervalKind::Millisecond)),
        ("ius", ChType::Interval(IntervalKind::Microsecond)),
        ("ins", ChType::Interval(IntervalKind::Nanosecond)),
        (
            "nid",
            ChType::Nullable(Box::new(ChType::Interval(IntervalKind::Day))),
        ),
        (
            "lc_ih",
            ChType::LowCardinality(Box::new(ChType::Interval(IntervalKind::Hour))),
        ),
        ("bf", ChType::BFloat16),
        ("nbf", ChType::Nullable(Box::new(ChType::BFloat16))),
        ("lc_bf", ChType::LowCardinality(Box::new(ChType::BFloat16))),
        (
            "tn",
            ChType::Tuple(vec![(None, ChType::Nullable(Box::new(ChType::Nothing)))]),
        ),
        ("v", ChType::Variant(vec![ChType::String, ChType::UInt64])),
    ]
    .into_iter()
    .map(|(name, ch_type)| Field {
        name: name.to_string(),
        ch_type,
    })
    .collect();

    // 0x00 = valid, 0x01 = null (ClickHouse null-map polarity): valid, null, valid, null.
    let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let mut ns = utf8_column(&[b"user_1", b"", b"user_2", b""]);
    ns.validity = Some(validity());
    let mut nu = fixed_binary_column(16, &[&[0x13; 16], &[0u8; 16], &[0x79; 16], &[0u8; 16]]);
    nu.validity = Some(validity());
    let mut nbf = bfloat16_column(&[0x4150, 0x0000, 0x429e, 0x0000]);
    nbf.validity = Some(validity());
    let dec32_neg = (-13i32).to_le_bytes();
    let dec32_zero = 0i32.to_le_bytes();
    let dec32_pos = 79i32.to_le_bytes();
    let dec32_one = 1i32.to_le_bytes();
    let dec64_neg = (-13i64).to_le_bytes();
    let dec64_zero = 0i64.to_le_bytes();
    let dec64_pos = 79i64.to_le_bytes();
    let dec64_one = 1i64.to_le_bytes();
    let dec128_neg = [0xFFu8; 16];
    let dec128_zero = [0u8; 16];
    let dec128_pos = *b"\x4F\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
    let dec128_one = *b"\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
    let dec256_neg = [0xFFu8; 32];
    let dec256_zero = [0u8; 32];
    let dec256_pos =
        *b"\x13\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
    let dec256_one =
        *b"\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
    let mut ndec = decimal_column(
        8,
        18,
        9,
        &[&dec64_neg, &dec64_zero, &dec64_pos, &dec64_zero],
    );
    ndec.validity = Some(validity());

    // Wide integers: raw little-endian fixed-width bytes with sign/boundary
    // coverage. Signed rows include the width MIN (MSB-only) and -1 (all 0xFF);
    // unsigned rows include a high-bit-set value that must stay positive.
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
    let mut i128_min = [0u8; 16];
    i128_min[15] = 0x80;
    let mut i128_max = [0xFFu8; 16];
    i128_max[15] = 0x7F;
    let mut u128_high = [0u8; 16];
    u128_high[15] = 0x80;
    let mut i256_min = [0u8; 32];
    i256_min[31] = 0x80;
    let mut i256_max = [0xFFu8; 32];
    i256_max[31] = 0x7F;
    let mut u256_high = [0u8; 32];
    u256_high[31] = 0x80;
    // Nullable(Int128): valid, null, valid, null. Null rows carry a placeholder.
    let mut ni128 = fixed_binary_column(16, &[&w16(13), &[0u8; 16], &[0xFFu8; 16], &[0u8; 16]]);
    ni128.validity = Some(validity());

    let columns = vec![
        Column::Int8(PrimitiveColumn::new(vec![i8::MIN, -13, 0, i8::MAX])),
        Column::Int16(PrimitiveColumn::new(vec![i16::MIN, -13, 0, i16::MAX])),
        Column::Int32(PrimitiveColumn::new(vec![i32::MIN, -79, 0, i32::MAX])),
        Column::Int64(PrimitiveColumn::new(vec![i64::MIN, -79, 0, i64::MAX])),
        Column::UInt8(PrimitiveColumn::new(vec![0, 13, 79, u8::MAX])),
        Column::UInt16(PrimitiveColumn::new(vec![0, 13, 79, u16::MAX])),
        Column::UInt32(PrimitiveColumn::new(vec![0, 13, 79, u32::MAX])),
        Column::UInt64(PrimitiveColumn::new(vec![0, 13, 79, u64::MAX])),
        Column::Float32(PrimitiveColumn::new(vec![-1.25, 0.0, 3.5, 79.125])),
        Column::Float64(PrimitiveColumn::new(vec![-1.25, 0.0, 3.5, 79.125])),
        Column::Utf8(utf8_column(&[b"user_1", b"", b"n", b"user_2_longer"])),
        Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1, 3],
            Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2", b"user_3"])),
        )),
        Column::Dictionary(DictionaryColumn::new_nullable(
            vec![1, 0, 2, 0],
            Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
            validity(),
        )),
        Column::FixedBinary(fixed_binary_column(
            4,
            &[b"road", b"1234", b"\x00\x00\x00\x00", b"n\x00\x00\x00"],
        )),
        Column::Bool(BoolColumn::from_wire_bytes(&[1, 0, 1, 0])),
        // Neutral in-range temporal values. Date is days since 1970-01-01, Date32
        // days signed (one row pre-epoch), DateTime seconds since the epoch,
        // DateTime64(3) milliseconds. All well within each type's server range so
        // the Memory table round-trips them verbatim.
        Column::Date(PrimitiveColumn::new(vec![0, 19000, 19001, 19710])),
        Column::Date32(PrimitiveColumn::new(vec![-25567, 0, 19000, 19710])),
        Column::DateTime(PrimitiveColumn::new(vec![
            0,
            1_600_000_000,
            1_700_000_000,
            1_710_000_000,
        ])),
        Column::DateTime64(PrimitiveColumn::new(vec![
            0,
            1_600_000_000_000,
            1_700_000_000_000,
            1_710_000_000_000,
        ])),
        // UUID and IPv6 are raw 16-byte passthrough (UUID in its wire UInt128 POD
        // order, IPv6 in network byte order); IPv4 is the standard u32 numeric
        // value. Distinct byte patterns per row so any reordering or misframing
        // shows up in the comparison.
        Column::Uuid(fixed_binary_column(
            16,
            &[
                &[0u8; 16],
                b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x0C\x0D\x0E\x0F\x10",
                &[0x79; 16],
                &[0xFF; 16],
            ],
        )),
        // 0.0.0.0, 127.0.0.1, 192.168.0.1, 255.255.255.255.
        Column::Ipv4(PrimitiveColumn::new(vec![
            0,
            2_130_706_433,
            3_232_235_521,
            u32::MAX,
        ])),
        // ::, ::1, 2001:db8::13, all-0xFF.
        Column::Ipv6(fixed_binary_column(
            16,
            &[
                &[0u8; 16],
                b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01",
                b"\x20\x01\x0D\xB8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x13",
                &[0xFF; 16],
            ],
        )),
        // Enum8/Enum16 physical values must be legal members of the declared
        // variant sets (the server validates each value on INSERT); they are the
        // underlying signed int on the wire. The values line up with the
        // `ORDER BY i32` row order: off, idle, busy, idle.
        Column::Enum8(PrimitiveColumn::new(vec![-1, 0, 13, 0])),
        Column::Enum16(PrimitiveColumn::new(vec![-1, 0, 79, 0])),
        // Decimal stores scaled signed integers as raw little-endian bytes. These
        // values fit each declared precision and include a negative row.
        Column::Decimal(decimal_column(
            4,
            9,
            4,
            &[&dec32_neg, &dec32_zero, &dec32_pos, &dec32_one],
        )),
        Column::Decimal(decimal_column(
            8,
            18,
            9,
            &[&dec64_neg, &dec64_zero, &dec64_pos, &dec64_one],
        )),
        Column::Decimal(decimal_column(
            16,
            38,
            10,
            &[&dec128_neg, &dec128_zero, &dec128_pos, &dec128_one],
        )),
        Column::Decimal(decimal_column(
            32,
            76,
            20,
            &[&dec256_neg, &dec256_zero, &dec256_pos, &dec256_one],
        )),
        // Wide integers: 16/32 raw bytes per row, verbatim little-endian
        // passthrough. Distinct sign/boundary values per row so any reordering,
        // byteswap, or sign misread would show up in the comparison.
        Column::Int128(fixed_binary_column(
            16,
            &[&i128_min, &[0xFFu8; 16], &[0u8; 16], &i128_max],
        )),
        Column::UInt128(fixed_binary_column(
            16,
            &[&[0u8; 16], &w16(13), &u128_high, &[0xFFu8; 16]],
        )),
        Column::Int256(fixed_binary_column(
            32,
            &[&i256_min, &[0xFFu8; 32], &[0u8; 32], &i256_max],
        )),
        Column::UInt256(fixed_binary_column(
            32,
            &[&[0u8; 32], &w32(13), &u256_high, &[0xFFu8; 32]],
        )),
        Column::Int128(ni128),
        // LowCardinality(Int256): slot 0 is the reserved default, real rows
        // reference slots 1... The server reorders its own dictionary on
        // readback, but `column_repr` resolves indices through the block-local
        // dictionary, so the physical comparison stays stable.
        Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1, 2],
            Column::Int256(fixed_binary_column(32, &[&[0u8; 32], &w32(13), &w32(79)])),
        )),
        Column::Int32(PrimitiveColumn::new_nullable(
            vec![13, 0, 79, 0],
            validity(),
        )),
        Column::Utf8(ns),
        Column::Bool(BoolColumn::from_wire_bytes_nullable(
            &[1, 0, 1, 0],
            validity(),
        )),
        Column::Uuid(nu),
        Column::Decimal(ndec),
        // Array(Int32): [13, 79], [], [21], [34, 55]. The empty row exercises
        // the adjacent-equal offset pair on a live INSERT.
        Column::Array(ArrayColumn::new(
            vec![0, 2, 2, 3, 5],
            Column::Int32(PrimitiveColumn::new(vec![13, 79, 21, 34, 55])),
        )),
        // Array(Nullable(String)): ["user_1", NULL], [], ["user_2"], [NULL].
        // The element null map covers the flattened run of four elements.
        Column::Array(ArrayColumn::new(vec![0, 2, 2, 3, 4], {
            let mut elements = utf8_column(&[b"user_1", b"", b"user_2", b""]);
            elements.validity = Some(Bitmap::from_ch_null_map(&[0, 1, 0, 1]));
            Column::Utf8(elements)
        })),
        // Array(LowCardinality(String)): [user_1, user_2], [], [user_1], [].
        // The LC key version is hoisted ahead of the offsets and the element
        // dictionary covers the flattened run of three elements.
        Column::Array(ArrayColumn::new(
            vec![0, 2, 2, 3, 3],
            Column::Dictionary(DictionaryColumn::new(
                vec![1, 2, 1],
                Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
            )),
        )),
        // Array(Array(Int32)): [[13, 79], [21]], [], [[34]], [[], [55, 89]].
        // Outer offsets count inner arrays; the inner level includes its own
        // empty array.
        Column::Array(ArrayColumn::new(
            vec![0, 2, 2, 3, 5],
            Column::Array(ArrayColumn::new(
                vec![0, 2, 3, 4, 4, 6],
                Column::Int32(PrimitiveColumn::new(vec![13, 79, 21, 34, 55, 89])),
            )),
        )),
        // Array(LowCardinality(String)) with EVERY row empty: the wire must be
        // the hoisted LC key version, four zero offsets, and NOTHING for the LC
        // element run (the server's limit == 0 early return); an index word or
        // key count here would make the server misparse the INSERT.
        Column::Array(ArrayColumn::new(
            vec![0, 0, 0, 0, 0],
            Column::Dictionary(DictionaryColumn::new(
                vec![],
                Column::Utf8(utf8_column(&[])),
            )),
        )),
        // Tuple(Int32, String): each element's full run in declaration order,
        // no tuple-level framing. Rows (13, user_1), (-7, ""), (79, user_2),
        // (0, user_3).
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![13, -7, 79, 0])),
                Column::Utf8(utf8_column(&[b"user_1", b"", b"user_2", b"user_3"])),
            ],
            4,
        )),
        // Tuple(a Int32, b Nullable(String)): element b carries its own null
        // map inside its element body (valid, null, valid, null).
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![1, 2, 3, 4])),
                Column::Utf8({
                    let mut b = utf8_column(&[b"user_1", b"", b"user_2", b""]);
                    b.validity = Some(validity());
                    b
                }),
            ],
            4,
        )),
        // Array(Tuple(Int32, Int32)): [(13, 79)], [], [(1, 2), (3, 4)],
        // [(-1, -2)] -> offsets [0, 1, 1, 3, 4] over a 4-row flattened tuple.
        Column::Array(ArrayColumn::new(
            vec![0, 1, 1, 3, 4],
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int32(PrimitiveColumn::new(vec![13, 1, 3, -1])),
                    Column::Int32(PrimitiveColumn::new(vec![79, 2, 4, -2])),
                ],
                4,
            )),
        )),
        // Nullable(Tuple(Int32, String)): the tuple-level null map precedes
        // the tuple body; null rows carry element defaults (0, "") so the
        // server's read-back placeholders compare equal.
        Column::Tuple(TupleColumn::new_nullable(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![13, 0, 79, 0])),
                Column::Utf8(utf8_column(&[b"user_1", b"", b"user_2", b""])),
            ],
            4,
            validity(),
        )),
        // Map(String, Int32): {a: 13} / {} / {a: 1, b: 2} / {k: -7}. The
        // entries column is the (keys, values) tuple over the flattened runs.
        Column::Map(map_column(
            vec![0, 1, 1, 3, 4],
            Column::Utf8(utf8_column(&[b"a", b"a", b"b", b"k"])),
            Column::Int32(PrimitiveColumn::new(vec![13, 1, 2, -7])),
        )),
        // Map(LowCardinality(String), UInt8): the LC key version is hoisted
        // ahead of the offsets. {red: 1} / {red: 2, blue: 3} / {} / {green: 4}.
        Column::Map(map_column(
            vec![0, 1, 3, 3, 4],
            Column::Dictionary(DictionaryColumn::new(
                vec![1, 1, 2, 3],
                Column::Utf8(utf8_column(&[b"", b"red", b"blue", b"green"])),
            )),
            Column::UInt8(PrimitiveColumn::new(vec![1, 2, 3, 4])),
        )),
        // Map(String, Nullable(String)): the flattened value run carries its
        // own null map. {a: user_1} / {b: NULL} / {} / {c: user_2, d: NULL}.
        Column::Map(map_column(
            vec![0, 1, 2, 2, 4],
            Column::Utf8(utf8_column(&[b"a", b"b", b"c", b"d"])),
            Column::Utf8({
                let mut v = utf8_column(&[b"user_1", b"", b"user_2", b""]);
                v.validity = Some(validity());
                v
            }),
        )),
        // Map(String, Array(Int32)): the flattened value run is itself an
        // Array. {a: [13]} / {} / {b: [], c: [1, 2]} / {d: [79]}.
        Column::Map(map_column(
            vec![0, 1, 1, 3, 4],
            Column::Utf8(utf8_column(&[b"a", b"b", b"c", b"d"])),
            Column::Array(ArrayColumn::new(
                vec![0, 1, 1, 3, 4],
                Column::Int32(PrimitiveColumn::new(vec![13, 1, 2, 79])),
            )),
        )),
        // Array(Map(String, Int32)): [] / [{a: 1}] / [{b: 2}, {}] / [{c: 3}].
        Column::Array(ArrayColumn::new(
            vec![0, 0, 1, 3, 4],
            Column::Map(map_column(
                vec![0, 1, 2, 2, 3],
                Column::Utf8(utf8_column(&[b"a", b"b", b"c"])),
                Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
            )),
        )),
        // Map(String, Int32) with EVERY row empty: all-zero offsets, no
        // key/value runs at all on the wire.
        Column::Map(map_column(
            vec![0, 0, 0, 0, 0],
            Column::Utf8(utf8_column(&[])),
            Column::Int32(PrimitiveColumn::new(vec![])),
        )),
        // Time and Time64 are raw signed seconds/ticks. Include the documented
        // text extrema, negative fractional ticks, Nullable wrappers, and the
        // one legal LC form (Time; the server forbids LowCardinality(Time64)).
        Column::Time(PrimitiveColumn::new(vec![
            -3_599_999, -3_600, 13, 3_599_999,
        ])),
        Column::Time64(PrimitiveColumn::new(vec![
            -3_599_999_999_999,
            -1,
            13_000_079,
            3_599_999_999_999,
        ])),
        Column::Time(PrimitiveColumn::new_nullable(
            vec![-13, 0, 79, 0],
            validity(),
        )),
        Column::Time64(PrimitiveColumn::new_nullable(
            vec![-13, 0, 79, 0],
            validity(),
        )),
        Column::Dictionary(DictionaryColumn::new(
            vec![0, 1, 0, 2],
            Column::Time(PrimitiveColumn::new(vec![-13, 79, 258])),
        )),
        // Every Interval* is the same signed Int64 count body with a distinct
        // logical unit in the type string. Wrapper representatives prove the
        // Nullable null map and LowCardinality dictionary paths compose.
        Column::Interval(PrimitiveColumn::new(vec![-13, 0, 79, 258])),
        Column::Interval(PrimitiveColumn::new(vec![-13, 0, 79, 258])),
        Column::Interval(PrimitiveColumn::new(vec![-13, 0, 79, 258])),
        Column::Interval(PrimitiveColumn::new(vec![-13, 0, 79, 258])),
        Column::Interval(PrimitiveColumn::new(vec![-13, 0, 79, 258])),
        Column::Interval(PrimitiveColumn::new(vec![-13, 0, 79, 258])),
        Column::Interval(PrimitiveColumn::new(vec![-13, 0, 79, 258])),
        Column::Interval(PrimitiveColumn::new(vec![-13, 0, 79, 258])),
        Column::Interval(PrimitiveColumn::new(vec![-13, 0, 79, 258])),
        Column::Interval(PrimitiveColumn::new(vec![-13, 0, 79, 258])),
        Column::Interval(PrimitiveColumn::new(vec![-13, 0, 79, 258])),
        Column::Interval(PrimitiveColumn::new_nullable(
            vec![13, 0, 79, 0],
            validity(),
        )),
        Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1, 3],
            Column::Interval(PrimitiveColumn::new(vec![0, 13, 79, 258])),
        )),
        // BFloat16 words are stored as exact raw little-endian bytes. Include
        // plain, Nullable, and LowCardinality shapes so the live server proves
        // every generic wrapper path accepts the width-2 body.
        Column::BFloat16(bfloat16_column(&[0xbfa0, 0x0000, 0x4060, 0x429e])),
        Column::BFloat16(nbf),
        Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1, 3],
            Column::BFloat16(bfloat16_column(&[0x0000, 0x4150, 0x429e, 0x4381])),
        )),
        // Top-level Nullable(Nothing) cannot be stored in a table, but the
        // server permits it as a Tuple element. This grounds the encoder's
        // null-map-then-placeholder body against a real INSERT path.
        Column::Tuple(TupleColumn::new(
            vec![Column::Nothing(NothingColumn::new_nullable(
                4,
                Bitmap::from_ch_null_map(&[1, 1, 1, 1]),
            ))],
            4,
        )),
        // Variant(String, UInt64): intrinsic NULL, String, UInt64, String.
        // Children are dense and follow canonical alternative order.
        Column::Variant(
            VariantColumn::try_new(
                &[u8::MAX, 0, 1, 0],
                vec![
                    Column::Utf8(utf8_column(&[b"user_1", b"user_2"])),
                    Column::UInt64(PrimitiveColumn::new(vec![13])),
                ],
            )
            .expect("valid Variant test column"),
        ),
    ];

    ColBatch::new(Schema::new(fields), columns, 4)
}

/// Build a `MapColumn` from Arrow-shaped offsets plus the keys and values
/// columns.
fn map_column(offsets: Vec<i64>, keys: Column, values: Column) -> MapColumn {
    let total = keys.len();
    MapColumn::new(
        offsets,
        Column::Tuple(TupleColumn::new(vec![keys, values], total)),
    )
}

/// A focused LowCardinality batch whose dictionary has more than 255 entries,
/// forcing the encoder's UInt16 index-width path. FixedString also covers a
/// non-String dictionary value body in the live server round-trip.
fn lc_fixed_string_u16_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "k".into(),
            ch_type: ChType::UInt16,
        },
        Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::FixedString(4))),
        },
    ];

    let mut values = Vec::with_capacity(260 * 4);
    for i in 0..260u16 {
        values.extend_from_slice(&i.to_le_bytes());
        values.extend_from_slice(&[0x13, 0x79]);
    }

    let columns = vec![
        Column::UInt16(PrimitiveColumn::new((0..260u16).collect())),
        Column::Dictionary(DictionaryColumn::new(
            (0..260i32).collect(),
            Column::FixedBinary(FixedBinaryColumn::new(values, 4)),
        )),
    ];

    ColBatch::new(Schema::new(fields), columns, 260)
}

/// A focused batch of the name-decoration alias types:
/// `SimpleAggregateFunction`, the geo aliases, and `Nested`. Each is a custom
/// name over a physical type whose serialization slot is null, so the encoder
/// writes the alias spelling in the header and the underlying type's body; the
/// physical `Column` shape here is exactly that underlying type. `i32` is
/// strictly ascending so `ORDER BY i32` on readback matches insertion order.
fn geo_saf_nested_batch() -> ColBatch {
    // 0x00 = valid, 0x01 = null: valid, null, valid, null.
    let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);

    let fields = vec![
        Field {
            name: "i32".into(),
            ch_type: ChType::Int32,
        },
        // SimpleAggregateFunction over a scalar: delegates to Float64.
        Field {
            name: "saf_sum".into(),
            ch_type: ChType::SimpleAggregateFunction {
                func: "sum".into(),
                inner: Box::new(ChType::Float64),
            },
        },
        // SimpleAggregateFunction over LowCardinality(Nullable(String)):
        // delegates to the LC dictionary, exactly a bare LC on the wire.
        Field {
            name: "saf_lc".into(),
            ch_type: ChType::SimpleAggregateFunction {
                func: "anyLast".into(),
                inner: Box::new(ChType::LowCardinality(Box::new(ChType::Nullable(
                    Box::new(ChType::String),
                )))),
            },
        },
        // Point = Tuple(Float64, Float64).
        Field {
            name: "point".into(),
            ch_type: ChType::Geo(GeoKind::Point),
        },
        // Nullable(Point): a Nullable over the Point tuple.
        Field {
            name: "npoint".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Geo(GeoKind::Point))),
        },
        // Ring = Array(Point).
        Field {
            name: "ring".into(),
            ch_type: ChType::Geo(GeoKind::Ring),
        },
        // MultiPolygon = Array(Array(Array(Point))), three Array levels.
        Field {
            name: "mpoly".into(),
            ch_type: ChType::Geo(GeoKind::MultiPolygon),
        },
        // Nested(x UInt32, y String) = Array(Tuple(named x, y)).
        Field {
            name: "nst".into(),
            ch_type: ChType::Nested(vec![
                ("x".into(), ChType::UInt32),
                ("y".into(), ChType::String),
            ]),
        },
        // Nullable(SimpleAggregateFunction(sum, UInt64)): the SAF is name
        // decoration inside the Nullable, delegating to Nullable(UInt64).
        Field {
            name: "nsaf".into(),
            ch_type: ChType::Nullable(Box::new(ChType::SimpleAggregateFunction {
                func: "sum".into(),
                inner: Box::new(ChType::UInt64),
            })),
        },
        // Tuple(v SimpleAggregateFunction(sum, UInt64)): the SAF sits in a named
        // Tuple element, delegating to a one-field UInt64 tuple.
        Field {
            name: "tsaf".into(),
            ch_type: ChType::Tuple(vec![(
                Some("v".into()),
                ChType::SimpleAggregateFunction {
                    func: "sum".into(),
                    inner: Box::new(ChType::UInt64),
                },
            )]),
        },
        // LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String))): the
        // SAF name decoration sits BETWEEN the LowCardinality and its
        // removeNullable Nullable, so encode must resolve the full SAF chain to
        // treat the column as nullable and write the bare String dictionary body.
        // This is a real server header confirmed live at v26.6.1.1193-stable.
        Field {
            name: "lc_nsaf".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
                func: "anyLast".into(),
                inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
            })),
        },
    ];

    let columns = vec![
        Column::Int32(PrimitiveColumn::new(vec![0, 1, 2, 3])),
        // saf_sum -> Float64 buffer.
        Column::Float64(PrimitiveColumn::new(vec![-1.25, 0.0, 13.0, 79.125])),
        // saf_lc -> nullable dictionary: rows resolve user_1, NULL, user_2, NULL.
        // Slot 0 is the null sentinel; validity marks rows 1 and 3 null.
        Column::Dictionary(DictionaryColumn::new_nullable(
            vec![1, 0, 2, 0],
            Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
            validity(),
        )),
        // point -> Tuple(Float64 x, Float64 y). Rows (13, 79), (-1.5, 2.5),
        // (0, 0), (79.125, -13.25).
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Float64(PrimitiveColumn::new(vec![13.0, -1.5, 0.0, 79.125])),
                Column::Float64(PrimitiveColumn::new(vec![79.0, 2.5, 0.0, -13.25])),
            ],
            4,
        )),
        // npoint -> Tuple with a tuple-level null map; null rows carry element
        // placeholders. Valid rows 0 and 2 are (13, 79) and (1.25, -2.5).
        Column::Tuple(TupleColumn::new_nullable(
            vec![
                Column::Float64(PrimitiveColumn::new(vec![13.0, 0.0, 1.25, 0.0])),
                Column::Float64(PrimitiveColumn::new(vec![79.0, 0.0, -2.5, 0.0])),
            ],
            4,
            validity(),
        )),
        // ring -> Array(Tuple). Rows [(13, 79)] / [] / [(1, 2), (3, 4)] /
        // [(-1.5, -2.5)] -> offsets [0, 1, 1, 3, 4].
        Column::Array(ArrayColumn::new(
            vec![0, 1, 1, 3, 4],
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Float64(PrimitiveColumn::new(vec![13.0, 1.0, 3.0, -1.5])),
                    Column::Float64(PrimitiveColumn::new(vec![79.0, 2.0, 4.0, -2.5])),
                ],
                4,
            )),
        )),
        // mpoly -> Array(Array(Array(Tuple))). Rows [[[(13, 79)]]] / [] /
        // [[[(1, 2), (3, 4)], [(5, 6)]]] / [[[(-1, -2)]], [[(7, 8)]]]. Outer
        // offsets [0, 1, 1, 2, 4], mid [0, 1, 3, 4, 5], inner [0, 1, 3, 4, 5, 6].
        Column::Array(ArrayColumn::new(
            vec![0, 1, 1, 2, 4],
            Column::Array(ArrayColumn::new(
                vec![0, 1, 3, 4, 5],
                Column::Array(ArrayColumn::new(
                    vec![0, 1, 3, 4, 5, 6],
                    Column::Tuple(TupleColumn::new(
                        vec![
                            Column::Float64(PrimitiveColumn::new(vec![
                                13.0, 1.0, 3.0, 5.0, -1.0, 7.0,
                            ])),
                            Column::Float64(PrimitiveColumn::new(vec![
                                79.0, 2.0, 4.0, 6.0, -2.0, 8.0,
                            ])),
                        ],
                        6,
                    )),
                )),
            )),
        )),
        // nst -> Array(Tuple(UInt32, String)). Rows [(13, user_1)] / [] /
        // [(79, a), (1, user_2)] / [(2, x)] -> offsets [0, 1, 1, 3, 4].
        Column::Array(ArrayColumn::new(
            vec![0, 1, 1, 3, 4],
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::UInt32(PrimitiveColumn::new(vec![13, 79, 1, 2])),
                    Column::Utf8(utf8_column(&[b"user_1", b"a", b"user_2", b"x"])),
                ],
                4,
            )),
        )),
        // nsaf -> nullable UInt64: valid, null, valid, null -> [13, 0, 79, 0];
        // null rows carry the server's placeholder 0 after the null map.
        Column::UInt64(PrimitiveColumn {
            values: vec![13, 0, 79, 0],
            validity: Some(validity()),
        }),
        // tsaf -> one-field UInt64 tuple. Values 13, 26, 39, 52.
        Column::Tuple(TupleColumn::new(
            vec![Column::UInt64(PrimitiveColumn::new(vec![13, 26, 39, 52]))],
            4,
        )),
        // lc_nsaf -> nullable dictionary: rows resolve user_1, NULL, user_2, NULL.
        // Slot 0 is the null sentinel; validity marks rows 1 and 3 null. Identical
        // wire body to a bare LowCardinality(Nullable(String)) once the SAF chain
        // is resolved.
        Column::Dictionary(DictionaryColumn::new_nullable(
            vec![1, 0, 2, 0],
            Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
            validity(),
        )),
    ];

    ColBatch::new(Schema::new(fields), columns, 4)
}

struct Server {
    base_url: String,
    user: String,
    password: String,
}

impl Server {
    fn from_env() -> Self {
        let host = env::var("CLICKHOUSE_CONNECT_TEST_HOST").unwrap_or_else(|_| "localhost".into());
        let port = env::var("CLICKHOUSE_CONNECT_TEST_PORT").unwrap_or_else(|_| "8123".into());
        let scheme = env::var("CLICKHOUSE_CONNECT_TEST_SCHEME").unwrap_or_else(|_| "http".into());
        Server {
            base_url: format!("{scheme}://{host}:{port}/"),
            user: env::var("CLICKHOUSE_CONNECT_TEST_USER").unwrap_or_else(|_| "default".into()),
            password: env::var("CLICKHOUSE_CONNECT_TEST_PASSWORD").unwrap_or_default(),
        }
    }

    /// Run `curl` against the server. `extra` carries the request-shaping args
    /// (the URL query, `--data-urlencode`, `--data-binary`, ...). `stdin`, if
    /// present, is piped to curl for a `@-` body. Returns (curl_ok, stdout,
    /// stderr).
    fn curl(&self, url: &str, extra: &[&str], stdin: Option<&[u8]>) -> (bool, Vec<u8>, Vec<u8>) {
        let mut cmd = Command::new("curl");
        cmd.arg("-sS")
            .arg("--user")
            .arg(format!("{}:{}", self.user, self.password))
            .arg(url);
        for a in extra {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });

        let mut child = cmd
            .spawn()
            .expect("failed to spawn curl (is it installed and on PATH?)");
        if let Some(bytes) = stdin {
            child
                .stdin
                .take()
                .expect("curl stdin")
                .write_all(bytes)
                .expect("write curl stdin");
        }
        let out = child.wait_with_output().expect("curl did not run");
        (out.status.success(), out.stdout, out.stderr)
    }

    /// Run a statement that should return an empty body (DDL, INSERT). A
    /// non-empty body is a ClickHouse error (HTTP error responses carry the
    /// message in the body, and curl without `--fail` still exits 0).
    fn exec_empty(&self, url: &str, extra: &[&str], stdin: Option<&[u8]>, what: &str) {
        let (ok, stdout, stderr) = self.curl(url, extra, stdin);
        assert!(
            ok,
            "curl failed for {what}: {}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(
            stdout.is_empty(),
            "{what} returned an error: {}",
            String::from_utf8_lossy(&stdout)
        );
    }

    /// Send a DDL/SQL statement. ClickHouse reads a POST body with no `query`
    /// URL param as the whole query, so `--data-binary` sends the SQL verbatim.
    /// This is a POST (unlike `--get`, which curl would turn into a read-only GET
    /// that rejects DDL).
    fn ddl(&self, sql: &str) {
        self.ddl_with_params(sql, "");
    }

    /// Send a DDL statement with extra URL query parameters (per-query server
    /// settings, e.g. the `enable_nullable_tuple_type` DDL gate for a
    /// `Nullable(Tuple(...))` column).
    fn ddl_with_params(&self, sql: &str, params: &str) {
        let url = format!("{}{params}", self.base_url);
        self.exec_empty(&url, &["--data-binary", sql], None, sql);
    }

    /// INSERT the given Native bytes into `TABLE`. The query goes in the URL (so
    /// the request body is exactly the Native stream) and the bytes are piped as
    /// a `--data-binary @-` body.
    fn insert_native(&self, bytes: &[u8]) {
        self.insert_native_into(TABLE, bytes);
    }

    /// INSERT the given Native bytes into `table`.
    fn insert_native_into(&self, table: &str, bytes: &[u8]) {
        let url = format!(
            "{}?query=INSERT%20INTO%20{table}%20FORMAT%20Native",
            self.base_url
        );
        self.exec_empty(&url, &["--data-binary", "@-"], Some(bytes), "INSERT");
    }

    fn insert_native_binary_types_into(&self, table: &str, bytes: &[u8]) {
        let url = format!(
            "{}?input_format_native_decode_types_in_binary_format=1&query=INSERT%20INTO%20{table}%20FORMAT%20Native",
            self.base_url
        );
        self.exec_empty(&url, &["--data-binary", "@-"], Some(bytes), "INSERT");
    }

    /// Run a SELECT and return the raw response body bytes. The query is the raw
    /// POST body, so a `FORMAT Native` response comes back as binary.
    fn select(&self, sql: &str) -> Vec<u8> {
        self.select_with_params(sql, "")
    }

    fn select_with_params(&self, sql: &str, params: &str) -> Vec<u8> {
        let url = format!("{}{params}", self.base_url);
        let (ok, stdout, stderr) = self.curl(&url, &["--data-binary", sql], None);
        assert!(
            ok,
            "curl failed for SELECT: {}",
            String::from_utf8_lossy(&stderr)
        );
        // A ClickHouse error comes back as text; a real Native response is
        // binary. Surface the text if it looks like an error.
        if stdout.starts_with(b"Code:") {
            panic!("SELECT errored: {}", String::from_utf8_lossy(&stdout));
        }
        stdout
    }
}

/// Render one column's physical values without applying that column's validity.
/// `LowCardinality` resolves each row through its block-local dictionary so
/// server-side dictionary reordering does not affect the live INSERT comparison.
fn raw_column_repr(column: &Column) -> Vec<String> {
    match column {
        Column::Nothing(c) => vec!["NULL".to_string(); c.len()],
        Column::Int8(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Int16(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Int32(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Int64(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::UInt8(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::UInt16(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::UInt32(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::UInt64(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Float32(c) => c.values.iter().map(|v| v.to_bits().to_string()).collect(),
        Column::Float64(c) => c.values.iter().map(|v| v.to_bits().to_string()).collect(),
        Column::Bool(c) => (0..c.len()).map(|i| c.get(i).to_string()).collect(),
        // Temporal columns are physically primitives; render the raw numeric
        // value (days / seconds / ticks) so the sent-vs-decoded comparison is a
        // straight physical check, the same as the numerics above.
        Column::Date(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Date32(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::DateTime(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::DateTime64(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Time(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Time64(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Interval(c) => c.values.iter().map(|v| v.to_string()).collect(),
        // Enum8/Enum16 are physically the underlying signed int; render the raw
        // value (the name->value map is type metadata, not per-row data).
        Column::Enum8(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Enum16(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Utf8(c) => (0..c.len()).map(|i| format!("{:?}", c.value(i))).collect(),
        Column::AggregateState(c) => (0..c.len()).map(|i| format!("{:?}", c.value(i))).collect(),
        // IPv4 is physically a u32; UUID and IPv6 are raw 16-byte rows, so
        // render the wire bytes verbatim (any reordering would show up here).
        Column::Ipv4(c) => c.values.iter().map(|v| v.to_string()).collect(),
        // UUID, IPv6, the wide integers, and FixedString are all raw fixed-width
        // rows; render the wire bytes verbatim (any reordering/byteswap shows up
        // here). Signedness is type metadata, not per-row data, so the four
        // wide-int variants render identically.
        Column::BFloat16(c) => c.values.iter().map(|v| format!("{v:?}")).collect(),
        Column::Uuid(c)
        | Column::Ipv6(c)
        | Column::FixedBinary(c)
        | Column::Int128(c)
        | Column::UInt128(c)
        | Column::Int256(c)
        | Column::UInt256(c) => (0..c.len()).map(|i| format!("{:?}", c.value(i))).collect(),
        Column::Decimal(c) => (0..c.len()).map(|i| format!("{:?}", c.value(i))).collect(),
        Column::Dictionary(c) => {
            let values = raw_column_repr(c.values.as_ref());
            c.indices
                .iter()
                .map(|&idx| values[idx as usize].clone())
                .collect()
        }
        // Tuple renders each row as the parenthesized joined element values,
        // resolved recursively through the element columns. Element-level
        // validity is not applied here, matching the raw (pre-validity)
        // rendering of the Dictionary and Array arms; the sent and decoded
        // sides render placeholders identically. Tuple() rows render as bare
        // "()" (the placeholder wire byte carries no value).
        Column::Tuple(c) => {
            let element_reprs: Vec<Vec<String>> = c.fields.iter().map(raw_column_repr).collect();
            (0..c.len)
                .map(|row| {
                    let parts: Vec<&str> = element_reprs.iter().map(|e| e[row].as_str()).collect();
                    format!("({})", parts.join(", "))
                })
                .collect()
        }
        // Array renders each row as its bracketed element sub-slice, resolved
        // recursively through the flattened element column. Element-level validity
        // is not applied here, matching the raw (pre-validity) rendering of the
        // Dictionary arm above.
        Column::Array(c) => {
            let elems = raw_column_repr(c.values.as_ref());
            (0..c.len())
                .map(|i| {
                    let start = c.offsets[i] as usize;
                    let end = c.offsets[i + 1] as usize;
                    format!("{:?}", &elems[start..end])
                })
                .collect()
        }
        // Map renders each row as its braced key: value entry sub-slice; the
        // entries column is the two-field keys/values tuple, resolved
        // recursively like the Array arm.
        Column::Map(c) => {
            let entries = raw_column_repr(c.entries.as_ref());
            (0..c.len())
                .map(|i| {
                    let start = c.offsets[i] as usize;
                    let end = c.offsets[i + 1] as usize;
                    format!("{{{}}}", entries[start..end].join(", "))
                })
                .collect()
        }
        // Variant stores dense child columns. Render the selected child and
        // its canonical alternative index so equal physical values in two
        // alternatives remain distinguishable.
        Column::Variant(c) => {
            let variants: Vec<Vec<String>> = c.variants.iter().map(raw_column_repr).collect();
            (0..c.len())
                .map(|row| match c.value_position(row) {
                    Some((u8::MAX, _)) => "NULL".to_string(),
                    Some((variant, offset)) => format!(
                        "Variant({variant}, {})",
                        variants[usize::from(variant)][offset as usize]
                    ),
                    None => "INVALID".to_string(),
                })
                .collect()
        }
        Column::Dynamic(c) => {
            let children = c
                .children
                .iter()
                .map(|child| match child {
                    DynamicChild::Typed { values, .. } => raw_column_repr(values),
                    DynamicChild::Shared(values) => (0..values.len())
                        .map(|row| format!("{:?}", values.value(row)))
                        .collect(),
                })
                .collect::<Vec<Vec<String>>>();
            (0..c.len())
                .map(|row| match c.value_position(row) {
                    Some((u32::MAX, _)) => "NULL".to_string(),
                    Some((child, offset)) => format!(
                        "Dynamic({child}, {})",
                        children[child as usize][offset as usize]
                    ),
                    None => "INVALID".to_string(),
                })
                .collect()
        }
    }
}

/// Gather every value of column `col` across all chunks, in chunk order, as a
/// debug string, so the supported types can be compared uniformly. A null row
/// renders as `"NULL"` regardless of the placeholder value in the buffer, so an
/// inverted or dropped null map is caught, not just wrong values.
fn column_repr(batch: &ch_core_rs::batch::ChunkedBatch, col: usize) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in &batch.chunks {
        let column = chunk.column(col);
        let mut vals = raw_column_repr(column);
        if let Some(validity) = column.validity() {
            for (i, s) in vals.iter_mut().enumerate() {
                if !validity.is_valid(i) {
                    *s = "NULL".to_string();
                }
            }
        }
        out.extend(vals);
    }
    out
}

#[test]
#[ignore = "requires a live ClickHouse server matching .server-ref; run with --ignored"]
fn insert_roundtrips_through_server() {
    let server = Server::from_env();
    let batch = sample_batch();

    // Clean slate, then a Memory table matching the batch's columns and order.
    // The Nullable(Tuple) column needs the enable_nullable_tuple_type DDL gate
    // (a creation-time setting with no wire effect).
    server.ddl(&format!("DROP TABLE IF EXISTS {TABLE}"));
    server.ddl_with_params(
        &format!(
            "CREATE TABLE {TABLE} (\
         i8 Int8, i16 Int16, i32 Int32, i64 Int64, \
         u8 UInt8, u16 UInt16, u32 UInt32, u64 UInt64, \
         f32 Float32, f64 Float64, \
         s String, lc LowCardinality(String), \
         lcn LowCardinality(Nullable(String)), fs FixedString(4), \
         b Bool, \
         d Date, d32 Date32, dt DateTime('UTC'), dt64 DateTime64(3, 'UTC'), \
         u UUID, ip4 IPv4, ip6 IPv6, \
         e8 Enum8('off' = -1, 'idle' = 0, 'busy' = 13), \
         e16 Enum16('off' = -1, 'idle' = 0, 'busy' = 79), \
         dec32 Decimal(9, 4), dec64 Decimal(18, 9), \
         dec128 Decimal(38, 10), dec256 Decimal(76, 20), \
         i128 Int128, u128 UInt128, i256 Int256, u256 UInt256, \
         ni128 Nullable(Int128), lc_i256 LowCardinality(Int256), \
         ni32 Nullable(Int32), ns Nullable(String), nb Nullable(Bool), \
         nu Nullable(UUID), ndec Nullable(Decimal(18, 9)), \
         arr_i32 Array(Int32), arr_ns Array(Nullable(String)), \
         arr_lc Array(LowCardinality(String)), \
         arr_arr Array(Array(Int32)), \
         arr_lc_empty Array(LowCardinality(String)), \
         tup Tuple(Int32, String), \
         tup_named Tuple(a Int32, b Nullable(String)), \
         arr_tup Array(Tuple(Int32, Int32)), \
         ntup Nullable(Tuple(Int32, String)), \
         m Map(String, Int32), \
         m_lc Map(LowCardinality(String), UInt8), \
         m_nv Map(String, Nullable(String)), \
         m_arr Map(String, Array(Int32)), \
         arr_m Array(Map(String, Int32)), \
         m_empty Map(String, Int32), \
         t Time, t64 Time64(6), nt Nullable(Time), \
         nt64 Nullable(Time64(6)), lc_time LowCardinality(Time), \
         iy IntervalYear, iq IntervalQuarter, imo IntervalMonth, iw IntervalWeek, \
         id IntervalDay, ih IntervalHour, imi IntervalMinute, isecond IntervalSecond, \
         ims IntervalMillisecond, ius IntervalMicrosecond, ins IntervalNanosecond, \
         nid Nullable(IntervalDay), lc_ih LowCardinality(IntervalHour), \
         bf BFloat16, nbf Nullable(BFloat16), \
         lc_bf LowCardinality(BFloat16), \
         tn Tuple(Nullable(Nothing)), \
         v Variant(String, UInt64)) ENGINE = Memory"
        ),
        // LowCardinality(Int256) is a suspicious LC inner (a numeric), gated at
        // CREATE time by allow_suspicious_low_cardinality_types (a creation-time
        // setting with no wire effect).
        "?enable_nullable_tuple_type=1&allow_suspicious_low_cardinality_types=1&enable_time_time64_type=1",
    );

    // Encode at revision 0: HTTP INSERT parses the body with server_revision 0,
    // so no BlockInfo preamble and no custom-serialization marker.
    let bytes = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: 0,
            ..EncodeOptions::default()
        },
    )
    .expect("encode numeric batch");
    server.insert_native(&bytes);

    // Read back as Native (HTTP output is revision 0 too) and decode with this
    // crate. ORDER BY i32 is deterministic (i32 is strictly ascending), so the
    // decoded rows line up with the inserted rows.
    let native = server.select(&format!(
        "SELECT i8, i16, i32, i64, u8, u16, u32, u64, f32, f64, s, lc, lcn, fs, b, \
         d, d32, dt, dt64, u, ip4, ip6, e8, e16, \
         dec32, dec64, dec128, dec256, \
         i128, u128, i256, u256, ni128, lc_i256, \
         ni32, ns, nb, nu, ndec, \
         arr_i32, arr_ns, arr_lc, arr_arr, arr_lc_empty, \
         tup, tup_named, arr_tup, ntup, \
         m, m_lc, m_nv, m_arr, arr_m, m_empty, \
         t, t64, nt, nt64, lc_time, \
         iy, iq, imo, iw, id, ih, imi, isecond, ims, ius, ins, nid, lc_ih, \
         bf, nbf, lc_bf, tn, v \
         FROM {TABLE} ORDER BY i32 FORMAT Native"
    ));
    let decoded = decode_all_bytes(
        &native,
        &DecodeOptions {
            protocol_revision: 0,
        },
    )
    .expect("decode server Native response");

    server.ddl(&format!("DROP TABLE IF EXISTS {TABLE}"));

    assert_eq!(decoded.num_rows(), batch.num_rows, "row count from server");
    assert_eq!(
        decoded.num_columns(),
        batch.num_columns(),
        "column count from server"
    );

    // The server round-tripped every value: compare each column against the
    // batch we sent, re-decoded through the same crate for an apples-to-apples
    // physical comparison.
    let sent = single_block(&batch);
    for col in 0..batch.num_columns() {
        assert_eq!(
            column_repr(&decoded, col),
            column_repr(&sent, col),
            "column {col} ({}) differs after server round-trip",
            batch.schema.fields[col].name
        );
    }
}

#[test]
#[ignore = "requires a live ClickHouse server matching .server-ref; run with --ignored"]
fn low_cardinality_fixed_string_u16_dictionary_roundtrips_through_server() {
    let server = Server::from_env();
    let batch = lc_fixed_string_u16_batch();

    server.ddl(&format!("DROP TABLE IF EXISTS {LC_U16_TABLE}"));
    server.ddl(&format!(
        "CREATE TABLE {LC_U16_TABLE} (\
         k UInt16, lc LowCardinality(FixedString(4))) ENGINE = Memory"
    ));

    let bytes = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: 0,
            ..EncodeOptions::default()
        },
    )
    .expect("encode LowCardinality FixedString batch");
    server.insert_native_into(LC_U16_TABLE, &bytes);

    let native = server.select(&format!(
        "SELECT k, lc FROM {LC_U16_TABLE} ORDER BY k FORMAT Native"
    ));
    let decoded = decode_all_bytes(
        &native,
        &DecodeOptions {
            protocol_revision: 0,
        },
    )
    .expect("decode server Native response");

    server.ddl(&format!("DROP TABLE IF EXISTS {LC_U16_TABLE}"));

    assert_eq!(decoded.num_rows(), batch.num_rows, "row count from server");
    assert_eq!(
        decoded.num_columns(),
        batch.num_columns(),
        "column count from server"
    );

    let sent = single_block(&batch);
    for col in 0..batch.num_columns() {
        assert_eq!(
            column_repr(&decoded, col),
            column_repr(&sent, col),
            "column {col} ({}) differs after server round-trip",
            batch.schema.fields[col].name
        );
    }
}

#[test]
#[ignore = "requires a live ClickHouse server matching .server-ref; run with --ignored"]
fn aggregate_function_count_roundtrips_through_server() {
    let server = Server::from_env();
    let batch = ColBatch::new(
        Schema::new(vec![
            Field {
                name: "id".into(),
                ch_type: ChType::UInt8,
            },
            Field {
                name: "c".into(),
                ch_type: ChType::AggregateFunction {
                    function: "count".into(),
                    arguments: vec![],
                },
            },
        ]),
        vec![
            Column::UInt8(PrimitiveColumn::new(vec![0, 1, 2, 3])),
            Column::AggregateState(AggregateStateColumn::new(
                vec![0, 1, 2, 3, 5],
                vec![0x00, 0x01, 0x0d, 0x80, 0x01],
            )),
        ],
        4,
    );

    server.ddl(&format!("DROP TABLE IF EXISTS {AGG_COUNT_TABLE}"));
    server.ddl(&format!(
        "CREATE TABLE {AGG_COUNT_TABLE} (id UInt8, c AggregateFunction(count)) ENGINE = Memory"
    ));

    let bytes = encode_block(&batch, &EncodeOptions::default()).expect("encode count states");
    server.insert_native_into(AGG_COUNT_TABLE, &bytes);

    // Finalize on the server so this checks the raw states were accepted and
    // interpreted as counts, not merely replayed as opaque bytes.
    let native = server.select(&format!(
        "SELECT id, finalizeAggregation(c) AS count FROM {AGG_COUNT_TABLE} ORDER BY id FORMAT Native"
    ));
    let decoded = decode_all_bytes(&native, &DecodeOptions::default())
        .expect("decode finalized count states");
    server.ddl(&format!("DROP TABLE IF EXISTS {AGG_COUNT_TABLE}"));

    assert_eq!(decoded.num_rows(), 4);
    match decoded.chunks[0].column(1) {
        Column::UInt64(c) => assert_eq!(c.values, vec![0, 1, 13, 128]),
        other => panic!("expected finalized UInt64 counts, got {other:?}"),
    }
}

#[test]
#[ignore = "requires a live ClickHouse server matching .server-ref; run with --ignored"]
fn aggregate_function_fixed_zero_states_roundtrip_through_server() {
    let server = Server::from_env();
    let batch = ColBatch::new(
        Schema::new(vec![
            Field {
                name: "id".into(),
                ch_type: ChType::UInt8,
            },
            Field {
                name: "c".into(),
                ch_type: ChType::AggregateFunction {
                    function: "nothingUInt64".into(),
                    arguments: vec![ChType::Nullable(Box::new(ChType::Nothing))],
                },
            },
            Field {
                name: "cn".into(),
                ch_type: ChType::AggregateFunction {
                    function: "nothingNull".into(),
                    arguments: vec![ChType::Nullable(Box::new(ChType::Nothing))],
                },
            },
        ]),
        vec![
            Column::UInt8(PrimitiveColumn::new(vec![0, 1, 2])),
            // One 0x00 placeholder byte per row; the server rejects any nonzero
            // byte as INCORRECT_DATA on read.
            Column::AggregateState(AggregateStateColumn::new(
                vec![0, 1, 2, 3],
                vec![0x00, 0x00, 0x00],
            )),
            Column::AggregateState(AggregateStateColumn::new(
                vec![0, 1, 2, 3],
                vec![0x00, 0x00, 0x00],
            )),
        ],
        3,
    );

    server.ddl(&format!("DROP TABLE IF EXISTS {AGG_NOTHING_TABLE}"));
    server.ddl(&format!(
        "CREATE TABLE {AGG_NOTHING_TABLE} \
         (id UInt8, \
         c AggregateFunction(nothingUInt64, Nullable(Nothing)), \
         cn AggregateFunction(nothingNull, Nullable(Nothing))) ENGINE = Memory"
    ));

    let bytes = encode_block(&batch, &EncodeOptions::default())
        .expect("encode fixed-zero aggregate states");
    server.insert_native_into(AGG_NOTHING_TABLE, &bytes);

    // Finalize on the server so this checks the raw states were accepted and
    // interpreted as an only-null count and sum, not merely replayed as opaque
    // bytes. nothingUInt64 finalizes to 0; nothingNull finalizes to NULL.
    let native = server.select(&format!(
        "SELECT id, finalizeAggregation(c) AS count, \
         isNull(finalizeAggregation(cn)) AS is_null \
         FROM {AGG_NOTHING_TABLE} ORDER BY id FORMAT Native"
    ));
    let decoded = decode_all_bytes(&native, &DecodeOptions::default())
        .expect("decode finalized fixed-zero states");
    server.ddl(&format!("DROP TABLE IF EXISTS {AGG_NOTHING_TABLE}"));

    assert_eq!(decoded.num_rows(), 3);
    match decoded.chunks[0].column(1) {
        Column::UInt64(c) => assert_eq!(c.values, vec![0, 0, 0]),
        other => panic!("expected finalized UInt64 counts, got {other:?}"),
    }
    match decoded.chunks[0].column(2) {
        Column::UInt8(c) => assert_eq!(c.values, vec![1, 1, 1]),
        other => panic!("expected nothingNull finalization markers, got {other:?}"),
    }
}

#[test]
#[ignore = "requires a live ClickHouse server matching .server-ref; run with --ignored"]
fn aggregate_function_sum_roundtrips_through_server() {
    let server = Server::from_env();

    let mut int_states = Vec::new();
    let mut decimal_states = Vec::new();
    let mut wide_states = Vec::new();
    let mut enum_states = Vec::new();
    let mut nullable_int_states = vec![0x00, 0x01];
    nullable_int_states.extend_from_slice(&(-13i64).to_le_bytes());
    nullable_int_states.push(0x01);
    nullable_int_states.extend_from_slice(&79i64.to_le_bytes());
    for ((int_value, decimal_value), (wide_value, enum_value)) in
        [(-13i64, 1300i128), (0, 0), (79, -7900)].into_iter().zip([
            (13u64, 0i64),
            (79, 4),
            (258, 14),
        ])
    {
        int_states.extend_from_slice(&int_value.to_le_bytes());
        decimal_states.extend_from_slice(&decimal_value.to_le_bytes());
        wide_states.extend_from_slice(&wide_value.to_le_bytes());
        wide_states.extend_from_slice(&[0u8; 24]);
        enum_states.extend_from_slice(&enum_value.to_le_bytes());
    }

    let batch = ColBatch::new(
        Schema::new(vec![
            Field {
                name: "id".into(),
                ch_type: ChType::UInt8,
            },
            Field {
                name: "i".into(),
                ch_type: ChType::AggregateFunction {
                    function: "sum".into(),
                    arguments: vec![ChType::Int32],
                },
            },
            Field {
                name: "ni".into(),
                ch_type: ChType::AggregateFunction {
                    function: "sum".into(),
                    arguments: vec![ChType::Nullable(Box::new(ChType::Int32))],
                },
            },
            Field {
                name: "d".into(),
                ch_type: ChType::AggregateFunction {
                    function: "sum".into(),
                    arguments: vec![ChType::Decimal {
                        precision: 9,
                        scale: 2,
                        bits: 32,
                    }],
                },
            },
            Field {
                name: "w".into(),
                ch_type: ChType::AggregateFunction {
                    function: "sum".into(),
                    arguments: vec![ChType::UInt256],
                },
            },
            Field {
                name: "e".into(),
                ch_type: ChType::AggregateFunction {
                    function: "sum".into(),
                    arguments: vec![ChType::Enum8 {
                        variants: vec![("debit".into(), -3), ("credit".into(), 7)],
                    }],
                },
            },
        ]),
        vec![
            Column::UInt8(PrimitiveColumn::new(vec![0, 1, 2])),
            Column::AggregateState(AggregateStateColumn::new(vec![0, 8, 16, 24], int_states)),
            Column::AggregateState(AggregateStateColumn::new(
                vec![0, 1, 10, 19],
                nullable_int_states,
            )),
            Column::AggregateState(AggregateStateColumn::new(
                vec![0, 16, 32, 48],
                decimal_states,
            )),
            Column::AggregateState(AggregateStateColumn::new(vec![0, 32, 64, 96], wide_states)),
            Column::AggregateState(AggregateStateColumn::new(vec![0, 8, 16, 24], enum_states)),
        ],
        3,
    );

    server.ddl(&format!("DROP TABLE IF EXISTS {AGG_SUM_TABLE}"));
    server.ddl(&format!(
        "CREATE TABLE {AGG_SUM_TABLE} \
         (id UInt8, i AggregateFunction(sum, Int32), \
         ni AggregateFunction(sum, Nullable(Int32)), \
         d AggregateFunction(sum, Decimal(9, 2)), \
         w AggregateFunction(sum, UInt256), \
         e AggregateFunction(sum, Enum8('debit' = -3, 'credit' = 7))) ENGINE = Memory"
    ));

    let bytes = encode_block(&batch, &EncodeOptions::default()).expect("encode sum states");
    server.insert_native_into(AGG_SUM_TABLE, &bytes);

    let native = server.select(&format!(
        "SELECT id, finalizeAggregation(i), finalizeAggregation(ni), \
         finalizeAggregation(d), \
         finalizeAggregation(w), finalizeAggregation(e) \
         FROM {AGG_SUM_TABLE} ORDER BY id FORMAT Native"
    ));
    let decoded =
        decode_all_bytes(&native, &DecodeOptions::default()).expect("decode finalized sum states");
    server.ddl(&format!("DROP TABLE IF EXISTS {AGG_SUM_TABLE}"));

    assert_eq!(decoded.num_rows(), 3);
    let block = &decoded.chunks[0];
    match block.column(1) {
        Column::Int64(c) => assert_eq!(c.values, vec![-13, 0, 79]),
        other => panic!("expected finalized Int64 sums, got {other:?}"),
    }
    match block.column(2) {
        Column::Int64(c) => {
            let validity = c.validity.as_ref().expect("nullable finalized sum");
            assert_eq!(validity.null_count(), 1);
            assert!(!validity.is_valid(0));
            assert!(validity.is_valid(1));
            assert!(validity.is_valid(2));
            assert_eq!(&c.values[1..], &[-13, 79]);
        }
        other => panic!("expected finalized Nullable(Int64) sum, got {other:?}"),
    }
    match block.column(3) {
        Column::Decimal(c) => {
            assert_eq!((c.precision, c.scale, c.width), (38, 2, 16));
            let expected: Vec<u8> = [1300i128, 0, -7900]
                .into_iter()
                .flat_map(i128::to_le_bytes)
                .collect();
            assert_eq!(c.data, expected);
        }
        other => panic!("expected finalized Decimal sums, got {other:?}"),
    }
    match block.column(4) {
        Column::UInt256(c) => {
            assert_eq!(c.width, 32);
            let expected: Vec<u8> = [13u64, 79, 258]
                .into_iter()
                .flat_map(|value| {
                    let mut bytes = [0u8; 32];
                    bytes[..8].copy_from_slice(&value.to_le_bytes());
                    bytes
                })
                .collect();
            assert_eq!(c.data, expected);
        }
        other => panic!("expected finalized UInt256 sums, got {other:?}"),
    }
    match block.column(5) {
        Column::Int64(c) => assert_eq!(c.values, vec![0, 4, 14]),
        other => panic!("expected finalized Enum Int64 sums, got {other:?}"),
    }
}

#[test]
#[ignore = "requires a live ClickHouse server matching .server-ref; run with --ignored"]
fn dynamic_roundtrips_through_server() {
    let server = Server::from_env();
    let batch = dynamic_batch();

    server.ddl(&format!("DROP TABLE IF EXISTS {DYNAMIC_TABLE}"));
    server.ddl(&format!(
        "CREATE TABLE {DYNAMIC_TABLE} (id UInt8, v Dynamic(max_types=1)) ENGINE = Memory"
    ));
    let bytes = encode_block(&batch, &EncodeOptions::default()).expect("encode Dynamic batch");
    server.insert_native_into(DYNAMIC_TABLE, &bytes);

    let native = server.select(&format!(
        "SELECT id, v FROM {DYNAMIC_TABLE} ORDER BY id FORMAT Native"
    ));
    let decoded = decode_all_bytes(&native, &DecodeOptions::default())
        .expect("decode server Dynamic response");
    server.ddl(&format!("DROP TABLE IF EXISTS {DYNAMIC_TABLE}"));

    let sent = single_block(&batch);
    assert_eq!(column_repr(&decoded, 0), column_repr(&sent, 0));
    assert_eq!(column_repr(&decoded, 1), column_repr(&sent, 1));
}

#[test]
#[ignore = "requires a live ClickHouse server matching .server-ref; run with --ignored"]
fn dynamic_binary_type_headers_roundtrip_through_server() {
    let server = Server::from_env();
    let batch = dynamic_batch();

    server.ddl(&format!("DROP TABLE IF EXISTS {DYNAMIC_BINARY_TABLE}"));
    server.ddl(&format!(
        "CREATE TABLE {DYNAMIC_BINARY_TABLE} (id UInt8, v Dynamic(max_types=1)) ENGINE = Memory"
    ));
    let bytes = encode_block_binary_types(&batch, &EncodeOptions::default())
        .expect("encode Dynamic batch with binary type headers");
    server.insert_native_binary_types_into(DYNAMIC_BINARY_TABLE, &bytes);

    let native = server.select_with_params(
        &format!("SELECT id, v FROM {DYNAMIC_BINARY_TABLE} ORDER BY id FORMAT Native"),
        "?output_format_native_encode_types_in_binary_format=1",
    );
    let decoded = decode_all_bytes_binary_types(&native, &DecodeOptions::default())
        .expect("decode server Dynamic binary-type response");
    server.ddl(&format!("DROP TABLE IF EXISTS {DYNAMIC_BINARY_TABLE}"));

    let sent = single_block(&batch);
    assert_eq!(column_repr(&decoded, 0), column_repr(&sent, 0));
    assert_eq!(column_repr(&decoded, 1), column_repr(&sent, 1));
}

#[test]
#[ignore = "requires a live ClickHouse server matching .server-ref; run with --ignored"]
fn geo_saf_nested_roundtrip_through_server() {
    let server = Server::from_env();
    let batch = geo_saf_nested_batch();

    server.ddl(&format!("DROP TABLE IF EXISTS {GSN_TABLE}"));
    // The Nested column needs flatten_nested = 0 so the literal Nested type is the
    // column type (not flattened into nst.x / nst.y siblings); Nullable(Point)
    // needs enable_nullable_tuple_type. Both are creation-time gates with no wire
    // effect. allow_suspicious_low_cardinality_types is harmless here (the LC
    // inner is a plain String) and kept for symmetry with the other tests.
    server.ddl_with_params(
        &format!(
            "CREATE TABLE {GSN_TABLE} (\
         i32 Int32, \
         saf_sum SimpleAggregateFunction(sum, Float64), \
         saf_lc SimpleAggregateFunction(anyLast, LowCardinality(Nullable(String))), \
         point Point, npoint Nullable(Point), ring Ring, mpoly MultiPolygon, \
         nst Nested(x UInt32, y String), \
         nsaf Nullable(SimpleAggregateFunction(sum, UInt64)), \
         tsaf Tuple(v SimpleAggregateFunction(sum, UInt64)), \
         lc_nsaf LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))) ENGINE = Memory"
        ),
        "?flatten_nested=0&enable_nullable_tuple_type=1&allow_suspicious_low_cardinality_types=1",
    );

    let bytes = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: 0,
            ..EncodeOptions::default()
        },
    )
    .expect("encode geo/SAF/Nested batch");
    server.insert_native_into(GSN_TABLE, &bytes);

    let native = server.select(&format!(
        "SELECT i32, saf_sum, saf_lc, point, npoint, ring, mpoly, nst, nsaf, tsaf, lc_nsaf \
         FROM {GSN_TABLE} ORDER BY i32 FORMAT Native"
    ));
    let decoded = decode_all_bytes(
        &native,
        &DecodeOptions {
            protocol_revision: 0,
        },
    )
    .expect("decode server Native response");

    server.ddl(&format!("DROP TABLE IF EXISTS {GSN_TABLE}"));

    assert_eq!(decoded.num_rows(), batch.num_rows, "row count from server");
    assert_eq!(
        decoded.num_columns(),
        batch.num_columns(),
        "column count from server"
    );

    let sent = single_block(&batch);
    for col in 0..batch.num_columns() {
        assert_eq!(
            column_repr(&decoded, col),
            column_repr(&sent, col),
            "column {col} ({}) differs after server round-trip",
            batch.schema.fields[col].name
        );
    }
}

/// Wrap a `ColBatch` as a one-chunk `ChunkedBatch` so `column_repr` can read the
/// sent values with the same code path as the decoded response.
fn single_block(batch: &ColBatch) -> ch_core_rs::batch::ChunkedBatch {
    ch_core_rs::batch::ChunkedBatch {
        schema: batch.schema.clone(),
        chunks: vec![std::sync::Arc::new(batch.clone())],
    }
}
