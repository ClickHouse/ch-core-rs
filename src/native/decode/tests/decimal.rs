use super::*;

#[test]
fn test_decimal_bits_from_precision_boundaries() {
    // Width is derived from P alone (server `createDecimal`): the byte width
    // jumps at every boundary. Pin each edge so a derivation regression is
    // caught.
    assert_eq!(decimal_bits_from_precision(1), Some(32));
    assert_eq!(decimal_bits_from_precision(9), Some(32));
    assert_eq!(decimal_bits_from_precision(10), Some(64));
    assert_eq!(decimal_bits_from_precision(18), Some(64));
    assert_eq!(decimal_bits_from_precision(19), Some(128));
    assert_eq!(decimal_bits_from_precision(38), Some(128));
    assert_eq!(decimal_bits_from_precision(39), Some(256));
    assert_eq!(decimal_bits_from_precision(76), Some(256));
    // Out of range: P = 0 and P > 76 have no backing integer.
    assert_eq!(decimal_bits_from_precision(0), None);
    assert_eq!(decimal_bits_from_precision(77), None);
}

#[test]
fn test_decode_decimal32_plain() {
    // Decimal32 is a raw 4-byte LE Int32 per row. Include a negative unscaled
    // value (-1) to exercise two's-complement: it must decode to all-0xFF
    // bytes of the width. Precision/scale live in the ChType, not the data.
    let rows: Vec<[u8; 4]> = vec![
        13i32.to_le_bytes(),
        (-1i32).to_le_bytes(),
        0i32.to_le_bytes(),
        i32::MIN.to_le_bytes(),
    ];
    let row_refs: Vec<&[u8]> = rows.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("d", "Decimal(9, 4)")
        .decimal_data(&row_refs, 4)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.width, 4);
            assert_eq!(c.precision, 9);
            assert_eq!(c.scale, 4);
            assert_eq!(c.len(), 4);
            // The raw unscaled integers, read back as i32 LE.
            assert_eq!(i32::from_le_bytes(c.value(0).try_into().unwrap()), 13);
            assert_eq!(i32::from_le_bytes(c.value(1).try_into().unwrap()), -1);
            // -1 is all-0xFF bytes of the width (two's-complement).
            assert_eq!(c.value(1), &[0xFF, 0xFF, 0xFF, 0xFF]);
            assert_eq!(i32::from_le_bytes(c.value(2).try_into().unwrap()), 0);
            assert_eq!(i32::from_le_bytes(c.value(3).try_into().unwrap()), i32::MIN);
            assert!(c.validity.is_none());
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Decimal {
            precision: 9,
            scale: 4,
            bits: 32,
        }
    );
}

#[test]
fn test_decode_decimal64_plain() {
    // Decimal64 is a raw 8-byte LE Int64 per row, including a negative.
    let rows: Vec<[u8; 8]> = vec![
        79i64.to_le_bytes(),
        (-1i64).to_le_bytes(),
        i64::MAX.to_le_bytes(),
    ];
    let row_refs: Vec<&[u8]> = rows.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("d", "Decimal(18, 9)")
        .decimal_data(&row_refs, 8)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.width, 8);
            assert_eq!(c.precision, 18);
            assert_eq!(c.scale, 9);
            assert_eq!(i64::from_le_bytes(c.value(0).try_into().unwrap()), 79);
            assert_eq!(i64::from_le_bytes(c.value(1).try_into().unwrap()), -1);
            assert_eq!(c.value(1), &[0xFF; 8]);
            assert_eq!(i64::from_le_bytes(c.value(2).try_into().unwrap()), i64::MAX);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
}

