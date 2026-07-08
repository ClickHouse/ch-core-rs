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
    ArrayColumn, BoolColumn, Column, DecimalColumn, DictionaryColumn, FixedBinaryColumn, MapColumn,
    PrimitiveColumn, TupleColumn, Utf8Column,
};
use ch_core_rs::native::decode::{decode_all_bytes, DecodeOptions};
use ch_core_rs::native::encode::{encode_block, EncodeOptions};
use ch_core_rs::schema::{ChType, Field, Schema};

const TABLE: &str = "ch_core_rs_encode_test";
const LC_U16_TABLE: &str = "ch_core_rs_encode_lc_u16_test";

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

/// Build a `FixedBinaryColumn` of the given width from equal-width byte values.
fn fixed_binary_column(width: usize, values: &[&[u8]]) -> FixedBinaryColumn {
    let mut data = Vec::with_capacity(width * values.len());
    for v in values {
        assert_eq!(v.len(), width, "fixed-string value must be {width} bytes");
        data.extend_from_slice(v);
    }
    FixedBinaryColumn::new(data, width)
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
/// numerics, `String`, `FixedString(4)`, `Bool`, the four temporal types,
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

    /// Run a SELECT and return the raw response body bytes. The query is the raw
    /// POST body, so a `FORMAT Native` response comes back as binary.
    fn select(&self, sql: &str) -> Vec<u8> {
        let url = self.base_url.clone();
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
        // Enum8/Enum16 are physically the underlying signed int; render the raw
        // value (the name->value map is type metadata, not per-row data).
        Column::Enum8(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Enum16(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Utf8(c) => (0..c.len()).map(|i| format!("{:?}", c.value(i))).collect(),
        // IPv4 is physically a u32; UUID and IPv6 are raw 16-byte rows, so
        // render the wire bytes verbatim (any reordering would show up here).
        Column::Ipv4(c) => c.values.iter().map(|v| v.to_string()).collect(),
        Column::Uuid(c) | Column::Ipv6(c) | Column::FixedBinary(c) => {
            (0..c.len()).map(|i| format!("{:?}", c.value(i))).collect()
        }
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
         m_empty Map(String, Int32)) ENGINE = Memory"
        ),
        "?enable_nullable_tuple_type=1",
    );

    // Encode at revision 0: HTTP INSERT parses the body with server_revision 0,
    // so no BlockInfo preamble and no custom-serialization marker.
    let bytes = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: 0,
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
         dec32, dec64, dec128, dec256, ni32, ns, nb, nu, ndec, \
         arr_i32, arr_ns, arr_lc, arr_arr, arr_lc_empty, \
         tup, tup_named, arr_tup, ntup, \
         m, m_lc, m_nv, m_arr, arr_m, m_empty \
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

/// Wrap a `ColBatch` as a one-chunk `ChunkedBatch` so `column_repr` can read the
/// sent values with the same code path as the decoded response.
fn single_block(batch: &ColBatch) -> ch_core_rs::batch::ChunkedBatch {
    ch_core_rs::batch::ChunkedBatch {
        schema: batch.schema.clone(),
        chunks: vec![std::sync::Arc::new(batch.clone())],
    }
}
