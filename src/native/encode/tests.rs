use super::*;
use crate::bitmap::Bitmap;
use crate::column::{DecimalColumn, DictionaryColumn, PrimitiveColumn};
use crate::native::decode::{decode_all_bytes, DecodeOptions, DBMS_TCP_PROTOCOL_VERSION};
use crate::schema::{GeoKind, Schema};

/// Build a `Utf8Column` from raw byte values, computing the Arrow offsets the
/// same way the decoder does (starting at 0, one entry past each value).
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

/// Build a `FixedBinaryColumn` of the given width from equal-width byte
/// values, concatenated into the contiguous data buffer.
fn fixed_binary_column(width: usize, values: &[&[u8]]) -> FixedBinaryColumn {
    let mut data = Vec::with_capacity(width * values.len());
    for v in values {
        assert_eq!(
            v.len(),
            width,
            "fixed-string test value must be {width} bytes"
        );
        data.extend_from_slice(v);
    }
    FixedBinaryColumn::new(data, width)
}

/// A `String` column and a `FixedString(4)` column over four rows. The string
/// values include an empty string and a value longer than the fixed width to
/// exercise the varint length framing; the fixed-string values include an
/// all-zero row and a zero-padded row.
fn string_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "s".into(),
            ch_type: ChType::String,
        },
        Field {
            name: "fs".into(),
            ch_type: ChType::FixedString(4),
        },
    ];
    let columns = vec![
        Column::Utf8(utf8_column(&[b"user_1", b"", b"n", b"user_2_longer"])),
        Column::FixedBinary(fixed_binary_column(
            4,
            &[b"road", b"1234", b"\x00\x00\x00\x00", b"n\x00\x00\x00"],
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// All ten fixed-width numeric columns over four rows, one batch. Values pick
/// each type's extremes plus a couple of neutral in-range values.
fn numeric_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "i8".into(),
            ch_type: ChType::Int8,
        },
        Field {
            name: "i16".into(),
            ch_type: ChType::Int16,
        },
        Field {
            name: "i32".into(),
            ch_type: ChType::Int32,
        },
        Field {
            name: "i64".into(),
            ch_type: ChType::Int64,
        },
        Field {
            name: "u8".into(),
            ch_type: ChType::UInt8,
        },
        Field {
            name: "u16".into(),
            ch_type: ChType::UInt16,
        },
        Field {
            name: "u32".into(),
            ch_type: ChType::UInt32,
        },
        Field {
            name: "u64".into(),
            ch_type: ChType::UInt64,
        },
        Field {
            name: "f32".into(),
            ch_type: ChType::Float32,
        },
        Field {
            name: "f64".into(),
            ch_type: ChType::Float64,
        },
    ];
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
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// A single `Bool` column over five rows (a non-multiple of 8 so the packed
/// bitmap's trailing partial byte is exercised).
fn bool_batch() -> ColBatch {
    let fields = vec![Field {
        name: "b".into(),
        ch_type: ChType::Bool,
    }];
    let columns = vec![Column::Bool(BoolColumn::from_wire_bytes(&[1, 0, 1, 1, 0]))];
    ColBatch::new(Schema::new(fields), columns, 5)
}

/// A `Nullable` numeric, string, and bool over four rows. The null pattern is
/// valid, null, valid, null, so the null map exercises both states and the
/// inner-value buffers still carry a (placeholder) value for the null rows.
fn nullable_batch() -> ColBatch {
    // 0x00 = valid, 0x01 = null (ClickHouse null-map polarity).
    let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let fields = vec![
        Field {
            name: "ni32".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Int32)),
        },
        Field {
            name: "ns".into(),
            ch_type: ChType::Nullable(Box::new(ChType::String)),
        },
        Field {
            name: "nb".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Bool)),
        },
    ];
    let mut ns = utf8_column(&[b"user_1", b"", b"user_2", b""]);
    ns.validity = Some(validity());
    let columns = vec![
        Column::Int32(PrimitiveColumn::new_nullable(
            vec![13, 0, 79, 0],
            validity(),
        )),
        Column::Utf8(ns),
        Column::Bool(BoolColumn::from_wire_bytes_nullable(
            &[1, 0, 1, 0],
            validity(),
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// The six temporal columns over four rows. Date/time metadata is carried in
/// the type string only; the bodies are faithful primitive-width integers.
/// Signed types include negative values to prove little-endian round-trips.
fn temporal_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "d".into(),
            ch_type: ChType::Date,
        },
        Field {
            name: "d32".into(),
            ch_type: ChType::Date32,
        },
        Field {
            name: "dt".into(),
            ch_type: ChType::DateTime {
                timezone: Some("UTC".into()),
            },
        },
        Field {
            name: "dt64".into(),
            ch_type: ChType::DateTime64 {
                precision: 3,
                timezone: Some("UTC".into()),
            },
        },
        Field {
            name: "t".into(),
            ch_type: ChType::Time,
        },
        Field {
            name: "t64".into(),
            ch_type: ChType::Time64 { precision: 3 },
        },
    ];
    let columns = vec![
        Column::Date(PrimitiveColumn::new(vec![0, 19000, 19001, u16::MAX])),
        Column::Date32(PrimitiveColumn::new(vec![i32::MIN, -25567, 0, i32::MAX])),
        Column::DateTime(PrimitiveColumn::new(vec![
            0,
            1_600_000_000,
            1_700_000_000,
            u32::MAX,
        ])),
        Column::DateTime64(PrimitiveColumn::new(vec![
            i64::MIN,
            -1_000,
            1_700_000_000_000,
            i64::MAX,
        ])),
        Column::Time(PrimitiveColumn::new(vec![-3_599_999, -13, 0, 3_599_999])),
        Column::Time64(PrimitiveColumn::new(vec![
            -3_599_999_999,
            -13_000,
            0,
            3_599_999_999,
        ])),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// Nullable DateTime64, Time, and Time64 columns over four rows with the
/// valid, null, valid, null pattern.
fn nullable_temporal_batch() -> ColBatch {
    let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let fields = vec![
        Field {
            name: "ndt64".into(),
            ch_type: ChType::Nullable(Box::new(ChType::DateTime64 {
                precision: 3,
                timezone: Some("UTC".into()),
            })),
        },
        Field {
            name: "nt".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Time)),
        },
        Field {
            name: "nt64".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Time64 { precision: 6 })),
        },
    ];
    let columns = vec![
        Column::DateTime64(PrimitiveColumn::new_nullable(
            vec![1_700_000_000_000, 0, -1_000, 0],
            validity(),
        )),
        Column::Time(PrimitiveColumn::new_nullable(
            vec![-13, 0, 79, 0],
            validity(),
        )),
        Column::Time64(PrimitiveColumn::new_nullable(
            vec![-13_000_000, 0, 79_000_000, 0],
            validity(),
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// A `UUID`, `IPv4`, and `IPv6` column over four rows. The UUID and IPv6
/// values are distinct 16-byte patterns (all-zero, an ascending run, a
/// constant, all-0xFF) that must survive verbatim with no reordering; the
/// IPv4 values hit the u32 boundaries plus two real addresses.
fn uuid_ip_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "u".into(),
            ch_type: ChType::Uuid,
        },
        Field {
            name: "ip4".into(),
            ch_type: ChType::Ipv4,
        },
        Field {
            name: "ip6".into(),
            ch_type: ChType::Ipv6,
        },
    ];
    let columns = vec![
        Column::Uuid(fixed_binary_column(
            16,
            &[
                &[0u8; 16],
                b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x0C\x0D\x0E\x0F\x10",
                &[0x79; 16],
                &[0xFF; 16],
            ],
        )),
        // 0.0.0.0, 127.0.0.1, 192.168.0.1, 255.255.255.255 as the standard
        // numeric value (a<<24 | b<<16 | c<<8 | d).
        Column::Ipv4(PrimitiveColumn::new(vec![
            0,
            2_130_706_433,
            3_232_235_521,
            u32::MAX,
        ])),
        // ::, ::1, 2001:db8::13, all-0xFF, in network byte order.
        Column::Ipv6(fixed_binary_column(
            16,
            &[
                &[0u8; 16],
                b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01",
                b"\x20\x01\x0D\xB8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x13",
                &[0xFF; 16],
            ],
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// `Nullable(UUID)`, `Nullable(IPv4)`, and `Nullable(IPv6)` over four rows
/// with the valid, null, valid, null pattern, proving the `Nullable` wrapper
/// composes with all three: the null map precedes the inner body.
fn nullable_uuid_ip_batch() -> ColBatch {
    // 0x00 = valid, 0x01 = null (ClickHouse null-map polarity).
    let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let fields = vec![
        Field {
            name: "nu".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Uuid)),
        },
        Field {
            name: "nip4".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Ipv4)),
        },
        Field {
            name: "nip6".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Ipv6)),
        },
    ];
    let mut nu = fixed_binary_column(16, &[&[0x13; 16], &[0u8; 16], &[0x79; 16], &[0u8; 16]]);
    nu.validity = Some(validity());
    let mut nip6 = fixed_binary_column(
        16,
        &[
            b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01",
            &[0u8; 16],
            b"\x20\x01\x0D\xB8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x13",
            &[0u8; 16],
        ],
    );
    nip6.validity = Some(validity());
    let columns = vec![
        Column::Uuid(nu),
        Column::Ipv4(PrimitiveColumn::new_nullable(
            vec![2_130_706_433, 0, 3_232_235_521, 0],
            validity(),
        )),
        Column::Ipv6(nip6),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// An `Enum8` and an `Enum16` column over four rows. The variant lists carry
/// distinct signed values in the server's ascending-by-value order, including
/// a negative variant and each width's boundary (`i8::MIN`/`i8::MAX`,
/// `i16::MIN`/`i16::MAX`), and the physical buffers pick those boundary and
/// negative values so the little-endian byte order and sign of the underlying
/// int are exercised. The name->value map lives only in the type string
/// (`ChType::Display`), so this proves that string round-trips too.
fn enum_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "e8".into(),
            ch_type: ChType::Enum8 {
                variants: vec![
                    ("floor".into(), i8::MIN),
                    ("neg".into(), -13),
                    ("idle".into(), 0),
                    ("busy".into(), 13),
                    ("ceil".into(), i8::MAX),
                ],
            },
        },
        Field {
            name: "e16".into(),
            ch_type: ChType::Enum16 {
                variants: vec![
                    ("floor".into(), i16::MIN),
                    ("neg".into(), -79),
                    ("idle".into(), 0),
                    ("busy".into(), 79),
                    ("ceil".into(), i16::MAX),
                ],
            },
        },
    ];
    let columns = vec![
        Column::Enum8(PrimitiveColumn::new(vec![i8::MIN, -13, 0, i8::MAX])),
        Column::Enum16(PrimitiveColumn::new(vec![i16::MIN, -79, 0, i16::MAX])),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// A `Nullable(Enum8(...))` and a `Nullable(Enum16(...))` column over four
/// rows with the valid, null, valid, null pattern, proving the `Nullable`
/// wrapper composes with an enum inner: the null map precedes the inner
/// signed-int values.
fn nullable_enum_batch() -> ColBatch {
    // 0x00 = valid, 0x01 = null (ClickHouse null-map polarity).
    let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let fields = vec![
        Field {
            name: "ne8".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Enum8 {
                variants: vec![("neg".into(), -13), ("idle".into(), 0), ("busy".into(), 13)],
            })),
        },
        Field {
            name: "ne16".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Enum16 {
                variants: vec![("neg".into(), -79), ("idle".into(), 0), ("busy".into(), 79)],
            })),
        },
    ];
    let columns = vec![
        Column::Enum8(PrimitiveColumn::new_nullable(
            vec![-13, 0, 13, 0],
            validity(),
        )),
        Column::Enum16(PrimitiveColumn::new_nullable(
            vec![-79, 0, 79, 0],
            validity(),
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// Build a DecimalColumn of the given width from equal-width raw wire-order
/// byte values.
fn decimal_column(width: usize, precision: u8, scale: u8, values: &[&[u8]]) -> DecimalColumn {
    let mut data = Vec::with_capacity(width * values.len());
    for v in values {
        assert_eq!(v.len(), width, "decimal test value must be {width} bytes");
        data.extend_from_slice(v);
    }
    DecimalColumn::new(data, width, precision, scale)
}

/// Decimal columns covering all four precision-derived widths. The raw bytes
/// include positive, zero, and negative two's-complement values, but the core
/// treats them as already-wire-order bytes and does not materialize integers.
fn decimal_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "d32".into(),
            ch_type: ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            },
        },
        Field {
            name: "d64".into(),
            ch_type: ChType::Decimal {
                precision: 18,
                scale: 9,
                bits: 64,
            },
        },
        Field {
            name: "d128".into(),
            ch_type: ChType::Decimal {
                precision: 38,
                scale: 10,
                bits: 128,
            },
        },
        Field {
            name: "d256".into(),
            ch_type: ChType::Decimal {
                precision: 76,
                scale: 20,
                bits: 256,
            },
        },
    ];
    let d32_neg = (-13i32).to_le_bytes();
    let d32_pos = 79i32.to_le_bytes();
    let d64_neg = (-13i64).to_le_bytes();
    let d64_pos = 79i64.to_le_bytes();
    let d64_zero = [0u8; 8];
    let d128_neg = [0xFFu8; 16];
    let d128_zero = [0u8; 16];
    let d256_neg = [0xFFu8; 32];
    let d256_zero = [0u8; 32];
    let columns = vec![
        Column::Decimal(decimal_column(
            4,
            9,
            4,
            &[&d32_neg, &[0, 0, 0, 0], &d32_pos],
        )),
        Column::Decimal(decimal_column(
            8,
            18,
            9,
            &[&d64_neg, &d64_zero, &d64_pos],
        )),
        Column::Decimal(decimal_column(
            16,
            38,
            10,
            &[
                &d128_neg,
                &d128_zero,
                b"\x4F\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
            ],
        )),
        Column::Decimal(decimal_column(
            32,
            76,
            20,
            &[
                &d256_neg,
                &d256_zero,
                b"\x13\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
            ],
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// A `Nullable(Decimal(18, 9))` column with valid, null, valid, null rows.
fn nullable_decimal_batch() -> ColBatch {
    let validity = Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let neg = (-13i64).to_le_bytes();
    let zero = [0u8; 8];
    let pos = 79i64.to_le_bytes();
    let values = [&neg[..], &zero[..], &pos[..], &zero[..]];
    let mut col = decimal_column(8, 18, 9, &values);
    col.validity = Some(validity);
    ColBatch::new(
        Schema::new(vec![Field {
            name: "nd".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Decimal {
                precision: 18,
                scale: 9,
                bits: 64,
            })),
        }]),
        vec![Column::Decimal(col)],
        4,
    )
}

/// Build a wide-int `FixedBinaryColumn` of the given width (16 or 32) from
/// equal-width raw wire-order byte values. Physically a FixedBinaryColumn, so
/// this delegates to `fixed_binary_column`.
fn wide_int_column(width: usize, values: &[&[u8]]) -> FixedBinaryColumn {
    fixed_binary_column(width, values)
}

/// All four wide-int types over three rows each, with sign and high-bit
/// boundary values so any accidental sign/endianness/reorder bug is caught:
/// the signed types include -1 (all 0xFF) and the width MIN (MSB-only); the
/// unsigned types include a high-bit-set value and the all-0xFF max. The core
/// stores the raw wire bytes verbatim, so these are already wire-order.
fn wide_int_batch() -> ColBatch {
    let mut i128_min = [0u8; 16];
    i128_min[15] = 0x80;
    let mut u128_high = [0u8; 16];
    u128_high[15] = 0x80;
    let mut w16_13 = [0u8; 16];
    w16_13[0] = 13;
    let mut w16_79 = [0u8; 16];
    w16_79[0] = 79;

    let mut i256_min = [0u8; 32];
    i256_min[31] = 0x80;
    let mut u256_high = [0u8; 32];
    u256_high[31] = 0x80;
    let mut w32_13 = [0u8; 32];
    w32_13[0] = 13;
    let mut w32_79 = [0u8; 32];
    w32_79[0] = 79;

    let fields = vec![
        Field {
            name: "i128".into(),
            ch_type: ChType::Int128,
        },
        Field {
            name: "u128".into(),
            ch_type: ChType::UInt128,
        },
        Field {
            name: "i256".into(),
            ch_type: ChType::Int256,
        },
        Field {
            name: "u256".into(),
            ch_type: ChType::UInt256,
        },
    ];
    let columns = vec![
        Column::Int128(wide_int_column(16, &[&w16_13, &[0xFFu8; 16], &i128_min])),
        Column::UInt128(wide_int_column(16, &[&w16_79, &u128_high, &[0xFFu8; 16]])),
        Column::Int256(wide_int_column(32, &[&w32_13, &[0xFFu8; 32], &i256_min])),
        Column::UInt256(wide_int_column(32, &[&w32_79, &u256_high, &[0xFFu8; 32]])),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// A `Nullable(Int128)` column with valid, null, valid, null rows.
fn nullable_wide_int_batch() -> ColBatch {
    let validity = Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let mut thirteen = [0u8; 16];
    thirteen[0] = 13;
    let mut col = wide_int_column(16, &[&thirteen, &[0u8; 16], &[0xFFu8; 16], &[0u8; 16]]);
    col.validity = Some(validity);
    ColBatch::new(
        Schema::new(vec![Field {
            name: "nw".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Int128)),
        }]),
        vec![Column::Int128(col)],
        4,
    )
}

/// A `LowCardinality(Int256)` column, so the encode LC path is exercised for
/// a wide-int inner. Dictionary slot 0 is the reserved default, real rows
/// reference slots 1...
fn low_cardinality_wide_int_batch() -> ColBatch {
    let mut thirteen = [0u8; 32];
    thirteen[0] = 13;
    let mut seventy_nine = [0u8; 32];
    seventy_nine[0] = 79;
    ColBatch::new(
        Schema::new(vec![Field {
            name: "lc_i256".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Int256)),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1],
            Column::Int256(wide_int_column(32, &[&[0u8; 32], &thirteen, &seventy_nine])),
        ))],
        3,
    )
}