#[test]
fn test_decode_decimal128_plain() {
    // Decimal128 is a raw 16-byte LE Int128 per row. The core has no native
    // i128, so assert the raw little-endian byte pattern directly. An
    // unscaled 1 is byte 0 = 0x01, the rest zero; an unscaled -1 is all
    // 0xFF (two's-complement of the full 16-byte width).
    let one: [u8; 16] = {
        let mut b = [0u8; 16];
        b[0] = 0x01;
        b
    };
    let neg_one: [u8; 16] = [0xFF; 16];
    let zero: [u8; 16] = [0u8; 16];
    let row_refs: Vec<&[u8]> = vec![one.as_slice(), neg_one.as_slice(), zero.as_slice()];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("d", "Decimal(20, 2)")
        .decimal_data(&row_refs, 16)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.precision, 20);
            assert_eq!(c.scale, 2);
            assert_eq!(c.value(0), one);
            assert_eq!(c.value(1), neg_one);
            assert_eq!(c.value(2), zero);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Decimal {
            precision: 20,
            scale: 2,
            bits: 128,
        }
    );
}

#[test]
fn test_decode_decimal256_plain() {
    // Decimal256 is a raw 32-byte LE Int256 per row. Assert the raw
    // little-endian byte pattern: an unscaled 1, an unscaled -1 (all 0xFF),
    // and a larger value 258 (0x0102 little-endian -> bytes 0x02, 0x01).
    let one: [u8; 32] = {
        let mut b = [0u8; 32];
        b[0] = 0x01;
        b
    };
    let neg_one: [u8; 32] = [0xFF; 32];
    let two_fifty_eight: [u8; 32] = {
        let mut b = [0u8; 32];
        b[0] = 0x02;
        b[1] = 0x01;
        b
    };
    let row_refs: Vec<&[u8]> = vec![
        one.as_slice(),
        neg_one.as_slice(),
        two_fifty_eight.as_slice(),
    ];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("d", "Decimal(50, 10)")
        .decimal_data(&row_refs, 32)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.width, 32);
            assert_eq!(c.precision, 50);
            assert_eq!(c.scale, 10);
            assert_eq!(c.value(0), one);
            assert_eq!(c.value(1), neg_one);
            assert_eq!(c.value(2), two_fifty_eight);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Decimal {
            precision: 50,
            scale: 10,
            bits: 256,
        }
    );
}

#[test]
fn test_decode_nullable_decimal64() {
    // Nullable(Decimal64): the null map first, then the Int64 buffer, exactly
    // like Nullable(Int64). Null rows still carry a placeholder on the wire.
    let rows: Vec<[u8; 8]> = vec![
        13i64.to_le_bytes(),
        0i64.to_le_bytes(),
        (-1i64).to_le_bytes(),
        0i64.to_le_bytes(),
    ];
    let row_refs: Vec<&[u8]> = rows.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("d", "Nullable(Decimal(18, 9))")
        .null_map(&[false, true, false, true])
        .decimal_data(&row_refs, 8)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.null_count(), 2);
            assert_eq!(i64::from_le_bytes(c.value(0).try_into().unwrap()), 13);
            assert_eq!(i64::from_le_bytes(c.value(2).try_into().unwrap()), -1);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
    assert!(batch.column(0).validity().unwrap().is_valid(0));
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Nullable(Box::new(ChType::Decimal {
            precision: 18,
            scale: 9,
            bits: 64,
        }))
    );
}

#[test]
fn test_decode_decimal_zero_rows() {
    // A zero-row block carrying decimals of every width contributes the
    // schema but no chunks; the empty columns have length 0 and keep their
    // width/precision/scale.
    let data = BlockBuilder::new()
        .header(4, 0)
        .column_header("d32", "Decimal(9, 2)")
        .column_header("d64", "Decimal(18, 4)")
        .column_header("d128", "Decimal(38, 10)")
        .column_header("d256", "Decimal(76, 20)")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 4);
    for (i, (p, s, bits)) in [(9u8, 2u8, 32u16), (18, 4, 64), (38, 10, 128), (76, 20, 256)]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            cb.schema.fields[i].ch_type,
            ChType::Decimal {
                precision: p,
                scale: s,
                bits,
            }
        );
    }
}
