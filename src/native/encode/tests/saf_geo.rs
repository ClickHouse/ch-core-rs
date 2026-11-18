use super::*;
use crate::native::stream_decoder::StreamDecoder;

/// `SimpleAggregateFunction(sum, Float64)` over three rows; the column buffer
/// is the physical Float64 (no new Column variant).
fn simple_aggregate_function_scalar_batch() -> ColBatch {
    let fields = vec![Field {
        name: "s".into(),
        ch_type: ChType::SimpleAggregateFunction {
            func: "sum".into(),
            inner: Box::new(ChType::Float64),
        },
    }];
    let columns = vec![Column::Float64(PrimitiveColumn::new(vec![3.5, -7.25, 0.0]))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// `SimpleAggregateFunction(anyLast, Nullable(String))` over three rows: the
/// alias sits over `Nullable(String)`, so nullability is read off the
/// delegate and the null map precedes the string body.
fn simple_aggregate_function_nullable_batch() -> ColBatch {
    let mut col = utf8_column(&[b"user_1", b"", b"user_2"]);
    col.validity = Some(Bitmap::from_ch_null_map(&[0, 1, 0]));
    let fields = vec![Field {
        name: "s".into(),
        ch_type: ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        },
    }];
    ColBatch::new(Schema::new(fields), vec![Column::Utf8(col)], 3)
}

/// `SimpleAggregateFunction(anyLast, LowCardinality(Nullable(String)))` over
/// four rows (valid, null, valid, null): the shared LC gate under a SAF
/// alias, nullable at the index level.
fn simple_aggregate_function_low_cardinality_batch() -> ColBatch {
    let fields = vec![Field {
        name: "s".into(),
        ch_type: ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::LowCardinality(Box::new(ChType::Nullable(
                Box::new(ChType::String),
            )))),
        },
    }];
    let columns = vec![Column::Dictionary(DictionaryColumn::new_nullable(
        vec![1, 0, 2, 0],
        Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
        Bitmap::from_ch_null_map(&[0, 1, 0, 1]),
    ))];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// `LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))` over
/// four rows (valid, null, valid, null). This is the previously-failing
/// shape: the SAF sits between the `LowCardinality` and its removeNullable
/// `Nullable`, so nullability and the dictionary value type must be resolved
/// through the full SAF chain, not a single-level see-through. Index 0 is the
/// NULL sentinel. Confirmed a real server header live at v26.6.1.1193-stable.
fn low_cardinality_saf_nullable_string_batch() -> ColBatch {
    let fields = vec![Field {
        name: "lc_nsaf".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        })),
    }];
    let columns = vec![Column::Dictionary(DictionaryColumn::new_nullable(
        vec![1, 0, 2, 0],
        Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
        Bitmap::from_ch_null_map(&[0, 1, 0, 1]),
    ))];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// The same `LowCardinality(SimpleAggregateFunction(anyLast,
