use super::*;

#[test]
fn test_decode_all_int_widths() {
    let data = BlockBuilder::new()
        .header(4, 2)
        .column_header("a", "Int8")
        .int8_data(&[1, -1])
        .column_header("b", "Int16")
        .int16_data(&[256, -256])
        .column_header("c", "Int32")
        .int32_data(&[70000, -70000])
        .column_header("d", "Int64")
        .int64_data(&[1_000_000_000, -1])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 2);
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Int8(c) => assert_eq!(c.values, vec![1i8, -1]),
        _ => panic!("expected Int8"),
    }
    match batch.column(1) {
        Column::Int16(c) => assert_eq!(c.values, vec![256i16, -256]),
        _ => panic!("expected Int16"),
    }
}

#[test]
fn test_decode_all_uint_widths() {
    let data = BlockBuilder::new()
        .header(4, 1)
        .column_header("a", "UInt8")
        .raw_bytes(&[255u8])
        .column_header("b", "UInt16")
        .raw_bytes(&200u16.to_le_bytes())
        .column_header("c", "UInt32")
        .uint32_data(&[4_000_000_000])
        .column_header("d", "UInt64")
        .uint64_data(&[u64::MAX])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::UInt8(c) => assert_eq!(c.values[0], 255),
        _ => panic!(),
    }
    match batch.column(3) {
        Column::UInt64(c) => assert_eq!(c.values[0], u64::MAX),
        _ => panic!(),
    }
}

#[test]
fn test_decode_float32() {
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("f", "Float32")
        .float32_data(&[1.5f32, -2.25])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Float32(c) => {
            assert_eq!(c.values[0], 1.5f32);
            assert_eq!(c.values[1], -2.25f32);
        }
        _ => panic!("expected Float32"),
    }
}

#[test]
fn test_decode_float64() {
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("f", "Float64")
        .float64_data(&[3.5f64, -7.25, 0.0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Float64(c) => {
            assert_eq!(c.values[0], 3.5f64);
            assert_eq!(c.values[1], -7.25f64);
            assert_eq!(c.values[2], 0.0f64);
        }
        _ => panic!("expected Float64"),
    }
}

#[test]
fn test_decode_bool() {
    let data = BlockBuilder::new()
        .header(1, 5)
        .column_header("b", "Bool")
        .raw_bytes(&[1, 0, 1, 0, 1])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Bool(c) => {
            assert_eq!(c.len(), 5);
            assert!(c.get(0));
            assert!(!c.get(1));
            assert!(c.get(2));
            assert!(!c.get(3));
            assert!(c.get(4));
        }
        _ => panic!("expected Bool"),
    }
}

#[test]
fn test_decode_nullable_int32() {
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("n", "Nullable(Int32)")
        .null_map(&[false, true, false])
        .int32_data(&[100, 0, 300])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Int32(c) => {
            assert_eq!(c.null_count(), 1);
            assert_eq!(c.values, vec![100, 0, 300]);
        }
        _ => panic!("expected Int32"),
    }
}

#[test]
fn test_decode_int128_plain() {
    // Int128 is a raw 16-byte LE two's-complement integer per row. The core
    // has no native i128, so assert the raw byte pattern directly. Sign and
    // boundary values: 13, -1 (all 0xFF), i128::MIN (only the MSB set:
    // b[15] = 0x80), and i128::MAX (all 0xFF then b[15] = 0x7F).
    let thirteen = w16(13);
    let neg_one = [0xFFu8; 16];
    let min = {
        let mut b = [0u8; 16];
        b[15] = 0x80;
        b
    };
    let max = {
        let mut b = [0xFFu8; 16];
        b[15] = 0x7F;
        b
    };
    let rows = [
        thirteen.as_slice(),
        neg_one.as_slice(),
        min.as_slice(),
        max.as_slice(),
    ];
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("w", "Int128")
        .wide_int_data(&rows, 16)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Int128(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.len(), 4);
            assert_eq!(c.value(0), thirteen);
            assert_eq!(c.value(1), neg_one);
            assert_eq!(c.value(2), min);
            assert_eq!(c.value(3), max);
            assert!(c.validity.is_none());
        }
        other => panic!("expected Int128, got {other:?}"),
    }
    assert_eq!(batch.schema.fields[0].ch_type, ChType::Int128);
}

