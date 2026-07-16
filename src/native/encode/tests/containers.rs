use super::*;

/// An `Array(Int32)` column over four rows: `[13, 79]`, `[]` (an empty row,
/// so an adjacent-equal offset pair), `[21]`, `[34, 55, 89]`.
fn array_int32_batch() -> ColBatch {
    let fields = vec![Field {
        name: "a".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 2, 3, 6],
        Column::Int32(PrimitiveColumn::new(vec![13, 79, 21, 34, 55, 89])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 4)
}

/// An `Array(Nullable(String))` column over three rows: `["user_1", NULL]`,
/// `[]`, `["user_2"]`. The element null map covers the flattened element
/// run, so its validity lives on the flattened Utf8 column, not the array.
fn array_nullable_string_batch() -> ColBatch {
    let mut elements = utf8_column(&[b"user_1", b"", b"user_2"]);
    elements.validity = Some(Bitmap::from_ch_null_map(&[0, 1, 0]));
    let fields = vec![Field {
        name: "ans".into(),
        ch_type: ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::String)))),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 2, 3],
        Column::Utf8(elements),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// An `Array(LowCardinality(String))` column over three rows:
/// `[user_1, user_2]`, `[]`, `[user_1]`. The element column is one
/// dictionary over the flattened run, and the LC key version is hoisted to
/// the front of the whole column, before the offsets.
fn array_low_cardinality_batch() -> ColBatch {
    let fields = vec![Field {
        name: "alc".into(),
        ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 2, 3],
        Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1],
            Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
        )),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// An `Array(LowCardinality(String))` column with rows > 0 but EVERY array
/// empty, so the flattened element run has zero length and the LC element
/// body must be entirely absent: the wire is `[key version][zero offsets]`
/// and nothing else (the server's `limit == 0` early return).
fn array_low_cardinality_all_empty_batch() -> ColBatch {
    let fields = vec![Field {
        name: "alc".into(),
        ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 0, 0],
        Column::Dictionary(DictionaryColumn::new(
            vec![],
            Column::Utf8(utf8_column(&[])),
        )),
    ))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// An `Array(Array(Int32))` column over three rows:
/// `[[13, 79], [21]]`, `[]`, `[[34, 55, 89]]`. The outer offsets count inner
/// arrays, the inner offsets count leaf ints, and only one offsets run per
/// level is written (no prefixes anywhere for an Int32 leaf).
fn array_of_array_batch() -> ColBatch {
    let fields = vec![Field {
        name: "aa".into(),
        ch_type: ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 2, 3],
        Column::Array(ArrayColumn::new(
            vec![0, 2, 3, 6],
            Column::Int32(PrimitiveColumn::new(vec![13, 79, 21, 34, 55, 89])),
        )),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// An `Array(Int128)` column over three rows, exercising a wide int as a
/// container element: `[13, INT128_MIN]`, `[]` (an empty row, adjacent-equal
/// offsets), `[-1]`. INT128_MIN is the sign-boundary value whose only high
/// byte is set (byte 15 = 0x80). A byte-reversal turns it into a small
/// positive value and a sign bug mangles it, so either fails the round-trip.
/// The core stores the raw wire bytes verbatim, so these are wire-order.
fn array_int128_batch() -> ColBatch {
    let mut i128_min = [0u8; 16];
    i128_min[15] = 0x80;
    let mut thirteen = [0u8; 16];
    thirteen[0] = 13;
    let fields = vec![Field {
        name: "a".into(),
        ch_type: ChType::Array(Box::new(ChType::Int128)),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 2, 2, 3],
        Column::Int128(wide_int_column(16, &[&thirteen, &i128_min, &[0xFFu8; 16]])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// Tuple(Int32, String) plus a named Tuple(a Int32, b Nullable(String)),
/// covering an unnamed tuple, element names, and a Nullable element.
fn tuple_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        },
        Field {
            name: "tn".into(),
            ch_type: ChType::Tuple(vec![
                (Some("a".to_string()), ChType::Int32),
                (
                    Some("b".to_string()),
                    ChType::Nullable(Box::new(ChType::String)),
                ),
            ]),
        },
    ];
    let mut b = utf8_column(&[b"user_1", b"", b"user_2"]);
    b.validity = Some(Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]));
    let columns = vec![
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![13, 79, -7])),
                Column::Utf8(utf8_column(&[b"user_1", b"user_2", b""])),
            ],
            3,
        )),
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
                Column::Utf8(b),
            ],
            3,
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// Nullable(Tuple(Int32, String)) with the tuple-level null map, plus a
/// tuple with a LowCardinality element (whose key-version prefix is hoisted
/// ahead of element 0's body).
fn nullable_and_lc_tuple_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "nt".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Tuple(vec![
                (None, ChType::Int32),
                (None, ChType::String),
            ]))),
        },
        Field {
            name: "tlc".into(),
            ch_type: ChType::Tuple(vec![
                (Some("k".to_string()), ChType::Int32),
                (
                    Some("lc".to_string()),
                    ChType::LowCardinality(Box::new(ChType::String)),
                ),
            ]),
        },
    ];
    let columns = vec![
        Column::Tuple(TupleColumn::new_nullable(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![13, 0, 79])),
                Column::Utf8(utf8_column(&[b"user_1", b"", b"user_2"])),
            ],
            3,
            Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]),
        )),
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
                Column::Dictionary(DictionaryColumn::new(
                    vec![1, 2, 1],
                    Column::Utf8(utf8_column(&[b"", b"red", b"green"])),
                )),
            ],
            3,
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// Array(Tuple(Int32, Int32)) and a nested Tuple(p Tuple(Int8, Int8), s
/// String), covering both container compositions.
fn array_and_nested_tuple_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "at".into(),
            ch_type: ChType::Array(Box::new(ChType::Tuple(vec![
                (None, ChType::Int32),
                (None, ChType::Int32),
            ]))),
        },
        Field {
            name: "tt".into(),
            ch_type: ChType::Tuple(vec![
                (
                    Some("p".to_string()),
                    ChType::Tuple(vec![(None, ChType::Int8), (None, ChType::Int8)]),
                ),
                (Some("s".to_string()), ChType::String),
            ]),
        },
    ];
    let columns = vec![
        // [], [(13, 79)], [(1, 2), (3, 4)] -> offsets [0, 0, 1, 3].
        Column::Array(ArrayColumn::new(
            vec![0, 0, 1, 3],
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int32(PrimitiveColumn::new(vec![13, 1, 3])),
                    Column::Int32(PrimitiveColumn::new(vec![79, 2, 4])),
                ],
                3,
            )),
        )),
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Tuple(TupleColumn::new(
                    vec![
                        Column::Int8(PrimitiveColumn::new(vec![1, 3, 5])),
                        Column::Int8(PrimitiveColumn::new(vec![2, 4, 6])),
                    ],
                    3,
                )),
                Column::Utf8(utf8_column(&[b"user_1", b"user_2", b"user_3"])),
            ],
            3,
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// The zero-element Tuple(): one placeholder byte per row on the wire.
fn empty_tuple_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "k".into(),
            ch_type: ChType::Int32,
        },
        Field {
            name: "t0".into(),
            ch_type: ChType::Tuple(vec![]),
        },
    ];
    let columns = vec![
        Column::Int32(PrimitiveColumn::new(vec![13, 79])),
        Column::Tuple(TupleColumn::new(vec![], 2)),
    ];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// Map(String, Int32) plus Map(Int32, Nullable(String)), covering a plain
