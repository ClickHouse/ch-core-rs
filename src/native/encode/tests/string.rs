use super::*;

/// A `String` column and a `FixedString(4)` column over four rows. The string
/// values include an empty string and a value longer than the fixed width to
/// exercise the varint length framing; the fixed-string values include an
/// all-zero row and a zero-padded row.
fn string_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "s".into(),
            ch_type: ChType::String,
        },
        Field {
            name: "fs".into(),
            ch_type: ChType::FixedString(4),
        },
    ];
    let columns = vec![
        Column::Utf8(utf8_column(&[b"user_1", b"", b"n", b"user_2_longer"])),
        Column::FixedBinary(fixed_binary_column(
            4,
            &[b"road", b"1234", b"\x00\x00\x00\x00", b"n\x00\x00\x00"],
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

#[test]
fn roundtrip_strings_rev0() {
    roundtrip(&string_batch(), 0);
}

#[test]
fn roundtrip_strings_tcp_revision() {
    roundtrip(&string_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn rev0_frames_string_bytes() {
    // Pin the String body framing: one varint length prefix then the raw
    // bytes, per row. One String column "s" with a single row "hi".
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "s".into(),
            ch_type: ChType::String,
        }]),
        vec![Column::Utf8(utf8_column(&[b"hi"]))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b's', // name "s"
        0x06, b'S', b't', b'r', b'i', b'n', b'g', // type "String"
        0x02, b'h', b'i', // value: varint len 2 then "hi"
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_fixed_string_bytes() {
    // Pin the FixedString body framing: contiguous width*num_rows bytes, no
    // per-row length prefix. One FixedString(4) column "fs", single row.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "fs".into(),
            ch_type: ChType::FixedString(4),
        }]),
        vec![Column::FixedBinary(fixed_binary_column(4, &[b"road"]))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x02, b'f', b's', // name "fs"
        0x0E, b'F', b'i', b'x', b'e', b'd', b'S', b't', b'r', b'i', b'n', b'g', b'(', b'4',
        b')', // type "FixedString(4)"
        b'r', b'o', b'a', b'd', // 4 raw bytes, no length prefix
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn fixed_string_width_mismatch_is_rejected() {
    // A FixedString(4) type string paired with a width-3 buffer would emit a
    // body with the wrong bytes-per-row; reject it rather than corrupt.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "fs".into(),
            ch_type: ChType::FixedString(4),
        }]),
        columns: vec![Column::FixedBinary(fixed_binary_column(3, &[b"abc"]))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn fixed_string_ragged_data_is_rejected() {
    // A FixedString(4) column whose data buffer is not a multiple of the width
    // (7 bytes) reports len() == 1 via truncating division, so it passes the
    // row-count check, but writing it verbatim would put 7 bytes where the
    // reader consumes 4, silently misframing the stream. Reject it before
    // writing. Construct directly so `ColBatch::new`'s debug_assert (which uses
    // the same truncating len()) does not mask it.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "fs".into(),
            ch_type: ChType::FixedString(4),
        }]),
        columns: vec![Column::FixedBinary(FixedBinaryColumn::new(
            b"road12X".to_vec(),
            4,
        ))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn nullable_fixed_string_ragged_data_is_rejected() {
    // The same ragged-buffer misframe under a `Nullable(FixedString(4))` must
    // also be rejected: the value type is unwrapped before the width and
    // byte-count checks, so the guard applies inside the wrapper too.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "fs".into(),
            ch_type: ChType::Nullable(Box::new(ChType::FixedString(4))),
        }]),
        columns: vec![Column::FixedBinary(FixedBinaryColumn::new_nullable(
            b"road12X".to_vec(),
            4,
            Bitmap::from_ch_null_map(&[0]),
        ))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

/// Build a one-column `String` batch directly from raw offsets and data so a
/// malformed offset array reaches the encoder (the `utf8_column` helper always
/// builds well-formed offsets). `num_rows` is set from the offsets so only the
/// offset invariants, not the row-count check, are exercised.
fn string_batch_from_parts(offsets: Vec<i32>, data: Vec<u8>, num_rows: usize) -> ColBatch {
    ColBatch {
        schema: Schema::new(vec![Field {
            name: "s".into(),
            ch_type: ChType::String,
        }]),
        columns: vec![Column::Utf8(Utf8Column::new(offsets, data))],
        num_rows,
    }
}

#[test]
fn non_monotonic_string_offsets_are_rejected() {
    // Offsets that decrease would make `encode_string_data` slice `data[3..1]`,
    // which panics. Reject before writing.
    let batch = string_batch_from_parts(vec![0, 3, 1], b"abc".to_vec(), 2);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn string_offset_past_data_is_rejected() {
    // A final offset past `data.len()` would slice out of bounds and panic.
    let batch = string_batch_from_parts(vec![0, 10], b"abc".to_vec(), 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn negative_string_offset_is_rejected() {
    // A negative offset wraps to a huge `usize` in `encode_string_data`. The
    // monotonic check (from a zero start) catches it before that can happen.
    let batch = string_batch_from_parts(vec![0, -1], Vec::new(), 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn trailing_string_data_is_rejected() {
    // Offsets that end before `data.len()` would silently drop the trailing
    // bytes from the wire. Reject rather than lose data (review item 4).
    let batch = string_batch_from_parts(vec![0, 2], b"abcd".to_vec(), 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn leading_string_slack_is_rejected() {
    // A nonzero first offset would silently drop the leading data bytes and
    // violates the Arrow convention that offsets start at 0.
    let batch = string_batch_from_parts(vec![2, 4], b"abcd".to_vec(), 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}
