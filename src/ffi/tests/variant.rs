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

fn geometry_child(kind: GeoKind, seed: f64) -> Column {
    let point = Column::Tuple(TupleColumn::new(
        vec![
            Column::Float64(PrimitiveColumn::new(vec![seed])),
            Column::Float64(PrimitiveColumn::new(vec![seed + 0.5])),
        ],
        1,
    ));
    let mut column = point;
    for _ in 1..kind.expansion_depth() {
        column = Column::Array(ArrayColumn::new(vec![0, 1], column));
    }
    column
}

fn geometry_batch_with_type(ch_type: ChType) -> Arc<ColBatch> {
    let children = crate::schema::GEOMETRY_ALTERNATIVES
        .iter()
        .enumerate()
        .map(|(index, alternative)| match alternative {
            ChType::Geo(kind) => geometry_child(*kind, 13.0 + index as f64),
            other => unreachable!("Geometry alternative is always geo, got {other:?}"),
        })
        .collect();
    let column = VariantColumn::try_new(&[0, 1, 2, 3, 4, 5, 6, u8::MAX], children).unwrap();
    Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "g".into(),
            ch_type,
        }]),
        vec![Column::Variant(column)],
        8,
    ))
}

fn geometry_batch() -> Arc<ColBatch> {
    geometry_batch_with_type(ChType::Geometry)
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
fn export_geometry_as_seven_child_dense_union() {
    let batch = geometry_batch();

    // Safety: both outputs are writable zeroed C Data structs. Their borrowed
    // buffers remain backed by `batch` until the release callbacks run.
    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema);
        let field = &**schema.children.add(0);
        assert_eq!(
            CStr::from_ptr(field.format).to_str().unwrap(),
            "+ud:0,1,2,3,4,5,6,7"
        );
        assert_eq!(field.flags & 2, 2, "Geometry has intrinsic NULL");
        assert_eq!(field.n_children, 8);
        let expected = [
            ("LineString", "+L"),
            ("MultiLineString", "+L"),
            ("MultiPolygon", "+L"),
            ("Point", "+s"),
            ("Polygon", "+L"),
            ("Ring", "+L"),
            ("MultiPoint", "+L"),
            ("NULL", "n"),
        ];
        for (index, (name, format)) in expected.into_iter().enumerate() {
            let child = &**field.children.add(index);
            assert_eq!(CStr::from_ptr(child.name).to_str().unwrap(), name);
            assert_eq!(CStr::from_ptr(child.format).to_str().unwrap(), format);
        }
        let point = &**field.children.add(3);
        assert_eq!(point.n_children, 2);
        let multi_point = &**field.children.add(6);
        assert_eq!(multi_point.n_children, 1);
        let multi_point_item = &**multi_point.children.add(0);
        assert_eq!(
            CStr::from_ptr(multi_point_item.format).to_str().unwrap(),
            "+s"
        );
        assert_eq!(multi_point_item.n_children, 2);
        for coordinate in 0..2 {
            let child = &**multi_point_item.children.add(coordinate);
            assert_eq!(CStr::from_ptr(child.format).to_str().unwrap(), "g");
        }
        (schema.release.unwrap())(&mut schema);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let field = &**array.children.add(0);
        assert_eq!(field.length, 8);
        assert_eq!(field.null_count, 0, "dense unions own no validity buffer");
        assert_eq!(field.n_buffers, 2, "type ids plus dense child offsets");
        assert_eq!(field.n_children, 8);
        let type_ids = *field.buffers.add(0) as *const i8;
        assert_eq!(
            std::slice::from_raw_parts(type_ids, 8),
            &[0, 1, 2, 3, 4, 5, 6, 7]
        );
        let offsets = *field.buffers.add(1) as *const i32;
        assert_eq!(std::slice::from_raw_parts(offsets, 8), &[0; 8]);
        for child in 0..8 {
            assert_eq!((**field.children.add(child)).length, 1);
        }
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn export_nullable_geometry_matches_bare_geometry_in_batch_and_stream() {
    // ClickHouse rejects Nullable(Geometry), but ChType is public. Keep this
    // hand-built illegal wrapper structurally safe on both standalone and
    // result-wide stream schema paths, just like Nullable(Variant).
    let batch = geometry_batch_with_type(ChType::Nullable(Box::new(ChType::Geometry)));

    // Safety: all outputs are writable zeroed C Data structs, their buffers
    // remain backed by `batch`, and every populated output is released.
    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema);
        let field = &**schema.children.add(0);
        assert_eq!(
            CStr::from_ptr(field.format).to_str().unwrap(),
            "+ud:0,1,2,3,4,5,6,7"
        );
        assert_eq!(field.flags & 2, 2);
        assert_eq!(field.n_children, 8);
        (schema.release.unwrap())(&mut schema);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let field = &**array.children.add(0);
        assert_eq!(field.n_buffers, 2);
        assert_eq!(field.n_children, 8);
        (array.release.unwrap())(&mut array);

        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(batch.schema.clone(), vec![batch], &mut stream);

        let mut stream_schema: ArrowSchema = std::mem::zeroed();
        assert_eq!(
            (stream.get_schema.unwrap())(&mut stream, &mut stream_schema),
            0
        );
        let field = &**stream_schema.children.add(0);
        assert_eq!(
            CStr::from_ptr(field.format).to_str().unwrap(),
            "+ud:0,1,2,3,4,5,6,7"
        );
        assert_eq!(field.n_children, 8);

        let mut stream_array: ArrowArray = std::mem::zeroed();
        assert_eq!(
            (stream.get_next.unwrap())(&mut stream, &mut stream_array),
            0
        );
        assert_eq!((**stream_array.children.add(0)).n_children, 8);

        (stream_array.release.unwrap())(&mut stream_array);
        (stream_schema.release.unwrap())(&mut stream_schema);
        (stream.release.unwrap())(&mut stream);
    }
}

#[test]
fn field_nullability_resolves_fixed_aliases_defensively() {
    assert!(field_is_nullable(&ChType::Geometry));
    assert!(!field_is_nullable(&ChType::Geo(GeoKind::Point)));
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
