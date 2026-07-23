use super::*;

fn qbit_type(element_type: QBitElementType, dimension: usize) -> ChType {
    ChType::QBit {
        element_type,
        dimension,
    }
}

#[test]
fn qbit_schema_is_fixed_size_list_with_one_scalar_child() {
    let schema = Schema::new(vec![
        Field {
            name: "qb".into(),
            ch_type: qbit_type(QBitElementType::BFloat16, 3),
        },
        Field {
            name: "qf".into(),
            ch_type: ChType::Nullable(Box::new(qbit_type(QBitElementType::Float32, 9))),
        },
        Field {
            name: "qd".into(),
            ch_type: qbit_type(QBitElementType::Float64, 2),
        },
    ]);

    // Safety: the zeroed output is writable and released exactly once below.
    unsafe {
        let mut out: ArrowSchema = std::mem::zeroed();
        export_schema(&schema, &mut out);
        let expected = [("+w:3", "w:2", 0), ("+w:9", "f", 2), ("+w:2", "g", 0)];
        for (index, (parent_format, child_format, flags)) in expected.into_iter().enumerate() {
            let parent = &**out.children.add(index);
            assert_eq!(
                CStr::from_ptr(parent.format).to_str().unwrap(),
                parent_format
            );
            assert_eq!(parent.flags, flags);
            assert_eq!(parent.n_children, 1);
            assert!(parent.dictionary.is_null());
            let child = &**parent.children;
            assert_eq!(CStr::from_ptr(child.name).to_str().unwrap(), "item");
            assert_eq!(CStr::from_ptr(child.format).to_str().unwrap(), child_format);
            assert_eq!(child.flags, 0);
            assert_eq!(child.n_children, 0);
        }
        (out.release.unwrap())(&mut out);
    }
}

#[test]
fn qbit_array_borrows_row_major_child_zero_copy() {
    let values = vec![1.25f32, -0.0, 13.0, 79.0, -2.5, f32::INFINITY];
    let values_ptr = values.as_ptr() as *const c_void;
    let validity = Bitmap::from_ch_null_map(&[0, 1]);
    let validity_ptr = validity.as_bytes().as_ptr() as *const c_void;
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "q".into(),
            ch_type: ChType::Nullable(Box::new(qbit_type(QBitElementType::Float32, 3))),
        }]),
        vec![Column::QBit(QBitColumn::new_nullable(
            Column::Float32(PrimitiveColumn::new(values)),
            3,
            validity,
        ))],
        2,
    ));

    // Safety: the zeroed output is writable and the Arc-backed batch outlives
    // the exported array until its release callback is invoked.
    unsafe {
        let mut out: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut out).unwrap();
        let parent = &**out.children;
        assert_eq!(parent.length, 2);
        assert_eq!(parent.null_count, 1);
        assert_eq!(parent.n_buffers, 1);
        assert_eq!(*parent.buffers, validity_ptr);
        assert_eq!(parent.n_children, 1);

        let child = &**parent.children;
        assert_eq!(child.length, 6);
        assert_eq!(child.null_count, 0);
        assert_eq!(child.n_buffers, 2);
        assert!((*child.buffers).is_null());
        assert_eq!(*child.buffers.add(1), values_ptr);
        assert_eq!(child.n_children, 0);
        (out.release.unwrap())(&mut out);
    }
}

#[test]
fn qbit_bfloat16_and_zero_row_arrays_keep_child_shape() {
    let raw = vec![[0xc0, 0x3f], [0x20, 0xc0], [0x50, 0x41]];
    let raw_ptr = raw.as_ptr() as *const c_void;
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "q".into(),
            ch_type: qbit_type(QBitElementType::BFloat16, 3),
        }]),
        vec![Column::QBit(QBitColumn::new(
            Column::BFloat16(PrimitiveColumn::new(raw)),
            3,
        ))],
        1,
    ));

    // Safety: each zeroed output is writable and released once while its batch
    // remains alive.
    unsafe {
        let mut out: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut out).unwrap();
        let parent = &**out.children;
        assert_eq!(parent.n_buffers, 1);
        assert!((*parent.buffers).is_null());
        let child = &**parent.children;
        assert_eq!(child.length, 3);
        assert_eq!(*child.buffers.add(1), raw_ptr);
        (out.release.unwrap())(&mut out);

        let empty_batch = Arc::new(ColBatch::new(
            Schema::new(vec![Field {
                name: "q".into(),
                ch_type: qbit_type(QBitElementType::BFloat16, 9),
            }]),
            vec![Column::QBit(QBitColumn::new(
                Column::BFloat16(PrimitiveColumn::new(vec![])),
                9,
            ))],
            0,
        ));
        let mut empty: ArrowArray = std::mem::zeroed();
        export_batch_array(&empty_batch, &mut empty).unwrap();
        let parent = &**empty.children;
        assert_eq!(parent.length, 0);
        assert_eq!(parent.n_buffers, 1);
        assert_eq!(parent.n_children, 1);
        let child = &**parent.children;
        assert_eq!(child.length, 0);
        assert_eq!(child.n_buffers, 2);
        (empty.release.unwrap())(&mut empty);
    }
}

#[test]
fn qbit_arrow_stream_schema_and_array_keep_fixed_size_child() {
    let schema = Schema::new(vec![Field {
        name: "q".into(),
        ch_type: qbit_type(QBitElementType::Float64, 2),
    }]);
    let batch = Arc::new(ColBatch::new(
        schema.clone(),
        vec![Column::QBit(QBitColumn::new(
            Column::Float64(PrimitiveColumn::new(vec![13.0, -1.25, 79.0, 0.5])),
            2,
        ))],
        2,
    ));

    // Safety: every Arrow output starts zeroed and is released exactly once.
    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![batch], &mut stream);

        let mut schema_out: ArrowSchema = std::mem::zeroed();
        assert_eq!(
            (stream.get_schema.unwrap())(&mut stream, &mut schema_out),
            0
        );
        let field = &**schema_out.children;
        assert_eq!(CStr::from_ptr(field.format).to_str().unwrap(), "+w:2");
        assert_eq!(field.n_children, 1);
        assert_eq!(
            CStr::from_ptr((**field.children).format).to_str().unwrap(),
            "g"
        );

        let mut array: ArrowArray = std::mem::zeroed();
        assert_eq!((stream.get_next.unwrap())(&mut stream, &mut array), 0);
        let qbit = &**array.children;
        assert_eq!(qbit.length, 2);
        assert_eq!(qbit.n_buffers, 1);
        assert_eq!(qbit.n_children, 1);
        assert_eq!((**qbit.children).length, 4);

        (array.release.unwrap())(&mut array);
        (schema_out.release.unwrap())(&mut schema_out);
        (stream.release.unwrap())(&mut stream);
    }
}
