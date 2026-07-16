use super::*;
use crate::column::{DynamicChild, DynamicColumn};

fn as_dynamic(column: &Column) -> &DynamicColumn {
    match column {
        Column::Dynamic(column) => column,
        other => panic!("expected Dynamic column, got {other:?}"),
    }
}

fn push_type_name(buf: &mut Vec<u8>, name: &str) {
    write_varint(buf, name.len() as u64);
    buf.extend_from_slice(name.as_bytes());
}

fn direct_prefix(version: u64, types: &[&str]) -> Vec<u8> {
    let mut bytes = version.to_le_bytes().to_vec();
    if version == 1 {
        write_varint(&mut bytes, types.len() as u64);
    }
    write_varint(&mut bytes, types.len() as u64);
    for name in types {
        push_type_name(&mut bytes, name);
    }
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes
}

fn flattened_prefix(types: &[&str]) -> Vec<u8> {
    let mut bytes = 3u64.to_le_bytes().to_vec();
    write_varint(&mut bytes, types.len() as u64);
    for name in types {
        push_type_name(&mut bytes, name);
    }
    bytes
}

#[test]
fn parse_dynamic_parameters_and_reject_illegal_wrappers() {
    assert_eq!(
        parse_ch_type("Dynamic"),
        Some(ChType::Dynamic { max_types: 32 })
    );
    assert_eq!(
        parse_ch_type("Dynamic()"),
        Some(ChType::Dynamic { max_types: 32 })
    );
    assert_eq!(
        parse_ch_type("Dynamic(max_types=79)"),
        Some(ChType::Dynamic { max_types: 79 })
    );
    assert_eq!(ChType::Dynamic { max_types: 32 }.to_string(), "Dynamic");
    assert_eq!(parse_ch_type("Dynamic(max_types=255)"), None);
    assert_eq!(parse_ch_type("Dynamic(MAX_TYPES=13)"), None);
    assert_eq!(parse_ch_type("Nullable(Dynamic)"), None);
    assert_eq!(parse_ch_type("Variant(Dynamic, String)"), None);
    let low_cardinality = parse_ch_type("LowCardinality(Dynamic)").unwrap();
    assert!(unsupported_header_type_name(&low_cardinality).is_some());
}

#[test]
fn decode_dynamic_v1_direct_with_shared_variant() {
    // Global child order is SharedVariant, String, UInt64. Rows are NULL,
    // String, UInt64, SharedVariant. The shared blob is a binary String type
    // descriptor followed by one String value's serializeBinary payload.
    let shared_blob = [0x15, 0x01, b'x'];
    let mut prefix = direct_prefix(1, &["String", "UInt64"]);
    prefix.extend_from_slice(&[u8::MAX, 1, 2, 0]);
    write_varint(&mut prefix, shared_blob.len() as u64);
    prefix.extend_from_slice(&shared_blob);

    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("v", "Dynamic(max_types=2)")
        .raw_bytes(&prefix)
        .string_data(&["user_1"])
        .uint64_data(&[13])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let dynamic = as_dynamic(decoded.chunks[0].column(0));
    assert_eq!(dynamic.type_ids, vec![u32::MAX, 1, 2, 0]);
    assert_eq!(dynamic.offsets, vec![0, 0, 0, 0]);
    assert_eq!(dynamic.null_count(), 1);
    assert_eq!(dynamic.children.len(), 3);
    match &dynamic.children[0] {
        DynamicChild::Shared(values) => assert_eq!(values.value(0), shared_blob),
        other => panic!("expected SharedVariant child, got {other:?}"),
    }
    match &dynamic.children[1] {
        DynamicChild::Typed { ch_type, values } => {
            assert_eq!(ch_type, &ChType::String);
            let Column::Utf8(values) = values else {
                panic!("expected String values")
            };
            assert_eq!(values.value(0), b"user_1");
        }
        other => panic!("expected typed child, got {other:?}"),
    }
    match &dynamic.children[2] {
        DynamicChild::Typed { ch_type, values } => {
            assert_eq!(ch_type, &ChType::UInt64);
            let Column::UInt64(values) = values else {
                panic!("expected UInt64 values")
            };
            assert_eq!(values.values, vec![13]);
        }
        other => panic!("expected typed child, got {other:?}"),
    }
}

#[test]
fn decode_dynamic_v2_and_flattened() {
    let mut v2 = direct_prefix(2, &["String", "UInt64"]);
    v2.extend_from_slice(&[1, 2, u8::MAX]);
    let v2 = BlockBuilder::new()
        .revision(DBMS_TCP_PROTOCOL_VERSION)
        .header(1, 3)
        .column_header("v", "Dynamic(max_types=2)")
        .raw_bytes(&v2)
        .string_data(&["user_1"])
        .uint64_data(&[79])
        .build();
    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    let decoded = decode_all_bytes(&v2, &options).unwrap();
    let direct = as_dynamic(decoded.chunks[0].column(0));
    assert_eq!(direct.type_ids, vec![1, 2, u32::MAX]);
    assert!(matches!(direct.children[0], DynamicChild::Shared(_)));

    let mut flattened = flattened_prefix(&["String", "UInt64"]);
    flattened.extend_from_slice(&[0, 1, 2]);
    let flattened = BlockBuilder::new()
        .header(1, 3)
        .column_header("v", "Dynamic(max_types=1)")
        .raw_bytes(&flattened)
        .string_data(&["user_2"])
        .uint64_data(&[79])
        .build();
    let decoded = decode_all_bytes(&flattened, &DecodeOptions::default()).unwrap();
    let flattened = as_dynamic(decoded.chunks[0].column(0));
    assert_eq!(flattened.type_ids, vec![0, 1, u32::MAX]);
    assert_eq!(flattened.children.len(), 2);
    assert!(flattened.shared_child_index().is_none());
}