/// map with an empty row and a Nullable value run.
fn map_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        },
        Field {
            name: "mnv".into(),
            ch_type: ChType::Map(
                Box::new(ChType::Int32),
                Box::new(ChType::Nullable(Box::new(ChType::String))),
            ),
        },
    ];
    let mut nullable_values = utf8_column(&[b"user_1", b"", b"user_2"]);
    nullable_values.validity = Some(Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]));
    let columns = vec![
        // {} / {a: 13} / {a: 1, b: 2}
        Column::Map(map_column(
            vec![0, 0, 1, 3],
            Column::Utf8(utf8_column(&[b"a", b"a", b"b"])),
            Column::Int32(PrimitiveColumn::new(vec![13, 1, 2])),
        )),
        // {1: user_1} / {2: NULL} / {3: user_2}
        Column::Map(map_column(
            vec![0, 1, 2, 3],
            Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
            Column::Utf8(nullable_values),
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// A `Map(String, Int256)` column over three rows, exercising a wide int as
/// a map value: `{}`, `{k1: INT256_MIN}`, `{k1: 13, k2: -1}`. INT256_MIN is
/// the sign-boundary value whose only high byte is set (byte 31 = 0x80), so
/// a byte-reversal or sign bug in the value run fails the round-trip.
fn map_string_int256_batch() -> ColBatch {
    let mut i256_min = [0u8; 32];
    i256_min[31] = 0x80;
    let mut thirteen = [0u8; 32];
    thirteen[0] = 13;
    let fields = vec![Field {
        name: "m".into(),
        ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int256)),
    }];
    let columns = vec![Column::Map(map_column(
        vec![0, 0, 1, 3],
        Column::Utf8(utf8_column(&[b"k1", b"k1", b"k2"])),
        Column::Int256(wide_int_column(32, &[&i256_min, &thirteen, &[0xFFu8; 32]])),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// Map(LowCardinality(String), UInt8) (the hoisted key prefix) plus
/// Map(String, Array(Int32)) and a nested Map value.
fn lc_and_nested_map_batch() -> ColBatch {
    let fields = vec![
        Field {
            name: "mlc".into(),
            ch_type: ChType::Map(
                Box::new(ChType::LowCardinality(Box::new(ChType::String))),
                Box::new(ChType::UInt8),
            ),
        },
        Field {
            name: "marr".into(),
            ch_type: ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Array(Box::new(ChType::Int32))),
            ),
        },
        Field {
            name: "mm".into(),
            ch_type: ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Map(
                    Box::new(ChType::String),
                    Box::new(ChType::Int32),
                )),
            ),
        },
    ];
    let columns = vec![
        // {red: 1} / {} / {red: 2, blue: 3}
        Column::Map(map_column(
            vec![0, 1, 1, 3],
            Column::Dictionary(DictionaryColumn::new(
                vec![1, 1, 2],
                Column::Utf8(utf8_column(&[b"", b"red", b"blue"])),
            )),
            Column::UInt8(PrimitiveColumn::new(vec![1, 2, 3])),
        )),
        // {a: [13]} / {b: [], c: [1, 2]} / {}
        Column::Map(map_column(
            vec![0, 1, 3, 3],
            Column::Utf8(utf8_column(&[b"a", b"b", b"c"])),
            Column::Array(ArrayColumn::new(
                vec![0, 1, 1, 3],
                Column::Int32(PrimitiveColumn::new(vec![13, 1, 2])),
            )),
        )),
        // {a: {x: 1}} / {b: {y: 2, z: 3}} / {}
        Column::Map(map_column(
            vec![0, 1, 2, 2],
            Column::Utf8(utf8_column(&[b"a", b"b"])),
            Column::Map(map_column(
                vec![0, 1, 3],
                Column::Utf8(utf8_column(&[b"x", b"y", b"z"])),
                Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
            )),
        )),
    ];
    ColBatch::new(Schema::new(fields), columns, 3)
}

/// Array(Map(String, Int32)): maps flattened under array offsets.
fn array_of_map_batch() -> ColBatch {
    let fields = vec![Field {
        name: "am".into(),
        ch_type: ChType::Array(Box::new(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Int32),
        ))),
    }];
    // [] / [{a: 1}] / [{b: 2}, {}]
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 0, 1, 3],
        Column::Map(map_column(
            vec![0, 1, 2, 2],
            Column::Utf8(utf8_column(&[b"a", b"b"])),
            Column::Int32(PrimitiveColumn::new(vec![1, 2])),
        )),
    ))];
    ColBatch::new(Schema::new(fields), columns, 3)
}

