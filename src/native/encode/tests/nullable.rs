use super::*;

/// A `Nullable` numeric, string, and bool over four rows. The null pattern is
/// valid, null, valid, null, so the null map exercises both states and the
/// inner-value buffers still carry a (placeholder) value for the null rows.
fn nullable_batch() -> ColBatch {
    // 0x00 = valid, 0x01 = null (ClickHouse null-map polarity).
    let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let fields = vec![
        Field {
            name: "ni32".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Int32)),
        },
        Field {
            name: "ns".into(),
            ch_type: ChType::Nullable(Box::new(ChType::String)),
        },
        Field {
            name: "nb".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Bool)),
        },
    ];
    let mut ns = utf8_column(&[b"user_1", b"", b"user_2", b""]);
    ns.validity = Some(validity());
    let columns = vec![
        Column::Int32(PrimitiveColumn::new_nullable(
            vec![13, 0, 79, 0],
            validity(),
        )),
        Column::Utf8(ns),
        Column::Bool(BoolColumn::from_wire_bytes_nullable(
            &[1, 0, 1, 0],
            validity(),
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

#[test]
fn roundtrip_nullable_rev0() {
    roundtrip(&nullable_batch(), 0);
}

#[test]
fn roundtrip_nullable_tcp_revision() {
    roundtrip(&nullable_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn rev0_frames_nullable_bytes() {
    // Pin the Nullable framing: the per-row null map (0x00 valid, 0x01 null)
    // precedes the inner values. One Nullable(Int32) column "n" over two rows:
    // 13 (valid), then a null row (inner value 0).
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Int32)),
        }]),
        vec![Column::Int32(PrimitiveColumn::new_nullable(
            vec![13, 0],
            Bitmap::from_ch_null_map(&[0, 1]),
        ))],
        2,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x01, b'n', // name "n"
        0x0F, b'N', b'u', b'l', b'l', b'a', b'b', b'l', b'e', b'(', b'I', b'n', b't', b'3', b'2',
        b')', // type "Nullable(Int32)"
        0x00, 0x01, // null map: row 0 valid, row 1 null
        0x0D, 0x00, 0x00, 0x00, // Int32 13, little-endian
        0x00, 0x00, 0x00, 0x00, // Int32 placeholder for the null row
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn nullable_validity_length_mismatch_is_rejected() {
    // A Nullable column whose validity bitmap does not cover num_rows would
    // write a null map of the wrong length; reject it as InconsistentBatch
    // before any bytes are written.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Int32)),
        }]),
        vec![Column::Int32(PrimitiveColumn::new_nullable(
            vec![13, 79],
            Bitmap::from_ch_null_map(&[0]), // covers one row, not two
        ))],
        2,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
#[cfg(not(debug_assertions))]
fn short_validity_buffer_is_rejected() {
    // A caller can build a `Bitmap` whose backing buffer is too short for its
    // bit length via the public `Bitmap::from_raw`, which only debug-asserts the
    // invariant. In a release build that bitmap would panic when
    // `encode_null_map` unpacks it (index out of bounds), so `validate_column`
    // must reject it as an inconsistent batch first. This test is release-only:
    // in a debug build `from_raw`'s `debug_assert!` fires at construction, so the
    // malformed state cannot be reached through the public API. 100 rows need 13
    // bitmap bytes; the buffer holds 1.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Int32)),
        }]),
        columns: vec![Column::Int32(PrimitiveColumn::new_nullable(
            vec![0i32; 100],
            Bitmap::from_raw(vec![0u8; 1], 100),
        ))],
        num_rows: 100,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn non_nullable_with_nulls_is_rejected() {
    // A non-Nullable field with a validity bitmap that marks a row null writes
    // no null map, so the null would be silently dropped and its placeholder
    // value encoded as real data. Reject it.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Int32,
        }]),
        columns: vec![Column::Int32(PrimitiveColumn::new_nullable(
            vec![13, 79],
            Bitmap::from_ch_null_map(&[0, 1]), // row 1 null under a non-Nullable field
        ))],
        num_rows: 2,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn non_nullable_all_valid_bitmap_is_accepted() {
    // A validity bitmap with no nulls under a non-Nullable field carries no null
    // information to lose, so it encodes fine (and round-trips: the decoder
    // produces a non-nullable column with no validity).
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Int32,
        }]),
        columns: vec![Column::Int32(PrimitiveColumn::new_nullable(
            vec![13, 79],
            Bitmap::from_ch_null_map(&[0, 0]), // all valid
        ))],
        num_rows: 2,
    };
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    match decoded.chunks[0].column(0) {
        Column::Int32(c) => assert_eq!(c.values, vec![13, 79]),
        other => panic!("expected Int32, got {other:?}"),
    }
}