#[test]
fn decode_flattened_index_width_boundary_255_and_256() {
    // The server selects the per-row flattened index width with n(num_types + 1)
    // (src/DataTypes/DataTypesNumber.cpp at v26.6.1.1193-stable): UInt8 when
    // num_types + 1 <= 256, UInt16 when <= 65536, and so on. The NULL sentinel
    // value equals num_types, so num_types == 255 still fits UInt8 (sentinel byte
    // 255) while num_types == 256 needs UInt16. Confirm the decoder reads exactly
    // that many index bytes per row at the boundary: a wrong width would misalign
    // the child bodies and either error or consume the wrong number of bytes.
    for &num_types in &[255usize, 256usize] {
        let names: Vec<String> = (1..=num_types)
            .map(|k| format!("FixedString({k})"))
            .collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut body = flattened_prefix(&name_refs);
        // One row selecting child 0 (FixedString(1)) at the server-chosen width.
        if num_types <= 255 {
            body.push(0u8); // UInt8 index
        } else {
            body.extend_from_slice(&0u16.to_le_bytes()); // UInt16 index
        }
        // child 0 is FixedString(1): one 1-byte value; the rest have no rows.
        body.push(0x13);
        let block = BlockBuilder::new()
            .header(1, 1)
            .column_header("v", "Dynamic(max_types=1)")
            .raw_bytes(&body)
            .build();
        assert_eq!(
            block_end(&block, &DecodeOptions::default()).unwrap(),
            Some(block.len()),
            "num_types={num_types}: decoder consumed the wrong number of index bytes",
        );
        let decoded = decode_all_bytes(&block, &DecodeOptions::default()).unwrap();
        let dynamic = as_dynamic(decoded.chunks[0].column(0));
        assert_eq!(dynamic.type_ids, vec![0]);
        assert_eq!(dynamic.children.len(), num_types);
    }
}

#[test]
fn decode_dynamic_zero_rows_and_multi_block() {
    let empty = BlockBuilder::new()
        .header(1, 0)
        .column_header("v", "Dynamic")
        .build();
    let decoded = decode_all_bytes(&empty, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    let empty_column = empty_column(&decoded.schema.fields[0].ch_type);
    assert!(as_dynamic(&empty_column).is_empty());

    let mut first_body = flattened_prefix(&["String"]);
    first_body.extend_from_slice(&[0]);
    let first = BlockBuilder::new()
        .header(1, 1)
        .column_header("v", "Dynamic")
        .raw_bytes(&first_body)
        .string_data(&["user_1"])
        .build();
    let mut second_body = flattened_prefix(&["UInt64"]);
    second_body.extend_from_slice(&[0]);
    let second = BlockBuilder::new()
        .header(1, 1)
        .column_header("v", "Dynamic")
        .raw_bytes(&second_body)
        .uint64_data(&[79])
        .build();
    let mut bytes = first;
    bytes.extend_from_slice(&second);
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    assert!(matches!(
        as_dynamic(decoded.chunks[0].column(0)).children[0],
        DynamicChild::Typed {
            ch_type: ChType::String,
            ..
        }
    ));
    assert!(matches!(
        as_dynamic(decoded.chunks[1].column(0)).children[0],
        DynamicChild::Typed {
            ch_type: ChType::UInt64,
            ..
        }
    ));
}

#[test]
fn deeply_nested_dynamic_stream_errors_instead_of_overflowing() {
    // Dynamic's runtime types are column DATA, so each level restarts the
    // header parser's per-type depth budget: a ~2.4 MB stream nesting
    // Array(Dynamic) 100k levels deep would overflow the stack without the
    // cumulative cap. Both the allocating decode and the allocation-free
    // completeness scan must reject it as InvalidDynamic, not abort.
    let mut body = Vec::new();
    for _ in 0..100_000 {
        body.extend_from_slice(&flattened_prefix(&["Array(Dynamic)"]));
    }
    body.extend_from_slice(&flattened_prefix(&["String"]));
    let block = BlockBuilder::new()
        .header(1, 1)
        .column_header("v", "Dynamic")
        .raw_bytes(&body)
        .build();
    assert!(matches!(
        decode_all_bytes(&block, &DecodeOptions::default()),
        Err(DecodeError::InvalidDynamic { .. })
    ));
    assert!(matches!(
        block_end(&block, &DecodeOptions::default()),
        Err(DecodeError::InvalidDynamic { .. })
    ));
}

#[test]
fn reject_malformed_dynamic_state() {
    let word_four = BlockBuilder::new()
        .header(1, 1)
        .column_header("v", "Dynamic")
        .raw_bytes(&4u64.to_le_bytes())
        .build();
    assert!(matches!(
        decode_all_bytes(&word_four, &DecodeOptions::default()),
        Err(DecodeError::InvalidDynamic { .. })
    ));

    let mut too_many = direct_prefix(1, &["String", "UInt64"]);
    too_many.push(u8::MAX);
    let too_many = BlockBuilder::new()
        .header(1, 1)
        .column_header("v", "Dynamic(max_types=1)")
        .raw_bytes(&too_many)
        .build();
    assert!(matches!(
        decode_all_bytes(&too_many, &DecodeOptions::default()),
        Err(DecodeError::InvalidDynamic { .. })
    ));
}
