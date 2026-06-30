use ch_core_rs::batch::ChunkedBatch;
use ch_core_rs::column::{Column, FixedBinaryColumn, Utf8Column};
use ch_core_rs::native::decode::{decode_all_bytes, DecodeOptions, DBMS_TCP_PROTOCOL_VERSION};
use ch_core_rs::schema::ChType;

/// Declare one `#[test]` per committed Native fixture. Each generated test
/// decodes the fixture bytes through the public API and runs its asserter, so
/// libtest reports each fixture separately, a failure names the specific
/// fixture, and one bad fixture does not abort the others.
macro_rules! fixture_tests {
    ($($name:ident: {
        file: $file:literal,
        protocol_revision: $revision:expr,
        assert: $assert:path $(,)?
    }),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                let options = DecodeOptions {
                    protocol_revision: $revision,
                };
                let bytes = include_bytes!(concat!("fixtures/", $file));
                let batch = decode_all_bytes(bytes, &options)
                    .unwrap_or_else(|err| panic!("failed to decode {}: {err}", $file));
                $assert(&batch);
            }
        )*
    };
}

fixture_tests! {
    all_types_rev0: {
        file: "all_types_rev0.native",
        protocol_revision: 0,
        assert: assert_all_types,
    },
    all_types_rev54483: {
        file: "all_types_rev54483.native",
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
        assert: assert_all_types,
    },
    multi_block_rev0: {
        file: "multi_block_rev0.native",
        protocol_revision: 0,
        assert: assert_multi_block,
    },
}

/// Expected schema entry. Most columns pin an exact `ChType`. The `dt_utc`
/// column is the one exception: the server emits a different type string for it
/// depending on the negotiated protocol revision (see `assert_all_types`), so
/// it is matched against a set of acceptable types instead of one.
enum Expected<'a> {
    Exact(&'a str, ChType),
    AnyOf(&'a str, &'a [ChType]),
}

fn assert_schema(batch: &ChunkedBatch, expected: &[Expected]) {
    assert_eq!(batch.num_columns(), expected.len());
    for (field, exp) in batch.schema.fields.iter().zip(expected) {
        match exp {
            Expected::Exact(name, ch_type) => {
                assert_eq!(field.name, *name);
                assert_eq!(&field.ch_type, ch_type);
            }
            Expected::AnyOf(name, types) => {
                assert_eq!(field.name, *name);
                assert!(
                    types.contains(&field.ch_type),
                    "column {name}: type {:?} not in {types:?}",
                    field.ch_type
                );
            }
        }
    }
}