/// Plain LowCardinality String, UInt32, and Time columns over four
/// rows. The dictionary includes the server's reserved default slot 0 and
/// rows reference real values in slots 1.., matching server-produced Native
/// blocks while still exercising the dictionary/index writer.
fn low_cardinality_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::String)),
        },
        Field {
            name: "lc_u32".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::UInt32)),
        },
        Field {
            name: "lc_time".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Time)),
        },
    ];
    let columns = vec![
        Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1, 2],
            Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
        )),
        Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1, 2],
            Column::UInt32(PrimitiveColumn::new(vec![0, 13, 79])),
        )),
        Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1, 2],
            Column::Time(PrimitiveColumn::new(vec![0, -13, 79])),
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// `LowCardinality(Nullable(String))` and
/// `LowCardinality(Nullable(UInt32))` over four rows with the valid, null,
/// valid, null pattern. Index 0 is the ClickHouse NULL sentinel and the
/// dictionary body is the bare non-nullable inner type.
fn low_cardinality_nullable_batch() -> ColBatch {
    let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let fields = vec![
        Field {
            name: "lcn".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String)))),
        },
        Field {
            name: "lcn_u32".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::UInt32)))),
        },
    ];
    let columns = vec![
        Column::Dictionary(DictionaryColumn::new_nullable(
            vec![1, 0, 2, 0],
            Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
            validity(),
        )),
        Column::Dictionary(DictionaryColumn::new_nullable(
            vec![1, 0, 2, 0],
            Column::UInt32(PrimitiveColumn::new(vec![0, 13, 79])),
            validity(),
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// An `Array(Int32)` column over four rows: `[13, 79]`, `[]` (an empty row,
/// so an adjacent-equal offset pair), `[21]`, `[34, 55, 89]`.
fn array_int32_batch() -> ColBatch {
    let fields = vec![Field {
        name: "a".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 2, 3, 6],
        Column::Int32(PrimitiveColumn::new(vec![13, 79, 21, 34, 55, 89])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// An `Array(Nullable(String))` column over three rows: `["user_1", NULL]`,
/// `[]`, `["user_2"]`. The element null map covers the flattened element
/// run, so its validity lives on the flattened Utf8 column, not the array.
fn array_nullable_string_batch() -> ColBatch {
    let mut elements = utf8_column(&[b"user_1", b"", b"user_2"]);
    elements.validity = Some(Bitmap::from_ch_null_map(&[0, 1, 0]));
    let fields = vec![Field {
        name: "ans".into(),
        ch_type: ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::String)))),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 2, 3],
        Column::Utf8(elements),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// An `Array(LowCardinality(String))` column over three rows:
/// `[user_1, user_2]`, `[]`, `[user_1]`. The element column is one
/// dictionary over the flattened run, and the LC key version is hoisted to
/// the front of the whole column, before the offsets.
fn array_low_cardinality_batch() -> ColBatch {
    let fields = vec![Field {
        name: "alc".into(),
        ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 2, 3],
        Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1],
            Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
        )),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// An `Array(LowCardinality(String))` column with rows > 0 but EVERY array
/// empty, so the flattened element run has zero length and the LC element
/// body must be entirely absent: the wire is `[key version][zero offsets]`
/// and nothing else (the server's `limit == 0` early return).
fn array_low_cardinality_all_empty_batch() -> ColBatch {
    let fields = vec![Field {
        name: "alc".into(),
        ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 0, 0],
        Column::Dictionary(DictionaryColumn::new(
            vec![],
            Column::Utf8(utf8_column(&[])),
        )),
    ))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// An `Array(Array(Int32))` column over three rows:
/// `[[13, 79], [21]]`, `[]`, `[[34, 55, 89]]`. The outer offsets count inner
/// arrays, the inner offsets count leaf ints, and only one offsets run per
/// level is written (no prefixes anywhere for an Int32 leaf).
fn array_of_array_batch() -> ColBatch {
    let fields = vec![Field {
        name: "aa".into(),
        ch_type: ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 2, 3],
        Column::Array(ArrayColumn::new(
            vec![0, 2, 3, 6],
            Column::Int32(PrimitiveColumn::new(vec![13, 79, 21, 34, 55, 89])),
        )),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// An `Array(Int128)` column over three rows, exercising a wide int as a
/// container element: `[13, INT128_MIN]`, `[]` (an empty row, adjacent-equal
/// offsets), `[-1]`. INT128_MIN is the sign-boundary value whose only high
/// byte is set (byte 15 = 0x80). A byte-reversal turns it into a small
/// positive value and a sign bug mangles it, so either fails the round-trip.
/// The core stores the raw wire bytes verbatim, so these are wire-order.
fn array_int128_batch() -> ColBatch {
    let mut i128_min = [0u8; 16];
    i128_min[15] = 0x80;
    let mut thirteen = [0u8; 16];
    thirteen[0] = 13;
    let fields = vec![Field {
        name: "a".into(),
        ch_type: ChType::Array(Box::new(ChType::Int128)),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 2, 3],
        Column::Int128(wide_int_column(16, &[&thirteen, &i128_min, &[0xFFu8; 16]])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// Tuple(Int32, String) plus a named Tuple(a Int32, b Nullable(String)),
/// covering an unnamed tuple, element names, and a Nullable element.
fn tuple_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        },
        Field {
            name: "tn".into(),
            ch_type: ChType::Tuple(vec![
                (Some("a".to_string()), ChType::Int32),
                (
                    Some("b".to_string()),
                    ChType::Nullable(Box::new(ChType::String)),
                ),
            ]),
        },
    ];
    let mut b = utf8_column(&[b"user_1", b"", b"user_2"]);
    b.validity = Some(Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]));
    let columns = vec![
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![13, 79, -7])),
                Column::Utf8(utf8_column(&[b"user_1", b"user_2", b""])),
            ],
            3,
        )),
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
                Column::Utf8(b),
            ],
            3,
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// Nullable(Tuple(Int32, String)) with the tuple-level null map, plus a
/// tuple with a LowCardinality element (whose key-version prefix is hoisted
/// ahead of element 0's body).
fn nullable_and_lc_tuple_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "nt".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Tuple(vec![
                (None, ChType::Int32),
                (None, ChType::String),
            ]))),
        },
        Field {
            name: "tlc".into(),
            ch_type: ChType::Tuple(vec![
                (Some("k".to_string()), ChType::Int32),
                (
                    Some("lc".to_string()),
                    ChType::LowCardinality(Box::new(ChType::String)),
                ),
            ]),
        },
    ];
    let columns = vec![
        Column::Tuple(TupleColumn::new_nullable(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![13, 0, 79])),
                Column::Utf8(utf8_column(&[b"user_1", b"", b"user_2"])),
            ],
            3,
            Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]),
        )),
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
                Column::Dictionary(DictionaryColumn::new(
                    vec![1, 2, 1],
                    Column::Utf8(utf8_column(&[b"", b"red", b"green"])),
                )),
            ],
            3,
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// Array(Tuple(Int32, Int32)) and a nested Tuple(p Tuple(Int8, Int8), s
/// String), covering both container compositions.
fn array_and_nested_tuple_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "at".into(),
            ch_type: ChType::Array(Box::new(ChType::Tuple(vec![
                (None, ChType::Int32),
                (None, ChType::Int32),
            ]))),
        },
        Field {
            name: "tt".into(),
            ch_type: ChType::Tuple(vec![
                (
                    Some("p".to_string()),
                    ChType::Tuple(vec![(None, ChType::Int8), (None, ChType::Int8)]),
                ),
                (Some("s".to_string()), ChType::String),
            ]),
        },
    ];
    let columns = vec![
        // [], [(13, 79)], [(1, 2), (3, 4)] -> offsets [0, 0, 1, 3].
        Column::Array(ArrayColumn::new(
            vec![0, 0, 1, 3],
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int32(PrimitiveColumn::new(vec![13, 1, 3])),
                    Column::Int32(PrimitiveColumn::new(vec![79, 2, 4])),
                ],
                3,
            )),
        )),
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Tuple(TupleColumn::new(
                    vec![
                        Column::Int8(PrimitiveColumn::new(vec![1, 3, 5])),
                        Column::Int8(PrimitiveColumn::new(vec![2, 4, 6])),
                    ],
                    3,
                )),
                Column::Utf8(utf8_column(&[b"user_1", b"user_2", b"user_3"])),
            ],
            3,
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// The zero-element Tuple(): one placeholder byte per row on the wire.
fn empty_tuple_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "k".into(),
            ch_type: ChType::Int32,
        },
        Field {
            name: "t0".into(),
            ch_type: ChType::Tuple(vec![]),
        },
    ];
    let columns = vec![
        Column::Int32(PrimitiveColumn::new(vec![13, 79])),
        Column::Tuple(TupleColumn::new(vec![], 2)),
    ];
    ColBatch::new(Schema::new(fields), columns, 2)
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

