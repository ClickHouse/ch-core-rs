use super::*;

fn dynamic_batch(child: DynamicChild, rows: &[u32]) -> Arc<ColBatch> {
    let column = DynamicColumn::try_new(rows, vec![child]).unwrap();
    Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Dynamic { max_types: 1 },
        }]),
        vec![Column::Dynamic(column)],
        rows.len(),
    ))
}

#[test]
fn export_batch_schema_and_array_for_dynamic() {
    let shared = Utf8Column::new(vec![0, 3], vec![0x15, 0x01, b'x']);
    let column = DynamicColumn::try_new(
        &[1, 2, 0, u32::MAX],
        vec![
            DynamicChild::Shared(shared),
            DynamicChild::Typed {
                ch_type: ChType::String,
                values: Column::Utf8(Utf8Column::new(vec![0, 6], b"user_1".to_vec())),
            },
            DynamicChild::Typed {
                ch_type: ChType::UInt64,
                values: Column::UInt64(PrimitiveColumn::new(vec![13])),
            },
        ],
    )
    .unwrap();
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Dynamic { max_types: 2 },
        }]),
        vec![Column::Dynamic(column)],
        4,
    ));

    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch(&batch, &mut schema, &mut array).unwrap();
        let field = &**schema.children.add(0);
        assert_eq!(
            CStr::from_ptr(field.format).to_str().unwrap(),
            "+ud:0,1,2,3"
        );
        assert_eq!(field.n_children, 4);
        assert_eq!(
            CStr::from_ptr((**field.children.add(0)).name)
                .to_str()
                .unwrap(),
            "SharedVariant"
        );
        assert_eq!(
            CStr::from_ptr((**field.children.add(0)).format)
                .to_str()
                .unwrap(),
            "z"
        );
        assert_eq!(
            CStr::from_ptr((**field.children.add(1)).format)
                .to_str()
                .unwrap(),
            "u"
        );
        assert_eq!(
            CStr::from_ptr((**field.children.add(2)).format)
                .to_str()
                .unwrap(),
            "L"
        );

        let field_array = &**array.children.add(0);
        assert_eq!(field_array.n_children, field.n_children);
        let ids = *field_array.buffers.add(0) as *const i8;
        assert_eq!(std::slice::from_raw_parts(ids, 4), &[1, 2, 0, 3]);
        let offsets = *field_array.buffers.add(1) as *const i32;
        assert_eq!(std::slice::from_raw_parts(offsets, 4), &[0, 0, 0, 0]);
        assert_eq!((**field_array.children.add(0)).n_buffers, 3);
        assert_eq!((**field_array.children.add(3)).null_count, 1);

        (array.release.unwrap())(&mut array);
        (schema.release.unwrap())(&mut schema);
    }
}

#[test]
fn stream_unifies_block_local_dynamic_children() {
    let string_batch = dynamic_batch(
        DynamicChild::Typed {
            ch_type: ChType::String,
            values: Column::Utf8(Utf8Column::new(vec![0, 6], b"user_1".to_vec())),
        },
        &[0],
    );
    let uint_batch = dynamic_batch(
        DynamicChild::Typed {
            ch_type: ChType::UInt64,
            values: Column::UInt64(PrimitiveColumn::new(vec![79])),
        },
        &[0],
    );
    let schema = string_batch.schema.clone();

    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![string_batch, uint_batch], &mut stream);

        let mut schema: ArrowSchema = std::mem::zeroed();
        assert_eq!((stream.get_schema.unwrap())(&mut stream, &mut schema), 0);
        let dynamic = &**schema.children.add(0);
        assert_eq!(
            CStr::from_ptr(dynamic.format).to_str().unwrap(),
            "+ud:0,1,2"
        );
        assert_eq!(dynamic.n_children, 3);
        assert_eq!(
            CStr::from_ptr((**dynamic.children.add(0)).name)
                .to_str()
                .unwrap(),
            "String"
        );
        assert_eq!(
            CStr::from_ptr((**dynamic.children.add(1)).name)
                .to_str()
                .unwrap(),
            "UInt64"
        );

        let mut first: ArrowArray = std::mem::zeroed();
        assert_eq!((stream.get_next.unwrap())(&mut stream, &mut first), 0);
        let first_dynamic = &**first.children.add(0);
        assert_eq!(first_dynamic.n_children, 3);
        assert_eq!((**first_dynamic.children.add(0)).length, 1);
        assert_eq!((**first_dynamic.children.add(1)).length, 0);
        let first_ids = *first_dynamic.buffers.add(0) as *const i8;
        assert_eq!(*first_ids, 0);

        let mut second: ArrowArray = std::mem::zeroed();
        assert_eq!((stream.get_next.unwrap())(&mut stream, &mut second), 0);
        let second_dynamic = &**second.children.add(0);
        assert_eq!(second_dynamic.n_children, 3);
        assert_eq!((**second_dynamic.children.add(0)).length, 0);
        assert_eq!((**second_dynamic.children.add(1)).length, 1);
        let second_ids = *second_dynamic.buffers.add(0) as *const i8;
        assert_eq!(*second_ids, 1);

        (second.release.unwrap())(&mut second);
        (first.release.unwrap())(&mut first);
        (schema.release.unwrap())(&mut schema);
        (stream.release.unwrap())(&mut stream);
    }
}

