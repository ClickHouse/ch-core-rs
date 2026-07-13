use super::*;

#[test]
fn test_decode_temporal_plain() {
    // Date is UInt16 days, Date32 is Int32 days (signed, can be pre-epoch),
    // DateTime is UInt32 seconds, DateTime64(3) is Int64 epoch ticks,
    // Time is signed Int32 seconds, and Time64(3) is signed Int64 time ticks.
    // Timezone and precision are type metadata only, never in the bytes.
    let data = BlockBuilder::new()
        .header(6, 4)
        .column_header("d", "Date")
        .date_data(&[0, 19737, 49710, 65535])
        .column_header("d32", "Date32")
        .int32_data(&[-7227, 0, 19737, 84370])
        .column_header("dt", "DateTime")
        .uint32_data(&[0, 1705322096, 961056000, 4294967295])
        .column_header("dt64", "DateTime64(3)")
        .int64_data(&[-877, 0, 1705322096789, 4102444799999])
        .column_header("t", "Time")
        .int32_data(&[-3_599_999, -13, 0, 3_599_999])
        .column_header("t64", "Time64(3)")
        .int64_data(&[-3_599_999_999, -13_000, 0, 3_599_999_999])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Date(c) => assert_eq!(c.values, vec![0u16, 19737, 49710, 65535]),
        other => panic!("expected Date, got {other:?}"),
    }
    match batch.column(1) {
        Column::Date32(c) => assert_eq!(c.values, vec![-7227i32, 0, 19737, 84370]),
        other => panic!("expected Date32, got {other:?}"),
    }
    match batch.column(2) {
        Column::DateTime(c) => {
            assert_eq!(c.values, vec![0u32, 1705322096, 961056000, 4294967295])
        }
        other => panic!("expected DateTime, got {other:?}"),
    }
    match batch.column(3) {
        Column::DateTime64(c) => {
            assert_eq!(c.values, vec![-877i64, 0, 1705322096789, 4102444799999])
        }
        other => panic!("expected DateTime64, got {other:?}"),
    }
    match batch.column(4) {
        Column::Time(c) => assert_eq!(c.values, vec![-3_599_999i32, -13, 0, 3_599_999]),
        other => panic!("expected Time, got {other:?}"),
    }
    match batch.column(5) {
        Column::Time64(c) => {
            assert_eq!(c.values, vec![-3_599_999_999i64, -13_000, 0, 3_599_999_999])
        }
        other => panic!("expected Time64, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_datetime64() {
    // Nullable(DateTime64(3)): null map then the Int64 ticks payload, with
    // null rows still carrying a placeholder value on the wire.
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("ts", "Nullable(DateTime64(3))")
        .null_map(&[false, true, false, true])
        .int64_data(&[-877, 0, 1705322096789, 0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::DateTime64(c) => {
            assert_eq!(c.null_count(), 2);
            assert_eq!(c.values, vec![-877i64, 0, 1705322096789, 0]);
        }
        other => panic!("expected DateTime64, got {other:?}"),
    }
    assert!(batch.column(0).validity().unwrap().is_valid(0));
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
}

#[test]
fn test_decode_nullable_time_types() {
    let data = BlockBuilder::new()
        .header(2, 4)
        .column_header("t", "Nullable(Time)")
        .null_map(&[false, true, false, true])
        .int32_data(&[-13, 0, 79, 0])
        .column_header("t64", "Nullable(Time64(6))")
        .null_map(&[false, true, false, true])
        .int64_data(&[-13_000_000, 0, 79_000_000, 0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Time(c) => {
            assert_eq!(c.values, vec![-13, 0, 79, 0]);
            assert_eq!(c.null_count(), 2);
        }
        other => panic!("expected nullable Time, got {other:?}"),
    }
    match batch.column(1) {
        Column::Time64(c) => {
            assert_eq!(c.values, vec![-13_000_000, 0, 79_000_000, 0]);
            assert_eq!(c.null_count(), 2);
        }
        other => panic!("expected nullable Time64, got {other:?}"),
    }
}

#[test]
fn test_decode_temporal_zero_rows() {
    // A zero-row block carrying temporal columns contributes the
    // schema but no chunks, and the empty columns have length 0.
    let data = BlockBuilder::new()
        .header(4, 0)
        .column_header("d", "Date")
        .column_header("dt", "DateTime")
        .column_header("t", "Time")
        .column_header("t64", "Time64(3)")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 4);
    assert_eq!(cb.schema.fields[0].ch_type, ChType::Date);
    assert_eq!(
        cb.schema.fields[1].ch_type,
        ChType::DateTime { timezone: None }
    );
    assert_eq!(cb.schema.fields[2].ch_type, ChType::Time);
    assert_eq!(cb.schema.fields[3].ch_type, ChType::Time64 { precision: 3 });
}
