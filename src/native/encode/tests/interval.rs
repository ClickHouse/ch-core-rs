use super::*;

const INTERVALS: [(&str, IntervalKind); 11] = [
    ("iy", IntervalKind::Year),
    ("iq", IntervalKind::Quarter),
    ("imo", IntervalKind::Month),
    ("iw", IntervalKind::Week),
    ("id", IntervalKind::Day),
    ("ih", IntervalKind::Hour),
    ("imi", IntervalKind::Minute),
    ("is", IntervalKind::Second),
    ("ims", IntervalKind::Millisecond),
    ("ius", IntervalKind::Microsecond),
    ("ins", IntervalKind::Nanosecond),
];

fn interval_batch() -> ColBatch {
    let mut fields: Vec<Field> = INTERVALS
        .iter()
        .map(|(name, kind)| Field {
            name: (*name).to_string(),
            ch_type: ChType::Interval(*kind),
        })
        .collect();
    let mut columns: Vec<Column> = INTERVALS
        .iter()
        .map(|_| Column::Interval(PrimitiveColumn::new(vec![i64::MIN, -79, 0, i64::MAX])))
        .collect();

    fields.push(Field {
        name: "nid".into(),
        ch_type: ChType::Nullable(Box::new(ChType::Interval(IntervalKind::Day))),
    });
    columns.push(Column::Interval(PrimitiveColumn::new_nullable(
        vec![13, 0, 79, 0],
        Bitmap::from_ch_null_map(&[0, 1, 0, 1]),
    )));

    fields.push(Field {
        name: "lc_ih".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::Interval(IntervalKind::Hour))),
    });
    columns.push(Column::Dictionary(DictionaryColumn::new(
        vec![1, 2, 1, 3],
        Column::Interval(PrimitiveColumn::new(vec![0, 13, 79, 258])),
    )));

    ColBatch::new(Schema::new(fields), columns, 4)
}

#[test]
fn roundtrip_intervals_rev0() {
    roundtrip(&interval_batch(), 0);
}

#[test]
fn roundtrip_intervals_tcp_revision() {
    roundtrip(&interval_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn encode_chunked_roundtrips_interval_blocks() {
    let schema = Schema::new(vec![Field {
        name: "i".into(),
        ch_type: ChType::Interval(IntervalKind::Month),
    }]);
    let make_chunk = |values: Vec<i64>| {
        let rows = values.len();
        std::sync::Arc::new(ColBatch::new(
            schema.clone(),
            vec![Column::Interval(PrimitiveColumn::new(values))],
            rows,
        ))
    };
    let batch = ChunkedBatch {
        schema: schema.clone(),
        chunks: vec![make_chunk(vec![-13, 0]), make_chunk(vec![79, 258])],
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
                ..DecodeOptions::default()
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
fn rev0_frames_interval_signed_little_endian_bytes() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "i".into(),
            ch_type: ChType::Interval(IntervalKind::Nanosecond),
        }]),
        vec![Column::Interval(PrimitiveColumn::new(vec![-79]))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'i', // name "i"
        0x12, b'I', b'n', b't', b'e', b'r', b'v', b'a', b'l', b'N', b'a', b'n', b'o', b's', b'e',
        b'c', b'o', b'n', b'd', // type "IntervalNanosecond"
        0xB1, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // i64 -79 LE
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn zero_row_intervals_encode_schema_without_bodies() {
    let fields = INTERVALS
        .iter()
        .map(|(name, kind)| Field {
            name: (*name).to_string(),
            ch_type: ChType::Interval(*kind),
        })
        .collect();
    let columns = INTERVALS
        .iter()
        .map(|_| Column::Interval(PrimitiveColumn::new(vec![])))
        .collect();
    let batch = ColBatch::new(Schema::new(fields), columns, 0);

    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema, batch.schema);
}
