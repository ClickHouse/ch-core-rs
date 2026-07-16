use super::*;
use crate::column::{JsonBody, JsonColumn, StructuredJson};
use crate::schema::{JSON_DEFAULT_MAX_DYNAMIC_PATHS, JSON_DEFAULT_MAX_DYNAMIC_TYPES};

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

/// A `JSON` type with the declared typed paths and defaults for everything else.
fn json_type(typed_paths: Vec<(String, ChType)>) -> ChType {
    ChType::Json {
        max_dynamic_paths: JSON_DEFAULT_MAX_DYNAMIC_PATHS,
        max_dynamic_types: JSON_DEFAULT_MAX_DYNAMIC_TYPES,
        typed_paths,
        skip_paths: Vec::new(),
        skip_regexps: Vec::new(),
    }
}

/// An empty shared-data triple for a `len`-row block: no `(path, value)` pairs.
fn empty_shared(len: usize) -> (Vec<i64>, Utf8Column, Utf8Column) {
    (
        vec![0i64; len + 1],
        Utf8Column::new(vec![0], Vec::new()),
        Utf8Column::new(vec![0], Vec::new()),
    )
}

/// One `Dynamic` column of `len` rows carrying a single typed child; ids route
/// each row either to child 0 or to NULL (`u32::MAX`).
fn single_child_dynamic(ch_type: ChType, values: Column, type_ids: &[u32]) -> DynamicColumn {
    DynamicColumn::try_new(type_ids, vec![DynamicChild::Typed { ch_type, values }]).unwrap()
}

fn structured_of(batch: &ColBatch) -> &StructuredJson {
    match &batch.columns[0] {
        Column::Json(column) => match column.body() {
            JsonBody::Structured(structured) => structured,
            JsonBody::Text(_) => panic!("expected a structured JSON body"),
        },
        other => panic!("expected a JSON column, got {other:?}"),
    }
}

fn cstr<'a>(ptr: *const std::os::raw::c_char) -> &'a str {
    unsafe { CStr::from_ptr(ptr).to_str().unwrap() }
}

// ---------------------------------------------------------------------------
// Structured schema and array shape
// ---------------------------------------------------------------------------