fn assert_all_types(batch: &ChunkedBatch) {
    assert_eq!(batch.num_chunks(), 1);
    assert_eq!(batch.num_rows(), 4);
    assert_schema(
        batch,
        &[
            Expected::Exact("i8", ChType::Int8),
            Expected::Exact("i16", ChType::Int16),
            Expected::Exact("i32", ChType::Int32),
            Expected::Exact("i64", ChType::Int64),
            Expected::Exact("u8", ChType::UInt8),
            Expected::Exact("u16", ChType::UInt16),
            Expected::Exact("u32", ChType::UInt32),
            Expected::Exact("u64", ChType::UInt64),
            Expected::Exact("f32", ChType::Float32),
            Expected::Exact("f64", ChType::Float64),
            Expected::Exact("b", ChType::Bool),
            Expected::Exact("s", ChType::String),
            Expected::Exact("fs", ChType::FixedString(4)),
            Expected::Exact("ni32", ChType::Nullable(Box::new(ChType::Int32))),
            Expected::Exact("ns", ChType::Nullable(Box::new(ChType::String))),
            // Temporal columns, with the exact type strings this server
            // (v26.2.4.23-stable) emits. A bare DateTime stays bare. The
            // DateTime64 columns keep their precision and timezone in both
            // captures.
            Expected::Exact("d", ChType::Date),
            Expected::Exact("d32", ChType::Date32),
            Expected::Exact("dt", ChType::DateTime { timezone: None }),
            // dt_utc is declared DateTime('UTC') in the query, but the emitted
            // type string depends on the negotiated protocol revision: the
            // rev54483 capture keeps DateTime('UTC'), while the rev0 capture
            // (HTTP FORMAT Native with no client_protocol_version) drops the
            // timezone and emits a bare DateTime. Both are what the server
            // actually wrote, so accept either. The raw seconds are identical
            // either way, and are asserted below.
            Expected::AnyOf(
                "dt_utc",
                &[
                    ChType::DateTime {
                        timezone: Some(String::from("UTC")),
                    },
                    ChType::DateTime { timezone: None },
                ],
            ),
            Expected::Exact(
                "dt64",
                ChType::DateTime64 {
                    precision: 3,
                    timezone: None,
                },
            ),
            Expected::Exact(
                "dt64_utc",
                ChType::DateTime64 {
                    precision: 3,
                    timezone: Some("UTC".to_string()),
                },
            ),
            Expected::Exact("lc", ChType::LowCardinality(Box::new(ChType::String))),
            Expected::Exact(
                "lcn",
                ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String)))),
            ),
            // LowCardinality over non-String inners, captured with
            // allow_suspicious_low_cardinality_types=1.
            Expected::Exact("lc_u32", ChType::LowCardinality(Box::new(ChType::UInt32))),
            Expected::Exact("lc_date", ChType::LowCardinality(Box::new(ChType::Date))),
            Expected::Exact(
                "lcn_u32",
                ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::UInt32)))),
            ),
        ],
    );

    let block = &batch.chunks[0];
    assert_eq!(block.num_rows, 4);

    match block.column(0) {
        Column::Int8(c) => assert_eq!(c.values.as_slice(), &[-128, -1, 0, 127]),
        other => panic!("expected Int8, got {other:?}"),
    }
    match block.column(1) {
        Column::Int16(c) => assert_eq!(c.values.as_slice(), &[-32768, -13, 0, 32767]),
        other => panic!("expected Int16, got {other:?}"),
    }
    match block.column(2) {
        Column::Int32(c) => {
            assert_eq!(c.values.as_slice(), &[i32::MIN, -79, 0, i32::MAX]);
        }
        other => panic!("expected Int32, got {other:?}"),
    }
    match block.column(3) {
        Column::Int64(c) => {
            assert_eq!(c.values.as_slice(), &[i64::MIN, -79, 0, i64::MAX]);
        }
        other => panic!("expected Int64, got {other:?}"),
    }
    match block.column(4) {
        Column::UInt8(c) => assert_eq!(c.values.as_slice(), &[0, 13, 79, u8::MAX]),
        other => panic!("expected UInt8, got {other:?}"),
    }
    match block.column(5) {
        Column::UInt16(c) => assert_eq!(c.values.as_slice(), &[0, 13, 79, u16::MAX]),
        other => panic!("expected UInt16, got {other:?}"),
    }
    match block.column(6) {
        Column::UInt32(c) => assert_eq!(c.values.as_slice(), &[0, 13, 79, u32::MAX]),
        other => panic!("expected UInt32, got {other:?}"),
    }
    match block.column(7) {
        Column::UInt64(c) => assert_eq!(c.values.as_slice(), &[0, 13, 79, u64::MAX]),
        other => panic!("expected UInt64, got {other:?}"),
    }
    match block.column(8) {
        Column::Float32(c) => assert_eq!(c.values.as_slice(), &[-1.25, 0.0, 3.5, 79.125]),
        other => panic!("expected Float32, got {other:?}"),
    }
    match block.column(9) {
        Column::Float64(c) => assert_eq!(c.values.as_slice(), &[-1.25, 0.0, 3.5, 79.125]),
        other => panic!("expected Float64, got {other:?}"),
    }
    match block.column(10) {
        Column::Bool(c) => {
            assert!(!c.get(0));
            assert!(c.get(1));
            assert!(!c.get(2));
            assert!(c.get(3));
        }
        other => panic!("expected Bool, got {other:?}"),
    }

    assert_utf8_values(
        block.column(11),
        &[b"" as &[u8], b"user_1", &[0xff, 0x00], b"user_2"],
    );
    assert_fixed_binary_values(
        block.column(12),
        &[
            b"x\0\0\0" as &[u8],
            b"ABCD",
            b"\0\0\0\0",
            &[0xff, 0x00, 0x00, 0x00],
        ],
    );

    match block.column(13) {
        Column::Int32(c) => {
            assert_eq!(c.values.as_slice(), &[-7, 0, -5, 0]);
            assert_eq!(c.null_count(), 2);
        }
        other => panic!("expected nullable Int32 storage, got {other:?}"),
    }
    assert_validity(block.column(13), &[true, false, true, false]);

    assert_utf8_values(block.column(14), &[b"user_0" as &[u8], b"", b"user_2", b""]);
    assert_validity(block.column(14), &[true, false, true, false]);

    // Temporal columns. Raw integers were derived independently from the server
    // (toUInt16/toInt32/toUInt32 of the values, and reinterpretAsInt64 of the
    // DateTime64 ticks), not by decoding with this crate.
    match block.column(15) {
        Column::Date(c) => assert_eq!(c.values.as_slice(), &[0u16, 19737, 49710, 65535]),
        other => panic!("expected Date, got {other:?}"),
    }
    match block.column(16) {
        Column::Date32(c) => assert_eq!(c.values.as_slice(), &[-7227i32, 0, 19737, 84370]),
        other => panic!("expected Date32, got {other:?}"),
    }
    // The bare DateTime and DateTime('UTC') columns carry identical raw seconds;
    // timezone is type metadata only, with no effect on the wire bytes.
    let expected_seconds = &[0u32, 1705322096, 961056000, 4294967295];
    match block.column(17) {
        Column::DateTime(c) => assert_eq!(c.values.as_slice(), expected_seconds),
        other => panic!("expected DateTime, got {other:?}"),
    }
    match block.column(18) {
        Column::DateTime(c) => assert_eq!(c.values.as_slice(), expected_seconds),
        other => panic!("expected DateTime, got {other:?}"),
    }
    // DateTime64(3) ticks are milliseconds since epoch, including a pre-epoch
    // negative tick. The bare and 'UTC' variants share the same raw ticks.
    let expected_ticks = &[-877i64, 0, 1705322096789, 4102444799999];
    match block.column(19) {
        Column::DateTime64(c) => assert_eq!(c.values.as_slice(), expected_ticks),
        other => panic!("expected DateTime64, got {other:?}"),
    }
    match block.column(20) {
        Column::DateTime64(c) => assert_eq!(c.values.as_slice(), expected_ticks),
        other => panic!("expected DateTime64, got {other:?}"),
    }

    // LowCardinality(String): rows resolve to user_1, user_2, user_1, user_3
    // against the per-block dictionary. No nulls.
    assert_dictionary_string_values(
        block.column(21),
        &[
            Some(b"user_1" as &[u8]),
            Some(b"user_2"),
            Some(b"user_1"),
            Some(b"user_3"),
        ],
    );
    // LowCardinality(Nullable(String)): rows 1 and 3 are NULL (wire index 0 maps
    // to the null sentinel), rows 0 and 2 are real values.
    assert_dictionary_string_values(
        block.column(22),
        &[Some(b"user_0" as &[u8]), None, Some(b"user_2"), None],
    );
    assert_validity(block.column(22), &[true, false, true, false]);

    // LowCardinality(UInt32): a primitive dictionary body (raw 4-byte LE), with a
    // repeated value so the dictionary is smaller than the row count. Rows resolve
    // to 13, 79, 13, 4294967295.
    assert_dictionary_u32_values(
        block.column(23),
        &[Some(13), Some(79), Some(13), Some(u32::MAX)],
    );
    // LowCardinality(Date): a UInt16 dictionary body. Rows resolve to the raw days
    // 19737, 49710, 19737, 0.
    assert_dictionary_date_values(
        block.column(24),
        &[Some(19737), Some(49710), Some(19737), Some(0)],
    );
    // LowCardinality(Nullable(UInt32)): rows 1 and 3 NULL via the index-0
    // sentinel, rows 0 and 2 the values 13 and 79.
    assert_dictionary_u32_values(block.column(25), &[Some(13), None, Some(79), None]);
    assert_validity(block.column(25), &[true, false, true, false]);
}

