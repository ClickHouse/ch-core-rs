use super::*;

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
