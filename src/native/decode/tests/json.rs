//! JSON (`DataTypeObject`) decode tests. These synthesize the Native wire bytes
//! from the layout confirmed against the server source at v26.6.1.1193-stable
//! (`SerializationObject`, `SerializationObjectSharedData`, `DataTypeObject`),
//! then decode and assert the columnar result. A shared wrong assumption would
//! pass here, so a real-server fixture follows in a later phase.

use super::*;
use crate::column::{Column, DynamicChild, JsonBody, JsonColumn};

fn push_str(buf: &mut Vec<u8>, s: &[u8]) {
    write_varint(buf, s.len() as u64);
    buf.extend_from_slice(s);
}

/// A full per-path Dynamic state prefix at structure `version` (1 = V1, 2 = V2,
/// 3 = FLATTENED) over the given runtime type names. Mirrors what a JSON
/// dynamic/flattened path carries. V1/V2 append the Variant BASIC mode word;
/// FLATTENED (word 3) does not.
fn dynamic_prefix(version: u64, types: &[&str]) -> Vec<u8> {
    let mut bytes = version.to_le_bytes().to_vec();
    if version == 1 {
        // V1's legacy count slot (ignored on read).
        write_varint(&mut bytes, types.len() as u64);
    }
    write_varint(&mut bytes, types.len() as u64);
    for name in types {
        push_str(&mut bytes, name.as_bytes());
    }
    if version == 1 || version == 2 {
        // Variant BASIC discriminator mode word.
        bytes.extend_from_slice(&0u64.to_le_bytes());
    }
    bytes
}

fn as_json(column: &Column) -> &JsonColumn {
    match column {
        Column::Json(column) => column,
        other => panic!("expected JSON column, got {other:?}"),
    }
}

fn structured(column: &JsonColumn) -> &crate::column::StructuredJson {
    match &column.body {
        JsonBody::Structured(structured) => structured.as_ref(),
        JsonBody::Text(_) => panic!("expected a structured JSON body"),
    }
}

#[test]
fn decode_json_v2_typed_dynamic_shared() {
    // JSON(a Int64) over 2 rows, one dynamic path "b" (Dynamic containing
    // String), and shared data carrying one (path, value) pair on row 0.
    let mut prefix = 2u64.to_le_bytes().to_vec(); // V2 structure word
    write_varint(&mut prefix, 1); // dynamic path count
    push_str(&mut prefix, b"b"); // dynamic path name
                                 // typed path "a" (Int64) has no state prefix.
    prefix.extend_from_slice(&dynamic_prefix(2, &["String"])); // "b" Dynamic prefix
                                                               // shared-data prefix: none.

    // Dynamic "b" child order sorts SharedVariant (0) before String (1).
    let shared_value = [0x0a, 0x0d, 0x00]; // opaque descriptor + payload bytes
    let mut body = Vec::new();
    // typed path "a" Int64 body.
    body.extend_from_slice(&13i64.to_le_bytes());
    body.extend_from_slice(&79i64.to_le_bytes());
    // dynamic path "b" body: discriminators then dense child bodies.
    body.extend_from_slice(&[1, u8::MAX]); // row 0 -> String, row 1 -> NULL
    push_str(&mut body, b"hello"); // String child (1 row)
                                   // shared data: offsets (cumulative end-offset per row), then paths, values.
    body.extend_from_slice(&1u64.to_le_bytes()); // row 0 ends at pair 1
    body.extend_from_slice(&1u64.to_le_bytes()); // row 1 ends at pair 1 (empty)
    push_str(&mut body, b"c.d"); // the one shared path
    push_str(&mut body, &shared_value); // the one opaque shared value

    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("j", "JSON(a Int64)")
        .raw_bytes(&prefix)
        .raw_bytes(&body)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len()),
        "scan must stop at the exact block end"
    );
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let json = as_json(decoded.chunks[0].column(0));
    assert_eq!(json.len(), 2);
    assert_eq!(json.null_count(), 0);
    let s = structured(json);

    // Typed path "a".
    assert_eq!(s.typed.len(), 1);
    assert_eq!(s.typed[0].0, "a");
    match &s.typed[0].1 {
        Column::Int64(c) => assert_eq!(c.values, vec![13, 79]),
        other => panic!("expected Int64 typed path, got {other:?}"),
    }

    // Dynamic path "b".
    assert_eq!(s.dynamic.len(), 1);
    assert_eq!(s.dynamic[0].0, "b");
    let dynamic = &s.dynamic[0].1;
    assert_eq!(dynamic.type_ids, vec![1, u32::MAX]);
    assert_eq!(dynamic.null_count(), 1);
    match &dynamic.children[1] {
        DynamicChild::Typed { ch_type, values } => {
            assert_eq!(ch_type, &ChType::String);
            match values {
                Column::Utf8(v) => assert_eq!(v.value(0), b"hello"),
                other => panic!("expected Utf8 values, got {other:?}"),
            }
        }
        other => panic!("expected typed String child, got {other:?}"),
    }

    // Shared data.
    assert_eq!(s.shared_offsets, vec![0, 1, 1]);
    assert_eq!(s.shared_paths.value(0), b"c.d");
    assert_eq!(s.shared_values.value(0), &shared_value);
}

