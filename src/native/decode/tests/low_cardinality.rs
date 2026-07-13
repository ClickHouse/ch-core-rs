use super::*;

#[test]
fn test_decode_low_cardinality_string() {
    // Six rows over the dictionary the server actually emits: even for a
    // non-nullable inner type the wire reserves slot 0 with an empty string
    // (the ColumnUnique default), and the per-row indexes start at 1. Slot 0
    // is simply never referenced here; it is not a null sentinel. This
    // mirrors the live `lc` fixture (`['', 'user_1', ...]`, indices from 1)
    // rather than a slot-0-less layout the server never produces.
    let dictionary = ["", "user_1", "user_2", "user_3"];
    let indices = [1u64, 2, 3, 1, 2, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(String)")
        .low_cardinality_string(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), 6);
            assert_eq!(d.null_count(), 0);
            assert!(d.validity.is_none());
            assert_eq!(d.indices, vec![1, 2, 3, 1, 2, 1]);
            match d.values.as_ref() {
                Column::Utf8(v) => {
                    assert_eq!(v.len(), 4);
                    assert_eq!(v.value(0), b"");
                    assert_eq!(v.value(1), b"user_1");
                    assert_eq!(v.value(2), b"user_2");
                    assert_eq!(v.value(3), b"user_3");
                }
                other => panic!("expected Utf8 values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    let expected: [&[u8]; 6] = [
        b"user_1", b"user_2", b"user_3", b"user_1", b"user_2", b"user_1",
    ];
    for (row, want) in expected.iter().enumerate() {
        assert_eq!(lc_value(batch.column(0), row).as_deref(), Some(*want));
    }
}

#[test]
fn test_decode_low_cardinality_nullable_string() {
    // For Nullable(String) the dictionary's index 0 is the NULL sentinel,
    // with an empty-string on-wire value. Rows whose index is 0 are null.
    let dictionary = ["", "user_1", "user_2"];
    let indices = [1u64, 0, 2, 0, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(Nullable(String))")
        .low_cardinality_string(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), 5);
            assert_eq!(d.null_count(), 2);
            let bm = d.validity.as_ref().expect("nullable dictionary validity");
            assert!(bm.is_valid(0));
            assert!(!bm.is_valid(1));
            assert!(bm.is_valid(2));
            assert!(!bm.is_valid(3));
            assert!(bm.is_valid(4));
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    assert_eq!(
        lc_value(batch.column(0), 0).as_deref(),
        Some(b"user_1" as &[u8])
    );
    assert_eq!(lc_value(batch.column(0), 1), None);
    assert_eq!(
        lc_value(batch.column(0), 2).as_deref(),
        Some(b"user_2" as &[u8])
    );
    assert_eq!(lc_value(batch.column(0), 3), None);
    assert_eq!(
        lc_value(batch.column(0), 4).as_deref(),
        Some(b"user_1" as &[u8])
    );
}

#[test]
fn test_decode_low_cardinality_saf_nullable_string() {
    // `LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))` is
    // a real server header, hexdump-confirmed live at v26.6.1.1193-stable. The
    // SAF is a pure name decoration, so the wire body is byte-identical to
    // `LowCardinality(Nullable(String))`: a per-block dictionary whose slot 0
    // is the NULL sentinel, then per-row indexes. Decode must see through the
    // SAF chain and treat the column as nullable. Exercised at a bare stream
    // (rev 0) and full modern framing (rev 54485), with one nulls block and
    // one all-valid block in the same stream.
    let type_name = "LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))";
    let expected_type = ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
        func: "anyLast".to_string(),
        inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
    }));

    for revision in [0u64, DBMS_TCP_PROTOCOL_VERSION] {
        // Block 1: rows with nulls. Index 0 is the NULL sentinel.
        let nulls_dict = ["", "user_1", "user_2"];
        let nulls_indices = [1u64, 0, 2, 0, 1];
        let mut data = BlockBuilder::new()
            .revision(revision)
            .header(1, nulls_indices.len())
            .column_header("lc_nsaf", type_name)
            .low_cardinality_string(&nulls_dict, &nulls_indices, 1)
            .build();
        // Block 2: all valid, no index-0 references, still nullable at the type
        // level.
        let valid_dict = ["", "user_3", "user_4"];
        let valid_indices = [1u64, 2, 1];
        data.extend(
            BlockBuilder::new()
                .revision(revision)
                .header(1, valid_indices.len())
                .column_header("lc_nsaf", type_name)
                .low_cardinality_string(&valid_dict, &valid_indices, 1)
                .build(),
        );

        let options = DecodeOptions {
            protocol_revision: revision,
        };
        let cb = decode_all_bytes(&data, &options).unwrap();
        assert_eq!(cb.schema.fields[0].ch_type, expected_type);
        assert_eq!(cb.num_chunks(), 2);

        let nulls = &cb.chunks[0];
        match nulls.column(0) {
            Column::Dictionary(d) => {
                assert_eq!(d.len(), 5);
                assert_eq!(d.null_count(), 2);
                assert!(d.validity.is_some());
            }
            other => panic!("expected Dictionary, got {other:?}"),
        }
        let want: [Option<&[u8]>; 5] = [
            Some(b"user_1"),
            None,
            Some(b"user_2"),
            None,
            Some(b"user_1"),
        ];
        for (row, w) in want.iter().enumerate() {
            assert_eq!(lc_value(nulls.column(0), row).as_deref(), *w);
        }

        let valid = &cb.chunks[1];
        match valid.column(0) {
            Column::Dictionary(d) => {
                assert_eq!(d.len(), 3);
                assert_eq!(d.null_count(), 0);
            }
            other => panic!("expected Dictionary, got {other:?}"),
        }
        let want_valid: [&[u8]; 3] = [b"user_3", b"user_4", b"user_3"];
        for (row, w) in want_valid.iter().enumerate() {
            assert_eq!(lc_value(valid.column(0), row).as_deref(), Some(*w));
        }
    }
}

