use super::*;
use crate::native::varint::write_varint;

#[test]
fn test_block_end_scans_string_column() {
    // The completeness scan must return the exact end offset of a block whose
    // String column it walks via the per-value length prefixes, and report a
    // one-byte-short buffer as "need more bytes" (UnexpectedEof).
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("s", "String")
        .string_data(&["user_1", "", "user_2"])
        .build();

    let end = block_end(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(end, Some(data.len()));

    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_block_end_zero_rows() {
    // A zero-row block is complete once its headers are buffered.
    let data = BlockBuilder::new()
        .header(2, 0)
        .column_header("a", "Int32")
        .column_header("b", "String")
        .build();
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_block_end_rejects_unsupported_type() {
    // An unsupported type inside an otherwise-complete block must surface as
    // a DecodeError from the scan, not be silently skipped or reported as
    // incomplete. `Dynamic` is not decoded yet, so it serves as the example.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("id", "Dynamic")
        .build();
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_block_end_no_block_at_clean_boundary() {
    // No-framing stream: an empty buffer is a clean boundary, not a block.
    assert_eq!(block_end(&[], &DecodeOptions::default()).unwrap(), None);
}

#[test]
fn test_unsupported_type() {
    // `Dynamic` is not decoded yet, so it serves as the unsupported example.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("id", "Dynamic")
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_multi_block_kept_as_chunks() {
    // Two Int32 blocks must be kept as two separate chunks, NOT merged.
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("n", "Int32")
        .int32_data(&[1, 2])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 3)
            .column_header("n", "Int32")
            .int32_data(&[3, 4, 5])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
    match cb.chunks[0].column(0) {
        Column::Int32(c) => assert_eq!(c.values, vec![1, 2]),
        _ => panic!(),
    }
    match cb.chunks[1].column(0) {
        Column::Int32(c) => assert_eq!(c.values, vec![3, 4, 5]),
        _ => panic!(),
    }
}

#[test]
fn test_block_schema_mismatch_rejected() {
    // Every block of a result shares one schema. A later block with a
    // different column count, type, or name is a corrupt payload.
    let first = BlockBuilder::new()
        .header(2, 1)
        .column_header("a", "Int32")
        .int32_data(&[13])
        .column_header("b", "Int32")
        .int32_data(&[79])
        .build();

    let fewer_columns = BlockBuilder::new()
        .header(1, 1)
        .column_header("a", "Int32")
        .int32_data(&[5])
        .build();
    let different_type = BlockBuilder::new()
        .header(2, 1)
        .column_header("a", "Int32")
        .int32_data(&[5])
        .column_header("b", "String")
        .string_data(&["u1"])
        .build();
    let different_name = BlockBuilder::new()
        .header(2, 1)
        .column_header("a", "Int32")
        .int32_data(&[5])
        .column_header("c", "Int32")
        .int32_data(&[7])
        .build();

    for second in [fewer_columns, different_type, different_name] {
        let mut data = first.clone();
        data.extend(second);
        match decode_all_bytes(&data, &DecodeOptions::default()) {
            Err(DecodeError::BlockSchemaMismatch { block_index }) => {
                assert_eq!(block_index, 1)
            }
            other => panic!("expected BlockSchemaMismatch, got {other:?}"),
        }
    }
}

#[test]
fn test_zero_row_trailer_with_matching_schema_accepted() {
    // The server re-emits the column headers in a zero-row trailer block;
    // a matching trailer must decode cleanly and contribute no chunk.
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("n", "Int32")
        .int32_data(&[1, 2])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 0)
            .column_header("n", "Int32")
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 1);
    assert_eq!(cb.num_rows(), 2);
}

#[test]
fn test_multi_block_bool_kept_as_chunks() {
    // Bool blocks stay separate — no O(n^2) bitmap re-packing.
    let mut data = BlockBuilder::new()
        .header(1, 3)
        .column_header("b", "Bool")
        .raw_bytes(&[1, 0, 1])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 2)
            .column_header("b", "Bool")
            .raw_bytes(&[0, 1])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
    match cb.chunks[0].column(0) {
        Column::Bool(c) => {
            assert!(c.get(0));
            assert!(!c.get(1));
            assert!(c.get(2));
        }
        _ => panic!(),
    }
    match cb.chunks[1].column(0) {
        Column::Bool(c) => {
            assert!(!c.get(0));
            assert!(c.get(1));
        }
        _ => panic!(),
    }
}

#[test]
fn test_empty_block() {
    // A zero-row block contributes the schema but no chunks.
    let data = BlockBuilder::new()
        .header(2, 0)
        .column_header("a", "Int32")
        .column_header("b", "String")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 2);
}