#[test]
fn structured_schema_and_array_shape() {
    let (offsets, paths, values) = empty_shared(2);
    let structured = StructuredJson::try_new(
        vec![
            (
                "a".to_string(),
                Column::Int64(PrimitiveColumn::new(vec![10, 20])),
            ),
            (
                "b".to_string(),
                Column::Utf8(Utf8Column::new(vec![0, 6, 12], b"user_1user_2".to_vec())),
            ),
        ],
        vec![
            (
                "c".to_string(),
                single_child_dynamic(
                    ChType::Int64,
                    Column::Int64(PrimitiveColumn::new(vec![13])),
                    &[0, u32::MAX],
                ),
            ),
            (
                "d".to_string(),
                single_child_dynamic(
                    ChType::String,
                    Column::Utf8(Utf8Column::new(vec![0, 5], b"world".to_vec())),
                    &[u32::MAX, 0],
                ),
            ),
        ],
        offsets,
        paths,
        values,
        2,
    )
    .unwrap();
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: json_type(vec![
                ("a".to_string(), ChType::Int64),
                ("b".to_string(), ChType::String),
            ]),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        2,
    ));

    // Zero-copy references captured from the owned batch; the Vec heap does not
    // move when the batch is exported.
    let int_ptr = match &structured_of(&batch).typed[0].1 {
        Column::Int64(c) => c.values.as_ptr(),
        _ => unreachable!(),
    };

    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch(&batch, &mut schema, &mut array).unwrap();

        // Top JSON field: nullable struct with 2 typed + 2 dynamic + shared.
        let field = &**schema.children.add(0);
        assert_eq!(cstr(field.format), "+s");
        assert_eq!(field.flags & 2, 2, "JSON is nullable-flagged");
        assert_eq!(field.n_children, 5);
        assert_eq!(cstr((**field.children.add(0)).name), "a");
        assert_eq!(cstr((**field.children.add(0)).format), "l");
        assert_eq!(cstr((**field.children.add(1)).name), "b");
        assert_eq!(cstr((**field.children.add(1)).format), "u");
        assert_eq!(cstr((**field.children.add(2)).name), "c");
        assert_eq!(cstr((**field.children.add(2)).format), "+ud:0,1");
        assert_eq!(cstr((**field.children.add(3)).name), "d");
        assert_eq!(cstr((**field.children.add(3)).format), "+ud:0,1");

        // _shared_data: LargeList of a non-nullable (paths, values) struct.
        let shared = &**field.children.add(4);
        assert_eq!(cstr(shared.name), "_shared_data");
        assert_eq!(cstr(shared.format), "+L");
        assert_eq!(shared.flags & 2, 0, "shared list is not nullable");
        assert_eq!(shared.n_children, 1);
        let item = &**shared.children.add(0);
        assert_eq!(cstr(item.format), "+s");
        assert_eq!(item.n_children, 2);
        assert_eq!(cstr((**item.children.add(0)).name), "paths");
        assert_eq!(cstr((**item.children.add(0)).format), "u");
        assert_eq!(cstr((**item.children.add(1)).name), "values");
        assert_eq!(cstr((**item.children.add(1)).format), "z");

        // Top JSON array: struct with one validity buffer, 5 children.
        let json = &**array.children.add(0);
        assert_eq!(json.length, 2);
        assert_eq!(json.null_count, 0);
        assert_eq!(json.n_buffers, 1, "struct has only the validity buffer");
        assert_eq!(json.n_children, 5);
        assert!(
            (*json.buffers.add(0)).is_null(),
            "bare JSON has no null map"
        );

        // Typed Int64 child borrows its data buffer verbatim.
        let a = &**json.children.add(0);
        assert_eq!(a.length, 2);
        assert_eq!(*a.buffers.add(1) as *const i64, int_ptr, "Int64 zero-copy");
        assert_eq!(std::slice::from_raw_parts(int_ptr, 2), &[10, 20]);

        // Dynamic path "c": row 0 -> Int64 child (id 0), row 1 -> NULL (id 1).
        let c = &**json.children.add(2);
        assert_eq!(c.n_children, 2);
        let c_ids = std::slice::from_raw_parts(*c.buffers.add(0) as *const i8, 2);
        assert_eq!(c_ids, &[0, 1]);
        assert_eq!((**c.children.add(0)).length, 1, "one typed Int64 row");
        assert_eq!((**c.children.add(1)).length, 1, "one NULL row");

        // _shared_data array: list length 2 (rows), empty pair struct.
        let shared_arr = &**json.children.add(4);
        assert_eq!(shared_arr.length, 2);
        assert_eq!(shared_arr.n_buffers, 2, "validity + i64 offsets");
        assert_eq!(shared_arr.n_children, 1);
        let shared_offsets =
            std::slice::from_raw_parts(*shared_arr.buffers.add(1) as *const i64, 3);
        assert_eq!(shared_offsets, &[0, 0, 0]);
        let item_arr = &**shared_arr.children.add(0);
        assert_eq!(item_arr.length, 0, "no shared pairs");
        assert_eq!(item_arr.n_children, 2);

        (array.release.unwrap())(&mut array);
        (schema.release.unwrap())(&mut schema);
    }
}

// ---------------------------------------------------------------------------
// Shared-data zero-copy
// ---------------------------------------------------------------------------