#[test]
fn test_decode_low_cardinality_saf_nullable_string_zero_rows() {
    // A zero-row block with the SAF-aliased LC header must build the same
    // empty column shape as `LowCardinality(Nullable(String))` (an empty
    // nullable dictionary), exercising the `empty_column` delegate path that
    // resolves the SAF chain through `low_cardinality_dict_value_type`.
    let type_name = "LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))";
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("lc_nsaf", type_name)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".to_string(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        }))
    );
    // The empty column is a nullable dictionary of non-nullable String values,
    // matching what the populated blocks decode.
    let empty = empty_column(&cb.schema.fields[0].ch_type);
    match empty {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), 0);
            assert!(d.validity.is_some());
            assert!(matches!(d.values.as_ref(), Column::Utf8(_)));
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_decode_low_cardinality_zero_rows() {
    // A zero-row block reads no LowCardinality prefix or data; it contributes
    // the schema and an empty dictionary column.
    let data = BlockBuilder::new()
        .header(2, 0)
        .column_header("lc", "LowCardinality(String)")
        .column_header("lcn", "LowCardinality(Nullable(String))")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 2);
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::String))
    );
    assert_eq!(
        cb.schema.fields[1].ch_type,
        ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String))))
    );
}

#[test]
fn test_low_cardinality_wider_index() {
    // A u32-wide index array (width tag 2) must widen correctly into i32.
    let dictionary = ["a", "b", "c"];
    let indices = [2u64, 0, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(String)")
        .low_cardinality_string(&dictionary, &indices, 4)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Dictionary(d) => assert_eq!(d.indices, vec![2, 0, 1]),
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_low_cardinality_rejects_bad_key_version() {
    // Key version != 1 is rejected (only SharedDictionariesWithAdditionalKeys
    // is valid in Native).
    let mut payload = Vec::new();
    payload.extend_from_slice(&2u64.to_le_bytes()); // bad key version
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("lc", "LowCardinality(String)")
        .raw_bytes(&payload)
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidLowCardinality { .. })
    ));
}