#[test]
fn decode_json_v1_legacy_count_slot() {
    // V1 (structure word 0) writes a legacy count slot before the real count.
    // The reader must skip its value and still land on the path list.
    let mut prefix = 0u64.to_le_bytes().to_vec(); // V1 structure word
    write_varint(&mut prefix, 1); // legacy slot (ignored value = path count)
    write_varint(&mut prefix, 1); // dynamic path count
    push_str(&mut prefix, b"p");
    prefix.extend_from_slice(&dynamic_prefix(1, &["UInt64"]));

    let mut body = Vec::new();
    // dynamic path "p": children [SharedVariant(0), UInt64(1)].
    body.extend_from_slice(&[1]); // row 0 -> UInt64
    body.extend_from_slice(&13u64.to_le_bytes()); // UInt64 child value
    body.extend_from_slice(&0u64.to_le_bytes()); // shared offsets: row 0 -> 0 pairs

    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("j", "JSON")
        .raw_bytes(&prefix)
        .raw_bytes(&body)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let json = as_json(decoded.chunks[0].column(0));
    let s = structured(json);
    assert_eq!(s.dynamic.len(), 1);
    assert_eq!(s.dynamic[0].0, "p");
    assert_eq!(s.dynamic[0].1.type_ids, vec![1]);
    assert_eq!(s.shared_offsets, vec![0, 0]);
}