#[test]
fn test_modern_framing_roundtrip() {
    // Full v26.6.1.1193 framing: a BlockInfo preamble plus a per-column
    // custom-serialization byte (0 = default) ahead of the data.
    let data = BlockBuilder::new()
        .revision(DBMS_TCP_PROTOCOL_VERSION)
        .header(1, 2)
        .column_header("v", "Int64")
        .int64_data(&[77, 88])
        .build();

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    let cb = decode_all_bytes(&data, &options).unwrap();
    assert_eq!(cb.num_rows(), 2);
    match cb.chunks[0].column(0) {
        Column::Int64(c) => assert_eq!(c.values, vec![77, 88]),
        _ => panic!("expected Int64"),
    }
}

#[test]
fn test_two_field_block_info_parses() {
    // A revision between the custom-serialization (54454) and
    // out-of-order-buckets (54480) cutoffs: the BlockInfo has only fields 1
    // and 2 (8 bytes), and the custom-serialization byte is present. The
    // self-describing parser handles the shorter preamble.
    let revision = 54460;
    let data = BlockBuilder::new()
        .revision(revision)
        .header(1, 1)
        .column_header("n", "Int32")
        .int32_data(&[91])
        .build();

    let options = DecodeOptions {
        protocol_revision: revision,
    };
    let cb = decode_all_bytes(&data, &options).unwrap();
    match cb.chunks[0].column(0) {
        Column::Int32(c) => assert_eq!(c.values, vec![91]),
        _ => panic!("expected Int32"),
    }
}

#[test]
fn test_multi_block_modern_framing() {
    // Each block carries its own BlockInfo preamble at revision > 0, and the
    // boundary between them is found by parsing, not a fixed skip.
    let mut data = BlockBuilder::new()
        .revision(DBMS_TCP_PROTOCOL_VERSION)
        .header(1, 2)
        .column_header("n", "Int32")
        .int32_data(&[1, 2])
        .build();
    data.extend(
        BlockBuilder::new()
            .revision(DBMS_TCP_PROTOCOL_VERSION)
            .header(1, 3)
            .column_header("n", "Int32")
            .int32_data(&[3, 4, 5])
            .build(),
    );

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    let cb = decode_all_bytes(&data, &options).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
}

#[test]
fn test_zero_row_modern_block() {
    // A zero-row block still carries the per-column custom-serialization
    // byte, which must be consumed even though no data follows.
    let data = BlockBuilder::new()
        .revision(DBMS_TCP_PROTOCOL_VERSION)
        .header(2, 0)
        .column_header("a", "Int32")
        .column_header("b", "String")
        .build();

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    let cb = decode_all_bytes(&data, &options).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 2);
}

#[test]
fn test_custom_serialization_rejected() {
    // A nonzero custom-serialization marker selects a layout this crate does
    // not decode. Reject it rather than misread the column.
    let data = BlockBuilder::new()
        .revision(DBMS_TCP_PROTOCOL_VERSION)
        .header(1, 1)
        .column_header_with_custom("c", "Int32", 0x01, &[0x01]) // 0x01 = SPARSE kind stack
        .build();

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    assert!(matches!(
        decode_all_bytes(&data, &options),
        Err(DecodeError::UnsupportedSerialization {
            serialization_byte: 1,
            ..
        })
    ));
}

#[test]
fn test_unknown_block_info_field_rejected() {
    // An unknown BlockInfo field number is rejected, matching the server.
    let mut data = Vec::new();
    write_varint(&mut data, 7); // unknown field number

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    assert!(matches!(
        decode_all_bytes(&data, &options),
        Err(DecodeError::InvalidBlockInfo { field_num: 7 })
    ));
}

