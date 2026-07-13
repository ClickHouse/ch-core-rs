use super::*;

#[test]
fn zero_row_block_roundtrips_schema() {
    // A zero-row block still carries full column headers. The decoder keeps
    // the schema but drops the empty block from `chunks`.
    let fields = vec![
        Field {
            name: "n".into(),
            ch_type: ChType::Int32,
        },
        Field {
            name: "x".into(),
            ch_type: ChType::Float64,
        },
        Field {
            name: "u".into(),
            ch_type: ChType::Uuid,
        },
        Field {
            name: "ip4".into(),
            ch_type: ChType::Ipv4,
        },
        Field {
            name: "ip6".into(),
            ch_type: ChType::Ipv6,
        },
        Field {
            name: "i128".into(),
            ch_type: ChType::Int128,
        },
        Field {
            name: "u256".into(),
            ch_type: ChType::UInt256,
        },
        Field {
            name: "dec".into(),
            ch_type: ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            },
        },
        Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::String)),
        },
        Field {
            name: "lcn".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String)))),
        },
        Field {
            name: "time".into(),
            ch_type: ChType::Time,
        },
        Field {
            name: "time64".into(),
            ch_type: ChType::Time64 { precision: 3 },
        },
        Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::Int32)),
        },
        Field {
            name: "alc".into(),
            ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
        },
        Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![
                (Some("a".to_string()), ChType::Int32),
                (Some("b".to_string()), ChType::String),
            ]),
        },
        Field {
            name: "t0".into(),
            ch_type: ChType::Tuple(vec![]),
        },
        Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        },
    ];
    let columns = vec![
        Column::Int32(PrimitiveColumn::new(vec![])),
        Column::Float64(PrimitiveColumn::new(vec![])),
        Column::Uuid(FixedBinaryColumn::new(Vec::new(), 16)),
        Column::Ipv4(PrimitiveColumn::new(Vec::new())),
        Column::Ipv6(FixedBinaryColumn::new(Vec::new(), 16)),
        Column::Int128(FixedBinaryColumn::new(Vec::new(), 16)),
        Column::UInt256(FixedBinaryColumn::new(Vec::new(), 32)),
        Column::Decimal(DecimalColumn::new(Vec::new(), 4, 9, 4)),
        Column::Dictionary(DictionaryColumn::new(
            vec![],
            Column::Utf8(utf8_column(&[])),
        )),
        Column::Dictionary(DictionaryColumn::new_nullable(
            vec![],
            Column::Utf8(utf8_column(&[])),
            Bitmap::from_ch_null_map(&[]),
        )),
        Column::Time(PrimitiveColumn::new(vec![])),
        Column::Time64(PrimitiveColumn::new(vec![])),
        // A zero-row Array carries only the leading-0 offset and writes no
        // data at all, not even the hoisted LC key version of an LC element.
        Column::Array(ArrayColumn::new(
            vec![0],
            Column::Int32(PrimitiveColumn::new(vec![])),
        )),
        Column::Array(ArrayColumn::new(
            vec![0],
            Column::Dictionary(DictionaryColumn::new(
                vec![],
                Column::Utf8(utf8_column(&[])),
            )),
        )),
        // A zero-row Tuple carries only the header: no element bodies, and
        // for Tuple() no placeholder bytes either.
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![])),
                Column::Utf8(utf8_column(&[])),
            ],
            0,
        )),
        Column::Tuple(TupleColumn::new(vec![], 0)),
        // A zero-row Map carries only the leading-0 offset and writes no
        // data at all.
        Column::Map(map_column(
            vec![0],
            Column::Utf8(utf8_column(&[])),
            Column::Int32(PrimitiveColumn::new(vec![])),
        )),
    ];
    let batch = ColBatch::new(Schema::new(fields), columns, 0);
    for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
        let bytes = encode_block(
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
        assert_eq!(decoded.num_rows(), 0);
        assert_eq!(decoded.num_chunks(), 0);
        assert_eq!(decoded.schema, batch.schema);
    }
}

#[test]
fn encode_chunked_roundtrips_multiple_blocks() {
    let field = Field {
        name: "n".into(),
        ch_type: ChType::Int32,
    };
    let chunk = |vals: Vec<i32>| {
        let n = vals.len();
        std::sync::Arc::new(ColBatch::new(
            Schema::new(vec![field.clone()]),
            vec![Column::Int32(PrimitiveColumn::new(vals))],
            n,
        ))
    };
    let batch = ChunkedBatch {
        schema: Schema::new(vec![field.clone()]),
        chunks: vec![chunk(vec![13, 14]), chunk(vec![15, 16]), chunk(vec![17])],
    };
    let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 3);
    assert_eq!(decoded.num_rows(), 5);
    let got: Vec<Vec<i32>> = decoded
        .chunks
        .iter()
        .map(|c| match c.column(0) {
            Column::Int32(p) => p.values.clone(),
            other => panic!("expected Int32, got {other:?}"),
        })
        .collect();
    assert_eq!(got, vec![vec![13, 14], vec![15, 16], vec![17]]);
}

