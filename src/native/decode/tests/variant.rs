use super::*;
use crate::column::{VariantLayout, ARROW_UNION_MAX_CHILDREN};

fn as_variant(column: &Column) -> &crate::column::VariantColumn {
    match column {
        Column::Variant(column) => column,
        other => panic!("expected Variant column, got {other:?}"),
    }
}

#[test]
fn test_parse_variant_normalizes_server_order() {
    assert_eq!(
        parse_ch_type("Variant(UInt64, String, String, Nothing)"),
        Some(ChType::Variant(vec![ChType::String, ChType::UInt64]))
    );
    assert_eq!(
        ChType::Variant(vec![ChType::String, ChType::UInt64]).to_string(),
        "Variant(String, UInt64)"
    );

    // Direct nullable-like alternatives are forbidden; intrinsic discriminator
    // 255 is Variant's null representation. Nested Nullable remains legal.
    assert_eq!(parse_ch_type("Variant(Nullable(String))"), None);
    assert_eq!(
        parse_ch_type("Variant(LowCardinality(Nullable(String)))"),
        None
    );
    assert_eq!(parse_ch_type("Variant(String, Variant(UInt64))"), None);
    assert_eq!(parse_ch_type("Nullable(Variant(String, UInt64))"), None);
    let low_cardinality_variant = parse_ch_type("LowCardinality(Variant(String, UInt64))").unwrap();
    assert!(unsupported_header_type_name(&low_cardinality_variant).is_some());
    assert_eq!(
        parse_ch_type("Variant(Array(Nullable(String)), UInt64)"),
        Some(ChType::Variant(vec![
            ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::String)))),
            ChType::UInt64,
        ]))
    );
    assert_eq!(parse_ch_type("Variant()"), None);
    assert_eq!(parse_ch_type("Variant(Nothing)"), None);
}

#[test]
fn test_parse_variant_drops_physically_nothing_alternatives() {
    // `SimpleAggregateFunction(anyLast, Nothing)` is physically `Nothing`
    // (pure name decoration), so the server drops it before assigning
    // discriminators, leaving `UInt64` at discriminator 0. A literal-only
    // Nothing check would instead keep it and reorder the discriminators.
    assert_eq!(
        parse_ch_type("Variant(SimpleAggregateFunction(anyLast, Nothing), UInt64)"),
        Some(ChType::Variant(vec![ChType::UInt64]))
    );

    // A chained decoration over Nothing resolves to Nothing through the whole
    // delegate chain and is dropped just the same.
    assert_eq!(
        parse_ch_type(
            "Variant(SimpleAggregateFunction(anyLast, SimpleAggregateFunction(sum, Nothing)), UInt64)"
        ),
        Some(ChType::Variant(vec![ChType::UInt64]))
    );

    // A caller-built `ChType::Variant` that keeps a physically-Nothing SAF
    // alternative is noncanonical: it disagrees with the server's
    // discriminator order, so encode validation (via the shared
    // `unsupported_header_type_name`) must reject it.
    let saf_nothing = ChType::SimpleAggregateFunction {
        func: "anyLast".into(),
        inner: Box::new(ChType::Nothing),
    };
    let noncanonical = ChType::Variant(vec![saf_nothing, ChType::UInt64]);
    assert!(unsupported_header_type_name(&noncanonical).is_some());
}

#[test]
fn test_parse_variant_rejects_more_than_255_raw_parts() {
    // A canonical server header never spells more than 255 alternatives, so a
    // longer top-level list is rejected before any part is parsed, bounding a
    // hostile header's allocation amplification. 256 duplicate `Int8` parts
    // would collapse to one alternative after dedup, but the cap fires first.
    let names = vec!["Int8"; 256].join(", ");
    let type_name = format!("Variant({names})");
    assert_eq!(parse_ch_type(&type_name), None);

    // 255 parts stay within the cap and parse (deduping to a single
    // alternative here).
    let names = vec!["Int8"; 255].join(", ");
    let type_name = format!("Variant({names})");
    assert_eq!(
        parse_ch_type(&type_name),
        Some(ChType::Variant(vec![ChType::Int8]))
    );
}