#[test]
fn structured_shared_data_is_zero_copy() {
    // Two rows, one shared (path, value) pair each. Values are opaque binary.
    let structured = StructuredJson::try_new(
        Vec::new(),
        Vec::new(),
        vec![0, 1, 2],
        Utf8Column::new(vec![0, 3, 4], b"x.yz".to_vec()),
        Utf8Column::new(vec![0, 2, 5], vec![0x01, 0x02, 0x03, 0x04, 0x05]),
        2,
    )
    .unwrap();
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: json_type(Vec::new()),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        2,
    ));

    let s = structured_of(&batch);
    let offsets_ptr = s.shared_offsets.as_ptr();
    let paths_offsets_ptr = s.shared_paths.offsets.as_ptr();
    let paths_data_ptr = s.shared_paths.data.as_ptr();
    let values_offsets_ptr = s.shared_values.offsets.as_ptr();
    let values_data_ptr = s.shared_values.data.as_ptr();

    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch(&batch, &mut schema, &mut array).unwrap();

        let json = &**array.children.add(0);
        assert_eq!(
            json.n_children, 1,
            "only the shared child, no paths declared"
        );
        let shared_arr = &**json.children.add(0);
        assert_eq!(shared_arr.length, 2);
        assert_eq!(
            *shared_arr.buffers.add(1) as *const i64,
            offsets_ptr,
            "list offsets borrowed from shared_offsets"
        );
        let item = &**shared_arr.children.add(0);
        assert_eq!(item.length, 2, "two flattened pairs");
        let paths = &**item.children.add(0);
        assert_eq!(
            *paths.buffers.add(1) as *const i32,
            paths_offsets_ptr,
            "paths offsets zero-copy"
        );
        assert_eq!(
            *paths.buffers.add(2) as *const u8,
            paths_data_ptr,
            "paths data zero-copy"
        );
        let values = &**item.children.add(1);
        assert_eq!(
            *values.buffers.add(1) as *const i32,
            values_offsets_ptr,
            "values offsets zero-copy"
        );
        assert_eq!(
            *values.buffers.add(2) as *const u8,
            values_data_ptr,
            "values data zero-copy"
        );

        (array.release.unwrap())(&mut array);
        (schema.release.unwrap())(&mut schema);
    }
}

// ---------------------------------------------------------------------------
// Nullable(JSON)
// ---------------------------------------------------------------------------

#[test]
fn nullable_json_carries_struct_validity() {
    let (offsets, paths, values) = empty_shared(2);
    let structured = StructuredJson::try_new(
        vec![(
            "a".to_string(),
            Column::Int64(PrimitiveColumn::new(vec![13, 79])),
        )],
        Vec::new(),
        offsets,
        paths,
        values,
        2,
    )
    .unwrap();
    // Row 0 valid, row 1 null (ClickHouse null map: 0 = valid, 1 = null).
    let column =
        JsonColumn::structured(structured).with_validity(Some(Bitmap::from_ch_null_map(&[0, 1])));
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: ChType::Nullable(Box::new(json_type(vec![("a".to_string(), ChType::Int64)]))),
        }]),
        vec![Column::Json(column)],
        2,
    ));

    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch(&batch, &mut schema, &mut array).unwrap();

        let field = &**schema.children.add(0);
        assert_eq!(cstr(field.format), "+s");
        assert_eq!(field.flags & 2, 2);

        let json = &**array.children.add(0);
        assert_eq!(json.null_count, 1);
        let validity = *json.buffers.add(0) as *const u8;
        assert!(!validity.is_null(), "Nullable(JSON) carries a null map");
        assert_eq!(*validity, 0b01, "row 0 valid, row 1 null");

        (array.release.unwrap())(&mut array);
        (schema.release.unwrap())(&mut schema);
    }
}

// ---------------------------------------------------------------------------
// Text body
// ---------------------------------------------------------------------------

