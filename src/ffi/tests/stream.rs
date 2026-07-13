use super::*;

#[test]
fn test_stream_yields_one_batch_then_ends() {
    let batch = make_test_batch();
    let schema = batch.schema.clone();
    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![batch], &mut stream);

        let mut schema: ArrowSchema = std::mem::zeroed();
        let rc = (stream.get_schema.unwrap())(&mut stream, &mut schema);
        assert_eq!(rc, 0);
        (schema.release.unwrap())(&mut schema);

        let mut array: ArrowArray = std::mem::zeroed();
        let rc = (stream.get_next.unwrap())(&mut stream, &mut array);
        assert_eq!(rc, 0);
        assert!(array.release.is_some());
        assert_eq!(array.length, 2);
        (array.release.unwrap())(&mut array);

        let mut array2: ArrowArray = std::mem::zeroed();
        let rc = (stream.get_next.unwrap())(&mut stream, &mut array2);
        assert_eq!(rc, 0);
        assert!(array2.release.is_none());

        (stream.release.unwrap())(&mut stream);
    }
}

#[test]
fn test_stream_release_if_set_is_idempotent() {
    let batch = make_test_batch();
    let schema = batch.schema.clone();
    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![batch], &mut stream);

        stream.release_if_set();
        assert!(stream.release.is_none());
        assert!(stream.private_data.is_null());

        // Second call sees a cleared callback and does nothing.
        stream.release_if_set();
    }
}

#[test]
fn test_stream_yields_multiple_chunks() {
    // Three separate chunks must come out as three record batches.
    let schema = make_test_batch().schema.clone();
    let chunks = vec![make_test_batch(), make_test_batch(), make_test_batch()];
    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, chunks, &mut stream);

        let mut count = 0;
        loop {
            let mut array: ArrowArray = std::mem::zeroed();
            let rc = (stream.get_next.unwrap())(&mut stream, &mut array);
            assert_eq!(rc, 0);
            if array.release.is_none() {
                break;
            }
            assert_eq!(array.length, 2);
            count += 1;
            (array.release.unwrap())(&mut array);
        }
        assert_eq!(count, 3);

        (stream.release.unwrap())(&mut stream);
    }
}

#[test]
fn test_stream_with_array_column() {
    // A stream whose batch carries an Array(Array(Int32)) column: the first
    // exported type with real `children`, so `get_schema` and `get_next`
    // must agree on the child shape all the way down.
    // One row: [[[7], [9, 11]]] -> outer offsets [0, 2], inner offsets
    // [0, 1, 3], leaf values [7, 9, 11].
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
    }]);
    let leaf = Column::Int32(PrimitiveColumn::new(vec![7, 9, 11]));
    let inner = Column::Array(crate::column::ArrayColumn::new(vec![0, 1, 3], leaf));
    let outer = Column::Array(crate::column::ArrayColumn::new(vec![0, 2], inner));
    let batch = Arc::new(ColBatch::new(schema.clone(), vec![outer], 1));

    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![batch], &mut stream);

        let mut schema_out: ArrowSchema = std::mem::zeroed();
        let rc = (stream.get_schema.unwrap())(&mut stream, &mut schema_out);
        assert_eq!(rc, 0);
        let s0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(s0.format).to_str().unwrap(), "+L");
        assert_eq!(s0.n_children, 1);
        let s_item = &**s0.children.add(0);
        assert_eq!(CStr::from_ptr(s_item.format).to_str().unwrap(), "+L");
        assert_eq!(s_item.n_children, 1);
        let s_leaf = &**s_item.children.add(0);
        assert_eq!(CStr::from_ptr(s_leaf.format).to_str().unwrap(), "i");

        let mut array: ArrowArray = std::mem::zeroed();
        let rc = (stream.get_next.unwrap())(&mut stream, &mut array);
        assert_eq!(rc, 0);
        assert!(array.release.is_some());
        assert_eq!(array.n_children, schema_out.n_children);
        let a0 = &**array.children.add(0);
        assert_eq!(a0.length, 1, "1 outer row");
        assert_eq!(a0.n_children, s0.n_children);
        let a_item = &**a0.children.add(0);
        assert_eq!(a_item.length, 2, "outer last offset");
        assert_eq!(a_item.n_children, s_item.n_children);
        let a_leaf = &**a_item.children.add(0);
        assert_eq!(a_leaf.length, 3, "inner last offset");
        let vals = *a_leaf.buffers.add(1) as *const i32;
        assert_eq!(*vals, 7);
        assert_eq!(*vals.add(2), 11);
        (array.release.unwrap())(&mut array);
        (schema_out.release.unwrap())(&mut schema_out);

        // End of stream after the single chunk.
        let mut array2: ArrowArray = std::mem::zeroed();
        let rc = (stream.get_next.unwrap())(&mut stream, &mut array2);
        assert_eq!(rc, 0);
        assert!(array2.release.is_none());

        (stream.release.unwrap())(&mut stream);
    }
}
