use super::*;

#[test]
fn test_decode_array_int32() {
    // Three rows, cumulative absolute end-offsets [2, 2, 5] (no leading zero):
    // row 0 has two elements, row 1 is EMPTY (equal adjacent offsets), row 2
    // has three. The flattened element body is the five Int32 values.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("a", "Array(Int32)")
        .array_offsets(&[2, 2, 5])
        .int32_data(&[13, 79, 21, 34, 55])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    // Arrow list offsets carry the leading 0 and are i64.
    assert_eq!(arr.offsets, vec![0i64, 2, 2, 5]);
    assert_eq!(arr.len(), 3);
    assert_eq!(arr.null_count(), 0);
    match arr.values.as_ref() {
        Column::Int32(v) => {
            assert!(v.validity.is_none());
            assert_eq!(v.values, vec![13, 79, 21, 34, 55]);
        }
        other => panic!("expected Int32 element values, got {other:?}"),
    }
}

#[test]
fn test_decode_array_string() {
    // Variable-length element body after the offsets: row 0 has two strings,
    // row 1 has one.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(String)")
        .array_offsets(&[2, 3])
        .string_data(&["user_1", "user_2", "user_3"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    match arr.values.as_ref() {
        Column::Utf8(v) => {
            assert_eq!(v.len(), 3);
            assert_eq!(v.value(0), b"user_1");
            assert_eq!(v.value(1), b"user_2");
            assert_eq!(v.value(2), b"user_3");
        }
        other => panic!("expected Utf8 element values, got {other:?}"),
    }
}

#[test]
fn test_decode_array_nullable_int32() {
    // For `Array(Nullable(Int32))` the element body is a `total_elements` null
    // map then the `total_elements` values. Element-level nulls live on the
    // element column's validity, never on the array itself.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(Nullable(Int32))")
        .array_offsets(&[2, 3])
        .null_map(&[false, true, false]) // element 1 is null
        .int32_data(&[13, 0, 79])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    // The array level is never nullable.
    assert_eq!(arr.null_count(), 0);
    match arr.values.as_ref() {
        Column::Int32(v) => {
            assert_eq!(v.values, vec![13, 0, 79]);
            let bm = v.validity.as_ref().expect("nullable element validity");
            assert!(bm.is_valid(0));
            assert!(!bm.is_valid(1));
            assert!(bm.is_valid(2));
            assert_eq!(v.null_count(), 1);
        }
        other => panic!("expected Int32 element values, got {other:?}"),
    }
}

#[test]
fn test_decode_array_low_cardinality_string() {
    // `Array(LowCardinality(String))`: SerializationArray recurses into the
    // element prefix, so the LC 8-byte key version is written FIRST, before the
    // offsets. The LC body (index word / dictionary / row count / indexes) is
    // the flattened element column and comes AFTER the offsets. Build a full
    // LC(String) block, then move its leading 8-byte key version ahead of the
    // offsets to match the wire order.
    let dictionary = ["", "user_1", "user_2"];
    let element_indices = [1u64, 2, 1]; // three flattened elements
    let lc_full = BlockBuilder::new()
        .low_cardinality_string(&dictionary, &element_indices, 1)
        .build();
    let (key_version, lc_body) = lc_full.split_at(8);

    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(LowCardinality(String))")
        .raw_bytes(key_version) // element state prefix, ahead of the offsets
        .array_offsets(&[2, 3]) // row 0: two elements, row 1: one element
        .raw_bytes(lc_body) // LC index word / dict / row count / indexes
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    // The element column is a per-block dictionary.
    match arr.values.as_ref() {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![1, 2, 1]);
            assert!(d.validity.is_none());
            match d.values.as_ref() {
                Column::Utf8(v) => {
                    assert_eq!(v.value(0), b"");
                    assert_eq!(v.value(1), b"user_1");
                    assert_eq!(v.value(2), b"user_2");
                }
                other => panic!("expected Utf8 dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary element values, got {other:?}"),
    }
}

#[test]
fn test_decode_array_low_cardinality_all_empty() {
    // Rows > 0 but every array empty: the server writes the hoisted LC key
    // version, then all-zero offsets, then NOTHING for the LC element run
    // (`SerializationLowCardinality::serializeBinaryBulkWithMultipleStreams`
    // early-returns at limit == 0, confirmed at v26.6.1.1193-stable). The
    // decoder must read zero LC body bytes rather than fail with EOF.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(LowCardinality(String))")
        .raw_bytes(&1u64.to_le_bytes()) // hoisted LC key version
        .array_offsets(&[0, 0]) // both rows empty
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 0, 0]);
    match arr.values.as_ref() {
        Column::Dictionary(d) => {
            assert!(d.indices.is_empty());
            assert!(d.validity.is_none());
            assert_eq!(d.values.len(), 0);
        }
        other => panic!("expected empty Dictionary element values, got {other:?}"),
    }

    // The completeness scan must consume exactly the same zero LC body bytes.
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_decode_array_of_array_int32() {
    // `Array(Array(Int32))`: outer offsets, then the inner array's offsets (its
    // flattened element column), then the leaf Int32 body. The Int32 leaf has
    // no state prefix, so nothing precedes the outer offsets.
    //
    // Outer 2 rows: row 0 = [[13, 79], [21]], row 1 = [[34, 55, 89]].
    // Outer offsets count inner arrays: [2, 3] -> 3 inner arrays total.
    // Inner offsets count leaf ints: [2, 3, 6] -> 6 leaf ints total.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(Array(Int32))")
        .array_offsets(&[2, 3]) // outer
        .array_offsets(&[2, 3, 6]) // inner
        .int32_data(&[13, 79, 21, 34, 55, 89]) // leaf
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let outer = as_array(cb.chunks[0].column(0));
    assert_eq!(outer.offsets, vec![0i64, 2, 3]);
    let inner = as_array(outer.values.as_ref());
    assert_eq!(inner.offsets, vec![0i64, 2, 3, 6]);
    match inner.values.as_ref() {
        Column::Int32(v) => assert_eq!(v.values, vec![13, 79, 21, 34, 55, 89]),
        other => panic!("expected Int32 leaf values, got {other:?}"),
    }
}

#[test]
fn test_decode_array_zero_rows() {
    // A zero-row Array block contributes the schema but no chunk, and reads no
    // offsets or element body. `empty_column` builds the offsets `[0]` shape.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("a", "Array(Int32)")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 1);
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::Array(Box::new(ChType::Int32))
    );

    // The zero-row column shape: offsets `[0]` (len 0) over an empty element
    // column.
    let empty = empty_column(&ChType::Array(Box::new(ChType::Int32)));
    let arr = as_array(&empty);
    assert_eq!(arr.offsets, vec![0i64]);
    assert_eq!(arr.len(), 0);
    match arr.values.as_ref() {
        Column::Int32(v) => assert!(v.values.is_empty()),
        other => panic!("expected empty Int32 element values, got {other:?}"),
    }
}