#[test]
fn text_body_exports_as_utf8() {
    let text = Utf8Column::new(vec![0, 7, 14], b"{\"k\":1}{\"k\":2}".to_vec());
    let data_ptr = text.data.as_ptr();
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: json_type(Vec::new()),
        }]),
        vec![Column::Json(JsonColumn::text(text))],
        2,
    ));

    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch(&batch, &mut schema, &mut array).unwrap();

        let field = &**schema.children.add(0);
        assert_eq!(cstr(field.format), "u", "text JSON exports as utf8");
        assert_eq!(field.flags & 2, 2);
        assert_eq!(field.n_children, 0);

        let json = &**array.children.add(0);
        assert_eq!(json.length, 2);
        assert_eq!(json.n_buffers, 3, "validity, offsets, data");
        assert_eq!(
            *json.buffers.add(2) as *const u8,
            data_ptr,
            "text zero-copy"
        );
        let offsets = std::slice::from_raw_parts(*json.buffers.add(1) as *const i32, 3);
        assert_eq!(offsets, &[0, 7, 14]);

        (array.release.unwrap())(&mut array);
        (schema.release.unwrap())(&mut schema);
    }
}

// ---------------------------------------------------------------------------
// Zero rows
// ---------------------------------------------------------------------------

#[test]
fn zero_row_structured_json() {
    let structured = StructuredJson::try_new(
        vec![("a".to_string(), Column::Int64(PrimitiveColumn::new(vec![])))],
        Vec::new(),
        vec![0],
        Utf8Column::new(vec![0], Vec::new()),
        Utf8Column::new(vec![0], Vec::new()),
        0,
    )
    .unwrap();
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: json_type(vec![("a".to_string(), ChType::Int64)]),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        0,
    ));

    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch(&batch, &mut schema, &mut array).unwrap();

        let json = &**array.children.add(0);
        assert_eq!(json.length, 0);
        assert_eq!(json.n_children, 2, "one typed path + shared");
        let shared_arr = &**json.children.add(1);
        assert_eq!(shared_arr.length, 0);
        // Arrow requires the offsets buffer to hold length+1 entries; the
        // decoder emits the leading [0] even for a zero-row block.
        let offsets = std::slice::from_raw_parts(*shared_arr.buffers.add(1) as *const i64, 1);
        assert_eq!(offsets, &[0]);
        assert!(
            !(*shared_arr.buffers.add(1)).is_null(),
            "offsets pointer is never null even at zero rows"
        );

        (array.release.unwrap())(&mut array);
        (schema.release.unwrap())(&mut schema);
    }
}

// ---------------------------------------------------------------------------
// JSON typed path of type JSON (recursion)
// ---------------------------------------------------------------------------

#[test]
fn typed_path_of_type_json_recurses() {
    let (inner_offsets, inner_paths, inner_values) = empty_shared(1);
    let inner = StructuredJson::try_new(
        vec![(
            "x".to_string(),
            Column::Int64(PrimitiveColumn::new(vec![13])),
        )],
        Vec::new(),
        inner_offsets,
        inner_paths,
        inner_values,
        1,
    )
    .unwrap();
    let (outer_offsets, outer_paths, outer_values) = empty_shared(1);
    let outer = StructuredJson::try_new(
        vec![(
            "nested".to_string(),
            Column::Json(JsonColumn::structured(inner)),
        )],
        Vec::new(),
        outer_offsets,
        outer_paths,
        outer_values,
        1,
    )
    .unwrap();
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: json_type(vec![(
                "nested".to_string(),
                json_type(vec![("x".to_string(), ChType::Int64)]),
            )]),
        }]),
        vec![Column::Json(JsonColumn::structured(outer))],
        1,
    ));

    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch(&batch, &mut schema, &mut array).unwrap();

        let field = &**schema.children.add(0);
        let nested = &**field.children.add(0);
        assert_eq!(cstr(nested.name), "nested");
        assert_eq!(cstr(nested.format), "+s", "nested JSON is a struct");
        // nested struct: typed "x" + "_shared_data".
        assert_eq!(nested.n_children, 2);
        assert_eq!(cstr((**nested.children.add(0)).name), "x");
        assert_eq!(cstr((**nested.children.add(0)).format), "l");
        assert_eq!(cstr((**nested.children.add(1)).name), "_shared_data");

        let json = &**array.children.add(0);
        let nested_arr = &**json.children.add(0);
        assert_eq!(nested_arr.length, 1);
        assert_eq!(nested_arr.n_children, 2);
        let x = &**nested_arr.children.add(0);
        assert_eq!(
            std::slice::from_raw_parts(*x.buffers.add(1) as *const i64, 1),
            &[13]
        );

        (array.release.unwrap())(&mut array);
        (schema.release.unwrap())(&mut schema);
    }
}