/// Map(String, Int32) plus Map(Int32, Nullable(String)), covering a plain
/// map with an empty row and a Nullable value run.
fn map_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        },
        Field {
            name: "mnv".into(),
            ch_type: ChType::Map(
                Box::new(ChType::Int32),
                Box::new(ChType::Nullable(Box::new(ChType::String))),
            ),
        },
    ];
    let mut nullable_values = utf8_column(&[b"user_1", b"", b"user_2"]);
    nullable_values.validity = Some(Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]));
    let columns = vec![
        // {} / {a: 13} / {a: 1, b: 2}
        Column::Map(map_column(
            vec![0, 0, 1, 3],
            Column::Utf8(utf8_column(&[b"a", b"a", b"b"])),
            Column::Int32(PrimitiveColumn::new(vec![13, 1, 2])),
        )),
        // {1: user_1} / {2: NULL} / {3: user_2}
        Column::Map(map_column(
            vec![0, 1, 2, 3],
            Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
            Column::Utf8(nullable_values),
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// A `Map(String, Int256)` column over three rows, exercising a wide int as
/// a map value: `{}`, `{k1: INT256_MIN}`, `{k1: 13, k2: -1}`. INT256_MIN is
/// the sign-boundary value whose only high byte is set (byte 31 = 0x80), so
/// a byte-reversal or sign bug in the value run fails the round-trip.
fn map_string_int256_batch() -> ColBatch {
    let mut i256_min = [0u8; 32];
    i256_min[31] = 0x80;
    let mut thirteen = [0u8; 32];
    thirteen[0] = 13;
    let fields = vec![Field {
        name: "m".into(),
        ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int256)),
    }];
    let columns = vec![Column::Map(map_column(
        vec![0, 0, 1, 3],
        Column::Utf8(utf8_column(&[b"k1", b"k1", b"k2"])),
        Column::Int256(wide_int_column(32, &[&i256_min, &thirteen, &[0xFFu8; 32]])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// Map(LowCardinality(String), UInt8) (the hoisted key prefix) plus
/// Map(String, Array(Int32)) and a nested Map value.
fn lc_and_nested_map_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "mlc".into(),
            ch_type: ChType::Map(
                Box::new(ChType::LowCardinality(Box::new(ChType::String))),
                Box::new(ChType::UInt8),
            ),
        },
        Field {
            name: "marr".into(),
            ch_type: ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Array(Box::new(ChType::Int32))),
            ),
        },
        Field {
            name: "mm".into(),
            ch_type: ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Map(
                    Box::new(ChType::String),
                    Box::new(ChType::Int32),
                )),
            ),
        },
    ];
    let columns = vec![
        // {red: 1} / {} / {red: 2, blue: 3}
        Column::Map(map_column(
            vec![0, 1, 1, 3],
            Column::Dictionary(DictionaryColumn::new(
                vec![1, 1, 2],
                Column::Utf8(utf8_column(&[b"", b"red", b"blue"])),
            )),
            Column::UInt8(PrimitiveColumn::new(vec![1, 2, 3])),
        )),
        // {a: [13]} / {b: [], c: [1, 2]} / {}
        Column::Map(map_column(
            vec![0, 1, 3, 3],
            Column::Utf8(utf8_column(&[b"a", b"b", b"c"])),
            Column::Array(ArrayColumn::new(
                vec![0, 1, 1, 3],
                Column::Int32(PrimitiveColumn::new(vec![13, 1, 2])),
            )),
        )),
        // {a: {x: 1}} / {b: {y: 2, z: 3}} / {}
        Column::Map(map_column(
            vec![0, 1, 2, 2],
            Column::Utf8(utf8_column(&[b"a", b"b"])),
            Column::Map(map_column(
                vec![0, 1, 3],
                Column::Utf8(utf8_column(&[b"x", b"y", b"z"])),
                Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
            )),
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// Array(Map(String, Int32)): maps flattened under array offsets.
fn array_of_map_batch() -> ColBatch {
    let fields = vec![Field {
        name: "am".into(),
        ch_type: ChType::Array(Box::new(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Int32),
        ))),
    }];
    // [] / [{a: 1}] / [{b: 2}, {}]
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 0, 1, 3],
        Column::Map(map_column(
            vec![0, 1, 2, 2],
            Column::Utf8(utf8_column(&[b"a", b"b"])),
            Column::Int32(PrimitiveColumn::new(vec![1, 2])),
        )),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// Compare two columns for the types this encoder covers. Used recursively
/// for `LowCardinality` dictionary values.
fn assert_columns_eq(left: &Column, right: &Column, label: &str) {
    macro_rules! eq {
        ($va:expr, $vb:expr) => {
            assert_eq!($va.values, $vb.values, "{label} values differ")
        };
    }
    match (left, right) {
        (Column::Bool(x), Column::Bool(y)) => {
            assert_eq!(x.len, y.len, "{label} bool len differs");
            for row in 0..x.len {
                assert_eq!(x.get(row), y.get(row), "{label} bool row {row} differs");
            }
        }
        (Column::Int8(x), Column::Int8(y)) => eq!(x, y),
        (Column::Int16(x), Column::Int16(y)) => eq!(x, y),
        (Column::Int32(x), Column::Int32(y)) => eq!(x, y),
        (Column::Int64(x), Column::Int64(y)) => eq!(x, y),
        (Column::UInt8(x), Column::UInt8(y)) => eq!(x, y),
        (Column::UInt16(x), Column::UInt16(y)) => eq!(x, y),
        (Column::UInt32(x), Column::UInt32(y)) => eq!(x, y),
        (Column::UInt64(x), Column::UInt64(y)) => eq!(x, y),
        (Column::Float32(x), Column::Float32(y)) => eq!(x, y),
        (Column::Float64(x), Column::Float64(y)) => eq!(x, y),
        (Column::Date(x), Column::Date(y)) => eq!(x, y),
        (Column::Date32(x), Column::Date32(y)) => eq!(x, y),
        (Column::DateTime(x), Column::DateTime(y)) => eq!(x, y),
        (Column::DateTime64(x), Column::DateTime64(y)) => eq!(x, y),
        (Column::Time(x), Column::Time(y)) => eq!(x, y),
        (Column::Time64(x), Column::Time64(y)) => eq!(x, y),
        (Column::Enum8(x), Column::Enum8(y)) => eq!(x, y),
        (Column::Enum16(x), Column::Enum16(y)) => eq!(x, y),
        (Column::Ipv4(x), Column::Ipv4(y)) => eq!(x, y),
        (Column::Uuid(x), Column::Uuid(y))
        | (Column::Ipv6(x), Column::Ipv6(y))
        | (Column::Int128(x), Column::Int128(y))
        | (Column::UInt128(x), Column::UInt128(y))
        | (Column::Int256(x), Column::Int256(y))
        | (Column::UInt256(x), Column::UInt256(y)) => {
            assert_eq!(x.width, y.width, "{label} width differ");
            assert_eq!(x.data, y.data, "{label} data differ");
        }
        (Column::Utf8(x), Column::Utf8(y)) => {
            assert_eq!(x.offsets, y.offsets, "{label} offsets differ");
            assert_eq!(x.data, y.data, "{label} data differ");
        }
        (Column::FixedBinary(x), Column::FixedBinary(y)) => {
            assert_eq!(x.width, y.width, "{label} width differ");
            assert_eq!(x.data, y.data, "{label} data differ");
        }
        (Column::Decimal(x), Column::Decimal(y)) => {
            assert_eq!(x.width, y.width, "{label} width differ");
            assert_eq!(x.precision, y.precision, "{label} precision differs");
            assert_eq!(x.scale, y.scale, "{label} scale differs");
            assert_eq!(x.data, y.data, "{label} data differ");
        }
        (Column::Dictionary(x), Column::Dictionary(y)) => {
            assert_eq!(x.indices, y.indices, "{label} dictionary indices differ");
            let dict_label = format!("{label} dictionary");
            assert_columns_eq(x.values.as_ref(), y.values.as_ref(), &dict_label);
        }
        (Column::Array(x), Column::Array(y)) => {
            assert_eq!(x.offsets, y.offsets, "{label} array offsets differ");
            let elem_label = format!("{label} array elements");
            assert_columns_eq(x.values.as_ref(), y.values.as_ref(), &elem_label);
        }
        (Column::Tuple(x), Column::Tuple(y)) => {
            assert_eq!(x.len, y.len, "{label} tuple len differs");
            assert_eq!(
                x.fields.len(),
                y.fields.len(),
                "{label} tuple field count differs"
            );
            for (i, (a, b)) in x.fields.iter().zip(&y.fields).enumerate() {
                assert_columns_eq(a, b, &format!("{label} tuple element {i}"));
            }
        }
        (Column::Map(x), Column::Map(y)) => {
            assert_eq!(x.offsets, y.offsets, "{label} map offsets differ");
            let entries_label = format!("{label} map entries");
            assert_columns_eq(x.entries.as_ref(), y.entries.as_ref(), &entries_label);
        }
        (other_a, other_b) => panic!("{label}: unexpected {other_a:?} vs {other_b:?}"),
    }

    // Validity (the null map or dictionary-index validity) must survive the
    // round-trip too. Both sides must agree on presence and on every row's
    // valid/null bit.
    match (left.validity(), right.validity()) {
        (None, None) => {}
        (Some(x), Some(y)) => {
            assert_eq!(x.len(), y.len(), "{label} validity len differs");
            for row in 0..x.len() {
                assert_eq!(
                    x.is_valid(row),
                    y.is_valid(row),
                    "{label} validity row {row} differs"
                );
            }
        }
        (x, y) => panic!(
            "{label} validity presence differs: {} vs {}",
            x.is_some(),
            y.is_some()
        ),
    }
}

/// Compare two batches column by column. Panics on any unexpected variant so
/// a wrong decode is loud.
fn assert_batches_eq(left: &ColBatch, right: &ColBatch) {
    assert_eq!(left.schema, right.schema, "schema mismatch");
    assert_eq!(left.num_rows, right.num_rows, "row count mismatch");
    assert_eq!(
        left.columns.len(),
        right.columns.len(),
        "column count mismatch"
    );
    for (i, (a, b)) in left.columns.iter().zip(&right.columns).enumerate() {
        assert_columns_eq(a, b, &format!("column {i}"));
    }
}

/// Encode `batch` as one block at `revision`, decode it back, and assert the
/// buffers survived unchanged.
fn roundtrip(batch: &ColBatch, revision: u64) {
    let bytes = encode_block(
        batch,
        &EncodeOptions {
            protocol_revision: revision,
        },
    )
    .unwrap();
    let decoded = decode_all_bytes(
        &bytes,
        &DecodeOptions {
            protocol_revision: revision,
        },
    )
    .unwrap_or_else(|e| panic!("decode at rev {revision} failed: {e}"));
    assert_eq!(decoded.num_chunks(), 1);
    assert_batches_eq(batch, &decoded.chunks[0]);
}

#[test]
fn roundtrip_numerics_rev0() {
    roundtrip(&numeric_batch(), 0);
}

#[test]
fn roundtrip_numerics_tcp_revision() {
    roundtrip(&numeric_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_strings_rev0() {
    roundtrip(&string_batch(), 0);
}

#[test]
fn roundtrip_strings_tcp_revision() {
    roundtrip(&string_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_bool_rev0() {
    roundtrip(&bool_batch(), 0);
}

#[test]
fn roundtrip_bool_tcp_revision() {
    roundtrip(&bool_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_rev0() {
    roundtrip(&nullable_batch(), 0);
}

#[test]
fn roundtrip_nullable_tcp_revision() {
    roundtrip(&nullable_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_temporal_rev0() {
    roundtrip(&temporal_batch(), 0);
}

#[test]
fn roundtrip_temporal_tcp_revision() {
    roundtrip(&temporal_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_temporal_rev0() {
    roundtrip(&nullable_temporal_batch(), 0);
}

#[test]
fn roundtrip_nullable_temporal_tcp_revision() {
    roundtrip(&nullable_temporal_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_uuid_ip_rev0() {
    roundtrip(&uuid_ip_batch(), 0);
}

#[test]
fn roundtrip_uuid_ip_tcp_revision() {
    roundtrip(&uuid_ip_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_uuid_ip_rev0() {
    roundtrip(&nullable_uuid_ip_batch(), 0);
}

#[test]
fn roundtrip_nullable_uuid_ip_tcp_revision() {
    roundtrip(&nullable_uuid_ip_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_enum_rev0() {
    roundtrip(&enum_batch(), 0);
}

#[test]
fn roundtrip_enum_tcp_revision() {
    roundtrip(&enum_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_enum_rev0() {
    roundtrip(&nullable_enum_batch(), 0);
}

#[test]
fn roundtrip_nullable_enum_tcp_revision() {
    roundtrip(&nullable_enum_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_decimal_rev0() {
    roundtrip(&decimal_batch(), 0);
}

#[test]
fn roundtrip_decimal_tcp_revision() {
    roundtrip(&decimal_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_decimal_rev0() {
    roundtrip(&nullable_decimal_batch(), 0);
}

#[test]
fn roundtrip_nullable_decimal_tcp_revision() {
    roundtrip(&nullable_decimal_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_wide_int_rev0() {
    roundtrip(&wide_int_batch(), 0);
}

#[test]
fn roundtrip_wide_int_tcp_revision() {
    roundtrip(&wide_int_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_wide_int_rev0() {
    roundtrip(&nullable_wide_int_batch(), 0);
}

#[test]
fn roundtrip_nullable_wide_int_tcp_revision() {
    roundtrip(&nullable_wide_int_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_low_cardinality_wide_int_rev0() {
    roundtrip(&low_cardinality_wide_int_batch(), 0);
}

#[test]
fn roundtrip_low_cardinality_wide_int_tcp_revision() {
    roundtrip(&low_cardinality_wide_int_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_low_cardinality_rev0() {
    roundtrip(&low_cardinality_batch(), 0);
}

#[test]
fn roundtrip_low_cardinality_tcp_revision() {
    roundtrip(&low_cardinality_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_low_cardinality_rev0() {
    roundtrip(&low_cardinality_nullable_batch(), 0);
}

#[test]
fn roundtrip_nullable_low_cardinality_tcp_revision() {
    roundtrip(&low_cardinality_nullable_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_int32_rev0() {
    roundtrip(&array_int32_batch(), 0);
}

#[test]
fn roundtrip_array_int32_tcp_revision() {
    roundtrip(&array_int32_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_nullable_string_rev0() {
    roundtrip(&array_nullable_string_batch(), 0);
}

#[test]
fn roundtrip_array_nullable_string_tcp_revision() {
    roundtrip(&array_nullable_string_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_low_cardinality_rev0() {
    roundtrip(&array_low_cardinality_batch(), 0);
}

#[test]
fn roundtrip_array_low_cardinality_tcp_revision() {
    roundtrip(&array_low_cardinality_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_low_cardinality_all_empty_rev0() {
    roundtrip(&array_low_cardinality_all_empty_batch(), 0);
}

#[test]
fn roundtrip_array_low_cardinality_all_empty_tcp_revision() {
    roundtrip(
        &array_low_cardinality_all_empty_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_array_of_array_rev0() {
    roundtrip(&array_of_array_batch(), 0);
}

#[test]
fn roundtrip_array_of_array_tcp_revision() {
    roundtrip(&array_of_array_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_int128_rev0() {
    roundtrip(&array_int128_batch(), 0);
}

#[test]
fn roundtrip_array_int128_tcp_revision() {
    roundtrip(&array_int128_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_tuple_rev0() {
    roundtrip(&tuple_batch(), 0);
}

#[test]
fn roundtrip_tuple_tcp_revision() {
    roundtrip(&tuple_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_and_lc_tuple_rev0() {
    roundtrip(&nullable_and_lc_tuple_batch(), 0);
}

#[test]
fn roundtrip_nullable_and_lc_tuple_tcp_revision() {
    roundtrip(&nullable_and_lc_tuple_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_and_nested_tuple_rev0() {
    roundtrip(&array_and_nested_tuple_batch(), 0);
}

#[test]
fn roundtrip_array_and_nested_tuple_tcp_revision() {
    roundtrip(&array_and_nested_tuple_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_map_rev0() {
    roundtrip(&map_batch(), 0);
}

#[test]
fn roundtrip_map_tcp_revision() {
    roundtrip(&map_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_map_string_int256_rev0() {
    roundtrip(&map_string_int256_batch(), 0);
}

#[test]
fn roundtrip_map_string_int256_tcp_revision() {
    roundtrip(&map_string_int256_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_lc_and_nested_map_rev0() {
    roundtrip(&lc_and_nested_map_batch(), 0);
}

#[test]
fn roundtrip_lc_and_nested_map_tcp_revision() {
    roundtrip(&lc_and_nested_map_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_of_map_rev0() {
    roundtrip(&array_of_map_batch(), 0);
}

#[test]
fn roundtrip_array_of_map_tcp_revision() {
    roundtrip(&array_of_map_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_map_all_empty_lc_key() {
    // Map(LowCardinality(String), Int32) with rows > 0 but every map empty:
    // the wire must be the hoisted LC key version, the zero offsets, and
    // NOTHING for the key/value runs (limit == 0 gates through the Map
    // path).
    let fields = vec![Field {
        name: "m".into(),
        ch_type: ChType::Map(
            Box::new(ChType::LowCardinality(Box::new(ChType::String))),
            Box::new(ChType::Int32),
        ),
    }];
    let columns = vec![Column::Map(map_column(
        vec![0, 0, 0, 0],
        Column::Dictionary(DictionaryColumn::new(
            vec![],
            Column::Utf8(utf8_column(&[])),
        )),
        Column::Int32(PrimitiveColumn::new(vec![])),
    ))];
    let batch = ColBatch::new(Schema::new(fields), columns, 3);

    // Pin the exact wire body: header, then key version + three zero
    // offsets and nothing else.
    let bytes = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: 0,
        },
    )
    .unwrap();
    let mut expected = Vec::new();
    expected.push(0x01); // 1 column
    expected.push(0x03); // 3 rows
    expected.push(0x01); // name len
    expected.extend_from_slice(b"m");
    let type_name = "Map(LowCardinality(String), Int32)";
    expected.push(type_name.len() as u8);
    expected.extend_from_slice(type_name.as_bytes());
    expected.extend_from_slice(&1u64.to_le_bytes()); // hoisted LC key version
    expected.extend_from_slice(&[0u8; 24]); // three zero offsets
    assert_eq!(bytes, expected);

    roundtrip(&batch, 0);
    roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn rev0_frames_map_bytes() {
    // Pin the Map body framing: the Array offsets run (no leading zero),
    // then the flattened key run, then the flattened value run. One
    // Map(String, Int32) column "m" with two rows {hi: 13} and {}.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        }]),
        vec![Column::Map(map_column(
            vec![0, 1, 1],
            Column::Utf8(utf8_column(&[b"hi"])),
            Column::Int32(PrimitiveColumn::new(vec![13])),
        ))],
        2,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x01, b'm', // name "m"
        0x12, // type name length 18
        b'M', b'a', b'p', b'(', b'S', b't', b'r', b'i', b'n', b'g', b',', b' ', b'I', b'n', b't',
        b'3', b'2', b')', // type "Map(String, Int32)"
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 0: 1
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 1: 1
        0x02, b'h', b'i', // key run: varint len 2 then "hi"
        0x0D, 0x00, 0x00, 0x00, // value run: Int32 13
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn map_illegal_key_type_is_rejected() {
    // A Nullable key violates the server's DataTypeMap::isValidKeyType, so
    // the type itself cannot exist: UnsupportedType, before any bytes.
    let ch_type = ChType::Map(
        Box::new(ChType::Nullable(Box::new(ChType::String))),
        Box::new(ChType::Int32),
    );
    let mut keys = utf8_column(&[b"a"]);
    keys.validity = Some(Bitmap::from_ch_null_map(&[0x00]));
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ch_type.clone(),
        }]),
        vec![Column::Map(map_column(
            vec![0, 1],
            Column::Utf8(keys),
            Column::Int32(PrimitiveColumn::new(vec![13])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::UnsupportedType { column, ch_type: t } => {
            assert_eq!(column, "m");
            assert_eq!(t, ch_type);
        }
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

#[test]
fn map_offsets_entries_mismatch_is_rejected() {
    // Offsets end at 2 but the entries tuple holds 1 row: a misframed
    // stream the server would reject, so InconsistentBatch before any
    // bytes.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        }]),
        vec![Column::Map(map_column(
            vec![0, 2],
            Column::Utf8(utf8_column(&[b"a"])),
            Column::Int32(PrimitiveColumn::new(vec![13])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn map_ragged_entries_are_rejected() {
    // Keys and values of different lengths cannot both be full runs of the
    // entry count: InconsistentBatch, not a panic.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        }]),
        vec![Column::Map(map_column(
            vec![0, 2],
            Column::Utf8(utf8_column(&[b"a", b"b"])),
            Column::Int32(PrimitiveColumn::new(vec![13])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn nullable_map_nesting_is_rejected() {
    // Nullable(Map) is not constructible on the server
    // (canBeInsideNullable false); the type-header round-trip check fails
    // before any bytes are written, like Nullable(LowCardinality).
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "nm".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Int32),
            ))),
        }]),
        vec![Column::Map(map_column(
            vec![0, 1],
            Column::Utf8(utf8_column(&[b"a"])),
            Column::Int32(PrimitiveColumn::new(vec![13])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn roundtrip_empty_tuple_rev0() {
    roundtrip(&empty_tuple_batch(), 0);
}

#[test]
fn roundtrip_empty_tuple_tcp_revision() {
    roundtrip(&empty_tuple_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_of_tuple_all_empty() {
    // Array(Tuple(LowCardinality(String), Int32)) with rows > 0 but every
    // array empty: the wire must be the hoisted LC key version, the zero
    // offsets, and NOTHING for the element bodies (each element gets a
    // limit == 0 run through the Tuple path; the LC early-return gate must
    // fire).
    let fields = vec![Field {
        name: "a".into(),
        ch_type: ChType::Array(Box::new(ChType::Tuple(vec![
            (None, ChType::LowCardinality(Box::new(ChType::String))),
            (None, ChType::Int32),
        ]))),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 0, 0, 0],
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Dictionary(DictionaryColumn::new(
                    vec![],
                    Column::Utf8(utf8_column(&[])),
                )),
                Column::Int32(PrimitiveColumn::new(vec![])),
            ],
            0,
        )),
    ))];
    let batch = ColBatch::new(Schema::new(fields), columns, 3);

    // Pin the exact wire body: header, then key version + three zero
    // offsets and nothing else.
    let bytes = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: 0,
        },
    )
    .unwrap();
    let mut expected = Vec::new();
    expected.push(0x01); // 1 column
    expected.push(0x03); // 3 rows
    expected.push(0x01); // name len
    expected.extend_from_slice(b"a");
    let type_name = "Array(Tuple(LowCardinality(String), Int32))";
    expected.push(type_name.len() as u8);
    expected.extend_from_slice(type_name.as_bytes());
    expected.extend_from_slice(&1u64.to_le_bytes()); // hoisted LC key version
    expected.extend_from_slice(&[0u8; 24]); // three zero offsets
    assert_eq!(bytes, expected);

    roundtrip(&batch, 0);
    roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn zero_row_block_roundtrips_schema() {
    // A zero-row block still carries full column headers. The decoder keeps
    // the schema but drops the empty block from `chunks`.
    let fields = vec![
        Field {
            name: "n".into(),
            ch_type: ChType::Int32,
        },
        Field {
            name: "x".into(),
            ch_type: ChType::Float64,
        },
        Field {
            name: "u".into(),
            ch_type: ChType::Uuid,
        },
        Field {
            name: "ip4".into(),
            ch_type: ChType::Ipv4,
        },
        Field {
            name: "ip6".into(),
            ch_type: ChType::Ipv6,
        },
        Field {
            name: "i128".into(),
            ch_type: ChType::Int128,
        },
        Field {
            name: "u256".into(),
            ch_type: ChType::UInt256,
        },
        Field {
            name: "dec".into(),
            ch_type: ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            },
        },
        Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::String)),
        },
        Field {
            name: "lcn".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String)))),
        },
        Field {
            name: "time".into(),
            ch_type: ChType::Time,
        },
        Field {
            name: "time64".into(),
            ch_type: ChType::Time64 { precision: 3 },
        },
        Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::Int32)),
        },
        Field {
            name: "alc".into(),
            ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
        },
        Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![
                (Some("a".to_string()), ChType::Int32),
                (Some("b".to_string()), ChType::String),
            ]),
        },
        Field {
            name: "t0".into(),
            ch_type: ChType::Tuple(vec![]),
        },
        Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        },
    ];
    let columns = vec![
        Column::Int32(PrimitiveColumn::new(vec![])),
        Column::Float64(PrimitiveColumn::new(vec![])),
        Column::Uuid(FixedBinaryColumn::new(Vec::new(), 16)),
        Column::Ipv4(PrimitiveColumn::new(Vec::new())),
        Column::Ipv6(FixedBinaryColumn::new(Vec::new(), 16)),
        Column::Int128(FixedBinaryColumn::new(Vec::new(), 16)),
        Column::UInt256(FixedBinaryColumn::new(Vec::new(), 32)),
        Column::Decimal(DecimalColumn::new(Vec::new(), 4, 9, 4)),
        Column::Dictionary(DictionaryColumn::new(
            vec![],
            Column::Utf8(utf8_column(&[])),
        )),
        Column::Dictionary(DictionaryColumn::new_nullable(
            vec![],
            Column::Utf8(utf8_column(&[])),
            Bitmap::from_ch_null_map(&[]),
        )),
        Column::Time(PrimitiveColumn::new(vec![])),
        Column::Time64(PrimitiveColumn::new(vec![])),
        // A zero-row Array carries only the leading-0 offset and writes no
        // data at all, not even the hoisted LC key version of an LC element.
        Column::Array(ArrayColumn::new(
            vec![0],
            Column::Int32(PrimitiveColumn::new(vec![])),
        )),
        Column::Array(ArrayColumn::new(
            vec![0],
            Column::Dictionary(DictionaryColumn::new(
                vec![],
                Column::Utf8(utf8_column(&[])),
            )),
        )),
        // A zero-row Tuple carries only the header: no element bodies, and
        // for Tuple() no placeholder bytes either.
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![])),
                Column::Utf8(utf8_column(&[])),
            ],
            0,
        )),
        Column::Tuple(TupleColumn::new(vec![], 0)),
        // A zero-row Map carries only the leading-0 offset and writes no
        // data at all.
        Column::Map(map_column(
            vec![0],
            Column::Utf8(utf8_column(&[])),
            Column::Int32(PrimitiveColumn::new(vec![])),
        )),
    ];
    let batch = ColBatch::new(Schema::new(fields), columns, 0);
    for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
        let bytes = encode_block(
            &batch,
            &EncodeOptions {
                protocol_revision: revision,
            },
        )
        .unwrap();
        let decoded = decode_all_bytes(
            &bytes,
            &DecodeOptions {
                protocol_revision: revision,
            },
        )
        .unwrap();
        assert_eq!(decoded.num_rows(), 0);
        assert_eq!(decoded.num_chunks(), 0);
        assert_eq!(decoded.schema, batch.schema);
    }
}

#[test]
fn encode_chunked_roundtrips_multiple_blocks() {
    let field = Field {
        name: "n".into(),
        ch_type: ChType::Int32,
    };
    let chunk = |vals: Vec<i32>| {
        let n = vals.len();
        std::sync::Arc::new(ColBatch::new(
            Schema::new(vec![field.clone()]),
            vec![Column::Int32(PrimitiveColumn::new(vals))],
            n,
        ))
    };
    let batch = ChunkedBatch {
        schema: Schema::new(vec![field.clone()]),
        chunks: vec![chunk(vec![13, 14]), chunk(vec![15, 16]), chunk(vec![17])],
    };
    let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 3);
    assert_eq!(decoded.num_rows(), 5);
    let got: Vec<Vec<i32>> = decoded
        .chunks
        .iter()
        .map(|c| match c.column(0) {
            Column::Int32(p) => p.values.clone(),
            other => panic!("expected Int32, got {other:?}"),
        })
        .collect();
    assert_eq!(got, vec![vec![13, 14], vec![15, 16], vec![17]]);
}

#[test]
fn encode_chunked_roundtrips_time_blocks() {
    let schema = Schema::new(vec![
        Field {
            name: "t".into(),
            ch_type: ChType::Time,
        },
        Field {
            name: "t64".into(),
            ch_type: ChType::Time64 { precision: 3 },
        },
    ]);
    let make_chunk = |time: Vec<i32>, time64: Vec<i64>| {
        let rows = time.len();
        assert_eq!(time64.len(), rows);
        std::sync::Arc::new(ColBatch::new(
            schema.clone(),
            vec![
                Column::Time(PrimitiveColumn::new(time)),
                Column::Time64(PrimitiveColumn::new(time64)),
            ],
            rows,
        ))
    };
    let batch = ChunkedBatch {
        schema: schema.clone(),
        chunks: vec![
            make_chunk(vec![-13, 0], vec![-13_000, 0]),
            make_chunk(vec![79, 3_599_999], vec![79_000, 3_599_999_999]),
        ],
    };
    for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
        let bytes = encode_chunked(
            &batch,
            &EncodeOptions {
                protocol_revision: revision,
            },
        )
        .unwrap();
        let decoded = decode_all_bytes(
            &bytes,
            &DecodeOptions {
                protocol_revision: revision,
            },
        )
        .unwrap();
        assert_eq!(decoded.num_chunks(), 2);
        for (sent, got) in batch.chunks.iter().zip(&decoded.chunks) {
            assert_batches_eq(sent, got);
        }
    }
}

#[test]
fn encode_chunked_roundtrips_uuid_ip_blocks() {
    // Two blocks of the UUID/IPv4/IPv6 schema round-trip through
    // `encode_chunked`, staying separate chunks with the buffers intact
    // (blocks are never merged; see AGENTS.md).
    let schema = uuid_ip_batch().schema.clone();
    let chunk = |uuid_byte: u8, ip4: u32, ip6_byte: u8| {
        std::sync::Arc::new(ColBatch::new(
            schema.clone(),
            vec![
                Column::Uuid(FixedBinaryColumn::new(vec![uuid_byte; 32], 16)),
                Column::Ipv4(PrimitiveColumn::new(vec![ip4, ip4 + 1])),
                Column::Ipv6(FixedBinaryColumn::new(vec![ip6_byte; 32], 16)),
            ],
            2,
        ))
    };
    let batch = ChunkedBatch {
        schema: schema.clone(),
        chunks: vec![
            chunk(0x13, 2_130_706_433, 0x20),
            chunk(0x79, 3_232_235_521, 0x0D),
        ],
    };
    for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
        let bytes = encode_chunked(
            &batch,
            &EncodeOptions {
                protocol_revision: revision,
            },
        )
        .unwrap();
        let decoded = decode_all_bytes(
            &bytes,
            &DecodeOptions {
                protocol_revision: revision,
            },
        )
        .unwrap_or_else(|e| panic!("decode at rev {revision} failed: {e}"));
        assert_eq!(decoded.num_chunks(), 2);
        for (sent, got) in batch.chunks.iter().zip(&decoded.chunks) {
            assert_batches_eq(sent, got);
        }
    }
}

#[test]
fn encode_chunked_roundtrips_decimal_blocks() {
    // Decimal blocks stay separate chunks, never concatenated.
    let field = Field {
        name: "dec".into(),
        ch_type: ChType::Decimal {
            precision: 9,
            scale: 4,
            bits: 32,
        },
    };
    let chunk = |vals: Vec<i32>| {
        let mut data = Vec::with_capacity(vals.len() * 4);
        for v in &vals {
            data.extend_from_slice(&v.to_le_bytes());
        }
        std::sync::Arc::new(ColBatch::new(
            Schema::new(vec![field.clone()]),
            vec![Column::Decimal(DecimalColumn::new(data, 4, 9, 4))],
            vals.len(),
        ))
    };
    let batch = ChunkedBatch {
        schema: Schema::new(vec![field.clone()]),
        chunks: vec![chunk(vec![-13, 0]), chunk(vec![79])],
    };
    let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    for (sent, got) in batch.chunks.iter().zip(&decoded.chunks) {
        assert_batches_eq(sent, got);
    }
}

#[test]
fn encode_chunked_roundtrips_wide_int_blocks() {
    // Wide-int blocks stay separate chunks, never concatenated. Each 16-byte
    // row is written verbatim; block A carries a value and -1 (all 0xFF),
    // block B a single value.
    let field = Field {
        name: "w".into(),
        ch_type: ChType::Int128,
    };
    let chunk = |rows: Vec<[u8; 16]>| {
        let mut data = Vec::with_capacity(rows.len() * 16);
        for r in &rows {
            data.extend_from_slice(r);
        }
        std::sync::Arc::new(ColBatch::new(
            Schema::new(vec![field.clone()]),
            vec![Column::Int128(FixedBinaryColumn::new(data, 16))],
            rows.len(),
        ))
    };
    let mut thirteen = [0u8; 16];
    thirteen[0] = 13;
    let mut seventy_nine = [0u8; 16];
    seventy_nine[0] = 79;
    let batch = ChunkedBatch {
        schema: Schema::new(vec![field.clone()]),
        chunks: vec![
            chunk(vec![thirteen, [0xFFu8; 16]]),
            chunk(vec![seventy_nine]),
        ],
    };
    let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    for (sent, got) in batch.chunks.iter().zip(&decoded.chunks) {
        assert_batches_eq(sent, got);
    }
}

#[test]
fn encode_chunked_roundtrips_low_cardinality_blocks() {
    // LowCardinality dictionaries are block-local. These two chunks use
    // different dictionaries and must stay separate after decode.
    let field = Field {
        name: "lc".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::String)),
    };
    let chunk = |values: &[&[u8]], indices: Vec<i32>| {
        let n = indices.len();
        std::sync::Arc::new(ColBatch::new(
            Schema::new(vec![field.clone()]),
            vec![Column::Dictionary(DictionaryColumn::new(
                indices,
                Column::Utf8(utf8_column(values)),
            ))],
            n,
        ))
    };
    let batch = ChunkedBatch {
        schema: Schema::new(vec![field.clone()]),
        chunks: vec![
            chunk(&[b"", b"user_1", b"user_2"], vec![1, 2, 1]),
            chunk(&[b"", b"user_3"], vec![1, 1]),
        ],
    };
    let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    for (sent, got) in batch.chunks.iter().zip(&decoded.chunks) {
        assert_batches_eq(sent, got);
    }
}

#[test]
fn encode_chunked_roundtrips_array_blocks() {
    // Array element data is block-local (offsets restart at 0 per block).
    // Two Array(Int32) chunks with different shapes must stay separate
    // after decode, never concatenated.
    let field = Field {
        name: "a".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    };
    let chunk = |offsets: Vec<i64>, values: Vec<i32>| {
        let n = offsets.len() - 1;
        std::sync::Arc::new(ColBatch::new(
            Schema::new(vec![field.clone()]),
            vec![Column::Array(ArrayColumn::new(
                offsets,
                Column::Int32(PrimitiveColumn::new(values)),
            ))],
            n,
        ))
    };
    let batch = ChunkedBatch {
        schema: Schema::new(vec![field.clone()]),
        chunks: vec![
            chunk(vec![0, 2, 2, 3], vec![13, 79, 21]),
            chunk(vec![0, 2], vec![34, 55]),
        ],
    };
    for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
        let bytes = encode_chunked(
            &batch,
            &EncodeOptions {
                protocol_revision: revision,
            },
        )
        .unwrap();
        let decoded = decode_all_bytes(
            &bytes,
            &DecodeOptions {
                protocol_revision: revision,
            },
        )
        .unwrap_or_else(|e| panic!("decode at rev {revision} failed: {e}"));
        assert_eq!(decoded.num_chunks(), 2);
        for (sent, got) in batch.chunks.iter().zip(&decoded.chunks) {
            assert_batches_eq(sent, got);
        }
    }
}

#[test]
fn rev0_frames_exact_bytes() {
    // Pin the rev-0 framing byte-for-byte: no BlockInfo, no marker. One Int32
    // column "n" with a single row = 1.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Int32,
        }]),
        vec![Column::Int32(PrimitiveColumn::new(vec![1]))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'n', // name "n"
        0x05, b'I', b'n', b't', b'3', b'2', // type "Int32"
        0x01, 0x00, 0x00, 0x00, // Int32 value 1, little-endian
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_time_signed_little_endian_bytes() {
    let batch = ColBatch::new(
        Schema::new(vec![
            Field {
                name: "t".into(),
                ch_type: ChType::Time,
            },
            Field {
                name: "t64".into(),
                ch_type: ChType::Time64 { precision: 3 },
            },
        ]),
        vec![
            Column::Time(PrimitiveColumn::new(vec![-13])),
            Column::Time64(PrimitiveColumn::new(vec![-79_000])),
        ],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x02, // num_cols = 2
        0x01, // num_rows = 1
        0x01, b't', // name "t"
        0x04, b'T', b'i', b'm', b'e', // type "Time"
        0xF3, 0xFF, 0xFF, 0xFF, // i32 -13 LE
        0x03, b't', b'6', b'4', // name "t64"
        0x09, b'T', b'i', b'm', b'e', b'6', b'4', b'(', b'3', b')', 0x68, 0xCB, 0xFE, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, // i64 -79000 LE
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev_tcp_frames_block_info_and_marker() {
    // At the TCP revision the block leads with the BlockInfo preamble and each
    // column header carries the default (0) custom-serialization marker.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::UInt8,
        }]),
        vec![Column::UInt8(PrimitiveColumn::new(vec![79]))],
        1,
    );
    let bytes = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
        },
    )
    .unwrap();
    let expected = [
        0x01, 0x00, // field 1 is_overflows = false
        0x02, 0xFF, 0xFF, 0xFF, 0xFF, // field 2 bucket_num = -1
        0x03, 0x00, // field 3 out_of_order_buckets = empty (rev >= 54480)
        0x00, // BlockInfo terminator
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'n', // name "n"
        0x05, b'U', b'I', b'n', b't', b'8', // type "UInt8"
        0x00, // custom-serialization marker = default
        0x4F, // UInt8 value 79
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn nullable_low_cardinality_nesting_is_rejected() {
    // `Nullable(LowCardinality(T))` is the illegal nesting direction. The
    // supported shape is `LowCardinality(Nullable(T))`, so this must fail at
    // the type-header round-trip check before any bytes are written.
    let lc_string = ChType::LowCardinality(Box::new(ChType::String));
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "nlc".into(),
            ch_type: ChType::Nullable(Box::new(lc_string)),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![0],
            Column::Utf8(utf8_column(&[b"user_1"])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn low_cardinality_unsupported_inner_reports_column_and_type() {
    // Decimal is encodable as a plain column, but the server forbids it as a
    // LowCardinality inner (`canBeInsideLowCardinality()` is false), so the
    // wrapper remains unsupported and reports the full declared type.
    let lc_decimal = ChType::LowCardinality(Box::new(ChType::Decimal {
        precision: 9,
        scale: 4,
        bits: 32,
    }));
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "lc".into(),
            ch_type: lc_decimal.clone(),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![0],
            Column::Decimal(DecimalColumn::new(vec![0u8; 4], 4, 9, 4)),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::UnsupportedType { column, ch_type } => {
            assert_eq!(column, "lc");
            assert_eq!(ch_type, lc_decimal);
        }
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

#[test]
fn low_cardinality_time64_is_unsupported() {
    let lc_time64 = ChType::LowCardinality(Box::new(ChType::Time64 { precision: 3 }));
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "lc_t64".into(),
            ch_type: lc_time64.clone(),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![0],
            Column::Time64(PrimitiveColumn::new(vec![0])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::UnsupportedType { column, ch_type } => {
            assert_eq!(column, "lc_t64");
            assert_eq!(ch_type, lc_time64);
        }
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

#[test]
fn rev0_frames_tuple_bytes() {
    // Pin the Tuple body framing: element 0's FULL run then element 1's,
    // column-of-columns, no interleaving, no offsets, no tuple-level
    // framing. One Tuple(Int32, String) column "t" with two rows
    // (13, "hi") and (-1, "").
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![13, -1])),
                Column::Utf8(utf8_column(&[b"hi", b""])),
            ],
            2,
        ))],
        2,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x01, b't', // name "t"
        0x14, // type name length 20
        b'T', b'u', b'p', b'l', b'e', b'(', b'I', b'n', b't', b'3', b'2', b',', b' ', b'S', b't',
        b'r', b'i', b'n', b'g', b')', // type "Tuple(Int32, String)"
        0x0D, 0x00, 0x00, 0x00, // element 0 row 0: Int32 13
        0xFF, 0xFF, 0xFF, 0xFF, // element 0 row 1: Int32 -1
        0x02, b'h', b'i', // element 1 row 0: varint len 2 then "hi"
        0x00, // element 1 row 1: varint len 0
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_empty_tuple_bytes() {
    // Pin the zero-element Tuple() body: exactly one literal ASCII '0'
    // byte (0x30) per row, nothing else.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t0".into(),
            ch_type: ChType::Tuple(vec![]),
        }]),
        vec![Column::Tuple(TupleColumn::new(vec![], 3))],
        3,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x03, // num_rows = 3
        0x02, b't', b'0', // name "t0"
        0x07, b'T', b'u', b'p', b'l', b'e', b'(', b')', // type "Tuple()"
        0x30, 0x30, 0x30, // one ASCII '0' per row
    ];
    assert_eq!(bytes, expected);
}

/// A one-element named-tuple batch over a matching one-field Int8 column,
/// for the element-name legality tests.
fn named_tuple_batch(name: Option<&str>) -> ColBatch {
    ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(name.map(str::to_string), ChType::Int8)]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![Column::Int8(PrimitiveColumn::new(vec![13]))],
            1,
        ))],
        1,
    )
}

#[test]
fn tuple_illegal_element_names_are_rejected() {
    // Mirror the server's checkTupleNames: an empty name and the reserved
    // exact-lowercase "null" cannot exist on the server, so they are
    // UnsupportedType. The decode parser round-trips these shapes (a
    // server-authored header is preserved), so the type-string round-trip
    // check cannot catch them; the explicit name check must.
    for bad in [Some(""), Some("null")] {
        match encode_block(&named_tuple_batch(bad), &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { column, .. } => assert_eq!(column, "t"),
            other => panic!("expected UnsupportedType for {bad:?}, got {other:?}"),
        }
    }
    // Any-case variants other than exact-lowercase "null" are legal on the
    // server (checkTupleNames compares exactly) and render backtick-quoted.
    for ok in [Some("NULL"), Some("Null"), Some("a"), None] {
        encode_block(&named_tuple_batch(ok), &EncodeOptions::default())
            .unwrap_or_else(|e| panic!("{ok:?} should encode: {e}"));
    }
}

#[test]
fn tuple_duplicate_element_names_are_rejected() {
    // checkTupleNames rejects duplicates (DUPLICATE_COLUMN). Unnamed
    // elements do not count as duplicates of each other.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![
                (Some("a".to_string()), ChType::Int8),
                (Some("a".to_string()), ChType::Int8),
            ]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![
                Column::Int8(PrimitiveColumn::new(vec![13])),
                Column::Int8(PrimitiveColumn::new(vec![79])),
            ],
            1,
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::UnsupportedType { column, .. } => assert_eq!(column, "t"),
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

#[test]
fn tuple_mixed_named_unnamed_elements_are_rejected() {
    // The server's tuple type factory rejects mixed named/unnamed
    // arguments ("Names are specified not for all elements of Tuple
    // type"), so a mixed ChType is caller-constructed-only and its
    // rendered header cannot be parsed back by the server.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![
                (Some("a".to_string()), ChType::Int8),
                (None, ChType::Int8),
            ]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![
                Column::Int8(PrimitiveColumn::new(vec![13])),
                Column::Int8(PrimitiveColumn::new(vec![79])),
            ],
            1,
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::UnsupportedType { column, .. } => assert_eq!(column, "t"),
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

#[test]
fn nested_tuple_illegal_names_are_rejected() {
    // The name legality check applies through nesting: a duplicate-named
    // tuple as an Array element is rejected by the recursive validation.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::Tuple(vec![
                (Some("x".to_string()), ChType::Int8),
                (Some("x".to_string()), ChType::Int8),
            ]))),
        }]),
        vec![Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int8(PrimitiveColumn::new(vec![13])),
                    Column::Int8(PrimitiveColumn::new(vec![79])),
                ],
                1,
            )),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::UnsupportedType { .. } => {}
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

#[test]
fn plain_tuple_validity_with_nulls_is_rejected() {
    // A non-Nullable Tuple field whose TupleColumn carries null-marked
    // validity would have the null map silently dropped (no null map is
    // written for a non-nullable column), so the generic nullability check
    // rejects it, the same as every other non-nullable column type.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int8)]),
        }]),
        vec![Column::Tuple(TupleColumn::new_nullable(
            vec![Column::Int8(PrimitiveColumn::new(vec![13, 0]))],
            2,
            Bitmap::from_ch_null_map(&[0x00, 0x01]),
        ))],
        2,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn map_entries_validity_is_rejected() {
    // The Map entries tuple never carries validity on the wire;
    // encode_map_data writes no null map for it, so a caller-attached
    // bitmap (even all-valid) would be silently dropped. Rejected before
    // any bytes.
    let entries = Column::Tuple(TupleColumn::new_nullable(
        vec![
            Column::Utf8(utf8_column(&[b"a"])),
            Column::Int32(PrimitiveColumn::new(vec![13])),
        ],
        1,
        Bitmap::from_ch_null_map(&[0x00]),
    ));
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        }]),
        vec![Column::Map(MapColumn::new(vec![0, 1], entries))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { detail } => {
            assert!(detail.contains("entries"), "got detail {detail:?}");
        }
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn tuple_field_count_mismatch_is_rejected() {
    // The declared type has two elements; the buffer carries one field
    // column. InconsistentBatch, before any bytes are written.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![Column::Int32(PrimitiveColumn::new(vec![13]))],
            1,
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn tuple_ragged_element_lengths_are_rejected() {
    // Element 0 has two rows, element 1 has one: a ragged tuple would put a
    // misframed stream on the wire (the server's equal-sizes INCORRECT_DATA
    // invariant), so it is InconsistentBatch, not a panic.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![13, 79])),
                Column::Utf8(utf8_column(&[b"user_1"])),
            ],
            2,
        ))],
        2,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn tuple_mismatched_element_buffer_is_rejected() {
    // A declared Int64 element over an Int32 buffer is a wrong-buffer
    // mismatch, not a wrong-width column on the wire.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int64)]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![Column::Int32(PrimitiveColumn::new(vec![13]))],
            1,
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn tuple_type_depth_is_capped_via_worklist() {
    // A pathologically deep caller-constructed type must be rejected by the
    // iterative depth walk before any recursive machinery touches it. Tuple
    // is the multi-child container, so this exercises the worklist path
    // with a depth well past MAX_TYPE_DEPTH.
    let mut ch_type = ChType::Int8;
    let mut column = Column::Int8(PrimitiveColumn::new(vec![13]));
    for _ in 0..(MAX_TYPE_DEPTH * 4) {
        ch_type = ChType::Tuple(vec![(None, ch_type)]);
        column = Column::Tuple(TupleColumn::new(vec![column], 1));
    }
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "deep".into(),
            ch_type,
        }]),
        vec![column],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { detail } => {
            assert!(detail.contains("nesting exceeds"), "got detail {detail:?}");
        }
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn rev0_frames_string_bytes() {
    // Pin the String body framing: one varint length prefix then the raw
    // bytes, per row. One String column "s" with a single row "hi".
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "s".into(),
            ch_type: ChType::String,
        }]),
        vec![Column::Utf8(utf8_column(&[b"hi"]))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b's', // name "s"
        0x06, b'S', b't', b'r', b'i', b'n', b'g', // type "String"
        0x02, b'h', b'i', // value: varint len 2 then "hi"
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_fixed_string_bytes() {
    // Pin the FixedString body framing: contiguous width*num_rows bytes, no
    // per-row length prefix. One FixedString(4) column "fs", single row.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "fs".into(),
            ch_type: ChType::FixedString(4),
        }]),
        vec![Column::FixedBinary(fixed_binary_column(4, &[b"road"]))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x02, b'f', b's', // name "fs"
        0x0E, b'F', b'i', b'x', b'e', b'd', b'S', b't', b'r', b'i', b'n', b'g', b'(', b'4',
        b')', // type "FixedString(4)"
        b'r', b'o', b'a', b'd', // 4 raw bytes, no length prefix
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_uuid_bytes() {
    // Pin the UUID body framing: 16 raw bytes per row, passthrough in wire
    // (UInt128 POD) order, no reordering and no per-row framing. One UUID
    // column "u", single row with 16 distinct bytes, so any byte shuffle on
    // encode would break the exact comparison.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "u".into(),
            ch_type: ChType::Uuid,
        }]),
        vec![Column::Uuid(fixed_binary_column(
            16,
            &[b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x0C\x0D\x0E\x0F\x10"],
        ))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'u', // name "u"
        0x04, b'U', b'U', b'I', b'D', // type "UUID"
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // 16 raw bytes,
        0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, // buffer order
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_ipv4_bytes() {
    // Pin the IPv4 body framing: the standard numeric value written as a
    // little-endian u32, exactly like UInt32. One IPv4 column "ip4", single
    // row 192.168.0.1 = 0xC0A80001, so the wire bytes must be the reversed
    // 01 00 A8 C0 and any big-endian write would fail.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "ip4".into(),
            ch_type: ChType::Ipv4,
        }]),
        vec![Column::Ipv4(PrimitiveColumn::new(vec![3_232_235_521]))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x03, b'i', b'p', b'4', // name "ip4"
        0x04, b'I', b'P', b'v', b'4', // type "IPv4"
        0x01, 0x00, 0xA8, 0xC0, // u32 0xC0A80001 (192.168.0.1), little-endian
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_ipv6_bytes() {
    // Pin the IPv6 body framing: 16 raw bytes per row, verbatim in network
    // byte order, no per-row framing. One IPv6 column "ip6", single row with
    // 16 distinct bytes, so any byte shuffle on encode would break the exact
    // comparison.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "ip6".into(),
            ch_type: ChType::Ipv6,
        }]),
        vec![Column::Ipv6(fixed_binary_column(
            16,
            &[b"\x20\x01\x0D\xB8\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x13"],
        ))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x03, b'i', b'p', b'6', // name "ip6"
        0x04, b'I', b'P', b'v', b'6', // type "IPv6"
        0x20, 0x01, 0x0D, 0xB8, 0x01, 0x02, 0x03, 0x04, // 16 raw bytes,
        0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x13, // network order
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_decimal_bytes() {
    // Pin the Decimal body framing: contiguous width*num_rows bytes, no
    // per-row length prefix, precision, or scale. The single Decimal(9, 4)
    // value is unscaled -13, little-endian two's-complement i32.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "d".into(),
            ch_type: ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            },
        }]),
        vec![Column::Decimal(DecimalColumn::new(
            (-13i32).to_le_bytes().to_vec(),
            4,
            9,
            4,
        ))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'd', // name "d"
        0x0D, b'D', b'e', b'c', b'i', b'm', b'a', b'l', b'(', b'9', b',', b' ', b'4',
        b')', // type "Decimal(9, 4)"
        0xF3, 0xFF, 0xFF, 0xFF, // i32 -13, little-endian two's-complement
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_int128_bytes() {
    // Pin the wide-int body framing: 16 raw bytes per row, passthrough in
    // wire (little-endian) order, no reordering and no per-row framing. One
    // Int128 column "w", single row with 16 distinct bytes so any byte
    // shuffle or byteswap on encode would break the exact comparison.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "w".into(),
            ch_type: ChType::Int128,
        }]),
        vec![Column::Int128(fixed_binary_column(
            16,
            &[b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x0C\x0D\x0E\x0F\x10"],
        ))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'w', // name "w"
        0x06, b'I', b'n', b't', b'1', b'2', b'8', // type "Int128"
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // 16 raw bytes,
        0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, // buffer order
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_uint256_bytes() {
    // Pin the 32-byte wide-int body framing. One UInt256 column "w", single
    // row whose only set byte is the most-significant (b[31] = 0x80 = 2^255):
    // it must land at the END of the 32-byte run, proving little-endian
    // passthrough and that the unsigned high bit is not treated as a sign.
    let mut value = [0u8; 32];
    value[31] = 0x80;
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "w".into(),
            ch_type: ChType::UInt256,
        }]),
        vec![Column::UInt256(fixed_binary_column(32, &[&value]))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let mut expected = vec![
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'w', // name "w"
        0x07, b'U', b'I', b'n', b't', b'2', b'5', b'6', // type "UInt256"
    ];
    expected.extend_from_slice(&value); // 31 zero bytes then 0x80
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_low_cardinality_string_bytes() {
    // Pin the LowCardinality body framing at rev 0. The server-confirmed
    // Native index word sets both HasAdditionalKeysBit and
    // NeedUpdateDictionary, so a UInt8-index block writes 0x600.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::String)),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![1],
            Column::Utf8(utf8_column(&[b"", b"user_1"])),
        ))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x02, b'l', b'c', // name "lc"
        0x16, b'L', b'o', b'w', b'C', b'a', b'r', b'd', b'i', b'n', b'a', b'l', b'i', b't', b'y',
        b'(', b'S', b't', b'r', b'i', b'n', b'g', b')',
        // LowCardinality key version = 1.
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // index_word = 0x600: UInt8 tag, HasAdditionalKeysBit,
        // NeedUpdateDictionary.
        0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, // num_keys = 2
        0x00, // dictionary[0] = ""
        0x06, b'u', b's', b'e', b'r', b'_', b'1', // dictionary[1]
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // row count = 1
        0x01, // row index = 1
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_low_cardinality_zero_rows_without_payload() {
    // Zero-row Native blocks write only the column header. The server skips
    // writeData entirely, so there is no LowCardinality key-version prefix.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::String)),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![],
            Column::Utf8(utf8_column(&[])),
        ))],
        0,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x00, // num_rows = 0
        0x02, b'l', b'c', // name "lc"
        0x16, b'L', b'o', b'w', b'C', b'a', b'r', b'd', b'i', b'n', b'a', b'l', b'i', b't', b'y',
        b'(', b'S', b't', b'r', b'i', b'n', b'g', b')',
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_array_int32_bytes() {
    // Pin the Array body framing: one raw LE u64 cumulative end-offset per
    // row with NO leading zero and no count, then the flattened element
    // body. Two rows [13, 79] and [] (the empty row repeats the previous
    // end-offset).
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::Int32)),
        }]),
        vec![Column::Array(ArrayColumn::new(
            vec![0, 2, 2],
            Column::Int32(PrimitiveColumn::new(vec![13, 79])),
        ))],
        2,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x01, b'a', // name "a"
        0x0C, b'A', b'r', b'r', b'a', b'y', b'(', b'I', b'n', b't', b'3', b'2',
        b')', // type "Array(Int32)"
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 0 = 2
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 1 = 2
        0x0D, 0x00, 0x00, 0x00, // Int32 13, little-endian
        0x4F, 0x00, 0x00, 0x00, // Int32 79, little-endian
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_array_low_cardinality_all_empty_bytes() {
    // Pin the all-empty Array(LowCardinality(String)) shape: the hoisted LC
    // key version FIRST (the element state prefix, before the offsets), then
    // the all-zero offsets, then NOTHING for the LC element run
    // (`SerializationLowCardinality::serializeBinaryBulkWithMultipleStreams`
    // early-returns at limit == 0; confirmed at v26.6.1.1193-stable). An
    // index word, key count, or row count here would make the server
    // misparse the INSERT.
    let bytes = encode_block(
        &array_low_cardinality_all_empty_batch(),
        &EncodeOptions::default(),
    )
    .unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x03, b'a', b'l', b'c', // name "alc"
        0x1D, b'A', b'r', b'r', b'a', b'y', b'(', b'L', b'o', b'w', b'C', b'a', b'r', b'd', b'i',
        b'n', b'a', b'l', b'i', b't', b'y', b'(', b'S', b't', b'r', b'i', b'n', b'g', b')',
        b')', // type "Array(LowCardinality(String))"
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // hoisted LC key version = 1
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 0 = 0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, // offset row 1 = 0
              // nothing else: zero-length LC element run writes no body
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn low_cardinality_index_width_selects_self_describing_widths() {
    assert_eq!(low_cardinality_index_width(0), (1, 0));
    assert_eq!(low_cardinality_index_width(255), (1, 0));
    assert_eq!(low_cardinality_index_width(256), (2, 1));
    assert_eq!(low_cardinality_index_width(65_535), (2, 1));
    assert_eq!(low_cardinality_index_width(65_536), (4, 2));
}

#[test]
fn low_cardinality_negative_index_is_rejected() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::String)),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![-1],
            Column::Utf8(utf8_column(&[b"", b"user_1"])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn low_cardinality_out_of_range_index_is_rejected() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::UInt32)),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![2],
            Column::UInt32(PrimitiveColumn::new(vec![0, 13])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn low_cardinality_nullable_valid_index_zero_is_rejected() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "lcn".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String)))),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new_nullable(
            vec![0],
            Column::Utf8(utf8_column(&[b"", b"user_1"])),
            Bitmap::from_ch_null_map(&[0]),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn low_cardinality_nullable_null_nonzero_index_is_rejected() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "lcn".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::UInt32)))),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new_nullable(
            vec![1],
            Column::UInt32(PrimitiveColumn::new(vec![0, 13])),
            Bitmap::from_ch_null_map(&[1]),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn low_cardinality_dictionary_type_mismatch_is_rejected() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::String)),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![1],
            Column::UInt32(PrimitiveColumn::new(vec![0, 13])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn low_cardinality_zero_rows_nonempty_dictionary_is_rejected() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::String)),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![],
            Column::Utf8(utf8_column(&[b"", b"user_1"])),
        ))],
        0,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn uuid_width_mismatch_is_rejected() {
    // A UUID buffer whose stored width is not 16 would put the wrong number of
    // bytes per row on the wire under a truthful type string; reject it before
    // any bytes are written, mirroring the FixedString width guard.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "u".into(),
            ch_type: ChType::Uuid,
        }]),
        vec![Column::Uuid(FixedBinaryColumn::new(vec![0u8; 8], 8))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn ipv6_ragged_data_is_rejected() {
    // An IPv6 buffer whose byte count is not exactly 16 * num_rows reports the
    // right row count via truncating division but would misframe the stream;
    // reject it, mirroring the FixedString ragged-data guard.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "ip6".into(),
            ch_type: ChType::Ipv6,
        }]),
        columns: vec![Column::Ipv6(FixedBinaryColumn::new(vec![0u8; 17], 16))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn decimal_width_mismatch_is_rejected() {
    // Decimal(9, 4) is 4 bytes per row by precision, so a width-8 buffer
    // would misframe the body under the truthful type string.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "dec".into(),
            ch_type: ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            },
        }]),
        vec![Column::Decimal(DecimalColumn::new(vec![0u8; 8], 8, 9, 4))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn decimal_ragged_data_is_rejected() {
    // A Decimal buffer whose byte count is not exactly width * num_rows
    // reports the right row count via truncating division but would put too
    // many bytes on the wire.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "dec".into(),
            ch_type: ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            },
        }]),
        columns: vec![Column::Decimal(DecimalColumn::new(vec![0u8; 7], 4, 9, 4))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn decimal_metadata_mismatch_is_rejected() {
    // The schema and DecimalColumn metadata must agree so downstream buffer
    // consumers see the same precision and scale as the type header.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "dec".into(),
            ch_type: ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            },
        }]),
        vec![Column::Decimal(DecimalColumn::new(vec![0u8; 4], 4, 9, 2))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn nullable_decimal_ragged_data_is_rejected() {
    // The Decimal body guard must apply inside `Nullable` too, after the
    // value type is unwrapped.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "dec".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            })),
        }]),
        columns: vec![Column::Decimal(DecimalColumn::new_nullable(
            vec![0u8; 7],
            4,
            9,
            4,
            Bitmap::from_ch_null_map(&[0]),
        ))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn wide_int_width_mismatch_is_rejected() {
    // Int128 is 16 bytes per row, so a width-32 buffer misframes the body
    // under the truthful type string.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "w".into(),
            ch_type: ChType::Int128,
        }]),
        vec![Column::Int128(FixedBinaryColumn::new(vec![0u8; 32], 32))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn wide_int_ragged_data_is_rejected() {
    // A wide-int buffer whose byte count is not exactly width * num_rows
    // reports the right row count via truncating division but would put too
    // many bytes on the wire.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "w".into(),
            ch_type: ChType::UInt256,
        }]),
        columns: vec![Column::UInt256(FixedBinaryColumn::new(vec![0u8; 40], 32))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn wide_int_signedness_variant_mismatch_is_rejected() {
    // An Int128 type over a UInt128 buffer is a mismatched column variant:
    // the four wide-int types map 1:1 to their Column variants, so this is
    // caught before any bytes are written even though both are width 16.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "w".into(),
            ch_type: ChType::Int128,
        }]),
        vec![Column::UInt128(FixedBinaryColumn::new(vec![0u8; 16], 16))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn rev0_frames_bool_bytes() {
    // Pin the Bool body framing: one byte per row, 0x01 = true, 0x00 = false.
    // One Bool column "b" over three rows: true, false, true.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "b".into(),
            ch_type: ChType::Bool,
        }]),
        vec![Column::Bool(BoolColumn::from_wire_bytes(&[1, 0, 1]))],
        3,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x03, // num_rows = 3
        0x01, b'b', // name "b"
        0x04, b'B', b'o', b'o', b'l', // type "Bool"
        0x01, 0x00, 0x01, // one byte per row: true, false, true
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_nullable_bytes() {
    // Pin the Nullable framing: the per-row null map (0x00 valid, 0x01 null)
    // precedes the inner values. One Nullable(Int32) column "n" over two rows:
    // 13 (valid), then a null row (inner value 0).
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Int32)),
        }]),
        vec![Column::Int32(PrimitiveColumn::new_nullable(
            vec![13, 0],
            Bitmap::from_ch_null_map(&[0, 1]),
        ))],
        2,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x01, b'n', // name "n"
        0x0F, b'N', b'u', b'l', b'l', b'a', b'b', b'l', b'e', b'(', b'I', b'n', b't', b'3', b'2',
        b')', // type "Nullable(Int32)"
        0x00, 0x01, // null map: row 0 valid, row 1 null
        0x0D, 0x00, 0x00, 0x00, // Int32 13, little-endian
        0x00, 0x00, 0x00, 0x00, // Int32 placeholder for the null row
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn nullable_validity_length_mismatch_is_rejected() {
    // A Nullable column whose validity bitmap does not cover num_rows would
    // write a null map of the wrong length; reject it as InconsistentBatch
    // before any bytes are written.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Int32)),
        }]),
        vec![Column::Int32(PrimitiveColumn::new_nullable(
            vec![13, 79],
            Bitmap::from_ch_null_map(&[0]), // covers one row, not two
        ))],
        2,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn bool_bitmap_length_mismatch_is_rejected() {
    // A Bool column whose `len` overruns its packed bitmap would panic when
    // unpacked positionally; reject it as InconsistentBatch before any bytes
    // are written. Construct the malformed column directly: `len` claims 100
    // rows but the bitmap holds one byte (room for 8). Row count is consistent
    // (`len` == num_rows), so it passes the row-count check and reaches the
    // bitmap-length guard.
    let col = BoolColumn {
        bitmap: vec![0x01],
        len: 100,
        validity: None,
    };
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "b".into(),
            ch_type: ChType::Bool,
        }]),
        vec![Column::Bool(col)],
        100,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn fixed_string_width_mismatch_is_rejected() {
    // A FixedString(4) type string paired with a width-3 buffer would emit a
    // body with the wrong bytes-per-row; reject it rather than corrupt.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "fs".into(),
            ch_type: ChType::FixedString(4),
        }]),
        columns: vec![Column::FixedBinary(fixed_binary_column(3, &[b"abc"]))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
#[cfg(not(debug_assertions))]
fn short_validity_buffer_is_rejected() {
    // A caller can build a `Bitmap` whose backing buffer is too short for its
    // bit length via the public `Bitmap::from_raw`, which only debug-asserts the
    // invariant. In a release build that bitmap would panic when
    // `encode_null_map` unpacks it (index out of bounds), so `validate_column`
    // must reject it as an inconsistent batch first. This test is release-only:
    // in a debug build `from_raw`'s `debug_assert!` fires at construction, so the
    // malformed state cannot be reached through the public API. 100 rows need 13
    // bitmap bytes; the buffer holds 1.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Int32)),
        }]),
        columns: vec![Column::Int32(PrimitiveColumn::new_nullable(
            vec![0i32; 100],
            Bitmap::from_raw(vec![0u8; 1], 100),
        ))],
        num_rows: 100,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn fixed_string_ragged_data_is_rejected() {
    // A FixedString(4) column whose data buffer is not a multiple of the width
    // (7 bytes) reports len() == 1 via truncating division, so it passes the
    // row-count check, but writing it verbatim would put 7 bytes where the
    // reader consumes 4, silently misframing the stream. Reject it before
    // writing. Construct directly so `ColBatch::new`'s debug_assert (which uses
    // the same truncating len()) does not mask it.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "fs".into(),
            ch_type: ChType::FixedString(4),
        }]),
        columns: vec![Column::FixedBinary(FixedBinaryColumn::new(
            b"road12X".to_vec(),
            4,
        ))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn nullable_fixed_string_ragged_data_is_rejected() {
    // The same ragged-buffer misframe under a `Nullable(FixedString(4))` must
    // also be rejected: the value type is unwrapped before the width and
    // byte-count checks, so the guard applies inside the wrapper too.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "fs".into(),
            ch_type: ChType::Nullable(Box::new(ChType::FixedString(4))),
        }]),
        columns: vec![Column::FixedBinary(FixedBinaryColumn::new_nullable(
            b"road12X".to_vec(),
            4,
            Bitmap::from_ch_null_map(&[0]),
        ))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

/// Build a one-column `String` batch directly from raw offsets and data so a
/// malformed offset array reaches the encoder (the `utf8_column` helper always
/// builds well-formed offsets). `num_rows` is set from the offsets so only the
/// offset invariants, not the row-count check, are exercised.
fn string_batch_from_parts(offsets: Vec<i32>, data: Vec<u8>, num_rows: usize) -> ColBatch {
    ColBatch {
        schema: Schema::new(vec![Field {
            name: "s".into(),
            ch_type: ChType::String,
        }]),
        columns: vec![Column::Utf8(Utf8Column::new(offsets, data))],
        num_rows,
    }
}

#[test]
fn non_monotonic_string_offsets_are_rejected() {
    // Offsets that decrease would make `encode_string_data` slice `data[3..1]`,
    // which panics. Reject before writing.
    let batch = string_batch_from_parts(vec![0, 3, 1], b"abc".to_vec(), 2);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn string_offset_past_data_is_rejected() {
    // A final offset past `data.len()` would slice out of bounds and panic.
    let batch = string_batch_from_parts(vec![0, 10], b"abc".to_vec(), 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn negative_string_offset_is_rejected() {
    // A negative offset wraps to a huge `usize` in `encode_string_data`. The
    // monotonic check (from a zero start) catches it before that can happen.
    let batch = string_batch_from_parts(vec![0, -1], Vec::new(), 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn trailing_string_data_is_rejected() {
    // Offsets that end before `data.len()` would silently drop the trailing
    // bytes from the wire. Reject rather than lose data (review item 4).
    let batch = string_batch_from_parts(vec![0, 2], b"abcd".to_vec(), 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn leading_string_slack_is_rejected() {
    // A nonzero first offset would silently drop the leading data bytes and
    // violates the Arrow convention that offsets start at 0.
    let batch = string_batch_from_parts(vec![2, 4], b"abcd".to_vec(), 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn type_string_buffer_mismatch_is_rejected() {
    // A supported numeric declared under a mismatched buffer variant must
    // error, never emit a wrong-width body under a truthful type string.
    // Here the type string would be "Int64" (8 bytes/row) but the buffer is a
    // 4-byte i32. Construct directly so `ColBatch::new`'s debug_assert on
    // column length (both are len 1) does not mask the type mismatch.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Int64,
        }]),
        columns: vec![Column::Int32(PrimitiveColumn::new(vec![13]))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

/// Build a one-column `Array(Int32)` batch directly from raw offsets and
/// leaf values so a malformed offset array reaches the encoder. `num_rows`
/// is passed explicitly so only the Array invariants under test, not the
/// row-count check, are exercised.
fn array_batch_from_parts(offsets: Vec<i64>, values: Vec<i32>, num_rows: usize) -> ColBatch {
    ColBatch {
        schema: Schema::new(vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::Int32)),
        }]),
        columns: vec![Column::Array(ArrayColumn::new(
            offsets,
            Column::Int32(PrimitiveColumn::new(values)),
        ))],
        num_rows,
    }
}

#[test]
fn array_non_monotonic_offsets_are_rejected() {
    // Decreasing offsets would frame a stream the server rejects with
    // INCORRECT_DATA (and would slice out of bounds on our own decode).
    let batch = array_batch_from_parts(vec![0, 3, 1], vec![13, 79, 21], 2);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn array_offset_element_count_mismatch_is_rejected() {
    // A final offset that does not equal the flattened element count would
    // either silently drop trailing elements (too small) or declare
    // elements the body does not carry (too large). Both directions.
    for (offsets, values) in [
        (vec![0i64, 2], vec![13, 79, 21]), // ends at 2, holds 3
        (vec![0i64, 3], vec![13, 79]),     // ends at 3, holds 2
    ] {
        let batch = array_batch_from_parts(offsets, values, 1);
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }
}

#[test]
fn array_missing_leading_zero_offset_is_rejected() {
    // Arrow list offsets start at 0; a nonzero first offset means the
    // leading zero is missing and row 0's slice would drop leading elements.
    let batch = array_batch_from_parts(vec![1, 3], vec![13, 79, 21], 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn array_wrong_offsets_length_is_rejected() {
    // An empty offsets vector reports 0 rows through the saturating len()
    // and so passes the row-count check at num_rows = 0, but it is not the
    // well-formed `[0]` shape; the explicit num_rows + 1 length check
    // rejects it before `offsets[0]` is read.
    let batch = array_batch_from_parts(vec![], vec![], 0);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn array_negative_offset_is_rejected() {
    // A negative offset would wrap through the i64 -> u64 cast into a huge
    // wire offset. The monotonic check from the zero start catches it.
    let batch = array_batch_from_parts(vec![0, -1], vec![], 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn array_element_validation_failure_is_rejected() {
    // Element-level guards must apply to the flattened element column: a
    // String element whose Utf8 offsets point past its data buffer would
    // panic mid-write, so the recursive element validation rejects it
    // through the Array before any bytes are written.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::String)),
        }]),
        columns: vec![Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::Utf8(Utf8Column::new(vec![0, 10], b"abc".to_vec())),
        ))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn over_deep_type_nesting_is_rejected() {
    // Encode input never passes through `parse_ch_type`, so its
    // MAX_TYPE_DEPTH cap does not protect the encoder: a caller-constructed
    // pathologically deep type would recurse one stack frame per wrapper
    // level in `column_variant_matches` / `is_encodable` / `Display` /
    // `write_state_prefix` and overflow the stack. The iterative depth walk
    // in `validate_column` rejects it first, and does so without cloning or
    // rendering the deep type (both recurse to full depth), which is why the
    // rejection is InconsistentBatch rather than UnsupportedType.
    let mut ch_type = ChType::Int32;
    for _ in 0..(MAX_TYPE_DEPTH + 100) {
        ch_type = ChType::Array(Box::new(ch_type));
    }
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "deep".into(),
            ch_type,
        }]),
        columns: vec![Column::Int32(PrimitiveColumn::new(vec![]))],
        num_rows: 0,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { detail } => {
            assert!(detail.contains("nesting"), "unexpected detail: {detail}");
        }
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn array_forbidden_low_cardinality_element_is_unsupported() {
    // Decimal is encodable as a plain column but forbidden inside
    // LowCardinality (`canBeInsideLowCardinality()` is false), and nesting
    // that LC inside an Array must not launder it: the element validation
    // recurses and reports UnsupportedType before any bytes are written.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(
                ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 32,
                },
            )))),
        }]),
        columns: vec![Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::Dictionary(DictionaryColumn::new(
                vec![0],
                Column::Decimal(DecimalColumn::new(vec![0u8; 4], 4, 9, 4)),
            )),
        ))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::UnsupportedType { .. } => {}
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