#[test]
fn test_array_rejects_non_monotonic_offsets() {
    // Decreasing offsets are INCORRECT_DATA on the server and corrupt here; the
    // decoder rejects them as InvalidArray rather than slicing out of bounds.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(Int32)")
        .array_offsets(&[3, 1]) // 1 < 3 -> reject
        .int32_data(&[13, 79, 21])
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidArray { .. })
    ));
    // The completeness scan surfaces the same error rather than stalling.
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidArray { .. })
    ));
}

#[test]
fn test_decode_array_nested_three_deep() {
    // A modestly nested type (well within MAX_TYPE_DEPTH) still parses and
    // decodes: `Array(Array(Array(Int32)))` with one outer row -> one middle
    // array -> one inner array -> two leaf ints. Each level writes its own
    // absolute end-offsets; the Int32 leaf has no state prefix.
    assert_eq!(
        parse_ch_type("Array(Array(Array(Int32)))"),
        Some(ChType::Array(Box::new(ChType::Array(Box::new(
            ChType::Array(Box::new(ChType::Int32))
        )))))
    );

    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("a", "Array(Array(Array(Int32)))")
        .array_offsets(&[1]) // outer: 1 middle array
        .array_offsets(&[1]) // middle: 1 inner array
        .array_offsets(&[2]) // inner: 2 leaf ints
        .int32_data(&[13, 79]) // leaf
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let outer = as_array(cb.chunks[0].column(0));
    assert_eq!(outer.offsets, vec![0i64, 1]);
    let middle = as_array(outer.values.as_ref());
    assert_eq!(middle.offsets, vec![0i64, 1]);
    let inner = as_array(middle.values.as_ref());
    assert_eq!(inner.offsets, vec![0i64, 2]);
    match inner.values.as_ref() {
        Column::Int32(v) => assert_eq!(v.values, vec![13, 79]),
        other => panic!("expected Int32 leaf values, got {other:?}"),
    }
}