#[test]
fn test_low_cardinality_rejects_global_dictionary_bit() {
    // NeedGlobalDictionaryBit must be clear in Native; set it and the decoder
    // must reject rather than misread.
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u64.to_le_bytes()); // key version
    let index_word = LC_HAS_ADDITIONAL_KEYS_BIT | LC_NEED_GLOBAL_DICTIONARY_BIT;
    payload.extend_from_slice(&index_word.to_le_bytes());
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("lc", "LowCardinality(String)")
        .raw_bytes(&payload)
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidLowCardinality { .. })
    ));
}

#[test]
fn test_low_cardinality_rejects_out_of_range_index() {
    // An index value past the block dictionary size is corrupt data.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("lc", "LowCardinality(String)")
        .low_cardinality_string(&["x", "y"], &[0, 5], 1)
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidLowCardinality { .. })
    ));
}

#[test]
fn test_low_cardinality_modern_framing_roundtrip() {
    // Full v26.6.1.1193 framing (BlockInfo + per-column custom-serialization
    // byte) ahead of the LowCardinality payload.
    let data = BlockBuilder::new()
        .revision(DBMS_TCP_PROTOCOL_VERSION)
        .header(1, 4)
        .column_header("lc", "LowCardinality(String)")
        .low_cardinality_string(&["alpha", "beta"], &[0, 1, 1, 0], 1)
        .build();

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    let cb = decode_all_bytes(&data, &options).unwrap();
    let batch = &cb.chunks[0];
    let expected: [&[u8]; 4] = [b"alpha", b"beta", b"beta", b"alpha"];
    for (row, want) in expected.iter().enumerate() {
        assert_eq!(lc_value(batch.column(0), row).as_deref(), Some(*want));
    }
}

