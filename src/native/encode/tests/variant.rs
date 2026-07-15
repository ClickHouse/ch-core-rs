use super::*;
use crate::column::VariantLayout;

fn variant_batch() -> ColBatch {
    let variants = vec![
        Column::Utf8(utf8_column(&[b"user_1", b"x"])),
        Column::UInt64(PrimitiveColumn::new(vec![13])),
    ];
    ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Variant(vec![ChType::String, ChType::UInt64]),
        }]),
        vec![Column::Variant(
            VariantColumn::try_new(&[u8::MAX, 0, 1, 0], variants).unwrap(),
        )],
        4,
    )
}

#[test]
fn roundtrip_variant_rev0() {
    roundtrip(&variant_batch(), 0);
}

#[test]
fn roundtrip_variant_tcp_revision() {
    roundtrip(&variant_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_of_variant() {
    let values = VariantColumn::try_new(
        &[0, 1, u8::MAX],
        vec![
            Column::Utf8(utf8_column(&[b"user_1"])),
            Column::UInt64(PrimitiveColumn::new(vec![13])),
        ],
    )
    .unwrap();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Array(Box::new(ChType::Variant(vec![
                ChType::String,
                ChType::UInt64,
            ]))),
        }]),
        vec![Column::Array(ArrayColumn::new(
            vec![0, 2, 3],
            Column::Variant(values),
        ))],
        2,
    );

    roundtrip(&batch, 0);
}

#[test]
fn rev0_frames_variant_basic_mode_and_dense_children() {
    let bytes = encode_block(&variant_batch(), &EncodeOptions::default()).unwrap();

    let mut expected_body = 0u64.to_le_bytes().to_vec();
    expected_body.extend_from_slice(&[u8::MAX, 0, 1, 0]);
    expected_body.extend_from_slice(&[6]);
    expected_body.extend_from_slice(b"user_1");
    expected_body.extend_from_slice(&[1, b'x']);
    expected_body.extend_from_slice(&13u64.to_le_bytes());
    assert!(bytes.ends_with(&expected_body));
}

#[test]
fn zero_row_variant_encodes_schema_without_body() {
    let column = VariantColumn::try_new(
        &[],
        vec![
            Column::Utf8(utf8_column(&[])),
            Column::UInt64(PrimitiveColumn::new(Vec::new())),
        ],
    )
    .unwrap();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Variant(vec![ChType::String, ChType::UInt64]),
        }]),
        vec![Column::Variant(column)],
        0,
    );

    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema, batch.schema);
}

#[test]
fn malformed_variant_routing_is_rejected_before_writing() {
    let mut batch = variant_batch();
    let Column::Variant(column) = &mut batch.columns[0] else {
        unreachable!("variant_batch always constructs a Variant column");
    };
    let VariantLayout::Flat { offsets, .. } = &mut column.layout else {
        unreachable!("two alternatives always use a flat union");
    };
    offsets[3] = 7;

    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}

#[test]
fn roundtrip_nested_variant_128_plus_alternatives() {
    // A Variant with >= 128 alternatives forces the two-level (Nested) Arrow
    // union layout. Use FixedString(1..=200) so canonicalization spreads the
    // alternatives across two groups, then route rows through both groups plus
    // NULLs. This drives encode_variant_data's nested-discriminator
    // reconstruction and validate_variant's Nested branch end to end.
    let mut names: Vec<String> = (1..=200).map(|w| format!("FixedString({w})")).collect();
    names.sort();
    let type_name = format!("Variant({})", names.join(", "));
    let ch_type = parse_ch_type(&type_name).expect("canonical Variant header parses");
    let ChType::Variant(alternatives) = &ch_type else {
        unreachable!("parsed a Variant header");
    };
    // Canonical alternative `i` is `FixedString(widths[i])`, derived from the
    // normalized order the parser produced (not from `names`), so the children
    // below line up with the discriminators regardless of sort details.
    let widths: Vec<usize> = alternatives
        .iter()
        .map(|a| match a {
            ChType::FixedString(w) => *w,
            other => panic!("expected FixedString alternative, got {other:?}"),
        })
        .collect();
    assert!(widths.len() >= 128, "must exceed the flat-union cap");

    // Route rows through both groups (discriminators below and above 128) plus
    // the NULL discriminator 255, including repeats within one child.
    let discriminators: Vec<u8> = vec![0, 5, 127, 128, 199, 255, 0, 128, 255, 63];

    let children: Vec<Column> = widths
        .iter()
        .enumerate()
        .map(|(alt, &width)| {
            let count = discriminators
                .iter()
                .filter(|&&d| d as usize == alt)
                .count();
            let mut data = Vec::with_capacity(width * count);
            for occurrence in 0..count {
                // A distinct fill byte per (alternative, occurrence) keeps the
                // decoded child data a meaningful round-trip subject.
                let fill = (alt as u8).wrapping_add(occurrence as u8).wrapping_add(1);
                data.resize(data.len() + width, fill);
            }
            Column::FixedBinary(FixedBinaryColumn::new(data, width))
        })
        .collect();

    let column = VariantColumn::try_new(&discriminators, children)
        .expect("child lengths match the discriminator counts");
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ch_type.clone(),
        }]),
        vec![Column::Variant(column)],
        discriminators.len(),
    );

    // Value-level round-trip: decode(encode(batch)) reproduces every buffer,
    // including the reconstructed Nested layout.
    roundtrip(&batch, 0);
    roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);

    // Byte-level round-trip: the decoded batch re-encodes to identical bytes.
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    let reencoded = encode_block(&decoded.chunks[0], &EncodeOptions::default()).unwrap();
    assert_eq!(
        bytes, reencoded,
        "nested Variant must re-encode byte-identically"
    );
}

#[test]
fn variant_child_type_mismatch_is_rejected() {
    let column = VariantColumn::try_new(
        &[0],
        vec![
            Column::UInt64(PrimitiveColumn::new(vec![13])),
            Column::UInt64(PrimitiveColumn::new(Vec::new())),
        ],
    )
    .unwrap();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Variant(vec![ChType::String, ChType::UInt64]),
        }]),
        vec![Column::Variant(column)],
        1,
    );

    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}
