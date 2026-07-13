use super::*;

/// A single `Bool` column over five rows (a non-multiple of 8 so the packed
/// bitmap's trailing partial byte is exercised).
fn bool_batch() -> ColBatch {
    let fields = vec![Field {
        name: "b".into(),
        ch_type: ChType::Bool,
    }];
    let columns = vec![Column::Bool(BoolColumn::from_wire_bytes(&[1, 0, 1, 1, 0]))];
    ColBatch::new(Schema::new(fields), columns, 5)
}

#[test]
fn roundtrip_bool_rev0() {
    roundtrip(&bool_batch(), 0);
}

#[test]
fn roundtrip_bool_tcp_revision() {
    roundtrip(&bool_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn rev0_frames_bool_bytes() {
    // Pin the Bool body framing: one byte per row, 0x01 = true, 0x00 = false.
    // One Bool column "b" over three rows: true, false, true.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "b".into(),
            ch_type: ChType::Bool,
        }]),
        vec![Column::Bool(BoolColumn::from_wire_bytes(&[1, 0, 1]))],
        3,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x03, // num_rows = 3
        0x01, b'b', // name "b"
        0x04, b'B', b'o', b'o', b'l', // type "Bool"
        0x01, 0x00, 0x01, // one byte per row: true, false, true
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn bool_bitmap_length_mismatch_is_rejected() {
    // A Bool column whose `len` overruns its packed bitmap would panic when
    // unpacked positionally; reject it as InconsistentBatch before any bytes
    // are written. Construct the malformed column directly: `len` claims 100
    // rows but the bitmap holds one byte (room for 8). Row count is consistent
    // (`len` == num_rows), so it passes the row-count check and reaches the
    // bitmap-length guard.
    let col = BoolColumn {
        bitmap: vec![0x01],
        len: 100,
        validity: None,
    };
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "b".into(),
            ch_type: ChType::Bool,
        }]),
        vec![Column::Bool(col)],
        100,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}