#[test]
fn test_decode_array_of_variant_hoists_variant_prefix() {
    // Rows: ["user_1", 13] / [NULL]. Array hoists its element Variant's
    // mode prefix ahead of the Array offsets, then the flattened Variant body
    // follows the offsets.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("v", "Array(Variant(String, UInt64))")
        .raw_bytes(&0u64.to_le_bytes())
        .uint64_data(&[2, 3])
        .raw_bytes(&[0, 1, u8::MAX])
        .string_data(&["user_1"])
        .uint64_data(&[13])
        .build();

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let array = match decoded.chunks[0].column(0) {
        Column::Array(column) => column,
        other => panic!("expected Array column, got {other:?}"),
    };
    assert_eq!(array.offsets, vec![0, 2, 3]);
    let variant = as_variant(array.values.as_ref());
    assert_eq!(variant.value_position(0), Some((0, 0)));
    assert_eq!(variant.value_position(1), Some((1, 0)));
    assert_eq!(variant.value_position(2), Some((u8::MAX, 0)));
}

#[test]
fn test_decode_variant_basic_dense_union() {
    // Variant(String, UInt64), rows: NULL, "user_1", 13, "x".
    // Prefix is the fixed-width BASIC mode 0. The body is all discriminators,
    // then the dense String run, then the dense UInt64 run.
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("v", "Variant(String, UInt64)")
        .raw_bytes(&0u64.to_le_bytes())
        .raw_bytes(&[u8::MAX, 0, 1, 0])
        .string_data(&["user_1", "x"])
        .uint64_data(&[13])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        decoded.schema.fields[0].ch_type,
        ChType::Variant(vec![ChType::String, ChType::UInt64])
    );
    let variant = as_variant(decoded.chunks[0].column(0));
    assert_eq!(variant.len(), 4);
    assert_eq!(variant.null_count(), 1);
    assert_eq!(
        variant.layout,
        VariantLayout::Flat {
            type_ids: vec![2, 0, 1, 0],
            offsets: vec![0, 0, 0, 1],
        }
    );
    assert_eq!(variant.value_position(0), Some((u8::MAX, 0)));
    assert_eq!(variant.value_position(1), Some((0, 0)));
    assert_eq!(variant.value_position(2), Some((1, 0)));
    assert_eq!(variant.value_position(3), Some((0, 1)));

    match &variant.variants[0] {
        Column::Utf8(column) => {
            assert_eq!(column.value(0), b"user_1");
            assert_eq!(column.value(1), b"x");
        }
        other => panic!("expected dense String child, got {other:?}"),
    }
    match &variant.variants[1] {
        Column::UInt64(column) => assert_eq!(column.values, vec![13]),
        other => panic!("expected dense UInt64 child, got {other:?}"),
    }
    assert_eq!(variant.nulls.len, 1);
}

#[test]
fn test_decode_variant_zero_rows() {
    // Native's rows gate suppresses the complete Variant payload, including its
    // mode word and alternative prefixes.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("v", "Variant(String, UInt64)")
        .build();
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(
        decoded.schema.fields[0].ch_type,
        ChType::Variant(vec![ChType::String, ChType::UInt64])
    );

    let empty = empty_column(&decoded.schema.fields[0].ch_type);
    let variant = as_variant(&empty);
    assert!(variant.is_empty());
    assert_eq!(variant.variants.len(), 2);
    assert!(variant.variants.iter().all(Column::is_empty));
    assert!(variant.nulls.is_empty());
}

