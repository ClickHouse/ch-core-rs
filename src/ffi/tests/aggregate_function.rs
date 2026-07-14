use super::*;

fn aggregate_batch() -> Arc<ColBatch> {
    Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "c".into(),
            ch_type: ChType::AggregateFunction {
                function: "count".into(),
                arguments: vec![],
            },
        }]),
        vec![Column::AggregateState(AggregateStateColumn::new(
            vec![0, 1, 3, 4],
            vec![0x0d, 0x80, 0x01, 0x4f],
        ))],
        3,
    ))
}

#[test]
fn export_count_state_as_large_binary_zero_copy() {
    let batch = aggregate_batch();

    // Safety: both outputs are writable zeroed C Data structs. The batch owns
    // the offsets and state data until both release callbacks run.
    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema);
        let field = &**schema.children.add(0);
        assert_eq!(CStr::from_ptr(field.format).to_str().unwrap(), "Z");
        assert_eq!(field.flags, 0);
        assert_eq!(field.n_children, 0);
        assert!(field.dictionary.is_null());
        (schema.release.unwrap())(&mut schema);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let child = &**array.children.add(0);
        assert_eq!(child.length, 3);
        assert_eq!(child.null_count, 0);
        assert_eq!(child.n_buffers, 3);
        assert!((*child.buffers.add(0)).is_null());

        let offsets = *child.buffers.add(1) as *const i64;
        assert_eq!(
            offsets,
            match batch.column(0) {
                Column::AggregateState(c) => c.offsets.as_ptr(),
                other => panic!("expected AggregateState, got {other:?}"),
            }
        );
        assert_eq!(*offsets.add(0), 0);
        assert_eq!(*offsets.add(3), 4);

        let data = *child.buffers.add(2) as *const u8;
        assert_eq!(
            data,
            match batch.column(0) {
                Column::AggregateState(c) => c.data.as_ptr(),
                other => panic!("expected AggregateState, got {other:?}"),
            }
        );
        assert_eq!(
            std::slice::from_raw_parts(data, 4),
            &[0x0d, 0x80, 0x01, 0x4f]
        );
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn export_nothing_uint64_state_as_large_binary_zero_copy() {
    // nothingUInt64 decodes into the same AggregateStateColumn/LargeBinary shape
    // as count, so the export needs no type-specific handling: the states are one
    // 0x00 byte each and export zero-copy under format `Z`.
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "c".into(),
            ch_type: ChType::AggregateFunction {
                function: "nothingUInt64".into(),
                arguments: vec![ChType::Nullable(Box::new(ChType::Nothing))],
            },
        }]),
        vec![Column::AggregateState(AggregateStateColumn::new(
            vec![0, 1, 2, 3],
            vec![0x00, 0x00, 0x00],
        ))],
        3,
    ));

    // Safety: both outputs are writable zeroed C Data structs. The batch owns the
    // offsets and state data until both release callbacks run.
    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema);
        let field = &**schema.children.add(0);
        assert_eq!(CStr::from_ptr(field.format).to_str().unwrap(), "Z");
        (schema.release.unwrap())(&mut schema);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let child = &**array.children.add(0);
        assert_eq!(child.length, 3);
        assert_eq!(child.null_count, 0);
        assert_eq!(child.n_buffers, 3);
        assert!((*child.buffers.add(0)).is_null());
        let offsets = *child.buffers.add(1) as *const i64;
        assert_eq!(*offsets.add(0), 0);
        assert_eq!(*offsets.add(3), 3);
        let data = *child.buffers.add(2) as *const u8;
        assert_eq!(std::slice::from_raw_parts(data, 3), &[0x00, 0x00, 0x00]);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn export_sum_state_as_large_binary_zero_copy() {
    // Fixed-width sum states use the same generic LargeBinary export. The
    // logical type retains the accumulator contract while Arrow receives exact
    // row slices without converting them into host numeric values.
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "s".into(),
            ch_type: ChType::AggregateFunction {
                function: "sum".into(),
                arguments: vec![ChType::UInt128],
            },
        }]),
        vec![Column::AggregateState(AggregateStateColumn::new(
            vec![0, 16, 32],
            vec![0x0d; 32],
        ))],
        2,
    ));

    // Safety: both outputs are writable zeroed C Data structs. The batch owns
    // the offsets and state data until both release callbacks run.
    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema);
        let field = &**schema.children.add(0);
        assert_eq!(CStr::from_ptr(field.format).to_str().unwrap(), "Z");
        (schema.release.unwrap())(&mut schema);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let child = &**array.children.add(0);
        assert_eq!(child.length, 2);
        assert_eq!(child.null_count, 0);
        assert_eq!(child.n_buffers, 3);
        assert!((*child.buffers.add(0)).is_null());
        let offsets = *child.buffers.add(1) as *const i64;
        let data = *child.buffers.add(2) as *const u8;
        match batch.column(0) {
            Column::AggregateState(c) => {
                assert_eq!(offsets, c.offsets.as_ptr());
                assert_eq!(data, c.data.as_ptr());
            }
            other => panic!("expected AggregateState, got {other:?}"),
        }
        assert_eq!(*offsets.add(2), 32);
        assert_eq!(std::slice::from_raw_parts(data, 32), &[0x0d; 32]);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn export_zero_row_count_state_has_large_binary_offsets() {
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "c".into(),
            ch_type: ChType::AggregateFunction {
                function: "count".into(),
                arguments: vec![ChType::UInt64],
            },
        }]),
        vec![Column::AggregateState(AggregateStateColumn::new(
            vec![0],
            vec![],
        ))],
        0,
    ));

    // Safety: output is a writable zeroed C Data array and is released below.
    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let child = &**array.children.add(0);
        assert_eq!(child.length, 0);
        assert_eq!(child.n_buffers, 3);
        assert_eq!(*(*child.buffers.add(1) as *const i64), 0);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn export_empty_offsets_aggregate_state_has_valid_leading_zero_offset() {
    // A hand-built AggregateState column with an empty offsets Vec is a zero-row
    // column. The decoder always emits [0], but Column fields are public, so the
    // export must not hand out the dangling as_ptr of a zero-capacity Vec; it
    // substitutes a 'static single zero so the i64 LargeBinary offsets buffer
    // satisfies Arrow's length + 1 contract instead of lacking the leading 0.
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "c".into(),
            ch_type: ChType::AggregateFunction {
                function: "count".into(),
                arguments: vec![],
            },
        }]),
        vec![Column::AggregateState(AggregateStateColumn::new(
            vec![],
            vec![],
        ))],
        0,
    ));

    // Safety: output is a writable zeroed C Data array, released below.
    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let child = &**array.children.add(0);
        assert_eq!(child.length, 0);
        assert_eq!(child.null_count, 0);
        assert_eq!(child.n_buffers, 3);
        let offsets = *child.buffers.add(1) as *const i64;
        assert!(!offsets.is_null());
        assert_eq!(*offsets, 0);
        (array.release.unwrap())(&mut array);
    }
}
