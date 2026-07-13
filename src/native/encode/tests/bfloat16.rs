use super::*;

fn bfloat16_column(bits: &[u16]) -> PrimitiveColumn<[u8; 2]> {
    PrimitiveColumn::new(bits.iter().map(|word| word.to_le_bytes()).collect())
}

fn bfloat16_batch() -> ColBatch {
    let mut nullable = bfloat16_column(&[0x4150, 0x0000, 0x429e, 0x0000]);
    nullable.validity = Some(Bitmap::from_ch_null_map(&[0, 1, 0, 1]));
    ColBatch::new(
        Schema::new(vec![
            Field {
                name: "bf".into(),
                ch_type: ChType::BFloat16,
            },
            Field {
                name: "nbf".into(),
                ch_type: ChType::Nullable(Box::new(ChType::BFloat16)),
            },
            Field {
                name: "lc_bf".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::BFloat16)),
            },
        ]),
        vec![
            Column::BFloat16(bfloat16_column(&[0xbfa0, 0x0000, 0x4060, 0x429e])),
            Column::BFloat16(nullable),
            Column::Dictionary(DictionaryColumn::new(
                vec![1, 2, 1, 3],
                Column::BFloat16(bfloat16_column(&[0x0000, 0x4150, 0x429e, 0x4381])),
            )),
        ],
        4,
    )
}

#[test]
fn roundtrip_bfloat16_rev0() {
    roundtrip(&bfloat16_batch(), 0);
}

#[test]
fn roundtrip_bfloat16_tcp_revision() {
    roundtrip(&bfloat16_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn rev0_frames_bfloat16_raw_little_endian_bytes() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "bf".into(),
            ch_type: ChType::BFloat16,
        }]),
        vec![Column::BFloat16(bfloat16_column(&[0xbf80, 0x3f80]))],
        2,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x02, b'b', b'f', // name "bf"
        0x08, b'B', b'F', b'l', b'o', b'a', b't', b'1', b'6', // type "BFloat16"
        0x80, 0xbf, // -1.0 BFloat16 raw bits, little-endian
        0x80, 0x3f, // 1.0 BFloat16 raw bits, little-endian
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn encode_chunked_roundtrips_bfloat16_blocks() {
    let schema = Schema::new(vec![Field {
        name: "bf".into(),
        ch_type: ChType::BFloat16,
    }]);
    let make_chunk = |bits: &[u16]| {
        std::sync::Arc::new(ColBatch::new(
            schema.clone(),
            vec![Column::BFloat16(bfloat16_column(bits))],
            bits.len(),
        ))
    };
    let batch = ChunkedBatch {
        schema: schema.clone(),
        chunks: vec![make_chunk(&[0xbf80, 0x0000]), make_chunk(&[0x3f80, 0x7fc1])],
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
fn zero_row_bfloat16_encodes_schema_without_bodies() {
    let nullable = PrimitiveColumn::new_nullable(vec![], Bitmap::from_ch_null_map(&[]));
    let batch = ColBatch::new(
        Schema::new(vec![
            Field {
                name: "bf".into(),
                ch_type: ChType::BFloat16,
            },
            Field {
                name: "nbf".into(),
                ch_type: ChType::Nullable(Box::new(ChType::BFloat16)),
            },
            Field {
                name: "lc_bf".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::BFloat16)),
            },
        ]),
        vec![
            Column::BFloat16(bfloat16_column(&[])),
            Column::BFloat16(nullable),
            Column::Dictionary(DictionaryColumn::new(
                vec![],
                Column::BFloat16(bfloat16_column(&[])),
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
fn bfloat16_uint16_variant_mismatch_is_rejected() {
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "bf".into(),
            ch_type: ChType::BFloat16,
        }]),
        columns: vec![Column::UInt16(PrimitiveColumn::new(vec![0x3f80]))],
        num_rows: 1,
    };
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}
