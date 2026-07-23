use super::*;

#[test]
fn test_decode_uuid_byte_order_passthrough() {
    // Decode is raw passthrough: the 16 wire bytes for RFC UUID
    // 00112233-4455-6677-8899-aabbccddeeff come back unchanged, in wire order
    // (NOT RFC order). A binding applies the wire->RFC mapping; the core does
    // not reorder. The second row is all-zero (the nil UUID).
    let nil = [0u8; 16];
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("u", "UUID")
        .fixed16_data(&[UUID_00112233_WIRE, nil])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Uuid(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.len(), 2);
            assert_eq!(c.value(0), UUID_00112233_WIRE);
            assert_eq!(c.value(1), nil);
        }
        other => panic!("expected Uuid, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_uuid() {
    // Nullable(UUID): null map first, then the 16-byte rows (null rows still
    // carry placeholder bytes on the wire).
    let nil = [0u8; 16];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("u", "Nullable(UUID)")
        .null_map(&[false, true, false])
        .fixed16_data(&[UUID_00112233_WIRE, nil, [0xffu8; 16]])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Uuid(c) => {
            assert_eq!(c.len(), 3);
            assert_eq!(c.null_count(), 1);
            assert_eq!(c.value(0), UUID_00112233_WIRE);
            assert_eq!(c.value(2), [0xffu8; 16]);
        }
        other => panic!("expected Uuid, got {other:?}"),
    }
    assert!(batch.column(0).validity().unwrap().is_valid(0));
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
    assert!(batch.column(0).validity().unwrap().is_valid(2));
}

#[test]
fn test_decode_uuid_zero_rows() {
    // A zero-row UUID block contributes the schema and an empty width-16
    // column.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("u", "UUID")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.schema.fields[0].ch_type, ChType::Uuid);
}

#[test]
fn test_decode_ipv4_plain() {
    // IPv4 is a UInt32 in bulk; reading 4 LE bytes yields the standard IPv4
    // numeric value (a<<24 | b<<16 | c<<8 | d). 192.0.2.235 = 3221226219.
    let values = [0u32, 3221226219, 169090600, u32::MAX];
    let data = BlockBuilder::new()
        .header(1, values.len())
        .column_header("ip", "IPv4")
        .ipv4_data(&values)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Ipv4(c) => {
            assert_eq!(c.values, vec![0, 3221226219, 169090600, u32::MAX]);
            assert!(c.validity.is_none());
        }
        other => panic!("expected Ipv4, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_ipv4() {
    // Nullable(IPv4): null map first, then the UInt32 payload.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("ip", "Nullable(IPv4)")
        .null_map(&[false, true, false])
        .ipv4_data(&[3221226219, 0, 169090600])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Ipv4(c) => {
            assert_eq!(c.null_count(), 1);
            assert_eq!(c.values, vec![3221226219, 0, 169090600]);
        }
        other => panic!("expected Ipv4, got {other:?}"),
    }
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
}

#[test]
fn test_decode_ipv4_zero_rows() {
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("ip", "IPv4")
        .build();
    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.schema.fields[0].ch_type, ChType::Ipv4);
}

#[test]
fn test_decode_ipv6_plain() {
    // IPv6 is 16 raw bytes in network byte order, passed through verbatim.
    // 2001:db8::68 and the all-zero (::) address.
    let db8: [u8; 16] = [
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x68,
    ];
    let unspecified = [0u8; 16];
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("ip", "IPv6")
        .fixed16_data(&[db8, unspecified])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Ipv6(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.len(), 2);
            assert_eq!(c.value(0), db8);
            assert_eq!(c.value(1), unspecified);
        }
        other => panic!("expected Ipv6, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_ipv6() {
    let db8: [u8; 16] = [
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x68,
    ];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("ip", "Nullable(IPv6)")
        .null_map(&[false, true, false])
        .fixed16_data(&[db8, [0u8; 16], [0xffu8; 16]])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Ipv6(c) => {
            assert_eq!(c.null_count(), 1);
            assert_eq!(c.value(0), db8);
            assert_eq!(c.value(2), [0xffu8; 16]);
        }
        other => panic!("expected Ipv6, got {other:?}"),
    }
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
}

#[test]
fn test_decode_ipv6_zero_rows() {
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("ip", "IPv6")
        .build();
    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.schema.fields[0].ch_type, ChType::Ipv6);
}

#[test]
fn test_decode_enum8_plain() {
    // Enum8 is raw Int8 on the wire (1 byte/row); the name->value map is in
    // the ChType only, never in the per-row data.
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header(
            "status",
            "Enum8('pending' = 1, 'active' = 2, 'closed' = -1)",
        )
        .int8_data(&[1, 2, -1, 1])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Enum8(c) => {
            assert_eq!(c.values, vec![1i8, 2, -1, 1]);
            assert!(c.validity.is_none());
        }
        other => panic!("expected Enum8, got {other:?}"),
    }
    // The variants live in the schema ChType.
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Enum8 {
            variants: vec![
                ("pending".to_string(), 1),
                ("active".to_string(), 2),
                ("closed".to_string(), -1),
            ],
        }
    );
}

#[test]
fn test_decode_enum16_plain() {
    // Enum16 is raw Int16 on the wire (2 bytes/row).
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header(
            "status",
            "Enum16('pending' = 1, 'active' = 2, 'closed' = -1)",
        )
        .int16_data(&[1, -1, 2])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Enum16(c) => assert_eq!(c.values, vec![1i16, -1, 2]),
        other => panic!("expected Enum16, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_enum8() {
    // Nullable(Enum8): the null map first, then the Int8 buffer, exactly like
    // Nullable(Int8). Null rows still carry a placeholder byte on the wire.
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("status", "Nullable(Enum8('pending' = 1, 'active' = 2))")
        .null_map(&[false, true, false, true])
        .int8_data(&[1, 0, 2, 0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Enum8(c) => {
            assert_eq!(c.null_count(), 2);
            assert_eq!(c.values, vec![1i8, 0, 2, 0]);
        }
        other => panic!("expected Enum8, got {other:?}"),
    }
    assert!(batch.column(0).validity().unwrap().is_valid(0));
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Nullable(Box::new(ChType::Enum8 {
            variants: vec![("pending".to_string(), 1), ("active".to_string(), 2)],
        }))
    );
}

#[test]
fn test_decode_enum_zero_rows() {
    // A zero-row block carrying an Enum8 and an Enum16 contributes the schema
    // (variants and all) but no chunks, and the empty columns have length 0.
    let data = BlockBuilder::new()
        .header(2, 0)
        .column_header("e8", "Enum8('a' = 1, 'b' = 2)")
        .column_header("e16", "Enum16('a' = 1, 'b' = -2)")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 2);
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::Enum8 {
            variants: vec![("a".to_string(), 1), ("b".to_string(), 2)],
        }
    );
    assert_eq!(
        cb.schema.fields[1].ch_type,
        ChType::Enum16 {
            variants: vec![("a".to_string(), 1), ("b".to_string(), -2)],
        }
    );
}
