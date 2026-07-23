use super::*;

const INTERVALS: [(&str, IntervalKind); 11] = [
    ("IntervalYear", IntervalKind::Year),
    ("IntervalQuarter", IntervalKind::Quarter),
    ("IntervalMonth", IntervalKind::Month),
    ("IntervalWeek", IntervalKind::Week),
    ("IntervalDay", IntervalKind::Day),
    ("IntervalHour", IntervalKind::Hour),
    ("IntervalMinute", IntervalKind::Minute),
    ("IntervalSecond", IntervalKind::Second),
    ("IntervalMillisecond", IntervalKind::Millisecond),
    ("IntervalMicrosecond", IntervalKind::Microsecond),
    ("IntervalNanosecond", IntervalKind::Nanosecond),
];

#[test]
fn test_decode_all_interval_kinds_plain() {
    let values = [i64::MIN, -79, 0, i64::MAX];
    let data = BlockBuilder::new()
        .header(INTERVALS.len(), values.len())
        .column_header("iy", "IntervalYear")
        .int64_data(&values)
        .column_header("iq", "IntervalQuarter")
        .int64_data(&values)
        .column_header("imo", "IntervalMonth")
        .int64_data(&values)
        .column_header("iw", "IntervalWeek")
        .int64_data(&values)
        .column_header("id", "IntervalDay")
        .int64_data(&values)
        .column_header("ih", "IntervalHour")
        .int64_data(&values)
        .column_header("imi", "IntervalMinute")
        .int64_data(&values)
        .column_header("is", "IntervalSecond")
        .int64_data(&values)
        .column_header("ims", "IntervalMillisecond")
        .int64_data(&values)
        .column_header("ius", "IntervalMicrosecond")
        .int64_data(&values)
        .column_header("ins", "IntervalNanosecond")
        .int64_data(&values)
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &decoded.chunks[0];
    for (index, (_, kind)) in INTERVALS.iter().enumerate() {
        assert_eq!(batch.schema.fields[index].ch_type, ChType::Interval(*kind));
        match batch.column(index) {
            Column::Interval(c) => assert_eq!(c.values, values),
            other => panic!("expected Interval at column {index}, got {other:?}"),
        }
    }
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_decode_nullable_interval() {
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("ni", "Nullable(IntervalDay)")
        .null_map(&[false, true, false, true])
        .int64_data(&[-13, 0, 79, 0])
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        decoded.schema.fields[0].ch_type,
        ChType::Nullable(Box::new(ChType::Interval(IntervalKind::Day)))
    );
    match decoded.chunks[0].column(0) {
        Column::Interval(c) => {
            assert_eq!(c.values, vec![-13, 0, 79, 0]);
            assert_eq!(c.null_count(), 2);
        }
        other => panic!("expected nullable Interval, got {other:?}"),
    }
    let validity = decoded.chunks[0].column(0).validity().unwrap();
    let actual: Vec<bool> = (0..4).map(|row| validity.is_valid(row)).collect();
    assert_eq!(actual, vec![true, false, true, false]);
}

#[test]
fn test_decode_low_cardinality_interval() {
    let mut dictionary = Vec::new();
    for value in [0i64, 13, 79, 258] {
        dictionary.extend_from_slice(&value.to_le_bytes());
    }
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("lc", "LowCardinality(IntervalHour)")
        .low_cardinality_block(4, &dictionary, &[1, 2, 1, 3], 1)
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        decoded.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::Interval(IntervalKind::Hour)))
    );
    match decoded.chunks[0].column(0) {
        Column::Dictionary(c) => {
            assert_eq!(c.indices, vec![1, 2, 1, 3]);
            match c.values.as_ref() {
                Column::Interval(values) => assert_eq!(values.values, vec![0, 13, 79, 258]),
                other => panic!("expected Interval dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Interval dictionary, got {other:?}"),
    }
}

#[test]
fn test_decode_interval_zero_rows() {
    let data = BlockBuilder::new()
        .header(INTERVALS.len(), 0)
        .column_header("iy", "IntervalYear")
        .column_header("iq", "IntervalQuarter")
        .column_header("imo", "IntervalMonth")
        .column_header("iw", "IntervalWeek")
        .column_header("id", "IntervalDay")
        .column_header("ih", "IntervalHour")
        .column_header("imi", "IntervalMinute")
        .column_header("is", "IntervalSecond")
        .column_header("ims", "IntervalMillisecond")
        .column_header("ius", "IntervalMicrosecond")
        .column_header("ins", "IntervalNanosecond")
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.num_columns(), INTERVALS.len());
    for (field, (_, kind)) in decoded.schema.fields.iter().zip(INTERVALS) {
        assert_eq!(field.ch_type, ChType::Interval(kind));
    }
}

#[test]
fn test_decode_interval_multi_block_keeps_chunks_separate() {
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("i", "IntervalMonth")
        .int64_data(&[-13, 0])
        .build();
    data.extend_from_slice(
        &BlockBuilder::new()
            .header(1, 2)
            .column_header("i", "IntervalMonth")
            .int64_data(&[79, 258])
            .build(),
    );

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    for (chunk, expected) in decoded.chunks.iter().zip([[-13, 0], [79, 258]]) {
        match chunk.column(0) {
            Column::Interval(c) => assert_eq!(c.values, expected),
            other => panic!("expected Interval, got {other:?}"),
        }
    }
}