#[test]
fn unrepresentable_type_string_is_rejected() {
    // A DateTime64/Time64 precision above 9, invalid Decimal metadata, and
    // FixedString(0) are constructible ChTypes whose rendered type string
    // this crate's parser and the server reject or normalize differently.
    // Encoding must fail at the source (InconsistentBatch) rather than emit a
    // header that fails to decode downstream, or worse, a Decimal header whose
    // server-derived width disagrees with the body width.
    let dt64 = ColBatch {
        schema: Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::DateTime64 {
                precision: 200,
                timezone: None,
            },
        }]),
        columns: vec![Column::DateTime64(PrimitiveColumn::new(vec![0]))],
        num_rows: 1,
    };
    match encode_block(&dt64, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch for DateTime64(200), got {other:?}"),
    }

    let time64 = ColBatch {
        schema: Schema::new(vec![Field {
            name: "t64".into(),
            ch_type: ChType::Time64 { precision: 200 },
        }]),
        columns: vec![Column::Time64(PrimitiveColumn::new(vec![0]))],
        num_rows: 1,
    };
    match encode_block(&time64, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch for Time64(200), got {other:?}"),
    }

    let decimal_cases = [
        (
            "Decimal(9, 4) with 64 bits",
            ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 64,
            },
            DecimalColumn::new(vec![0u8; 8], 8, 9, 4),
        ),
        (
            "Decimal(100, 4)",
            ChType::Decimal {
                precision: 100,
                scale: 4,
                bits: 128,
            },
            DecimalColumn::new(vec![0u8; 16], 16, 100, 4),
        ),
        (
            "Decimal(9, 20)",
            ChType::Decimal {
                precision: 9,
                scale: 20,
                bits: 32,
            },
            DecimalColumn::new(vec![0u8; 4], 4, 9, 20),
        ),
    ];
    for (label, ch_type, column) in decimal_cases {
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "dec".into(),
                ch_type,
            }]),
            columns: vec![Column::Decimal(column)],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch for {label}, got {other:?}"),
        }
    }

    let fs0 = ColBatch {
        schema: Schema::new(vec![Field {
            name: "fs".into(),
            ch_type: ChType::FixedString(0),
        }]),
        columns: vec![Column::FixedBinary(FixedBinaryColumn::new(vec![], 0))],
        num_rows: 0,
    };
    match encode_block(&fs0, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch for FixedString(0), got {other:?}"),
    }
}