#[test]
fn rev_tcp_frames_block_info_and_marker() {
    // At the TCP revision the block leads with the BlockInfo preamble and each
    // column header carries the default (0) custom-serialization marker.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::UInt8,
        }]),
        vec![Column::UInt8(PrimitiveColumn::new(vec![79]))],
        1,
    );
    let bytes = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
        },
    )
    .unwrap();
    let expected = [
        0x01, 0x00, // field 1 is_overflows = false
        0x02, 0xFF, 0xFF, 0xFF, 0xFF, // field 2 bucket_num = -1
        0x03, 0x00, // field 3 out_of_order_buckets = empty (rev >= 54480)
        0x00, // BlockInfo terminator
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'n', // name "n"
        0x05, b'U', b'I', b'n', b't', b'8', // type "UInt8"
        0x00, // custom-serialization marker = default
        0x4F, // UInt8 value 79
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn over_deep_type_nesting_is_rejected() {
    // Encode input never passes through `parse_ch_type`, so its
    // MAX_TYPE_DEPTH cap does not protect the encoder: a caller-constructed
    // pathologically deep type would recurse one stack frame per wrapper
    // level in `column_variant_matches` / `is_encodable` / `Display` /
    // `write_state_prefix` and overflow the stack. The iterative depth walk
    // in `validate_column` rejects it first, and does so without cloning or
    // rendering the deep type (both recurse to full depth), which is why the
    // rejection is InconsistentBatch rather than UnsupportedType.
    let mut ch_type = ChType::Int32;
    for _ in 0..(MAX_TYPE_DEPTH + 100) {
        ch_type = ChType::Array(Box::new(ch_type));
    }
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "deep".into(),
            ch_type,
        }]),
        columns: vec![Column::Int32(PrimitiveColumn::new(vec![]))],
        num_rows: 0,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { detail } => {
            assert!(detail.contains("nesting"), "unexpected detail: {detail}");
        }
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn unrepresentable_type_string_is_rejected() {
    // A DateTime64/Time64 precision above 9, invalid Decimal metadata, and
    // FixedString(0) are constructible ChTypes whose rendered type string
    // this crate's parser and the server reject or normalize differently.
    // Encoding must fail at the source (InconsistentBatch) rather than emit a
    // header that fails to decode downstream, or worse, a Decimal header whose
    // server-derived width disagrees with the body width.
    let dt64 = ColBatch {
        schema: Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::DateTime64 {
                precision: 200,
                timezone: None,
            },
        }]),
        columns: vec![Column::DateTime64(PrimitiveColumn::new(vec![0]))],
        num_rows: 1,
    };
    match encode_block(&dt64, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch for DateTime64(200), got {other:?}"),
    }

    let time64 = ColBatch {
        schema: Schema::new(vec![Field {
            name: "t64".into(),
            ch_type: ChType::Time64 { precision: 200 },
        }]),
        columns: vec![Column::Time64(PrimitiveColumn::new(vec![0]))],
        num_rows: 1,
    };
    match encode_block(&time64, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch for Time64(200), got {other:?}"),
    }

    let decimal_cases = [
        (
            "Decimal(9, 4) with 64 bits",
            ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 64,
            },
            DecimalColumn::new(vec![0u8; 8], 8, 9, 4),
        ),
        (
            "Decimal(100, 4)",
            ChType::Decimal {
                precision: 100,
                scale: 4,
                bits: 128,
            },
            DecimalColumn::new(vec![0u8; 16], 16, 100, 4),
        ),
        (
            "Decimal(9, 20)",
            ChType::Decimal {
                precision: 9,
                scale: 20,
                bits: 32,
            },
            DecimalColumn::new(vec![0u8; 4], 4, 9, 20),
        ),
    ];
    for (label, ch_type, column) in decimal_cases {
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "dec".into(),
                ch_type,
            }]),
            columns: vec![Column::Decimal(column)],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch for {label}, got {other:?}"),
        }
    }

    let fs0 = ColBatch {
        schema: Schema::new(vec![Field {
            name: "fs".into(),
            ch_type: ChType::FixedString(0),
        }]),
        columns: vec![Column::FixedBinary(FixedBinaryColumn::new(vec![], 0))],
        num_rows: 0,
    };
    match encode_block(&fs0, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch for FixedString(0), got {other:?}"),
    }
}

#[test]
fn encode_chunked_rejects_mismatched_chunk_schema() {
    // A chunk whose schema differs from the batch schema would encode a block
    // the server rejects mid-insert. Reject before writing anything.
    let field = |name: &str| Field {
        name: name.into(),
        ch_type: ChType::Int32,
    };
    let chunk = |name: &str, vals: Vec<i32>| {
        let n = vals.len();
        std::sync::Arc::new(ColBatch::new(
            Schema::new(vec![field(name)]),
            vec![Column::Int32(PrimitiveColumn::new(vals))],
            n,
        ))
    };
    let batch = ChunkedBatch {
        schema: Schema::new(vec![field("n")]),
        chunks: vec![chunk("n", vec![13, 14]), chunk("m", vec![15])],
    };
    match encode_chunked(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn inconsistent_batch_is_rejected() {
    // Build a batch whose column length disagrees with num_rows. Bypass
    // `ColBatch::new` (its debug_assert would fire) by constructing directly.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Int32,
        }]),
        columns: vec![Column::Int32(PrimitiveColumn::new(vec![1, 2]))],
        num_rows: 3,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}
