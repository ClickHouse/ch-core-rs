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

/// FLATTENED opt-in options at `revision`.
fn flattened_options(revision: u64) -> EncodeOptions {
    EncodeOptions {
        protocol_revision: revision,
        flattened_dynamic: true,
    }
}

/// Read the 8-byte Dynamic structure word out of a revision-0 single-column
/// block for column "v" of `type_name` (all header varints are single-byte).
fn dynamic_structure_word(bytes: &[u8], type_name: &str) -> u64 {
    let header = 2 + (1 + 1) + (1 + type_name.len());
    u64::from_le_bytes(bytes[header..header + 8].try_into().unwrap())
}

#[test]
fn roundtrip_dynamic_v1_and_v2() {
    roundtrip(&direct_dynamic_batch(), 0);
    roundtrip(&direct_dynamic_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_dynamic_flattened_opt_in() {
    // FLATTENED is opt-in (structure word 3 is unknown to pre-25.6 servers).
    let batch = flattened_dynamic_batch();
    let bytes = encode_block(&batch, &flattened_options(0)).unwrap();
    assert_eq!(dynamic_structure_word(&bytes, "Dynamic(max_types=1)"), 3);
    roundtrip_opts(&batch, &flattened_options(0));
    roundtrip_opts(&batch, &flattened_options(DBMS_TCP_PROTOCOL_VERSION));
}

#[test]
fn default_shared_less_dynamic_encodes_v1_v2_with_empty_shared() {
    // Without the opt-in, a shared-less column takes the V1/V2 layout: V1 at
    // revision 0, V2 at the current TCP revision, with an implicit empty
    // SharedVariant child at its canonical position.
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
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Dynamic { max_types: 2 },
        }]),
        vec![Column::Dynamic(dynamic)],
        3,
    );

    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    assert_eq!(dynamic_structure_word(&bytes, "Dynamic(max_types=2)"), 1);
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    let dynamic = match decoded.chunks[0].column(0) {
        Column::Dynamic(c) => c,
        other => panic!("expected Dynamic, got {other:?}"),
    };
    // Canonical global order: SharedVariant < String < UInt64, so the typed
    // children shift by one and the shared child decodes empty.
    assert_eq!(dynamic.children.len(), 3);
    assert_eq!(dynamic.type_ids, vec![1, 2, u32::MAX]);
    assert_eq!(dynamic.offsets, vec![0, 0, 0]);
    assert_eq!(dynamic.null_count(), 1);
    match &dynamic.children[0] {
        DynamicChild::Shared(values) => assert_eq!(values.len(), 0),
        other => panic!("expected empty SharedVariant child, got {other:?}"),
    }
    match &dynamic.children[1] {
        DynamicChild::Typed { ch_type, values } => {
            assert_eq!(ch_type, &ChType::String);
            let Column::Utf8(values) = values else {
                panic!("expected String values")
            };
            assert_eq!(values.value(0), b"user_2");
        }
        other => panic!("expected typed String child, got {other:?}"),
    }
    match &dynamic.children[2] {
        DynamicChild::Typed { ch_type, values } => {
            assert_eq!(ch_type, &ChType::UInt64);
            let Column::UInt64(values) = values else {
                panic!("expected UInt64 values")
            };
            assert_eq!(values.values, vec![79]);
        }
        other => panic!("expected typed UInt64 child, got {other:?}"),
    }

    // The decoded shape (a real, empty SharedVariant child) is a fixpoint:
    // re-encoding and decoding it round-trips exactly, at both revisions.
    roundtrip(&decoded.chunks[0], 0);
    roundtrip(&decoded.chunks[0], DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn default_shared_less_dynamic_enforces_v1_v2_count_limits() {
    // flattened_dynamic_batch carries 2 direct types over max_types=1, legal
    // for FLATTENED (the server expands overflow out of SharedVariant there)
    // but not for the default V1/V2 fallback.
    let batch = flattened_dynamic_batch();
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
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
    roundtrip_opts(&batch, &flattened_options(0));
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
    roundtrip_opts(&batch, &flattened_options(0));
    roundtrip_opts(&batch, &flattened_options(DBMS_TCP_PROTOCOL_VERSION));

    // The default path also accepts moderate nesting: it decodes back with an
    // implicit empty SharedVariant per level, and that shape is a fixpoint.
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    roundtrip(&decoded.chunks[0], 0);
}

/// Build `levels` DynamicColumns nested through `Array(Dynamic)` children,
/// with a one-row String innermost.
fn nested_dynamic_column(levels: usize) -> Column {
    let mut column = Column::Dynamic(
        DynamicColumn::try_new(
            &[0],
            vec![DynamicChild::Typed {
                ch_type: ChType::String,
                values: Column::Utf8(utf8_column(&[b"x"])),
            }],
        )
        .unwrap(),
    );
    for _ in 0..levels {
        let array = ArrayColumn::new(vec![0, 1], column);
        column = Column::Dynamic(
            DynamicColumn::try_new(
                &[0],
                vec![DynamicChild::Typed {
                    ch_type: ChType::Array(Box::new(ChType::Dynamic { max_types: 1 })),
                    values: Column::Array(array),
                }],
            )
            .unwrap(),
        );
    }
    column
}

fn nested_dynamic_batch(levels: usize) -> ColBatch {
    ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Dynamic { max_types: 1 },
        }]),
        vec![nested_dynamic_column(levels)],
        1,
    )
}

#[test]
fn moderately_nested_dynamic_roundtrips() {
    let batch = nested_dynamic_batch(5);
    roundtrip_opts(&batch, &flattened_options(0));
    roundtrip_opts(&batch, &flattened_options(DBMS_TCP_PROTOCOL_VERSION));
}

#[test]
fn deeply_nested_dynamic_encode_errors_instead_of_overflowing() {
    // Each Dynamic level restarts the per-type depth budget (its child types
    // are column data, not part of the declared schema type), so without the
    // cumulative cap this overflows the stack in validation/encode. Well past
    // the cap, well below any stack limit.
    let batch = nested_dynamic_batch(2_000);
    for flattened_dynamic in [false, true] {
        let options = EncodeOptions {
            flattened_dynamic,
            ..EncodeOptions::default()
        };
        match encode_block(&batch, &options) {
            Err(EncodeError::InconsistentBatch { detail }) => {
                assert!(
                    detail.contains("Dynamic nesting exceeds the maximum type depth"),
                    "unexpected detail: {detail}"
                );
            }
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }
}

#[test]
fn binary_type_descriptors_roundtrip() {
    for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
        let batch = direct_dynamic_batch();
        let encode_options = EncodeOptions {
            protocol_revision: revision,
            ..EncodeOptions::default()
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
