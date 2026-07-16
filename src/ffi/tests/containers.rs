use super::*;

#[test]
fn test_arrow_format_array() {
    // Array(T) is an Arrow LargeList: `+L`. The element type is NOT in this
    // format string, it lives in the `item` child schema.
    assert_eq!(arrow_format(&ChType::Array(Box::new(ChType::Int32))), "+L");
    // Nesting does not change the top-level format string.
    assert_eq!(
        arrow_format(&ChType::Array(Box::new(ChType::Array(Box::new(
            ChType::Int32
        ))))),
        "+L"
    );
}

#[test]
fn test_export_array_int32_schema() {
    // Schema of Array(Int32): field format `+L`, flags clear (array level is
    // never nullable), one child named `item` with the element format `i`.
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    }]);

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&schema, &mut schema_out);

        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
        assert_eq!(c0.flags & 2, 0, "array level is not nullable");
        assert_eq!(c0.n_children, 1);
        assert!(c0.dictionary.is_null(), "array field has no dictionary");

        let item = &**c0.children.add(0);
        assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "i");
        assert_eq!(CStr::from_ptr(item.name).to_str().unwrap(), "item");
        assert_eq!(item.flags & 2, 0, "plain Int32 element is not nullable");

        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_array_int32_buffers() {
    // Array(Int32) over 3 rows including an empty row (row 1). LargeList:
    // 2 buffers (null validity, i64 offsets), 1 child holding the flattened
    // elements.
    // Rows: [[10,20,30], [], [40]] -> offsets [0,3,3,4], values [10,20,30,40].
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    }]);
    let values = Column::Int32(PrimitiveColumn::new(vec![10, 20, 30, 40]));
    let col = Column::Array(crate::column::ArrayColumn::new(vec![0, 3, 3, 4], values));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 3));

    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();

        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 3, "array length is the row count");
        assert_eq!(c0.null_count, 0);
        assert_eq!(c0.n_buffers, 2, "LargeList: validity + offsets");
        assert!(
            (*c0.buffers.add(0)).is_null(),
            "array level validity is always null"
        );
        let offsets = *c0.buffers.add(1) as *const i64;
        assert_eq!(*offsets, 0, "leading 0");
        assert_eq!(*offsets.add(1), 3);
        assert_eq!(*offsets.add(2), 3, "empty row -> repeated offset");
        assert_eq!(*offsets.add(3), 4, "last offset == total elements");

        assert_eq!(c0.n_children, 1);
        let item = &**c0.children.add(0);
        assert_eq!(item.length, 4, "element count == last offset");
        assert_eq!(item.n_buffers, 2, "int32 element: validity + values");
        let vals = *item.buffers.add(1) as *const i32;
        assert_eq!(*vals, 10);
        assert_eq!(*vals.add(3), 40);

        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_array_nullable_int32() {
    use crate::bitmap::Bitmap;

    // Array(Nullable(Int32)): the `item` child schema carries the nullable
    // flag, the element array's validity is non-null with a matching
    // null_count, and the array-level validity stays null.
    // Rows: [[10, null], [30]] -> offsets [0,2,3], values [10, _, 30] with
    // element index 1 null.
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::Int32)))),
    }]);
    // ClickHouse null map: 1 = null. Element index 1 is null.
    let validity = Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]);
    let values = Column::Int32(PrimitiveColumn::new_nullable(vec![10, 0, 30], validity));
    let col = Column::Array(crate::column::ArrayColumn::new(vec![0, 2, 3], values));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 2));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
        assert_eq!(c0.flags & 2, 0, "array level not nullable");
        let item = &**c0.children.add(0);
        assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "i");
        assert_eq!(item.flags & 2, 2, "nullable element flag set");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 2);
        assert_eq!(c0.null_count, 0, "array level has no nulls");
        assert!(
            (*c0.buffers.add(0)).is_null(),
            "array level validity stays null"
        );
        let item = &**c0.children.add(0);
        assert_eq!(item.length, 3);
        assert_eq!(item.null_count, 1, "one null element");
        assert!(
            !(*item.buffers.add(0)).is_null(),
            "element validity buffer present"
        );
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_array_low_cardinality_string() {
    use crate::column::DictionaryColumn;

    // Array(LowCardinality(String)): the `item` child schema is a dictionary
    // (format `i`) with a non-null dictionary child of format `u`; the
    // element array carries the index buffers plus a dictionary child.
    // Rows: [["user_1"], ["user_2","user_1"]] -> offsets [0,1,3].
    // Dictionary values ["user_1","user_2"], indices [0,1,0].
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
    }]);
    let dict_values = Column::Utf8(Utf8Column::new(vec![0, 6, 12], b"user_1user_2".to_vec()));
    let dict = DictionaryColumn::new(vec![0, 1, 0], dict_values);
    let col = Column::Array(crate::column::ArrayColumn::new(
        vec![0, 1, 3],
        Column::Dictionary(dict),
    ));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 2));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
        let item = &**c0.children.add(0);
        assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "i");
        assert!(!item.dictionary.is_null(), "dictionary child present");
        let dict_schema = &*item.dictionary;
        assert_eq!(CStr::from_ptr(dict_schema.format).to_str().unwrap(), "u");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 2);
        let item = &**c0.children.add(0);
        assert_eq!(item.length, 3, "3 flattened index rows");
        assert_eq!(item.n_buffers, 2, "dictionary: validity + i32 indices");
        let idx = *item.buffers.add(1) as *const i32;
        assert_eq!(*idx, 0);
        assert_eq!(*idx.add(1), 1);
        assert!(!item.dictionary.is_null(), "dictionary child array present");
        let dict_array = &*item.dictionary;
        assert_eq!(dict_array.length, 2, "2 dictionary entries");
        assert_eq!(
            dict_array.n_buffers, 3,
            "utf8 values: validity, offsets, data"
        );
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_array_of_array_int32() {
    // Array(Array(Int32)): the `item` child schema is itself `+L` with its own
    // `item` child of format `i`; nested child arrays have the right lengths.
    // Outer rows: [[[1,2],[3]], [[4]]] -> outer offsets [0,2,3].
    // Inner arrays (3 of them): offsets [0,2,3,4], values [1,2,3,4].
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
    }]);
    let inner_values = Column::Int32(PrimitiveColumn::new(vec![1, 2, 3, 4]));
    let inner = Column::Array(crate::column::ArrayColumn::new(
        vec![0, 2, 3, 4],
        inner_values,
    ));
    let outer = Column::Array(crate::column::ArrayColumn::new(vec![0, 2, 3], inner));
    let batch = Arc::new(ColBatch::new(schema, vec![outer], 2));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
        let item = &**c0.children.add(0);
        assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "+L");
        assert_eq!(item.n_children, 1);
        let leaf = &**item.children.add(0);
        assert_eq!(CStr::from_ptr(leaf.format).to_str().unwrap(), "i");
        assert_eq!(CStr::from_ptr(leaf.name).to_str().unwrap(), "item");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 2, "2 outer rows");
        let offsets = *c0.buffers.add(1) as *const i64;
        assert_eq!(*offsets, 0);
        assert_eq!(*offsets.add(2), 3, "3 inner arrays total");

        let item = &**c0.children.add(0);
        assert_eq!(item.length, 3, "3 inner arrays == outer last offset");
        let inner_offsets = *item.buffers.add(1) as *const i64;
        assert_eq!(*inner_offsets.add(3), 4, "4 leaf elements total");

        let leaf = &**item.children.add(0);
        assert_eq!(leaf.length, 4, "4 leaf int32 elements");
        let vals = *leaf.buffers.add(1) as *const i32;
        assert_eq!(*vals, 1);
        assert_eq!(*vals.add(3), 4);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_empty_array_column() {
    // A zero-row Array column has offsets == [0]: length 0, offsets buffer
    // still non-null with a single leading 0, and an empty element child.
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    }]);
    let values = Column::Int32(PrimitiveColumn::new(vec![]));
    let col = Column::Array(crate::column::ArrayColumn::new(vec![0], values));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 0));

    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 0);
        assert_eq!(c0.n_buffers, 2);
        assert!((*c0.buffers.add(0)).is_null());
        let offsets = *c0.buffers.add(1) as *const i64;
        assert_eq!(*offsets, 0, "the leading 0 for a zero-row column");
        let item = &**c0.children.add(0);
        assert_eq!(item.length, 0, "no elements");
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_release_array_skips_moved_out_child() {
    // Spec "Moving child arrays": a consumer may take ownership of a child
    // by bitwise-copying its struct and marking the SOURCE released
    // (release = None) WITHOUT calling the source's release callback, then
    // must release the parent. The parent's release must skip the moved
    // child while still freeing the producer-owned child shell, and the
    // moved copy must stay independently valid (it holds its own
    // Arc<ColBatch> in private data) until its own, idempotent release.
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    }]);
    let values = Column::Int32(PrimitiveColumn::new(vec![10, 20, 30, 40]));
    let col = Column::Array(crate::column::ArrayColumn::new(vec![0, 3, 3, 4], values));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 3));

    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        // Drop our own Arc so the export's private-data clones are the only
        // owners of the buffers from here on.
        drop(batch);

        let list_ptr = *array.children.add(0);
        let item_ptr = *(*list_ptr).children.add(0);

        // Consumer move: bitwise copy the item child out of the producer's
        // shell, then mark the source released without calling its
        // callback.
        let mut moved: ArrowArray = ptr::read(item_ptr);
        (*item_ptr).release = None;

        // Per spec the parent must be released after moving a child out.
        // Its release must skip the moved-out item (null release), freeing
        // only the producer-owned shell; the moved copy stays valid.
        (array.release.unwrap())(&mut array);
        assert!(array.release.is_none(), "parent marked released");

        // The moved child still owns its buffers through its own private
        // data: length and values remain readable after the parent (and
        // our Arc) are gone.
        assert_eq!(moved.length, 4);
        assert_eq!(moved.n_buffers, 2);
        let vals = *moved.buffers.add(1) as *const i32;
        assert_eq!(*vals, 10);
        assert_eq!(*vals.add(3), 40);

        // Releasing the moved copy frees its private data exactly once and
        // marks it released; a second call through the producer callback is
        // a no-op (private_data was cleared).
        (moved.release.unwrap())(&mut moved);
        assert!(moved.release.is_none(), "moved child marked released");
        release_array(&mut moved);
    }
}

