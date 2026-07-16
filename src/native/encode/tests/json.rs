//! JSON (`DataTypeObject`) encode tests: exact inverse of the decode path.
//! Each round-trip builds the columnar buffers the decoder produces, encodes a
//! block, decodes it back, and asserts the buffers survived. Layout confirmed
//! against the server source at v26.6.1.1193-stable.

use super::*;
use crate::batch::ChunkedBatch;
use crate::column::{Column, DynamicChild, DynamicColumn, JsonColumn, StructuredJson};

/// FLATTENED opt-in options at `revision`.
fn flattened_options(revision: u64) -> EncodeOptions {
    EncodeOptions {
        protocol_revision: revision,
        flattened_dynamic: true,
    }
}

/// A canonical block-local `Dynamic` with a String runtime type and the leading
/// SharedVariant child, exactly the shape decode produces for a V1/V2 dynamic
/// path. `type_ids` route rows to child 1 (String) or `u32::MAX` (NULL).
fn dynamic_string_shared(type_ids: &[u32], strings: &[&[u8]]) -> DynamicColumn {
    DynamicColumn::try_new(
        type_ids,
        vec![
            DynamicChild::Shared(utf8_column(&[])),
            DynamicChild::Typed {
                ch_type: ChType::String,
                values: Column::Utf8(utf8_column(strings)),
            },
        ],
    )
    .unwrap()
}

/// A shared-less block-local `Dynamic`, the shape decode produces for a
/// FLATTENED dynamic path (child 0 is the sole String runtime type).
fn dynamic_string_flat(type_ids: &[u32], strings: &[&[u8]]) -> DynamicColumn {
    DynamicColumn::try_new(
        type_ids,
        vec![DynamicChild::Typed {
            ch_type: ChType::String,
            values: Column::Utf8(utf8_column(strings)),
        }],
    )
    .unwrap()
}

/// `JSON(a Int64)` over 2 rows: typed path "a", dynamic path "b" (String with a
/// SharedVariant), and one shared (path, value) pair on row 0.
fn structured_batch() -> ColBatch {
    let shared_value: &[u8] = &[0x0a, 0x0d, 0x00];
    let structured = StructuredJson::try_new(
        vec![(
            "a".into(),
            Column::Int64(PrimitiveColumn::new(vec![13, 79])),
        )],
        vec![(
            "b".into(),
            dynamic_string_shared(&[1, u32::MAX], &[b"hello"]),
        )],
        vec![0, 1, 1],
        utf8_column(&[b"c.d"]),
        utf8_column(&[shared_value]),
        2,
    )
    .unwrap();
    ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("JSON(a Int64)").unwrap(),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        2,
    )
}

#[test]
fn roundtrip_structured_v1_and_v2() {
    // rev 0 selects V1 (structure word 0), rev 54485 selects V2 (word 2). Both
    // carry the shared-data stream, so the buffers must survive unchanged.
    roundtrip(&structured_batch(), 0);
    roundtrip(&structured_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn structured_selects_v1_or_v2_by_revision() {
    // Read the JSON structure word out of a revision-0 single-column block for
    // the bare `JSON` type (all header varints are single-byte, no BlockInfo, no
    // custom-serialization marker).
    fn structure_word(bytes: &[u8]) -> u64 {
        let header = 2 + (1 + 1) + (1 + "JSON".len());
        u64::from_le_bytes(bytes[header..header + 8].try_into().unwrap())
    }
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("JSON").unwrap(),
        }]),
        vec![Column::Json(JsonColumn::structured(
            StructuredJson::try_new(
                Vec::new(),
                Vec::new(),
                vec![0, 0],
                utf8_column(&[]),
                utf8_column(&[]),
                1,
            )
            .unwrap(),
        ))],
        1,
    );
    let v1 = encode_block(&batch, &EncodeOptions::default()).unwrap();
    assert_eq!(structure_word(&v1), 0, "revision 0 must emit V1 (word 0)");
    let v2 = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
            ..EncodeOptions::default()
        },
    )
    .unwrap();
    // At the negotiated TCP revision the block carries a BlockInfo preamble and a
    // custom-serialization marker, so decode it back and confirm the structure
    // through a round-trip instead of a raw offset.
    let decoded = decode_all_bytes(
        &v2,
        &DecodeOptions {
            protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
        },
    )
    .unwrap();
    assert_batches_eq(&batch, &decoded.chunks[0]);
}

#[test]
fn roundtrip_flattened_opt_in() {
    // FLATTENED (structure word 3) is opt-in and carries no shared-data stream,
    // so the column must have empty shared data and shared-less dynamic paths.
    let structured = StructuredJson::try_new(
        vec![("a".into(), Column::Int64(PrimitiveColumn::new(vec![13])))],
        vec![("b".into(), dynamic_string_flat(&[0], &[b"user_1"]))],
        vec![0, 0],
        utf8_column(&[]),
        utf8_column(&[]),
        1,
    )
    .unwrap();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("JSON(a Int64)").unwrap(),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        1,
    );
    roundtrip_opts(&batch, &flattened_options(0));
    roundtrip_opts(&batch, &flattened_options(DBMS_TCP_PROTOCOL_VERSION));
}

