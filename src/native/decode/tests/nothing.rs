use super::*;

#[test]
fn decode_nothing_accepts_arbitrary_placeholders() {
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("n", "Nothing")
        .raw_bytes(&[0x00, 0x30, 0x7f, 0xff])
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.schema.fields[0].ch_type, ChType::Nothing);
    match decoded.chunks[0].column(0) {
        Column::Nothing(c) => {
            assert_eq!(c.len, 4);
            assert_eq!(c.null_count(), 4);
            assert!(c.validity.is_none());
        }
        other => panic!("expected Nothing, got {other:?}"),
    }
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn decode_nothing_consumes_exactly_one_byte_before_next_column() {
    let data = BlockBuilder::new()
        .header(2, 3)
        .column_header("n", "Nothing")
        .raw_bytes(&[0xaa, 0xbb, 0xcc])
        .column_header("u", "UInt8")
        .raw_bytes(&[13, 79, 5])
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.chunks[0].column(0).len(), 3);
    match decoded.chunks[0].column(1) {
        Column::UInt8(c) => assert_eq!(c.values, vec![13, 79, 5]),
        other => panic!("expected trailing UInt8, got {other:?}"),
    }
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn decode_nullable_nothing_preserves_structural_mask() {
    // The server treats every nonzero null-map byte as null and ignores every
    // Nothing placeholder byte. Use noncanonical values for both runs to prove
    // the decoder consumes them structurally without validation.
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("n", "Nullable(Nothing)")
        .raw_bytes(&[0x00, 0x02, 0xff, 0x00])
        .raw_bytes(&[0xaa, 0x00, 0x31, 0x7f])
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        decoded.schema.fields[0].ch_type,
        ChType::Nullable(Box::new(ChType::Nothing))
    );
    match decoded.chunks[0].column(0) {
        Column::Nothing(c) => {
            assert_eq!(c.len, 4);
            // Arrow Null semantics are intrinsic, independent of this mask.
            assert_eq!(c.null_count(), 4);
            let validity = c.validity.as_ref().unwrap();
            assert_eq!(
                (0..4).map(|row| validity.is_valid(row)).collect::<Vec<_>>(),
                vec![true, false, false, true]
            );
        }
        other => panic!("expected nullable Nothing, got {other:?}"),
    }
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn decode_nothing_truncation_is_unexpected_eof() {
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("n", "Nothing")
        .raw_bytes(&[0x30, 0x30])
        .build();

    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn decode_nothing_zero_rows_has_schema_and_no_chunk() {
    let data = BlockBuilder::new()
        .header(2, 0)
        .column_header("n", "Nothing")
        .column_header("nn", "Nullable(Nothing)")
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.num_columns(), 2);
    assert_eq!(decoded.schema.fields[0].ch_type, ChType::Nothing);
    assert_eq!(
        decoded.schema.fields[1].ch_type,
        ChType::Nullable(Box::new(ChType::Nothing))
    );
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn decode_nothing_multi_block_keeps_chunks_separate() {
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("n", "Nothing")
        .raw_bytes(&[0x30, 0x30])
        .build();
    data.extend_from_slice(
        &BlockBuilder::new()
            .header(1, 3)
            .column_header("n", "Nothing")
            .raw_bytes(&[0x00, 0x01, 0xff])
            .build(),
    );

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    assert_eq!(decoded.chunks[0].column(0).len(), 2);
    assert_eq!(decoded.chunks[1].column(0).len(), 3);
}

#[test]
fn decode_array_nothing_with_only_empty_runs() {
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("a", "Array(Nothing)")
        .array_offsets(&[0, 0, 0])
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        decoded.schema.fields[0].ch_type,
        ChType::Array(Box::new(ChType::Nothing))
    );
    match decoded.chunks[0].column(0) {
        Column::Array(c) => {
            assert_eq!(c.offsets, vec![0, 0, 0, 0]);
            match c.values.as_ref() {
                Column::Nothing(values) => assert_eq!(values.len, 0),
                other => panic!("expected Nothing array values, got {other:?}"),
            }
        }
        other => panic!("expected Array(Nothing), got {other:?}"),
    }
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn low_cardinality_nothing_is_rejected_for_all_row_counts() {
    for type_name in [
        "LowCardinality(Nothing)",
        "LowCardinality(Nullable(Nothing))",
    ] {
        for rows in [0, 1] {
            let data = BlockBuilder::new()
                .header(1, rows)
                .column_header("n", type_name)
                .build();
            assert!(matches!(
                decode_all_bytes(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ));
        }
    }
}