#[test]
fn test_array_rejects_offset_above_i64_max() {
    // An absolute offset in (i64::MAX, u64::MAX] cannot widen into the i64
    // LargeList offset. Both the allocating decode and the completeness scan
    // must reject it as InvalidArray for the SAME bytes; if the scan instead
    // returned UnexpectedEof, StreamDecoder would treat a fully-present corrupt
    // block as "need more bytes" and stall.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("a", "Array(Int32)")
        .array_offsets(&[u64::MAX])
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidArray { .. })
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidArray { .. })
    ));
}

#[test]
fn test_array_forbidden_low_cardinality_inner_rejected_regardless_of_rows() {
    // A forbidden LowCardinality inner (Decimal is not `canBeInsideLowCardinality`)
    // nested inside an Array must be rejected at header-read time on BOTH the
    // zero-row and the with-rows paths, so `empty_column` (which never consults
    // the allowlist) cannot silently accept what a row-bearing block rejects.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("a", "Array(LowCardinality(Decimal(9, 4)))")
            .build();
        assert!(
            matches!(
                decode_all_bytes(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ),
            "decode should reject forbidden LC-in-Array at {num_rows} rows"
        );
        assert!(
            matches!(
                block_end(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ),
            "scan should reject forbidden LC-in-Array at {num_rows} rows"
        );
    }
}

#[test]
fn test_decode_tuple_plain() {
    // Tuple(Int32, String): element 0's full Int32 run, then element 1's
    // full String run, column-of-columns with no interleaving and no
    // tuple-level framing.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("t", "Tuple(Int32, String)")
        .int32_data(&[13, 79, -7])
        .string_data(&["user_1", "user_2", ""])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    let t = as_tuple(batch.column(0));
    assert_eq!(t.len(), 3);
    assert_eq!(t.fields.len(), 2);
    assert!(t.validity.is_none());
    match &t.fields[0] {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), &[13, 79, -7]),
        other => panic!("expected Int32 element, got {other:?}"),
    }
    match &t.fields[1] {
        Column::Utf8(c) => {
            assert_eq!(c.value(0), b"user_1");
            assert_eq!(c.value(1), b"user_2");
            assert_eq!(c.value(2), b"");
        }
        other => panic!("expected Utf8 element, got {other:?}"),
    }
}