#[test]
fn roundtrip_array_int32_rev0() {
    roundtrip(&array_int32_batch(), 0);
}

#[test]
fn roundtrip_array_int32_tcp_revision() {
    roundtrip(&array_int32_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_nullable_string_rev0() {
    roundtrip(&array_nullable_string_batch(), 0);
}

#[test]
fn roundtrip_array_nullable_string_tcp_revision() {
    roundtrip(&array_nullable_string_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_low_cardinality_rev0() {
    roundtrip(&array_low_cardinality_batch(), 0);
}

#[test]
fn roundtrip_array_low_cardinality_tcp_revision() {
    roundtrip(&array_low_cardinality_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_low_cardinality_all_empty_rev0() {
    roundtrip(&array_low_cardinality_all_empty_batch(), 0);
}

#[test]
fn roundtrip_array_low_cardinality_all_empty_tcp_revision() {
    roundtrip(
        &array_low_cardinality_all_empty_batch(),
        DBMS_TCP_PROTOCOL_VERSION,
    );
}

#[test]
fn roundtrip_array_of_array_rev0() {
    roundtrip(&array_of_array_batch(), 0);
}

#[test]
fn roundtrip_array_of_array_tcp_revision() {
    roundtrip(&array_of_array_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_int128_rev0() {
    roundtrip(&array_int128_batch(), 0);
}

#[test]
fn roundtrip_array_int128_tcp_revision() {
    roundtrip(&array_int128_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_tuple_rev0() {
    roundtrip(&tuple_batch(), 0);
}

#[test]
fn roundtrip_tuple_tcp_revision() {
    roundtrip(&tuple_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nullable_and_lc_tuple_rev0() {
    roundtrip(&nullable_and_lc_tuple_batch(), 0);
}

#[test]
fn roundtrip_nullable_and_lc_tuple_tcp_revision() {
    roundtrip(&nullable_and_lc_tuple_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_and_nested_tuple_rev0() {
    roundtrip(&array_and_nested_tuple_batch(), 0);
}

#[test]
fn roundtrip_array_and_nested_tuple_tcp_revision() {
    roundtrip(&array_and_nested_tuple_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_map_rev0() {
    roundtrip(&map_batch(), 0);
}

#[test]
fn roundtrip_map_tcp_revision() {
    roundtrip(&map_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_map_string_int256_rev0() {
    roundtrip(&map_string_int256_batch(), 0);
}

#[test]
fn roundtrip_map_string_int256_tcp_revision() {
    roundtrip(&map_string_int256_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_lc_and_nested_map_rev0() {
    roundtrip(&lc_and_nested_map_batch(), 0);
}

#[test]
fn roundtrip_lc_and_nested_map_tcp_revision() {
    roundtrip(&lc_and_nested_map_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_of_map_rev0() {
    roundtrip(&array_of_map_batch(), 0);
}

#[test]
fn roundtrip_array_of_map_tcp_revision() {
    roundtrip(&array_of_map_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_map_all_empty_lc_key() {
    // Map(LowCardinality(String), Int32) with rows > 0 but every map empty:
    // the wire must be the hoisted LC key version, the zero offsets, and
    // NOTHING for the key/value runs (limit == 0 gates through the Map
    // path).
    let fields = vec![Field {
        name: "m".into(),
        ch_type: ChType::Map(
            Box::new(ChType::LowCardinality(Box::new(ChType::String))),
            Box::new(ChType::Int32),
        ),
    }];
    let columns = vec![Column::Map(map_column(
        vec![0, 0, 0, 0],
        Column::Dictionary(DictionaryColumn::new(
            vec![],
            Column::Utf8(utf8_column(&[])),
        )),
        Column::Int32(PrimitiveColumn::new(vec![])),
    ))];
    let batch = ColBatch::new(Schema::new(fields), columns, 3);

    // Pin the exact wire body: header, then key version + three zero
    // offsets and nothing else.
    let bytes = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: 0,
            ..EncodeOptions::default()
        },
    )
    .unwrap();
    let mut expected = Vec::new();
    expected.push(0x01); // 1 column
    expected.push(0x03); // 3 rows
    expected.push(0x01); // name len
    expected.extend_from_slice(b"m");
    let type_name = "Map(LowCardinality(String), Int32)";
    expected.push(type_name.len() as u8);
    expected.extend_from_slice(type_name.as_bytes());
    expected.extend_from_slice(&1u64.to_le_bytes()); // hoisted LC key version
    expected.extend_from_slice(&[0u8; 24]); // three zero offsets
    assert_eq!(bytes, expected);

    roundtrip(&batch, 0);
    roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn rev0_frames_map_bytes() {
    // Pin the Map body framing: the Array offsets run (no leading zero),
    // then the flattened key run, then the flattened value run. One
    // Map(String, Int32) column "m" with two rows {hi: 13} and {}.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        }]),
        vec![Column::Map(map_column(
            vec![0, 1, 1],
            Column::Utf8(utf8_column(&[b"hi"])),
            Column::Int32(PrimitiveColumn::new(vec![13])),
        ))],
        2,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x01, b'm', // name "m"
        0x12, // type name length 18
        b'M', b'a', b'p', b'(', b'S', b't', b'r', b'i', b'n', b'g', b',', b' ', b'I', b'n', b't',
        b'3', b'2', b')', // type "Map(String, Int32)"
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 0: 1
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 1: 1
        0x02, b'h', b'i', // key run: varint len 2 then "hi"
        0x0D, 0x00, 0x00, 0x00, // value run: Int32 13
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn map_illegal_key_type_is_rejected() {
    // A Nullable key violates the server's DataTypeMap::isValidKeyType, so
    // the type itself cannot exist: UnsupportedType, before any bytes.
    let ch_type = ChType::Map(
        Box::new(ChType::Nullable(Box::new(ChType::String))),
        Box::new(ChType::Int32),
    );
    let mut keys = utf8_column(&[b"a"]);
    keys.validity = Some(Bitmap::from_ch_null_map(&[0x00]));
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ch_type.clone(),
        }]),
        vec![Column::Map(map_column(
            vec![0, 1],
            Column::Utf8(keys),
            Column::Int32(PrimitiveColumn::new(vec![13])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::UnsupportedType { column, ch_type: t } => {
            assert_eq!(column, "m");
            assert_eq!(t, ch_type);
        }
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

#[test]
fn map_offsets_entries_mismatch_is_rejected() {
    // Offsets end at 2 but the entries tuple holds 1 row: a misframed
    // stream the server would reject, so InconsistentBatch before any
    // bytes.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        }]),
        vec![Column::Map(map_column(
            vec![0, 2],
            Column::Utf8(utf8_column(&[b"a"])),
            Column::Int32(PrimitiveColumn::new(vec![13])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn map_ragged_entries_are_rejected() {
    // Keys and values of different lengths cannot both be full runs of the
    // entry count: InconsistentBatch, not a panic.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        }]),
        vec![Column::Map(map_column(
            vec![0, 2],
            Column::Utf8(utf8_column(&[b"a", b"b"])),
            Column::Int32(PrimitiveColumn::new(vec![13])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn nullable_map_nesting_is_rejected() {
    // Nullable(Map) is not constructible on the server
    // (canBeInsideNullable false); the type-header round-trip check fails
    // before any bytes are written, like Nullable(LowCardinality).
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "nm".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Int32),
            ))),
        }]),
        vec![Column::Map(map_column(
            vec![0, 1],
            Column::Utf8(utf8_column(&[b"a"])),
            Column::Int32(PrimitiveColumn::new(vec![13])),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn roundtrip_empty_tuple_rev0() {
    roundtrip(&empty_tuple_batch(), 0);
}

#[test]
fn roundtrip_empty_tuple_tcp_revision() {
    roundtrip(&empty_tuple_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_array_of_tuple_all_empty() {
    // Array(Tuple(LowCardinality(String), Int32)) with rows > 0 but every
    // array empty: the wire must be the hoisted LC key version, the zero
    // offsets, and NOTHING for the element bodies (each element gets a
    // limit == 0 run through the Tuple path; the LC early-return gate must
    // fire).
    let fields = vec![Field {
        name: "a".into(),
        ch_type: ChType::Array(Box::new(ChType::Tuple(vec![
            (None, ChType::LowCardinality(Box::new(ChType::String))),
            (None, ChType::Int32),
        ]))),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(
        vec![0, 0, 0, 0],
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Dictionary(DictionaryColumn::new(
                    vec![],
                    Column::Utf8(utf8_column(&[])),
                )),
                Column::Int32(PrimitiveColumn::new(vec![])),
            ],
            0,
        )),
    ))];
    let batch = ColBatch::new(Schema::new(fields), columns, 3);

    // Pin the exact wire body: header, then key version + three zero
    // offsets and nothing else.
    let bytes = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: 0,
            ..EncodeOptions::default()
        },
    )
    .unwrap();
    let mut expected = Vec::new();
    expected.push(0x01); // 1 column
    expected.push(0x03); // 3 rows
    expected.push(0x01); // name len
    expected.extend_from_slice(b"a");
    let type_name = "Array(Tuple(LowCardinality(String), Int32))";
    expected.push(type_name.len() as u8);
    expected.extend_from_slice(type_name.as_bytes());
    expected.extend_from_slice(&1u64.to_le_bytes()); // hoisted LC key version
    expected.extend_from_slice(&[0u8; 24]); // three zero offsets
    assert_eq!(bytes, expected);

    roundtrip(&batch, 0);
    roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn encode_chunked_roundtrips_array_blocks() {
    // Array element data is block-local (offsets restart at 0 per block).
    // Two Array(Int32) chunks with different shapes must stay separate
    // after decode, never concatenated.
    let field = Field {
        name: "a".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    };
    let chunk = |offsets: Vec<i64>, values: Vec<i32>| {
        let n = offsets.len() - 1;
        std::sync::Arc::new(ColBatch::new(
            Schema::new(vec![field.clone()]),
            vec![Column::Array(ArrayColumn::new(
                offsets,
                Column::Int32(PrimitiveColumn::new(values)),
            ))],
            n,
        ))
    };
    let batch = ChunkedBatch {
        schema: Schema::new(vec![field.clone()]),
        chunks: vec![
            chunk(vec![0, 2, 2, 3], vec![13, 79, 21]),
            chunk(vec![0, 2], vec![34, 55]),
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
fn rev0_frames_tuple_bytes() {
    // Pin the Tuple body framing: element 0's FULL run then element 1's,
    // column-of-columns, no interleaving, no offsets, no tuple-level
    // framing. One Tuple(Int32, String) column "t" with two rows
    // (13, "hi") and (-1, "").
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![13, -1])),
                Column::Utf8(utf8_column(&[b"hi", b""])),
            ],
            2,
        ))],
        2,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x01, b't', // name "t"
        0x14, // type name length 20
        b'T', b'u', b'p', b'l', b'e', b'(', b'I', b'n', b't', b'3', b'2', b',', b' ', b'S', b't',
        b'r', b'i', b'n', b'g', b')', // type "Tuple(Int32, String)"
        0x0D, 0x00, 0x00, 0x00, // element 0 row 0: Int32 13
        0xFF, 0xFF, 0xFF, 0xFF, // element 0 row 1: Int32 -1
        0x02, b'h', b'i', // element 1 row 0: varint len 2 then "hi"
        0x00, // element 1 row 1: varint len 0
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_empty_tuple_bytes() {
    // Pin the zero-element Tuple() body: exactly one literal ASCII '0'
    // byte (0x30) per row, nothing else.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t0".into(),
            ch_type: ChType::Tuple(vec![]),
        }]),
        vec![Column::Tuple(TupleColumn::new(vec![], 3))],
        3,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x03, // num_rows = 3
        0x02, b't', b'0', // name "t0"
        0x07, b'T', b'u', b'p', b'l', b'e', b'(', b')', // type "Tuple()"
        0x30, 0x30, 0x30, // one ASCII '0' per row
    ];
    assert_eq!(bytes, expected);
}

/// A one-element named-tuple batch over a matching one-field Int8 column,
/// for the element-name legality tests.
fn named_tuple_batch(name: Option<&str>) -> ColBatch {
    ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(name.map(str::to_string), ChType::Int8)]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![Column::Int8(PrimitiveColumn::new(vec![13]))],
            1,
        ))],
        1,
    )
}

#[test]
fn tuple_illegal_element_names_are_rejected() {
    // Mirror the server's checkTupleNames: an empty name and the reserved
    // exact-lowercase "null" cannot exist on the server, so they are
    // UnsupportedType. The decode parser round-trips these shapes (a
    // server-authored header is preserved), so the type-string round-trip
    // check cannot catch them; the explicit name check must.
    for bad in [Some(""), Some("null")] {
        match encode_block(&named_tuple_batch(bad), &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { column, .. } => assert_eq!(column, "t"),
            other => panic!("expected UnsupportedType for {bad:?}, got {other:?}"),
        }
    }
    // Any-case variants other than exact-lowercase "null" are legal on the
    // server (checkTupleNames compares exactly) and render backtick-quoted.
    for ok in [Some("NULL"), Some("Null"), Some("a"), None] {
        encode_block(&named_tuple_batch(ok), &EncodeOptions::default())
            .unwrap_or_else(|e| panic!("{ok:?} should encode: {e}"));
    }
}

#[test]
fn tuple_duplicate_element_names_are_rejected() {
    // checkTupleNames rejects duplicates (DUPLICATE_COLUMN). Unnamed
    // elements do not count as duplicates of each other.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![
                (Some("a".to_string()), ChType::Int8),
                (Some("a".to_string()), ChType::Int8),
            ]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![
                Column::Int8(PrimitiveColumn::new(vec![13])),
                Column::Int8(PrimitiveColumn::new(vec![79])),
            ],
            1,
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::UnsupportedType { column, .. } => assert_eq!(column, "t"),
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

#[test]
fn tuple_mixed_named_unnamed_elements_are_rejected() {
    // The server's tuple type factory rejects mixed named/unnamed
    // arguments ("Names are specified not for all elements of Tuple
    // type"), so a mixed ChType is caller-constructed-only and its
    // rendered header cannot be parsed back by the server.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![
                (Some("a".to_string()), ChType::Int8),
                (None, ChType::Int8),
            ]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![
                Column::Int8(PrimitiveColumn::new(vec![13])),
                Column::Int8(PrimitiveColumn::new(vec![79])),
            ],
            1,
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::UnsupportedType { column, .. } => assert_eq!(column, "t"),
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

#[test]
fn nested_tuple_illegal_names_are_rejected() {
    // The name legality check applies through nesting: a duplicate-named
    // tuple as an Array element is rejected by the recursive validation.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::Tuple(vec![
                (Some("x".to_string()), ChType::Int8),
                (Some("x".to_string()), ChType::Int8),
            ]))),
        }]),
        vec![Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int8(PrimitiveColumn::new(vec![13])),
                    Column::Int8(PrimitiveColumn::new(vec![79])),
                ],
                1,
            )),
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::UnsupportedType { .. } => {}
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

#[test]
fn plain_tuple_validity_with_nulls_is_rejected() {
    // A non-Nullable Tuple field whose TupleColumn carries null-marked
    // validity would have the null map silently dropped (no null map is
    // written for a non-nullable column), so the generic nullability check
    // rejects it, the same as every other non-nullable column type.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int8)]),
        }]),
        vec![Column::Tuple(TupleColumn::new_nullable(
            vec![Column::Int8(PrimitiveColumn::new(vec![13, 0]))],
            2,
            Bitmap::from_ch_null_map(&[0x00, 0x01]),
        ))],
        2,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn map_entries_validity_is_rejected() {
    // The Map entries tuple never carries validity on the wire;
    // encode_map_data writes no null map for it, so a caller-attached
    // bitmap (even all-valid) would be silently dropped. Rejected before
    // any bytes.
    let entries = Column::Tuple(TupleColumn::new_nullable(
        vec![
            Column::Utf8(utf8_column(&[b"a"])),
            Column::Int32(PrimitiveColumn::new(vec![13])),
        ],
        1,
        Bitmap::from_ch_null_map(&[0x00]),
    ));
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        }]),
        vec![Column::Map(MapColumn::new(vec![0, 1], entries))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { detail } => {
            assert!(detail.contains("entries"), "got detail {detail:?}");
        }
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn tuple_field_count_mismatch_is_rejected() {
    // The declared type has two elements; the buffer carries one field
    // column. InconsistentBatch, before any bytes are written.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![Column::Int32(PrimitiveColumn::new(vec![13]))],
            1,
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn tuple_ragged_element_lengths_are_rejected() {
    // Element 0 has two rows, element 1 has one: a ragged tuple would put a
    // misframed stream on the wire (the server's equal-sizes INCORRECT_DATA
    // invariant), so it is InconsistentBatch, not a panic.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![13, 79])),
                Column::Utf8(utf8_column(&[b"user_1"])),
            ],
            2,
        ))],
        2,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn tuple_mismatched_element_buffer_is_rejected() {
    // A declared Int64 element over an Int32 buffer is a wrong-buffer
    // mismatch, not a wrong-width column on the wire.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int64)]),
        }]),
        vec![Column::Tuple(TupleColumn::new(
            vec![Column::Int32(PrimitiveColumn::new(vec![13]))],
            1,
        ))],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn tuple_type_depth_is_capped_via_worklist() {
    // A pathologically deep caller-constructed type must be rejected by the
    // iterative depth walk before any recursive machinery touches it. Tuple
    // is the multi-child container, so this exercises the worklist path
    // with a depth well past MAX_TYPE_DEPTH.
    let mut ch_type = ChType::Int8;
    let mut column = Column::Int8(PrimitiveColumn::new(vec![13]));
    for _ in 0..(MAX_TYPE_DEPTH * 4) {
        ch_type = ChType::Tuple(vec![(None, ch_type)]);
        column = Column::Tuple(TupleColumn::new(vec![column], 1));
    }
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "deep".into(),
            ch_type,
        }]),
        vec![column],
        1,
    );
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { detail } => {
            assert!(detail.contains("nesting exceeds"), "got detail {detail:?}");
        }
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn rev0_frames_array_int32_bytes() {
    // Pin the Array body framing: one raw LE u64 cumulative end-offset per
    // row with NO leading zero and no count, then the flattened element
    // body. Two rows [13, 79] and [] (the empty row repeats the previous
    // end-offset).
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::Int32)),
        }]),
        vec![Column::Array(ArrayColumn::new(
            vec![0, 2, 2],
            Column::Int32(PrimitiveColumn::new(vec![13, 79])),
        ))],
        2,
    );
    let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x01, b'a', // name "a"
        0x0C, b'A', b'r', b'r', b'a', b'y', b'(', b'I', b'n', b't', b'3', b'2',
        b')', // type "Array(Int32)"
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 0 = 2
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 1 = 2
        0x0D, 0x00, 0x00, 0x00, // Int32 13, little-endian
        0x4F, 0x00, 0x00, 0x00, // Int32 79, little-endian
    ];
    assert_eq!(bytes, expected);
}