#[test]
fn stream_unifies_dynamic_nested_in_array() {
    let make = |ch_type: ChType, values: Column| {
        let dynamic =
            DynamicColumn::try_new(&[0], vec![DynamicChild::Typed { ch_type, values }]).unwrap();
        Arc::new(ColBatch::new(
            Schema::new(vec![Field {
                name: "v".into(),
                ch_type: ChType::Array(Box::new(ChType::Dynamic { max_types: 1 })),
            }]),
            vec![Column::Array(ArrayColumn::new(
                vec![0, 1],
                Column::Dynamic(dynamic),
            ))],
            1,
        ))
    };
    let first = make(
        ChType::String,
        Column::Utf8(Utf8Column::new(vec![0, 6], b"user_2".to_vec())),
    );
    let second = make(
        ChType::UInt64,
        Column::UInt64(PrimitiveColumn::new(vec![13])),
    );
    let schema = first.schema.clone();

    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![first, second], &mut stream);
        let mut schema: ArrowSchema = std::mem::zeroed();
        (stream.get_schema.unwrap())(&mut stream, &mut schema);
        let list = &**schema.children.add(0);
        let dynamic = &**list.children.add(0);
        assert_eq!(
            CStr::from_ptr(dynamic.format).to_str().unwrap(),
            "+ud:0,1,2"
        );

        for expected_id in [0i8, 1i8] {
            let mut array: ArrowArray = std::mem::zeroed();
            (stream.get_next.unwrap())(&mut stream, &mut array);
            let list = &**array.children.add(0);
            let dynamic = &**list.children.add(0);
            assert_eq!(dynamic.n_children, 3);
            assert_eq!(*(*dynamic.buffers.add(0) as *const i8), expected_id);
            (array.release.unwrap())(&mut array);
        }
        (schema.release.unwrap())(&mut schema);
        (stream.release.unwrap())(&mut stream);
    }
}

#[test]
fn export_128_dynamic_children_as_union_of_unions() {
    let children = (1..=128)
        .map(|width| DynamicChild::Typed {
            ch_type: ChType::FixedString(width),
            values: Column::FixedBinary(FixedBinaryColumn::new(
                if width == 128 {
                    vec![0x13; width]
                } else {
                    Vec::new()
                },
                width,
            )),
        })
        .collect();
    let dynamic = DynamicColumn::try_new(&[127, u32::MAX], children).unwrap();
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Dynamic { max_types: 254 },
        }]),
        vec![Column::Dynamic(dynamic)],
        2,
    ));

    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch(&batch, &mut schema, &mut array).unwrap();

        let outer_schema = &**schema.children.add(0);
        assert_eq!(outer_schema.n_children, 2); // one group plus Null
        assert_eq!((**outer_schema.children.add(0)).n_children, 128);

        let outer = &**array.children.add(0);
        let outer_ids = *outer.buffers.add(0) as *const i8;
        assert_eq!(std::slice::from_raw_parts(outer_ids, 2), &[0, 1]);
        let group = &**outer.children.add(0);
        let group_ids = *group.buffers.add(0) as *const i8;
        assert_eq!(*group_ids, 127);

        (array.release.unwrap())(&mut array);
        (schema.release.unwrap())(&mut schema);
    }
}

/// One chunk whose Dynamic carries distinct `FixedString(N)` children for every
/// width in `widths`, with a single row selecting the first (lowest-width) child
/// so each chunk actually routes a value through the union.
fn fixed_string_dynamic_chunk(widths: std::ops::RangeInclusive<usize>) -> Arc<ColBatch> {
    let lo = *widths.start();
    let children = widths
        .map(|width| DynamicChild::Typed {
            ch_type: ChType::FixedString(width),
            values: Column::FixedBinary(FixedBinaryColumn::new(
                if width == lo {
                    vec![0x13; width]
                } else {
                    Vec::new()
                },
                width,
            )),
        })
        .collect::<Vec<_>>();
    let dynamic = DynamicColumn::try_new(&[0], children).unwrap();
    Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Dynamic { max_types: 255 },
        }]),
        vec![Column::Dynamic(dynamic)],
        1,
    ))
}

