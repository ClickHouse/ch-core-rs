use super::*;

/// The six temporal columns over four rows. Date/time metadata is carried in
/// the type string only; the bodies are faithful primitive-width integers.
/// Signed types include negative values to prove little-endian round-trips.
fn temporal_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "d".into(),
            ch_type: ChType::Date,
        },
        Field {
            name: "d32".into(),
            ch_type: ChType::Date32,
        },
        Field {
            name: "dt".into(),
            ch_type: ChType::DateTime {
                timezone: Some("UTC".into()),
            },
        },
        Field {
            name: "dt64".into(),
            ch_type: ChType::DateTime64 {
                precision: 3,
                timezone: Some("UTC".into()),
            },
        },
        Field {
            name: "t".into(),
            ch_type: ChType::Time,
        },
        Field {
            name: "t64".into(),
            ch_type: ChType::Time64 { precision: 3 },
        },
    ];
    let columns = vec![
        Column::Date(PrimitiveColumn::new(vec![0, 19000, 19001, u16::MAX])),
        Column::Date32(PrimitiveColumn::new(vec![i32::MIN, -25567, 0, i32::MAX])),
        Column::DateTime(PrimitiveColumn::new(vec![
            0,
            1_600_000_000,
            1_700_000_000,
            u32::MAX,
        ])),
        Column::DateTime64(PrimitiveColumn::new(vec![
            i64::MIN,
            -1_000,
            1_700_000_000_000,
            i64::MAX,
        ])),
        Column::Time(PrimitiveColumn::new(vec![-3_599_999, -13, 0, 3_599_999])),
        Column::Time64(PrimitiveColumn::new(vec![
            -3_599_999_999,
            -13_000,
            0,
            3_599_999_999,
        ])),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// Nullable DateTime64, Time, and Time64 columns over four rows with the
/// valid, null, valid, null pattern.
fn nullable_temporal_batch() -> ColBatch {
    let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let fields = vec![
        Field {
            name: "ndt64".into(),
            ch_type: ChType::Nullable(Box::new(ChType::DateTime64 {
                precision: 3,
                timezone: Some("UTC".into()),
            })),
        },
        Field {
            name: "nt".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Time)),
        },
        Field {
            name: "nt64".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Time64 { precision: 6 })),
        },
    ];
    let columns = vec![
        Column::DateTime64(PrimitiveColumn::new_nullable(
            vec![1_700_000_000_000, 0, -1_000, 0],
            validity(),
        )),
        Column::Time(PrimitiveColumn::new_nullable(
            vec![-13, 0, 79, 0],
            validity(),
        )),
        Column::Time64(PrimitiveColumn::new_nullable(
            vec![-13_000_000, 0, 79_000_000, 0],
            validity(),
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

#[test]
fn roundtrip_temporal_rev0() {
    roundtrip(&temporal_batch(), 0);
}

#[test]
fn roundtrip_temporal_tcp_revision() {
    roundtrip(&temporal_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_temporal_rev0() {
    roundtrip(&nullable_temporal_batch(), 0);
}

#[test]
fn roundtrip_nullable_temporal_tcp_revision() {
    roundtrip(&nullable_temporal_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn encode_chunked_roundtrips_time_blocks() {
    let schema = Schema::new(vec![
        Field {
            name: "t".into(),
            ch_type: ChType::Time,
        },
        Field {
            name: "t64".into(),
            ch_type: ChType::Time64 { precision: 3 },
        },
    ]);
    let make_chunk = |time: Vec<i32>, time64: Vec<i64>| {
        let rows = time.len();
        assert_eq!(time64.len(), rows);
        std::sync::Arc::new(ColBatch::new(
            schema.clone(),
            vec![
                Column::Time(PrimitiveColumn::new(time)),
                Column::Time64(PrimitiveColumn::new(time64)),
            ],
            rows,
        ))
    };
    let batch = ChunkedBatch {
        schema: schema.clone(),
        chunks: vec![
            make_chunk(vec![-13, 0], vec![-13_000, 0]),
            make_chunk(vec![79, 3_599_999], vec![79_000, 3_599_999_999]),
        ],
    };
    for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
        let bytes = encode_chunked(
            &batch,
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
fn rev0_frames_time_signed_little_endian_bytes() {
    let batch = ColBatch::new(
        Schema::new(vec![
            Field {
                name: "t".into(),
                ch_type: ChType::Time,
            },
            Field {
                name: "t64".into(),
                ch_type: ChType::Time64 { precision: 3 },
            },
        ]),
        vec![
            Column::Time(PrimitiveColumn::new(vec![-13])),
            Column::Time64(PrimitiveColumn::new(vec![-79_000])),
        ],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x02, // num_cols = 2
        0x01, // num_rows = 1
        0x01, b't', // name "t"
        0x04, b'T', b'i', b'm', b'e', // type "Time"
        0xF3, 0xFF, 0xFF, 0xFF, // i32 -13 LE
        0x03, b't', b'6', b'4', // name "t64"
        0x09, b'T', b'i', b'm', b'e', b'6', b'4', b'(', b'3', b')', 0x68, 0xCB, 0xFE, 0xFF, 0xFF,
        0xFF, 0xFF, 0xFF, // i64 -79000 LE
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn timezone_with_quote_is_rejected() {
    // A DateTime/DateTime64 timezone containing a single quote renders a
    // malformed header (`DateTime('UTC')')`) that the crate's lenient parser
    // round-trips but the server rejects. It must be caught at the source. Both
    // the bare and Nullable forms, and both temporal types, are covered.
    let cases = [
        ChType::DateTime {
            timezone: Some("UTC')".into()),
        },
        ChType::Nullable(Box::new(ChType::DateTime {
            timezone: Some("UTC')".into()),
        })),
        ChType::DateTime64 {
            precision: 3,
            timezone: Some("UTC')".into()),
        },
    ];
    for ch_type in cases {
        let is_nullable = matches!(ch_type, ChType::Nullable(_));
        let column = match ch_type.inner() {
            ChType::DateTime { .. } => {
                let p = PrimitiveColumn::new(vec![0u32]);
                Column::DateTime(p)
            }
            ChType::DateTime64 { .. } => Column::DateTime64(PrimitiveColumn::new(vec![0i64])),
            other => panic!("unexpected inner {other:?}"),
        };
        // Give a nullable case an all-valid bitmap so only the timezone check fires.
        let column = if is_nullable {
            match column {
                Column::DateTime(mut p) => {
                    p.validity = Some(Bitmap::from_ch_null_map(&[0]));
                    Column::DateTime(p)
                }
                other => other,
            }
        } else {
            column
        };
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "t".into(),
                ch_type,
            }]),
            columns: vec![column],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }
}