#[test]
fn decode_json_string_mode() {
    // STRING (structure word 1): one re-serialized document string per row and
    // nothing else in the prefix. The declared typed paths are absent on the wire.
    let mut prefix = 1u64.to_le_bytes().to_vec(); // STRING structure word
    let mut body = Vec::new();
    push_str(&mut body, br#"{"a":13}"#);
    push_str(&mut body, br#"{"a":79}"#);
    prefix.extend_from_slice(&body);

    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("j", "JSON(a Int64)")
        .raw_bytes(&prefix)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let json = as_json(decoded.chunks[0].column(0));
    assert_eq!(json.len(), 2);
    match &json.body {
        JsonBody::Text(values) => {
            assert_eq!(values.value(0), br#"{"a":13}"#);
            assert_eq!(values.value(1), br#"{"a":79}"#);
        }
        JsonBody::Structured(_) => panic!("STRING mode must decode to a Text body"),
    }
}

#[test]
fn decode_json_flattened_has_no_shared_stream() {
    // FLATTENED (structure word 3): typed paths then one full Dynamic per
    // flattened path, with NO shared-data stream. Decodes to a structured column
    // whose shared data is empty (offsets are all-zero, one per row).
    let mut prefix = 3u64.to_le_bytes().to_vec(); // FLATTENED structure word
    write_varint(&mut prefix, 1); // flattened path count
    push_str(&mut prefix, b"q");
    prefix.extend_from_slice(&dynamic_prefix(2, &["String"]));

    let mut body = Vec::new();
    // flattened path "q" Dynamic body: children [SharedVariant(0), String(1)].
    body.extend_from_slice(&[1]); // row 0 -> String
    push_str(&mut body, b"user_1");
    // NO shared-data stream.

    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("j", "JSON")
        .raw_bytes(&prefix)
        .raw_bytes(&body)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let json = as_json(decoded.chunks[0].column(0));
    let s = structured(json);
    assert_eq!(s.dynamic.len(), 1);
    assert_eq!(s.dynamic[0].0, "q");
    // Empty shared data with one offset per row plus the Arrow leading zero.
    assert_eq!(s.shared_offsets, vec![0, 0]);
    assert!(s.shared_paths.is_empty());
    assert!(s.shared_values.is_empty());
}

#[test]
fn decode_flattened_path_count_may_exceed_max_dynamic_paths() {
    // FLATTENED writes the union of dynamic and distinct shared-data paths, so
    // its count legitimately exceeds max_dynamic_paths; the server's FLATTENED
    // reader (unflattenAndInsertPaths, SerializationObjectHelpers.cpp,
    // v26.6.1.1193-stable) enforces no bound at all. Here max_dynamic_paths=1 but
    // three flattened paths must decode, each a full (shared-less) Dynamic.
    let mut prefix = 3u64.to_le_bytes().to_vec(); // FLATTENED
    write_varint(&mut prefix, 3); // three flattened paths, > max_dynamic_paths=1
    for path in ["a", "b", "c"] {
        push_str(&mut prefix, path.as_bytes());
    }
    for _ in 0..3 {
        prefix.extend_from_slice(&dynamic_prefix(3, &["String"]));
    }

    let mut body = Vec::new();
    // Each flattened path is a Dynamic over 1 row; route every row to NULL
    // (flattened index width 1, NULL index = num_children = 1).
    body.extend_from_slice(&[0x01; 3]);
    // FLATTENED carries no shared-data stream.

    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("j", "JSON(max_dynamic_paths=1)")
        .raw_bytes(&prefix)
        .raw_bytes(&body)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let json = as_json(decoded.chunks[0].column(0));
    let s = structured(json);
    assert_eq!(s.dynamic.len(), 3, "all three flattened paths must decode");
    assert_eq!(
        s.dynamic
            .iter()
            .map(|(p, _)| p.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );

    // The V1/V2 forms keep the strict bound: the same over-count on a V2 block is
    // rejected (the writer could never produce it, since overflow goes to shared).
    let mut v2 = 2u64.to_le_bytes().to_vec();
    write_varint(&mut v2, 3); // 3 > max_dynamic_paths=1
    let v2 = BlockBuilder::new()
        .header(1, 1)
        .column_header("j", "JSON(max_dynamic_paths=1)")
        .raw_bytes(&v2)
        .build();
    assert!(matches!(
        decode_all_bytes(&v2, &DecodeOptions::default()),
        Err(DecodeError::InvalidJson { .. })
    ));
}

#[test]
fn decode_pathless_flattened_with_fewer_bytes_than_rows() {
    // A pathless FLATTENED JSON column (no typed paths, no flattened paths, no
    // shared-data stream) writes ZERO body bytes per row, so a block of 100 empty
    // objects has fewer body bytes than rows. The old global row-count guard
    // rejected this; the type-aware guard must accept it, and the allocation-free
    // scan must agree (it never applied a row-count guard).
    let mut prefix = 3u64.to_le_bytes().to_vec(); // FLATTENED
    write_varint(&mut prefix, 0); // zero flattened paths
    let data = BlockBuilder::new()
        .header(1, 100)
        .column_header("j", "JSON")
        .raw_bytes(&prefix)
        .build();
    // Far fewer bytes remain than the 100 rows claimed.
    assert!(data.len() < 100);

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len()),
        "scan must accept a pathless FLATTENED block with no per-row bytes"
    );
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let json = as_json(decoded.chunks[0].column(0));
    assert_eq!(json.len(), 100);
    let s = structured(json);
    assert!(s.typed.is_empty());
    assert!(s.dynamic.is_empty());
    assert_eq!(s.shared_offsets, vec![0i64; 101]);
}

#[test]
fn reject_unbounded_pathless_flattened_row_counts() {
    // A pathless FLATTENED body writes zero bytes per row, so only decode_json's
    // own guards bound the header row count: the i32::MAX cap and the pathless
    // block-row ceiling. Each hostile count must return a clean error instead of
    // allocating num_rows + 1 offsets.
    let mut prefix = 3u64.to_le_bytes().to_vec(); // FLATTENED
    write_varint(&mut prefix, 0); // zero flattened paths
    for num_rows in [MAX_PATHLESS_JSON_ROWS + 1, 1usize << 45, 1usize << 61] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("j", "JSON")
            .raw_bytes(&prefix)
            .build();
        assert!(
            matches!(
                block_end(&data, &DecodeOptions::default()),
                Err(DecodeError::InvalidJson { .. })
            ),
            "scan must reject {num_rows} rows"
        );
        assert!(
            matches!(
                decode_all_bytes(&data, &DecodeOptions::default()),
                Err(DecodeError::InvalidJson { .. })
            ),
            "decode must reject {num_rows} rows"
        );
    }

    // The same body reached through a Tuple element (Tuple(JSON) also has no
    // one-byte-per-row guarantee, so no header guard applies).
    let data = BlockBuilder::new()
        .header(1, 1usize << 45)
        .column_header("t", "Tuple(JSON)")
        .raw_bytes(&prefix)
        .build();
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidJson { .. })
    ));
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidJson { .. })
    ));
}

