use super::*;

/// All four wide-int types over three rows each, with sign and high-bit
/// boundary values so any accidental sign/endianness/reorder bug is caught:
/// the signed types include -1 (all 0xFF) and the width MIN (MSB-only); the
/// unsigned types include a high-bit-set value and the all-0xFF max. The core
/// stores the raw wire bytes verbatim, so these are already wire-order.
fn wide_int_batch() -> ColBatch {
    let mut i128_min = [0u8; 16];
    i128_min[15] = 0x80;
    let mut u128_high = [0u8; 16];
    u128_high[15] = 0x80;
    let mut w16_13 = [0u8; 16];
    w16_13[0] = 13;
    let mut w16_79 = [0u8; 16];
    w16_79[0] = 79;

    let mut i256_min = [0u8; 32];
    i256_min[31] = 0x80;
    let mut u256_high = [0u8; 32];
    u256_high[31] = 0x80;
    let mut w32_13 = [0u8; 32];
    w32_13[0] = 13;
    let mut w32_79 = [0u8; 32];
    w32_79[0] = 79;

    let fields = vec![
        Field {
            name: "i128".into(),
            ch_type: ChType::Int128,
        },
        Field {
            name: "u128".into(),
            ch_type: ChType::UInt128,
        },
        Field {
            name: "i256".into(),
            ch_type: ChType::Int256,
        },
        Field {
            name: "u256".into(),
            ch_type: ChType::UInt256,
        },
    ];
    let columns = vec![
        Column::Int128(wide_int_column(16, &[&w16_13, &[0xFFu8; 16], &i128_min])),
        Column::UInt128(wide_int_column(16, &[&w16_79, &u128_high, &[0xFFu8; 16]])),
        Column::Int256(wide_int_column(32, &[&w32_13, &[0xFFu8; 32], &i256_min])),
        Column::UInt256(wide_int_column(32, &[&w32_79, &u256_high, &[0xFFu8; 32]])),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// A `Nullable(Int128)` column with valid, null, valid, null rows.
fn nullable_wide_int_batch() -> ColBatch {
    let validity = Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let mut thirteen = [0u8; 16];
    thirteen[0] = 13;
    let mut col = wide_int_column(16, &[&thirteen, &[0u8; 16], &[0xFFu8; 16], &[0u8; 16]]);
    col.validity = Some(validity);
    ColBatch::new(
        Schema::new(vec![Field {
            name: "nw".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Int128)),
        }]),
        vec![Column::Int128(col)],
        4,
    )
}

/// A `LowCardinality(Int256)` column, so the encode LC path is exercised for
/// a wide-int inner. Dictionary slot 0 is the reserved default, real rows
/// reference slots 1...
fn low_cardinality_wide_int_batch() -> ColBatch {
    let mut thirteen = [0u8; 32];
    thirteen[0] = 13;
    let mut seventy_nine = [0u8; 32];
    seventy_nine[0] = 79;
    ColBatch::new(
        Schema::new(vec![Field {
            name: "lc_i256".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Int256)),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1],
            Column::Int256(wide_int_column(32, &[&[0u8; 32], &thirteen, &seventy_nine])),
        ))],
        3,
    )
}

#[test]
fn roundtrip_wide_int_rev0() {
    roundtrip(&wide_int_batch(), 0);
}

