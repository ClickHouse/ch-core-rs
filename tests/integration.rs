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

fn assert_schema(batch: &ChunkedBatch, expected: &[(&str, ChType)]) {
    assert_eq!(batch.num_columns(), expected.len());
    for (field, (name, ch_type)) in batch.schema.fields.iter().zip(expected) {
        assert_eq!(field.name, *name);
        assert_eq!(&field.ch_type, ch_type);
    }
}

fn assert_all_types(batch: &ChunkedBatch) {
    assert_eq!(batch.num_chunks(), 1);
    assert_eq!(batch.num_rows(), 4);
    assert_schema(
        batch,
        &[
            ("i8", ChType::Int8),
            ("i16", ChType::Int16),
            ("i32", ChType::Int32),
            ("i64", ChType::Int64),
            ("u8", ChType::UInt8),
            ("u16", ChType::UInt16),
            ("u32", ChType::UInt32),
            ("u64", ChType::UInt64),
            ("f32", ChType::Float32),
            ("f64", ChType::Float64),
            ("b", ChType::Bool),
            ("s", ChType::String),
            ("fs", ChType::FixedString(4)),
            ("ni32", ChType::Nullable(Box::new(ChType::Int32))),
            ("ns", ChType::Nullable(Box::new(ChType::String))),
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
}

fn assert_multi_block(batch: &ChunkedBatch) {
    assert_eq!(batch.num_chunks(), 3);
    assert_eq!(batch.num_rows(), 5);
    assert_schema(batch, &[("n", ChType::Int32)]);

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
