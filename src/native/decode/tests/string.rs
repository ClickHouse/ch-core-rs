use super::*;

#[test]
fn test_decode_fixed_string() {
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("fs", "FixedString(3)")
        .raw_bytes(b"abcdef")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::FixedBinary(c) => {
            assert_eq!(c.len(), 2);
            assert_eq!(c.value(0), b"abc");
            assert_eq!(c.value(1), b"def");
        }
        _ => panic!("expected FixedBinary"),
    }
}

#[test]
fn test_decode_string() {
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("s", "String")
        .string_data(&["hello", "", "world!"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Utf8(c) => {
            assert_eq!(c.len(), 3);
            assert_eq!(c.value(0), b"hello");
            assert_eq!(c.value(1), b"");
            assert_eq!(c.value(2), b"world!");
        }
        _ => panic!("expected Utf8"),
    }
}

#[test]
fn test_decode_string_multi_row_roundtrip() {
    // Exercise the slice-borrowing string path over many rows, including
    // empty strings, multi-byte UTF-8, and a length that crosses the
    // single-byte varint boundary (>= 128 bytes -> two-byte prefix).
    let long = "x".repeat(200);
    let values = [
        "user_1",
        "",
        "user_2",
        "naive_caf\u{00e9}", // multi-byte UTF-8
        long.as_str(),
        "13",
    ];
    let data = BlockBuilder::new()
        .header(1, values.len())
        .column_header("s", "String")
        .string_data(&values)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Utf8(c) => {
            assert_eq!(c.len(), values.len());
            for (i, v) in values.iter().enumerate() {
                assert_eq!(c.value(i), v.as_bytes());
            }
            // Offsets are monotonic and cover exactly the data buffer.
            assert_eq!(*c.offsets.last().unwrap() as usize, c.data.len());
        }
        _ => panic!("expected Utf8"),
    }
}

#[test]
fn test_decode_nullable_string_roundtrip() {
    // Nullable(String): null map then the string payload. The null rows
    // still carry a (here empty) value on the wire.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("s", "Nullable(String)")
        .null_map(&[false, true, false])
        .string_data(&["user_1", "", "user_2"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Utf8(c) => {
            assert_eq!(c.len(), 3);
            assert_eq!(c.null_count(), 1);
            assert_eq!(c.value(0), b"user_1");
            assert_eq!(c.value(2), b"user_2");
        }
        _ => panic!("expected Utf8"),
    }
}

#[test]
fn test_fixed_string_zero_width_rejected() {
    // FixedString(0) is not a valid ClickHouse type and cannot be
    // represented in the width * num_rows buffer, so it parses to None and
    // decoding reports UnsupportedType rather than an inconsistent column.
    assert_eq!(parse_ch_type("FixedString(0)"), None);
    assert_eq!(
        parse_ch_type("FixedString(1)"),
        Some(ChType::FixedString(1))
    );

    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("fs", "FixedString(0)")
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}