#[test]
fn test_export_array_all_rows_empty_low_cardinality() {
    use crate::column::DictionaryColumn;

    // Rows but every array empty: offsets [0, 0, 0] over an empty element
    // column. This is the shape the decoder produces for the documented
    // wire quirk where a rows-but-all-empty `Array(LowCardinality(String))`
    // column carries no element body at all (see CODEC_CONTRACT.md). The
    // offsets buffer must still be non-null with num_rows + 1 entries, and
    // the item child (and its dictionary) must export with length 0.
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
    }]);
    let dict_values = Column::Utf8(Utf8Column::new(vec![0], Vec::new()));
    let dict = DictionaryColumn::new(Vec::new(), dict_values);
    let col = Column::Array(crate::column::ArrayColumn::new(
        vec![0, 0, 0],
        Column::Dictionary(dict),
    ));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 2));

    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 2, "2 rows, all empty");
        assert_eq!(c0.null_count, 0);
        assert_eq!(c0.n_buffers, 2);
        assert!((*c0.buffers.add(0)).is_null());
        let offsets_ptr = *c0.buffers.add(1);
        assert!(!offsets_ptr.is_null(), "offsets buffer stays non-null");
        let offsets = offsets_ptr as *const i64;
        assert_eq!(*offsets, 0);
        assert_eq!(*offsets.add(1), 0);
        assert_eq!(*offsets.add(2), 0, "num_rows + 1 all-zero offsets");

        assert_eq!(c0.n_children, 1);
        let item = &**c0.children.add(0);
        assert_eq!(item.length, 0, "no flattened elements");
        assert_eq!(item.null_count, 0);
        assert!(!item.dictionary.is_null(), "dictionary child still present");
        let dict_array = &*item.dictionary;
        assert_eq!(dict_array.length, 0, "empty dictionary");
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_arrow_format_tuple() {
    // Tuple exports as an Arrow struct (`+s`); the element types live in
    // the child schemas, never in the format string. Nullable(Tuple)
    // recurses to the same format.
    assert_eq!(
        arrow_format(&ChType::Tuple(vec![
            (None, ChType::Int32),
            (None, ChType::String),
        ])),
        "+s"
    );
    assert_eq!(arrow_format(&ChType::Tuple(vec![])), "+s");
    assert_eq!(
        arrow_format(&ChType::Nullable(Box::new(ChType::Tuple(vec![(
            None,
            ChType::Int32,
        )])))),
        "+s"
    );
}