#[test]
fn pathless_flattened_json_offsets_are_bounded_per_allocation() {
    use crate::native::stream_decoder::StreamDecoder;

    let mut prefix = 3u64.to_le_bytes().to_vec(); // FLATTENED
    write_varint(&mut prefix, 0); // zero flattened paths
    let num_rows = 4usize;
    let one_offsets_run = (num_rows + 1) * std::mem::size_of::<i64>();
    let options = |limit: usize| DecodeOptions {
        max_synthetic_allocation_bytes: limit,
        ..DecodeOptions::default()
    };

    // The limit bounds each synthesized offsets run individually, never
    // cumulatively: two pathless columns whose runs each fit exactly both
    // decode under a limit of one run.
    let two_columns = BlockBuilder::new()
        .header(2, num_rows)
        .column_header("j1", "JSON")
        .raw_bytes(&prefix)
        .column_header("j2", "JSON")
        .raw_bytes(&prefix)
        .build();
    assert_eq!(
        block_end(&two_columns, &options(one_offsets_run)).unwrap(),
        Some(two_columns.len())
    );
    let decoded = decode_all_bytes(&two_columns, &options(one_offsets_run)).unwrap();
    assert_eq!(as_json(decoded.chunks[0].column(0)).len(), num_rows);
    assert_eq!(as_json(decoded.chunks[0].column(1)).len(), num_rows);

    // Likewise across blocks in the complete and streaming decoders.
    let one_block = BlockBuilder::new()
        .header(1, num_rows)
        .column_header("j", "JSON")
        .raw_bytes(&prefix)
        .build();
    let two_blocks = [one_block.as_slice(), one_block.as_slice()].concat();
    assert_eq!(
        decode_all_bytes(&two_blocks, &options(one_offsets_run))
            .unwrap()
            .chunks
            .len(),
        2
    );
    let mut stream = StreamDecoder::new(options(one_offsets_run));
    assert_eq!(stream.feed(&two_blocks).unwrap().len(), 2);
    assert!(stream.finish().unwrap().is_empty());

    // A single run past the limit still fails.
    assert!(matches!(
        decode_all_bytes(&one_block, &options(one_offsets_run - 1)),
        Err(DecodeError::ResourceLimit {
            limit,
            requested,
            what: "FLATTENED JSON shared offsets",
        }) if limit == one_offsets_run - 1 && requested == one_offsets_run
    ));
}

#[test]
fn many_pathless_flattened_blocks_decode_with_no_cumulative_ceiling() {
    use crate::native::stream_decoder::StreamDecoder;

    // 520 blocks of 65536 rows synthesize ~273 MB of offsets in total, past
    // the old 256 MiB session-cumulative ceiling. Under default options every
    // block must decode.
    let mut prefix = 3u64.to_le_bytes().to_vec(); // FLATTENED
    write_varint(&mut prefix, 0); // zero flattened paths
    let num_rows = 65536usize;
    let one_block = BlockBuilder::new()
        .header(1, num_rows)
        .column_header("j", "JSON")
        .raw_bytes(&prefix)
        .build();

    let num_blocks = 520usize;
    assert!((num_rows + 1) * std::mem::size_of::<i64>() * num_blocks > 256 * 1024 * 1024);

    let mut stream = StreamDecoder::new(DecodeOptions::default());
    let mut total_rows = 0usize;
    for _ in 0..num_blocks {
        for batch in stream.feed(&one_block).unwrap() {
            total_rows += batch.num_rows;
        }
    }
    assert!(stream.finish().unwrap().is_empty());
    assert_eq!(total_rows, num_rows * num_blocks);
}

