use super::*;

fn count_type(arguments: Vec<ChType>) -> ChType {
    ChType::AggregateFunction {
        function: "count".into(),
        arguments,
    }
}

fn nothing_uint64_type() -> ChType {
    ChType::AggregateFunction {
        function: "nothingUInt64".into(),
        arguments: vec![ChType::Nullable(Box::new(ChType::Nothing))],
    }
}

const NOTHING_UINT64: &str = "AggregateFunction(nothingUInt64, Nullable(Nothing))";

#[test]
fn decode_count_states_preserves_exact_varuint_rows_and_next_column_boundary() {
    // States: 0, 13, 128, and u64::MAX. The last two exercise multi-byte row
    // boundaries; the trailing UInt8 column proves decode and block_end stop at
    // the same exact byte after the final aggregate state.
    let states = [
        0x00, 0x0d, 0x80, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
    ];
    let data = BlockBuilder::new()
        .header(2, 4)
        .column_header("c", "AggregateFunction(count)")
        .raw_bytes(&states)
        .column_header("u", "UInt8")
        .raw_bytes(&[13, 79, 5, 11])
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.schema.fields[0].ch_type, count_type(vec![]));
    match decoded.chunks[0].column(0) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0, 1, 2, 4, 14]);
            assert_eq!(c.data, states);
            assert_eq!(c.value(0), &[0x00]);
            assert_eq!(c.value(2), &[0x80, 0x01]);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }
    match decoded.chunks[0].column(1) {
        Column::UInt8(c) => assert_eq!(c.values, vec![13, 79, 5, 11]),
        other => panic!("expected trailing UInt8, got {other:?}"),
    }
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn decode_count_with_nullable_argument_uses_the_same_state_codec() {
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("c", "AggregateFunction(count, Nullable(String))")
        .raw_bytes(&[0x00, 0x0d, 0x4f])
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        decoded.schema.fields[0].ch_type,
        count_type(vec![ChType::Nullable(Box::new(ChType::String))])
    );
    match decoded.chunks[0].column(0) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0, 1, 2, 3]);
            assert_eq!(c.data, vec![0x00, 0x0d, 0x4f]);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }
}

#[test]
fn decode_array_count_states_composes_with_container_offsets() {
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("a", "Array(AggregateFunction(count))")
        .array_offsets(&[2, 2, 3])
        .raw_bytes(&[0x0d, 0x4f, 0x80, 0x01])
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match decoded.chunks[0].column(0) {
        Column::Array(c) => {
            assert_eq!(c.offsets, vec![0, 2, 2, 3]);
            match c.values.as_ref() {
                Column::AggregateState(states) => {
                    assert_eq!(states.offsets, vec![0, 1, 2, 4]);
                    assert_eq!(states.data, vec![0x0d, 0x4f, 0x80, 0x01]);
                }
                other => panic!("expected aggregate array values, got {other:?}"),
            }
        }
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn decode_count_zero_rows_keeps_schema_without_a_chunk() {
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("c", "AggregateFunction(count, UInt64)")
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(
        decoded.schema.fields[0].ch_type,
        count_type(vec![ChType::UInt64])
    );
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn decode_count_multi_block_keeps_state_buffers_separate() {
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("c", "AggregateFunction(count)")
        .raw_bytes(&[0x0d, 0x4f])
        .build();
    data.extend_from_slice(
        &BlockBuilder::new()
            .header(1, 1)
            .column_header("c", "AggregateFunction(count)")
            .raw_bytes(&[0x80, 0x01])
            .build(),
    );

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    match decoded.chunks[0].column(0) {
        Column::AggregateState(c) => assert_eq!(c.data, vec![0x0d, 0x4f]),
        other => panic!("expected AggregateState, got {other:?}"),
    }
    match decoded.chunks[1].column(0) {
        Column::AggregateState(c) => assert_eq!(c.data, vec![0x80, 0x01]),
        other => panic!("expected AggregateState, got {other:?}"),
    }
}

#[test]
fn decode_count_truncated_varuint_is_unexpected_eof() {
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("c", "AggregateFunction(count)")
        .raw_bytes(&[0x80])
        .build();

    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn decode_nothing_uint64_states_are_one_zero_byte_per_row() {
    // Four all-zero states, then a trailing UInt8 column so decode and block_end
    // must stop at the same byte after the fixed-width states.
    let data = BlockBuilder::new()
        .header(2, 4)
        .column_header("c", NOTHING_UINT64)
        .raw_bytes(&[0x00, 0x00, 0x00, 0x00])
        .column_header("u", "UInt8")
        .raw_bytes(&[13, 79, 5, 11])
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.schema.fields[0].ch_type, nothing_uint64_type());
    match decoded.chunks[0].column(0) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0, 1, 2, 3, 4]);
            assert_eq!(c.data, vec![0x00, 0x00, 0x00, 0x00]);
            assert_eq!(c.value(0), &[0x00]);
            assert_eq!(c.value(3), &[0x00]);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }
    match decoded.chunks[0].column(1) {
        Column::UInt8(c) => assert_eq!(c.values, vec![13, 79, 5, 11]),
        other => panic!("expected trailing UInt8, got {other:?}"),
    }
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn decode_nothing_uint64_zero_rows_keeps_schema_without_a_chunk() {
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("c", NOTHING_UINT64)
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema.fields[0].ch_type, nothing_uint64_type());
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn decode_nothing_uint64_multi_block_keeps_state_buffers_separate() {
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("c", NOTHING_UINT64)
        .raw_bytes(&[0x00, 0x00])
        .build();
    data.extend_from_slice(
        &BlockBuilder::new()
            .header(1, 1)
            .column_header("c", NOTHING_UINT64)
            .raw_bytes(&[0x00])
            .build(),
    );

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    match decoded.chunks[0].column(0) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0, 1, 2]);
            assert_eq!(c.data, vec![0x00, 0x00]);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }
    match decoded.chunks[1].column(0) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0, 1]);
            assert_eq!(c.data, vec![0x00]);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }
}

