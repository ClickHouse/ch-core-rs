use super::*;

fn nothing_batch(nullable: bool, len: usize) -> Arc<ColBatch> {
    let ch_type = if nullable {
        ChType::Nullable(Box::new(ChType::Nothing))
    } else {
        ChType::Nothing
    };
    let column = if nullable {
        let null_map: Vec<u8> = (0..len)
            .map(|row| if row % 2 == 0 { 0x00 } else { 0x01 })
            .collect();
        Column::Nothing(NothingColumn::new_nullable(
            len,
            Bitmap::from_ch_null_map(&null_map),
        ))
    } else {
        Column::Nothing(NothingColumn::new(len))
    };
    Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type,
        }]),
        vec![column],
        len,
    ))
}

fn assert_nothing_export(batch: &Arc<ColBatch>, expected_flags: i64, expected_len: i64) {
    // Safety: both outputs are writable zeroed C Data structs. Each remains
    // alive until its release callback runs, and `batch` outlives both exports.
    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema);
        let field = &**schema.children.add(0);
        assert_eq!(CStr::from_ptr(field.format).to_str().unwrap(), "n");
        assert_eq!(field.flags, expected_flags);
        assert_eq!(field.n_children, 0);
        assert!(field.children.is_null());
        assert!(field.dictionary.is_null());
        (schema.release.unwrap())(&mut schema);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(batch, &mut array).unwrap();
        let child = &**array.children.add(0);
        assert_eq!(child.length, expected_len);
        assert_eq!(child.null_count, expected_len);
        assert_eq!(child.n_buffers, 0);
        assert!(child.buffers.is_null());
        assert_eq!(child.n_children, 0);
        assert!(child.children.is_null());
        assert!(child.dictionary.is_null());
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn export_bare_nothing_as_nullable_arrow_null() {
    // Arrow requires Null-type fields to be nullable even though bare
    // ClickHouse Nothing carries no Nullable wrapper.
    assert_nothing_export(&nothing_batch(false, 3), 2, 3);
}

#[test]
fn export_nullable_nothing_as_nullable_arrow_null_ignores_mask() {
    // The retained mixed ClickHouse mask does not become an Arrow buffer. Arrow
    // Null intrinsically reports every row null.
    assert_nothing_export(&nothing_batch(true, 4), 2, 4);
}

#[test]
fn export_zero_row_nothing_as_empty_arrow_null() {
    assert_nothing_export(&nothing_batch(false, 0), 2, 0);
    assert_nothing_export(&nothing_batch(true, 0), 2, 0);
}

#[test]
fn export_array_nothing_child_is_zero_buffer_null() {
    // Array(Nothing) with every row an empty array: the flattened `item`
    // child is an Arrow Null (format `n`) of length 0, exported as a nested
    // child under the LargeList container. Mirrors the decode/encode
    // Array(Nothing) fixtures. Rows: [[], []] -> offsets [0, 0, 0], leaving
    // zero flattened Nothing values.
    let schema = Schema::new(vec![Field {
        name: "a".into(),
        ch_type: ChType::Array(Box::new(ChType::Nothing)),
    }]);
    let col = Column::Array(ArrayColumn::new(
        vec![0, 0, 0],
        Column::Nothing(NothingColumn::new(0)),
    ));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 2));

    // Safety: both outputs are writable zeroed C Data structs. Each remains
    // alive until its release callback runs, and `batch` outlives both exports.
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
        assert_eq!(c0.flags & 2, 0, "array level is not nullable");
        assert_eq!(c0.n_children, 1);
        let item = &**c0.children.add(0);
        assert_eq!(CStr::from_ptr(item.name).to_str().unwrap(), "item");
        assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "n");
        assert_eq!(item.flags, 2, "Nothing child is a nullable Null field");
        assert_eq!(item.n_children, 0, "Arrow Null has no grandchildren");
        assert!(item.children.is_null());
        assert!(item.dictionary.is_null());
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 2, "2 rows, all empty");
        assert_eq!(c0.null_count, 0);
        assert_eq!(c0.n_buffers, 2, "LargeList: validity + offsets");
        assert!((*c0.buffers.add(0)).is_null(), "array level validity null");
        let offsets = *c0.buffers.add(1) as *const i64;
        assert_eq!(*offsets, 0, "leading 0");
        assert_eq!(*offsets.add(2), 0, "num_rows + 1 all-zero offsets");

        assert_eq!(c0.n_children, 1);
        let item = &**c0.children.add(0);
        assert_eq!(item.length, 0, "no flattened Nothing values");
        assert_eq!(item.null_count, 0, "null_count == child length");
        assert_eq!(item.n_buffers, 0, "Arrow Null carries no buffers");
        assert!(item.buffers.is_null());
        assert_eq!(item.n_children, 0);
        assert!(item.children.is_null());
        assert!(item.dictionary.is_null());
        (array.release.unwrap())(&mut array);
    }
}