// ---------------------------------------------------------------------------
// Stream: differing dynamic path sets across chunks
// ---------------------------------------------------------------------------

fn structured_json_batch(dynamic: Vec<(String, DynamicColumn)>, len: usize) -> Arc<ColBatch> {
    let (offsets, paths, values) = empty_shared(len);
    let structured =
        StructuredJson::try_new(Vec::new(), dynamic, offsets, paths, values, len).unwrap();
    Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: json_type(Vec::new()),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        len,
    ))
}

/// Read the dense-union `type_ids` (buffer 0) and `offsets` (buffer 1) of a
/// union array node as owned Vecs. Copying the bytes out lets a caller keep
/// asserting after intervening allocations, and reading buffer 1 at all is what
/// catches a dangling offsets pointer for the synthetic all-NULL path.
unsafe fn union_ids_and_offsets(node: &ArrowArray, len: usize) -> (Vec<i8>, Vec<i32>) {
    let ids = std::slice::from_raw_parts(*node.buffers.add(0) as *const i8, len).to_vec();
    let offsets = std::slice::from_raw_parts(*node.buffers.add(1) as *const i32, len).to_vec();
    (ids, offsets)
}

#[test]
fn stream_unifies_differing_dynamic_path_sets() {
    // Chunk A carries path "p1", chunk B carries path "p2". The stream schema
    // must hold both; each chunk exports an all-NULL union for the path it lacks.
    // Three rows per block so the NULL-child dense-union offsets form a real
    // 0,1,2 run: a dangling offsets pointer would not reproduce it.
    let chunk_a = structured_json_batch(
        vec![(
            "p1".to_string(),
            single_child_dynamic(
                ChType::Int64,
                Column::Int64(PrimitiveColumn::new(vec![13, 79, 101])),
                &[0, 0, 0],
            ),
        )],
        3,
    );
    let chunk_b = structured_json_batch(
        vec![(
            "p2".to_string(),
            single_child_dynamic(
                ChType::String,
                Column::Utf8(Utf8Column::new(vec![0, 2, 4, 6], b"hihuho".to_vec())),
                &[0, 0, 0],
            ),
        )],
        3,
    );
    let schema = chunk_a.schema.clone();

    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![chunk_a, chunk_b], &mut stream);

        let mut out_schema: ArrowSchema = std::mem::zeroed();
        assert_eq!(
            (stream.get_schema.unwrap())(&mut stream, &mut out_schema),
            0
        );
        let field = &**out_schema.children.add(0);
        assert_eq!(field.n_children, 3, "p1, p2, _shared_data");
        assert_eq!(cstr((**field.children.add(0)).name), "p1");
        assert_eq!(cstr((**field.children.add(1)).name), "p2");
        assert_eq!(cstr((**field.children.add(2)).name), "_shared_data");

        // Chunk A: p1 has real Int64 rows (id 0); p2 is the synthetic all-NULL
        // union (missing path). Read p2's offsets buffer contents to prove they
        // are owned, not dangling into a dropped synthetic column.
        let mut first: ArrowArray = std::mem::zeroed();
        assert_eq!((stream.get_next.unwrap())(&mut stream, &mut first), 0);
        let json_a = &**first.children.add(0);
        let p1_a = &**json_a.children.add(0);
        let (p1_a_ids, p1_a_offsets) = union_ids_and_offsets(p1_a, 3);
        assert_eq!(p1_a_ids, [0, 0, 0], "p1 routes to Int64");
        assert_eq!(p1_a_offsets, [0, 1, 2], "three Int64 occurrences");
        assert_eq!((**p1_a.children.add(0)).length, 3);
        let p2_a = &**json_a.children.add(1);
        // p2's plan child count is 1 (String), so the null id is 1.
        let (p2_a_ids, p2_a_offsets) = union_ids_and_offsets(p2_a, 3);
        // Churn the allocator: a pre-fix dangling read would now see reused heap.
        let churn: Vec<i32> = (100..1124).collect();
        assert_eq!(churn.len(), 1024);
        assert_eq!(p2_a_ids, [1, 1, 1], "p2 missing -> every row NULL");
        assert_eq!(p2_a_offsets, [0, 1, 2], "owned NULL-child offset run");
        assert_eq!((**p2_a.children.add(0)).length, 0, "no String rows in A");
        assert_eq!((**p2_a.children.add(1)).length, 3, "three NULL rows in A");

        // Chunk B: p1 is the synthetic all-NULL union, p2 has real String rows.
        let mut second: ArrowArray = std::mem::zeroed();
        assert_eq!((stream.get_next.unwrap())(&mut stream, &mut second), 0);
        let json_b = &**second.children.add(0);
        let p1_b = &**json_b.children.add(0);
        let (p1_b_ids, p1_b_offsets) = union_ids_and_offsets(p1_b, 3);
        assert_eq!(p1_b_ids, [1, 1, 1], "p1 missing -> every row NULL");
        assert_eq!(p1_b_offsets, [0, 1, 2], "owned NULL-child offset run");
        assert_eq!((**p1_b.children.add(1)).length, 3, "three NULL rows in B");
        let p2_b = &**json_b.children.add(1);
        let (p2_b_ids, p2_b_offsets) = union_ids_and_offsets(p2_b, 3);
        assert_eq!(p2_b_ids, [0, 0, 0], "p2 routes to String");
        assert_eq!(p2_b_offsets, [0, 1, 2], "three String occurrences");
        assert_eq!((**p2_b.children.add(0)).length, 3);

        (second.release.unwrap())(&mut second);
        (first.release.unwrap())(&mut first);
        (out_schema.release.unwrap())(&mut out_schema);
        (stream.release.unwrap())(&mut stream);
    }
}