#[test]
fn test_decode_variant_multi_block() {
    let first = BlockBuilder::new()
        .header(1, 2)
        .column_header("v", "Variant(String, UInt64)")
        .raw_bytes(&0u64.to_le_bytes())
        .raw_bytes(&[0, u8::MAX])
        .string_data(&["user_1"])
        .build();
    let second = BlockBuilder::new()
        .header(1, 2)
        .column_header("v", "Variant(String, UInt64)")
        .raw_bytes(&0u64.to_le_bytes())
        .raw_bytes(&[1, 0])
        .string_data(&["user_2"])
        .uint64_data(&[79])
        .build();
    let mut data = first;
    data.extend_from_slice(&second);

    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    assert_eq!(as_variant(decoded.chunks[0].column(0)).null_count(), 1);
    let second = as_variant(decoded.chunks[1].column(0));
    assert_eq!(second.value_position(0), Some((1, 0)));
    assert_eq!(second.value_position(1), Some((0, 0)));
}

#[test]
fn test_decode_variant_uses_nested_arrow_union_above_127_alternatives() {
    let mut names = (1..=ARROW_UNION_MAX_CHILDREN)
        .map(|width| format!("FixedString({width})"))
        .collect::<Vec<_>>();
    names.sort();
    let type_name = format!("Variant({})", names.join(", "));

    // One row selects canonical alternative 0, FixedString(1). Every other
    // dense child has zero rows and therefore no body bytes.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("v", &type_name)
        .raw_bytes(&0u64.to_le_bytes())
        .raw_bytes(&[0])
        .raw_bytes(b"x")
        .build();
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let variant = as_variant(decoded.chunks[0].column(0));
    match &variant.layout {
        VariantLayout::Nested {
            type_ids,
            offsets,
            groups,
        } => {
            assert_eq!(type_ids, &[0]);
            assert_eq!(offsets, &[0]);
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].type_ids, vec![0]);
            assert_eq!(groups[0].offsets, vec![0]);
        }
        other => panic!("expected nested Arrow union layout, got {other:?}"),
    }
}

#[test]
fn test_variant_layout_supports_all_255_alternatives() {
    let variants = (0..u8::MAX as usize)
        .map(|alternative| {
            let values = if alternative == u8::MAX as usize - 1 {
                vec![13]
            } else {
                Vec::new()
            };
            Column::UInt8(PrimitiveColumn::new(values))
        })
        .collect();
    let variant = crate::column::VariantColumn::try_new(&[u8::MAX - 1, u8::MAX], variants)
        .expect("255 alternatives plus intrinsic NULL must fit");
    assert_eq!(variant.value_position(0), Some((u8::MAX - 1, 0)));
    assert_eq!(variant.value_position(1), Some((u8::MAX, 0)));
    assert_eq!(variant.null_count(), 1);

    let too_many = (0..=u8::MAX)
        .map(|_| Column::UInt8(PrimitiveColumn::new(Vec::new())))
        .collect();
    assert!(crate::column::VariantColumn::try_new(&[], too_many).is_err());
}

#[test]
fn test_variant_rejects_non_native_mode_and_invalid_discriminator() {
    let compact = BlockBuilder::new()
        .header(1, 1)
        .column_header("v", "Variant(String, UInt64)")
        .raw_bytes(&1u64.to_le_bytes())
        .build();
    assert!(matches!(
        decode_all_bytes(&compact, &DecodeOptions::default()),
        Err(DecodeError::InvalidVariant { .. })
    ));

    let invalid = BlockBuilder::new()
        .header(1, 1)
        .column_header("v", "Variant(String, UInt64)")
        .raw_bytes(&0u64.to_le_bytes())
        .raw_bytes(&[2])
        .build();
    assert!(matches!(
        decode_all_bytes(&invalid, &DecodeOptions::default()),
        Err(DecodeError::InvalidVariant { .. })
    ));
    assert!(matches!(
        block_end(&invalid, &DecodeOptions::default()),
        Err(DecodeError::InvalidVariant { .. })
    ));
}