/// Assert the per-row resolved `UInt32` values of a dictionary column, treating a
/// null index as `None`, the way a consumer reads them.
fn assert_dictionary_u32_values(column: &Column, expected: &[Option<u32>]) {
    match column {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), expected.len());
            let values = match d.values.as_ref() {
                Column::UInt32(v) => v,
                other => panic!("expected UInt32 dictionary values, got {other:?}"),
            };
            for (row, want) in expected.iter().enumerate() {
                let is_null = d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row));
                match want {
                    None => assert!(is_null, "row {row} expected null"),
                    Some(v) => {
                        assert!(!is_null, "row {row} expected a value, got null");
                        let idx = d.indices[row] as usize;
                        assert_eq!(values.values[idx], *v, "row {row}");
                    }
                }
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

/// Assert the per-row resolved `Date` (UInt16 days) values of a dictionary column.
fn assert_dictionary_date_values(column: &Column, expected: &[Option<u16>]) {
    match column {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), expected.len());
            let values = match d.values.as_ref() {
                Column::Date(v) => v,
                other => panic!("expected Date dictionary values, got {other:?}"),
            };
            for (row, want) in expected.iter().enumerate() {
                let is_null = d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row));
                match want {
                    None => assert!(is_null, "row {row} expected null"),
                    Some(v) => {
                        assert!(!is_null, "row {row} expected a value, got null");
                        let idx = d.indices[row] as usize;
                        assert_eq!(values.values[idx], *v, "row {row}");
                    }
                }
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