// ---------------------------------------------------------------------------
// Stream: Dynamic union remap inside a shared JSON dynamic path
// ---------------------------------------------------------------------------

#[test]
fn stream_remaps_dynamic_children_inside_json_path() {
    // Both chunks carry path "p", but with different block-local typed children.
    // The result-wide union unifies to [Int64, String, NULL] in BTreeMap name
    // order, and each chunk's local child id is remapped to its global slot.
    let chunk_a = structured_json_batch(
        vec![(
            "p".to_string(),
            single_child_dynamic(
                ChType::String,
                Column::Utf8(Utf8Column::new(vec![0, 6], b"user_1".to_vec())),
                &[0],
            ),
        )],
        1,
    );
    let chunk_b = structured_json_batch(
        vec![(
            "p".to_string(),
            single_child_dynamic(
                ChType::Int64,
                Column::Int64(PrimitiveColumn::new(vec![79])),
                &[0],
            ),
        )],
        1,
    );
    let schema = chunk_a.schema.clone();

    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![chunk_a, chunk_b], &mut stream);

        let mut out_schema: ArrowSchema = std::mem::zeroed();
        (stream.get_schema.unwrap())(&mut stream, &mut out_schema);
        let field = &**out_schema.children.add(0);
        let p = &**field.children.add(0);
        assert_eq!(cstr(p.name), "p");
        assert_eq!(cstr(p.format), "+ud:0,1,2", "Int64, String, NULL");
        assert_eq!(cstr((**p.children.add(0)).name), "Int64");
        assert_eq!(cstr((**p.children.add(1)).name), "String");

        // Chunk A's local String child (local id 0) remaps to global id 1.
        let mut first: ArrowArray = std::mem::zeroed();
        (stream.get_next.unwrap())(&mut stream, &mut first);
        let json_a = &**first.children.add(0);
        let p_a = &**json_a.children.add(0);
        assert_eq!(*(*p_a.buffers.add(0) as *const i8), 1, "String -> global 1");
        assert_eq!(
            (**p_a.children.add(1)).length,
            1,
            "String child has the row"
        );
        assert_eq!((**p_a.children.add(0)).length, 0, "Int64 child empty in A");

        // Chunk B's local Int64 child (local id 0) remaps to global id 0.
        let mut second: ArrowArray = std::mem::zeroed();
        (stream.get_next.unwrap())(&mut stream, &mut second);
        let json_b = &**second.children.add(0);
        let p_b = &**json_b.children.add(0);
        assert_eq!(*(*p_b.buffers.add(0) as *const i8), 0, "Int64 -> global 0");
        assert_eq!((**p_b.children.add(0)).length, 1, "Int64 child has the row");

        (second.release.unwrap())(&mut second);
        (first.release.unwrap())(&mut first);
        (out_schema.release.unwrap())(&mut out_schema);
        (stream.release.unwrap())(&mut stream);
    }
}

