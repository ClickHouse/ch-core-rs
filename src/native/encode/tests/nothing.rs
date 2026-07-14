use super::*;

fn nothing_batch() -> ColBatch {
    ColBatch::new(
        Schema::new(vec![
            Field {
                name: "n".into(),
                ch_type: ChType::Nothing,
            },
            Field {
                name: "nn".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Nothing)),
            },
            Field {
                name: "an".into(),
                ch_type: ChType::Array(Box::new(ChType::Nothing)),
            },
            Field {
                name: "mn".into(),
                ch_type: ChType::Map(Box::new(ChType::Nothing), Box::new(ChType::UInt8)),
            },
        ]),
        vec![
            Column::Nothing(NothingColumn::new(4)),
            Column::Nothing(NothingColumn::new_nullable(
                4,
                Bitmap::from_ch_null_map(&[0, 1, 0, 1]),
            )),
            Column::Array(ArrayColumn::new(
                vec![0, 0, 2, 2, 3],
                Column::Nothing(NothingColumn::new(3)),
            )),
            Column::Map(map_column(
                vec![0, 1, 1, 2, 2],
                Column::Nothing(NothingColumn::new(2)),
                Column::UInt8(PrimitiveColumn::new(vec![13, 79])),
            )),
        ],
        4,
    )
}

#[test]
fn roundtrip_nothing_rev0() {
    roundtrip(&nothing_batch(), 0);
}

#[test]
fn roundtrip_nothing_tcp_revision() {
    roundtrip(&nothing_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn rev0_frames_nothing_with_canonical_ascii_zero_bytes() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nothing,
        }]),
        vec![Column::Nothing(NothingColumn::new(3))],
        3,
    );

    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x03, // num_rows = 3
        0x01, b'n', // name "n"
        0x07, b'N', b'o', b't', b'h', b'i', b'n', b'g', // type "Nothing"
        b'0', b'0', b'0', // canonical placeholders
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_nullable_nothing_mask_before_canonical_body() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Nothing)),
        }]),
        vec![Column::Nothing(NothingColumn::new_nullable(
            3,
            Bitmap::from_ch_null_map(&[0, 1, 0]),
        ))],
        3,
    );

    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let body = &bytes[bytes.len() - 6..];
    assert_eq!(body, &[0x00, 0x01, 0x00, b'0', b'0', b'0']);
}

#[test]
fn zero_row_nothing_encodes_schema_without_body() {
    let batch = ColBatch::new(
        Schema::new(vec![
            Field {
                name: "n".into(),
                ch_type: ChType::Nothing,
            },
            Field {
                name: "nn".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Nothing)),
            },
        ]),
        vec![
            Column::Nothing(NothingColumn::new(0)),
            Column::Nothing(NothingColumn::new_nullable(
                0,
                Bitmap::from_ch_null_map(&[]),
            )),
        ],
        0,
    );

    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema, batch.schema);
}

#[test]
fn nothing_variant_mismatches_are_rejected() {
    let wrong_buffer = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nothing,
        }]),
        columns: vec![Column::UInt8(PrimitiveColumn::new(vec![13]))],
        num_rows: 1,
    };
    assert!(matches!(
        encode_block(&wrong_buffer, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));

    let wrong_type = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::UInt8,
        }]),
        columns: vec![Column::Nothing(NothingColumn::new(1))],
        num_rows: 1,
    };
    assert!(matches!(
        encode_block(&wrong_type, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}

#[test]
fn bare_nothing_rejects_structural_null_mask_but_not_intrinsic_null_count() {
    let accepted = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nothing,
        }]),
        vec![Column::Nothing(NothingColumn::new(2))],
        2,
    );
    assert!(encode_block(&accepted, &EncodeOptions::default()).is_ok());

    let rejected = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nothing,
        }]),
        vec![Column::Nothing(NothingColumn::new_nullable(
            2,
            Bitmap::from_ch_null_map(&[0, 1]),
        ))],
        2,
    );
    assert!(matches!(
        encode_block(&rejected, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}

#[test]
fn low_cardinality_nothing_is_not_encodable() {
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Nothing)),
        }]),
        columns: vec![Column::Dictionary(DictionaryColumn::new(
            vec![0],
            Column::Nothing(NothingColumn::new(1)),
        ))],
        num_rows: 1,
    };
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::UnsupportedType { .. })
    ));
}