#[test]
fn with_paths_flattened_json_is_not_checked_against_the_allocation_limit() {
    // A FLATTENED column WITH a dynamic path is input-bounded (>= 1 byte/row
    // through its path columns), so its all-zero shared offsets are never
    // checked against the synthetic-allocation limit. Only the PATHLESS case,
    // whose body carries zero bytes per row, is checked. This column decodes
    // even under a limit one byte short of the (num_rows + 1) * 8 those
    // offsets occupy, proving the check is skipped for with-paths columns.
    let num_rows = 4usize;
    let mut prefix = 3u64.to_le_bytes().to_vec(); // FLATTENED structure word
    write_varint(&mut prefix, 1); // one flattened (dynamic) path
    push_str(&mut prefix, b"q");
    prefix.extend_from_slice(&dynamic_prefix(2, &["String"]));

    let mut body = Vec::new();
    // Dynamic "q" body: children [SharedVariant(0), String(1)]; all rows -> String.
    body.extend_from_slice(&[1u8; 4]); // one discriminator byte per row
    for value in [b"user_1", b"user_2", b"user_3", b"user_4"] {
        push_str(&mut body, value);
    }
    // FLATTENED carries no shared-data stream.

    let data = BlockBuilder::new()
        .header(1, num_rows)
        .column_header("j", "JSON")
        .raw_bytes(&prefix)
        .raw_bytes(&body)
        .build();

    // One byte short of a full offsets run: a pathless column would trip this,
    // a with-paths column is never checked against it.
    let shared_offsets_bytes = (num_rows + 1) * std::mem::size_of::<i64>();
    let options = DecodeOptions {
        max_synthetic_allocation_bytes: shared_offsets_bytes - 1,
        ..DecodeOptions::default()
    };

    let decoded = decode_all_bytes(&data, &options).unwrap();
    let json = as_json(decoded.chunks[0].column(0));
    let s = structured(json);
    assert_eq!(s.dynamic.len(), 1);
    assert_eq!(s.dynamic[0].0, "q");
    assert_eq!(s.dynamic[0].1.type_ids, vec![1, 1, 1, 1]);
    // The synthetic empty shared offsets are still present (Arrow leading 0 plus
    // one entry per row); they were never checked against the limit.
    assert_eq!(s.shared_offsets, vec![0i64; num_rows + 1]);
    assert!(s.shared_paths.is_empty());
    assert!(s.shared_values.is_empty());
}

#[test]
fn zero_width_looking_typed_paths_still_bound_the_row_count() {
    // `Nothing`'s BULK body is one placeholder byte per row (unlike its
    // zero-byte row-binary form), so a FLATTENED `JSON(a Nothing)` block with a
    // huge row count and a tiny buffer fails the bounds-checked typed-path skip
    // before the synthetic shared-offsets allocation is reached.
    let mut prefix = 3u64.to_le_bytes().to_vec(); // FLATTENED
    write_varint(&mut prefix, 0); // zero flattened paths
    let huge = BlockBuilder::new()
        .header(1, (i32::MAX - 1) as usize)
        .column_header("j", "JSON(a Nothing)")
        .raw_bytes(&prefix)
        .build();
    assert!(matches!(
        decode_all_bytes(&huge, &DecodeOptions::default()),
        Err(DecodeError::Io(_))
    ));
    assert!(matches!(
        block_end(&huge, &DecodeOptions::default()),
        Err(DecodeError::Io(_))
    ));

    // The same-count body with its placeholder bytes present decodes: the
    // typed path really does consume one byte per row.
    let mut body = prefix.clone();
    body.extend_from_slice(&[0x30; 3]); // Nothing placeholder bytes, 3 rows
    let valid = BlockBuilder::new()
        .header(1, 3)
        .column_header("j", "JSON(a Nothing)")
        .raw_bytes(&body)
        .build();
    let decoded = decode_all_bytes(&valid, &DecodeOptions::default()).unwrap();
    let s = structured(as_json(decoded.chunks[0].column(0)));
    assert_eq!(s.typed.len(), 1);
    assert_eq!(s.typed[0].1.len(), 3);
    assert_eq!(s.shared_offsets, vec![0i64; 4]);

    // A typed path that is itself a pathless FLATTENED JSON re-enters
    // decode_json, so the pathless ceiling fires recursively.
    let mut nested_prefix = 3u64.to_le_bytes().to_vec(); // outer FLATTENED
    write_varint(&mut nested_prefix, 0);
    nested_prefix.extend_from_slice(&3u64.to_le_bytes()); // inner FLATTENED
    write_varint(&mut nested_prefix, 0);
    let nested = BlockBuilder::new()
        .header(1, 1usize << 30)
        .column_header("j", "JSON(a JSON)")
        .raw_bytes(&nested_prefix)
        .build();
    assert!(matches!(
        decode_all_bytes(&nested, &DecodeOptions::default()),
        Err(DecodeError::InvalidJson { .. })
    ));
}

#[test]
fn json_state_exhaustion_is_a_json_error() {
    // A JSON body traversal that runs past the retained prefix states reports
    // an InvalidJson error, not an InvalidDynamic one.
    let mut cursor = 0usize;
    let allocation_budget = AllocationBudget::new(usize::MAX);
    let err = decode_json(
        &mut ByteReader::new(&[]),
        &parse_ch_type("JSON").unwrap(),
        1,
        "j",
        None,
        &[],
        &mut cursor,
        &allocation_budget,
    )
    .unwrap_err();
    assert!(matches!(err, DecodeError::InvalidJson { .. }), "{err}");
}