// A row/column count that big would make `Vec::with_capacity` abort the
// process. The hardened decoder must return an error instead of panicking.
// `1 << 61` also overflows the primitive byte-length multiply (`* 8`), so
// these cover both the count guard and the `checked_mul` path.
const HOSTILE_COUNT: usize = 1 << 61;

#[test]
fn test_oversized_row_count_primitive_rejected() {
    let data = BlockBuilder::new()
        .header(1, HOSTILE_COUNT)
        .column_header("v", "Int64")
        .build();
    match decode_all_bytes(&data, &DecodeOptions::default()) {
        Err(DecodeError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

#[test]
fn test_oversized_row_count_string_rejected() {
    let data = BlockBuilder::new()
        .header(1, HOSTILE_COUNT)
        .column_header("s", "String")
        .build();
    match decode_all_bytes(&data, &DecodeOptions::default()) {
        Err(DecodeError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

#[test]
fn test_oversized_column_count_rejected() {
    let data = BlockBuilder::new()
        .header(HOSTILE_COUNT, 1)
        .column_header("n", "Int8")
        .raw_bytes(&[13])
        .build();
    match decode_all_bytes(&data, &DecodeOptions::default()) {
        Err(DecodeError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

#[test]
fn test_inflated_column_count_with_truncated_headers_rejected() {
    // A count that slips past `check_header_count` (num_cols <= remaining) but is
    // still inflated far beyond the headers actually present: 30 declared columns
    // behind only six real Int8 headers. The per-column reservation is capped at
    // what the remaining bytes could frame, and the header-read loop then runs
    // out on the seventh column and reports "need more bytes" cleanly.
    let mut builder = BlockBuilder::new().header(30, 0);
    for name in ["a", "b", "c", "d", "e", "f"] {
        builder = builder.column_header(name, "Int8");
    }
    let data = builder.build();

    match decode_all_bytes(&data, &DecodeOptions::default()) {
        Err(DecodeError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

#[test]
fn test_multi_block_date_kept_as_chunks() {
    // Date blocks stay separate chunks, never concatenated.
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("d", "Date")
        .date_data(&[0, 19737])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 3)
            .column_header("d", "Date")
            .date_data(&[49710, 65535, 13])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
    match cb.chunks[0].column(0) {
        Column::Date(c) => assert_eq!(c.values, vec![0u16, 19737]),
        other => panic!("expected Date, got {other:?}"),
    }
    match cb.chunks[1].column(0) {
        Column::Date(c) => assert_eq!(c.values, vec![49710u16, 65535, 13]),
        other => panic!("expected Date, got {other:?}"),
    }
}

#[test]
fn test_multi_block_time_types_kept_as_chunks() {
    let mut data = BlockBuilder::new()
        .header(2, 2)
        .column_header("t", "Time")
        .int32_data(&[-13, 0])
        .column_header("t64", "Time64(3)")
        .int64_data(&[-13_000, 0])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(2, 2)
            .column_header("t", "Time")
            .int32_data(&[79, 3_599_999])
            .column_header("t64", "Time64(3)")
            .int64_data(&[79_000, 3_599_999_999])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    match cb.chunks[0].column(0) {
        Column::Time(c) => assert_eq!(c.values, vec![-13, 0]),
        other => panic!("expected Time, got {other:?}"),
    }
    match cb.chunks[1].column(1) {
        Column::Time64(c) => assert_eq!(c.values, vec![79_000, 3_599_999_999]),
        other => panic!("expected Time64, got {other:?}"),
    }
}

#[test]
fn test_block_end_scans_time_types() {
    let data = BlockBuilder::new()
        .header(2, 2)
        .column_header("t", "Time")
        .int32_data(&[-13, 79])
        .column_header("t64", "Time64(9)")
        .int64_data(&[-13_000_000_000, 79_000_000_000])
        .build();
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    assert!(matches!(
        block_end(truncated, &DecodeOptions::default()),
        Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_block_info_out_of_order_buckets_skipped() {
    // A BlockInfo carrying a nonzero out_of_order_buckets vector (field 3,
    // present at server revision >= 54480): a varint count then that many
    // Int32 values, confirmed against BlockInfo::write at v26.6.1.1193-stable.
    // The committed fixtures only ever exercise the empty-vector case, so
    // assemble a nonzero one by hand and confirm the decoder skips the whole
    // vector and lands exactly on the block body.
    let mut data = Vec::new();
    write_varint(&mut data, 1); // field 1: is_overflows
    data.push(0x00);
    write_varint(&mut data, 2); // field 2: bucket_num
    data.extend_from_slice(&(-1i32).to_le_bytes());
    write_varint(&mut data, 3); // field 3: out_of_order_buckets
    write_varint(&mut data, 2); // count = 2
    data.extend_from_slice(&7i32.to_le_bytes());
    data.extend_from_slice(&9i32.to_le_bytes());
    write_varint(&mut data, 0); // terminator
                                // Block body: one Int32 column, one row.
    write_varint(&mut data, 1); // num_cols
    write_varint(&mut data, 1); // num_rows
    write_varint(&mut data, 1); // name length
    data.extend_from_slice(b"n");
    write_varint(&mut data, 5); // type length
    data.extend_from_slice(b"Int32");
    data.push(0x00); // default serialization (revision >= 54454)
    data.extend_from_slice(&13i32.to_le_bytes());

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    let cb = decode_all_bytes(&data, &options).unwrap();
    match cb.chunks[0].column(0) {
        Column::Int32(c) => assert_eq!(c.values, vec![13]),
        other => panic!("expected Int32, got {other:?}"),
    }
}

#[test]
fn test_multi_block_low_cardinality_separate_dictionaries() {
    // Each Native block carries its own per-block dictionary. The two blocks
    // here use DIFFERENT dictionaries and different index widths, and stay
    // separate chunks. A consumer must resolve each chunk against its own
    // dictionary, never a shared one.
    let mut data = BlockBuilder::new()
        .header(1, 3)
        .column_header("lc", "LowCardinality(String)")
        .low_cardinality_string(&["red", "green"], &[0, 1, 0], 1)
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 2)
            .column_header("lc", "LowCardinality(String)")
            .low_cardinality_string(&["blue", "amber"], &[1, 0], 2)
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);

    let chunk0 = &cb.chunks[0];
    assert_eq!(
        lc_value(chunk0.column(0), 0).as_deref(),
        Some(b"red" as &[u8])
    );
    assert_eq!(
        lc_value(chunk0.column(0), 1).as_deref(),
        Some(b"green" as &[u8])
    );
    assert_eq!(
        lc_value(chunk0.column(0), 2).as_deref(),
        Some(b"red" as &[u8])
    );

    let chunk1 = &cb.chunks[1];
    assert_eq!(
        lc_value(chunk1.column(0), 0).as_deref(),
        Some(b"amber" as &[u8])
    );
    assert_eq!(
        lc_value(chunk1.column(0), 1).as_deref(),
        Some(b"blue" as &[u8])
    );
}

#[test]
fn test_block_end_scans_low_cardinality() {
    // The completeness scan must walk a LowCardinality column to the exact
    // block end, and report a one-byte-short buffer as "need more bytes".
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("lc", "LowCardinality(String)")
        .low_cardinality_string(&["user_1", "user_2"], &[0, 1, 0], 1)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_multi_block_low_cardinality_uint32_separate_dictionaries() {
    // Each Native block carries its own per-block UInt32 dictionary and may
    // use a different index width; the blocks stay separate chunks and each
    // resolves against its own dictionary.
    let mut data = BlockBuilder::new()
        .header(1, 3)
        .column_header("lc", "LowCardinality(UInt32)")
        .low_cardinality_u32(&[0, 13, 79], &[1, 2, 1], 1)
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 2)
            .column_header("lc", "LowCardinality(UInt32)")
            .low_cardinality_u32(&[0, 4_294_967_295], &[1, 1], 2)
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
    assert_eq!(lc_u32_value(cb.chunks[0].column(0), 0), Some(13));
    assert_eq!(lc_u32_value(cb.chunks[0].column(0), 1), Some(79));
    assert_eq!(lc_u32_value(cb.chunks[0].column(0), 2), Some(13));
    assert_eq!(lc_u32_value(cb.chunks[1].column(0), 0), Some(4_294_967_295));
    assert_eq!(lc_u32_value(cb.chunks[1].column(0), 1), Some(4_294_967_295));
}

#[test]
fn test_block_end_scans_numeric_low_cardinality() {
    // The completeness scan must walk a numeric LowCardinality column (raw
    // fixed-width dictionary body, no varint prefixes) to the exact block
    // end, and report a one-byte-short buffer as "need more bytes".
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("lc", "LowCardinality(UInt32)")
        .low_cardinality_u32(&[0, 13, 79], &[1, 2, 1], 1)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_multi_block_uuid_kept_as_chunks() {
    // UUID blocks stay separate chunks, never concatenated.
    let a = [0x01u8; 16];
    let b = [0x02u8; 16];
    let c = [0x03u8; 16];
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("u", "UUID")
        .fixed16_data(&[a, b])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("u", "UUID")
            .fixed16_data(&[c])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);
    match cb.chunks[0].column(0) {
        Column::Uuid(col) => {
            assert_eq!(col.value(0), a);
            assert_eq!(col.value(1), b);
        }
        other => panic!("expected Uuid, got {other:?}"),
    }
    match cb.chunks[1].column(0) {
        Column::Uuid(col) => assert_eq!(col.value(0), c),
        other => panic!("expected Uuid, got {other:?}"),
    }
}

#[test]
fn test_multi_block_ipv4_kept_as_chunks() {
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("ip", "IPv4")
        .ipv4_data(&[0, 3221226219])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 3)
            .column_header("ip", "IPv4")
            .ipv4_data(&[169090600, u32::MAX, 13])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
    match cb.chunks[0].column(0) {
        Column::Ipv4(c) => assert_eq!(c.values, vec![0, 3221226219]),
        other => panic!("expected Ipv4, got {other:?}"),
    }
    match cb.chunks[1].column(0) {
        Column::Ipv4(c) => assert_eq!(c.values, vec![169090600, u32::MAX, 13]),
        other => panic!("expected Ipv4, got {other:?}"),
    }
}

#[test]
fn test_multi_block_ipv6_kept_as_chunks() {
    let a = [0x0au8; 16];
    let b = [0x0bu8; 16];
    let c = [0x0cu8; 16];
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("ip", "IPv6")
        .fixed16_data(&[a, b])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("ip", "IPv6")
            .fixed16_data(&[c])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);
    match cb.chunks[1].column(0) {
        Column::Ipv6(col) => assert_eq!(col.value(0), c),
        other => panic!("expected Ipv6, got {other:?}"),
    }
}

#[test]
fn test_block_end_scans_uuid_ipv4_ipv6() {
    // The completeness scan must walk UUID (16/row), IPv4 (4/row), and IPv6
    // (16/row) to the exact block end, and report a one-byte-short buffer as
    // "need more bytes".
    let data = BlockBuilder::new()
        .header(3, 2)
        .column_header("u", "UUID")
        .fixed16_data(&[UUID_00112233_WIRE, [0u8; 16]])
        .column_header("ip4", "IPv4")
        .ipv4_data(&[3221226219, 0])
        .column_header("ip6", "IPv6")
        .fixed16_data(&[[0x20u8; 16], [0u8; 16]])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_multi_block_enum8_kept_as_chunks() {
    // Enum8 blocks stay separate chunks, never concatenated.
    let type_str = "Enum8('pending' = 1, 'active' = 2, 'closed' = -1)";
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("status", type_str)
        .int8_data(&[1, 2])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 3)
            .column_header("status", type_str)
            .int8_data(&[-1, 1, 2])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
    match cb.chunks[0].column(0) {
        Column::Enum8(c) => assert_eq!(c.values, vec![1i8, 2]),
        other => panic!("expected Enum8, got {other:?}"),
    }
    match cb.chunks[1].column(0) {
        Column::Enum8(c) => assert_eq!(c.values, vec![-1i8, 1, 2]),
        other => panic!("expected Enum8, got {other:?}"),
    }
}

#[test]
fn test_block_end_scans_enum_columns() {
    // The completeness scan must walk Enum8 (1/row) and Enum16 (2/row) to the
    // exact block end, and report a one-byte-short buffer as "need more bytes".
    let data = BlockBuilder::new()
        .header(2, 3)
        .column_header("e8", "Enum8('a' = 1, 'b' = 2)")
        .int8_data(&[1, 2, 1])
        .column_header("e16", "Enum16('a' = 1, 'b' = -2)")
        .int16_data(&[1, -2, 1])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_multi_block_decimal_kept_as_chunks() {
    // Decimal blocks stay separate chunks, never concatenated.
    let block_a: Vec<[u8; 4]> = vec![13i32.to_le_bytes(), (-1i32).to_le_bytes()];
    let block_b: Vec<[u8; 4]> = vec![79i32.to_le_bytes()];
    let refs_a: Vec<&[u8]> = block_a.iter().map(|r| r.as_slice()).collect();
    let refs_b: Vec<&[u8]> = block_b.iter().map(|r| r.as_slice()).collect();
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("d", "Decimal(9, 4)")
        .decimal_data(&refs_a, 4)
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("d", "Decimal(9, 4)")
            .decimal_data(&refs_b, 4)
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);
    match cb.chunks[0].column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.len(), 2);
            assert_eq!(i32::from_le_bytes(c.value(1).try_into().unwrap()), -1);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
    match cb.chunks[1].column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.len(), 1);
            assert_eq!(i32::from_le_bytes(c.value(0).try_into().unwrap()), 79);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
}

#[test]
fn test_block_end_scans_decimal_columns() {
    // The completeness scan must walk every Decimal width (4/8/16/32 bytes
    // per row) to the exact block end, and report a one-byte-short buffer as
    // "need more bytes".
    let d32: Vec<[u8; 4]> = vec![13i32.to_le_bytes(), (-1i32).to_le_bytes()];
    let d256: Vec<[u8; 32]> = vec![[0x01u8; 32], [0xFFu8; 32]];
    let refs32: Vec<&[u8]> = d32.iter().map(|r| r.as_slice()).collect();
    let refs256: Vec<&[u8]> = d256.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(2, 2)
        .column_header("d32", "Decimal(9, 4)")
        .decimal_data(&refs32, 4)
        .column_header("d256", "Decimal(50, 10)")
        .decimal_data(&refs256, 32)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_multi_block_wide_int_kept_as_chunks() {
    // Wide-int blocks stay separate chunks, never concatenated.
    let a = [w32(13), [0xFFu8; 32]];
    let b = [w32(79)];
    let refs_a: Vec<&[u8]> = a.iter().map(|r| r.as_slice()).collect();
    let refs_b: Vec<&[u8]> = b.iter().map(|r| r.as_slice()).collect();
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("w", "Int256")
        .wide_int_data(&refs_a, 32)
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("w", "Int256")
            .wide_int_data(&refs_b, 32)
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);
    match cb.chunks[0].column(0) {
        Column::Int256(c) => {
            assert_eq!(c.len(), 2);
            assert_eq!(c.value(1), [0xFFu8; 32]);
        }
        other => panic!("expected Int256, got {other:?}"),
    }
    match cb.chunks[1].column(0) {
        Column::Int256(c) => {
            assert_eq!(c.len(), 1);
            assert_eq!(c.value(0), w32(79));
        }
        other => panic!("expected Int256, got {other:?}"),
    }
}

#[test]
fn test_block_end_scans_wide_int_columns() {
    // The completeness scan must walk both wide-int widths (16 and 32 bytes
    // per row) to the exact block end, and report a one-byte-short buffer as
    // "need more bytes".
    let i128_rows = [w16(13), [0xFFu8; 16]];
    let u256_rows = [w32(79), [0xFFu8; 32]];
    let refs128: Vec<&[u8]> = i128_rows.iter().map(|r| r.as_slice()).collect();
    let refs256: Vec<&[u8]> = u256_rows.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(2, 2)
        .column_header("i128", "Int128")
        .wide_int_data(&refs128, 16)
        .column_header("u256", "UInt256")
        .wide_int_data(&refs256, 32)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_multi_block_array_separate_chunks() {
    // Native blocks stay separate chunks; two Array(Int32) blocks must not be
    // concatenated.
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(Int32)")
        .array_offsets(&[1, 3])
        .int32_data(&[13, 79, 21])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("a", "Array(Int32)")
            .array_offsets(&[2])
            .int32_data(&[34, 55])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);

    let chunk0 = as_array(cb.chunks[0].column(0));
    assert_eq!(chunk0.offsets, vec![0i64, 1, 3]);
    match chunk0.values.as_ref() {
        Column::Int32(v) => assert_eq!(v.values, vec![13, 79, 21]),
        other => panic!("expected Int32, got {other:?}"),
    }
    let chunk1 = as_array(cb.chunks[1].column(0));
    assert_eq!(chunk1.offsets, vec![0i64, 2]);
    match chunk1.values.as_ref() {
        Column::Int32(v) => assert_eq!(v.values, vec![34, 55]),
        other => panic!("expected Int32, got {other:?}"),
    }
}

#[test]
fn test_block_end_scans_array() {
    // The completeness scan must walk an Array column to the exact block end
    // and report a one-byte-short buffer as "need more bytes".
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("a", "Array(Int32)")
        .array_offsets(&[2, 2, 5])
        .int32_data(&[13, 79, 21, 34, 55])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_multi_block_tuple_kept_as_chunks() {
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("t", "Tuple(Int32, String)")
        .int32_data(&[13, 79])
        .string_data(&["user_1", "user_2"])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("t", "Tuple(Int32, String)")
            .int32_data(&[-7])
            .string_data(&["user_3"])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);
    let t0 = as_tuple(cb.chunks[0].column(0));
    let t1 = as_tuple(cb.chunks[1].column(0));
    assert_eq!(t0.len(), 2);
    assert_eq!(t1.len(), 1);
    match &t1.fields[0] {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), &[-7]),
        other => panic!("expected Int32 element, got {other:?}"),
    }
}

#[test]
fn test_multi_block_map_kept_as_chunks() {
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("m", "Map(String, Int32)")
        .array_offsets(&[1, 2])
        .string_data(&["a", "b"])
        .int32_data(&[13, 79])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("m", "Map(String, Int32)")
            .array_offsets(&[1])
            .string_data(&["c"])
            .int32_data(&[-7])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);
    let m1 = as_map(cb.chunks[1].column(0));
    assert_eq!(m1.offsets, vec![0i64, 1]);
    let (_, values) = map_entries(m1);
    match values {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), &[-7]),
        other => panic!("expected Int32 values, got {other:?}"),
    }
}

#[test]
fn test_multi_block_point_kept_as_chunks() {
    // Geo blocks stay separate chunks, never concatenated.
    let mut data = BlockBuilder::new()
        .header(1, 1)
        .column_header("p", "Point")
        .float64_data(&[1.0])
        .float64_data(&[2.0])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 2)
            .column_header("p", "Point")
            .float64_data(&[3.0, 5.0])
            .float64_data(&[4.0, 6.0])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(as_tuple(cb.chunks[0].column(0)).len(), 1);
    assert_eq!(as_tuple(cb.chunks[1].column(0)).len(), 2);
}

#[test]
fn test_block_end_scans_name_decoration_types() {
    // The completeness scan walks the same bytes the decoders consume for all
    // three alias groups, ending exactly at the block boundary.
    let dictionary = ["", "user_1"];
    let lc_full = BlockBuilder::new()
        .low_cardinality_string(&dictionary, &[1u64], 1)
        .build();
    let (key_version, lc_body) = lc_full.split_at(8);
    let data = BlockBuilder::new()
        .header(3, 1)
        // Column 0: SAF body is one Float64 (each header is immediately
        // followed by its own data, per the Native per-column framing).
        .column_header("s", "SimpleAggregateFunction(sum, Float64)")
        .float64_data(&[3.5])
        // Column 1: MultiPolygon, three offset levels then one Point.
        .column_header("mp", "MultiPolygon")
        .array_offsets(&[1])
        .array_offsets(&[1])
        .array_offsets(&[1])
        .float64_data(&[1.0])
        .float64_data(&[2.0])
        // Column 2: Nested, hoisted LC key version, offsets, LC body.
        .column_header("n", "Nested(a LowCardinality(String))")
        .raw_bytes(key_version)
        .array_offsets(&[1])
        .raw_bytes(lc_body)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    // And a full decode consumes it without error.
    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_columns(), 3);
}