#[test]
fn timezone_with_quote_is_rejected() {
    // A DateTime/DateTime64 timezone containing a single quote renders a
    // malformed header (`DateTime('UTC')')`) that the crate's lenient parser
    // round-trips but the server rejects. It must be caught at the source. Both
    // the bare and Nullable forms, and both temporal types, are covered.
    let cases = [
        ChType::DateTime {
            timezone: Some("UTC')".into()),
        },
        ChType::Nullable(Box::new(ChType::DateTime {
            timezone: Some("UTC')".into()),
        })),
        ChType::DateTime64 {
            precision: 3,
            timezone: Some("UTC')".into()),
        },
    ];
    for ch_type in cases {
        let is_nullable = matches!(ch_type, ChType::Nullable(_));
        let column = match ch_type.inner() {
            ChType::DateTime { .. } => {
                let p = PrimitiveColumn::new(vec![0u32]);
                Column::DateTime(p)
            }
            ChType::DateTime64 { .. } => Column::DateTime64(PrimitiveColumn::new(vec![0i64])),
            other => panic!("unexpected inner {other:?}"),
        };
        // Give a nullable case an all-valid bitmap so only the timezone check fires.
        let column = if is_nullable {
            match column {
                Column::DateTime(mut p) => {
                    p.validity = Some(Bitmap::from_ch_null_map(&[0]));
                    Column::DateTime(p)
                }
                other => other,
            }
        } else {
            column
        };
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "t".into(),
                ch_type,
            }]),
            columns: vec![column],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }
}