#[test]
fn rev0_frames_array_low_cardinality_all_empty_bytes() {
    // Pin the all-empty Array(LowCardinality(String)) shape: the hoisted LC
    // key version FIRST (the element state prefix, before the offsets), then
    // the all-zero offsets, then NOTHING for the LC element run
    // (`SerializationLowCardinality::serializeBinaryBulkWithMultipleStreams`
    // early-returns at limit == 0; confirmed at v26.6.1.1193-stable). An
    // index word, key count, or row count here would make the server
    // misparse the INSERT.
    let bytes = encode_block(
        &array_low_cardinality_all_empty_batch(),
        &EncodeOptions::default(),
    )
    .unwrap();
    let expected = [
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x03, b'a', b'l', b'c', // name "alc"
        0x1D, b'A', b'r', b'r', b'a', b'y', b'(', b'L', b'o', b'w', b'C', b'a', b'r', b'd', b'i',
        b'n', b'a', b'l', b'i', b't', b'y', b'(', b'S', b't', b'r', b'i', b'n', b'g', b')',
        b')', // type "Array(LowCardinality(String))"
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // hoisted LC key version = 1
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 0 = 0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, // offset row 1 = 0
              // nothing else: zero-length LC element run writes no body
    ];
    assert_eq!(bytes, expected);
}