#[test]
fn roundtrip_wide_int_tcp_revision() {
    roundtrip(&wide_int_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_wide_int_rev0() {
    roundtrip(&nullable_wide_int_batch(), 0);
}

#[test]
fn roundtrip_nullable_wide_int_tcp_revision() {
    roundtrip(&nullable_wide_int_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_low_cardinality_wide_int_rev0() {
    roundtrip(&low_cardinality_wide_int_batch(), 0);
}

#[test]
fn roundtrip_low_cardinality_wide_int_tcp_revision() {
    roundtrip(&low_cardinality_wide_int_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn encode_chunked_roundtrips_wide_int_blocks() {
    // Wide-int blocks stay separate chunks, never concatenated. Each 16-byte
    // row is written verbatim; block A carries a value and -1 (all 0xFF),
    // block B a single value.
    let field = Field {
        name: "w".into(),
        ch_type: ChType::Int128,
    };
    let chunk = |rows: Vec<[u8; 16]>| {
        let mut data = Vec::with_capacity(rows.len() * 16);
        for r in &rows {
            data.extend_from_slice(r);
        }
        std::sync::Arc::new(ColBatch::new(
            Schema::new(vec![field.clone()]),
            vec![Column::Int128(FixedBinaryColumn::new(data, 16))],
            rows.len(),
        ))
    };
    let mut thirteen = [0u8; 16];
    thirteen[0] = 13;
    let mut seventy_nine = [0u8; 16];
    seventy_nine[0] = 79;
    let batch = ChunkedBatch {
        schema: Schema::new(vec![field.clone()]),
        chunks: vec![
            chunk(vec![thirteen, [0xFFu8; 16]]),
            chunk(vec![seventy_nine]),
        ],
    };
    let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
    let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
    assert_eq!(decoded.num_chunks(), 2);
    for (sent, got) in batch.chunks.iter().zip(&decoded.chunks) {
        assert_batches_eq(sent, got);
    }
}

#[test]
fn rev0_frames_int128_bytes() {
    // Pin the wide-int body framing: 16 raw bytes per row, passthrough in
    // wire (little-endian) order, no reordering and no per-row framing. One
    // Int128 column "w", single row with 16 distinct bytes so any byte
    // shuffle or byteswap on encode would break the exact comparison.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "w".into(),
            ch_type: ChType::Int128,
        }]),
        vec![Column::Int128(fixed_binary_column(
            16,
            &[b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x0C\x0D\x0E\x0F\x10"],
        ))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'w', // name "w"
        0x06, b'I', b'n', b't', b'1', b'2', b'8', // type "Int128"
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // 16 raw bytes,
        0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, // buffer order
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_uint256_bytes() {
    // Pin the 32-byte wide-int body framing. One UInt256 column "w", single
    // row whose only set byte is the most-significant (b[31] = 0x80 = 2^255):
    // it must land at the END of the 32-byte run, proving little-endian
    // passthrough and that the unsigned high bit is not treated as a sign.
    let mut value = [0u8; 32];
    value[31] = 0x80;
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "w".into(),
            ch_type: ChType::UInt256,
        }]),
        vec![Column::UInt256(fixed_binary_column(32, &[&value]))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let mut expected = vec![
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'w', // name "w"
        0x07, b'U', b'I', b'n', b't', b'2', b'5', b'6', // type "UInt256"
    ];
    expected.extend_from_slice(&value); // 31 zero bytes then 0x80
    assert_eq!(bytes, expected);
}

#[test]
fn wide_int_width_mismatch_is_rejected() {
    // Int128 is 16 bytes per row, so a width-32 buffer misframes the body
    // under the truthful type string.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "w".into(),
            ch_type: ChType::Int128,
        }]),
        vec![Column::Int128(FixedBinaryColumn::new(vec![0u8; 32], 32))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn wide_int_ragged_data_is_rejected() {
    // A wide-int buffer whose byte count is not exactly width * num_rows
    // reports the right row count via truncating division but would put too
    // many bytes on the wire.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "w".into(),
            ch_type: ChType::UInt256,
        }]),
        columns: vec![Column::UInt256(FixedBinaryColumn::new(vec![0u8; 40], 32))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn wide_int_signedness_variant_mismatch_is_rejected() {
    // An Int128 type over a UInt128 buffer is a mismatched column variant:
    // the four wide-int types map 1:1 to their Column variants, so this is
    // caught before any bytes are written even though both are width 16.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "w".into(),
            ch_type: ChType::Int128,
        }]),
        vec![Column::UInt128(FixedBinaryColumn::new(vec![0u8; 16], 16))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}