#[test]
fn non_nullable_with_nulls_is_rejected() {
    // A non-Nullable field with a validity bitmap that marks a row null writes
    // no null map, so the null would be silently dropped and its placeholder
    // value encoded as real data. Reject it.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Int32,
        }]),
        columns: vec![Column::Int32(PrimitiveColumn::new_nullable(
            vec![13, 79],
            Bitmap::from_ch_null_map(&[0, 1]), // row 1 null under a non-Nullable field
        ))],
        num_rows: 2,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn non_nullable_all_valid_bitmap_is_accepted() {
    // A validity bitmap with no nulls under a non-Nullable field carries no null
    // information to lose, so it encodes fine (and round-trips: the decoder
    // produces a non-nullable column with no validity).
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Int32,
        }]),
        columns: vec![Column::Int32(PrimitiveColumn::new_nullable(
            vec![13, 79],
            Bitmap::from_ch_null_map(&[0, 0]), // all valid
        ))],
        num_rows: 2,
    };
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    match decoded.chunks[0].column(0) {
        Column::Int32(c) => assert_eq!(c.values, vec![13, 79]),
        other => panic!("expected Int32, got {other:?}"),
    }
}

#[test]
fn encode_chunked_rejects_mismatched_chunk_schema() {
    // A chunk whose schema differs from the batch schema would encode a block
    // the server rejects mid-insert. Reject before writing anything.
    let field = |name: &str| Field {
        name: name.into(),
        ch_type: ChType::Int32,
    };
    let chunk = |name: &str, vals: Vec<i32>| {
        let n = vals.len();
        std::sync::Arc::new(ColBatch::new(
            Schema::new(vec![field(name)]),
            vec![Column::Int32(PrimitiveColumn::new(vals))],
            n,
        ))
    };
    let batch = ChunkedBatch {
        schema: Schema::new(vec![field("n")]),
        chunks: vec![chunk("n", vec![13, 14]), chunk("m", vec![15])],
    };
    match encode_chunked(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn inconsistent_batch_is_rejected() {
    // Build a batch whose column length disagrees with num_rows. Bypass
    // `ColBatch::new` (its debug_assert would fire) by constructing directly.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Int32,
        }]),
        columns: vec![Column::Int32(PrimitiveColumn::new(vec![1, 2]))],
        num_rows: 3,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// SimpleAggregateFunction / geo aliases / Nested encode
