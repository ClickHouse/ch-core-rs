use super::*;
use crate::native::decode::decode_all_bytes_binary_types;

fn qbit_type(element_type: QBitElementType, dimension: usize) -> ChType {
    ChType::QBit {
        element_type,
        dimension,
    }
}

fn qbit_batch() -> ColBatch {
    let mut nullable = QBitColumn::new(
        Column::Float32(PrimitiveColumn::new(vec![
            1.25,
            -0.0,
            13.0,
            79.0,
            -2.5,
            f32::INFINITY,
        ])),
        2,
    );
    nullable.validity = Some(Bitmap::from_ch_null_map(&[1, 0, 1]));
    ColBatch::new(
        Schema::new(vec![
            Field {
                name: "qb".into(),
                ch_type: qbit_type(QBitElementType::BFloat16, 3),
            },
            Field {
                name: "qf".into(),
                ch_type: ChType::Nullable(Box::new(qbit_type(QBitElementType::Float32, 2))),
            },
            Field {
                name: "qd".into(),
                ch_type: qbit_type(QBitElementType::Float64, 1),
            },
        ]),
        vec![
            Column::QBit(QBitColumn::new(
                Column::BFloat16(PrimitiveColumn::new(vec![
                    0x3fc0u16.to_le_bytes(),
                    0xc020u16.to_le_bytes(),
                    0x4150u16.to_le_bytes(),
                    0x8000u16.to_le_bytes(),
                    0x0001u16.to_le_bytes(),
                    0x7fc1u16.to_le_bytes(),
                    0x3f80u16.to_le_bytes(),
                    0xbf80u16.to_le_bytes(),
                    0x0000u16.to_le_bytes(),
                ])),
                3,
            )),
            Column::QBit(nullable),
            Column::QBit(QBitColumn::new(
                Column::Float64(PrimitiveColumn::new(vec![13.0, -0.0, 79.125])),
                1,
            )),
        ],
        3,
    )
}

#[test]
fn roundtrip_qbit_all_widths_rev0_and_tcp_revision() {
    let batch = qbit_batch();
    roundtrip(&batch, 0);
    roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn qbit_binary_type_header_block_roundtrip() {
    let batch = qbit_batch();
    let bytes = encode_block_binary_types(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes_binary_types(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 1);
    assert_batches_eq(&batch, decoded.chunks[0].as_ref());
}

#[test]
fn rev0_frames_qbit_dimension_9_with_exact_plane_order() {
    let mut values = vec![0.0f32; 18];
    values[0] = -0.0;
    values[7] = -0.0;
    values[17] = -0.0;
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "q".into(),
            ch_type: qbit_type(QBitElementType::Float32, 9),
        }]),
        vec![Column::QBit(QBitColumn::new(
            Column::Float32(PrimitiveColumn::new(values)),
            9,
        ))],
        2,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();

    let header_len = 2 + 2 + 1 + "QBit(Float32, 9)".len();
    let body = &bytes[header_len..];
    assert_eq!(body.len(), 32 * 2 * 2);
    assert_eq!(&body[..4], &[0x00, 0x81, 0x01, 0x00]);
    assert!(body[4..].iter().all(|&byte| byte == 0));
}

#[test]
fn qbit_chunked_and_zero_row_roundtrip() {
    let schema = Schema::new(vec![Field {
        name: "q".into(),
        ch_type: qbit_type(QBitElementType::Float64, 2),
    }]);
    let make = |values: Vec<f64>| {
        std::sync::Arc::new(ColBatch::new(
            schema.clone(),
            vec![Column::QBit(QBitColumn::new(
                Column::Float64(PrimitiveColumn::new(values)),
                2,
            ))],
            1,
        ))
    };
    let chunked = ChunkedBatch {
        schema: schema.clone(),
        chunks: vec![make(vec![13.0, -1.0]), make(vec![79.0, 1.25])],
    };
    for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
        let bytes = encode_chunked(
            &chunked,
            &EncodeOptions {
                protocol_revision: revision,
                ..EncodeOptions::default()
            },
        )
        .unwrap();
        let decoded = decode_all_bytes(
            &bytes,
            &DecodeOptions {
                protocol_revision: revision,
                ..DecodeOptions::default()
            },
        )
        .unwrap();
        assert_eq!(decoded.num_chunks(), 2);
    }

    let empty = ColBatch::new(
        Schema::new(vec![Field {
            name: "q".into(),
            ch_type: ChType::Nullable(Box::new(qbit_type(QBitElementType::BFloat16, 9))),
        }]),
        vec![Column::QBit(QBitColumn::new_nullable(
            Column::BFloat16(PrimitiveColumn::new(vec![])),
            9,
            Bitmap::from_ch_null_map(&[]),
        ))],
        0,
    );
    let bytes = encode_block(&empty, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema, empty.schema);
}

#[test]
fn qbit_validation_rejects_malformed_public_buffers() {
    let make_batch = |column: Column| ColBatch {
        schema: Schema::new(vec![Field {
            name: "q".into(),
            ch_type: qbit_type(QBitElementType::Float32, 2),
        }]),
        columns: vec![column],
        num_rows: 1,
    };

    let bad_dimension = make_batch(Column::QBit(QBitColumn::new(
        Column::Float32(PrimitiveColumn::new(vec![1.0, 2.0, 3.0])),
        3,
    )));
    assert!(matches!(
        encode_block(&bad_dimension, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));

    let ragged = make_batch(Column::QBit(QBitColumn::new(
        Column::Float32(PrimitiveColumn::new(vec![1.0, 2.0, 3.0])),
        2,
    )));
    assert!(matches!(
        encode_block(&ragged, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));

    let wrong_child = make_batch(Column::QBit(QBitColumn::new(
        Column::Float64(PrimitiveColumn::new(vec![1.0, 2.0])),
        2,
    )));
    assert!(matches!(
        encode_block(&wrong_child, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));

    let nullable_child = make_batch(Column::QBit(QBitColumn::new(
        Column::Float32(PrimitiveColumn::new_nullable(
            vec![1.0, 2.0],
            Bitmap::from_ch_null_map(&[0, 0]),
        )),
        2,
    )));
    assert!(matches!(
        encode_block(&nullable_child, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}