#[test]
fn test_decode_uint128_plain() {
    // UInt128 shares the 16-byte LE layout; the type is unsigned, so a
    // high-bit-set value must survive verbatim (it is NOT a negative). Cover
    // 79, 2^127 (b[15] = 0x80, the high bit), and u128::MAX (all 0xFF).
    let seventy_nine = w16(79);
    let two_pow_127 = {
        let mut b = [0u8; 16];
        b[15] = 0x80;
        b
    };
    let max = [0xFFu8; 16];
    let rows = [
        seventy_nine.as_slice(),
        two_pow_127.as_slice(),
        max.as_slice(),
    ];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("w", "UInt128")
        .wide_int_data(&rows, 16)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::UInt128(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.value(0), seventy_nine);
            assert_eq!(c.value(1), two_pow_127);
            assert_eq!(c.value(2), max);
        }
        other => panic!("expected UInt128, got {other:?}"),
    }
    assert_eq!(batch.schema.fields[0].ch_type, ChType::UInt128);
}

#[test]
fn test_decode_int256_plain() {
    // Int256 is a raw 32-byte LE two's-complement integer per row. Cover 13,
    // -1 (all 0xFF), i256::MIN (b[31] = 0x80) and i256::MAX (all 0xFF then
    // b[31] = 0x7F).
    let thirteen = w32(13);
    let neg_one = [0xFFu8; 32];
    let min = {
        let mut b = [0u8; 32];
        b[31] = 0x80;
        b
    };
    let max = {
        let mut b = [0xFFu8; 32];
        b[31] = 0x7F;
        b
    };
    let rows = [
        thirteen.as_slice(),
        neg_one.as_slice(),
        min.as_slice(),
        max.as_slice(),
    ];
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("w", "Int256")
        .wide_int_data(&rows, 32)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Int256(c) => {
            assert_eq!(c.width, 32);
            assert_eq!(c.len(), 4);
            assert_eq!(c.value(0), thirteen);
            assert_eq!(c.value(1), neg_one);
            assert_eq!(c.value(2), min);
            assert_eq!(c.value(3), max);
        }
        other => panic!("expected Int256, got {other:?}"),
    }
    assert_eq!(batch.schema.fields[0].ch_type, ChType::Int256);
}

#[test]
fn test_decode_uint256_plain() {
    // UInt256 is a raw 32-byte LE unsigned integer per row. A high-bit-set
    // value (b[31] = 0x80 = 2^255) must survive verbatim as a positive value.
    let seventy_nine = w32(79);
    let two_pow_255 = {
        let mut b = [0u8; 32];
        b[31] = 0x80;
        b
    };
    let max = [0xFFu8; 32];
    let rows = [
        seventy_nine.as_slice(),
        two_pow_255.as_slice(),
        max.as_slice(),
    ];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("w", "UInt256")
        .wide_int_data(&rows, 32)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::UInt256(c) => {
            assert_eq!(c.width, 32);
            assert_eq!(c.value(1), two_pow_255);
            assert_eq!(c.value(2), max);
        }
        other => panic!("expected UInt256, got {other:?}"),
    }
    assert_eq!(batch.schema.fields[0].ch_type, ChType::UInt256);
}

#[test]
fn test_decode_nullable_int128() {
    // Nullable(Int128): the null map first, then the 16-byte body, exactly
    // like Nullable(Decimal128). Null rows still carry a placeholder.
    let rows = [
        w16(13),
        w16(0),
        [0xFFu8; 16], // -1
        w16(0),
    ];
    let row_refs: Vec<&[u8]> = rows.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("w", "Nullable(Int128)")
        .null_map(&[false, true, false, true])
        .wide_int_data(&row_refs, 16)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Int128(c) => {
            assert_eq!(c.null_count(), 2);
            assert_eq!(c.value(0), w16(13));
            assert_eq!(c.value(2), [0xFFu8; 16]);
        }
        other => panic!("expected Int128, got {other:?}"),
    }
    assert!(batch.column(0).validity().unwrap().is_valid(0));
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Nullable(Box::new(ChType::Int128))
    );
}

#[test]
fn test_decode_wide_int_zero_rows() {
    // A zero-row block carrying every wide-int type contributes the schema
    // but no chunks; the empty columns have length 0 and keep their width.
    let data = BlockBuilder::new()
        .header(4, 0)
        .column_header("i128", "Int128")
        .column_header("u128", "UInt128")
        .column_header("i256", "Int256")
        .column_header("u256", "UInt256")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 4);
    assert_eq!(cb.schema.fields[0].ch_type, ChType::Int128);
    assert_eq!(cb.schema.fields[1].ch_type, ChType::UInt128);
    assert_eq!(cb.schema.fields[2].ch_type, ChType::Int256);
    assert_eq!(cb.schema.fields[3].ch_type, ChType::UInt256);
}