/// `JSON(max_dynamic_paths=1)` with three shared-less dynamic paths and empty
/// shared data: legal only as a FLATTENED wire block.
fn over_limit_flattened_batch() -> ColBatch {
    let structured = StructuredJson::try_new(
        Vec::new(),
        vec![
            ("a".into(), dynamic_string_flat(&[0], &[b"x"])),
            ("b".into(), dynamic_string_flat(&[0], &[b"y"])),
            ("c".into(), dynamic_string_flat(&[0], &[b"z"])),
        ],
        vec![0, 0],
        utf8_column(&[]),
        utf8_column(&[]),
        1,
    )
    .unwrap();
    ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("JSON(max_dynamic_paths=1)").unwrap(),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        1,
    )
}

#[test]
fn roundtrip_flattened_path_count_over_max() {
    // FLATTENED legitimately carries more paths than max_dynamic_paths (the union
    // of dynamic and shared-data paths), so a column with 3 paths over a limit of
    // 1 round-trips through the FLATTENED wire shape.
    roundtrip_opts(&over_limit_flattened_batch(), &flattened_options(0));
    roundtrip_opts(
        &over_limit_flattened_batch(),
        &flattened_options(DBMS_TCP_PROTOCOL_VERSION),
    );
}

#[test]
fn roundtrip_pathless_flattened_more_rows_than_bytes() {
    // A pathless FLATTENED column (no typed/dynamic paths, empty shared) writes
    // no per-row body bytes, so 100 rows encode to a tiny block. Encoding it as
    // FLATTENED and decoding it back must reproduce all 100 rows.
    let structured = StructuredJson::try_new(
        Vec::new(),
        Vec::new(),
        vec![0i64; 101],
        utf8_column(&[]),
        utf8_column(&[]),
        100,
    )
    .unwrap();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("JSON").unwrap(),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        100,
    );
    let bytes = encode_block(&batch, &flattened_options(0)).unwrap();
    assert!(
        bytes.len() < 100,
        "pathless FLATTENED must have no per-row bytes"
    );
    roundtrip_opts(&batch, &flattened_options(0));
    roundtrip_opts(&batch, &flattened_options(DBMS_TCP_PROTOCOL_VERSION));
}

#[test]
fn encode_rejects_over_limit_paths_as_v1_v2() {
    // The same over-limit column encoded as V1/V2 (the FLATTENED opt-in off) is
    // rejected cleanly: that wire shape routes overflow into shared data, so the
    // server could never produce a V1/V2 block with more direct paths than the
    // limit.
    assert!(matches!(
        encode_block(&over_limit_flattened_batch(), &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}

#[test]
fn roundtrip_string_text_body() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("JSON").unwrap(),
        }]),
        vec![Column::Json(JsonColumn::text(utf8_column(&[
            br#"{"a":13}"#,
            br#"{"b":"user_1"}"#,
        ])))],
        2,
    );
    roundtrip(&batch, 0);
    roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_json() {
    let validity = Bitmap::from_ch_null_map(&[0x00, 0x01]);
    let structured = StructuredJson::try_new(
        Vec::new(),
        Vec::new(),
        vec![0, 0, 0],
        utf8_column(&[]),
        utf8_column(&[]),
        2,
    )
    .unwrap();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("Nullable(JSON)").unwrap(),
        }]),
        vec![Column::Json(
            JsonColumn::structured(structured).with_validity(Some(validity)),
        )],
        2,
    );
    roundtrip(&batch, 0);
    roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn zero_row_json_has_header_only() {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("JSON(a Int64)").unwrap(),
        }]),
        vec![Column::Json(JsonColumn::structured(
            StructuredJson::try_new(
                vec![("a".into(), Column::Int64(PrimitiveColumn::new(Vec::new())))],
                Vec::new(),
                vec![0],
                utf8_column(&[]),
                utf8_column(&[]),
                0,
            )
            .unwrap(),
        ))],
        0,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 0);
    assert_eq!(decoded.schema, batch.schema);
}

