use super::*;

#[test]
fn test_decode_chained_simple_aggregate_function() {
    // A chained SAF resolves through the full delegate chain to its physical
    // inner. `SimpleAggregateFunction(anyLast, SimpleAggregateFunction(sum,
    // UInt64))` is constructible live at v26.6.1.1193-stable; its wire body is
    // a plain UInt64 column. Both the standalone chain and the same chain as a
    // LowCardinality inner must decode.
    let chain = ChType::SimpleAggregateFunction {
        func: "anyLast".to_string(),
        inner: Box::new(ChType::SimpleAggregateFunction {
            func: "sum".to_string(),
            inner: Box::new(ChType::UInt64),
        }),
    };
    assert_eq!(
        parse_ch_type("SimpleAggregateFunction(anyLast, SimpleAggregateFunction(sum, UInt64))"),
        Some(chain.clone())
    );

    // Standalone chained SAF: decodes as a UInt64 primitive body.
    let values = [13u64, 79, 8_589_934_592];
    let data = BlockBuilder::new()
        .header(1, values.len())
        .column_header("saf_chain", &chain.to_string())
        .uint64_data(&values)
        .build();
    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.schema.fields[0].ch_type, chain);
    match cb.chunks[0].column(0) {
        Column::UInt64(c) => assert_eq!(c.values, values.to_vec()),
        other => panic!("expected UInt64, got {other:?}"),
    }

    // Same chain as a LowCardinality inner: the dictionary body is a plain
    // UInt64 run and the column is non-nullable.
    let lc_chain = ChType::LowCardinality(Box::new(chain));
    let (nullable, dict_value_type) = match &lc_chain {
        ChType::LowCardinality(inner) => low_cardinality_dict_value_type(inner),
        _ => unreachable!(),
    };
    assert!(!nullable);
    assert_eq!(dict_value_type, &ChType::UInt64);
}

#[test]
fn test_decode_simple_aggregate_function_scalar() {
    // Wire bytes are byte-identical to the bare inner Float64.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("s", "SimpleAggregateFunction(sum, Float64)")
        .float64_data(&[3.5, -7.25, 0.0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::SimpleAggregateFunction {
            func: "sum".to_string(),
            inner: Box::new(ChType::Float64),
        }
    );
    match cb.chunks[0].column(0) {
        Column::Float64(c) => assert_eq!(c.values, vec![3.5, -7.25, 0.0]),
        other => panic!("expected Float64 delegate column, got {other:?}"),
    }
}

#[test]
fn test_decode_simple_aggregate_function_nullable_string() {
    // SAF(anyLast, Nullable(String)) decodes exactly as Nullable(String):
    // the per-row null map then the string run.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("s", "SimpleAggregateFunction(anyLast, Nullable(String))")
        .null_map(&[false, true, false])
        .string_data(&["user_1", "", "user_2"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Utf8(c) => {
            assert_eq!(c.value(0), b"user_1");
            assert_eq!(c.value(2), b"user_2");
            assert_eq!(c.null_count(), 1);
            let bm = c.validity.as_ref().expect("nullable validity");
            assert!(bm.is_valid(0) && !bm.is_valid(1) && bm.is_valid(2));
        }
        other => panic!("expected Utf8 delegate column, got {other:?}"),
    }
}

#[test]
fn test_decode_simple_aggregate_function_over_low_cardinality() {
    // Shared gate: SAF over LowCardinality(String) hoists the LC 8-byte key
    // version to the front through the delegate, then the LC body.
    let dictionary = ["", "user_1", "user_2"];
    let indices = [1u64, 2, 1];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header(
            "s",
            "SimpleAggregateFunction(anyLast, LowCardinality(String))",
        )
        .low_cardinality_string(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![1, 2, 1]);
            assert_eq!(
                lc_value(cb.chunks[0].column(0), 0).as_deref(),
                Some(&b"user_1"[..])
            );
        }
        other => panic!("expected Dictionary delegate column, got {other:?}"),
    }
}

