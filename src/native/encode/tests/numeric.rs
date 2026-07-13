use super::*;

/// All ten fixed-width numeric columns over four rows, one batch. Values pick
/// each type's extremes plus a couple of neutral in-range values.
fn numeric_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "i8".into(),
            ch_type: ChType::Int8,
        },
        Field {
            name: "i16".into(),
            ch_type: ChType::Int16,
        },
        Field {
            name: "i32".into(),
            ch_type: ChType::Int32,
        },
        Field {
            name: "i64".into(),
            ch_type: ChType::Int64,
        },
        Field {
            name: "u8".into(),
            ch_type: ChType::UInt8,
        },
        Field {
            name: "u16".into(),
            ch_type: ChType::UInt16,
        },
        Field {
            name: "u32".into(),
            ch_type: ChType::UInt32,
        },
        Field {
            name: "u64".into(),
            ch_type: ChType::UInt64,
        },
        Field {
            name: "f32".into(),
            ch_type: ChType::Float32,
        },
        Field {
            name: "f64".into(),
            ch_type: ChType::Float64,
        },
    ];
    let columns = vec![
        Column::Int8(PrimitiveColumn::new(vec![i8::MIN, -13, 0, i8::MAX])),
        Column::Int16(PrimitiveColumn::new(vec![i16::MIN, -13, 0, i16::MAX])),
        Column::Int32(PrimitiveColumn::new(vec![i32::MIN, -79, 0, i32::MAX])),
        Column::Int64(PrimitiveColumn::new(vec![i64::MIN, -79, 0, i64::MAX])),
        Column::UInt8(PrimitiveColumn::new(vec![0, 13, 79, u8::MAX])),
        Column::UInt16(PrimitiveColumn::new(vec![0, 13, 79, u16::MAX])),
        Column::UInt32(PrimitiveColumn::new(vec![0, 13, 79, u32::MAX])),
        Column::UInt64(PrimitiveColumn::new(vec![0, 13, 79, u64::MAX])),
        Column::Float32(PrimitiveColumn::new(vec![-1.25, 0.0, 3.5, 79.125])),
        Column::Float64(PrimitiveColumn::new(vec![-1.25, 0.0, 3.5, 79.125])),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

#[test]
fn roundtrip_numerics_rev0() {
    roundtrip(&numeric_batch(), 0);
}

#[test]
fn roundtrip_numerics_tcp_revision() {
    roundtrip(&numeric_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn rev0_frames_exact_bytes() {
    // Pin the rev-0 framing byte-for-byte: no BlockInfo, no marker. One Int32
    // column "n" with a single row = 1.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Int32,
        }]),
        vec![Column::Int32(PrimitiveColumn::new(vec![1]))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'n', // name "n"
        0x05, b'I', b'n', b't', b'3', b'2', // type "Int32"
        0x01, 0x00, 0x00, 0x00, // Int32 value 1, little-endian
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn type_string_buffer_mismatch_is_rejected() {
    // A supported numeric declared under a mismatched buffer variant must
    // error, never emit a wrong-width body under a truthful type string.
    // Here the type string would be "Int64" (8 bytes/row) but the buffer is a
    // 4-byte i32. Construct directly so `ColBatch::new`'s debug_assert on
    // column length (both are len 1) does not mask the type mismatch.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Int64,
        }]),
        columns: vec![Column::Int32(PrimitiveColumn::new(vec![13]))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}
