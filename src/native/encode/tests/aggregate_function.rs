use super::*;

fn count_batch() -> ColBatch {
    ColBatch::new(
        Schema::new(vec![
            Field {
                name: "c".into(),
                ch_type: parse_ch_type("AggregateFunction(count)").unwrap(),
            },
            Field {
                name: "cn".into(),
                ch_type: parse_ch_type("AggregateFunction(count, Nullable(String))").unwrap(),
            },
        ]),
        vec![
            Column::AggregateState(AggregateStateColumn::new(
                vec![0, 1, 2, 4],
                vec![0x00, 0x0d, 0x80, 0x01],
            )),
            Column::AggregateState(AggregateStateColumn::new(
                vec![0, 1, 2, 3],
                vec![0x4f, 0x05, 0x0b],
            )),
        ],
        3,
    )
}

#[test]
fn roundtrip_count_states_rev0() {
    roundtrip(&count_batch(), 0);
}

#[test]
fn roundtrip_count_states_tcp_revision() {
    roundtrip(&count_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn rev0_frames_count_state_bytes_verbatim() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "c".into(),
            ch_type: parse_ch_type("AggregateFunction(count)").unwrap(),
        }]),
        vec![Column::AggregateState(AggregateStateColumn::new(
            vec![0, 1, 3],
            vec![0x0d, 0x80, 0x01],
        ))],
        2,
    );

    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    assert!(bytes.ends_with(&[0x0d, 0x80, 0x01]));
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_batches_eq(&batch, &decoded.chunks[0]);
}

fn sum_batch() -> ColBatch {
    let mut uint_states = Vec::new();
    let mut decimal_states = Vec::new();
    let mut wide_states = Vec::new();
    let mut enum_states = Vec::new();
    for value in [0u64, 13, 79] {
        uint_states.extend_from_slice(&value.to_le_bytes());
        decimal_states.extend_from_slice(&((value as i128) * 100).to_le_bytes());
        wide_states.extend_from_slice(&value.to_le_bytes());
        wide_states.extend_from_slice(&[0u8; 24]);
        enum_states.extend_from_slice(&(value as i64).to_le_bytes());
    }

    ColBatch::new(
        Schema::new(vec![
            Field {
                name: "u".into(),
                ch_type: parse_ch_type("AggregateFunction(sum, UInt8)").unwrap(),
            },
            Field {
                name: "d".into(),
                ch_type: parse_ch_type("AggregateFunction(sum, Decimal(9, 2))").unwrap(),
            },
            Field {
                name: "w".into(),
                ch_type: parse_ch_type("AggregateFunction(sum, UInt256)").unwrap(),
            },
            Field {
                name: "e".into(),
                ch_type: parse_ch_type(
                    "AggregateFunction(sum, Enum8('zero' = 0, 'thirteen' = 13, 'seventy_nine' = 79))",
                )
                .unwrap(),
            },
        ]),
        vec![
            Column::AggregateState(AggregateStateColumn::new(
                vec![0, 8, 16, 24],
                uint_states,
            )),
            Column::AggregateState(AggregateStateColumn::new(
                vec![0, 16, 32, 48],
                decimal_states,
            )),
            Column::AggregateState(AggregateStateColumn::new(
                vec![0, 32, 64, 96],
                wide_states,
            )),
            Column::AggregateState(AggregateStateColumn::new(
                vec![0, 8, 16, 24],
                enum_states,
            )),
        ],
        3,
    )
}

#[test]
fn roundtrip_sum_states_rev0() {
    roundtrip(&sum_batch(), 0);
}

#[test]
fn roundtrip_sum_states_tcp_revision() {
    roundtrip(&sum_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn rev0_frames_sum_state_bytes_verbatim() {
    let states = [13u64.to_le_bytes(), 79u64.to_le_bytes()].concat();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "s".into(),
            ch_type: parse_ch_type("AggregateFunction(sum, UInt8)").unwrap(),
        }]),
        vec![Column::AggregateState(AggregateStateColumn::new(
            vec![0, 8, 16],
            states.clone(),
        ))],
        2,
    );

    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    assert!(bytes.ends_with(&states));
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_batches_eq(&batch, &decoded.chunks[0]);
}

#[test]
fn zero_row_sum_state_encodes_schema_without_body() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "s".into(),
            ch_type: parse_ch_type("AggregateFunction(sum, BFloat16)").unwrap(),
        }]),
        vec![Column::AggregateState(AggregateStateColumn::new(
            vec![0],
            vec![],
        ))],
        0,
    );

    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema, batch.schema);
}

#[test]
fn sum_state_validation_rejects_wrong_width_states() {
    for (type_name, wrong_width) in [
        ("AggregateFunction(sum, UInt64)", 7),
        ("AggregateFunction(sum, Decimal(9, 2))", 15),
        ("AggregateFunction(sum, UInt256)", 33),
    ] {
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "s".into(),
                ch_type: parse_ch_type(type_name).unwrap(),
            }]),
            vec![Column::AggregateState(AggregateStateColumn::new(
                vec![0, wrong_width as i64],
                vec![0x0d; wrong_width],
            ))],
            1,
        );
        assert!(matches!(
            encode_block(&batch, &EncodeOptions::default()),
            Err(EncodeError::InconsistentBatch { .. })
        ));
    }
}

#[test]
fn zero_row_count_state_encodes_schema_without_body() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "c".into(),
            ch_type: parse_ch_type("AggregateFunction(count, UInt64)").unwrap(),
        }]),
        vec![Column::AggregateState(AggregateStateColumn::new(
            vec![0],
            vec![],
        ))],
        0,
    );

    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema, batch.schema);
}

