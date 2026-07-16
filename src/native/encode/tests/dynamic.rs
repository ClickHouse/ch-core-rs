use super::*;
use crate::column::{DynamicChild, DynamicColumn};
use crate::native::decode::decode_all_bytes_binary_types;

fn shared_blob_string(value: &[u8]) -> Vec<u8> {
    let mut blob = vec![0x15];
    write_varint(&mut blob, value.len() as u64);
    blob.extend_from_slice(value);
    blob
}

fn direct_dynamic_batch() -> ColBatch {
    let shared = shared_blob_string(b"x");
    let children = vec![
        DynamicChild::Shared(utf8_column(&[&shared])),
        DynamicChild::Typed {
            ch_type: ChType::String,
            values: Column::Utf8(utf8_column(&[b"user_1"])),
        },
        DynamicChild::Typed {
            ch_type: ChType::UInt64,
            values: Column::UInt64(PrimitiveColumn::new(vec![13])),
        },
    ];
    let dynamic = DynamicColumn::try_new(&[u32::MAX, 1, 2, 0], children).unwrap();
    ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Dynamic { max_types: 2 },
        }]),
        vec![Column::Dynamic(dynamic)],
        4,
    )
}

fn flattened_dynamic_batch() -> ColBatch {
    let children = vec![
        DynamicChild::Typed {
            ch_type: ChType::String,
            values: Column::Utf8(utf8_column(&[b"user_2"])),
        },
        DynamicChild::Typed {
            ch_type: ChType::UInt64,
            values: Column::UInt64(PrimitiveColumn::new(vec![79])),
        },
    ];
    let dynamic = DynamicColumn::try_new(&[0, 1, u32::MAX], children).unwrap();
    ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Dynamic { max_types: 1 },
        }]),
        vec![Column::Dynamic(dynamic)],
        3,
    )
}

#[test]
fn roundtrip_dynamic_v1_and_v2() {
    roundtrip(&direct_dynamic_batch(), 0);
    roundtrip(&direct_dynamic_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_dynamic_flattened() {
    roundtrip(&flattened_dynamic_batch(), 0);
    roundtrip(&flattened_dynamic_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_of_dynamic() {
    let dynamic = DynamicColumn::try_new(
        &[0, 1, u32::MAX],
        vec![
            DynamicChild::Typed {
                ch_type: ChType::String,
                values: Column::Utf8(utf8_column(&[b"user_1"])),
            },
            DynamicChild::Typed {
                ch_type: ChType::UInt64,
                values: Column::UInt64(PrimitiveColumn::new(vec![13])),
            },
        ],
    )
    .unwrap();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Array(Box::new(ChType::Dynamic { max_types: 1 })),
        }]),
        vec![Column::Array(ArrayColumn::new(
            vec![0, 2, 3],
            Column::Dynamic(dynamic),
        ))],
        2,
    );
    roundtrip(&batch, 0);
}

#[test]
fn roundtrip_dynamic_child_containing_nested_dynamic() {
    let inner = DynamicColumn::try_new(
        &[0, u32::MAX],
        vec![DynamicChild::Typed {
            ch_type: ChType::UInt64,
            values: Column::UInt64(PrimitiveColumn::new(vec![79])),
        }],
    )
    .unwrap();
    let outer = DynamicColumn::try_new(
        &[0],
        vec![DynamicChild::Typed {
            ch_type: ChType::Array(Box::new(ChType::Dynamic { max_types: 1 })),
            values: Column::Array(ArrayColumn::new(vec![0, 2], Column::Dynamic(inner))),
        }],
    )
    .unwrap();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Dynamic { max_types: 1 },
        }]),
        vec![Column::Dynamic(outer)],
        1,
    );
    roundtrip(&batch, 0);
    roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn binary_type_descriptors_roundtrip() {
    for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
        let batch = direct_dynamic_batch();
        let encode_options = EncodeOptions {
            protocol_revision: revision,
        };
        let bytes = encode_block_binary_types(&batch, &encode_options).unwrap();
        let decoded = decode_all_bytes_binary_types(
            &bytes,
            &DecodeOptions {
                protocol_revision: revision,
            },
        )
        .unwrap();
        assert_batches_eq(&batch, &decoded.chunks[0]);
    }
}

#[test]
fn zero_row_dynamic_has_header_only() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Dynamic { max_types: 32 },
        }]),
        vec![Column::Dynamic(
            DynamicColumn::try_new(&[], Vec::new()).unwrap(),
        )],
        0,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema, batch.schema);
}

#[test]
fn malformed_dynamic_routing_and_order_are_rejected() {
    let mut bad_routing = direct_dynamic_batch();
    let Column::Dynamic(dynamic) = &mut bad_routing.columns[0] else {
        unreachable!("fixture is Dynamic")
    };
    dynamic.offsets[3] = 7;
    assert!(matches!(
        encode_block(&bad_routing, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));

    let mut bad_order = direct_dynamic_batch();
    let Column::Dynamic(dynamic) = &mut bad_order.columns[0] else {
        unreachable!("fixture is Dynamic")
    };
    dynamic.children.swap(0, 1);
    assert!(matches!(
        encode_block(&bad_order, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}

#[test]
fn dynamic_nothing_child_is_rejected_before_writing() {
    let dynamic = DynamicColumn::try_new(
        &[0],
        vec![DynamicChild::Typed {
            ch_type: ChType::Nothing,
            values: Column::Nothing(NothingColumn::new(1)),
        }],
    )
    .unwrap();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Dynamic { max_types: 1 },
        }]),
        vec![Column::Dynamic(dynamic)],
        1,
    );
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::UnsupportedType { .. })
    ));
}