/// Build a one-column `Array(Int32)` batch directly from raw offsets and
/// leaf values so a malformed offset array reaches the encoder. `num_rows`
/// is passed explicitly so only the Array invariants under test, not the
/// row-count check, are exercised.
fn array_batch_from_parts(offsets: Vec<i64>, values: Vec<i32>, num_rows: usize) -> ColBatch {
    ColBatch {
        schema: Schema::new(vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::Int32)),
        }]),
        columns: vec![Column::Array(ArrayColumn::new(
            offsets,
            Column::Int32(PrimitiveColumn::new(values)),
        ))],
        num_rows,
    }
}

#[test]
fn array_non_monotonic_offsets_are_rejected() {
    // Decreasing offsets would frame a stream the server rejects with
    // INCORRECT_DATA (and would slice out of bounds on our own decode).
    let batch = array_batch_from_parts(vec![0, 3, 1], vec![13, 79, 21], 2);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn array_offset_element_count_mismatch_is_rejected() {
    // A final offset that does not equal the flattened element count would
    // either silently drop trailing elements (too small) or declare
    // elements the body does not carry (too large). Both directions.
    for (offsets, values) in [
        (vec![0i64, 2], vec![13, 79, 21]), // ends at 2, holds 3
        (vec![0i64, 3], vec![13, 79]),     // ends at 3, holds 2
    ] {
        let batch = array_batch_from_parts(offsets, values, 1);
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }
}

#[test]
fn array_missing_leading_zero_offset_is_rejected() {
    // Arrow list offsets start at 0; a nonzero first offset means the
    // leading zero is missing and row 0's slice would drop leading elements.
    let batch = array_batch_from_parts(vec![1, 3], vec![13, 79, 21], 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn array_wrong_offsets_length_is_rejected() {
    // At a nonzero row count the offsets must be exactly num_rows + 1 (a leading
    // 0 plus one end-offset per row). A single [0] over one row is one short and
    // is rejected before any element bytes are written.
    let batch = array_batch_from_parts(vec![0], vec![], 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn array_zero_rows_accepts_empty_or_sentinel_offsets() {
    // Uniform zero-row offset policy, shared by String/Array/Map/AggregateFunction:
    // a zero-row column accepts either an empty offsets vec or the single [0]
    // sentinel the decoder emits, both over empty element data. An empty vec
    // reports 0 rows through ArrayColumn::len()'s saturating subtraction, so it
    // passes the row-count check, and the shared validator accepts it leniently
    // rather than regressing bindings that hand back an empty offsets buffer.
    for offsets in [vec![], vec![0i64]] {
        let batch = array_batch_from_parts(offsets, vec![], 0);
        encode_block(&batch, &EncodeOptions::default())
            .expect("zero-row array with empty or [0] offsets should encode");
    }
}

#[test]
fn array_negative_offset_is_rejected() {
    // A negative offset would wrap through the i64 -> u64 cast into a huge
    // wire offset. The monotonic check from the zero start catches it.
    let batch = array_batch_from_parts(vec![0, -1], vec![], 1);
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn array_element_validation_failure_is_rejected() {
    // Element-level guards must apply to the flattened element column: a
    // String element whose Utf8 offsets point past its data buffer would
    // panic mid-write, so the recursive element validation rejects it
    // through the Array before any bytes are written.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::String)),
        }]),
        columns: vec![Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::Utf8(Utf8Column::new(vec![0, 10], b"abc".to_vec())),
        ))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::InconsistentBatch { .. } => {}
        other => panic!("expected InconsistentBatch, got {other:?}"),
    }
}

#[test]
fn array_forbidden_low_cardinality_element_is_unsupported() {
    // Decimal is encodable as a plain column but forbidden inside
    // LowCardinality (`canBeInsideLowCardinality()` is false), and nesting
    // that LC inside an Array must not launder it: the element validation
    // recurses and reports UnsupportedType before any bytes are written.
    let batch = ColBatch {
        schema: Schema::new(vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(
                ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 32,
                },
            )))),
        }]),
        columns: vec![Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::Dictionary(DictionaryColumn::new(
                vec![0],
                Column::Decimal(DecimalColumn::new(vec![0u8; 4], 4, 9, 4)),
            )),
        ))],
        num_rows: 1,
    };
    match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
        EncodeError::UnsupportedType { .. } => {}
        other => panic!("expected UnsupportedType, got {other:?}"),
    }
}

