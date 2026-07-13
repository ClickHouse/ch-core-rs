use super::*;

fn make_dictionary_batch(nullable: bool) -> Arc<ColBatch> {
    use crate::bitmap::Bitmap;
    use crate::column::DictionaryColumn;

    let inner = if nullable {
        ChType::Nullable(Box::new(ChType::String))
    } else {
        ChType::String
    };
    let schema = Schema::new(vec![Field {
        name: "lc".into(),
        ch_type: ChType::LowCardinality(Box::new(inner)),
    }]);
    // values dictionary: 3 entries, indices over 4 rows.
    let values = Column::Utf8(Utf8Column::new(
        vec![0, 5, 11, 17],
        b"user_user_1user_2".to_vec(),
    ));
    let dict = if nullable {
        // Row 1 is null (validity bit 0); the rest are valid.
        let validity = Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00, 0x00]);
        DictionaryColumn::new_nullable(vec![0, 0, 1, 2], values, validity)
    } else {
        DictionaryColumn::new(vec![0, 1, 2, 0], values)
    };
    let columns = vec![Column::Dictionary(dict)];
    Arc::new(ColBatch::new(schema, columns, 4))
}

#[test]
fn test_export_low_cardinality_schema() {
    // Dictionary schema: the field format is the index type (`i`), the
    // dictionary child carries the value type (`u`), and the nullable flag
    // tracks the inner Nullable.
    let batch = make_dictionary_batch(true);
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);

        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "i");
        assert_eq!(c0.flags & 2, 2, "nullable flag set for LC(Nullable(...))");
        assert!(!c0.dictionary.is_null(), "dictionary child present");
        let dict = &*c0.dictionary;
        assert_eq!(CStr::from_ptr(dict.format).to_str().unwrap(), "u");

        (schema_out.release.unwrap())(&mut schema_out);
    }

    // A non-nullable LowCardinality has the flag clear.
    let batch = make_dictionary_batch(false);
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(c0.flags & 2, 0, "nullable flag clear for LC(String)");
        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_low_cardinality_array() {
    // Dictionary array: 2 index buffers (validity + i32 indices), a length
    // equal to the row count, and a dictionary child holding the values.
    let batch = make_dictionary_batch(true);
    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);

        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 4);
        assert_eq!(c0.null_count, 1);
        assert_eq!(c0.n_buffers, 2);
        assert!(!(*c0.buffers.add(0)).is_null(), "validity buffer present");
        let idx = *c0.buffers.add(1) as *const i32;
        assert_eq!(*idx, 0);
        assert_eq!(*idx.add(2), 1);

        assert!(!c0.dictionary.is_null(), "dictionary child array present");
        let dict = &*c0.dictionary;
        assert_eq!(dict.length, 3, "dictionary holds 3 entries");
        assert_eq!(dict.n_buffers, 3, "utf8 values: validity, offsets, data");

        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_arrow_format_low_cardinality() {
    assert_eq!(
        arrow_format(&ChType::LowCardinality(Box::new(ChType::String))),
        "i"
    );
    assert_eq!(
        arrow_format(&ChType::LowCardinality(Box::new(ChType::Nullable(
            Box::new(ChType::String)
        )))),
        "i"
    );
}