#[test]
fn json_named_arguments_accept_spaces_around_equals() {
    // Spaces around '=' parse and canonicalize to the compact form.
    let spaced = parse_ch_type("JSON(max_dynamic_paths = 8)").unwrap();
    assert_eq!(spaced.to_string(), "JSON(max_dynamic_paths=8)");
    let both = parse_ch_type("JSON(max_dynamic_paths=8, max_dynamic_types = 4)").unwrap();
    assert_eq!(
        both.to_string(),
        "JSON(max_dynamic_types=4, max_dynamic_paths=8)"
    );
    assert_eq!(
        parse_ch_type("JSON(max_dynamic_paths =8, max_dynamic_types= 4)"),
        Some(both)
    );

    // A backticked path whose NAME contains " = " stays a typed path.
    let tricky = parse_ch_type("JSON(`max_dynamic_paths = x` Int8)").unwrap();
    match &tricky {
        ChType::Json {
            typed_paths,
            max_dynamic_paths,
            ..
        } => {
            assert_eq!(
                typed_paths[0],
                ("max_dynamic_paths = x".to_string(), ChType::Int8)
            );
            assert_eq!(
                *max_dynamic_paths,
                crate::schema::JSON_DEFAULT_MAX_DYNAMIC_PATHS
            );
        }
        other => panic!("expected JSON, got {other:?}"),
    }
    assert_eq!(tricky.to_string(), "JSON(`max_dynamic_paths = x` Int8)");

    // A quoted literal elsewhere keeps its '=' untouched.
    let regexp = parse_ch_type("JSON(max_dynamic_paths = 8, SKIP REGEXP 'a = b')").unwrap();
    match &regexp {
        ChType::Json {
            skip_regexps,
            max_dynamic_paths,
            ..
        } => {
            assert_eq!(skip_regexps, &["a = b".to_string()]);
            assert_eq!(*max_dynamic_paths, 8);
        }
        other => panic!("expected JSON, got {other:?}"),
    }

    // A bare path merely named like a parameter still parses as a path.
    let path = parse_ch_type("JSON(max_dynamic_paths Int64)").unwrap();
    match &path {
        ChType::Json { typed_paths, .. } => {
            assert_eq!(
                typed_paths[0],
                ("max_dynamic_paths".to_string(), ChType::Int64)
            );
        }
        other => panic!("expected JSON, got {other:?}"),
    }
}

#[test]
fn reject_truncated_string_body() {
    // STRING mode still self-bounds: a block claiming 100 rows but carrying a
    // few document bytes fails on the per-row varint reads (UnexpectedEof), on
    // both the decode and the scan, even without a global row-count guard.
    let mut prefix = 1u64.to_le_bytes().to_vec(); // STRING structure word
    push_str(&mut prefix, br#"{"a":1}"#); // one document, then truncation
    let data = BlockBuilder::new()
        .header(1, 100)
        .column_header("j", "JSON")
        .raw_bytes(&prefix)
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(_))
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::Io(_))
    ));
}

#[test]
fn decode_nullable_json() {
    // Nullable(JSON): the per-row null map precedes the full JSON body.
    let mut prefix = 2u64.to_le_bytes().to_vec(); // V2
    write_varint(&mut prefix, 0); // no dynamic paths
                                  // no typed paths, no dynamic prefixes, no shared prefix.

    let mut body = Vec::new();
    // null map: row 0 valid, row 1 null.
    body.extend_from_slice(&[0x00, 0x01]);
    // shared offsets over 2 rows: both empty.
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());

    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("j", "Nullable(JSON)")
        .raw_bytes(&prefix)
        .raw_bytes(&body)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let json = as_json(decoded.chunks[0].column(0));
    assert_eq!(json.len(), 2);
    let validity = json
        .validity
        .as_ref()
        .expect("Nullable(JSON) keeps validity");
    assert!(validity.is_valid(0));
    assert!(!validity.is_valid(1));
    assert_eq!(json.null_count(), 1);
}

#[test]
fn decode_json_zero_rows_builds_empty_structured() {
    // A zero-row block writes only the column header; the empty column carries
    // the declared typed paths with empty child columns and no dynamic paths.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("j", "JSON(a Int64)")
        .build();
    let decoded = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    let empty = empty_column(&decoded.schema.fields[0].ch_type);
    let json = as_json(&empty);
    let s = structured(json);
    assert_eq!(json.len(), 0);
    assert_eq!(s.typed.len(), 1);
    assert_eq!(s.typed[0].0, "a");
    assert!(s.dynamic.is_empty());
    assert_eq!(s.shared_offsets, vec![0]);
}