// ---------------------------------------------------------------------------
// Stream: mixed structured/text bodies rejected
// ---------------------------------------------------------------------------

#[test]
fn stream_rejects_mixed_structured_and_text_bodies() {
    let (offsets, paths, values) = empty_shared(1);
    let structured =
        StructuredJson::try_new(Vec::new(), Vec::new(), offsets, paths, values, 1).unwrap();
    let structured_chunk = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: json_type(Vec::new()),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        1,
    ));
    let text_chunk = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: json_type(Vec::new()),
        }]),
        vec![Column::Json(JsonColumn::text(Utf8Column::new(
            vec![0, 7],
            b"{\"k\":1}".to_vec(),
        )))],
        1,
    ));
    let schema = structured_chunk.schema.clone();

    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![structured_chunk, text_chunk], &mut stream);

        let mut out_schema: ArrowSchema = std::mem::zeroed();
        assert_eq!(
            (stream.get_schema.unwrap())(&mut stream, &mut out_schema),
            STREAM_INIT_ERROR,
            "mixed body kinds fail stream init"
        );
        let mut out_array: ArrowArray = std::mem::zeroed();
        assert_eq!(
            (stream.get_next.unwrap())(&mut stream, &mut out_array),
            STREAM_INIT_ERROR
        );
        let message = cstr((stream.get_last_error.unwrap())(&mut stream));
        assert!(
            message.contains("structured and text"),
            "descriptive error: {message}"
        );

        (stream.release.unwrap())(&mut stream);
    }
}

// ---------------------------------------------------------------------------
// Logical-only schema (no column)
// ---------------------------------------------------------------------------

#[test]
fn logical_schema_exposes_typed_paths_and_shared() {
    // Pins the documented `export_schema` contract for a column-less JSON field:
    // it can only emit the structured struct with the declared typed paths plus
    // `_shared_data`. It cannot know a block's dynamic-path children and cannot
    // represent a STRING-mode text body, so this schema MUST NOT be paired with
    // export_batch_array (a concrete array may add dynamic-path children or be a
    // utf8 array). This is the same limitation Dynamic has (only its NULL child).
    let schema_in = Schema::new(vec![Field {
        name: "j".into(),
        ch_type: json_type(vec![("a".to_string(), ChType::Int64)]),
    }]);

    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        export_schema(&schema_in, &mut schema);
        let field = &**schema.children.add(0);
        assert_eq!(
            cstr(field.format),
            "+s",
            "structured struct, never text utf8"
        );
        assert_eq!(field.flags & 2, 2, "JSON is nullable-flagged");
        assert_eq!(
            field.n_children, 2,
            "only the typed path + shared, no dynamic"
        );
        assert_eq!(cstr((**field.children.add(0)).name), "a");
        assert_eq!(cstr((**field.children.add(0)).format), "l");
        let shared = &**field.children.add(1);
        assert_eq!(cstr(shared.name), "_shared_data");
        assert_eq!(cstr(shared.format), "+L");
        (schema.release.unwrap())(&mut schema);
    }
}