#[test]
fn test_decode_point_plain() {
    // Point = Tuple(Float64, Float64), field-major: all X then all Y.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("p", "Point")
        .float64_data(&[1.0, 3.0]) // X coordinates
        .float64_data(&[2.0, 4.0]) // Y coordinates
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.schema.fields[0].ch_type, ChType::Geo(GeoKind::Point));
    let t = as_tuple(cb.chunks[0].column(0));
    assert_eq!(t.fields.len(), 2);
    match (&t.fields[0], &t.fields[1]) {
        (Column::Float64(x), Column::Float64(y)) => {
            assert_eq!(x.values, vec![1.0, 3.0]);
            assert_eq!(y.values, vec![2.0, 4.0]);
        }
        other => panic!("expected two Float64 tuple fields, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_point() {
    // Nullable(Point): null map then the Tuple(Float64, Float64) body.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("p", "Nullable(Point)")
        .null_map(&[false, true])
        .float64_data(&[1.0, 0.0])
        .float64_data(&[2.0, 0.0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let t = as_tuple(cb.chunks[0].column(0));
    assert_eq!(t.len(), 2);
    let bm = t.validity.as_ref().expect("tuple-level validity");
    assert!(bm.is_valid(0) && !bm.is_valid(1));
}

#[test]
fn test_decode_ring() {
    // Ring = Array(Point): offsets then the flattened Point body (field-major
    // over all points).
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("r", "Ring")
        .array_offsets(&[2, 3]) // row 0 has 2 points, row 1 has 1 point
        .float64_data(&[1.0, 3.0, 5.0]) // X for all 3 points
        .float64_data(&[2.0, 4.0, 6.0]) // Y for all 3 points
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    let t = as_tuple(arr.values.as_ref());
    match (&t.fields[0], &t.fields[1]) {
        (Column::Float64(x), Column::Float64(y)) => {
            assert_eq!(x.values, vec![1.0, 3.0, 5.0]);
            assert_eq!(y.values, vec![2.0, 4.0, 6.0]);
        }
        other => panic!("expected Point tuple fields, got {other:?}"),
    }
}

#[test]
fn test_decode_multi_point() {
    // MultiPoint = Array(Point): one offset run followed by the flattened
    // field-major Point body. Row 0 has two points and row 1 has one.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("mp", "MultiPoint")
        .array_offsets(&[2, 3])
        .float64_data(&[1.0, 3.0, 5.0])
        .float64_data(&[2.0, 4.0, 6.0])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::Geo(GeoKind::MultiPoint)
    );
    let points = as_array(cb.chunks[0].column(0));
    assert_eq!(points.offsets, vec![0i64, 2, 3]);
    let point = as_tuple(points.values.as_ref());
    match (&point.fields[0], &point.fields[1]) {
        (Column::Float64(x), Column::Float64(y)) => {
            assert_eq!(x.values, vec![1.0, 3.0, 5.0]);
            assert_eq!(y.values, vec![2.0, 4.0, 6.0]);
        }
        other => panic!("expected Point tuple fields, got {other:?}"),
    }
}

#[test]
fn test_decode_multi_polygon() {
    // MultiPolygon = Array(Array(Array(Point))): three offset levels then the
    // Point body. One row holding one polygon of one ring of two points.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("mp", "MultiPolygon")
        .array_offsets(&[1]) // 1 polygon in the row
        .array_offsets(&[1]) // 1 ring in the polygon
        .array_offsets(&[2]) // 2 points in the ring
        .float64_data(&[1.0, 3.0])
        .float64_data(&[2.0, 4.0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let l0 = as_array(cb.chunks[0].column(0));
    assert_eq!(l0.offsets, vec![0i64, 1]);
    let l1 = as_array(l0.values.as_ref());
    assert_eq!(l1.offsets, vec![0i64, 1]);
    let l2 = as_array(l1.values.as_ref());
    assert_eq!(l2.offsets, vec![0i64, 2]);
    let t = as_tuple(l2.values.as_ref());
    match (&t.fields[0], &t.fields[1]) {
        (Column::Float64(x), Column::Float64(y)) => {
            assert_eq!(x.values, vec![1.0, 3.0]);
            assert_eq!(y.values, vec![2.0, 4.0]);
        }
        other => panic!("expected Point tuple fields, got {other:?}"),
    }
}

#[test]
fn test_decode_name_decoration_zero_rows() {
    // A zero-row block carrying the three alias groups contributes the schema
    // but no chunks; the empty columns delegate to the physical layout.
    let data = BlockBuilder::new()
        .header(4, 0)
        .column_header("s", "SimpleAggregateFunction(sum, Float64)")
        .column_header("p", "Point")
        .column_header("mp", "MultiPoint")
        .column_header("n", "Nested(a UInt32, b String)")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 4);
    assert_eq!(cb.schema.fields[1].ch_type, ChType::Geo(GeoKind::Point));
    assert_eq!(
        cb.schema.fields[2].ch_type,
        ChType::Geo(GeoKind::MultiPoint)
    );
}

#[test]
fn test_decode_alias_over_wrapper_zero_rows() {
    // A zero-row block whose header is a name-decoration alias OVER a
    // Nullable/geo/Nested inner must build an empty column via the physical
    // delegate, never reach the `empty_column` `unreachable!` arm. Before the
    // Fix, `SimpleAggregateFunction(anyLast, Nullable(String))` (and the SAF
    // over Point/Nested shapes) panicked on this untrusted 0-row header.
    let data = BlockBuilder::new()
        .header(4, 0)
        // SAF over a Nullable inner: delegate is Nullable(String).
        .column_header("s", "SimpleAggregateFunction(anyLast, Nullable(String))")
        // SAF over a geo inner: delegate is Geo(Point) -> Tuple(Float64, Float64).
        .column_header("g", "SimpleAggregateFunction(anyLast, Point)")
        // A Nested field group.
        .column_header("n", "Nested(a UInt32, b String)")
        // Alias legal directly inside Nullable, expanded post-unwrap.
        .column_header("np", "Nullable(Point)")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 4);
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::SimpleAggregateFunction {
            func: "anyLast".to_string(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        }
    );
    // The block_end completeness scan agrees the zero-row block is complete.
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_decode_nullable_simple_aggregate_function() {
    // Nullable(SAF(sum, UInt64)) decodes exactly as Nullable(UInt64): the
    // per-row null map then the UInt64 run. The SAF is name decoration inside
    // the Nullable (confirmed legal live at v26.6.1.1193-stable).
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("s", "Nullable(SimpleAggregateFunction(sum, UInt64))")
        .null_map(&[false, true, false])
        .uint64_data(&[13, 0, 79])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::Nullable(Box::new(ChType::SimpleAggregateFunction {
            func: "sum".to_string(),
            inner: Box::new(ChType::UInt64),
        }))
    );
    match cb.chunks[0].column(0) {
        Column::UInt64(c) => {
            assert_eq!(c.values, vec![13, 0, 79]);
            let bm = c.validity.as_ref().expect("nullable validity");
            assert!(bm.is_valid(0) && !bm.is_valid(1) && bm.is_valid(2));
        }
        other => panic!("expected UInt64 delegate column, got {other:?}"),
    }
}

#[test]
fn test_decode_array_simple_aggregate_function() {
    // Array(SAF(sum, UInt64)) decodes exactly as Array(UInt64): offsets then
    // the flattened UInt64 element run.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(SimpleAggregateFunction(sum, UInt64))")
        .array_offsets(&[2, 3]) // row 0 has 2 elements, row 1 has 1
        .uint64_data(&[13, 79, 5])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    match arr.values.as_ref() {
        Column::UInt64(c) => assert_eq!(c.values, vec![13, 79, 5]),
        other => panic!("expected UInt64 element column, got {other:?}"),
    }
}

#[test]
fn test_decode_low_cardinality_simple_aggregate_function() {
    // LowCardinality(SAF(anyLast, String)) decodes exactly as
    // LowCardinality(String): the key version prefix (in the helper), the
    // per-block dictionary, and the indexes.
    let dictionary = ["", "user_1", "user_2"];
    let indices = [1u64, 2, 1];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header(
            "s",
            "LowCardinality(SimpleAggregateFunction(anyLast, String))",
        )
        .low_cardinality_string(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".to_string(),
            inner: Box::new(ChType::String),
        }))
    );
    match cb.chunks[0].column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![1, 2, 1]);
            assert_eq!(
                lc_value(cb.chunks[0].column(0), 0).as_deref(),
                Some(&b"user_1"[..])
            );
        }
        other => panic!("expected Dictionary delegate column, got {other:?}"),
    }
}