#[test]
fn decode_json_multi_block_differing_dynamic_paths() {
    // Each block carries its own block-local dynamic path set.
    fn one_block(path: &str, ty: &str, value: &[u8]) -> Vec<u8> {
        let mut prefix = 2u64.to_le_bytes().to_vec();
        write_varint(&mut prefix, 1);
        push_str(&mut prefix, path.as_bytes());
        prefix.extend_from_slice(&dynamic_prefix(2, &[ty]));
        let mut body = Vec::new();
        // The single runtime type sorts after SharedVariant, so it is child 1.
        body.extend_from_slice(&[1]);
        body.extend_from_slice(value);
        body.extend_from_slice(&0u64.to_le_bytes()); // shared: 1 row, no pairs
        BlockBuilder::new()
            .header(1, 1)
            .column_header("j", "JSON")
            .raw_bytes(&prefix)
            .raw_bytes(&body)
            .build()
    }

    let mut string_value = Vec::new();
    push_str(&mut string_value, b"user_1");
    let mut bytes = one_block("first", "String", &string_value);
    bytes.extend_from_slice(&one_block("second", "UInt64", &79u64.to_le_bytes()));

    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    assert_eq!(
        structured(as_json(decoded.chunks[0].column(0))).dynamic[0].0,
        "first"
    );
    assert_eq!(
        structured(as_json(decoded.chunks[1].column(0))).dynamic[0].0,
        "second"
    );
}

#[test]
fn json_param_and_quoting_roundtrip() {
    // Parameters and typed-path quoting must round-trip parse -> Display -> parse.
    for spelling in [
        "JSON",
        "JSON(max_dynamic_types=16)",
        "JSON(max_dynamic_paths=64)",
        "JSON(max_dynamic_types=16, max_dynamic_paths=64)",
        "JSON(a Int64)",
        "JSON(a Int64, b String)",
        "JSON(`a.b` UInt64)",
        "JSON(`weird path` Int8, SKIP `x.y`, SKIP REGEXP '^tmp')",
        "JSON(max_dynamic_types=8, user_1 Int64, SKIP secret, SKIP REGEXP 'a,b')",
        "JSON(`SKIP` Int8)",
    ] {
        let parsed =
            parse_ch_type(spelling).unwrap_or_else(|| panic!("failed to parse {spelling}"));
        let rendered = parsed.to_string();
        assert_eq!(rendered, spelling, "Display did not round-trip {spelling}");
        assert_eq!(
            parse_ch_type(&rendered),
            Some(parsed),
            "reparse of {rendered} diverged"
        );
    }

    // Canonicalization: unsorted typed paths and out-of-order params normalize.
    let canonical = parse_ch_type("JSON(b String, a Int64, max_dynamic_paths=64)").unwrap();
    assert_eq!(
        canonical.to_string(),
        "JSON(max_dynamic_paths=64, a Int64, b String)"
    );

    // Parameter bounds are rejected.
    assert_eq!(parse_ch_type("JSON(max_dynamic_types=255)"), None);
    assert_eq!(parse_ch_type("JSON(max_dynamic_paths=10001)"), None);
    assert_eq!(
        parse_ch_type("JSON(max_dynamic_types=1, max_dynamic_types=2)"),
        None
    );

    // Illegal nesting: LowCardinality(JSON) is rejected, Nullable(JSON) is not.
    assert!(
        unsupported_header_type_name(&parse_ch_type("LowCardinality(JSON)").unwrap()).is_some()
    );
    assert_eq!(
        parse_ch_type("Nullable(JSON)"),
        Some(ChType::Nullable(Box::new(parse_ch_type("JSON").unwrap())))
    );
    // JSON is legal as a typed-path type inside JSON (nested).
    assert!(parse_ch_type("JSON(a JSON)").is_some());
}

#[test]
fn reject_malformed_json_structure() {
    // V3 (structure word 4) is MergeTree-only and never emitted by NativeWriter.
    let word_four = BlockBuilder::new()
        .header(1, 1)
        .column_header("j", "JSON")
        .raw_bytes(&4u64.to_le_bytes())
        .build();
    assert!(matches!(
        decode_all_bytes(&word_four, &DecodeOptions::default()),
        Err(DecodeError::InvalidJson { .. })
    ));

    // A garbage structure word.
    let garbage = BlockBuilder::new()
        .header(1, 1)
        .column_header("j", "JSON")
        .raw_bytes(&99u64.to_le_bytes())
        .build();
    assert!(matches!(
        decode_all_bytes(&garbage, &DecodeOptions::default()),
        Err(DecodeError::InvalidJson { .. })
    ));

    // Unsorted / duplicate dynamic paths (the server always writes them sorted).
    let mut unsorted = 2u64.to_le_bytes().to_vec();
    write_varint(&mut unsorted, 2);
    push_str(&mut unsorted, b"z");
    push_str(&mut unsorted, b"a");
    let unsorted = BlockBuilder::new()
        .header(1, 1)
        .column_header("j", "JSON")
        .raw_bytes(&unsorted)
        .build();
    assert!(matches!(
        decode_all_bytes(&unsorted, &DecodeOptions::default()),
        Err(DecodeError::InvalidJson { .. })
    ));

    // Dynamic path count over max_dynamic_paths.
    let mut over = 2u64.to_le_bytes().to_vec();
    write_varint(&mut over, 3); // 3 > max_dynamic_paths=2
    let over = BlockBuilder::new()
        .header(1, 1)
        .column_header("j", "JSON(max_dynamic_paths=2)")
        .raw_bytes(&over)
        .build();
    assert!(matches!(
        decode_all_bytes(&over, &DecodeOptions::default()),
        Err(DecodeError::InvalidJson { .. })
    ));
}