/// `Nested(a UInt32, b String)` over two rows: delegates to
/// `Array(Tuple(a UInt32, b String))`. Row 0 has two elements, row 1 has one.
fn nested_batch() -> ColBatch {
    let entries = Column::Tuple(TupleColumn::new(
        vec![
            Column::UInt32(PrimitiveColumn::new(vec![10, 20, 30])),
            Column::Utf8(utf8_column(&[b"x", b"y", b"z"])),
        ],
        3,
    ));
    let fields = vec![Field {
        name: "n".into(),
        ch_type: ChType::Nested(vec![
            ("a".into(), ChType::UInt32),
            ("b".into(), ChType::String),
        ]),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(vec![0, 2, 3], entries))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

/// `Nested(a LowCardinality(String))` over two rows: the shared LC gate, so
/// the LC key version is hoisted to the front of the whole column, ahead of
/// the Array offsets.
fn nested_low_cardinality_batch() -> ColBatch {
    let entries = Column::Tuple(TupleColumn::new(
        vec![Column::Dictionary(DictionaryColumn::new(
            vec![1, 2, 1],
            Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
        ))],
        3,
    ));
    let fields = vec![Field {
        name: "n".into(),
        ch_type: ChType::Nested(vec![(
            "a".into(),
            ChType::LowCardinality(Box::new(ChType::String)),
        )]),
    }];
    let columns = vec![Column::Array(ArrayColumn::new(vec![0, 2, 3], entries))];
    ColBatch::new(Schema::new(fields), columns, 2)
}

#[test]
fn roundtrip_nested_rev0() {
    roundtrip(&nested_batch(), 0);
}

#[test]
fn roundtrip_nested_tcp_revision() {
    roundtrip(&nested_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn roundtrip_nested_low_cardinality_rev0() {
    roundtrip(&nested_low_cardinality_batch(), 0);
}

#[test]
fn roundtrip_nested_low_cardinality_tcp_revision() {
    roundtrip(&nested_low_cardinality_batch(), DBMS_TCP_PROTOCOL_VERSION);
}

#[test]
fn rev0_frames_nested_bytes() {
    // Nested body is the Array(Tuple(a, b)) shape: offsets[1..] as raw LE u64,
    // then the flattened tuple body field-major (all a's then all b's).
    let bytes = encode_block(&nested_batch(), &EncodeOptions::default()).unwrap();
    let mut expected = vec![
        0x01, // num_cols = 1
        0x02, // num_rows = 2
        0x01, b'n', // name "n"
        0x1A, // type string length = 26
    ];
    expected.extend_from_slice(b"Nested(a UInt32, b String)");
    // Offsets [2, 3] as raw LE u64 (no leading zero).
    expected.extend_from_slice(&2u64.to_le_bytes());
    expected.extend_from_slice(&3u64.to_le_bytes());
    // Field a: UInt32 [10, 20, 30].
    expected.extend_from_slice(&10u32.to_le_bytes());
    expected.extend_from_slice(&20u32.to_le_bytes());
    expected.extend_from_slice(&30u32.to_le_bytes());
    // Field b: String [x, y, z] as varint len + bytes.
    expected.extend_from_slice(&[0x01, b'x', 0x01, b'y', 0x01, b'z']);
    assert_eq!(bytes, expected);
}

#[test]
fn encode_rejects_nested_duplicate_names() {
    // Duplicate element names fail `checkTupleNames` through the Tuple
    // delegation, reported as `UnsupportedType`.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nested(vec![
                ("a".into(), ChType::UInt32),
                ("a".into(), ChType::String),
            ]),
        }]),
        vec![Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::UInt32(PrimitiveColumn::new(vec![10])),
                    Column::Utf8(utf8_column(&[b"x"])),
                ],
                1,
            )),
        ))],
        1,
    );
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::UnsupportedType { .. })
    ));
}

#[test]
fn encode_rejects_nested_empty_name() {
    // An empty element name is unconstructible on the server, rejected via the
    // Tuple delegation.
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nested(vec![("".into(), ChType::UInt32)]),
        }]),
        vec![Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::Tuple(TupleColumn::new(
                vec![Column::UInt32(PrimitiveColumn::new(vec![10]))],
                1,
            )),
        ))],
        1,
    );
    assert!(matches!(
        encode_block(&batch, &EncodeOptions::default()),
        Err(EncodeError::UnsupportedType { .. })
    ));
}