// -----------------------------------------------------------------------

/// `SimpleAggregateFunction(sum, Float64)` over three rows; the column buffer
/// is the physical Float64 (no new Column variant).
fn simple_aggregate_function_scalar_batch() -> ColBatch {
    let fields = vec![Field {
        name: "s".into(),
        ch_type: ChType::SimpleAggregateFunction {
            func: "sum".into(),
            inner: Box::new(ChType::Float64),
        },
    }];
    let columns = vec![Column::Float64(PrimitiveColumn::new(vec![3.5, -7.25, 0.0]))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// `SimpleAggregateFunction(anyLast, Nullable(String))` over three rows: the
/// alias sits over `Nullable(String)`, so nullability is read off the
/// delegate and the null map precedes the string body.
fn simple_aggregate_function_nullable_batch() -> ColBatch {
    let mut col = utf8_column(&[b"user_1", b"", b"user_2"]);
    col.validity = Some(Bitmap::from_ch_null_map(&[0, 1, 0]));
    let fields = vec![Field {
        name: "s".into(),
        ch_type: ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        },
    }];
    ColBatch::new(Schema::new(fields), vec![Column::Utf8(col)], 3)
}

/// `SimpleAggregateFunction(anyLast, LowCardinality(Nullable(String)))` over
/// four rows (valid, null, valid, null): the shared LC gate under a SAF
/// alias, nullable at the index level.
fn simple_aggregate_function_low_cardinality_batch() -> ColBatch {
    let fields = vec![Field {
        name: "s".into(),
        ch_type: ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::LowCardinality(Box::new(ChType::Nullable(
                Box::new(ChType::String),
            )))),
        },
    }];
    let columns = vec![Column::Dictionary(DictionaryColumn::new_nullable(
        vec![1, 0, 2, 0],
        Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
        Bitmap::from_ch_null_map(&[0, 1, 0, 1]),
    ))];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// `LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))` over
/// four rows (valid, null, valid, null). This is the previously-failing
/// shape: the SAF sits between the `LowCardinality` and its removeNullable
/// `Nullable`, so nullability and the dictionary value type must be resolved
/// through the full SAF chain, not a single-level see-through. Index 0 is the
/// NULL sentinel. Confirmed a real server header live at v26.6.1.1193-stable.
fn low_cardinality_saf_nullable_string_batch() -> ColBatch {
    let fields = vec![Field {
        name: "lc_nsaf".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        })),
    }];
    let columns = vec![Column::Dictionary(DictionaryColumn::new_nullable(
        vec![1, 0, 2, 0],
        Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
        Bitmap::from_ch_null_map(&[0, 1, 0, 1]),
    ))];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// The same `LowCardinality(SimpleAggregateFunction(anyLast,