#[test]
fn roundtrip_multi_block_differing_dynamic_paths() {
    // Two chunks of the same `JSON` schema but different block-local dynamic
    // path sets, encoded and decoded as a chunked batch.
    fn chunk(path: &str, value: &[u8]) -> ColBatch {
        let structured = StructuredJson::try_new(
            Vec::new(),
            vec![(path.into(), dynamic_string_shared(&[1], &[value]))],
            vec![0, 0],
            utf8_column(&[]),
            utf8_column(&[]),
            1,
        )
        .unwrap();
        ColBatch::new(
            Schema::new(vec![Field {
                name: "j".into(),
                ch_type: parse_ch_type("JSON").unwrap(),
            }]),
            vec![Column::Json(JsonColumn::structured(structured))],
            1,
        )
    }
    let schema = Schema::new(vec![Field {
        name: "j".into(),
        ch_type: parse_ch_type("JSON").unwrap(),
    }]);
    let chunks = vec![
        std::sync::Arc::new(chunk("first", b"user_1")),
        std::sync::Arc::new(chunk("second", b"user_2")),
    ];
    let batch = ChunkedBatch { schema, chunks };
    let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    assert_batches_eq(&batch.chunks[0], &decoded.chunks[0]);
    assert_batches_eq(&batch.chunks[1], &decoded.chunks[1]);
}

#[test]
fn exact_bytes_pin_small_v2_column() {
    // A single-row bare `JSON` column with no typed or dynamic paths and empty
    // shared data, at the negotiated TCP revision (V2). Every byte is pinned.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("JSON").unwrap(),
        }]),
        vec![Column::Json(JsonColumn::structured(
            StructuredJson::try_new(
                Vec::new(),
                Vec::new(),
                vec![0, 0],
                utf8_column(&[]),
                utf8_column(&[]),
                1,
            )
            .unwrap(),
        ))],
        1,
    );
    let bytes = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
            ..EncodeOptions::default()
        },
    )
    .unwrap();
    #[rustfmt::skip]
    let expected: Vec<u8> = vec![
        // BlockInfo: is_overflows=false, bucket_num=-1, empty out_of_order_buckets.
        0x01, 0x00, 0x02, 0xFF, 0xFF, 0xFF, 0xFF, 0x03, 0x00, 0x00,
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'j', // column name "j"
        0x04, b'J', b'S', b'O', b'N', // type string "JSON"
        0x00, // custom-serialization marker = default
        // JSON structure prefix: V2 structure word, then dynamic path count 0.
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
        // Shared-data body: one i64 end-offset (row 0 -> 0 pairs), no strings.
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn encode_rejects_unsorted_dynamic_paths() {
    // Build past `try_new` (which would reject) via `from_parts` so encode's
    // own validation is what rejects the unsorted dynamic path list.
    let structured = StructuredJson::from_parts(
        Vec::new(),
        vec![
            ("z".into(), dynamic_string_shared(&[1], &[b"x"])),
            ("a".into(), dynamic_string_shared(&[1], &[b"y"])),
        ],
        vec![0, 0],
        utf8_column(&[]),
        utf8_column(&[]),
        1,
    );
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("JSON").unwrap(),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        1,
    );
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}

#[test]
fn encode_rejects_dynamic_paths_over_limit() {
    let structured = StructuredJson::try_new(
        Vec::new(),
        vec![
            ("a".into(), dynamic_string_shared(&[1], &[b"x"])),
            ("b".into(), dynamic_string_shared(&[1], &[b"y"])),
        ],
        vec![0, 0],
        utf8_column(&[]),
        utf8_column(&[]),
        1,
    )
    .unwrap();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("JSON(max_dynamic_paths=1)").unwrap(),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        1,
    );
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}

#[test]
fn encode_rejects_typed_path_length_mismatch() {
    // The typed path column carries the wrong number of rows.
    let structured = StructuredJson::from_parts(
        vec![("a".into(), Column::Int64(PrimitiveColumn::new(vec![13])))],
        Vec::new(),
        vec![0, 0, 0],
        utf8_column(&[]),
        utf8_column(&[]),
        2,
    );
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("JSON(a Int64)").unwrap(),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        2,
    );
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}

#[test]
fn encode_rejects_typed_path_set_mismatch() {
    // The buffer's typed path name does not match the declared JSON type.
    let structured = StructuredJson::try_new(
        vec![("x".into(), Column::Int64(PrimitiveColumn::new(vec![13])))],
        Vec::new(),
        vec![0, 0],
        utf8_column(&[]),
        utf8_column(&[]),
        1,
    )
    .unwrap();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "j".into(),
            ch_type: parse_ch_type("JSON(a Int64)").unwrap(),
        }]),
        vec![Column::Json(JsonColumn::structured(structured))],
        1,
    );
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::InconsistentBatch { .. })
    ));
}

#[test]
fn binary_type_descriptors_roundtrip() {
    // The JSON binary type descriptor (tag 0x30) round-trips through the
    // binary-header encode/decode entry points.
    let batch = structured_batch();
    for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
        let options = EncodeOptions {
            protocol_revision: revision,
            ..EncodeOptions::default()
        };
        let bytes = crate::native::encode::encode_block_binary_types(&batch, &options).unwrap();
        let decoded = crate::native::decode::decode_all_bytes_binary_types(
            &bytes,
            &DecodeOptions {
                protocol_revision: revision,
            },
        )
        .unwrap();
        assert_batches_eq(&batch, &decoded.chunks[0]);
    }
}