#[test]
fn decode_nothing_uint64_nonzero_state_byte_is_rejected_not_panicked() {
    // The server throws INCORRECT_DATA on a nonzero placeholder; decode must
    // return a clean error, never panic, on this untrusted byte.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("c", NOTHING_UINT64)
        .raw_bytes(&[0x00, 0x01])
        .build();

    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::InvalidData
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::InvalidData
    ));
}

#[test]
fn decode_nothing_uint64_truncated_body_is_unexpected_eof() {
    // Only two of the three declared row bytes are present; a short buffer is
    // "need more bytes", not corruption.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("c", NOTHING_UINT64)
        .raw_bytes(&[0x00, 0x00])
        .build();

    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn decode_sum_state_widths_preserve_bytes_and_next_column_boundary() {
    // The full accepted argument set, grouped by the server-selected
    // accumulator width. Two arbitrary states plus a trailing UInt8 column prove
    // materialization and block_end stop at the same fixed boundary.
    for (case, (argument, width)) in [
        ("Bool", 8),
        ("UInt8", 8),
        ("UInt16", 8),
        ("UInt32", 8),
        ("UInt64", 8),
        ("Int8", 8),
        ("Int16", 8),
        ("Int32", 8),
        ("Int64", 8),
        ("UInt128", 16),
        ("Int128", 16),
        ("UInt256", 32),
        ("Int256", 32),
        ("BFloat16", 8),
        ("Float32", 8),
        ("Float64", 8),
        ("Decimal(9, 4)", 16),
        ("Decimal(18, 4)", 16),
        ("Decimal(38, 4)", 16),
        ("Decimal(76, 4)", 32),
        ("Enum8('debit' = -3, 'credit' = 7)", 8),
        ("Enum16('debit' = -300, 'credit' = 700)", 8),
    ]
    .into_iter()
    .enumerate()
    {
        let type_name = format!("AggregateFunction(sum, {argument})");
        // Base sum's deserializer accepts any complete accumulator bit pattern,
        // so distinct nonzero bytes make exact passthrough easy to assert.
        let states = vec![(case + 1) as u8; width * 2];
        let data = BlockBuilder::new()
            .header(2, 2)
            .column_header("s", &type_name)
            .raw_bytes(&states)
            .column_header("u", "UInt8")
            .raw_bytes(&[13, 79])
            .build();

        let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
        match decoded.chunks[0].column(0) {
            Column::AggregateState(c) => {
                assert_eq!(c.offsets, vec![0, width as i64, (width * 2) as i64]);
                assert_eq!(c.data, states, "state bytes for {type_name}");
            }
            other => panic!("expected AggregateState for {type_name}, got {other:?}"),
        }
        match decoded.chunks[0].column(1) {
            Column::UInt8(c) => assert_eq!(c.values, vec![13, 79]),
            other => panic!("expected trailing UInt8 for {type_name}, got {other:?}"),
        }
        assert_eq!(
            block_end(&data, &DecodeOptions::default()).unwrap(),
            Some(data.len()),
            "block end for {type_name}"
        );
    }
}

#[test]
fn decode_nullable_sum_state_widths_preserve_flags_bytes_and_next_column_boundary() {
    // The Null adapter is the same around every accepted nested sum promotion.
    // For each signature, row 0 has no nested accumulator and row 1 uses a
    // noncanonical true flag. The server's bool reader accepts any nonzero flag,
    // and the trailing UInt8 proves both decode and block_end honor the
    // conditional row boundary.
    for (case, (argument, width)) in [
        ("Bool", 8),
        ("UInt8", 8),
        ("UInt16", 8),
        ("UInt32", 8),
        ("UInt64", 8),
        ("Int8", 8),
        ("Int16", 8),
        ("Int32", 8),
        ("Int64", 8),
        ("UInt128", 16),
        ("Int128", 16),
        ("UInt256", 32),
        ("Int256", 32),
        ("BFloat16", 8),
        ("Float32", 8),
        ("Float64", 8),
        ("Decimal(9, 4)", 16),
        ("Decimal(18, 4)", 16),
        ("Decimal(38, 4)", 16),
        ("Decimal(76, 4)", 32),
        ("Enum8('debit' = -3, 'credit' = 7)", 8),
        ("Enum16('debit' = -300, 'credit' = 700)", 8),
    ]
    .into_iter()
    .enumerate()
    {
        let type_name = format!("AggregateFunction(sum, Nullable({argument}))");
        let mut states = vec![0x00, 0x02];
        states.extend(std::iter::repeat_n((case + 1) as u8, width));
        let data = BlockBuilder::new()
            .header(2, 2)
            .column_header("s", &type_name)
            .raw_bytes(&states)
            .column_header("u", "UInt8")
            .raw_bytes(&[13, 79])
            .build();

        let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
        match decoded.chunks[0].column(0) {
            Column::AggregateState(c) => {
                assert_eq!(c.offsets, vec![0, 1, (width + 2) as i64]);
                assert_eq!(c.data, states, "state bytes for {type_name}");
            }
            other => panic!("expected AggregateState for {type_name}, got {other:?}"),
        }
        match decoded.chunks[0].column(1) {
            Column::UInt8(c) => assert_eq!(c.values, vec![13, 79]),
            other => panic!("expected trailing UInt8 for {type_name}, got {other:?}"),
        }
        assert_eq!(
            block_end(&data, &DecodeOptions::default()).unwrap(),
            Some(data.len()),
            "block end for {type_name}"
        );
    }
}

#[test]
fn decode_array_sum_states_composes_with_container_offsets() {
    let mut states = Vec::new();
    for value in [13i64, -79, 258] {
        states.extend_from_slice(&value.to_le_bytes());
    }
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("a", "Array(AggregateFunction(sum, Int32))")
        .array_offsets(&[2, 2, 3])
        .raw_bytes(&states)
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match decoded.chunks[0].column(0) {
        Column::Array(c) => {
            assert_eq!(c.offsets, vec![0, 2, 2, 3]);
            match c.values.as_ref() {
                Column::AggregateState(s) => {
                    assert_eq!(s.offsets, vec![0, 8, 16, 24]);
                    assert_eq!(s.data, states);
                }
                other => panic!("expected aggregate array values, got {other:?}"),
            }
        }
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn decode_array_nullable_sum_states_composes_with_container_offsets() {
    let mut states = vec![0x00, 0x01];
    states.extend_from_slice(&13i64.to_le_bytes());
    states.push(0x00);
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("a", "Array(AggregateFunction(sum, Nullable(Int32)))")
        .array_offsets(&[2, 2, 3])
        .raw_bytes(&states)
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match decoded.chunks[0].column(0) {
        Column::Array(c) => {
            assert_eq!(c.offsets, vec![0, 2, 2, 3]);
            match c.values.as_ref() {
                Column::AggregateState(s) => {
                    assert_eq!(s.offsets, vec![0, 1, 10, 11]);
                    assert_eq!(s.data, states);
                }
                other => panic!("expected aggregate array values, got {other:?}"),
            }
        }
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn decode_sum_zero_rows_keeps_schema_without_a_chunk() {
    for type_name in [
        "AggregateFunction(sum, Decimal(9, 2))",
        "AggregateFunction(sum, Nullable(Decimal(9, 2)))",
    ] {
        let data = BlockBuilder::new()
            .header(1, 0)
            .column_header("s", type_name)
            .build();

        let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
        assert_eq!(decoded.num_chunks(), 0);
        assert_eq!(
            decoded.schema.fields[0].ch_type,
            parse_ch_type(type_name).unwrap()
        );
        assert_eq!(
            block_end(&data, &DecodeOptions::default()).unwrap(),
            Some(data.len())
        );
    }
}

#[test]
fn decode_nullable_sum_multi_block_keeps_state_buffers_separate() {
    let mut first_states = vec![0x00, 0x01];
    first_states.extend_from_slice(&13i128.to_le_bytes());
    let mut second_states = vec![0x01];
    second_states.extend_from_slice(&79i128.to_le_bytes());

    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("s", "AggregateFunction(sum, Nullable(UInt128))")
        .raw_bytes(&first_states)
        .build();
    data.extend_from_slice(
        &BlockBuilder::new()
            .header(1, 1)
            .column_header("s", "AggregateFunction(sum, Nullable(UInt128))")
            .raw_bytes(&second_states)
            .build(),
    );

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    match decoded.chunks[0].column(0) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0, 1, 18]);
            assert_eq!(c.data, first_states);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }
    match decoded.chunks[1].column(0) {
        Column::AggregateState(c) => {
            assert_eq!(c.offsets, vec![0, 17]);
            assert_eq!(c.data, second_states);
        }
        other => panic!("expected AggregateState, got {other:?}"),
    }
}

#[test]
fn decode_sum_multi_block_keeps_state_buffers_separate() {
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("s", "AggregateFunction(sum, UInt128)")
        .raw_bytes(&[0x0d; 32])
        .build();
    data.extend_from_slice(
        &BlockBuilder::new()
            .header(1, 1)
            .column_header("s", "AggregateFunction(sum, UInt128)")
            .raw_bytes(&[0x4f; 16])
            .build(),
    );

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    match decoded.chunks[0].column(0) {
        Column::AggregateState(c) => assert_eq!(c.data, vec![0x0d; 32]),
        other => panic!("expected AggregateState, got {other:?}"),
    }
    match decoded.chunks[1].column(0) {
        Column::AggregateState(c) => assert_eq!(c.data, vec![0x4f; 16]),
        other => panic!("expected AggregateState, got {other:?}"),
    }
}

#[test]
fn decode_sum_truncated_fixed_width_state_is_unexpected_eof() {
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("s", "AggregateFunction(sum, UInt256)")
        .raw_bytes(&[0x0d; 63])
        .build();

    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn decode_nullable_sum_missing_flag_or_accumulator_is_unexpected_eof() {
    for states in [&[][..], &[0x01][..], &[0x01, 0x0d, 0x00][..]] {
        let data = BlockBuilder::new()
            .header(1, 1)
            .column_header("s", "AggregateFunction(sum, Nullable(UInt64))")
            .raw_bytes(states)
            .build();

        assert!(matches!(
            decode_all_bytes(&data, &DecodeOptions::default()),
            Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert!(matches!(
            block_end(&data, &DecodeOptions::default()),
            Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
        ));
    }
}

#[test]
fn decode_sum_inflated_row_count_with_truncated_payload_is_unexpected_eof() {
    // A hostile header declares 60 rows of an 8-byte `sum` accumulator but
    // carries only 80 payload bytes (ten states). The block is padded enough for
    // 60 to slip past `check_header_count` (60 <= the bytes remaining at the
    // header), so the width guard in `decode_aggregate_states` is what keeps the
    // i64 offsets reservation from ballooning to 8x the count. Either way the
    // decode must fail cleanly as "need more bytes", never panic or over-reserve.
    let data = BlockBuilder::new()
        .header(1, 60)
        .column_header("s", "AggregateFunction(sum, UInt64)")
        .raw_bytes(&[0x0d; 80])
        .build();

    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn unsupported_aggregate_layouts_and_wrappers_are_rejected_at_zero_rows() {
    for type_name in [
        "AggregateFunction(avg, UInt64)",
        "AggregateFunction(sum)",
        "AggregateFunction(sum, Nullable(Nothing))",
        "AggregateFunction(sum, Nullable(String))",
        "AggregateFunction(sum, String)",
        "AggregateFunction(sum, UInt8, UInt16)",
        "AggregateFunction(count, UInt8, UInt16)",
        "AggregateFunction(1, count)",
        // count(Nullable(Nothing)) canonicalizes to nothingUInt64 on the wire, so
        // the count spelling never appears and must be rejected here.
        "AggregateFunction(count, Nullable(Nothing))",
        // nothingUInt64 is confirmed only with the Nullable(Nothing) argument.
        "AggregateFunction(nothingUInt64, UInt64)",
        "AggregateFunction(nothingUInt64)",
        "Nullable(AggregateFunction(count))",
        "LowCardinality(AggregateFunction(count))",
    ] {
        let data = BlockBuilder::new()
            .header(1, 0)
            .column_header("c", type_name)
            .build();
        assert!(matches!(
            decode_all_bytes(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
    }
}

#[test]
fn count_rejects_server_invalid_argument_types_with_and_without_rows() {
    for type_name in [
        "AggregateFunction(count, LowCardinality(Decimal(9, 4)))",
        "AggregateFunction(count, Map(Nullable(UInt8), UInt8))",
        "AggregateFunction(count, Tuple(a UInt8, a UInt8))",
    ] {
        for num_rows in [0, 1] {
            let mut builder = BlockBuilder::new()
                .header(1, num_rows)
                .column_header("c", type_name);
            if num_rows != 0 {
                builder = builder.raw_bytes(&[0x0d]);
            }
            let data = builder.build();
            assert!(matches!(
                decode_all_bytes(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ));
            assert!(matches!(
                block_end(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ));
        }
    }
}