/// Assert the per-row resolved string values of a dictionary (`LowCardinality`)
/// column, treating a null index as `None`, the way a consumer reads them.
fn assert_dictionary_string_values(column: &Column, expected: &[Option<&[u8]>]) {
    match column {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), expected.len());
            let values = match d.values.as_ref() {
                Column::Utf8(v) => v,
                other => panic!("expected Utf8 dictionary values, got {other:?}"),
            };
            for (row, want) in expected.iter().enumerate() {
                let is_null = d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row));
                match want {
                    None => assert!(is_null, "row {row} expected null"),
                    Some(bytes) => {
                        assert!(!is_null, "row {row} expected a value, got null");
                        let idx = d.indices[row] as usize;
                        assert_eq!(values.value(idx), *bytes, "row {row}");
                    }
                }
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

fn assert_multi_block(batch: &ChunkedBatch) {
    assert_eq!(batch.num_chunks(), 3);
    assert_eq!(batch.num_rows(), 5);
    assert_schema(batch, &[Expected::Exact("n", ChType::Int32)]);

    assert_int32_chunk(batch, 0, &[13, 14]);
    assert_int32_chunk(batch, 1, &[15, 16]);
    assert_int32_chunk(batch, 2, &[17]);
}

fn assert_int32_chunk(batch: &ChunkedBatch, chunk: usize, expected: &[i32]) {
    let block = &batch.chunks[chunk];
    assert_eq!(block.num_rows, expected.len());
    match block.column(0) {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), expected),
        other => panic!("expected Int32, got {other:?}"),
    }
}

fn assert_utf8_values(column: &Column, expected: &[&[u8]]) {
    match column {
        Column::Utf8(c) => assert_utf8_column(c, expected),
        other => panic!("expected Utf8, got {other:?}"),
    }
}

fn assert_utf8_column(column: &Utf8Column, expected: &[&[u8]]) {
    assert_eq!(column.len(), expected.len());
    for (row, expected_value) in expected.iter().enumerate() {
        assert_eq!(column.value(row), *expected_value, "row {row}");
    }
}

fn assert_fixed_binary_values(column: &Column, expected: &[&[u8]]) {
    match column {
        Column::FixedBinary(c) => assert_fixed_binary_column(c, expected),
        other => panic!("expected FixedBinary, got {other:?}"),
    }
}

fn assert_fixed_binary_column(column: &FixedBinaryColumn, expected: &[&[u8]]) {
    assert_eq!(column.len(), expected.len());
    for (row, expected_value) in expected.iter().enumerate() {
        assert_eq!(column.value(row), *expected_value, "row {row}");
    }
}

fn assert_validity(column: &Column, expected: &[bool]) {
    let validity = column.validity().expect("expected validity bitmap");
    assert_eq!(validity.len(), expected.len());
    for (row, expected_valid) in expected.iter().enumerate() {
        assert_eq!(validity.is_valid(row), *expected_valid, "row {row}");
    }
}
