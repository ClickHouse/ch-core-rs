use super::*;

/// A `UUID`, `IPv4`, and `IPv6` column over four rows. The UUID and IPv6
/// values are distinct 16-byte patterns (all-zero, an ascending run, a
/// constant, all-0xFF) that must survive verbatim with no reordering; the
/// IPv4 values hit the u32 boundaries plus two real addresses.
fn uuid_ip_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "u".into(),
            ch_type: ChType::Uuid,
        },
        Field {
            name: "ip4".into(),
            ch_type: ChType::Ipv4,
        },
        Field {
            name: "ip6".into(),
            ch_type: ChType::Ipv6,
        },
    ];
    let columns = vec![
        Column::Uuid(fixed_binary_column(
            16,
            &[
                &[0u8; 16],
                b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x0C\x0D\x0E\x0F\x10",
                &[0x79; 16],
                &[0xFF; 16],
            ],
        )),
        // 0.0.0.0, 127.0.0.1, 192.168.0.1, 255.255.255.255 as the standard
        // numeric value (a<<24 | b<<16 | c<<8 | d).
        Column::Ipv4(PrimitiveColumn::new(vec![
            0,
            2_130_706_433,
            3_232_235_521,
            u32::MAX,
        ])),
        // ::, ::1, 2001:db8::13, all-0xFF, in network byte order.
        Column::Ipv6(fixed_binary_column(
            16,
            &[
                &[0u8; 16],
                b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01",
                b"\x20\x01\x0D\xB8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x13",
                &[0xFF; 16],
            ],
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// `Nullable(UUID)`, `Nullable(IPv4)`, and `Nullable(IPv6)` over four rows
/// with the valid, null, valid, null pattern, proving the `Nullable` wrapper
/// composes with all three: the null map precedes the inner body.
fn nullable_uuid_ip_batch() -> ColBatch {
    // 0x00 = valid, 0x01 = null (ClickHouse null-map polarity).
    let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let fields = vec![
        Field {
            name: "nu".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Uuid)),
        },
        Field {
            name: "nip4".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Ipv4)),
        },
        Field {
            name: "nip6".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Ipv6)),
        },
    ];
    let mut nu = fixed_binary_column(16, &[&[0x13; 16], &[0u8; 16], &[0x79; 16], &[0u8; 16]]);
    nu.validity = Some(validity());
    let mut nip6 = fixed_binary_column(
        16,
        &[
            b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01",
            &[0u8; 16],
            b"\x20\x01\x0D\xB8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x13",
            &[0u8; 16],
        ],
    );
    nip6.validity = Some(validity());
    let columns = vec![
        Column::Uuid(nu),
        Column::Ipv4(PrimitiveColumn::new_nullable(
            vec![2_130_706_433, 0, 3_232_235_521, 0],
            validity(),
        )),
        Column::Ipv6(nip6),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// An `Enum8` and an `Enum16` column over four rows. The variant lists carry
/// distinct signed values in the server's ascending-by-value order, including
/// a negative variant and each width's boundary (`i8::MIN`/`i8::MAX`,
/// `i16::MIN`/`i16::MAX`), and the physical buffers pick those boundary and
/// negative values so the little-endian byte order and sign of the underlying
/// int are exercised. The name->value map lives only in the type string
/// (`ChType::Display`), so this proves that string round-trips too.
fn enum_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "e8".into(),
            ch_type: ChType::Enum8 {
                variants: vec![
                    ("floor".into(), i8::MIN),
                    ("neg".into(), -13),
                    ("idle".into(), 0),
                    ("busy".into(), 13),
                    ("ceil".into(), i8::MAX),
                ],
            },
        },
        Field {
            name: "e16".into(),
            ch_type: ChType::Enum16 {
                variants: vec![
                    ("floor".into(), i16::MIN),
                    ("neg".into(), -79),
                    ("idle".into(), 0),
                    ("busy".into(), 79),
                    ("ceil".into(), i16::MAX),
                ],
            },
        },
    ];
    let columns = vec![
        Column::Enum8(PrimitiveColumn::new(vec![i8::MIN, -13, 0, i8::MAX])),
        Column::Enum16(PrimitiveColumn::new(vec![i16::MIN, -79, 0, i16::MAX])),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// A `Nullable(Enum8(...))` and a `Nullable(Enum16(...))` column over four
/// rows with the valid, null, valid, null pattern, proving the `Nullable`
/// wrapper composes with an enum inner: the null map precedes the inner
/// signed-int values.
fn nullable_enum_batch() -> ColBatch {
    // 0x00 = valid, 0x01 = null (ClickHouse null-map polarity).
    let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
    let fields = vec![
        Field {
            name: "ne8".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Enum8 {
                variants: vec![("neg".into(), -13), ("idle".into(), 0), ("busy".into(), 13)],
            })),
        },
        Field {
            name: "ne16".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Enum16 {
                variants: vec![("neg".into(), -79), ("idle".into(), 0), ("busy".into(), 79)],
            })),
        },
    ];
    let columns = vec![
        Column::Enum8(PrimitiveColumn::new_nullable(
            vec![-13, 0, 13, 0],
            validity(),
        )),
        Column::Enum16(PrimitiveColumn::new_nullable(
            vec![-79, 0, 79, 0],
            validity(),
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 4)
}

#[test]
fn roundtrip_uuid_ip_rev0() {
    roundtrip(&uuid_ip_batch(), 0);
}

#[test]
fn roundtrip_uuid_ip_tcp_revision() {
    roundtrip(&uuid_ip_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_uuid_ip_rev0() {
    roundtrip(&nullable_uuid_ip_batch(), 0);
}

#[test]
fn roundtrip_nullable_uuid_ip_tcp_revision() {
    roundtrip(&nullable_uuid_ip_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_enum_rev0() {
    roundtrip(&enum_batch(), 0);
}

#[test]
fn roundtrip_enum_tcp_revision() {
    roundtrip(&enum_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_enum_rev0() {
    roundtrip(&nullable_enum_batch(), 0);
}

#[test]
fn roundtrip_nullable_enum_tcp_revision() {
    roundtrip(&nullable_enum_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn encode_chunked_roundtrips_uuid_ip_blocks() {
    // Two blocks of the UUID/IPv4/IPv6 schema round-trip through
    // `encode_chunked`, staying separate chunks with the buffers intact
    // (blocks are never merged; see AGENTS.md).
    let schema = uuid_ip_batch().schema.clone();
    let chunk = |uuid_byte: u8, ip4: u32, ip6_byte: u8| {
        std::sync::Arc::new(ColBatch::new(
            schema.clone(),
            vec![
                Column::Uuid(FixedBinaryColumn::new(vec![uuid_byte; 32], 16)),
                Column::Ipv4(PrimitiveColumn::new(vec![ip4, ip4 + 1])),
                Column::Ipv6(FixedBinaryColumn::new(vec![ip6_byte; 32], 16)),
            ],
            2,
        ))
    };
    let batch = ChunkedBatch {
        schema: schema.clone(),
        chunks: vec![
            chunk(0x13, 2_130_706_433, 0x20),
            chunk(0x79, 3_232_235_521, 0x0D),
        ],
    };
    for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
        let bytes = encode_chunked(
            &batch,
            &EncodeOptions {
                protocol_revision: revision,
                ..EncodeOptions::default()
            },
        )
        .unwrap();
        let decoded = decode_all_bytes(
            &bytes,
            &DecodeOptions {
                protocol_revision: revision,
                ..DecodeOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("decode at rev {revision} failed: {e}"));
        assert_eq!(decoded.num_chunks(), 2);
        for (sent, got) in batch.chunks.iter().zip(&decoded.chunks) {
            assert_batches_eq(sent, got);
        }
    }
}

#[test]
fn rev0_frames_uuid_bytes() {
    // Pin the UUID body framing: 16 raw bytes per row, passthrough in wire
    // (UInt128 POD) order, no reordering and no per-row framing. One UUID
    // column "u", single row with 16 distinct bytes, so any byte shuffle on
    // encode would break the exact comparison.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "u".into(),
            ch_type: ChType::Uuid,
        }]),
        vec![Column::Uuid(fixed_binary_column(
            16,
            &[b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x0C\x0D\x0E\x0F\x10"],
        ))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x01, b'u', // name "u"
        0x04, b'U', b'U', b'I', b'D', // type "UUID"
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // 16 raw bytes,
        0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, // buffer order
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_ipv4_bytes() {
    // Pin the IPv4 body framing: the standard numeric value written as a
    // little-endian u32, exactly like UInt32. One IPv4 column "ip4", single
    // row 192.168.0.1 = 0xC0A80001, so the wire bytes must be the reversed
    // 01 00 A8 C0 and any big-endian write would fail.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "ip4".into(),
            ch_type: ChType::Ipv4,
        }]),
        vec![Column::Ipv4(PrimitiveColumn::new(vec![3_232_235_521]))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x03, b'i', b'p', b'4', // name "ip4"
        0x04, b'I', b'P', b'v', b'4', // type "IPv4"
        0x01, 0x00, 0xA8, 0xC0, // u32 0xC0A80001 (192.168.0.1), little-endian
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_ipv6_bytes() {
    // Pin the IPv6 body framing: 16 raw bytes per row, verbatim in network
    // byte order, no per-row framing. One IPv6 column "ip6", single row with
    // 16 distinct bytes, so any byte shuffle on encode would break the exact
    // comparison.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "ip6".into(),
            ch_type: ChType::Ipv6,
        }]),
        vec![Column::Ipv6(fixed_binary_column(
            16,
            &[b"\x20\x01\x0D\xB8\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x13"],
        ))],
        1,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x01, // num_rows = 1
        0x03, b'i', b'p', b'6', // name "ip6"
        0x04, b'I', b'P', b'v', b'6', // type "IPv6"
        0x20, 0x01, 0x0D, 0xB8, 0x01, 0x02, 0x03, 0x04, // 16 raw bytes,
        0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x13, // network order
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn uuid_width_mismatch_is_rejected() {
    // A UUID buffer whose stored width is not 16 would put the wrong number of
    // bytes per row on the wire under a truthful type string; reject it before
    // any bytes are written, mirroring the FixedString width guard.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "u".into(),
            ch_type: ChType::Uuid,
        }]),
        vec![Column::Uuid(FixedBinaryColumn::new(vec![0u8; 8], 8))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn ipv6_ragged_data_is_rejected() {
    // An IPv6 buffer whose byte count is not exactly 16 * num_rows reports the
    // right row count via truncating division but would misframe the stream;
    // reject it, mirroring the FixedString ragged-data guard.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "ip6".into(),
            ch_type: ChType::Ipv6,
        }]),
        columns: vec![Column::Ipv6(FixedBinaryColumn::new(vec![0u8; 17], 16))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}