#[test]
fn stream_unifies_dynamic_across_chunks_into_grouped_union() {
    // Each chunk's child set stays well below 128, but their union is exactly
    // 128 distinct types, so the planned stream branch must build the
    // union-of-unions (one 128-wide group plus NULL). This exercises
    // write_dynamic_schema_with_plan and export_dynamic_array_with_plan's
    // grouped path, which the single-block standalone tests cannot reach.
    let first = fixed_string_dynamic_chunk(1..=64);
    let second = fixed_string_dynamic_chunk(65..=128);
    let schema = first.schema.clone();

    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![first, second], &mut stream);

        let mut arrow_schema: ArrowSchema = std::mem::zeroed();
        assert_eq!(
            (stream.get_schema.unwrap())(&mut stream, &mut arrow_schema),
            0
        );
        let dynamic = &**arrow_schema.children.add(0);
        assert_eq!(CStr::from_ptr(dynamic.format).to_str().unwrap(), "+ud:0,1");
        assert_eq!(dynamic.n_children, 2);
        assert_eq!((**dynamic.children.add(0)).n_children, 128);
        assert_eq!(
            CStr::from_ptr((**dynamic.children.add(1)).name)
                .to_str()
                .unwrap(),
            "NULL"
        );

        for _ in 0..2 {
            let mut array: ArrowArray = std::mem::zeroed();
            assert_eq!((stream.get_next.unwrap())(&mut stream, &mut array), 0);
            let outer = &**array.children.add(0);
            assert_eq!(outer.length, 1);
            assert_eq!(outer.n_children, 2);
            // The single row routes into group 0, the only non-null group.
            assert_eq!(*(*outer.buffers.add(0) as *const i8), 0);
            let group = &**outer.children.add(0);
            assert_eq!(group.n_children, 128);
            assert_eq!(group.length, 1);
            (array.release.unwrap())(&mut array);
        }
        (arrow_schema.release.unwrap())(&mut arrow_schema);
        (stream.release.unwrap())(&mut stream);
    }
}

#[test]
fn stream_rejects_dynamic_child_set_beyond_union_limit() {
    // Synthesize a result-wide Dynamic whose distinct child set is one past the
    // Arrow signed-i8 union limit (127 * 128 = 16,256). Empty FixedString(N)
    // children keep this cheap: the plan is built with no per-row data. The
    // stream must then refuse to export, surfacing the failure through the Arrow
    // C Stream error contract instead of emitting a malformed union.
    let over = DYNAMIC_MAX_EXPORT_CHILDREN + 1;
    let children = (1..=over)
        .map(|width| DynamicChild::Typed {
            ch_type: ChType::FixedString(width),
            values: Column::FixedBinary(FixedBinaryColumn::new(Vec::new(), width)),
        })
        .collect();
    let dynamic = DynamicColumn::try_new(&[], children).unwrap();
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Dynamic { max_types: 255 },
        }]),
        vec![Column::Dynamic(dynamic)],
        0,
    ));
    let schema = batch.schema.clone();

    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![batch], &mut stream);

        let mut arrow_schema: ArrowSchema = std::mem::zeroed();
        assert_ne!(
            (stream.get_schema.unwrap())(&mut stream, &mut arrow_schema),
            0
        );
        assert!(arrow_schema.release.is_none());

        let mut array: ArrowArray = std::mem::zeroed();
        assert_ne!((stream.get_next.unwrap())(&mut stream, &mut array), 0);
        assert!(array.release.is_none());

        let msg = CStr::from_ptr((stream.get_last_error.unwrap())(&mut stream))
            .to_str()
            .unwrap();
        assert!(msg.contains(&over.to_string()), "unexpected message: {msg}");
        assert!(
            msg.contains(&DYNAMIC_MAX_EXPORT_CHILDREN.to_string()),
            "unexpected message: {msg}"
        );

        (stream.release.unwrap())(&mut stream);
    }
}