#[test]
fn test_export_tuple_schema() {
    // Schema of a named Tuple(a Int32, b Nullable(String)) and an unnamed
    // Tuple(Int32, String): format `+s`, one child per element. Named
    // elements keep their ClickHouse names verbatim; unnamed elements are
    // named by 1-based position. A Nullable element carries its own
    // nullable flag on the child.
    let schema = Schema::new(vec![
        Field {
            name: "tn".into(),
            ch_type: ChType::Tuple(vec![
                (Some("a".to_string()), ChType::Int32),
                (
                    Some("b".to_string()),
                    ChType::Nullable(Box::new(ChType::String)),
                ),
            ]),
        },
        Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        },
        Field {
            name: "t0".into(),
            ch_type: ChType::Tuple(vec![]),
        },
    ]);

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&schema, &mut schema_out);

        let tn = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(tn.format).to_str().unwrap(), "+s");
        assert_eq!(tn.flags & 2, 0, "plain tuple is not nullable");
        assert_eq!(tn.n_children, 2);
        assert!(tn.dictionary.is_null());
        let a = &**tn.children.add(0);
        assert_eq!(CStr::from_ptr(a.name).to_str().unwrap(), "a");
        assert_eq!(CStr::from_ptr(a.format).to_str().unwrap(), "i");
        let b = &**tn.children.add(1);
        assert_eq!(CStr::from_ptr(b.name).to_str().unwrap(), "b");
        assert_eq!(CStr::from_ptr(b.format).to_str().unwrap(), "u");
        assert_eq!(b.flags & 2, 2, "Nullable element child is nullable");

        let t = &**schema_out.children.add(1);
        assert_eq!(t.n_children, 2);
        let e1 = &**t.children.add(0);
        assert_eq!(CStr::from_ptr(e1.name).to_str().unwrap(), "1");
        let e2 = &**t.children.add(1);
        assert_eq!(CStr::from_ptr(e2.name).to_str().unwrap(), "2");

        let t0 = &**schema_out.children.add(2);
        assert_eq!(CStr::from_ptr(t0.format).to_str().unwrap(), "+s");
        assert_eq!(t0.n_children, 0, "Tuple() exports with no children");

        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_tuple_buffers() {
    use crate::column::TupleColumn;

    // Tuple(Int32, String) over 2 rows: struct node with 1 buffer (null
    // validity slot, null_count 0), 2 children exported recursively; and a
    // zero-element Tuple() whose node still carries its explicit length.
    let schema = Schema::new(vec![
        Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        },
        Field {
            name: "t0".into(),
            ch_type: ChType::Tuple(vec![]),
        },
    ]);
    let columns = vec![
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![13, 79])),
                Column::Utf8(Utf8Column::new(vec![0, 2, 2], b"hi".to_vec())),
            ],
            2,
        )),
        Column::Tuple(TupleColumn::new(vec![], 2)),
    ];
    let batch = Arc::new(ColBatch::new(schema, columns, 2));

    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();

        let t = &**array.children.add(0);
        assert_eq!(t.length, 2);
        assert_eq!(t.null_count, 0);
        assert_eq!(t.n_buffers, 1, "struct: validity slot only");
        assert!((*t.buffers.add(0)).is_null(), "plain tuple validity null");
        assert_eq!(t.n_children, 2);
        assert!(t.dictionary.is_null());
        let e1 = &**t.children.add(0);
        assert_eq!(e1.length, 2);
        let vals = *e1.buffers.add(1) as *const i32;
        assert_eq!(*vals, 13);
        assert_eq!(*vals.add(1), 79);
        let e2 = &**t.children.add(1);
        assert_eq!(e2.length, 2);
        assert_eq!(e2.n_buffers, 3, "utf8 element: validity, offsets, data");

        let t0 = &**array.children.add(1);
        assert_eq!(t0.length, 2, "Tuple() length from the explicit len");
        assert_eq!(t0.n_buffers, 1);
        assert_eq!(t0.n_children, 0);

        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_nullable_tuple() {
    use crate::bitmap::Bitmap;
    use crate::column::TupleColumn;

    // Nullable(Tuple(Int32)): the struct node carries the nullable flag on
    // the schema and the validity bitmap in buffers[0] with a matching
    // null_count; children stay independent per the C Data spec.
    let schema = Schema::new(vec![Field {
        name: "nt".into(),
        ch_type: ChType::Nullable(Box::new(ChType::Tuple(vec![(None, ChType::Int32)]))),
    }]);
    let columns = vec![Column::Tuple(TupleColumn::new_nullable(
        vec![Column::Int32(PrimitiveColumn::new(vec![13, 0, 79]))],
        3,
        Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]),
    ))];
    let batch = Arc::new(ColBatch::new(schema, columns, 3));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+s");
        assert_eq!(c0.flags & 2, 2, "Nullable(Tuple) sets the nullable flag");
        assert_eq!(c0.n_children, 1, "element children still described");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 3);
        assert_eq!(c0.null_count, 1);
        assert_eq!(c0.n_buffers, 1);
        assert!(
            !(*c0.buffers.add(0)).is_null(),
            "tuple-level validity bitmap present in buffers[0]"
        );
        let child = &**c0.children.add(0);
        assert_eq!(child.length, 3, "children carry placeholders for nulls");
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_arrow_format_map() {
    // Map exports as LargeList-of-struct (`+L`), never `+m` (whose i32
    // offsets would force a copy of the i64 offset buffer).
    assert_eq!(
        arrow_format(&ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Int32),
        )),
        "+L"
    );
}