#[test]
fn test_decode_low_cardinality_uint32() {
    // The dictionary values are a plain UInt32 column body (raw 4-byte LE),
    // decoded via the shared per-type body decoder. Slot 0 is the server's
    // reserved default (0) and the per-row indexes start at 1, mirroring the
    // String layout.
    let dictionary = [0u32, 13, 79, 4_294_967_295];
    let indices = [1u64, 2, 3, 1, 2, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(UInt32)")
        .low_cardinality_u32(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), 6);
            assert_eq!(d.null_count(), 0);
            assert!(d.validity.is_none());
            assert_eq!(d.indices, vec![1, 2, 3, 1, 2, 1]);
            match d.values.as_ref() {
                Column::UInt32(v) => {
                    assert_eq!(v.values, vec![0, 13, 79, 4_294_967_295]);
                    assert!(v.validity.is_none());
                }
                other => panic!("expected UInt32 values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    let expected = [13u32, 79, 4_294_967_295, 13, 79, 13];
    for (row, want) in expected.iter().enumerate() {
        assert_eq!(lc_u32_value(batch.column(0), row), Some(*want));
    }
}

#[test]
fn test_decode_low_cardinality_ipv4() {
    // IPv4 is allowlisted inside LowCardinality and its dictionary body is a
    // plain UInt32 column body (raw 4-byte LE), so it shares the UInt32 layout.
    // Slot 0 is the reserved default; the per-row indexes start at 1.
    let dictionary = [0u32, 0x7F00_0001, 0x0808_0808];
    let indices = [1u64, 2, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(IPv4)")
        .low_cardinality_u32(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![1, 2, 1]);
            assert!(d.validity.is_none());
            match d.values.as_ref() {
                Column::Ipv4(v) => {
                    assert_eq!(v.values, vec![0, 0x7F00_0001, 0x0808_0808]);
                }
                other => panic!("expected Ipv4 values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    // The scan must consume exactly the same bytes.
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_decode_low_cardinality_ipv6() {
    // IPv6 is allowlisted inside LowCardinality; its dictionary body is raw
    // 16-byte rows, the same shape as UUID and FixedString(16).
    let zero = [0u8; 16];
    let loopback = {
        let mut v = [0u8; 16];
        v[15] = 1;
        v
    };
    let dictionary = [zero, loopback];
    let indices = [1u64, 0, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(IPv6)")
        .low_cardinality_fixed16(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![1, 0, 1]);
            assert!(d.validity.is_none());
            match d.values.as_ref() {
                Column::Ipv6(v) => {
                    assert_eq!(v.width, 16);
                    assert_eq!(v.value(0), &zero);
                    assert_eq!(v.value(1), &loopback);
                }
                other => panic!("expected Ipv6 values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_scan_low_cardinality_rejects_bad_flags() {
    // The completeness scan must reject the same index-type-word bits the decode
    // rejects (NeedGlobalDictionaryBit set, HasAdditionalKeysBit clear), so a
    // hostile flags word cannot make the scan walk framing the decode refuses
    // and stall the StreamDecoder with a misleading truncation error.
    for index_word in [
        LC_HAS_ADDITIONAL_KEYS_BIT | LC_NEED_GLOBAL_DICTIONARY_BIT,
        0, // HasAdditionalKeysBit clear
    ] {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u64.to_le_bytes()); // key version
        payload.extend_from_slice(&index_word.to_le_bytes());
        let data = BlockBuilder::new()
            .header(1, 1)
            .column_header("lc", "LowCardinality(String)")
            .raw_bytes(&payload)
            .build();
        assert!(
            matches!(
                block_end(&data, &DecodeOptions::default()),
                Err(DecodeError::InvalidLowCardinality { .. })
            ),
            "scan should reject index word {index_word:#x}"
        );
    }
}

#[test]
fn test_low_cardinality_bad_inner_rejected_at_zero_rows() {
    // A zero-row LowCardinality(Decimal(9, 4)) must be rejected as consistently
    // as the row-bearing form: the inner allowlist is checked at header time, so
    // the zero-row `empty_column` path no longer silently accepts an inner the
    // row-bearing decode rejects.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("lc", "LowCardinality(Decimal(9, 4))")
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

#[test]
fn test_decode_low_cardinality_nullable_uint32() {
    // For a Nullable inner, dictionary slot 0 is the NULL sentinel (its
    // on-wire value is the inner default 0). Rows whose index is 0 are null;
    // the dictionary itself still decodes as a bare non-nullable UInt32.
    let dictionary = [0u32, 13, 79];
    let indices = [1u64, 0, 2, 0, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(Nullable(UInt32))")
        .low_cardinality_u32(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), 5);
            assert_eq!(d.null_count(), 2);
            let bm = d.validity.as_ref().expect("nullable dictionary validity");
            assert!(bm.is_valid(0));
            assert!(!bm.is_valid(1));
            assert!(bm.is_valid(2));
            assert!(!bm.is_valid(3));
            assert!(bm.is_valid(4));
            // The dictionary values column carries no validity of its own.
            match d.values.as_ref() {
                Column::UInt32(v) => assert!(v.validity.is_none()),
                other => panic!("expected UInt32 values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    assert_eq!(lc_u32_value(batch.column(0), 0), Some(13));
    assert_eq!(lc_u32_value(batch.column(0), 1), None);
    assert_eq!(lc_u32_value(batch.column(0), 2), Some(79));
    assert_eq!(lc_u32_value(batch.column(0), 3), None);
    assert_eq!(lc_u32_value(batch.column(0), 4), Some(13));
}

#[test]
fn test_decode_low_cardinality_date() {
    // Date is a UInt16-backed number, a legal LowCardinality inner. The
    // dictionary is a plain Date (UInt16) column body.
    let dictionary = [0u16, 19737, 49710];
    let indices = [1u64, 2, 1, 0];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("d", "LowCardinality(Date)")
        .low_cardinality_u16(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert!(d.validity.is_none());
            match d.values.as_ref() {
                Column::Date(v) => assert_eq!(v.values, vec![0u16, 19737, 49710]),
                other => panic!("expected Date values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    let expected = [19737u16, 49710, 19737, 0];
    for (row, want) in expected.iter().enumerate() {
        assert_eq!(lc_date_value(batch.column(0), row), Some(*want));
    }
}

#[test]
fn test_decode_low_cardinality_nullable_date() {
    // Nullable(Date) inner: index 0 is the NULL sentinel.
    let dictionary = [0u16, 19737, 49710];
    let indices = [0u64, 1, 0, 2];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("d", "LowCardinality(Nullable(Date))")
        .low_cardinality_u16(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    assert_eq!(lc_date_value(batch.column(0), 0), None);
    assert_eq!(lc_date_value(batch.column(0), 1), Some(19737));
    assert_eq!(lc_date_value(batch.column(0), 2), None);
    assert_eq!(lc_date_value(batch.column(0), 3), Some(49710));
    match batch.column(0) {
        Column::Dictionary(d) => assert_eq!(d.null_count(), 2),
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_decode_low_cardinality_fixed_string() {
    // FixedString(N) is a legal inner; the dictionary body is raw N-byte
    // entries with no length prefix, decoded as a FixedBinary values column.
    let dictionary: [&[u8]; 3] = [b"\0\0\0\0", b"abcd", b"wxyz"];
    let indices = [1u64, 2, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("fs", "LowCardinality(FixedString(4))")
        .low_cardinality_fixed(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => match d.values.as_ref() {
            Column::FixedBinary(v) => {
                assert_eq!(v.width, 4);
                assert_eq!(v.len(), 3);
                assert_eq!(v.value(0), b"\0\0\0\0");
                assert_eq!(v.value(1), b"abcd");
                assert_eq!(v.value(2), b"wxyz");
                let idx1 = d.indices[0] as usize;
                assert_eq!(v.value(idx1), b"abcd");
            }
            other => panic!("expected FixedBinary values, got {other:?}"),
        },
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_decode_low_cardinality_bool_and_date32_inner() {
    // Bool (UInt8-backed) and Date32 (Int32-backed) are number-backed types
    // whose canBeInsideLowCardinality is true at v26.6.1.1193-stable, so both
    // are legal LowCardinality inners and decode through the shared body.
    let data = BlockBuilder::new()
        .header(2, 3)
        .column_header("b", "LowCardinality(Bool)")
        .low_cardinality_block(2, &[0u8, 1], &[0, 1, 1], 1)
        .column_header("d32", "LowCardinality(Date32)")
        .low_cardinality_block(
            2,
            &[(-7227i32).to_le_bytes(), 84370i32.to_le_bytes()].concat(),
            &[1, 0, 1],
            1,
        )
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => match d.values.as_ref() {
            Column::Bool(v) => {
                assert_eq!(v.len(), 2);
                assert!(!v.get(0));
                assert!(v.get(1));
                // Rows resolve to false, true, true.
                assert!(!v.get(d.indices[0] as usize));
                assert!(v.get(d.indices[1] as usize));
            }
            other => panic!("expected Bool values, got {other:?}"),
        },
        other => panic!("expected Dictionary, got {other:?}"),
    }
    match batch.column(1) {
        Column::Dictionary(d) => match d.values.as_ref() {
            Column::Date32(v) => {
                assert_eq!(v.values, vec![-7227i32, 84370]);
                assert_eq!(v.values[d.indices[0] as usize], 84370);
            }
            other => panic!("expected Date32 values, got {other:?}"),
        },
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_decode_low_cardinality_time() {
    // Time is Int32-number-backed and legal inside LowCardinality. The
    // dictionary body is the same raw signed seconds run as a plain Time.
    let dictionary = [0i32, -13, 79];
    let dictionary_bytes: Vec<u8> = dictionary
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("t", "LowCardinality(Time)")
        .low_cardinality_block(3, &dictionary_bytes, &[1, 2, 1, 0], 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![1, 2, 1, 0]);
            match d.values.as_ref() {
                Column::Time(values) => assert_eq!(values.values, dictionary),
                other => panic!("expected Time dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_low_cardinality_rejects_datetime64_inner() {
    // DateTime64 is DataTypeDecimalBase, whose canBeInsideLowCardinality is
    // false, so the server never emits LowCardinality(DateTime64). The crate
    // decodes DateTime64 as an ordinary column but must reject it as a LC
    // inner rather than mis-decode a payload the server cannot produce.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("lc", "LowCardinality(DateTime64(3))")
        // A well-formed-looking prefix and index word; decode must reject on
        // the inner type before consuming the dictionary body.
        .low_cardinality_block(1, &0i64.to_le_bytes(), &[0], 1)
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
    // The completeness scan must reject it identically, so block_end agrees
    // with decode on which columns are accepted.
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_low_cardinality_rejects_time64_inner() {
    // Time64 is DecimalBase-backed, so canBeInsideLowCardinality is false.
    // Decode and the completeness scan must reject the same header.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("lc", "LowCardinality(Time64(3))")
        .low_cardinality_block(1, &0i64.to_le_bytes(), &[0], 1)
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

#[test]
fn test_decode_low_cardinality_numeric_zero_rows() {
    // A zero-row block reads no LowCardinality prefix or data for a numeric
    // inner either; it contributes the schema and an empty dictionary column
    // whose empty values carry the inner type.
    let data = BlockBuilder::new()
        .header(2, 0)
        .column_header("lc", "LowCardinality(UInt32)")
        .column_header("lcn", "LowCardinality(Nullable(UInt32))")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::UInt32))
    );
    assert_eq!(
        cb.schema.fields[1].ch_type,
        ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::UInt32))))
    );
}

#[test]
fn test_decode_low_cardinality_uuid() {
    // UUID is a legal LowCardinality inner (unconditionally allowed by the
    // server). The dictionary body is raw 16-byte UUID rows, decoded as a
    // FixedBinary (width 16) values column via the shared per-type body. Slot
    // 0 is the reserved default (all-zero); the per-row indexes start at 1.
    let nil = [0u8; 16];
    let one = [0x11u8; 16];
    let dictionary = [nil, UUID_00112233_WIRE, one];
    let indices = [1u64, 2, 1, 2];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("u", "LowCardinality(UUID)")
        .low_cardinality_fixed16(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), 4);
            assert!(d.validity.is_none());
            assert_eq!(d.indices, vec![1, 2, 1, 2]);
            match d.values.as_ref() {
                Column::Uuid(v) => {
                    assert_eq!(v.width, 16);
                    assert_eq!(v.len(), 3);
                    assert_eq!(v.value(0), nil);
                    assert_eq!(v.value(1), UUID_00112233_WIRE);
                    assert_eq!(v.value(2), one);
                    // Row 0 resolves to the 00112233... UUID, in wire order.
                    assert_eq!(v.value(d.indices[0] as usize), UUID_00112233_WIRE);
                }
                other => panic!("expected Uuid dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_decode_low_cardinality_nullable_uuid() {
    // LowCardinality(Nullable(UUID)): dictionary slot 0 is the NULL sentinel
    // (all-zero on the wire). Rows whose index is 0 are null.
    let nil = [0u8; 16];
    let dictionary = [nil, UUID_00112233_WIRE, [0x22u8; 16]];
    let indices = [1u64, 0, 2, 0];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("u", "LowCardinality(Nullable(UUID))")
        .low_cardinality_fixed16(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.null_count(), 2);
            let bm = d.validity.as_ref().expect("nullable dictionary validity");
            assert!(bm.is_valid(0));
            assert!(!bm.is_valid(1));
            assert!(bm.is_valid(2));
            assert!(!bm.is_valid(3));
            match d.values.as_ref() {
                Column::Uuid(v) => {
                    assert_eq!(v.value(d.indices[0] as usize), UUID_00112233_WIRE)
                }
                other => panic!("expected Uuid values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_decimal_rejected_as_low_cardinality_inner() {
    // The server forbids Decimal as a LowCardinality inner
    // (`canBeInsideLowCardinality()` is false on DataTypeDecimalBase), so it
    // never appears on the wire and the decoder rejects it as
    // UnsupportedType rather than mis-decoding.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("lc", "LowCardinality(Decimal(9, 4))")
        // key-version prefix then an index word; decode rejects before
        // reaching the body, so the exact trailing bytes do not matter.
        .raw_bytes(&1u64.to_le_bytes())
        .raw_bytes(&(LC_HAS_ADDITIONAL_KEYS_BIT).to_le_bytes())
        .raw_bytes(&0u64.to_le_bytes())
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_enum_rejected_as_low_cardinality_inner() {
    // The server forbids Enum as a LowCardinality inner
    // (`canBeInsideLowCardinality()` is false), so it never appears on the
    // wire and the decoder rejects it as UnsupportedType rather than
    // mis-decoding. This is independent of Enum decode support.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("e", "LowCardinality(Enum8('a' = 1))")
        // key-version prefix then an index word; decode rejects before
        // reaching the body, so the exact trailing bytes do not matter.
        .raw_bytes(&1u64.to_le_bytes())
        .raw_bytes(&(LC_HAS_ADDITIONAL_KEYS_BIT).to_le_bytes())
        .raw_bytes(&0u64.to_le_bytes())
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_decode_low_cardinality_int256() {
    // LowCardinality(Int256) IS legal on the wire (a DataTypeNumberBase
    // subclass, canBeInsideLowCardinality is true). The dictionary body is
    // the plain 32-byte-per-entry Int256 run; indices resolve into it. This
    // exercises the wide-int entry in the LC allowlist end to end.
    let dict = [w32(13), [0xFFu8; 32]]; // entry 0 = 13, entry 1 = -1
    let dict_refs: Vec<&[u8]> = dict.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("lc", "LowCardinality(Int256)")
        .low_cardinality_fixed(&dict_refs, &[0, 1, 0], 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(c) => {
            assert_eq!(c.indices, vec![0, 1, 0]);
            assert!(c.validity.is_none());
            match c.values.as_ref() {
                Column::Int256(v) => {
                    assert_eq!(v.width, 32);
                    assert_eq!(v.len(), 2);
                    assert_eq!(v.value(0), w32(13));
                    assert_eq!(v.value(1), [0xFFu8; 32]);
                }
                other => panic!("expected Int256 dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::Int256))
    );
}

#[test]
fn test_decode_low_cardinality_uint128() {
    // LowCardinality(UInt128): unsigned, 16-byte dictionary entries. Include
    // a high-bit-set entry to prove the dictionary body is a raw passthrough.
    let high_bit = {
        let mut b = [0u8; 16];
        b[15] = 0x80;
        b
    };
    let dict = [w16(79), high_bit];
    let dict_refs: Vec<&[u8]> = dict.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("lc", "LowCardinality(UInt128)")
        .low_cardinality_fixed(&dict_refs, &[0, 1, 1, 0], 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Dictionary(c) => {
            assert_eq!(c.indices, vec![0, 1, 1, 0]);
            match c.values.as_ref() {
                Column::UInt128(v) => {
                    assert_eq!(v.value(0), w16(79));
                    assert_eq!(v.value(1), high_bit);
                }
                other => panic!("expected UInt128 dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_low_cardinality_tuple_inner_rejected() {
    // LowCardinality(Tuple(...)) is illegal (Tuple inherits
    // canBeInsideLowCardinality() == false), rejected at header time on
    // both paths regardless of row count.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("lc", "LowCardinality(Tuple(Int32, String))")
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

#[test]
fn test_decode_rejects_low_cardinality_geo() {
    // LowCardinality is illegal for all six geo kinds (no canBeInsideLowCardinality
    // override), rejected at header time regardless of row count.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("lc", "LowCardinality(Point)")
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