/// Nullable(String)))` type over three all-valid rows (no index-0 references),
/// still nullable at the type level.
fn low_cardinality_saf_nullable_string_all_valid_batch() -> ColBatch {
    let fields = vec![Field {
        name: "lc_nsaf".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        })),
    }];
    let columns = vec![Column::Dictionary(DictionaryColumn::new_nullable(
        vec![1, 2, 1],
        Column::Utf8(utf8_column(&[b"", b"user_3", b"user_4"])),
        Bitmap::from_ch_null_map(&[0, 0, 0]),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// A chained `SimpleAggregateFunction` as a `LowCardinality` inner:
/// `LowCardinality(SimpleAggregateFunction(anyLast,
/// SimpleAggregateFunction(sum, UInt64)))`. The full SAF chain resolves to a
/// plain non-nullable `UInt64` dictionary body; a single-level see-through
/// would leave an alias and die at write. The chain is live-constructible at
/// v26.6.1.1193-stable.
fn low_cardinality_chained_saf_batch() -> ColBatch {
    let fields = vec![Field {
        name: "lc_saf2".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::SimpleAggregateFunction {
                func: "sum".into(),
                inner: Box::new(ChType::UInt64),
            }),
        })),
    }];
    let columns = vec![Column::Dictionary(DictionaryColumn::new(
        vec![1, 2, 1],
        Column::UInt64(PrimitiveColumn::new(vec![0, 13, 79])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// A standalone chained `SimpleAggregateFunction(anyLast,
/// SimpleAggregateFunction(sum, UInt64))` over three rows: the buffer is the
/// physical `UInt64` and the whole chain resolves through the delegate.
fn chained_simple_aggregate_function_batch() -> ColBatch {
    let fields = vec![Field {
        name: "saf2".into(),
        ch_type: ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::SimpleAggregateFunction {
                func: "sum".into(),
                inner: Box::new(ChType::UInt64),
            }),
        },
    }];
    let columns = vec![Column::UInt64(PrimitiveColumn::new(vec![
        13,
        79,
        8_589_934_592,
    ]))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// `Point` over two rows: the column buffer is a two-field `Tuple(Float64,
/// Float64)` (unnamed), field-major on the wire.
fn point_batch() -> ColBatch {
    let fields = vec![Field {
        name: "p".into(),
        ch_type: ChType::Geo(GeoKind::Point),
    }];
    let columns = vec![Column::Tuple(TupleColumn::new(
        vec![
            Column::Float64(PrimitiveColumn::new(vec![1.0, 3.0])),
            Column::Float64(PrimitiveColumn::new(vec![2.0, 4.0])),
        ],
        2,
    ))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// `Nullable(Point)` over two rows (valid, null): the tuple-level validity
/// bitmap plus a null-row placeholder point, the ordinary `Nullable(Tuple)`
/// framing.
fn nullable_point_batch() -> ColBatch {
    let fields = vec![Field {
        name: "p".into(),
        ch_type: ChType::Nullable(Box::new(ChType::Geo(GeoKind::Point))),
    }];
    let columns = vec![Column::Tuple(TupleColumn::new_nullable(
        vec![
            Column::Float64(PrimitiveColumn::new(vec![1.0, 0.0])),
            Column::Float64(PrimitiveColumn::new(vec![2.0, 0.0])),
        ],
        2,
        Bitmap::from_ch_null_map(&[0, 1]),
    ))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// `MultiPolygon` over two rows, exercising all four expanded Array/Tuple
/// levels. Row 0 holds one polygon of one ring of two points; row 1 is empty.
fn multi_polygon_batch() -> ColBatch {
    let point = Column::Tuple(TupleColumn::new(
        vec![
            Column::Float64(PrimitiveColumn::new(vec![1.0, 3.0])),
            Column::Float64(PrimitiveColumn::new(vec![2.0, 4.0])),
        ],
        2,
    ));
    let ring = Column::Array(ArrayColumn::new(vec![0, 2], point)); // one ring, two points
    let polygon = Column::Array(ArrayColumn::new(vec![0, 1], ring)); // one ring
    let multi = Column::Array(ArrayColumn::new(vec![0, 1, 1], polygon)); // row0: 1 polygon, row1: empty
    let fields = vec![Field {
        name: "mp".into(),
        ch_type: ChType::Geo(GeoKind::MultiPolygon),
    }];
    ColBatch::new(Schema::new(fields), vec![multi], 2)
}

/// `Nested(a UInt32, b String)` over two rows: delegates to
/// `Array(Tuple(a UInt32, b String))`. Row 0 has two elements, row 1 has one.
fn nested_batch() -> ColBatch {
    let entries = Column::Tuple(TupleColumn::new(
        vec![
            Column::UInt32(PrimitiveColumn::new(vec![10, 20, 30])),
            Column::Utf8(utf8_column(&[b"x", b"y", b"z"])),
        ],
        3,
    ));
    let fields = vec![Field {
        name: "n".into(),
        ch_type: ChType::Nested(vec![
            ("a".into(), ChType::UInt32),
            ("b".into(), ChType::String),
        ]),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(vec![0, 2, 3], entries))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// `Nested(a LowCardinality(String))` over two rows: the shared LC gate, so
/// the LC key version is hoisted to the front of the whole column, ahead of
/// the Array offsets.
fn nested_low_cardinality_batch() -> ColBatch {
    let entries = Column::Tuple(TupleColumn::new(
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1],
            Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
        ))],
        3,
    ));
    let fields = vec![Field {
        name: "n".into(),
        ch_type: ChType::Nested(vec![(
            "a".into(),
            ChType::LowCardinality(Box::new(ChType::String)),
        )]),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(vec![0, 2, 3], entries))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

#[test]
fn roundtrip_simple_aggregate_function_scalar_rev0() {
    roundtrip(&simple_aggregate_function_scalar_batch(), 0);
}

#[test]
fn roundtrip_simple_aggregate_function_scalar_tcp_revision() {
    roundtrip(
        &simple_aggregate_function_scalar_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_simple_aggregate_function_nullable_rev0() {
    roundtrip(&simple_aggregate_function_nullable_batch(), 0);
}

#[test]
fn roundtrip_simple_aggregate_function_nullable_tcp_revision() {
    roundtrip(
        &simple_aggregate_function_nullable_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_simple_aggregate_function_low_cardinality_rev0() {
    roundtrip(&simple_aggregate_function_low_cardinality_batch(), 0);
}

#[test]
fn roundtrip_simple_aggregate_function_low_cardinality_tcp_revision() {
    roundtrip(
        &simple_aggregate_function_low_cardinality_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

// LowCardinality with a SimpleAggregateFunction between the LC and its
// removeNullable Nullable, plus chained SAF. Both were rejected before the
// shared `low_cardinality_dict_value_type` resolution; the null-row case in
// particular used to fail with a misleading InconsistentBatch on encode.

#[test]
fn roundtrip_low_cardinality_saf_nullable_string_rev0() {
    roundtrip(&low_cardinality_saf_nullable_string_batch(), 0);
}

#[test]
fn roundtrip_low_cardinality_saf_nullable_string_tcp_revision() {
    roundtrip(
        &low_cardinality_saf_nullable_string_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_low_cardinality_saf_nullable_string_all_valid_rev0() {
    roundtrip(&low_cardinality_saf_nullable_string_all_valid_batch(), 0);
}

#[test]
fn roundtrip_low_cardinality_saf_nullable_string_all_valid_tcp_revision() {
    roundtrip(
        &low_cardinality_saf_nullable_string_all_valid_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_low_cardinality_chained_saf_rev0() {
    roundtrip(&low_cardinality_chained_saf_batch(), 0);
}

#[test]
fn roundtrip_low_cardinality_chained_saf_tcp_revision() {
    roundtrip(
        &low_cardinality_chained_saf_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_chained_simple_aggregate_function_rev0() {
    roundtrip(&chained_simple_aggregate_function_batch(), 0);
}

#[test]
fn roundtrip_chained_simple_aggregate_function_tcp_revision() {
    roundtrip(
        &chained_simple_aggregate_function_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn encode_low_cardinality_saf_nullable_null_row_succeeds() {
    // The previously-failing shape: encoding a null row of
    // LowCardinality(SAF(anyLast, Nullable(String))) used to be rejected as an
    // InconsistentBatch because nullability was read one SAF level too shallow.
    // It must now encode, and the encoder's own output must decode back.
    let batch = low_cardinality_saf_nullable_string_batch();
    let bytes = encode_block(&batch, &EncodeOptions::default())
        .expect("encoding a null row of LC(SAF(Nullable(String))) must succeed");
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default())
        .expect("decoding the encoder's own LC(SAF(Nullable)) output must succeed");
    assert_eq!(decoded.num_chunks(), 1);
    assert_batches_eq(&batch, &decoded.chunks[0]);
}

#[test]
fn encode_zero_row_low_cardinality_saf_nullable_block() {
    // A zero-row LC(SAF(anyLast, Nullable(String))) block encodes (no column
    // data is written for a zero-row block) and decodes back to just the
    // schema with no chunks, exercising the empty_column delegate path.
    let fields = vec![Field {
        name: "lc_nsaf".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        })),
    }];
    let columns = vec![Column::Dictionary(DictionaryColumn::new_nullable(
        vec![],
        Column::Utf8(utf8_column(&[])),
        Bitmap::from_ch_null_map(&[]),
    ))];
    let batch = ColBatch::new(Schema::new(fields.clone()), columns, 0);
    let bytes = encode_block(&batch, &EncodeOptions::default())
        .expect("encoding a zero-row LC(SAF(Nullable)) block must succeed");
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default())
        .expect("decoding a zero-row LC(SAF(Nullable)) block must succeed");
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema.fields[0].ch_type, fields[0].ch_type);
}

// SimpleAggregateFunction INSIDE wrappers/containers (confirmed legal live at
// v26.6.1.1193-stable). The buffer is the physical delegate's, so encode
// delegates through and the header keeps the verbatim SAF spelling.

/// `Nullable(SimpleAggregateFunction(sum, UInt64))` over three rows (valid,
/// null, valid): the null map precedes the UInt64 run, nullability read off
/// the delegate.
fn nullable_simple_aggregate_function_batch() -> ColBatch {
    let fields = vec![Field {
        name: "s".into(),
        ch_type: ChType::Nullable(Box::new(ChType::SimpleAggregateFunction {
            func: "sum".into(),
            inner: Box::new(ChType::UInt64),
        })),
    }];
    let columns = vec![Column::UInt64(PrimitiveColumn {
        values: vec![13, 0, 79],
        validity: Some(Bitmap::from_ch_null_map(&[0, 1, 0])),
    })];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// `Array(SimpleAggregateFunction(sum, UInt64))` over two rows: `[13, 79]`,
/// `[5]`. Offsets then the flattened UInt64 run.
fn array_simple_aggregate_function_batch() -> ColBatch {
    let fields = vec![Field {
        name: "a".into(),
        ch_type: ChType::Array(Box::new(ChType::SimpleAggregateFunction {
            func: "sum".into(),
            inner: Box::new(ChType::UInt64),
        })),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 3],
        Column::UInt64(PrimitiveColumn::new(vec![13, 79, 5])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// `LowCardinality(SimpleAggregateFunction(anyLast, String))` over three rows:
/// the LC body decodes/encodes as its physical `String` inner.
fn low_cardinality_simple_aggregate_function_batch() -> ColBatch {
    let fields = vec![Field {
        name: "s".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::String),
        })),
    }];
    let columns = vec![Column::Dictionary(DictionaryColumn::new(
        vec![1, 2, 1],
        Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// `Tuple(v SimpleAggregateFunction(sum, UInt64))` over two rows: one field
/// column of the physical UInt64.
fn tuple_simple_aggregate_function_batch() -> ColBatch {
    let fields = vec![Field {
        name: "t".into(),
        ch_type: ChType::Tuple(vec![(
            Some("v".into()),
            ChType::SimpleAggregateFunction {
                func: "sum".into(),
                inner: Box::new(ChType::UInt64),
            },
        )]),
    }];
    let columns = vec![Column::Tuple(TupleColumn::new(
        vec![Column::UInt64(PrimitiveColumn::new(vec![13, 79]))],
        2,
    ))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// `SimpleAggregateFunction(groupArrayLastArray(5), Array(UInt64))` over two
/// rows: a parametrized function name whose balanced `(5)` suffix must survive
/// the header round-trip.
fn simple_aggregate_function_parametrized_batch() -> ColBatch {
    let fields = vec![Field {
        name: "s".into(),
        ch_type: ChType::SimpleAggregateFunction {
            func: "groupArrayLastArray(5)".into(),
            inner: Box::new(ChType::Array(Box::new(ChType::UInt64))),
        },
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 3],
        Column::UInt64(PrimitiveColumn::new(vec![13, 79, 5])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

#[test]
fn roundtrip_nullable_simple_aggregate_function_rev0() {
    roundtrip(&nullable_simple_aggregate_function_batch(), 0);
}

#[test]
fn roundtrip_nullable_simple_aggregate_function_tcp_revision() {
    roundtrip(
        &nullable_simple_aggregate_function_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_array_simple_aggregate_function_rev0() {
    roundtrip(&array_simple_aggregate_function_batch(), 0);
}

#[test]
fn roundtrip_array_simple_aggregate_function_tcp_revision() {
    roundtrip(
        &array_simple_aggregate_function_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_low_cardinality_simple_aggregate_function_rev0() {
    roundtrip(&low_cardinality_simple_aggregate_function_batch(), 0);
}

#[test]
fn roundtrip_low_cardinality_simple_aggregate_function_tcp_revision() {
    roundtrip(
        &low_cardinality_simple_aggregate_function_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_tuple_simple_aggregate_function_rev0() {
    roundtrip(&tuple_simple_aggregate_function_batch(), 0);
}

#[test]
fn roundtrip_tuple_simple_aggregate_function_tcp_revision() {
    roundtrip(
        &tuple_simple_aggregate_function_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_simple_aggregate_function_parametrized_func() {
    // Fix 3 correctness check: encode-then-decode equality for a parametrized
    // function name, proving Display(parse(...)) holds through the header.
    roundtrip(&simple_aggregate_function_parametrized_batch(), 0);
    roundtrip(
        &simple_aggregate_function_parametrized_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn encode_rejects_malformed_simple_aggregate_function_func() {
    // A caller-constructed SAF `func` is untrusted and Displayed into the
    // header's type-string channel, so a malformed spelling is rejected as
    // UnsupportedType before any bytes are written (injection guard). The
    // server's function whitelist is deliberately NOT enforced here.
    for bad_func in [
        "sum, UInt64), evil", // a top-level comma would inject extra type tokens
        "sum(",               // unbalanced open paren
        "sum)",               // stray close paren
        "sum(a))",            // paren imbalance in the params suffix
        "",                   // empty
        "1sum",               // leading digit
        "sum bar",            // embedded space
    ] {
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "s".into(),
                ch_type: ChType::SimpleAggregateFunction {
                    func: bad_func.into(),
                    inner: Box::new(ChType::UInt64),
                },
            }]),
            vec![Column::UInt64(PrimitiveColumn::new(vec![13]))],
            1,
        );
        assert!(
            matches!(
                encode_block(&batch, &EncodeOptions::default()),
                Err(EncodeError::UnsupportedType { .. })
            ),
            "func {bad_func:?} should be rejected as UnsupportedType"
        );
    }

    // A malformed SAF func nested inside a container is caught too: the walk
    // descends every wrapper/container.
    let nested_bad = ColBatch::new(
        Schema::new(vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::SimpleAggregateFunction {
                func: "sum, evil".into(),
                inner: Box::new(ChType::UInt64),
            })),
        }]),
        vec![Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::UInt64(PrimitiveColumn::new(vec![13])),
        ))],
        1,
    );
    assert!(matches!(
        encode_block(&nested_bad, &EncodeOptions::default()),
        Err(EncodeError::UnsupportedType { .. })
    ));
}

#[test]
fn roundtrip_point_rev0() {
    roundtrip(&point_batch(), 0);
}

#[test]
fn roundtrip_point_tcp_revision() {
    roundtrip(&point_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_point_rev0() {
    roundtrip(&nullable_point_batch(), 0);
}

#[test]
fn roundtrip_nullable_point_tcp_revision() {
    roundtrip(&nullable_point_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_multi_polygon_rev0() {
    roundtrip(&multi_polygon_batch(), 0);
}

#[test]
fn roundtrip_multi_polygon_tcp_revision() {
    roundtrip(&multi_polygon_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nested_rev0() {
    roundtrip(&nested_batch(), 0);
}

#[test]
fn roundtrip_nested_tcp_revision() {
    roundtrip(&nested_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nested_low_cardinality_rev0() {
    roundtrip(&nested_low_cardinality_batch(), 0);
}

#[test]
fn roundtrip_nested_low_cardinality_tcp_revision() {
    roundtrip(&nested_low_cardinality_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn multi_block_name_decoration_roundtrips() {
    // Two blocks of the same schema stay separate chunks through encode ->
    // decode.
    let batch = ChunkedBatch {
        schema: point_batch().schema.clone(),
        chunks: vec![
            std::sync::Arc::new(point_batch()),
            std::sync::Arc::new(point_batch()),
        ],
    };
    let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    assert_batches_eq(&point_batch(), &decoded.chunks[0]);
    assert_batches_eq(&point_batch(), &decoded.chunks[1]);
}

#[test]
fn rev0_frames_simple_aggregate_function_bytes() {
    // The header carries the VERBATIM alias spelling and the body is
    // byte-identical to the bare inner Float64.
    let bytes = encode_block(
        &simple_aggregate_function_scalar_batch(),
        &EncodeOptions::default(),
    )
    .unwrap();
    let mut expected = vec![
        0x01, // num_cols = 1
        0x03, // num_rows = 3
        0x01, b's', // name "s"
        0x25, // type string length = 37
    ];
    expected.extend_from_slice(b"SimpleAggregateFunction(sum, Float64)");
    expected.extend_from_slice(&3.5f64.to_le_bytes());
    expected.extend_from_slice(&(-7.25f64).to_le_bytes());
    expected.extend_from_slice(&0.0f64.to_le_bytes());
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_point_bytes() {
    // Point body is the field-major Tuple(Float64, Float64): all X then all Y,
    // no offsets, no tuple-level framing.
    let bytes = encode_block(&point_batch(), &EncodeOptions::default()).unwrap();
    let mut expected = vec![
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x01, b'p', // name "p"
        0x05, b'P', b'o', b'i', b'n', b't', // type "Point"
    ];
    expected.extend_from_slice(&1.0f64.to_le_bytes()); // X row 0
    expected.extend_from_slice(&3.0f64.to_le_bytes()); // X row 1
    expected.extend_from_slice(&2.0f64.to_le_bytes()); // Y row 0
    expected.extend_from_slice(&4.0f64.to_le_bytes()); // Y row 1
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_nested_bytes() {
    // Nested body is the Array(Tuple(a, b)) shape: offsets[1..] as raw LE u64,
    // then the flattened tuple body field-major (all a's then all b's).
    let bytes = encode_block(&nested_batch(), &EncodeOptions::default()).unwrap();
    let mut expected = vec![
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x01, b'n', // name "n"
        0x1A, // type string length = 26
    ];
    expected.extend_from_slice(b"Nested(a UInt32, b String)");
    // Offsets [2, 3] as raw LE u64 (no leading zero).
    expected.extend_from_slice(&2u64.to_le_bytes());
    expected.extend_from_slice(&3u64.to_le_bytes());
    // Field a: UInt32 [10, 20, 30].
    expected.extend_from_slice(&10u32.to_le_bytes());
    expected.extend_from_slice(&20u32.to_le_bytes());
    expected.extend_from_slice(&30u32.to_le_bytes());
    // Field b: String [x, y, z] as varint len + bytes.
    expected.extend_from_slice(&[0x01, b'x', 0x01, b'y', 0x01, b'z']);
    assert_eq!(bytes, expected);
}

#[test]
fn encode_rejects_nested_duplicate_names() {
    // Duplicate element names fail `checkTupleNames` through the Tuple
    // delegation, reported as `UnsupportedType`.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nested(vec![
                ("a".into(), ChType::UInt32),
                ("a".into(), ChType::String),
            ]),
        }]),
        vec![Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::UInt32(PrimitiveColumn::new(vec![10])),
                    Column::Utf8(utf8_column(&[b"x"])),
                ],
                1,
            )),
        ))],
        1,
    );
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::UnsupportedType { .. })
    ));
}

#[test]
fn encode_rejects_nested_empty_name() {
    // An empty element name is unconstructible on the server, rejected via the
    // Tuple delegation.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nested(vec![("".into(), ChType::UInt32)]),
        }]),
        vec![Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::Tuple(TupleColumn::new(
                vec![Column::UInt32(PrimitiveColumn::new(vec![10]))],
                1,
            )),
        ))],
        1,
    );
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::UnsupportedType { .. })
    ));
}

#[test]
fn type_depth_of_alias_matches_physical_delegate() {
    // The encoder's depth cap must count a name-decoration alias at its
    // physical depth so a geo/Nested type near the cap is not under-counted,
    // and it must count it the SAME as the decode parser so a type that
    // decodes is always re-encodable.
    //
    // Geo and Nested are charged exactly their physical expansion depth.
    for kind in [
        GeoKind::Point,
        GeoKind::Ring,
        GeoKind::LineString,
        GeoKind::MultiLineString,
        GeoKind::Polygon,
        GeoKind::MultiPolygon,
    ] {
        let alias = ChType::Geo(kind);
        assert_eq!(
            type_depth(&alias),
            type_depth(&kind.underlying_type()),
            "geo {kind:?} depth mismatch"
        );
        // And the geo token's own charge equals its expansion_depth constant.
        assert_eq!(type_depth(&alias), kind.expansion_depth());
    }
    let nested = ChType::Nested(vec![
        ("a".into(), ChType::UInt32),
        ("b".into(), ChType::Array(Box::new(ChType::String))),
    ]);
    assert_eq!(
        type_depth(&nested),
        type_depth(&nested.physical_delegate().unwrap())
    );
    // SimpleAggregateFunction charges ONE level over its inner (not zero): it
    // expands via one extra decode recursion frame, so both the parser and
    // type_depth charge +1 to bound a hostile chain of nested SAFs and keep
    // the two directions aligned.
    let saf = ChType::SimpleAggregateFunction {
        func: "sum".into(),
        inner: Box::new(ChType::Array(Box::new(ChType::Float64))),
    };
    assert_eq!(
        type_depth(&saf),
        type_depth(&saf.physical_delegate().unwrap()) + 1
    );
}

#[test]
fn decode_accept_implies_encode_accept_at_the_cap() {
    // Fix 4 boundary: any type the decode parser accepts must pass the
    // encoder's type_depth cap, and vice versa, for a geo-tipped and a
    // Nested-tipped chain. Walk Array nesting from just under to just over the
    // point where the alias expansion crosses MAX_TYPE_DEPTH and confirm the
    // two sides flip together.
    for (label, tip, expansion) in [
        ("geo", ChType::Geo(GeoKind::MultiPolygon), 4usize),
        (
            "nested",
            ChType::Nested(vec![("a".into(), ChType::UInt32)]),
            2usize,
        ),
    ] {
        // arrays + expansion must be <= MAX_TYPE_DEPTH to be accepted, so the
        // last accepted array count is MAX_TYPE_DEPTH - expansion.
        let last_ok = MAX_TYPE_DEPTH - expansion;
        for arrays in [last_ok, last_ok + 1] {
            let mut ty = tip.clone();
            for _ in 0..arrays {
                ty = ChType::Array(Box::new(ty));
            }
            let parse_ok = parse_ch_type(&ty.to_string()).is_some();
            let encode_ok = type_depth(&ty) <= MAX_TYPE_DEPTH;
            assert_eq!(
                parse_ok, encode_ok,
                "{label} chain with {arrays} arrays: decode-accept {parse_ok} but encode-accept {encode_ok}"
            );
            // At exactly last_ok both accept; one deeper both reject.
            assert_eq!(parse_ok, arrays == last_ok, "{label} {arrays} arrays");
        }
    }
}

#[test]
fn encode_rejects_geo_type_over_the_depth_cap() {
    // A geo type wrapped in enough Arrays that its physical expansion exceeds
    // MAX_TYPE_DEPTH is rejected as InconsistentBatch (the same iterative cap
    // as any deep caller-constructed type). MultiPolygon adds four physical
    // levels, so wrapping it in MAX_TYPE_DEPTH Arrays pushes it over.
    let mut ty = ChType::Geo(GeoKind::MultiPolygon);
    for _ in 0..MAX_TYPE_DEPTH {
        ty = ChType::Array(Box::new(ty));
    }
    assert!(type_depth(&ty) > MAX_TYPE_DEPTH);
    // The decode parser now charges the geo expansion too, so it rejects the
    // very same over-deep header: the two sides agree instead of the encoder
    // rejecting a type the decoder would have accepted (the old asymmetry).
    assert_eq!(parse_ch_type(&ty.to_string()), None);
    // A one-row batch whose column buffer is irrelevant: the depth check runs
    // first. Use a zero-row batch to avoid building the deep nesting buffer.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "g".into(),
            ch_type: ty,
        }]),
        columns: vec![Column::Array(ArrayColumn::new(vec![0], empty_deep_array()))],
        num_rows: 0,
    };
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}

/// A throwaway element column for the depth-cap rejection test; the depth
/// check fires before the buffer is inspected, so its exact shape does not
/// matter.
fn empty_deep_array() -> Column {
    Column::Array(ArrayColumn::new(
        vec![0],
        Column::Float64(PrimitiveColumn::new(vec![])),
    ))
}