#[test]
fn test_export_map_schema() {
    // Schema of Map(String, Nullable(Int32)): field format `+L`, flags 0
    // (maps are never nullable at the map level), one child named
    // "entries" (a non-nullable struct), with grandchildren "key" (flags 0)
    // and "value" (nullable flag per the value type). No
    // ARROW_FLAG_MAP_KEYS_SORTED anywhere (it is +m-only).
    let schema = Schema::new(vec![Field {
        name: "m".into(),
        ch_type: ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Nullable(Box::new(ChType::Int32))),
        ),
    }]);

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&schema, &mut schema_out);

        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
        assert_eq!(c0.flags, 0, "map level is not nullable, no sorted-keys");
        assert_eq!(c0.n_children, 1);
        assert!(c0.dictionary.is_null());

        let entries = &**c0.children.add(0);
        assert_eq!(CStr::from_ptr(entries.name).to_str().unwrap(), "entries");
        assert_eq!(CStr::from_ptr(entries.format).to_str().unwrap(), "+s");
        assert_eq!(entries.flags, 0, "entries struct is non-nullable");
        assert_eq!(entries.n_children, 2);

        let key = &**entries.children.add(0);
        assert_eq!(CStr::from_ptr(key.name).to_str().unwrap(), "key");
        assert_eq!(CStr::from_ptr(key.format).to_str().unwrap(), "u");
        assert_eq!(key.flags & 2, 0, "keys are never nullable");
        let value = &**entries.children.add(1);
        assert_eq!(CStr::from_ptr(value.name).to_str().unwrap(), "value");
        assert_eq!(CStr::from_ptr(value.format).to_str().unwrap(), "i");
        assert_eq!(value.flags & 2, 2, "Nullable value child is nullable");

        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_map_lc_key_schema() {
    // Map(LowCardinality(String), UInt8): the key grandchild is a
    // dictionary field (index format `i`, values in the dictionary child),
    // composing through the entries struct exactly like a top-level LC.
    let schema = Schema::new(vec![Field {
        name: "m".into(),
        ch_type: ChType::Map(
            Box::new(ChType::LowCardinality(Box::new(ChType::String))),
            Box::new(ChType::UInt8),
        ),
    }]);

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        let entries = &**c0.children.add(0);
        let key = &**entries.children.add(0);
        assert_eq!(CStr::from_ptr(key.format).to_str().unwrap(), "i");
        assert!(!key.dictionary.is_null(), "LC key has a dictionary child");
        let dict = &*key.dictionary;
        assert_eq!(CStr::from_ptr(dict.format).to_str().unwrap(), "u");
        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_map_buffers() {
    use crate::column::{MapColumn, TupleColumn};

    // Map(String, Int32) over 3 rows including an empty row: the map node
    // is byte-identical in shape to an Array node (2 buffers: null
    // validity, i64 offsets), with the entries struct as the single child
    // and the key/value columns as its children.
    // Rows: {a: 13} / {} / {b: 1, c: 2} -> offsets [0, 1, 1, 3].
    let schema = Schema::new(vec![Field {
        name: "m".into(),
        ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
    }]);
    let entries = Column::Tuple(TupleColumn::new(
        vec![
            Column::Utf8(Utf8Column::new(vec![0, 1, 2, 3], b"abc".to_vec())),
            Column::Int32(PrimitiveColumn::new(vec![13, 1, 2])),
        ],
        3,
    ));
    let col = Column::Map(MapColumn::new(vec![0, 1, 1, 3], entries));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 3));

    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();

        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 3, "map length is the row count");
        assert_eq!(c0.null_count, 0);
        assert_eq!(c0.n_buffers, 2, "LargeList shape: validity + offsets");
        assert!((*c0.buffers.add(0)).is_null(), "map validity always null");
        let offsets = *c0.buffers.add(1) as *const i64;
        assert_eq!(*offsets, 0, "leading 0");
        assert_eq!(*offsets.add(2), 1, "empty row -> repeated offset");
        assert_eq!(*offsets.add(3), 3, "last offset == total entries");

        assert_eq!(c0.n_children, 1);
        let entries = &**c0.children.add(0);
        assert_eq!(entries.length, 3, "entry count == last offset");
        assert_eq!(entries.null_count, 0);
        assert_eq!(entries.n_buffers, 1, "struct: validity slot only");
        assert!((*entries.buffers.add(0)).is_null());
        assert_eq!(entries.n_children, 2);
        let key = &**entries.children.add(0);
        assert_eq!(key.length, 3);
        assert_eq!(key.n_buffers, 3, "utf8 keys: validity, offsets, data");
        let value = &**entries.children.add(1);
        assert_eq!(value.length, 3);
        let vals = *value.buffers.add(1) as *const i32;
        assert_eq!(*vals, 13);
        assert_eq!(*vals.add(2), 2);

        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn export_empty_offsets_array_has_valid_leading_zero_offset() {
    // A hand-built Array column with an empty offsets Vec is a zero-row LargeList
    // (ArrayColumn::len is offsets.len().saturating_sub(1)). The decoder always
    // emits [0], but Column fields are public, so the export must not hand out
    // the dangling as_ptr of a zero-capacity Vec; it substitutes a 'static
    // single zero so the i64 offsets buffer keeps Arrow's length + 1 contract.
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    }]);
    let values = Column::Int32(PrimitiveColumn::new(vec![]));
    let col = Column::Array(crate::column::ArrayColumn::new(vec![], values));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 0));

    // Safety: output is a writable zeroed C Data array, released below.
    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 0);
        assert_eq!(c0.n_buffers, 2);
        let offsets = *c0.buffers.add(1) as *const i64;
        assert!(!offsets.is_null());
        assert_eq!(*offsets, 0);
        (array.release.unwrap())(&mut array);
    }
}
