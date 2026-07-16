use super::*;
use crate::column::{FixedBinaryColumn, VariantColumn, ARROW_UNION_MAX_CHILDREN};

fn flat_variant_batch() -> Arc<ColBatch> {
    let column = VariantColumn::try_new(
        &[u8::MAX, 0, 1, 0],
        vec![
            Column::Utf8(Utf8Column::new(vec![0, 6, 7], b"user_1x".to_vec())),
            Column::UInt64(PrimitiveColumn::new(vec![13])),
        ],
    )
    .unwrap();
    Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Variant(vec![ChType::String, ChType::UInt64]),
        }]),
        vec![Column::Variant(column)],
        4,
    ))
}

#[test]
fn export_flat_variant_schema_and_buffers() {
    let batch = flat_variant_batch();

    // Safety: both outputs are writable zeroed C Data structs. Each remains
    // alive until its release callback runs, and `batch` outlives both exports.
    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema);
        let field = &**schema.children.add(0);
        assert_eq!(CStr::from_ptr(field.format).to_str().unwrap(), "+ud:0,1,2");
        assert_eq!(field.flags & 2, 2, "Variant can contain intrinsic NULL");
        assert_eq!(field.n_children, 3);

        let string = &**field.children.add(0);
        assert_eq!(CStr::from_ptr(string.name).to_str().unwrap(), "String");
        assert_eq!(CStr::from_ptr(string.format).to_str().unwrap(), "u");
        let uint64 = &**field.children.add(1);
        assert_eq!(CStr::from_ptr(uint64.name).to_str().unwrap(), "UInt64");
        assert_eq!(CStr::from_ptr(uint64.format).to_str().unwrap(), "L");
        let null = &**field.children.add(2);
        assert_eq!(CStr::from_ptr(null.name).to_str().unwrap(), "NULL");
        assert_eq!(CStr::from_ptr(null.format).to_str().unwrap(), "n");
        (schema.release.unwrap())(&mut schema);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let field = &**array.children.add(0);
        assert_eq!(field.length, 4);
        assert_eq!(field.null_count, 0, "unions have no top-level null count");
        assert_eq!(field.n_buffers, 2, "type ids plus dense child offsets");
        assert_eq!(field.n_children, 3);

        let type_ids = *field.buffers.add(0) as *const i8;
        assert_eq!(std::slice::from_raw_parts(type_ids, 4), &[2, 0, 1, 0]);
        let offsets = *field.buffers.add(1) as *const i32;
        assert_eq!(std::slice::from_raw_parts(offsets, 4), &[0, 0, 0, 1]);
        assert_eq!((**field.children.add(0)).length, 2);
        assert_eq!((**field.children.add(1)).length, 1);
        let null = &**field.children.add(2);
        assert_eq!(null.length, 1);
        assert_eq!(null.null_count, 1);
        assert_eq!(null.n_buffers, 0);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn export_nullable_variant_matches_bare_variant() {
    // ClickHouse forbids `Nullable(Variant)` (`canBeInsideNullable()` is false,
    // confirmed v26.6.1.1193-stable) and the type parser rejects it, so this
    // wrapper is only reachable from a hand-built `ChType`. The pub export path
    // must still treat it exactly as a bare Variant on BOTH the schema and array
    // sides so the two shapes agree: a naive fall-through would emit a `+ud`
    // union format string with zero children (a malformed union).
    let column = VariantColumn::try_new(
        &[u8::MAX, 0, 1, 0],
        vec![
            Column::Utf8(Utf8Column::new(vec![0, 6, 7], b"user_2x".to_vec())),
            Column::UInt64(PrimitiveColumn::new(vec![79])),
        ],
    )
    .unwrap();
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Variant(vec![
                ChType::String,
                ChType::UInt64,
            ]))),
        }]),
        vec![Column::Variant(column)],
        4,
    ));

    // Safety: both outputs are writable zeroed C Data structs. Each remains
    // alive until its release callback runs, and `batch` outlives both exports.
    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema);
        let field = &**schema.children.add(0);
        // Identical to a bare Variant: a dense union node, not a `+ud` header
        // with zero children.
        assert_eq!(CStr::from_ptr(field.format).to_str().unwrap(), "+ud:0,1,2");
        assert_eq!(field.flags & 2, 2, "Variant field is nullable");
        assert_eq!(field.n_children, 3, "String, UInt64, then the NULL child");
        let null = &**field.children.add(2);
        assert_eq!(CStr::from_ptr(null.name).to_str().unwrap(), "NULL");
        assert_eq!(CStr::from_ptr(null.format).to_str().unwrap(), "n");
        (schema.release.unwrap())(&mut schema);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let field = &**array.children.add(0);
        // Array shape must match the schema: 3 children, 2 union buffers, no
        // top-level validity or null count.
        assert_eq!(field.length, 4);
        assert_eq!(field.null_count, 0, "unions have no top-level null count");
        assert_eq!(field.n_buffers, 2, "type ids plus dense child offsets");
        assert_eq!(field.n_children, 3);
        let type_ids = *field.buffers.add(0) as *const i8;
        assert_eq!(std::slice::from_raw_parts(type_ids, 4), &[2, 0, 1, 0]);
        let null = &**field.children.add(2);
        assert_eq!(null.length, 1, "one NULL row routes to the Null child");
        assert_eq!(null.null_count, 1);
        assert_eq!(null.n_buffers, 0);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn export_128_alternatives_as_union_of_unions() {
    let alternatives = (1..=ARROW_UNION_MAX_CHILDREN)
        .map(ChType::FixedString)
        .collect::<Vec<_>>();
    let variants = (1..=ARROW_UNION_MAX_CHILDREN)
        .enumerate()
        .map(|(index, width)| {
            let data = if index == ARROW_UNION_MAX_CHILDREN - 1 {
                vec![b'x'; width]
            } else {
                Vec::new()
            };
            Column::FixedBinary(FixedBinaryColumn::new(data, width))
        })
        .collect();
    let column = VariantColumn::try_new(&[127, u8::MAX], variants).unwrap();
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "v".into(),
            ch_type: ChType::Variant(alternatives),
        }]),
        vec![Column::Variant(column)],
        2,
    ));

    // Safety: both outputs are writable zeroed C Data structs and are released
    // before their backing batch goes out of scope.
    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema);
        let field = &**schema.children.add(0);
        assert_eq!(CStr::from_ptr(field.format).to_str().unwrap(), "+ud:0,1");
        assert_eq!(field.n_children, 2, "one group plus NULL");
        let group = &**field.children.add(0);
        assert_eq!(group.n_children, 128);
        assert_eq!(group.flags, 0);
        let group_format = CStr::from_ptr(group.format).to_str().unwrap();
        assert!(group_format.starts_with("+ud:0,1,2,"));
        assert!(group_format.ends_with(",127"));
        (schema.release.unwrap())(&mut schema);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let field = &**array.children.add(0);
        assert_eq!(field.length, 2);
        assert_eq!(field.n_children, 2);
        let outer_ids = *field.buffers.add(0) as *const i8;
        assert_eq!(std::slice::from_raw_parts(outer_ids, 2), &[0, 1]);
        let group = &**field.children.add(0);
        assert_eq!(group.length, 1);
        assert_eq!(group.n_children, 128);
        let inner_ids = *group.buffers.add(0) as *const i8;
        assert_eq!(*inner_ids, 127);
        assert_eq!((**group.children.add(127)).length, 1);
        assert_eq!((**field.children.add(1)).length, 1, "NULL child");
        (array.release.unwrap())(&mut array);
    }
}
