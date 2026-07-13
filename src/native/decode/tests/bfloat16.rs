use super::*;

const NEG_ONE: u16 = 0xbf80;
const ZERO: u16 = 0x0000;
const NEG_ZERO: u16 = 0x8000;
const ONE: u16 = 0x3f80;
const POS_INFINITY: u16 = 0x7f80;
const NEG_INFINITY: u16 = 0xff80;
const SUBNORMAL: u16 = 0x0001;
const NAN_PAYLOAD: u16 = 0x7fc1;

fn expected_words(bits: &[u16]) -> Vec<[u8; 2]> {
    bits.iter().map(|word| word.to_le_bytes()).collect()
}

#[test]
fn test_decode_bfloat16_plain_preserves_bits() {
    let bits = [
        NEG_ONE,
        ZERO,
        NEG_ZERO,
        ONE,
        POS_INFINITY,
        NEG_INFINITY,
        SUBNORMAL,
        NAN_PAYLOAD,
    ];
    let data = BlockBuilder::new()
        .header(1, bits.len())
        .column_header("bf", "BFloat16")
        .bfloat16_data(&bits)
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.schema.fields[0].ch_type, ChType::BFloat16);
    match decoded.chunks[0].column(0) {
        Column::BFloat16(c) => {
            assert_eq!(c.values, expected_words(&bits));
            assert!(c.validity.is_none());
        }
        other => panic!("expected BFloat16, got {other:?}"),
    }
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_decode_bfloat16_truncated_body_is_unexpected_eof() {
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("bf", "BFloat16")
        .bfloat16_data(&[ONE])
        .build();

    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_decode_nullable_bfloat16() {
    let bits = [NEG_ONE, ZERO, ONE, ZERO];
    let data = BlockBuilder::new()
        .header(1, bits.len())
        .column_header("nbf", "Nullable(BFloat16)")
        .null_map(&[false, true, false, true])
        .bfloat16_data(&bits)
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        decoded.schema.fields[0].ch_type,
        ChType::Nullable(Box::new(ChType::BFloat16))
    );
    match decoded.chunks[0].column(0) {
        Column::BFloat16(c) => {
            assert_eq!(c.values, expected_words(&bits));
            assert_eq!(c.null_count(), 2);
        }
        other => panic!("expected nullable BFloat16, got {other:?}"),
    }
    let validity = decoded.chunks[0].column(0).validity().unwrap();
    assert_eq!(
        (0..bits.len())
            .map(|row| validity.is_valid(row))
            .collect::<Vec<_>>(),
        vec![true, false, true, false]
    );
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_decode_low_cardinality_bfloat16() {
    let dictionary_bits = [ZERO, ONE, 0xbfa0, 0x429e];
    let dictionary: Vec<u8> = dictionary_bits
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .collect();
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("lc", "LowCardinality(BFloat16)")
        .low_cardinality_block(4, &dictionary, &[1, 2, 1, 3], 1)
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        decoded.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::BFloat16))
    );
    match decoded.chunks[0].column(0) {
        Column::Dictionary(c) => {
            assert_eq!(c.indices, vec![1, 2, 1, 3]);
            match c.values.as_ref() {
                Column::BFloat16(values) => {
                    assert_eq!(values.values, expected_words(&dictionary_bits));
                }
                other => panic!("expected BFloat16 dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected BFloat16 dictionary, got {other:?}"),
    }
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_decode_bfloat16_zero_rows() {
    let data = BlockBuilder::new()
        .header(2, 0)
        .column_header("bf", "BFloat16")
        .column_header("nbf", "Nullable(BFloat16)")
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.num_columns(), 2);
    assert_eq!(decoded.schema.fields[0].ch_type, ChType::BFloat16);
    assert_eq!(
        decoded.schema.fields[1].ch_type,
        ChType::Nullable(Box::new(ChType::BFloat16))
    );
}

#[test]
fn test_decode_bfloat16_multi_block_keeps_chunks_separate() {
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("bf", "BFloat16")
        .bfloat16_data(&[NEG_ONE, ZERO])
        .build();
    data.extend_from_slice(
        &BlockBuilder::new()
            .header(1, 2)
            .column_header("bf", "BFloat16")
            .bfloat16_data(&[ONE, NAN_PAYLOAD])
            .build(),
    );

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    for (chunk, expected) in decoded
        .chunks
        .iter()
        .zip([[NEG_ONE, ZERO], [ONE, NAN_PAYLOAD]])
    {
        match chunk.column(0) {
            Column::BFloat16(c) => assert_eq!(c.values, expected_words(&expected)),
            other => panic!("expected BFloat16, got {other:?}"),
        }
    }
}