#[test]
fn test_decode_named_tuple_with_nullable_element() {
    // Tuple(a Int32, b Nullable(String)): the names live in the schema's
    // ChType only; element b's body is its own per-row null map then the
    // string run, the ordinary Nullable framing at element level.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("t", "Tuple(a Int32, b Nullable(String))")
        .int32_data(&[1, 2, 3])
        .null_map(&[false, true, false])
        .string_data(&["user_1", "", "user_2"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::Tuple(vec![
            (Some("a".to_string()), ChType::Int32),
            (
                Some("b".to_string()),
                ChType::Nullable(Box::new(ChType::String)),
            ),
        ])
    );
    let t = as_tuple(cb.chunks[0].column(0));
    assert_eq!(t.len(), 3);
    match &t.fields[1] {
        Column::Utf8(c) => {
            assert_eq!(c.null_count(), 1);
            let bm = c.validity.as_ref().expect("element validity");
            assert!(bm.is_valid(0) && !bm.is_valid(1) && bm.is_valid(2));
        }
        other => panic!("expected Utf8 element, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_tuple() {
    // Nullable(Tuple(Int32, String)): the ordinary Nullable framing, the
    // per-row null map first, then the tuple body. Null rows still carry
    // placeholder element values.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("t", "Nullable(Tuple(Int32, String))")
        .null_map(&[false, true, false])
        .int32_data(&[13, 0, 79])
        .string_data(&["user_1", "", "user_2"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let t = as_tuple(cb.chunks[0].column(0));
    assert_eq!(t.len(), 3);
    assert_eq!(t.null_count(), 1);
    let bm = t.validity.as_ref().expect("tuple-level validity");
    assert!(bm.is_valid(0) && !bm.is_valid(1) && bm.is_valid(2));
    match &t.fields[0] {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), &[13, 0, 79]),
        other => panic!("expected Int32 element, got {other:?}"),
    }
}

#[test]
fn test_decode_tuple_low_cardinality_element_prefix_is_hoisted() {
    // Tuple(Int32, LowCardinality(String)): the element state prefixes are
    // written at the very FRONT of the whole Tuple column in declaration
    // order (SerializationTuple delegates), so the LC 8-byte key version
    // precedes even element 0's Int32 run, and the LC body itself (element
    // 1) carries no key version of its own.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("t", "Tuple(Int32, LowCardinality(String))")
        // Hoisted prefix: element 1's LC key version.
        .uint64_data(&[LOW_CARDINALITY_KEY_VERSION])
        // Element 0: the full Int32 run.
        .int32_data(&[13, 79, -7])
        // Element 1: the LC body WITHOUT its key version: index word
        // (width tag 0 = u8, additional keys), dictionary, row count,
        // indices.
        .uint64_data(&[LC_HAS_ADDITIONAL_KEYS_BIT])
        .uint64_data(&[2]) // num_keys
        .string_data(&["red", "green"])
        .uint64_data(&[3]) // num_rows restated
        .raw_bytes(&[0, 1, 0]) // u8 indices
        .build();

    // The completeness scan and the decode must agree on the framing.
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let t = as_tuple(cb.chunks[0].column(0));
    assert_eq!(t.len(), 3);
    match &t.fields[1] {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![0, 1, 0]);
            match d.values.as_ref() {
                Column::Utf8(c) => {
                    assert_eq!(c.value(0), b"red");
                    assert_eq!(c.value(1), b"green");
                }
                other => panic!("expected Utf8 dictionary, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary element, got {other:?}"),
    }
}

#[test]
fn test_decode_array_of_tuple() {
    // Array(Tuple(Int32, Int32)): offsets first (the tuple elements write no
    // prefix), then the flattened tuple body: element 0's full run of
    // total_elements rows, then element 1's. Rows: [], [(13, 79)],
    // [(1, 2), (3, 4)], [(-1, -2)].
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("a", "Array(Tuple(Int32, Int32))")
        .array_offsets(&[0, 1, 3, 4])
        .int32_data(&[13, 1, 3, -1])
        .int32_data(&[79, 2, 4, -2])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Array(arr) => {
            assert_eq!(arr.offsets, vec![0i64, 0, 1, 3, 4]);
            let t = as_tuple(arr.values.as_ref());
            assert_eq!(t.len(), 4);
            match (&t.fields[0], &t.fields[1]) {
                (Column::Int32(a), Column::Int32(b)) => {
                    assert_eq!(a.values.as_slice(), &[13, 1, 3, -1]);
                    assert_eq!(b.values.as_slice(), &[79, 2, 4, -2]);
                }
                other => panic!("expected Int32 elements, got {other:?}"),
            }
        }
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn test_decode_nested_tuple() {
    // Tuple(p Tuple(Int8, Int8), s String): the inner tuple's body is its
    // own two element runs, nested in declaration order inside the outer
    // tuple's element sequence.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("t", "Tuple(p Tuple(Int8, Int8), s String)")
        .int8_data(&[1, 3])
        .int8_data(&[2, 4])
        .string_data(&["user_1", "user_2"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let outer = as_tuple(cb.chunks[0].column(0));
    assert_eq!(outer.len(), 2);
    let inner = as_tuple(&outer.fields[0]);
    assert_eq!(inner.len(), 2);
    match (&inner.fields[0], &inner.fields[1]) {
        (Column::Int8(a), Column::Int8(b)) => {
            assert_eq!(a.values.as_slice(), &[1, 3]);
            assert_eq!(b.values.as_slice(), &[2, 4]);
        }
        other => panic!("expected Int8 elements, got {other:?}"),
    }
}

#[test]
fn test_decode_empty_tuple() {
    // Tuple(): exactly one placeholder byte per row and nothing else. The
    // server writes ASCII '0' and ignores the values on read (tryIgnore),
    // so arbitrary byte values decode too.
    for body in [b"0000".as_slice(), &[0xAB, 0x00, 0x30, 0xFF]] {
        let data = BlockBuilder::new()
            .header(1, 4)
            .column_header("t", "Tuple()")
            .raw_bytes(body)
            .build();

        assert_eq!(
            block_end(&data, &DecodeOptions::default()).unwrap(),
            Some(data.len())
        );
        let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
        let t = as_tuple(cb.chunks[0].column(0));
        assert_eq!(t.len(), 4);
        assert!(t.fields.is_empty());
    }

    // Truncated placeholder bytes are "need more bytes", on both paths.
    let short = BlockBuilder::new()
        .header(1, 4)
        .column_header("t", "Tuple()")
        .raw_bytes(b"000")
        .build();
    assert!(matches!(
        decode_all_bytes(&short, &DecodeOptions::default()),
        Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
    assert!(matches!(
        block_end(&short, &DecodeOptions::default()),
        Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_decode_tuple_zero_rows() {
    // A zero-row block carries only the headers: no element bodies and no
    // Tuple() placeholder bytes.
    for type_name in ["Tuple(Int32, String)", "Tuple()", "Nullable(Tuple(Int8))"] {
        let data = BlockBuilder::new()
            .header(1, 0)
            .column_header("t", type_name)
            .build();
        let batch = decode_next_block(&mut ByteReader::new(&data), &DecodeOptions::default())
            .unwrap()
            .unwrap();
        let t = as_tuple(batch.column(0));
        assert_eq!(t.len(), 0);
        assert_eq!(
            block_end(&data, &DecodeOptions::default()).unwrap(),
            Some(data.len())
        );
    }
}

#[test]
fn test_tuple_truncated_element_body_is_eof_not_panic() {
    // Truncate inside element 1's String run: both the decode and the
    // completeness scan must report "need more bytes" (UnexpectedEof), the
    // signal the streaming decoder waits on, and never panic.
    let full = BlockBuilder::new()
        .header(1, 3)
        .column_header("t", "Tuple(Int32, String)")
        .int32_data(&[13, 79, -7])
        .string_data(&["user_1", "user_2", "user_3"])
        .build();

    for end in [full.len() - 1, full.len() - 8, full.len() - 20] {
        let truncated = &full[..end];
        assert!(matches!(
            decode_all_bytes(truncated, &DecodeOptions::default()),
            Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert!(matches!(
            block_end(truncated, &DecodeOptions::default()),
            Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
    }
}

#[test]
fn test_array_of_tuple_all_empty_passes_zero_limit_to_elements() {
    // Array(Tuple(LowCardinality(String), Int32)) with rows > 0 but every
    // array empty: the hoisted prefix walk still runs (the LC key version
    // is at the very front), the offsets are all zero, and the element
    // bodies are entirely absent; the LC element's limit == 0 early-return
    // gate must fire through the Tuple element path.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("a", "Array(Tuple(LowCardinality(String), Int32))")
        .uint64_data(&[LOW_CARDINALITY_KEY_VERSION]) // hoisted LC prefix
        .array_offsets(&[0, 0, 0])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Array(arr) => {
            assert_eq!(arr.offsets, vec![0i64, 0, 0, 0]);
            let t = as_tuple(arr.values.as_ref());
            assert_eq!(t.len(), 0);
            match &t.fields[0] {
                Column::Dictionary(d) => {
                    assert!(d.indices.is_empty());
                    assert_eq!(d.values.len(), 0);
                }
                other => panic!("expected empty Dictionary element, got {other:?}"),
            }
        }
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn test_decode_map_plain() {
    // Map(String, Int32): the Array(Tuple(keys, values)) wire layout, the
    // cumulative end-offsets then the flattened key run then the flattened
    // value run. Rows: {} / {a: 13} / {a: 1, b: 2} / {k: -7}.
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("m", "Map(String, Int32)")
        .array_offsets(&[0, 1, 3, 4])
        .string_data(&["a", "a", "b", "k"])
        .int32_data(&[13, 1, 2, -7])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let m = as_map(cb.chunks[0].column(0));
    assert_eq!(m.len(), 4);
    assert_eq!(m.offsets, vec![0i64, 0, 1, 3, 4]);
    assert_eq!(m.null_count(), 0);
    let (keys, values) = map_entries(m);
    match keys {
        Column::Utf8(c) => {
            assert_eq!(c.len(), 4);
            assert_eq!(c.value(0), b"a");
            assert_eq!(c.value(2), b"b");
            assert_eq!(c.value(3), b"k");
        }
        other => panic!("expected Utf8 keys, got {other:?}"),
    }
    match values {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), &[13, 1, 2, -7]),
        other => panic!("expected Int32 values, got {other:?}"),
    }
}

#[test]
fn test_decode_map_low_cardinality_key_prefix_is_hoisted() {
    // Map(LowCardinality(String), Int32): the prefix chain is Map -> Array
    // (nothing) -> Tuple -> key then value, so the LC 8-byte key version
    // sits at the very FRONT of the whole column, before the offsets; the
    // LC key run itself carries no key version.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("m", "Map(LowCardinality(String), Int32)")
        .uint64_data(&[LOW_CARDINALITY_KEY_VERSION]) // hoisted key prefix
        .array_offsets(&[1, 1, 3])
        // Flattened LC key run (3 entries), WITHOUT its key version.
        .uint64_data(&[LC_HAS_ADDITIONAL_KEYS_BIT])
        .uint64_data(&[2]) // num_keys
        .string_data(&["red", "green"])
        .uint64_data(&[3]) // entry count restated
        .raw_bytes(&[0, 1, 0]) // u8 indices
        // Flattened Int32 value run.
        .int32_data(&[13, 79, -7])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let m = as_map(cb.chunks[0].column(0));
    assert_eq!(m.offsets, vec![0i64, 1, 1, 3]);
    let (keys, values) = map_entries(m);
    match keys {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![0, 1, 0]);
            match d.values.as_ref() {
                Column::Utf8(c) => {
                    assert_eq!(c.value(0), b"red");
                    assert_eq!(c.value(1), b"green");
                }
                other => panic!("expected Utf8 dictionary, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary keys, got {other:?}"),
    }
    match values {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), &[13, 79, -7]),
        other => panic!("expected Int32 values, got {other:?}"),
    }
}

#[test]
fn test_decode_map_nullable_value() {
    // Map(Int32, Nullable(String)): the flattened value run is its own
    // per-entry null map then the strings, ordinary Nullable framing at the
    // value level. Rows: {1: user_1} / {2: NULL, 3: user_2}.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("m", "Map(Int32, Nullable(String))")
        .array_offsets(&[1, 3])
        .int32_data(&[1, 2, 3])
        .null_map(&[false, true, false])
        .string_data(&["user_1", "", "user_2"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let m = as_map(cb.chunks[0].column(0));
    assert_eq!(m.offsets, vec![0i64, 1, 3]);
    let (_, values) = map_entries(m);
    match values {
        Column::Utf8(c) => {
            assert_eq!(c.null_count(), 1);
            let bm = c.validity.as_ref().expect("value validity");
            assert!(bm.is_valid(0) && !bm.is_valid(1) && bm.is_valid(2));
            assert_eq!(c.value(0), b"user_1");
            assert_eq!(c.value(2), b"user_2");
        }
        other => panic!("expected Utf8 values, got {other:?}"),
    }
}

#[test]
fn test_decode_map_array_value() {
    // Map(String, Array(Int32)): the flattened value run is itself an
    // Array column over the entries: its own offsets then the leaf ints.
    // Rows: {a: [13]} / {b: [], c: [1, 2]}.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("m", "Map(String, Array(Int32))")
        .array_offsets(&[1, 3]) // map offsets: 3 entries
        .string_data(&["a", "b", "c"])
        .array_offsets(&[1, 1, 3]) // value-array offsets over 3 entries
        .int32_data(&[13, 1, 2])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let m = as_map(cb.chunks[0].column(0));
    assert_eq!(m.offsets, vec![0i64, 1, 3]);
    let (_, values) = map_entries(m);
    match values {
        Column::Array(arr) => {
            assert_eq!(arr.offsets, vec![0i64, 1, 1, 3]);
            match arr.values.as_ref() {
                Column::Int32(c) => assert_eq!(c.values.as_slice(), &[13, 1, 2]),
                other => panic!("expected Int32 leaf, got {other:?}"),
            }
        }
        other => panic!("expected Array values, got {other:?}"),
    }
}

#[test]
fn test_decode_array_of_map() {
    // Array(Map(String, Int32)): the outer Array offsets count maps; the
    // flattened element column is a Map over the total, with its own
    // offsets counting entries. Rows: [] / [{a: 1}] / [{b: 2}, {}].
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("a", "Array(Map(String, Int32))")
        .array_offsets(&[0, 1, 3]) // outer: 3 flattened maps
        .array_offsets(&[1, 2, 2]) // map offsets over the 3 maps
        .string_data(&["a", "b"])
        .int32_data(&[1, 2])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Array(arr) => {
            assert_eq!(arr.offsets, vec![0i64, 0, 1, 3]);
            let m = as_map(arr.values.as_ref());
            assert_eq!(m.len(), 3);
            assert_eq!(m.offsets, vec![0i64, 1, 2, 2]);
            let (keys, values) = map_entries(m);
            match (keys, values) {
                (Column::Utf8(k), Column::Int32(v)) => {
                    assert_eq!(k.value(0), b"a");
                    assert_eq!(k.value(1), b"b");
                    assert_eq!(v.values.as_slice(), &[1, 2]);
                }
                other => panic!("expected (Utf8, Int32) entries, got {other:?}"),
            }
        }
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn test_decode_nested_map() {
    // Map(String, Map(String, Int32)): the flattened value run is itself a
    // Map over the outer entries. Rows: {a: {x: 1}} / {b: {y: 2, z: 3}}.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("m", "Map(String, Map(String, Int32))")
        .array_offsets(&[1, 2]) // outer: 2 entries
        .string_data(&["a", "b"]) // outer keys
        .array_offsets(&[1, 3]) // inner map offsets over the 2 entries
        .string_data(&["x", "y", "z"]) // inner keys
        .int32_data(&[1, 2, 3]) // inner values
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let outer = as_map(cb.chunks[0].column(0));
    assert_eq!(outer.offsets, vec![0i64, 1, 2]);
    let (_, outer_values) = map_entries(outer);
    let inner = as_map(outer_values);
    assert_eq!(inner.offsets, vec![0i64, 1, 3]);
    let (inner_keys, inner_values) = map_entries(inner);
    match (inner_keys, inner_values) {
        (Column::Utf8(k), Column::Int32(v)) => {
            assert_eq!(k.value(0), b"x");
            assert_eq!(k.value(2), b"z");
            assert_eq!(v.values.as_slice(), &[1, 2, 3]);
        }
        other => panic!("expected (Utf8, Int32) inner entries, got {other:?}"),
    }
}

#[test]
fn test_decode_map_zero_rows() {
    // A zero-row block carries only the header: no prefix, no offsets, no
    // entry runs.
    for type_name in ["Map(String, Int32)", "Map(LowCardinality(String), UInt8)"] {
        let data = BlockBuilder::new()
            .header(1, 0)
            .column_header("m", type_name)
            .build();
        let batch = decode_next_block(&mut ByteReader::new(&data), &DecodeOptions::default())
            .unwrap()
            .unwrap();
        let m = as_map(batch.column(0));
        assert_eq!(m.len(), 0);
        assert_eq!(m.offsets, vec![0i64]);
        let (keys, values) = map_entries(m);
        assert_eq!(keys.len(), 0);
        assert_eq!(values.len(), 0);
        assert_eq!(
            block_end(&data, &DecodeOptions::default()).unwrap(),
            Some(data.len())
        );
    }
}

#[test]
fn test_map_all_empty_lc_key_has_no_body() {
    // Map(LowCardinality(String), Int32) with rows > 0 but every map empty:
    // the hoisted LC key version and the all-zero offsets are the whole
    // column; the key and value runs are entirely absent (limit == 0 gates
    // through the Map path).
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("m", "Map(LowCardinality(String), Int32)")
        .uint64_data(&[LOW_CARDINALITY_KEY_VERSION])
        .array_offsets(&[0, 0, 0])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let m = as_map(cb.chunks[0].column(0));
    assert_eq!(m.offsets, vec![0i64, 0, 0, 0]);
    let (keys, values) = map_entries(m);
    match keys {
        Column::Dictionary(d) => {
            assert!(d.indices.is_empty());
            assert_eq!(d.values.len(), 0);
        }
        other => panic!("expected empty Dictionary keys, got {other:?}"),
    }
    assert_eq!(values.len(), 0);
}

#[test]
fn test_map_truncated_is_eof_not_panic() {
    // Truncate at several points (inside the value run, inside the key
    // run, inside the offsets): decode and scan must both report "need
    // more bytes" (UnexpectedEof), never panic.
    let full = BlockBuilder::new()
        .header(1, 3)
        .column_header("m", "Map(String, Int32)")
        .array_offsets(&[1, 2, 4])
        .string_data(&["a", "b", "c", "d"])
        .int32_data(&[1, 2, 3, 4])
        .build();

    for end in [full.len() - 1, full.len() - 17, full.len() - 30] {
        let truncated = &full[..end];
        assert!(matches!(
            decode_all_bytes(truncated, &DecodeOptions::default()),
            Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert!(matches!(
            block_end(truncated, &DecodeOptions::default()),
            Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
    }
}

#[test]
fn test_map_illegal_headers_rejected() {
    // Nullable-key and LowCardinality(Nullable)-key maps violate the
    // server's DataTypeMap::isValidKeyType and are rejected at header time
    // on both paths; LowCardinality(Map) violates
    // canBeInsideLowCardinality. All regardless of row count.
    // (Nullable(Map) is rejected by the parser itself; see
    // test_parse_ch_type_map.)
    for bad in [
        "Map(Nullable(String), Int32)",
        "Map(LowCardinality(Nullable(String)), Int32)",
        "LowCardinality(Map(String, Int32))",
        "Array(Map(Nullable(String), Int32))",
    ] {
        for num_rows in [0usize, 1] {
            let data = BlockBuilder::new()
                .header(1, num_rows)
                .column_header("m", bad)
                .build();
            assert!(
                matches!(
                    decode_all_bytes(&data, &DecodeOptions::default()),
                    Err(DecodeError::UnsupportedType { .. })
                ),
                "decode should reject {bad:?} at {num_rows} rows"
            );
            assert!(
                matches!(
                    block_end(&data, &DecodeOptions::default()),
                    Err(DecodeError::UnsupportedType { .. })
                ),
                "scan should reject {bad:?} at {num_rows} rows"
            );
        }
    }
}

#[test]
fn test_decode_nested_plain() {
    // Nested(a UInt32, b String) = Array(Tuple(a UInt32, b String)): offsets,
    // then the flattened tuple body (all a's then all b's), field-major.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("n", "Nested(a UInt32, b String)")
        .array_offsets(&[2, 3]) // row 0 has 2 elements, row 1 has 1
        .uint32_data(&[10, 20, 30])
        .string_data(&["x", "y", "z"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::Nested(vec![
            ("a".to_string(), ChType::UInt32),
            ("b".to_string(), ChType::String),
        ])
    );
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    let t = as_tuple(arr.values.as_ref());
    match (&t.fields[0], &t.fields[1]) {
        (Column::UInt32(a), Column::Utf8(b)) => {
            assert_eq!(a.values, vec![10, 20, 30]);
            assert_eq!(b.value(0), b"x");
            assert_eq!(b.value(2), b"z");
        }
        other => panic!("expected (UInt32, Utf8) tuple fields, got {other:?}"),
    }
}

#[test]
fn test_decode_nested_with_low_cardinality_hoists_key_version() {
    // Shared gate: Nested(a LowCardinality(String)) delegates to
    // Array(Tuple(a LowCardinality(String))). The state prefix recurses
    // Array -> Tuple -> LowCardinality, so the LC 8-byte key version is
    // hoisted to the very front of the column, before the offsets, then the
    // LC body follows the offsets.
    let dictionary = ["", "user_1", "user_2"];
    let element_indices = [1u64, 2, 1];
    let lc_full = BlockBuilder::new()
        .low_cardinality_string(&dictionary, &element_indices, 1)
        .build();
    let (key_version, lc_body) = lc_full.split_at(8);

    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("n", "Nested(a LowCardinality(String))")
        .raw_bytes(key_version) // hoisted LC key version, ahead of the offsets
        .array_offsets(&[2, 3])
        .raw_bytes(lc_body)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    let t = as_tuple(arr.values.as_ref());
    match &t.fields[0] {
        Column::Dictionary(d) => assert_eq!(d.indices, vec![1, 2, 1]),
        other => panic!("expected LC dictionary element, got {other:?}"),
    }
    // The scan agrees on the framing (including the hoisted prefix).
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_decode_rejects_unnamed_nested_element_header() {
    // An unnamed Nested element makes the whole header unsupported.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("n", "Nested(UInt32)")
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_decode_rejects_nested_low_cardinality_bad_inner() {
    // A forbidden LowCardinality inner nested inside a Nested field is
    // rejected at header time on both paths, at every row count, because
    // validate_header_type expands the Nested delegate and recurses.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("n", "Nested(a LowCardinality(Decimal(9, 4)))")
            .build();
        assert!(matches!(
            decode_all_bytes(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
        assert!(matches!(
            block_end(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
    }
}
