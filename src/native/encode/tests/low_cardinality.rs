use super::*;

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