/// Nullable(String)))` type over three all-valid rows (no index-0 references),
/// still nullable at the type level.
fn low_cardinality_saf_nullable_string_all_valid_batch() -> ColBatch {
    let fields = vec![Field {
        name: "lc_nsaf".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        })),
    }];
    let columns = vec![Column::Dictionary(DictionaryColumn::new_nullable(
        vec![1, 2, 1],
        Column::Utf8(utf8_column(&[b"", b"user_3", b"user_4"])),
        Bitmap::from_ch_null_map(&[0, 0, 0]),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// A chained `SimpleAggregateFunction` as a `LowCardinality` inner:
/// `LowCardinality(SimpleAggregateFunction(anyLast,
/// SimpleAggregateFunction(sum, UInt64)))`. The full SAF chain resolves to a
/// plain non-nullable `UInt64` dictionary body; a single-level see-through
/// would leave an alias and die at write. The chain is live-constructible at
/// v26.6.1.1193-stable.
fn low_cardinality_chained_saf_batch() -> ColBatch {
    let fields = vec![Field {
        name: "lc_saf2".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::SimpleAggregateFunction {
                func: "sum".into(),
                inner: Box::new(ChType::UInt64),
            }),
        })),
    }];
    let columns = vec![Column::Dictionary(DictionaryColumn::new(
        vec![1, 2, 1],
        Column::UInt64(PrimitiveColumn::new(vec![0, 13, 79])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// A standalone chained `SimpleAggregateFunction(anyLast,
/// SimpleAggregateFunction(sum, UInt64))` over three rows: the buffer is the
/// physical `UInt64` and the whole chain resolves through the delegate.
fn chained_simple_aggregate_function_batch() -> ColBatch {
    let fields = vec![Field {
        name: "saf2".into(),
        ch_type: ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::SimpleAggregateFunction {
                func: "sum".into(),
                inner: Box::new(ChType::UInt64),
            }),
        },
    }];
    let columns = vec![Column::UInt64(PrimitiveColumn::new(vec![
        13,
        79,
        8_589_934_592,
    ]))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// `Point` over two rows: the column buffer is a two-field `Tuple(Float64,
/// Float64)` (unnamed), field-major on the wire.
fn point_batch() -> ColBatch {
    let fields = vec![Field {
        name: "p".into(),
        ch_type: ChType::Geo(GeoKind::Point),
    }];
    let columns = vec![Column::Tuple(TupleColumn::new(
        vec![
            Column::Float64(PrimitiveColumn::new(vec![1.0, 3.0])),
            Column::Float64(PrimitiveColumn::new(vec![2.0, 4.0])),
        ],
        2,
    ))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// `Nullable(Point)` over two rows (valid, null): the tuple-level validity
/// bitmap plus a null-row placeholder point, the ordinary `Nullable(Tuple)`
/// framing.
fn nullable_point_batch() -> ColBatch {
    let fields = vec![Field {
        name: "p".into(),
        ch_type: ChType::Nullable(Box::new(ChType::Geo(GeoKind::Point))),
    }];
    let columns = vec![Column::Tuple(TupleColumn::new_nullable(
        vec![
            Column::Float64(PrimitiveColumn::new(vec![1.0, 0.0])),
            Column::Float64(PrimitiveColumn::new(vec![2.0, 0.0])),
        ],
        2,
        Bitmap::from_ch_null_map(&[0, 1]),
    ))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// `MultiPoint` over two rows. Row 0 holds two points and row 1 holds one,
/// using the ordinary `Array(Point)` offsets plus field-major Point body.
fn multi_point_batch() -> ColBatch {
    let point = Column::Tuple(TupleColumn::new(
        vec![
            Column::Float64(PrimitiveColumn::new(vec![1.0, 3.0, 5.0])),
            Column::Float64(PrimitiveColumn::new(vec![2.0, 4.0, 6.0])),
        ],
        3,
    ));
    let multi_point = Column::Array(ArrayColumn::new(vec![0, 2, 3], point));
    let fields = vec![Field {
        name: "mp".into(),
        ch_type: ChType::Geo(GeoKind::MultiPoint),
    }];
    ColBatch::new(Schema::new(fields), vec![multi_point], 2)
}

/// `MultiPolygon` over two rows, exercising all four expanded Array/Tuple
/// levels. Row 0 holds one polygon of one ring of two points; row 1 is empty.
fn multi_polygon_batch() -> ColBatch {
    let point = Column::Tuple(TupleColumn::new(
        vec![
            Column::Float64(PrimitiveColumn::new(vec![1.0, 3.0])),
            Column::Float64(PrimitiveColumn::new(vec![2.0, 4.0])),
        ],
        2,
    ));
    let ring = Column::Array(ArrayColumn::new(vec![0, 2], point)); // one ring, two points
    let polygon = Column::Array(ArrayColumn::new(vec![0, 1], ring)); // one ring
    let multi = Column::Array(ArrayColumn::new(vec![0, 1, 1], polygon)); // row0: 1 polygon, row1: empty
    let fields = vec![Field {
        name: "mp".into(),
        ch_type: ChType::Geo(GeoKind::MultiPolygon),
    }];
    ColBatch::new(Schema::new(fields), vec![multi], 2)
}

#[test]
fn roundtrip_simple_aggregate_function_scalar_rev0() {
    roundtrip(&simple_aggregate_function_scalar_batch(), 0);
}

#[test]
fn roundtrip_simple_aggregate_function_scalar_tcp_revision() {
    roundtrip(
        &simple_aggregate_function_scalar_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_simple_aggregate_function_nullable_rev0() {
    roundtrip(&simple_aggregate_function_nullable_batch(), 0);
}

#[test]
fn roundtrip_simple_aggregate_function_nullable_tcp_revision() {
    roundtrip(
        &simple_aggregate_function_nullable_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_simple_aggregate_function_low_cardinality_rev0() {
    roundtrip(&simple_aggregate_function_low_cardinality_batch(), 0);
}

#[test]
fn roundtrip_simple_aggregate_function_low_cardinality_tcp_revision() {
    roundtrip(
        &simple_aggregate_function_low_cardinality_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_low_cardinality_saf_nullable_string_rev0() {
    roundtrip(&low_cardinality_saf_nullable_string_batch(), 0);
}

#[test]
fn roundtrip_low_cardinality_saf_nullable_string_tcp_revision() {
    roundtrip(
        &low_cardinality_saf_nullable_string_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_low_cardinality_saf_nullable_string_all_valid_rev0() {
    roundtrip(&low_cardinality_saf_nullable_string_all_valid_batch(), 0);
}

#[test]
fn roundtrip_low_cardinality_saf_nullable_string_all_valid_tcp_revision() {
    roundtrip(
        &low_cardinality_saf_nullable_string_all_valid_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_low_cardinality_chained_saf_rev0() {
    roundtrip(&low_cardinality_chained_saf_batch(), 0);
}

#[test]
fn roundtrip_low_cardinality_chained_saf_tcp_revision() {
    roundtrip(
        &low_cardinality_chained_saf_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_chained_simple_aggregate_function_rev0() {
    roundtrip(&chained_simple_aggregate_function_batch(), 0);
}

#[test]
fn roundtrip_chained_simple_aggregate_function_tcp_revision() {
    roundtrip(
        &chained_simple_aggregate_function_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn encode_low_cardinality_saf_nullable_null_row_succeeds() {
    // The previously-failing shape: encoding a null row of
    // LowCardinality(SAF(anyLast, Nullable(String))) used to be rejected as an
    // InconsistentBatch because nullability was read one SAF level too shallow.
    // It must now encode, and the encoder's own output must decode back.
    let batch = low_cardinality_saf_nullable_string_batch();
    let bytes = encode_block(&batch, &EncodeOptions::default())
        .expect("encoding a null row of LC(SAF(Nullable(String))) must succeed");
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default())
        .expect("decoding the encoder's own LC(SAF(Nullable)) output must succeed");
    assert_eq!(decoded.num_chunks(), 1);
    assert_batches_eq(&batch, &decoded.chunks[0]);
}

#[test]
fn encode_zero_row_low_cardinality_saf_nullable_block() {
    // A zero-row LC(SAF(anyLast, Nullable(String))) block encodes (no column
    // data is written for a zero-row block) and decodes back to just the
    // schema with no chunks, exercising the empty_column delegate path.
    let fields = vec![Field {
        name: "lc_nsaf".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        })),
    }];
    let columns = vec![Column::Dictionary(DictionaryColumn::new_nullable(
        vec![],
        Column::Utf8(utf8_column(&[])),
        Bitmap::from_ch_null_map(&[]),
    ))];
    let batch = ColBatch::new(Schema::new(fields.clone()), columns, 0);
    let bytes = encode_block(&batch, &EncodeOptions::default())
        .expect("encoding a zero-row LC(SAF(Nullable)) block must succeed");
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default())
        .expect("decoding a zero-row LC(SAF(Nullable)) block must succeed");
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema.fields[0].ch_type, fields[0].ch_type);
}

/// `Nullable(SimpleAggregateFunction(sum, UInt64))` over three rows (valid,
/// null, valid): the null map precedes the UInt64 run, nullability read off
/// the delegate.
fn nullable_simple_aggregate_function_batch() -> ColBatch {
    let fields = vec![Field {
        name: "s".into(),
        ch_type: ChType::Nullable(Box::new(ChType::SimpleAggregateFunction {
            func: "sum".into(),
            inner: Box::new(ChType::UInt64),
        })),
    }];
    let columns = vec![Column::UInt64(PrimitiveColumn {
        values: vec![13, 0, 79],
        validity: Some(Bitmap::from_ch_null_map(&[0, 1, 0])),
    })];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// `Array(SimpleAggregateFunction(sum, UInt64))` over two rows: `[13, 79]`,
/// `[5]`. Offsets then the flattened UInt64 run.
fn array_simple_aggregate_function_batch() -> ColBatch {
    let fields = vec![Field {
        name: "a".into(),
        ch_type: ChType::Array(Box::new(ChType::SimpleAggregateFunction {
            func: "sum".into(),
            inner: Box::new(ChType::UInt64),
        })),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 3],
        Column::UInt64(PrimitiveColumn::new(vec![13, 79, 5])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// `LowCardinality(SimpleAggregateFunction(anyLast, String))` over three rows:
/// the LC body decodes/encodes as its physical `String` inner.
fn low_cardinality_simple_aggregate_function_batch() -> ColBatch {
    let fields = vec![Field {
        name: "s".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::String),
        })),
    }];
    let columns = vec![Column::Dictionary(DictionaryColumn::new(
        vec![1, 2, 1],
        Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// `Tuple(v SimpleAggregateFunction(sum, UInt64))` over two rows: one field
/// column of the physical UInt64.
fn tuple_simple_aggregate_function_batch() -> ColBatch {
    let fields = vec![Field {
        name: "t".into(),
        ch_type: ChType::Tuple(vec![(
            Some("v".into()),
            ChType::SimpleAggregateFunction {
                func: "sum".into(),
                inner: Box::new(ChType::UInt64),
            },
        )]),
    }];
    let columns = vec![Column::Tuple(TupleColumn::new(
        vec![Column::UInt64(PrimitiveColumn::new(vec![13, 79]))],
        2,
    ))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// `SimpleAggregateFunction(groupArrayLastArray(5), Array(UInt64))` over two
/// rows: a parametrized function name whose balanced `(5)` suffix must survive
/// the header round-trip.
fn simple_aggregate_function_parametrized_batch() -> ColBatch {
    let fields = vec![Field {
        name: "s".into(),
        ch_type: ChType::SimpleAggregateFunction {
            func: "groupArrayLastArray(5)".into(),
            inner: Box::new(ChType::Array(Box::new(ChType::UInt64))),
        },
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 3],
        Column::UInt64(PrimitiveColumn::new(vec![13, 79, 5])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

#[test]
fn roundtrip_nullable_simple_aggregate_function_rev0() {
    roundtrip(&nullable_simple_aggregate_function_batch(), 0);
}

#[test]
fn roundtrip_nullable_simple_aggregate_function_tcp_revision() {
    roundtrip(
        &nullable_simple_aggregate_function_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_array_simple_aggregate_function_rev0() {
    roundtrip(&array_simple_aggregate_function_batch(), 0);
}

#[test]
fn roundtrip_array_simple_aggregate_function_tcp_revision() {
    roundtrip(
        &array_simple_aggregate_function_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_low_cardinality_simple_aggregate_function_rev0() {
    roundtrip(&low_cardinality_simple_aggregate_function_batch(), 0);
}

#[test]
fn roundtrip_low_cardinality_simple_aggregate_function_tcp_revision() {
    roundtrip(
        &low_cardinality_simple_aggregate_function_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_tuple_simple_aggregate_function_rev0() {
    roundtrip(&tuple_simple_aggregate_function_batch(), 0);
}

#[test]
fn roundtrip_tuple_simple_aggregate_function_tcp_revision() {
    roundtrip(
        &tuple_simple_aggregate_function_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_simple_aggregate_function_parametrized_func() {
    // Fix 3 correctness check: encode-then-decode equality for a parametrized
    // function name, proving Display(parse(...)) holds through the header.
    roundtrip(&simple_aggregate_function_parametrized_batch(), 0);
    roundtrip(
        &simple_aggregate_function_parametrized_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn encode_rejects_malformed_simple_aggregate_function_func() {
    // A caller-constructed SAF `func` is untrusted and Displayed into the
    // header's type-string channel, so a malformed spelling is rejected as
    // UnsupportedType before any bytes are written (injection guard). The
    // server's function whitelist is deliberately NOT enforced here.
    for bad_func in [
        "sum, UInt64), evil", // a top-level comma would inject extra type tokens
        "sum(",               // unbalanced open paren
        "sum)",               // stray close paren
        "sum(a))",            // paren imbalance in the params suffix
        "",                   // empty
        "1sum",               // leading digit
        "sum bar",            // embedded space
    ] {
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "s".into(),
                ch_type: ChType::SimpleAggregateFunction {
                    func: bad_func.into(),
                    inner: Box::new(ChType::UInt64),
                },
            }]),
            vec![Column::UInt64(PrimitiveColumn::new(vec![13]))],
            1,
        );
        assert!(
            matches!(
                encode_block(&batch, &EncodeOptions::default()),
                Err(EncodeError::UnsupportedType { .. })
            ),
            "func {bad_func:?} should be rejected as UnsupportedType"
        );
    }

    // A malformed SAF func nested inside a container is caught too: the walk
    // descends every wrapper/container.
    let nested_bad = ColBatch::new(
        Schema::new(vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::SimpleAggregateFunction {
                func: "sum, evil".into(),
                inner: Box::new(ChType::UInt64),
            })),
        }]),
        vec![Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::UInt64(PrimitiveColumn::new(vec![13])),
        ))],
        1,
    );
    assert!(matches!(
        encode_block(&nested_bad, &EncodeOptions::default()),
        Err(EncodeError::UnsupportedType { .. })
    ));
}

#[test]
fn roundtrip_point_rev0() {
    roundtrip(&point_batch(), 0);
}

#[test]
fn roundtrip_point_tcp_revision() {
    roundtrip(&point_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_point_rev0() {
    roundtrip(&nullable_point_batch(), 0);
}

#[test]
fn roundtrip_nullable_point_tcp_revision() {
    roundtrip(&nullable_point_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_multi_point_rev0() {
    roundtrip(&multi_point_batch(), 0);
}

#[test]
fn roundtrip_multi_point_tcp_revision() {
    roundtrip(&multi_point_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_multi_polygon_rev0() {
    roundtrip(&multi_polygon_batch(), 0);
}

#[test]
fn roundtrip_multi_polygon_tcp_revision() {
    roundtrip(&multi_polygon_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn multi_block_multi_point_roundtrips() {
    // Two standalone MultiPoint blocks stay separate chunks through encode ->
    // decode, preserving the Array(Point) buffers in each chunk.
    let batch = ChunkedBatch {
        schema: multi_point_batch().schema.clone(),
        chunks: vec![
            std::sync::Arc::new(multi_point_batch()),
            std::sync::Arc::new(multi_point_batch()),
        ],
    };
    let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    assert_batches_eq(&multi_point_batch(), &decoded.chunks[0]);
    assert_batches_eq(&multi_point_batch(), &decoded.chunks[1]);
}

#[test]
fn multi_block_point_roundtrips() {
    // Two standalone Point blocks stay separate chunks through encode ->
    // decode, preserving the Tuple(Float64, Float64) buffers in each chunk.
    let batch = ChunkedBatch {
        schema: point_batch().schema.clone(),
        chunks: vec![
            std::sync::Arc::new(point_batch()),
            std::sync::Arc::new(point_batch()),
        ],
    };
    let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    assert_batches_eq(&point_batch(), &decoded.chunks[0]);
    assert_batches_eq(&point_batch(), &decoded.chunks[1]);
}

#[test]
fn rev0_frames_simple_aggregate_function_bytes() {
    // The header carries the VERBATIM alias spelling and the body is
    // byte-identical to the bare inner Float64.
    let bytes = encode_block(
        &simple_aggregate_function_scalar_batch(),
        &EncodeOptions::default(),
    )
    .unwrap();
    let mut expected = vec![
        0x01, // num_cols = 1
        0x03, // num_rows = 3
        0x01, b's', // name "s"
        0x25, // type string length = 37
    ];
    expected.extend_from_slice(b"SimpleAggregateFunction(sum, Float64)");
    expected.extend_from_slice(&3.5f64.to_le_bytes());
    expected.extend_from_slice(&(-7.25f64).to_le_bytes());
    expected.extend_from_slice(&0.0f64.to_le_bytes());
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_point_bytes() {
    // Point body is the field-major Tuple(Float64, Float64): all X then all Y,
    // no offsets, no tuple-level framing.
    let bytes = encode_block(&point_batch(), &EncodeOptions::default()).unwrap();
    let mut expected = vec![
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x01, b'p', // name "p"
        0x05, b'P', b'o', b'i', b'n', b't', // type "Point"
    ];
    expected.extend_from_slice(&1.0f64.to_le_bytes()); // X row 0
    expected.extend_from_slice(&3.0f64.to_le_bytes()); // X row 1
    expected.extend_from_slice(&2.0f64.to_le_bytes()); // Y row 0
    expected.extend_from_slice(&4.0f64.to_le_bytes()); // Y row 1
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_multi_point_bytes() {
    // MultiPoint is the ordinary Array(Point) body: cumulative UInt64 offsets,
    // then all X coordinates, then all Y coordinates.
    let bytes = encode_block(&multi_point_batch(), &EncodeOptions::default()).unwrap();
    let mut expected = vec![
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x02, b'm', b'p', // name "mp"
        0x0a, // type-name length
    ];
    expected.extend_from_slice(b"MultiPoint");
    expected.extend_from_slice(&2u64.to_le_bytes());
    expected.extend_from_slice(&3u64.to_le_bytes());
    for value in [1.0f64, 3.0, 5.0, 2.0, 4.0, 6.0] {
        expected.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(bytes, expected);
}

#[test]
fn type_depth_of_alias_matches_physical_delegate() {
    // The encoder's depth cap must count a name-decoration alias at its
    // physical depth so a geo/Nested type near the cap is not under-counted,
    // and it must count it the SAME as the decode parser so a type that
    // decodes is always re-encodable.
    //
    // Geo and Nested are charged exactly their physical expansion depth.
    for kind in [
        GeoKind::Point,
        GeoKind::Ring,
        GeoKind::LineString,
        GeoKind::MultiLineString,
        GeoKind::Polygon,
        GeoKind::MultiPolygon,
        GeoKind::MultiPoint,
    ] {
        let alias = ChType::Geo(kind);
        assert_eq!(
            type_depth(&alias),
            type_depth(&kind.underlying_type()),
            "geo {kind:?} depth mismatch"
        );
        // And the geo token's own charge equals its expansion_depth constant.
        assert_eq!(type_depth(&alias), kind.expansion_depth());
    }
    let geometry = ChType::Geometry;
    assert_eq!(
        type_depth(&geometry),
        type_depth(&geometry.physical_delegate().unwrap())
    );
    assert_eq!(
        type_depth(&geometry),
        crate::schema::GEOMETRY_EXPANSION_DEPTH
    );
    let nested = ChType::Nested(vec![
        ("a".into(), ChType::UInt32),
        ("b".into(), ChType::Array(Box::new(ChType::String))),
    ]);
    assert_eq!(
        type_depth(&nested),
        type_depth(&nested.physical_delegate().unwrap())
    );
    // SimpleAggregateFunction charges ONE level over its inner (not zero): it
    // expands via one extra decode recursion frame, so both the parser and
    // type_depth charge +1 to bound a hostile chain of nested SAFs and keep
    // the two directions aligned.
    let saf = ChType::SimpleAggregateFunction {
        func: "sum".into(),
        inner: Box::new(ChType::Array(Box::new(ChType::Float64))),
    };
    assert_eq!(
        type_depth(&saf),
        type_depth(&saf.physical_delegate().unwrap()) + 1
    );
}

#[test]
fn decode_accept_implies_encode_accept_at_the_cap() {
    // Fix 4 boundary: any type the decode parser accepts must pass the
    // encoder's type_depth cap, and vice versa, for a geo-tipped and a
    // Nested-tipped chain. Walk Array nesting from just under to just over the
    // point where the alias expansion crosses MAX_TYPE_DEPTH and confirm the
    // two sides flip together.
    for (label, tip, expansion) in [
        ("geo", ChType::Geo(GeoKind::MultiPolygon), 4usize),
        (
            "geometry",
            ChType::Geometry,
            crate::schema::GEOMETRY_EXPANSION_DEPTH,
        ),
        (
            "nested",
            ChType::Nested(vec![("a".into(), ChType::UInt32)]),
            2usize,
        ),
    ] {
        // arrays + expansion must be <= MAX_TYPE_DEPTH to be accepted, so the
        // last accepted array count is MAX_TYPE_DEPTH - expansion.
        let last_ok = MAX_TYPE_DEPTH - expansion;
        for arrays in [last_ok, last_ok + 1] {
            let mut ty = tip.clone();
            for _ in 0..arrays {
                ty = ChType::Array(Box::new(ty));
            }
            let parse_ok = parse_ch_type(&ty.to_string()).is_some();
            let encode_ok = type_depth(&ty) <= MAX_TYPE_DEPTH;
            assert_eq!(
                parse_ok, encode_ok,
                "{label} chain with {arrays} arrays: decode-accept {parse_ok} but encode-accept {encode_ok}"
            );
            // At exactly last_ok both accept; one deeper both reject.
            assert_eq!(parse_ok, arrays == last_ok, "{label} {arrays} arrays");
        }
    }
}

#[test]
fn encode_rejects_geo_type_over_the_depth_cap() {
    // A geo type wrapped in enough Arrays that its physical expansion exceeds
    // MAX_TYPE_DEPTH is rejected as InconsistentBatch (the same iterative cap
    // as any deep caller-constructed type). MultiPolygon adds four physical
    // levels, so wrapping it in MAX_TYPE_DEPTH Arrays pushes it over.
    let mut ty = ChType::Geo(GeoKind::MultiPolygon);
    for _ in 0..MAX_TYPE_DEPTH {
        ty = ChType::Array(Box::new(ty));
    }
    assert!(type_depth(&ty) > MAX_TYPE_DEPTH);
    // The decode parser now charges the geo expansion too, so it rejects the
    // very same over-deep header: the two sides agree instead of the encoder
    // rejecting a type the decoder would have accepted (the old asymmetry).
    assert_eq!(parse_ch_type(&ty.to_string()), None);
    // A one-row batch whose column buffer is irrelevant: the depth check runs
    // first. Use a zero-row batch to avoid building the deep nesting buffer.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "g".into(),
            ch_type: ty,
        }]),
        columns: vec![Column::Array(ArrayColumn::new(vec![0], empty_deep_array()))],
        num_rows: 0,
    };
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}

/// A throwaway element column for the depth-cap rejection test; the depth
/// check fires before the buffer is inspected, so its exact shape does not
/// matter.
fn empty_deep_array() -> Column {
    Column::Array(ArrayColumn::new(
        vec![0],
        Column::Float64(PrimitiveColumn::new(vec![])),
    ))
}

fn nested_nullable_saf_batches(null_map: &[u8], wrong_variant: bool) -> Vec<ColBatch> {
    let rows = null_map.len();
    let validity = Bitmap::from_ch_null_map(null_map);
    let values = if wrong_variant {
        Column::UInt32(PrimitiveColumn::new_nullable(vec![13; rows], validity))
    } else {
        Column::UInt64(PrimitiveColumn::new_nullable(vec![13; rows], validity))
    };
    let offsets: Vec<_> = (0..=rows as i64).collect();
    let tuple = || Column::Tuple(TupleColumn::new(vec![values.clone()], rows));
    let cases = [
        (
            "Array(SimpleAggregateFunction(max, Nullable(UInt64)))",
            Column::Array(ArrayColumn::new(offsets.clone(), values.clone())),
        ),
        (
            "Tuple(SimpleAggregateFunction(max, Nullable(UInt64)))",
            tuple(),
        ),
        (
            "Tuple(value SimpleAggregateFunction(max, Nullable(UInt64)))",
            tuple(),
        ),
        (
            "Map(UInt64, SimpleAggregateFunction(max, Nullable(UInt64)))",
            Column::Map(map_column(
                offsets.clone(),
                Column::UInt64(PrimitiveColumn::new(vec![79; rows])),
                values.clone(),
            )),
        ),
        (
            "Nested(value SimpleAggregateFunction(max, Nullable(UInt64)))",
            Column::Array(ArrayColumn::new(offsets.clone(), tuple())),
        ),
        (
            "Array(SimpleAggregateFunction(max, SimpleAggregateFunction(max, Nullable(UInt64))))",
            Column::Array(ArrayColumn::new(offsets.clone(), values.clone())),
        ),
        (
            "Array(Nullable(SimpleAggregateFunction(max, UInt64)))",
            Column::Array(ArrayColumn::new(offsets.clone(), values.clone())),
        ),
        (
            "Array(SimpleAggregateFunction(max, Nullable(SimpleAggregateFunction(max, UInt64))))",
            Column::Array(ArrayColumn::new(offsets.clone(), values.clone())),
        ),
        (
            "Nullable(SimpleAggregateFunction(max, SimpleAggregateFunction(max, UInt64)))",
            values.clone(),
        ),
        (
            "SimpleAggregateFunction(max, Nullable(SimpleAggregateFunction(max, SimpleAggregateFunction(max, UInt64))))",
            values.clone(),
        ),
        (
            "Array(Nullable(SimpleAggregateFunction(max, SimpleAggregateFunction(max, UInt64))))",
            Column::Array(ArrayColumn::new(offsets.clone(), values.clone())),
        ),
        (
            "Tuple(Nullable(SimpleAggregateFunction(max, SimpleAggregateFunction(max, UInt64))))",
            tuple(),
        ),
        (
            "Map(UInt64, Nullable(SimpleAggregateFunction(max, SimpleAggregateFunction(max, UInt64))))",
            Column::Map(map_column(
                offsets.clone(),
                Column::UInt64(PrimitiveColumn::new(vec![79; rows])),
                values.clone(),
            )),
        ),
        (
            "Nested(value Nullable(SimpleAggregateFunction(max, SimpleAggregateFunction(max, UInt64))))",
            Column::Array(ArrayColumn::new(offsets.clone(), tuple())),
        ),
        (
            "Variant(Array(SimpleAggregateFunction(max, Nullable(UInt64))), String)",
            Column::Variant(VariantColumn::try_new(
                &vec![0; rows],
                vec![
                    Column::Array(ArrayColumn::new(offsets, values)),
                    Column::Utf8(utf8_column(&[])),
                ],
            ).unwrap()),
        ),
    ];
    cases
        .into_iter()
        .map(|(name, column)| {
            ColBatch::new(
                Schema::new(vec![Field {
                    name: "value".into(),
                    ch_type: parse_ch_type(name).unwrap(),
                }]),
                vec![column],
                rows,
            )
        })
        .collect()
}

#[test]
fn roundtrip_nested_nullable_saf() {
    for null_map in [&[0, 0][..], &[0, 1], &[1, 1], &[]] {
        for batch in nested_nullable_saf_batches(null_map, false) {
            for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
                if batch.num_rows == 0 {
                    let bytes = encode_block(
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
                            ..DecodeOptions::default()
                        },
                    )
                    .unwrap();
                    assert_eq!(decoded.schema, batch.schema);
                    assert_eq!(decoded.num_rows(), 0);
                } else {
                    roundtrip(&batch, revision);
                }
            }
        }
    }
}

#[test]
fn nested_nullable_saf_rejects_wrong_buffers() {
    for null_map in [&[0, 0][..], &[0, 1], &[]] {
        for batch in nested_nullable_saf_batches(null_map, true) {
            assert!(matches!(
                encode_block(&batch, &EncodeOptions::default()),
                Err(EncodeError::InconsistentBatch { .. })
            ));
        }
    }
}

#[test]
fn nested_saf_preserves_nullability_validation() {
    for mut batch in nested_nullable_saf_batches(&[0, 1], false) {
        let name = batch.schema.fields[0].ch_type.to_string();
        let non_nullable = name
            .replace("Nullable(UInt64)", "UInt64")
            .replace(
                "Nullable(SimpleAggregateFunction(max, UInt64))",
                "SimpleAggregateFunction(max, UInt64)",
            )
            .replace(
                "Nullable(SimpleAggregateFunction(max, SimpleAggregateFunction(max, UInt64)))",
                "SimpleAggregateFunction(max, SimpleAggregateFunction(max, UInt64))",
            );
        assert_ne!(name, non_nullable);
        batch.schema.fields[0].ch_type = parse_ch_type(&non_nullable).unwrap();
        assert!(matches!(
            encode_block(&batch, &EncodeOptions::default()),
            Err(EncodeError::InconsistentBatch { .. })
        ));
    }

    let mut batch = nested_nullable_saf_batches(&[0, 1], false).remove(0);
    let Column::Array(array) = &mut batch.columns[0] else {
        panic!("expected Array");
    };
    let Column::UInt64(values) = array.values.as_mut() else {
        panic!("expected UInt64");
    };
    values.validity = Some(Bitmap::from_ch_null_map(&[0]));
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}

#[test]
fn nested_saf_map_key_validation() {
    for rows in [0, 2] {
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "value".into(),
                ch_type: parse_ch_type(
                    "Map(SimpleAggregateFunction(max, SimpleAggregateFunction(max, UInt64)), UInt64)",
                )
                .unwrap(),
            }]),
            vec![Column::Map(map_column(
                (0..=rows as i64).collect(),
                Column::UInt64(PrimitiveColumn::new(vec![13; rows])),
                Column::UInt64(PrimitiveColumn::new(vec![79; rows])),
            ))],
            rows,
        );
        if rows > 0 {
            roundtrip(&batch, 0);
            roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);
        } else {
            encode_block(&batch, &EncodeOptions::default()).unwrap();
        }
        let mut invalid = batch.clone();
        let Column::Map(map) = &mut invalid.columns[0] else {
            panic!("expected Map");
        };
        let Column::Tuple(entries) = map.entries.as_mut() else {
            panic!("expected Tuple");
        };
        entries.fields[0] = Column::UInt32(PrimitiveColumn::new(vec![13; rows]));
        assert!(matches!(
            encode_block(&invalid, &EncodeOptions::default()),
            Err(EncodeError::InconsistentBatch { .. })
        ));
        let mut invalid = batch;
        invalid.schema.fields[0].ch_type = ChType::Map(
            Box::new(ChType::SimpleAggregateFunction {
                func: "max".into(),
                inner: Box::new(ChType::Nullable(Box::new(ChType::UInt64))),
            }),
            Box::new(ChType::UInt64),
        );
        assert!(matches!(
            encode_block(&invalid, &EncodeOptions::default()),
            Err(EncodeError::UnsupportedType { .. })
        ));
    }
}

#[test]
fn rev0_frames_nullable_chained_saf_bytes() {
    let name = "Array(SimpleAggregateFunction(max, Nullable(SimpleAggregateFunction(max, SimpleAggregateFunction(max, UInt64)))))";
    for null_map in [&[0, 0][..], &[0, 1], &[1, 1], &[]] {
        let rows = null_map.len();
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "value".into(),
                ch_type: parse_ch_type(name).unwrap(),
            }]),
            vec![Column::Array(ArrayColumn::new(
                (0..=rows as i64).collect(),
                Column::UInt64(PrimitiveColumn::new_nullable(
                    vec![13; rows],
                    Bitmap::from_ch_null_map(null_map),
                )),
            ))],
            rows,
        );
        let mut expected = vec![1, rows as u8];
        write_string(&mut expected, b"value");
        write_string(&mut expected, name.as_bytes());
        for offset in 1..=rows as u64 {
            expected.extend_from_slice(&offset.to_le_bytes());
        }
        expected.extend_from_slice(null_map);
        for _ in 0..rows {
            expected.extend_from_slice(&13u64.to_le_bytes());
        }
        assert_eq!(
            encode_block(&batch, &EncodeOptions::default()).unwrap(),
            expected
        );
        if rows > 0 {
            roundtrip(&batch, 0);
            roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);
        }
    }
}

fn roundtrip_streamed_saf(batch: &ColBatch, revision: u64) {
    let bytes = encode_block(
        batch,
        &EncodeOptions {
            protocol_revision: revision,
            ..EncodeOptions::default()
        },
    )
    .unwrap();
    let mut decoder = StreamDecoder::new(DecodeOptions {
        protocol_revision: revision,
        ..DecodeOptions::default()
    });
    let mut chunks = Vec::new();
    for byte in bytes.chunks(1) {
        chunks.extend(decoder.feed(byte).unwrap());
    }
    chunks.extend(decoder.finish().unwrap());
    assert_eq!(chunks.len(), 1);
    assert_batches_eq(batch, &chunks[0]);
}

#[test]
fn stream_nested_nullable_saf() {
    for null_map in [&[0, 0][..], &[0, 1], &[1, 1]] {
        for batch in nested_nullable_saf_batches(null_map, false) {
            for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
                roundtrip_streamed_saf(&batch, revision);
            }
        }
    }
}

#[test]
fn roundtrip_nullable_saf_point() {
    for name in [
        "Nullable(SimpleAggregateFunction(max, Point))",
        "Nullable(SimpleAggregateFunction(max, SimpleAggregateFunction(max, Point)))",
        "SimpleAggregateFunction(max, Nullable(SimpleAggregateFunction(max, Point)))",
    ] {
        let mut batch = nullable_point_batch();
        batch.schema.fields[0].ch_type = parse_ch_type(name).unwrap();
        for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
            roundtrip(&batch, revision);
            roundtrip_streamed_saf(&batch, revision);
        }
    }
}
