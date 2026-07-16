use super::*;

// -----------------------------------------------------------------------
// SimpleAggregateFunction / geo aliases / Nested Arrow export
// -----------------------------------------------------------------------

#[test]
fn test_arrow_format_name_decoration_delegates() {
    // Each alias reports the Arrow format of the physical type it delegates to.
    assert_eq!(
        arrow_format(&ChType::SimpleAggregateFunction {
            func: "sum".into(),
            inner: Box::new(ChType::Float64),
        }),
        "g"
    );
    assert_eq!(
        arrow_format(&ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::LowCardinality(Box::new(ChType::String))),
        }),
        "i" // dictionary index type
    );
    assert_eq!(arrow_format(&ChType::Geo(GeoKind::Point)), "+s");
    assert_eq!(arrow_format(&ChType::Geo(GeoKind::Ring)), "+L");
    assert_eq!(arrow_format(&ChType::Geo(GeoKind::MultiPolygon)), "+L");
    assert_eq!(
        arrow_format(&ChType::Nested(vec![("a".into(), ChType::UInt32)])),
        "+L"
    );
}

#[test]
fn test_export_point_schema_and_array() {
    // Point exports as an Arrow struct of two float64 children (unnamed tuple
    // elements are named by 1-based position).
    let schema = Schema::new(vec![Field {
        name: "p".into(),
        ch_type: ChType::Geo(GeoKind::Point),
    }]);
    let columns = vec![Column::Tuple(TupleColumn::new(
        vec![
            Column::Float64(PrimitiveColumn::new(vec![1.0, 3.0])),
            Column::Float64(PrimitiveColumn::new(vec![2.0, 4.0])),
        ],
        2,
    ))];
    let batch = Arc::new(ColBatch::new(schema, columns, 2));
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let p = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(p.format).to_str().unwrap(), "+s");
        assert_eq!(p.n_children, 2);
        let x = &**p.children.add(0);
        let y = &**p.children.add(1);
        assert_eq!(CStr::from_ptr(x.format).to_str().unwrap(), "g");
        assert_eq!(CStr::from_ptr(y.format).to_str().unwrap(), "g");
        assert_eq!(CStr::from_ptr(x.name).to_str().unwrap(), "1");
        assert_eq!(CStr::from_ptr(y.name).to_str().unwrap(), "2");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let p = &**array.children.add(0);
        assert_eq!(p.length, 2);
        assert_eq!(p.n_children, 2);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_nullable_point_schema() {
    // Nullable(Point) sets the struct's nullable flag while still emitting its
    // two element children.
    let schema = Schema::new(vec![Field {
        name: "p".into(),
        ch_type: ChType::Nullable(Box::new(ChType::Geo(GeoKind::Point))),
    }]);
    let columns = vec![Column::Tuple(TupleColumn::new_nullable(
        vec![
            Column::Float64(PrimitiveColumn::new(vec![1.0, 0.0])),
            Column::Float64(PrimitiveColumn::new(vec![2.0, 0.0])),
        ],
        2,
        Bitmap::from_ch_null_map(&[0, 1]),
    ))];
    let batch = Arc::new(ColBatch::new(schema, columns, 2));
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let p = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(p.format).to_str().unwrap(), "+s");
        assert_eq!(p.flags & 2, 2, "nullable flag set for Nullable(Point)");
        assert_eq!(p.n_children, 2);
        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_nested_schema() {
    // Nested exports as a LargeList of a struct whose children carry the
    // Nested field names verbatim.
    let schema = Schema::new(vec![Field {
        name: "n".into(),
        ch_type: ChType::Nested(vec![
            ("a".into(), ChType::UInt32),
            ("b".into(), ChType::String),
        ]),
    }]);
    let entries = Column::Tuple(TupleColumn::new(
        vec![
            Column::UInt32(PrimitiveColumn::new(vec![10, 20])),
            Column::Utf8(Utf8Column::new(vec![0, 1, 2], b"xy".to_vec())),
        ],
        2,
    ));
    let columns = vec![Column::Array(ArrayColumn::new(vec![0, 2], entries))];
    let batch = Arc::new(ColBatch::new(schema, columns, 1));
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let n = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(n.format).to_str().unwrap(), "+L");
        assert_eq!(n.n_children, 1);
        let item = &**n.children.add(0);
        assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "+s");
        assert_eq!(CStr::from_ptr(item.name).to_str().unwrap(), "item");
        assert_eq!(item.n_children, 2);
        let a = &**item.children.add(0);
        let b = &**item.children.add(1);
        assert_eq!(CStr::from_ptr(a.name).to_str().unwrap(), "a");
        assert_eq!(CStr::from_ptr(a.format).to_str().unwrap(), "I");
        assert_eq!(CStr::from_ptr(b.name).to_str().unwrap(), "b");
        assert_eq!(CStr::from_ptr(b.format).to_str().unwrap(), "u");
        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_simple_aggregate_function_over_low_cardinality_schema() {
    // SAF over LowCardinality(Nullable(String)) exports as a dictionary field
    // (index format `i`, a `u` dictionary child, the nullable flag set).
    let schema = Schema::new(vec![Field {
        name: "s".into(),
        ch_type: ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::LowCardinality(Box::new(ChType::Nullable(
                Box::new(ChType::String),
            )))),
        },
    }]);
    let columns = vec![Column::Dictionary(DictionaryColumn::new_nullable(
        vec![1, 0],
        Column::Utf8(Utf8Column::new(vec![0, 0, 6], b"user_1".to_vec())),
        Bitmap::from_ch_null_map(&[0, 1]),
    ))];
    let batch = Arc::new(ColBatch::new(schema, columns, 2));
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let s = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(s.format).to_str().unwrap(), "i");
        assert_eq!(
            s.flags & 2,
            2,
            "nullable flag set for SAF over LC(Nullable)"
        );
        assert!(!s.dictionary.is_null(), "dictionary child present");
        let dict = &*s.dictionary;
        assert_eq!(CStr::from_ptr(dict.format).to_str().unwrap(), "u");
        (schema_out.release.unwrap())(&mut schema_out);
    }
}
