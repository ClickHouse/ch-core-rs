use super::*;
use crate::native::stream_decoder::StreamDecoder;

fn transpose(words: &[u64], rows: usize, dimension: usize, bit_width: usize) -> Vec<u8> {
    assert_eq!(words.len(), rows * dimension);
    let bytes_per_plane_row = dimension.div_ceil(8);
    let plane_stride = rows * bytes_per_plane_row;
    let mut wire = vec![0u8; bit_width * plane_stride];
    for row in 0..rows {
        for element in 0..dimension {
            let word = words[row * dimension + element];
            let byte = bytes_per_plane_row - 1 - element / 8;
            let mask = 1u8 << (element % 8);
            for plane in 0..bit_width {
                if (word >> (bit_width - 1 - plane)) & 1 != 0 {
                    wire[plane * plane_stride + row * bytes_per_plane_row + byte] |= mask;
                }
            }
        }
    }
    wire
}

fn qbit_values(column: &Column) -> &Column {
    match column {
        Column::QBit(column) => column.values.as_ref(),
        other => panic!("expected QBit, got {other:?}"),
    }
}

#[test]
fn decode_qbit_float32_dimension_9_pins_plane_and_bit_order() {
    let mut words = vec![0u64; 18];
    words[0] = 0x8000_0000;
    words[7] = 0x8000_0000;
    words[17] = 0x8000_0000;
    let wire = transpose(&words, 2, 9, 32);
    assert_eq!(&wire[..4], &[0x00, 0x81, 0x01, 0x00]);
    assert!(wire[4..].iter().all(|&byte| byte == 0));

    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("q", "QBit(Float32, 9)")
        .raw_bytes(&wire)
        .build();
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        decoded.schema.fields[0].ch_type,
        ChType::QBit {
            element_type: QBitElementType::Float32,
            dimension: 9,
        }
    );
    let Column::QBit(qbit) = decoded.chunks[0].column(0) else {
        panic!("expected QBit")
    };
    assert_eq!(qbit.dimension, 9);
    assert!(qbit.validity.is_none());
    let Column::Float32(values) = qbit.values.as_ref() else {
        panic!("expected Float32 QBit child")
    };
    assert_eq!(
        values
            .values
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        words.iter().map(|&word| word as u32).collect::<Vec<_>>()
    );
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn decode_qbit_all_element_widths_preserves_float_bits() {
    let bf_words = [0x3fc0u64, 0xc020, 0x4150];
    let f32_words = [
        1.5f32.to_bits() as u64,
        (-2.5f32).to_bits() as u64,
        13f32.to_bits() as u64,
    ];
    let f64_words = [1.5f64.to_bits(), (-2.5f64).to_bits(), 13f64.to_bits()];
    let data = BlockBuilder::new()
        .header(3, 1)
        .column_header("qb", "QBit(BFloat16, 3)")
        .raw_bytes(&transpose(&bf_words, 1, 3, 16))
        .column_header("qf", "QBit(Float32, 3)")
        .raw_bytes(&transpose(&f32_words, 1, 3, 32))
        .column_header("qd", "QBit(Float64, 3)")
        .raw_bytes(&transpose(&f64_words, 1, 3, 64))
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match qbit_values(decoded.chunks[0].column(0)) {
        Column::BFloat16(values) => assert_eq!(
            values
                .values
                .iter()
                .copied()
                .map(u16::from_le_bytes)
                .collect::<Vec<_>>(),
            bf_words.iter().map(|&word| word as u16).collect::<Vec<_>>()
        ),
        other => panic!("expected BFloat16 child, got {other:?}"),
    }
    match qbit_values(decoded.chunks[0].column(1)) {
        Column::Float32(values) => assert_eq!(
            values
                .values
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            f32_words
                .iter()
                .map(|&word| word as u32)
                .collect::<Vec<_>>()
        ),
        other => panic!("expected Float32 child, got {other:?}"),
    }
    match qbit_values(decoded.chunks[0].column(2)) {
        Column::Float64(values) => assert_eq!(
            values
                .values
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            f64_words
        ),
        other => panic!("expected Float64 child, got {other:?}"),
    }
}