#[test]
fn count_state_validation_rejects_bad_offsets_and_row_payloads() {
    for column in [
        AggregateStateColumn::new(vec![0, 1], vec![0x0d, 0x4f]),
        AggregateStateColumn::new(vec![1, 2, 3], vec![0x0d, 0x4f, 0x05]),
        AggregateStateColumn::new(vec![0, 2, 3], vec![0x0d, 0x4f, 0x05]),
        AggregateStateColumn::new(vec![0, 1, 2], vec![0x80, 0x0d]),
    ] {
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "c".into(),
                ch_type: parse_ch_type("AggregateFunction(count)").unwrap(),
            }]),
            columns: vec![Column::AggregateState(column)],
            num_rows: 2,
        };
        assert!(matches!(
            encode_block(&batch, &EncodeOptions::default()),
            Err(EncodeError::InconsistentBatch { .. })
        ));
    }
}

fn nothing_uint64_batch() -> ColBatch {
    ColBatch::new(
        Schema::new(vec![Field {
            name: "c".into(),
            ch_type: parse_ch_type("AggregateFunction(nothingUInt64, Nullable(Nothing))").unwrap(),
        }]),
        vec![Column::AggregateState(AggregateStateColumn::new(
            vec![0, 1, 2, 3],
            vec![0x00, 0x00, 0x00],
        ))],
        3,
    )
}

#[test]
fn roundtrip_nothing_uint64_states_rev0() {
    roundtrip(&nothing_uint64_batch(), 0);
}

#[test]
fn roundtrip_nothing_uint64_states_tcp_revision() {
    roundtrip(&nothing_uint64_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn zero_row_nothing_uint64_state_encodes_schema_without_body() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "c".into(),
            ch_type: parse_ch_type("AggregateFunction(nothingUInt64, Nullable(Nothing))").unwrap(),
        }]),
        vec![Column::AggregateState(AggregateStateColumn::new(
            vec![0],
            vec![],
        ))],
        0,
    );

    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema, batch.schema);
}

#[test]
fn nothing_uint64_validation_rejects_nonzero_or_wrong_width_states() {
    for column in [
        // A nonzero state byte: the server writes only 0x00.
        AggregateStateColumn::new(vec![0, 1, 2], vec![0x00, 0x01]),
        // A two-byte state: each row must be exactly one 0x00.
        AggregateStateColumn::new(vec![0, 2], vec![0x00, 0x00]),
        // An empty state: each row must be exactly one 0x00.
        AggregateStateColumn::new(vec![0, 0], vec![]),
    ] {
        let num_rows = column.offsets.len() - 1;
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "c".into(),
                ch_type: parse_ch_type("AggregateFunction(nothingUInt64, Nullable(Nothing))")
                    .unwrap(),
            }]),
            columns: vec![Column::AggregateState(column)],
            num_rows,
        };
        assert!(matches!(
            encode_block(&batch, &EncodeOptions::default()),
            Err(EncodeError::InconsistentBatch { .. })
        ));
    }
}

#[test]
fn count_nullable_nothing_spelling_is_not_encodable() {
    // The parser rejects this spelling (it canonicalizes to nothingUInt64 on the
    // wire), so it is constructed directly to prove encode validation also refuses
    // to write a VarUInt Count state under a header the server reads as
    // nothingUInt64.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "c".into(),
            ch_type: ChType::AggregateFunction {
                function: "count".into(),
                arguments: vec![ChType::Nullable(Box::new(ChType::Nothing))],
            },
        }]),
        columns: vec![Column::AggregateState(AggregateStateColumn::new(
            vec![0, 1],
            vec![0x0d],
        ))],
        num_rows: 1,
    };
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::UnsupportedType { .. })
    ));
}

#[test]
fn unsupported_aggregate_function_is_not_encodable() {
    // A hand-built signature with no registered state codec. The parser rejects
    // this spelling, so it is constructed directly to prove encode validation
    // also rejects it.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "s".into(),
            ch_type: ChType::AggregateFunction {
                function: "avg".into(),
                arguments: vec![ChType::UInt64],
            },
        }]),
        columns: vec![Column::AggregateState(AggregateStateColumn::new(
            vec![0, 8],
            13u64.to_le_bytes().to_vec(),
        ))],
        num_rows: 1,
    };
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::UnsupportedType { .. })
    ));
}

#[test]
fn count_with_server_invalid_argument_type_is_not_encodable() {
    for argument in [
        ChType::LowCardinality(Box::new(ChType::Decimal {
            precision: 9,
            scale: 4,
            bits: 32,
        })),
        ChType::Map(
            Box::new(ChType::Nullable(Box::new(ChType::UInt8))),
            Box::new(ChType::UInt8),
        ),
        ChType::Tuple(vec![
            (Some("a".into()), ChType::UInt8),
            (Some("a".into()), ChType::UInt8),
        ]),
    ] {
        // A server-invalid argument type makes the whole header unparseable, so
        // the count type is constructed directly to reach encode validation.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "c".into(),
                ch_type: ChType::AggregateFunction {
                    function: "count".into(),
                    arguments: vec![argument],
                },
            }]),
            columns: vec![Column::AggregateState(AggregateStateColumn::new(
                vec![0, 1],
                vec![0x0d],
            ))],
            num_rows: 1,
        };
        assert!(matches!(
            encode_block(&batch, &EncodeOptions::default()),
            Err(EncodeError::UnsupportedType { .. })
        ));
    }
}