#[test]
fn test_export_low_cardinality_saf_nullable_string_schema() {
    use crate::column::DictionaryColumn;

    // LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String))) must
    // export exactly like LowCardinality(Nullable(String)): field format `i`,
    // the index-level nullable flag set (nulls live in the index validity),
    // and a non-nullable `u` (String) dictionary child. The SAF chain between
    // the LC and its removeNullable Nullable is resolved through the shared
    // `low_cardinality_dict_value_type` helper.
    let schema = Schema::new(vec![Field {
        name: "lc_nsaf".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        })),
    }]);
    let values = Column::Utf8(crate::column::Utf8Column::new(vec![0], vec![]));
    let dict = DictionaryColumn::new_nullable(
        vec![],
        values,
        crate::bitmap::Bitmap::from_ch_null_map(&[]),
    );
    let batch = Arc::new(ColBatch::new(schema, vec![Column::Dictionary(dict)], 0));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "i");
        assert_eq!(
            c0.flags & 2,
            2,
            "nullable flag set for LC(SAF(_, Nullable(String)))"
        );
        assert!(!c0.dictionary.is_null(), "dictionary child present");
        let dict_schema = &*c0.dictionary;
        assert_eq!(CStr::from_ptr(dict_schema.format).to_str().unwrap(), "u");
        assert_eq!(
            dict_schema.flags & 2,
            0,
            "dictionary values are non-nullable; nulls live in the index validity"
        );
        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_low_cardinality_uint32() {
    use crate::column::DictionaryColumn;

    // A non-String dictionary value type still exports as dictionary(i32, T):
    // the field format is the index type `i`, and the dictionary child format
    // is the value type, here `I` (uint32). The dictionary export path is
    // generic over the value column, so a UInt32 values column flows through
    // unchanged. This mirrors the String case but pins the child format for a
    // primitive inner.
    let schema = Schema::new(vec![Field {
        name: "lc".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::UInt32)),
    }]);
    // Dictionary slot 0 is the reserved default; the 3 rows index into 1..=3.
    let values = Column::UInt32(PrimitiveColumn::new(vec![0, 13, 79, 4_294_967_295]));
    let dict = DictionaryColumn::new(vec![1, 2, 3], values);
    let batch = Arc::new(ColBatch::new(schema, vec![Column::Dictionary(dict)], 3));

    unsafe {
        // Schema: field format `i`, flag clear (non-nullable), child `I`.
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "i");
        assert_eq!(c0.flags & 2, 0, "non-nullable LC has the flag clear");
        assert!(!c0.dictionary.is_null(), "dictionary child present");
        let dict_schema = &*c0.dictionary;
        assert_eq!(CStr::from_ptr(dict_schema.format).to_str().unwrap(), "I");
        (schema_out.release.unwrap())(&mut schema_out);

        // Array: 2 index buffers (validity, i32 indices), dictionary child
        // holding 4 uint32 entries with 2 buffers (validity, values).
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 3);
        assert_eq!(c0.n_buffers, 2);
        let idx = *c0.buffers.add(1) as *const i32;
        assert_eq!(*idx, 1);
        assert_eq!(*idx.add(2), 3);
        assert!(!c0.dictionary.is_null(), "dictionary child array present");
        let dict_array = &*c0.dictionary;
        assert_eq!(dict_array.length, 4, "dictionary holds 4 entries");
        assert_eq!(dict_array.n_buffers, 2, "uint32 values: validity, values");
        let vals = *dict_array.buffers.add(1) as *const u32;
        assert_eq!(*vals.add(1), 13);
        assert_eq!(*vals.add(3), 4_294_967_295);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_low_cardinality_bfloat16_child_zero_copy() {
    let values_data = vec![[0x00, 0x00], [0x50, 0x41], [0x9e, 0x42]];
    let values_ptr = values_data.as_ptr() as *const c_void;
    let schema = Schema::new(vec![Field {
        name: "lc_bf".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::BFloat16)),
    }]);
    let values = Column::BFloat16(PrimitiveColumn::new(values_data));
    let dictionary = DictionaryColumn::new(vec![1, 2, 1], values);
    let batch = Arc::new(ColBatch::new(
        schema,
        vec![Column::Dictionary(dictionary)],
        3,
    ));

    // Safety: the zeroed FFI outputs are writable and the batch remains alive
    // until each matching release callback is invoked below.
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let field = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(field.format).to_str().unwrap(), "i");
        assert_eq!(field.flags, 0);
        assert!(!field.dictionary.is_null());
        let values_schema = &*field.dictionary;
        assert_eq!(
            CStr::from_ptr(values_schema.format).to_str().unwrap(),
            "w:2"
        );
        assert_eq!(values_schema.flags, 0);
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let field = &**array.children.add(0);
        assert!(!field.dictionary.is_null());
        let values = &*field.dictionary;
        assert_eq!(values.length, 3);
        assert_eq!(values.n_buffers, 2);
        assert!((*values.buffers.add(0)).is_null());
        assert_eq!(*values.buffers.add(1), values_ptr);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_low_cardinality_interval_formats_and_buffers() {
    let second_values = vec![0i64, 13, 79];
    let second_indices = vec![1i32, 2, 1];
    let month_values = vec![0i64, -13, 79];
    let month_indices = vec![1i32, 0, 2];
    let month_validity = Bitmap::from_ch_null_map(&[0, 1, 0]);

    let second_values_ptr = second_values.as_ptr() as *const c_void;
    let second_indices_ptr = second_indices.as_ptr() as *const c_void;
    let month_values_ptr = month_values.as_ptr() as *const c_void;
    let month_indices_ptr = month_indices.as_ptr() as *const c_void;
    let month_validity_ptr = month_validity.as_bytes().as_ptr() as *const c_void;

    let schema = Schema::new(vec![
        Field {
            name: "lc_second".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Interval(IntervalKind::Second))),
        },
        Field {
            name: "lc_month".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(
                ChType::Interval(IntervalKind::Month),
            )))),
        },
    ]);
    let columns = vec![
        Column::Dictionary(DictionaryColumn::new(
            second_indices,
            Column::Interval(PrimitiveColumn::new(second_values)),
        )),
        Column::Dictionary(DictionaryColumn::new_nullable(
            month_indices,
            Column::Interval(PrimitiveColumn::new(month_values)),
            month_validity,
        )),
    ];
    let batch = Arc::new(ColBatch::new(schema, columns, 3));

    // Safety: the zeroed FFI outputs are writable and the batch remains alive
    // until each matching release callback is invoked below.
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);

        let second_schema = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(second_schema.format).to_str().unwrap(), "i");
        assert_eq!(second_schema.flags, 0);
        assert!(!second_schema.dictionary.is_null());
        let second_dict_schema = &*second_schema.dictionary;
        assert_eq!(
            CStr::from_ptr(second_dict_schema.format).to_str().unwrap(),
            "tDs"
        );
        assert_eq!(second_dict_schema.flags, 0);

        let month_schema = &**schema_out.children.add(1);
        assert_eq!(CStr::from_ptr(month_schema.format).to_str().unwrap(), "i");
        assert_eq!(month_schema.flags, 2);
        assert!(!month_schema.dictionary.is_null());
        let month_dict_schema = &*month_schema.dictionary;
        assert_eq!(
            CStr::from_ptr(month_dict_schema.format).to_str().unwrap(),
            "l"
        );
        assert_eq!(month_dict_schema.flags, 0);
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);

        let second = &**array.children.add(0);
        assert_eq!(second.length, 3);
        assert_eq!(second.null_count, 0);
        assert_eq!(second.n_buffers, 2);
        assert!((*second.buffers.add(0)).is_null());
        assert_eq!(*second.buffers.add(1), second_indices_ptr);
        assert!(!second.dictionary.is_null());
        let second_dict = &*second.dictionary;
        assert_eq!(second_dict.length, 3);
        assert_eq!(second_dict.n_buffers, 2);
        assert!((*second_dict.buffers.add(0)).is_null());
        assert_eq!(*second_dict.buffers.add(1), second_values_ptr);

        let month = &**array.children.add(1);
        assert_eq!(month.length, 3);
        assert_eq!(month.null_count, 1);
        assert_eq!(month.n_buffers, 2);
        assert_eq!(*month.buffers.add(0), month_validity_ptr);
        assert_eq!(*month.buffers.add(1), month_indices_ptr);
        assert!(!month.dictionary.is_null());
        let month_dict = &*month.dictionary;
        assert_eq!(month_dict.length, 3);
        assert_eq!(month_dict.n_buffers, 2);
        assert!((*month_dict.buffers.add(0)).is_null());
        assert_eq!(*month_dict.buffers.add(1), month_values_ptr);

        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_low_cardinality_uuid_child_format() {
    use crate::column::{DictionaryColumn, FixedBinaryColumn};

    // LowCardinality(UUID) exports as dictionary(i32, w:16): field format `i`,
    // dictionary child format `w:16`.
    let schema = Schema::new(vec![Field {
        name: "lc".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::Uuid)),
    }]);
    let values = Column::Uuid(FixedBinaryColumn::new(vec![0u8; 48], 16)); // 3 entries
    let dict = DictionaryColumn::new(vec![1, 2, 1], values);
    let batch = Arc::new(ColBatch::new(schema, vec![Column::Dictionary(dict)], 3));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "i");
        assert!(!c0.dictionary.is_null(), "dictionary child present");
        let dict_schema = &*c0.dictionary;
        assert_eq!(CStr::from_ptr(dict_schema.format).to_str().unwrap(), "w:16");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let c0 = &**array.children.add(0);
        assert!(!c0.dictionary.is_null(), "dictionary child array present");
        let dict_array = &*c0.dictionary;
        assert_eq!(dict_array.length, 3, "dictionary holds 3 entries");
        assert_eq!(dict_array.n_buffers, 2, "fixed binary: validity, data");
        (array.release.unwrap())(&mut array);
    }
}