#[test]
fn reject_truncated_shared_and_bad_utf8() {
    // Truncated shared data: the offsets promise a pair the string bodies do
    // not carry, so the read runs off the end (UnexpectedEof).
    let mut prefix = 2u64.to_le_bytes().to_vec();
    write_varint(&mut prefix, 0); // no dynamic paths
    let mut body = Vec::new();
    body.extend_from_slice(&1u64.to_le_bytes()); // row 0 claims 1 pair
                                                 // ... but no path/value strings follow.
    prefix.extend_from_slice(&body);
    let truncated = BlockBuilder::new()
        .header(1, 1)
        .column_header("j", "JSON")
        .raw_bytes(&prefix)
        .build();
    assert!(matches!(
        decode_all_bytes(&truncated, &DecodeOptions::default()),
        Err(DecodeError::Io(_))
    ));

    // A dynamic path string that is not valid UTF-8 is rejected where the crate
    // reads a varint string (InvalidData surfaces as an Io error).
    let mut bad_utf8 = 2u64.to_le_bytes().to_vec();
    write_varint(&mut bad_utf8, 1);
    write_varint(&mut bad_utf8, 2);
    bad_utf8.extend_from_slice(&[0xff, 0xfe]); // invalid UTF-8 path bytes
    let bad_utf8 = BlockBuilder::new()
        .header(1, 1)
        .column_header("j", "JSON")
        .raw_bytes(&bad_utf8)
        .build();
    assert!(matches!(
        decode_all_bytes(&bad_utf8, &DecodeOptions::default()),
        Err(DecodeError::Io(_))
    ));
}

#[test]
fn deeply_nested_json_typed_paths_are_rejected_by_the_parser() {
    // A JSON header nesting JSON typed paths past MAX_TYPE_DEPTH is rejected by
    // the depth-bounded parser (surfacing as UnsupportedType), so no over-deep
    // ChType ever reaches the decoder.
    let mut spelling = String::new();
    for _ in 0..(MAX_TYPE_DEPTH + 2) {
        spelling.push_str("JSON(a ");
    }
    spelling.push_str("Int64");
    for _ in 0..(MAX_TYPE_DEPTH + 2) {
        spelling.push(')');
    }
    assert_eq!(parse_ch_type(&spelling), None);
}

#[test]
fn deeply_nested_json_in_dynamic_stream_errors_instead_of_overflowing() {
    // JSON reached through a runtime Dynamic type table restarts the per-type
    // depth budget at each level, so a hostile stream nesting Array(JSON) tens
    // of thousands deep must error, not overflow the stack. Both the allocating
    // decode and the completeness scan reject it.
    fn flattened_dynamic(types: &[&str]) -> Vec<u8> {
        let mut bytes = 3u64.to_le_bytes().to_vec();
        write_varint(&mut bytes, types.len() as u64);
        for name in types {
            push_str(&mut bytes, name.as_bytes());
        }
        bytes
    }
    let mut body = Vec::new();
    for _ in 0..100_000 {
        body.extend_from_slice(&flattened_dynamic(&["Array(JSON)"]));
    }
    body.extend_from_slice(&flattened_dynamic(&["String"]));
    let block = BlockBuilder::new()
        .header(1, 1)
        .column_header("v", "Dynamic")
        .raw_bytes(&body)
        .build();
    assert!(matches!(
        decode_all_bytes(&block, &DecodeOptions::default()),
        Err(DecodeError::InvalidJson { .. }) | Err(DecodeError::InvalidDynamic { .. })
    ));
    assert!(matches!(
        block_end(&block, &DecodeOptions::default()),
        Err(DecodeError::InvalidJson { .. }) | Err(DecodeError::InvalidDynamic { .. })
    ));
}