#[test]
fn decode_nullable_qbit_consumes_hidden_rows_and_ignores_padding() {
    let words = [
        1.25f32.to_bits() as u64,
        (-0.0f32).to_bits() as u64,
        79f32.to_bits() as u64,
        13f32.to_bits() as u64,
        (-2.5f32).to_bits() as u64,
        f32::NAN.to_bits() as u64,
    ];
    let mut wire = transpose(&words, 3, 2, 32);
    // Dimension 2 uses only bits 0 and 1 of its sole byte. The server ignores
    // the remaining padding bits, so a decoder must consume but not reject them.
    for plane in 0..32 {
        wire[plane * 3] |= 0xfc;
    }
    let data = BlockBuilder::new()
        .header(2, 3)
        .column_header("q", "Nullable(QBit(Float32, 2))")
        .null_map(&[true, false, true])
        .raw_bytes(&wire)
        .column_header("tail", "UInt8")
        .raw_bytes(&[13, 79, 1])
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let Column::QBit(qbit) = decoded.chunks[0].column(0) else {
        panic!("expected QBit")
    };
    assert_eq!(qbit.null_count(), 2);
    let Column::Float32(values) = qbit.values.as_ref() else {
        panic!("expected Float32 child")
    };
    assert_eq!(
        values
            .values
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        words.iter().map(|&word| word as u32).collect::<Vec<_>>()
    );
    match decoded.chunks[0].column(1) {
        Column::UInt8(tail) => assert_eq!(tail.values, vec![13, 79, 1]),
        other => panic!("expected tail UInt8, got {other:?}"),
    }
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn decode_qbit_zero_rows_and_multi_block_keep_chunk_boundaries() {
    let zero = BlockBuilder::new()
        .header(2, 0)
        .column_header("q", "QBit(Float64, 1)")
        .column_header("nq", "Nullable(QBit(BFloat16, 9))")
        .build();
    let decoded = decode_all_bytes(&zero, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.num_columns(), 2);

    let mut data = BlockBuilder::new()
        .header(1, 1)
        .column_header("q", "QBit(Float64, 1)")
        .raw_bytes(&transpose(&[13f64.to_bits()], 1, 1, 64))
        .build();
    data.extend_from_slice(
        &BlockBuilder::new()
            .header(1, 2)
            .column_header("q", "QBit(Float64, 1)")
            .raw_bytes(&transpose(&[79f64.to_bits(), (-1f64).to_bits()], 2, 1, 64))
            .build(),
    );
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    assert_eq!(decoded.chunks[0].num_rows, 1);
    assert_eq!(decoded.chunks[1].num_rows, 2);
}

#[test]
fn decode_array_qbit_streamed_byte_by_byte() {
    let words = [
        1.25f32.to_bits() as u64,
        (-0.0f32).to_bits() as u64,
        13f32.to_bits() as u64,
        79f32.to_bits() as u64,
        (-2.5f32).to_bits() as u64,
        f32::INFINITY.to_bits() as u64,
        0.0f32.to_bits() as u64,
        f32::NEG_INFINITY.to_bits() as u64,
        f32::NAN.to_bits() as u64,
        3.5f32.to_bits() as u64,
        (-1.25f32).to_bits() as u64,
        0.5f32.to_bits() as u64,
    ];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("q", "Array(QBit(Float32, 3))")
        .array_offsets(&[2, 2, 4])
        .raw_bytes(&transpose(&words, 4, 3, 32))
        .build();

    let mut decoder = StreamDecoder::new(DecodeOptions::default());
    let mut blocks = Vec::new();
    for byte in data {
        blocks.extend(decoder.feed(&[byte]).unwrap());
    }
    blocks.extend(decoder.finish().unwrap());
    assert_eq!(blocks.len(), 1);

    let Column::Array(array) = blocks[0].column(0) else {
        panic!("expected Array")
    };
    assert_eq!(array.offsets, vec![0, 2, 2, 4]);
    let Column::QBit(qbit) = array.values.as_ref() else {
        panic!("expected QBit item")
    };
    let Column::Float32(values) = qbit.values.as_ref() else {
        panic!("expected Float32 child")
    };
    assert_eq!(
        values
            .values
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        words.iter().map(|&word| word as u32).collect::<Vec<_>>()
    );
}

#[test]
fn qbit_rejects_bad_headers_and_truncated_body_without_panicking() {
    for type_name in [
        "QBit(Int32, 9)",
        "QBit(Float32, 0)",
        "QBit(Float32, -1)",
        "QBit(Float32)",
        "qbit(Float32, 9)",
    ] {
        let data = BlockBuilder::new()
            .header(1, 0)
            .column_header("q", type_name)
            .build();
        assert!(matches!(
            decode_all_bytes(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
    }
    assert!(parse_ch_type(&format!("QBit(Float32, {})", QBIT_MAX_DIMENSION)).is_some());
    assert!(parse_ch_type(&format!("QBit(Float32, {})", QBIT_MAX_DIMENSION + 1)).is_none());
    assert_eq!(
        parse_ch_type("QBit(real, 9)"),
        Some(ChType::QBit {
            element_type: QBitElementType::Float32,
            dimension: 9,
        })
    );

    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("q", "QBit(Float32, 9)")
        .raw_bytes(&[0; 63])
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn low_cardinality_qbit_is_rejected_even_for_zero_rows() {
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("q", "LowCardinality(QBit(Float32, 9))")
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}