#[test]
fn export_batch_rejects_dynamic_child_set_beyond_union_limit() {
    // A FLATTENED Dynamic block can legitimately carry more distinct runtime
    // types than the two-level union can route within Arrow's signed-i8 code
    // space (its type count is bounded by row count, not max_types). The
    // standalone batch export must fail loudly rather than emit a union with an
    // out-of-range type code. Exactly the limit still succeeds; one past it is
    // rejected with the out-structs left untouched. Empty FixedString(N)
    // children keep both halves cheap (no per-row data).
    let dynamic_batch = |count: usize| {
        let children = (1..=count)
            .map(|width| DynamicChild::Typed {
                ch_type: ChType::FixedString(width),
                values: Column::FixedBinary(FixedBinaryColumn::new(Vec::new(), width)),
            })
            .collect();
        Arc::new(ColBatch::new(
            Schema::new(vec![Field {
                name: "v".into(),
                ch_type: ChType::Dynamic { max_types: 255 },
            }]),
            vec![Column::Dynamic(
                DynamicColumn::try_new(&[], children).unwrap(),
            )],
            0,
        ))
    };

    unsafe {
        // Exactly the limit: 127 groups of 128, NULL outer code 127 still fits i8.
        let ok = dynamic_batch(DYNAMIC_MAX_EXPORT_CHILDREN);
        let mut schema: ArrowSchema = std::mem::zeroed();
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch(&ok, &mut schema, &mut array).unwrap();
        assert!(schema.release.is_some());
        assert!(array.release.is_some());
        (array.release.unwrap())(&mut array);
        (schema.release.unwrap())(&mut schema);

        // One past the limit: rejected, both out-structs left zeroed/unreleased.
        let too_wide = dynamic_batch(DYNAMIC_MAX_EXPORT_CHILDREN + 1);
        let expected = ExportError::DynamicUnionTooWide {
            children: DYNAMIC_MAX_EXPORT_CHILDREN + 1,
            limit: DYNAMIC_MAX_EXPORT_CHILDREN,
        };

        let mut schema: ArrowSchema = std::mem::zeroed();
        let mut array: ArrowArray = std::mem::zeroed();
        assert_eq!(
            export_batch(&too_wide, &mut schema, &mut array).unwrap_err(),
            expected
        );
        assert!(schema.release.is_none());
        assert!(array.release.is_none());

        // The paired helpers guard independently, not just through export_batch.
        let mut schema_only: ArrowSchema = std::mem::zeroed();
        assert_eq!(
            export_batch_schema(&too_wide, &mut schema_only).unwrap_err(),
            expected
        );
        assert!(schema_only.release.is_none());
        let mut array_only: ArrowArray = std::mem::zeroed();
        assert_eq!(
            export_batch_array(&too_wide, &mut array_only).unwrap_err(),
            expected
        );
        assert!(array_only.release.is_none());
    }
}

#[test]
fn stream_remaps_reversed_local_children_and_preserves_dense_offsets() {
    let make = |children, ids: &[u32]| {
        Arc::new(ColBatch::new(
            Schema::new(vec![Field {
                name: "v".into(),
                ch_type: ChType::Dynamic { max_types: 2 },
            }]),
            vec![Column::Dynamic(
                DynamicColumn::try_new(ids, children).unwrap(),
            )],
            ids.len(),
        ))
    };
    let first = make(
        vec![
            DynamicChild::Typed {
                ch_type: ChType::String,
                values: Column::Utf8(Utf8Column::new(vec![0, 6, 12], b"user_1user_2".to_vec())),
            },
            DynamicChild::Typed {
                ch_type: ChType::UInt64,
                values: Column::UInt64(PrimitiveColumn::new(vec![13])),
            },
        ],
        &[0, 1, 0],
    );
    let second = make(
        vec![
            DynamicChild::Typed {
                ch_type: ChType::UInt64,
                values: Column::UInt64(PrimitiveColumn::new(vec![79])),
            },
            DynamicChild::Typed {
                ch_type: ChType::String,
                values: Column::Utf8(Utf8Column::new(vec![0, 6, 12], b"user_3user_4".to_vec())),
            },
        ],
        &[1, 0, 1],
    );
    let schema = first.schema.clone();

    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![first, second], &mut stream);
        for _ in 0..2 {
            let mut array: ArrowArray = std::mem::zeroed();
            assert_eq!((stream.get_next.unwrap())(&mut stream, &mut array), 0);
            let dynamic = &**array.children.add(0);
            let ids = *dynamic.buffers.add(0) as *const i8;
            let offsets = *dynamic.buffers.add(1) as *const i32;
            assert_eq!(std::slice::from_raw_parts(ids, 3), &[0, 1, 0]);
            assert_eq!(std::slice::from_raw_parts(offsets, 3), &[0, 0, 1]);
            (array.release.unwrap())(&mut array);
        }
        (stream.release.unwrap())(&mut stream);
    }
}
