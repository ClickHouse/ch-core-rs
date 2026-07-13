use super::*;
use crate::batch::ColBatch;
use crate::bitmap::Bitmap;
use crate::column::{
    ArrayColumn, Column, DictionaryColumn, PrimitiveColumn, TupleColumn, Utf8Column,
};
use crate::schema::{ChType, Field, GeoKind, Schema};
use std::ffi::CStr;

fn make_test_batch() -> Arc<ColBatch> {
    let schema = Schema::new(vec![
        Field {
            name: "i".into(),
            ch_type: ChType::Int64,
        },
        Field {
            name: "f".into(),
            ch_type: ChType::Float64,
        },
        Field {
            name: "s".into(),
            ch_type: ChType::String,
        },
    ]);
    let columns = vec![
        Column::Int64(PrimitiveColumn::new(vec![10, 20])),
        Column::Float64(PrimitiveColumn::new(vec![1.5, 2.5])),
        Column::Utf8(Utf8Column::new(vec![0, 2, 5], b"abcde".to_vec())),
    ];
    Arc::new(ColBatch::new(schema, columns, 2))
}

#[test]
fn test_export_schema_format_strings() {
    let batch = make_test_batch();
    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema);

        let fmt = CStr::from_ptr(schema.format).to_str().unwrap();
        assert_eq!(fmt, "+s");
        assert_eq!(schema.n_children, 3);

        let c0 = &**schema.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "l");

        let c1 = &**schema.children.add(1);
        assert_eq!(CStr::from_ptr(c1.format).to_str().unwrap(), "g");

        let c2 = &**schema.children.add(2);
        assert_eq!(CStr::from_ptr(c2.format).to_str().unwrap(), "u");

        (schema.release.unwrap())(&mut schema);
    }
}

#[test]
fn test_export_array_buffers() {
    let batch = make_test_batch();
    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);

        assert_eq!(array.length, 2);
        assert_eq!(array.n_children, 3);

        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 2);
        assert_eq!(c0.n_buffers, 2);
        assert!((*c0.buffers.add(0)).is_null()); // non-nullable
        let data_ptr = *c0.buffers.add(1) as *const i64;
        assert_eq!(*data_ptr, 10);
        assert_eq!(*data_ptr.add(1), 20);

        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_bool_column() {
    use crate::column::BoolColumn;

    let schema = Schema::new(vec![Field {
        name: "b".into(),
        ch_type: ChType::Bool,
    }]);
    let columns = vec![Column::Bool(BoolColumn::from_wire_bytes(&[1, 0, 1]))];
    let batch = Arc::new(ColBatch::new(schema, columns, 3));

    unsafe {
        let mut schema: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema);

        let c0 = &**schema.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "b");

        (schema.release.unwrap())(&mut schema);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);

        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 3);
        assert_eq!(c0.n_buffers, 2);

        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_fixed_binary_column() {
    use crate::column::FixedBinaryColumn;

    let schema = Schema::new(vec![Field {
        name: "fs".into(),
        ch_type: ChType::FixedString(4),
    }]);
    let columns = vec![Column::FixedBinary(FixedBinaryColumn::new(
        b"abcdwxyz".to_vec(),
        4,
    ))];
    let batch = Arc::new(ColBatch::new(schema, columns, 2));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);

        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "w:4");

        (schema_out.release.unwrap())(&mut schema_out);
    }
}

fn make_dictionary_batch(nullable: bool) -> Arc<ColBatch> {
    use crate::bitmap::Bitmap;
    use crate::column::DictionaryColumn;

    let inner = if nullable {
        ChType::Nullable(Box::new(ChType::String))
    } else {
        ChType::String
    };
    let schema = Schema::new(vec![Field {
        name: "lc".into(),
        ch_type: ChType::LowCardinality(Box::new(inner)),
    }]);
    // values dictionary: 3 entries, indices over 4 rows.
    let values = Column::Utf8(Utf8Column::new(
        vec![0, 5, 11, 17],
        b"user_user_1user_2".to_vec(),
    ));
    let dict = if nullable {
        // Row 1 is null (validity bit 0); the rest are valid.
        let validity = Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00, 0x00]);
        DictionaryColumn::new_nullable(vec![0, 0, 1, 2], values, validity)
    } else {
        DictionaryColumn::new(vec![0, 1, 2, 0], values)
    };
    let columns = vec![Column::Dictionary(dict)];
    Arc::new(ColBatch::new(schema, columns, 4))
}

#[test]
fn test_export_low_cardinality_schema() {
    // Dictionary schema: the field format is the index type (`i`), the
    // dictionary child carries the value type (`u`), and the nullable flag
    // tracks the inner Nullable.
    let batch = make_dictionary_batch(true);
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);

        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "i");
        assert_eq!(c0.flags & 2, 2, "nullable flag set for LC(Nullable(...))");
        assert!(!c0.dictionary.is_null(), "dictionary child present");
        let dict = &*c0.dictionary;
        assert_eq!(CStr::from_ptr(dict.format).to_str().unwrap(), "u");

        (schema_out.release.unwrap())(&mut schema_out);
    }

    // A non-nullable LowCardinality has the flag clear.
    let batch = make_dictionary_batch(false);
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(c0.flags & 2, 0, "nullable flag clear for LC(String)");
        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_low_cardinality_array() {
    // Dictionary array: 2 index buffers (validity + i32 indices), a length
    // equal to the row count, and a dictionary child holding the values.
    let batch = make_dictionary_batch(true);
    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);

        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 4);
        assert_eq!(c0.null_count, 1);
        assert_eq!(c0.n_buffers, 2);
        assert!(!(*c0.buffers.add(0)).is_null(), "validity buffer present");
        let idx = *c0.buffers.add(1) as *const i32;
        assert_eq!(*idx, 0);
        assert_eq!(*idx.add(2), 1);

        assert!(!c0.dictionary.is_null(), "dictionary child array present");
        let dict = &*c0.dictionary;
        assert_eq!(dict.length, 3, "dictionary holds 3 entries");
        assert_eq!(dict.n_buffers, 3, "utf8 values: validity, offsets, data");

        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_arrow_format_low_cardinality() {
    assert_eq!(
        arrow_format(&ChType::LowCardinality(Box::new(ChType::String))),
        "i"
    );
    assert_eq!(
        arrow_format(&ChType::LowCardinality(Box::new(ChType::Nullable(
            Box::new(ChType::String)
        )))),
        "i"
    );
}

#[test]
fn test_export_low_cardinality_saf_nullable_string_schema() {
    use crate::column::DictionaryColumn;

    // LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String))) must
    // export exactly like LowCardinality(Nullable(String)): field format `i`,
    // the index-level nullable flag set (nulls live in the index validity),
    // and a non-nullable `u` (String) dictionary child. The SAF chain between
    // the LC and its removeNullable Nullable is resolved through the shared
    // `low_cardinality_dict_value_type` helper.
    let schema = Schema::new(vec![Field {
        name: "lc_nsaf".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        })),
    }]);
    let values = Column::Utf8(crate::column::Utf8Column::new(vec![0], vec![]));
    let dict = DictionaryColumn::new_nullable(
        vec![],
        values,
        crate::bitmap::Bitmap::from_ch_null_map(&[]),
    );
    let batch = Arc::new(ColBatch::new(schema, vec![Column::Dictionary(dict)], 0));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "i");
        assert_eq!(
            c0.flags & 2,
            2,
            "nullable flag set for LC(SAF(_, Nullable(String)))"
        );
        assert!(!c0.dictionary.is_null(), "dictionary child present");
        let dict_schema = &*c0.dictionary;
        assert_eq!(CStr::from_ptr(dict_schema.format).to_str().unwrap(), "u");
        assert_eq!(
            dict_schema.flags & 2,
            0,
            "dictionary values are non-nullable; nulls live in the index validity"
        );
        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_low_cardinality_uint32() {
    use crate::column::DictionaryColumn;

    // A non-String dictionary value type still exports as dictionary(i32, T):
    // the field format is the index type `i`, and the dictionary child format
    // is the value type, here `I` (uint32). The dictionary export path is
    // generic over the value column, so a UInt32 values column flows through
    // unchanged. This mirrors the String case but pins the child format for a
    // primitive inner.
    let schema = Schema::new(vec![Field {
        name: "lc".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::UInt32)),
    }]);
    // Dictionary slot 0 is the reserved default; the 3 rows index into 1..=3.
    let values = Column::UInt32(PrimitiveColumn::new(vec![0, 13, 79, 4_294_967_295]));
    let dict = DictionaryColumn::new(vec![1, 2, 3], values);
    let batch = Arc::new(ColBatch::new(schema, vec![Column::Dictionary(dict)], 3));

    unsafe {
        // Schema: field format `i`, flag clear (non-nullable), child `I`.
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "i");
        assert_eq!(c0.flags & 2, 0, "non-nullable LC has the flag clear");
        assert!(!c0.dictionary.is_null(), "dictionary child present");
        let dict_schema = &*c0.dictionary;
        assert_eq!(CStr::from_ptr(dict_schema.format).to_str().unwrap(), "I");
        (schema_out.release.unwrap())(&mut schema_out);

        // Array: 2 index buffers (validity, i32 indices), dictionary child
        // holding 4 uint32 entries with 2 buffers (validity, values).
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 3);
        assert_eq!(c0.n_buffers, 2);
        let idx = *c0.buffers.add(1) as *const i32;
        assert_eq!(*idx, 1);
        assert_eq!(*idx.add(2), 3);
        assert!(!c0.dictionary.is_null(), "dictionary child array present");
        let dict_array = &*c0.dictionary;
        assert_eq!(dict_array.length, 4, "dictionary holds 4 entries");
        assert_eq!(dict_array.n_buffers, 2, "uint32 values: validity, values");
        let vals = *dict_array.buffers.add(1) as *const u32;
        assert_eq!(*vals.add(1), 13);
        assert_eq!(*vals.add(3), 4_294_967_295);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_arrow_format_uuid_ipv4_ipv6() {
    // IPv4 exports as Arrow uint32 (`I`), zero-copy. UUID and IPv6 export as
    // Arrow fixed-size binary of width 16 (`w:16`); the crate emits plain
    // `w:16`, not the `arrow.uuid` extension.
    assert_eq!(arrow_format(&ChType::Ipv4), "I");
    assert_eq!(arrow_format(&ChType::Uuid), "w:16");
    assert_eq!(arrow_format(&ChType::Ipv6), "w:16");
    // LowCardinality(UUID) exports as dictionary(i32, w:16): the field format
    // is the index type `i` and the dictionary child carries `w:16`.
    assert_eq!(
        arrow_format(&ChType::LowCardinality(Box::new(ChType::Uuid))),
        "i"
    );
}

#[test]
fn test_export_uuid_ipv4_ipv6_schema_and_buffers() {
    use crate::column::FixedBinaryColumn;

    let schema = Schema::new(vec![
        Field {
            name: "ip4".into(),
            ch_type: ChType::Ipv4,
        },
        Field {
            name: "u".into(),
            ch_type: ChType::Uuid,
        },
        Field {
            name: "ip6".into(),
            ch_type: ChType::Ipv6,
        },
    ]);
    let uuid_bytes = vec![0xaau8; 32]; // 2 rows of width 16
    let ip6_bytes = vec![0xbbu8; 32];
    let columns = vec![
        Column::Ipv4(PrimitiveColumn::new(vec![3221226219u32, 0])),
        Column::Uuid(FixedBinaryColumn::new(uuid_bytes, 16)),
        Column::Ipv6(FixedBinaryColumn::new(ip6_bytes, 16)),
    ];
    let batch = Arc::new(ColBatch::new(schema, columns, 2));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);

        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "I");
        let c1 = &**schema_out.children.add(1);
        assert_eq!(CStr::from_ptr(c1.format).to_str().unwrap(), "w:16");
        let c2 = &**schema_out.children.add(2);
        assert_eq!(CStr::from_ptr(c2.format).to_str().unwrap(), "w:16");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);

        // IPv4: 2 buffers (validity, u32 values).
        let a0 = &**array.children.add(0);
        assert_eq!(a0.length, 2);
        assert_eq!(a0.n_buffers, 2);
        let ip4 = *a0.buffers.add(1) as *const u32;
        assert_eq!(*ip4, 3221226219);

        // UUID and IPv6: 2 buffers (validity, data), like FixedString.
        let a1 = &**array.children.add(1);
        assert_eq!(a1.length, 2);
        assert_eq!(a1.n_buffers, 2);
        let a2 = &**array.children.add(2);
        assert_eq!(a2.length, 2);
        assert_eq!(a2.n_buffers, 2);

        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_low_cardinality_uuid_child_format() {
    use crate::column::{DictionaryColumn, FixedBinaryColumn};

    // LowCardinality(UUID) exports as dictionary(i32, w:16): field format `i`,
    // dictionary child format `w:16`.
    let schema = Schema::new(vec![Field {
        name: "lc".into(),
        ch_type: ChType::LowCardinality(Box::new(ChType::Uuid)),
    }]);
    let values = Column::Uuid(FixedBinaryColumn::new(vec![0u8; 48], 16)); // 3 entries
    let dict = DictionaryColumn::new(vec![1, 2, 1], values);
    let batch = Arc::new(ColBatch::new(schema, vec![Column::Dictionary(dict)], 3));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "i");
        assert!(!c0.dictionary.is_null(), "dictionary child present");
        let dict_schema = &*c0.dictionary;
        assert_eq!(CStr::from_ptr(dict_schema.format).to_str().unwrap(), "w:16");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let c0 = &**array.children.add(0);
        assert!(!c0.dictionary.is_null(), "dictionary child array present");
        let dict_array = &*c0.dictionary;
        assert_eq!(dict_array.length, 3, "dictionary holds 3 entries");
        assert_eq!(dict_array.n_buffers, 2, "fixed binary: validity, data");
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_stream_yields_one_batch_then_ends() {
    let batch = make_test_batch();
    let schema = batch.schema.clone();
    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![batch], &mut stream);

        let mut schema: ArrowSchema = std::mem::zeroed();
        let rc = (stream.get_schema.unwrap())(&mut stream, &mut schema);
        assert_eq!(rc, 0);
        (schema.release.unwrap())(&mut schema);

        let mut array: ArrowArray = std::mem::zeroed();
        let rc = (stream.get_next.unwrap())(&mut stream, &mut array);
        assert_eq!(rc, 0);
        assert!(array.release.is_some());
        assert_eq!(array.length, 2);
        (array.release.unwrap())(&mut array);

        let mut array2: ArrowArray = std::mem::zeroed();
        let rc = (stream.get_next.unwrap())(&mut stream, &mut array2);
        assert_eq!(rc, 0);
        assert!(array2.release.is_none());

        (stream.release.unwrap())(&mut stream);
    }
}

#[test]
fn test_arrow_format_and_export_enum() {
    // Enum8 exports as Arrow int8 (`c`), Enum16 as int16 (`s`): the
    // underlying signed int buffer, zero-copy, like Int8/Int16. No
    // dictionary child (ClickHouse enum values are arbitrary signed ints,
    // not 0..N-1 indices).
    let e8 = ChType::Enum8 {
        variants: vec![("pending".to_string(), 1), ("closed".to_string(), -1)],
    };
    let e16 = ChType::Enum16 {
        variants: vec![("a".to_string(), 1)],
    };
    assert_eq!(arrow_format(&e8), "c");
    assert_eq!(arrow_format(&e16), "s");

    let schema = Schema::new(vec![
        Field {
            name: "e8".into(),
            ch_type: e8,
        },
        Field {
            name: "e16".into(),
            ch_type: e16,
        },
    ]);
    let columns = vec![
        Column::Enum8(PrimitiveColumn::new(vec![1i8, -1, 1])),
        Column::Enum16(PrimitiveColumn::new(vec![1i16, 1, 1])),
    ];
    let batch = Arc::new(ColBatch::new(schema, columns, 3));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "c");
        assert!(c0.dictionary.is_null(), "enum has no dictionary child");
        let c1 = &**schema_out.children.add(1);
        assert_eq!(CStr::from_ptr(c1.format).to_str().unwrap(), "s");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let a0 = &**array.children.add(0);
        assert_eq!(a0.length, 3);
        assert_eq!(a0.n_buffers, 2);
        assert!((*a0.buffers.add(0)).is_null(), "non-nullable validity null");
        let vals = *a0.buffers.add(1) as *const i8;
        assert_eq!(*vals, 1);
        assert_eq!(*vals.add(1), -1);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_arrow_format_and_export_decimal() {
    use crate::column::DecimalColumn;

    // Decimal exports with the Arrow decimal format string. The 128-bit case
    // is bare `d:P,S`; the other widths carry the bit width as the third
    // field (`d:P,S,bits`). The buffer is the native-width contiguous
    // little-endian data, zero-copy, never widened to 128.
    assert_eq!(
        arrow_format(&ChType::Decimal {
            precision: 9,
            scale: 4,
            bits: 32,
        }),
        "d:9,4,32"
    );
    assert_eq!(
        arrow_format(&ChType::Decimal {
            precision: 18,
            scale: 9,
            bits: 64,
        }),
        "d:18,9,64"
    );
    assert_eq!(
        arrow_format(&ChType::Decimal {
            precision: 38,
            scale: 10,
            bits: 128,
        }),
        "d:38,10"
    );
    assert_eq!(
        arrow_format(&ChType::Decimal {
            precision: 50,
            scale: 10,
            bits: 256,
        }),
        "d:50,10,256"
    );

    // Export a Decimal128 column (2 rows): schema format `d:20,2`, array with
    // 2 buffers (validity null for non-nullable, then the 32-byte data).
    let schema = Schema::new(vec![Field {
        name: "d".into(),
        ch_type: ChType::Decimal {
            precision: 20,
            scale: 2,
            bits: 128,
        },
    }]);
    let mut data = vec![0u8; 32]; // 2 rows of width 16
    data[0] = 0x01; // row 0 unscaled = 1
    let columns = vec![Column::Decimal(DecimalColumn::new(data, 16, 20, 2))];
    let batch = Arc::new(ColBatch::new(schema, columns, 2));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "d:20,2");
        assert!(c0.dictionary.is_null(), "decimal has no dictionary child");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let a0 = &**array.children.add(0);
        assert_eq!(a0.length, 2);
        assert_eq!(a0.n_buffers, 2);
        assert!((*a0.buffers.add(0)).is_null(), "non-nullable validity null");
        let bytes = *a0.buffers.add(1) as *const u8;
        assert_eq!(*bytes, 0x01, "row 0 first byte is the unscaled 1");
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_arrow_format_temporal() {
    // Zero-copy temporal export: Date32 and the {0,3,6,9}-precision
    // DateTime64 map to real Arrow temporal formats; Date, DateTime, and
    // other-precision DateTime64 expose the raw integer.
    assert_eq!(arrow_format(&ChType::Date), "S");
    assert_eq!(arrow_format(&ChType::Date32), "tdD");
    assert_eq!(arrow_format(&ChType::DateTime { timezone: None }), "I");
    assert_eq!(
        arrow_format(&ChType::DateTime64 {
            precision: 3,
            timezone: Some("UTC".to_string())
        }),
        "tsm:UTC"
    );
    assert_eq!(
        arrow_format(&ChType::DateTime64 {
            precision: 2,
            timezone: None
        }),
        "l"
    );
    // ClickHouse times can be negative or exceed 24 hours, so every
    // precision exports as the raw signed integer rather than Arrow Time.
    assert_eq!(arrow_format(&ChType::Time), "i");
    for precision in 0..=9 {
        assert_eq!(arrow_format(&ChType::Time64 { precision }), "l");
    }
}

#[test]
fn test_export_time_buffers() {
    let schema = Schema::new(vec![
        Field {
            name: "t".into(),
            ch_type: ChType::Time,
        },
        Field {
            name: "t64".into(),
            ch_type: ChType::Time64 { precision: 3 },
        },
    ]);
    let batch = Arc::new(ColBatch::new(
        schema,
        vec![
            Column::Time(PrimitiveColumn::new(vec![-3_599_999, 0, 3_599_999])),
            Column::Time64(PrimitiveColumn::new(vec![-3_599_999_999, 0, 3_599_999_999])),
        ],
        3,
    ));

    // Safety: the zeroed FFI outputs are writable and the batch remains
    // alive until each matching release callback is invoked below.
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let t_schema = &**schema_out.children.add(0);
        let t64_schema = &**schema_out.children.add(1);
        assert_eq!(CStr::from_ptr(t_schema.format).to_str().unwrap(), "i");
        assert_eq!(CStr::from_ptr(t64_schema.format).to_str().unwrap(), "l");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let t = &**array.children.add(0);
        let t64 = &**array.children.add(1);
        assert_eq!(t.length, 3);
        assert_eq!(t.n_buffers, 2);
        assert!((*t.buffers.add(0)).is_null());
        assert_eq!(*(*t.buffers.add(1) as *const i32), -3_599_999);
        assert_eq!(t64.length, 3);
        assert_eq!(t64.n_buffers, 2);
        assert!((*t64.buffers.add(0)).is_null());
        assert_eq!(*(*t64.buffers.add(1) as *const i64), -3_599_999_999);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_nullable_time_buffers_zero_copy() {
    let t_validity = Bitmap::from_ch_null_map(&[0, 1, 0]);
    let t64_validity = Bitmap::from_ch_null_map(&[0, 1, 0]);
    let t_values = vec![-13i32, 0, 79];
    let t64_values = vec![-13_000_000i64, 0, 79_000_000];
    let t_validity_ptr = t_validity.as_bytes().as_ptr() as *const c_void;
    let t64_validity_ptr = t64_validity.as_bytes().as_ptr() as *const c_void;
    let t_values_ptr = t_values.as_ptr() as *const c_void;
    let t64_values_ptr = t64_values.as_ptr() as *const c_void;

    let schema = Schema::new(vec![
        Field {
            name: "nt".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Time)),
        },
        Field {
            name: "nt64".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Time64 { precision: 6 })),
        },
    ]);
    let batch = Arc::new(ColBatch::new(
        schema,
        vec![
            Column::Time(PrimitiveColumn::new_nullable(t_values, t_validity)),
            Column::Time64(PrimitiveColumn::new_nullable(t64_values, t64_validity)),
        ],
        3,
    ));

    // Safety: the zeroed FFI outputs are writable and the batch remains
    // alive until each matching release callback is invoked below.
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        assert_eq!((**schema_out.children.add(0)).flags, 2);
        assert_eq!((**schema_out.children.add(1)).flags, 2);
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let t = &**array.children.add(0);
        let t64 = &**array.children.add(1);
        assert_eq!(t.null_count, 1);
        assert_eq!(*t.buffers.add(0), t_validity_ptr);
        assert_eq!(*t.buffers.add(1), t_values_ptr);
        assert_eq!(t64.null_count, 1);
        assert_eq!(*t64.buffers.add(0), t64_validity_ptr);
        assert_eq!(*t64.buffers.add(1), t64_values_ptr);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_schema_tolerates_nul_in_wire_names() {
    // A column name and a timezone both come from the untrusted wire stream
    // and are only UTF-8 validated, so either can carry an interior NUL.
    // Exporting must not panic (a panic could unwind across the extern "C"
    // stream callbacks). The NUL bytes are stripped from the C strings.
    let schema = Schema::new(vec![
        Field {
            name: "a\0b".into(),
            ch_type: ChType::Int32,
        },
        Field {
            name: "ts".into(),
            ch_type: ChType::DateTime64 {
                precision: 3,
                timezone: Some("U\0TC".into()),
            },
        },
    ]);
    unsafe {
        let mut out: ArrowSchema = std::mem::zeroed();
        export_schema(&schema, &mut out);

        let c0 = &**out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.name).to_str().unwrap(), "ab");

        let c1 = &**out.children.add(1);
        assert_eq!(CStr::from_ptr(c1.format).to_str().unwrap(), "tsm:UTC");

        (out.release.unwrap())(&mut out);
    }
}

#[test]
fn test_stream_release_if_set_is_idempotent() {
    let batch = make_test_batch();
    let schema = batch.schema.clone();
    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![batch], &mut stream);

        stream.release_if_set();
        assert!(stream.release.is_none());
        assert!(stream.private_data.is_null());

        // Second call sees a cleared callback and does nothing.
        stream.release_if_set();
    }
}

#[test]
fn test_stream_yields_multiple_chunks() {
    // Three separate chunks must come out as three record batches.
    let schema = make_test_batch().schema.clone();
    let chunks = vec![make_test_batch(), make_test_batch(), make_test_batch()];
    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, chunks, &mut stream);

        let mut count = 0;
        loop {
            let mut array: ArrowArray = std::mem::zeroed();
            let rc = (stream.get_next.unwrap())(&mut stream, &mut array);
            assert_eq!(rc, 0);
            if array.release.is_none() {
                break;
            }
            assert_eq!(array.length, 2);
            count += 1;
            (array.release.unwrap())(&mut array);
        }
        assert_eq!(count, 3);

        (stream.release.unwrap())(&mut stream);
    }
}

#[test]
fn test_arrow_format_array() {
    // Array(T) is an Arrow LargeList: `+L`. The element type is NOT in this
    // format string, it lives in the `item` child schema.
    assert_eq!(arrow_format(&ChType::Array(Box::new(ChType::Int32))), "+L");
    // Nesting does not change the top-level format string.
    assert_eq!(
        arrow_format(&ChType::Array(Box::new(ChType::Array(Box::new(
            ChType::Int32
        ))))),
        "+L"
    );
}

#[test]
fn test_export_array_int32_schema() {
    // Schema of Array(Int32): field format `+L`, flags clear (array level is
    // never nullable), one child named `item` with the element format `i`.
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    }]);

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&schema, &mut schema_out);

        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
        assert_eq!(c0.flags & 2, 0, "array level is not nullable");
        assert_eq!(c0.n_children, 1);
        assert!(c0.dictionary.is_null(), "array field has no dictionary");

        let item = &**c0.children.add(0);
        assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "i");
        assert_eq!(CStr::from_ptr(item.name).to_str().unwrap(), "item");
        assert_eq!(item.flags & 2, 0, "plain Int32 element is not nullable");

        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_array_int32_buffers() {
    // Array(Int32) over 3 rows including an empty row (row 1). LargeList:
    // 2 buffers (null validity, i64 offsets), 1 child holding the flattened
    // elements.
    // Rows: [[10,20,30], [], [40]] -> offsets [0,3,3,4], values [10,20,30,40].
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    }]);
    let values = Column::Int32(PrimitiveColumn::new(vec![10, 20, 30, 40]));
    let col = Column::Array(crate::column::ArrayColumn::new(vec![0, 3, 3, 4], values));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 3));

    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);

        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 3, "array length is the row count");
        assert_eq!(c0.null_count, 0);
        assert_eq!(c0.n_buffers, 2, "LargeList: validity + offsets");
        assert!(
            (*c0.buffers.add(0)).is_null(),
            "array level validity is always null"
        );
        let offsets = *c0.buffers.add(1) as *const i64;
        assert_eq!(*offsets, 0, "leading 0");
        assert_eq!(*offsets.add(1), 3);
        assert_eq!(*offsets.add(2), 3, "empty row -> repeated offset");
        assert_eq!(*offsets.add(3), 4, "last offset == total elements");

        assert_eq!(c0.n_children, 1);
        let item = &**c0.children.add(0);
        assert_eq!(item.length, 4, "element count == last offset");
        assert_eq!(item.n_buffers, 2, "int32 element: validity + values");
        let vals = *item.buffers.add(1) as *const i32;
        assert_eq!(*vals, 10);
        assert_eq!(*vals.add(3), 40);

        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_array_nullable_int32() {
    use crate::bitmap::Bitmap;

    // Array(Nullable(Int32)): the `item` child schema carries the nullable
    // flag, the element array's validity is non-null with a matching
    // null_count, and the array-level validity stays null.
    // Rows: [[10, null], [30]] -> offsets [0,2,3], values [10, _, 30] with
    // element index 1 null.
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::Int32)))),
    }]);
    // ClickHouse null map: 1 = null. Element index 1 is null.
    let validity = Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]);
    let values = Column::Int32(PrimitiveColumn::new_nullable(vec![10, 0, 30], validity));
    let col = Column::Array(crate::column::ArrayColumn::new(vec![0, 2, 3], values));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 2));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
        assert_eq!(c0.flags & 2, 0, "array level not nullable");
        let item = &**c0.children.add(0);
        assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "i");
        assert_eq!(item.flags & 2, 2, "nullable element flag set");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 2);
        assert_eq!(c0.null_count, 0, "array level has no nulls");
        assert!(
            (*c0.buffers.add(0)).is_null(),
            "array level validity stays null"
        );
        let item = &**c0.children.add(0);
        assert_eq!(item.length, 3);
        assert_eq!(item.null_count, 1, "one null element");
        assert!(
            !(*item.buffers.add(0)).is_null(),
            "element validity buffer present"
        );
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_array_low_cardinality_string() {
    use crate::column::DictionaryColumn;

    // Array(LowCardinality(String)): the `item` child schema is a dictionary
    // (format `i`) with a non-null dictionary child of format `u`; the
    // element array carries the index buffers plus a dictionary child.
    // Rows: [["user_1"], ["user_2","user_1"]] -> offsets [0,1,3].
    // Dictionary values ["user_1","user_2"], indices [0,1,0].
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
    }]);
    let dict_values = Column::Utf8(Utf8Column::new(vec![0, 6, 12], b"user_1user_2".to_vec()));
    let dict = DictionaryColumn::new(vec![0, 1, 0], dict_values);
    let col = Column::Array(crate::column::ArrayColumn::new(
        vec![0, 1, 3],
        Column::Dictionary(dict),
    ));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 2));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
        let item = &**c0.children.add(0);
        assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "i");
        assert!(!item.dictionary.is_null(), "dictionary child present");
        let dict_schema = &*item.dictionary;
        assert_eq!(CStr::from_ptr(dict_schema.format).to_str().unwrap(), "u");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 2);
        let item = &**c0.children.add(0);
        assert_eq!(item.length, 3, "3 flattened index rows");
        assert_eq!(item.n_buffers, 2, "dictionary: validity + i32 indices");
        let idx = *item.buffers.add(1) as *const i32;
        assert_eq!(*idx, 0);
        assert_eq!(*idx.add(1), 1);
        assert!(!item.dictionary.is_null(), "dictionary child array present");
        let dict_array = &*item.dictionary;
        assert_eq!(dict_array.length, 2, "2 dictionary entries");
        assert_eq!(
            dict_array.n_buffers, 3,
            "utf8 values: validity, offsets, data"
        );
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_array_of_array_int32() {
    // Array(Array(Int32)): the `item` child schema is itself `+L` with its own
    // `item` child of format `i`; nested child arrays have the right lengths.
    // Outer rows: [[[1,2],[3]], [[4]]] -> outer offsets [0,2,3].
    // Inner arrays (3 of them): offsets [0,2,3,4], values [1,2,3,4].
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
    }]);
    let inner_values = Column::Int32(PrimitiveColumn::new(vec![1, 2, 3, 4]));
    let inner = Column::Array(crate::column::ArrayColumn::new(
        vec![0, 2, 3, 4],
        inner_values,
    ));
    let outer = Column::Array(crate::column::ArrayColumn::new(vec![0, 2, 3], inner));
    let batch = Arc::new(ColBatch::new(schema, vec![outer], 2));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
        let item = &**c0.children.add(0);
        assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "+L");
        assert_eq!(item.n_children, 1);
        let leaf = &**item.children.add(0);
        assert_eq!(CStr::from_ptr(leaf.format).to_str().unwrap(), "i");
        assert_eq!(CStr::from_ptr(leaf.name).to_str().unwrap(), "item");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 2, "2 outer rows");
        let offsets = *c0.buffers.add(1) as *const i64;
        assert_eq!(*offsets, 0);
        assert_eq!(*offsets.add(2), 3, "3 inner arrays total");

        let item = &**c0.children.add(0);
        assert_eq!(item.length, 3, "3 inner arrays == outer last offset");
        let inner_offsets = *item.buffers.add(1) as *const i64;
        assert_eq!(*inner_offsets.add(3), 4, "4 leaf elements total");

        let leaf = &**item.children.add(0);
        assert_eq!(leaf.length, 4, "4 leaf int32 elements");
        let vals = *leaf.buffers.add(1) as *const i32;
        assert_eq!(*vals, 1);
        assert_eq!(*vals.add(3), 4);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_empty_array_column() {
    // A zero-row Array column has offsets == [0]: length 0, offsets buffer
    // still non-null with a single leading 0, and an empty element child.
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    }]);
    let values = Column::Int32(PrimitiveColumn::new(vec![]));
    let col = Column::Array(crate::column::ArrayColumn::new(vec![0], values));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 0));

    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 0);
        assert_eq!(c0.n_buffers, 2);
        assert!((*c0.buffers.add(0)).is_null());
        let offsets = *c0.buffers.add(1) as *const i64;
        assert_eq!(*offsets, 0, "the leading 0 for a zero-row column");
        let item = &**c0.children.add(0);
        assert_eq!(item.length, 0, "no elements");
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_release_array_skips_moved_out_child() {
    // Spec "Moving child arrays": a consumer may take ownership of a child
    // by bitwise-copying its struct and marking the SOURCE released
    // (release = None) WITHOUT calling the source's release callback, then
    // must release the parent. The parent's release must skip the moved
    // child while still freeing the producer-owned child shell, and the
    // moved copy must stay independently valid (it holds its own
    // Arc<ColBatch> in private data) until its own, idempotent release.
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Int32)),
    }]);
    let values = Column::Int32(PrimitiveColumn::new(vec![10, 20, 30, 40]));
    let col = Column::Array(crate::column::ArrayColumn::new(vec![0, 3, 3, 4], values));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 3));

    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        // Drop our own Arc so the export's private-data clones are the only
        // owners of the buffers from here on.
        drop(batch);

        let list_ptr = *array.children.add(0);
        let item_ptr = *(*list_ptr).children.add(0);

        // Consumer move: bitwise copy the item child out of the producer's
        // shell, then mark the source released without calling its
        // callback.
        let mut moved: ArrowArray = ptr::read(item_ptr);
        (*item_ptr).release = None;

        // Per spec the parent must be released after moving a child out.
        // Its release must skip the moved-out item (null release), freeing
        // only the producer-owned shell; the moved copy stays valid.
        (array.release.unwrap())(&mut array);
        assert!(array.release.is_none(), "parent marked released");

        // The moved child still owns its buffers through its own private
        // data: length and values remain readable after the parent (and
        // our Arc) are gone.
        assert_eq!(moved.length, 4);
        assert_eq!(moved.n_buffers, 2);
        let vals = *moved.buffers.add(1) as *const i32;
        assert_eq!(*vals, 10);
        assert_eq!(*vals.add(3), 40);

        // Releasing the moved copy frees its private data exactly once and
        // marks it released; a second call through the producer callback is
        // a no-op (private_data was cleared).
        (moved.release.unwrap())(&mut moved);
        assert!(moved.release.is_none(), "moved child marked released");
        release_array(&mut moved);
    }
}

#[test]
fn test_stream_with_array_column() {
    // A stream whose batch carries an Array(Array(Int32)) column: the first
    // exported type with real `children`, so `get_schema` and `get_next`
    // must agree on the child shape all the way down.
    // One row: [[[7], [9, 11]]] -> outer offsets [0, 2], inner offsets
    // [0, 1, 3], leaf values [7, 9, 11].
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
    }]);
    let leaf = Column::Int32(PrimitiveColumn::new(vec![7, 9, 11]));
    let inner = Column::Array(crate::column::ArrayColumn::new(vec![0, 1, 3], leaf));
    let outer = Column::Array(crate::column::ArrayColumn::new(vec![0, 2], inner));
    let batch = Arc::new(ColBatch::new(schema.clone(), vec![outer], 1));

    unsafe {
        let mut stream: ArrowArrayStream = std::mem::zeroed();
        export_chunks_to_stream(schema, vec![batch], &mut stream);

        let mut schema_out: ArrowSchema = std::mem::zeroed();
        let rc = (stream.get_schema.unwrap())(&mut stream, &mut schema_out);
        assert_eq!(rc, 0);
        let s0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(s0.format).to_str().unwrap(), "+L");
        assert_eq!(s0.n_children, 1);
        let s_item = &**s0.children.add(0);
        assert_eq!(CStr::from_ptr(s_item.format).to_str().unwrap(), "+L");
        assert_eq!(s_item.n_children, 1);
        let s_leaf = &**s_item.children.add(0);
        assert_eq!(CStr::from_ptr(s_leaf.format).to_str().unwrap(), "i");

        let mut array: ArrowArray = std::mem::zeroed();
        let rc = (stream.get_next.unwrap())(&mut stream, &mut array);
        assert_eq!(rc, 0);
        assert!(array.release.is_some());
        assert_eq!(array.n_children, schema_out.n_children);
        let a0 = &**array.children.add(0);
        assert_eq!(a0.length, 1, "1 outer row");
        assert_eq!(a0.n_children, s0.n_children);
        let a_item = &**a0.children.add(0);
        assert_eq!(a_item.length, 2, "outer last offset");
        assert_eq!(a_item.n_children, s_item.n_children);
        let a_leaf = &**a_item.children.add(0);
        assert_eq!(a_leaf.length, 3, "inner last offset");
        let vals = *a_leaf.buffers.add(1) as *const i32;
        assert_eq!(*vals, 7);
        assert_eq!(*vals.add(2), 11);
        (array.release.unwrap())(&mut array);
        (schema_out.release.unwrap())(&mut schema_out);

        // End of stream after the single chunk.
        let mut array2: ArrowArray = std::mem::zeroed();
        let rc = (stream.get_next.unwrap())(&mut stream, &mut array2);
        assert_eq!(rc, 0);
        assert!(array2.release.is_none());

        (stream.release.unwrap())(&mut stream);
    }
}

#[test]
fn test_export_array_all_rows_empty_low_cardinality() {
    use crate::column::DictionaryColumn;

    // Rows but every array empty: offsets [0, 0, 0] over an empty element
    // column. This is the shape the decoder produces for the documented
    // wire quirk where a rows-but-all-empty `Array(LowCardinality(String))`
    // column carries no element body at all (see CODEC_CONTRACT.md). The
    // offsets buffer must still be non-null with num_rows + 1 entries, and
    // the item child (and its dictionary) must export with length 0.
    let schema = Schema::new(vec![Field {
        name: "arr".into(),
        ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
    }]);
    let dict_values = Column::Utf8(Utf8Column::new(vec![0], Vec::new()));
    let dict = DictionaryColumn::new(Vec::new(), dict_values);
    let col = Column::Array(crate::column::ArrayColumn::new(
        vec![0, 0, 0],
        Column::Dictionary(dict),
    ));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 2));

    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 2, "2 rows, all empty");
        assert_eq!(c0.null_count, 0);
        assert_eq!(c0.n_buffers, 2);
        assert!((*c0.buffers.add(0)).is_null());
        let offsets_ptr = *c0.buffers.add(1);
        assert!(!offsets_ptr.is_null(), "offsets buffer stays non-null");
        let offsets = offsets_ptr as *const i64;
        assert_eq!(*offsets, 0);
        assert_eq!(*offsets.add(1), 0);
        assert_eq!(*offsets.add(2), 0, "num_rows + 1 all-zero offsets");

        assert_eq!(c0.n_children, 1);
        let item = &**c0.children.add(0);
        assert_eq!(item.length, 0, "no flattened elements");
        assert_eq!(item.null_count, 0);
        assert!(!item.dictionary.is_null(), "dictionary child still present");
        let dict_array = &*item.dictionary;
        assert_eq!(dict_array.length, 0, "empty dictionary");
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_arrow_format_tuple() {
    // Tuple exports as an Arrow struct (`+s`); the element types live in
    // the child schemas, never in the format string. Nullable(Tuple)
    // recurses to the same format.
    assert_eq!(
        arrow_format(&ChType::Tuple(vec![
            (None, ChType::Int32),
            (None, ChType::String),
        ])),
        "+s"
    );
    assert_eq!(arrow_format(&ChType::Tuple(vec![])), "+s");
    assert_eq!(
        arrow_format(&ChType::Nullable(Box::new(ChType::Tuple(vec![(
            None,
            ChType::Int32,
        )])))),
        "+s"
    );
}

#[test]
fn test_export_tuple_schema() {
    // Schema of a named Tuple(a Int32, b Nullable(String)) and an unnamed
    // Tuple(Int32, String): format `+s`, one child per element. Named
    // elements keep their ClickHouse names verbatim; unnamed elements are
    // named by 1-based position. A Nullable element carries its own
    // nullable flag on the child.
    let schema = Schema::new(vec![
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
        Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        },
        Field {
            name: "t0".into(),
            ch_type: ChType::Tuple(vec![]),
        },
    ]);

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&schema, &mut schema_out);

        let tn = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(tn.format).to_str().unwrap(), "+s");
        assert_eq!(tn.flags & 2, 0, "plain tuple is not nullable");
        assert_eq!(tn.n_children, 2);
        assert!(tn.dictionary.is_null());
        let a = &**tn.children.add(0);
        assert_eq!(CStr::from_ptr(a.name).to_str().unwrap(), "a");
        assert_eq!(CStr::from_ptr(a.format).to_str().unwrap(), "i");
        let b = &**tn.children.add(1);
        assert_eq!(CStr::from_ptr(b.name).to_str().unwrap(), "b");
        assert_eq!(CStr::from_ptr(b.format).to_str().unwrap(), "u");
        assert_eq!(b.flags & 2, 2, "Nullable element child is nullable");

        let t = &**schema_out.children.add(1);
        assert_eq!(t.n_children, 2);
        let e1 = &**t.children.add(0);
        assert_eq!(CStr::from_ptr(e1.name).to_str().unwrap(), "1");
        let e2 = &**t.children.add(1);
        assert_eq!(CStr::from_ptr(e2.name).to_str().unwrap(), "2");

        let t0 = &**schema_out.children.add(2);
        assert_eq!(CStr::from_ptr(t0.format).to_str().unwrap(), "+s");
        assert_eq!(t0.n_children, 0, "Tuple() exports with no children");

        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_tuple_buffers() {
    use crate::column::TupleColumn;

    // Tuple(Int32, String) over 2 rows: struct node with 1 buffer (null
    // validity slot, null_count 0), 2 children exported recursively; and a
    // zero-element Tuple() whose node still carries its explicit length.
    let schema = Schema::new(vec![
        Field {
            name: "t".into(),
            ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
        },
        Field {
            name: "t0".into(),
            ch_type: ChType::Tuple(vec![]),
        },
    ]);
    let columns = vec![
        Column::Tuple(TupleColumn::new(
            vec![
                Column::Int32(PrimitiveColumn::new(vec![13, 79])),
                Column::Utf8(Utf8Column::new(vec![0, 2, 2], b"hi".to_vec())),
            ],
            2,
        )),
        Column::Tuple(TupleColumn::new(vec![], 2)),
    ];
    let batch = Arc::new(ColBatch::new(schema, columns, 2));

    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);

        let t = &**array.children.add(0);
        assert_eq!(t.length, 2);
        assert_eq!(t.null_count, 0);
        assert_eq!(t.n_buffers, 1, "struct: validity slot only");
        assert!((*t.buffers.add(0)).is_null(), "plain tuple validity null");
        assert_eq!(t.n_children, 2);
        assert!(t.dictionary.is_null());
        let e1 = &**t.children.add(0);
        assert_eq!(e1.length, 2);
        let vals = *e1.buffers.add(1) as *const i32;
        assert_eq!(*vals, 13);
        assert_eq!(*vals.add(1), 79);
        let e2 = &**t.children.add(1);
        assert_eq!(e2.length, 2);
        assert_eq!(e2.n_buffers, 3, "utf8 element: validity, offsets, data");

        let t0 = &**array.children.add(1);
        assert_eq!(t0.length, 2, "Tuple() length from the explicit len");
        assert_eq!(t0.n_buffers, 1);
        assert_eq!(t0.n_children, 0);

        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_nullable_tuple() {
    use crate::bitmap::Bitmap;
    use crate::column::TupleColumn;

    // Nullable(Tuple(Int32)): the struct node carries the nullable flag on
    // the schema and the validity bitmap in buffers[0] with a matching
    // null_count; children stay independent per the C Data spec.
    let schema = Schema::new(vec![Field {
        name: "nt".into(),
        ch_type: ChType::Nullable(Box::new(ChType::Tuple(vec![(None, ChType::Int32)]))),
    }]);
    let columns = vec![Column::Tuple(TupleColumn::new_nullable(
        vec![Column::Int32(PrimitiveColumn::new(vec![13, 0, 79]))],
        3,
        Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]),
    ))];
    let batch = Arc::new(ColBatch::new(schema, columns, 3));

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+s");
        assert_eq!(c0.flags & 2, 2, "Nullable(Tuple) sets the nullable flag");
        assert_eq!(c0.n_children, 1, "element children still described");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 3);
        assert_eq!(c0.null_count, 1);
        assert_eq!(c0.n_buffers, 1);
        assert!(
            !(*c0.buffers.add(0)).is_null(),
            "tuple-level validity bitmap present in buffers[0]"
        );
        let child = &**c0.children.add(0);
        assert_eq!(child.length, 3, "children carry placeholders for nulls");
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_arrow_format_map() {
    // Map exports as LargeList-of-struct (`+L`), never `+m` (whose i32
    // offsets would force a copy of the i64 offset buffer).
    assert_eq!(
        arrow_format(&ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Int32),
        )),
        "+L"
    );
}

#[test]
fn test_export_map_schema() {
    // Schema of Map(String, Nullable(Int32)): field format `+L`, flags 0
    // (maps are never nullable at the map level), one child named
    // "entries" (a non-nullable struct), with grandchildren "key" (flags 0)
    // and "value" (nullable flag per the value type). No
    // ARROW_FLAG_MAP_KEYS_SORTED anywhere (it is +m-only).
    let schema = Schema::new(vec![Field {
        name: "m".into(),
        ch_type: ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Nullable(Box::new(ChType::Int32))),
        ),
    }]);

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&schema, &mut schema_out);

        let c0 = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
        assert_eq!(c0.flags, 0, "map level is not nullable, no sorted-keys");
        assert_eq!(c0.n_children, 1);
        assert!(c0.dictionary.is_null());

        let entries = &**c0.children.add(0);
        assert_eq!(CStr::from_ptr(entries.name).to_str().unwrap(), "entries");
        assert_eq!(CStr::from_ptr(entries.format).to_str().unwrap(), "+s");
        assert_eq!(entries.flags, 0, "entries struct is non-nullable");
        assert_eq!(entries.n_children, 2);

        let key = &**entries.children.add(0);
        assert_eq!(CStr::from_ptr(key.name).to_str().unwrap(), "key");
        assert_eq!(CStr::from_ptr(key.format).to_str().unwrap(), "u");
        assert_eq!(key.flags & 2, 0, "keys are never nullable");
        let value = &**entries.children.add(1);
        assert_eq!(CStr::from_ptr(value.name).to_str().unwrap(), "value");
        assert_eq!(CStr::from_ptr(value.format).to_str().unwrap(), "i");
        assert_eq!(value.flags & 2, 2, "Nullable value child is nullable");

        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_map_lc_key_schema() {
    // Map(LowCardinality(String), UInt8): the key grandchild is a
    // dictionary field (index format `i`, values in the dictionary child),
    // composing through the entries struct exactly like a top-level LC.
    let schema = Schema::new(vec![Field {
        name: "m".into(),
        ch_type: ChType::Map(
            Box::new(ChType::LowCardinality(Box::new(ChType::String))),
            Box::new(ChType::UInt8),
        ),
    }]);

    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&schema, &mut schema_out);
        let c0 = &**schema_out.children.add(0);
        let entries = &**c0.children.add(0);
        let key = &**entries.children.add(0);
        assert_eq!(CStr::from_ptr(key.format).to_str().unwrap(), "i");
        assert!(!key.dictionary.is_null(), "LC key has a dictionary child");
        let dict = &*key.dictionary;
        assert_eq!(CStr::from_ptr(dict.format).to_str().unwrap(), "u");
        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_map_buffers() {
    use crate::column::{MapColumn, TupleColumn};

    // Map(String, Int32) over 3 rows including an empty row: the map node
    // is byte-identical in shape to an Array node (2 buffers: null
    // validity, i64 offsets), with the entries struct as the single child
    // and the key/value columns as its children.
    // Rows: {a: 13} / {} / {b: 1, c: 2} -> offsets [0, 1, 1, 3].
    let schema = Schema::new(vec![Field {
        name: "m".into(),
        ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
    }]);
    let entries = Column::Tuple(TupleColumn::new(
        vec![
            Column::Utf8(Utf8Column::new(vec![0, 1, 2, 3], b"abc".to_vec())),
            Column::Int32(PrimitiveColumn::new(vec![13, 1, 2])),
        ],
        3,
    ));
    let col = Column::Map(MapColumn::new(vec![0, 1, 1, 3], entries));
    let batch = Arc::new(ColBatch::new(schema, vec![col], 3));

    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);

        let c0 = &**array.children.add(0);
        assert_eq!(c0.length, 3, "map length is the row count");
        assert_eq!(c0.null_count, 0);
        assert_eq!(c0.n_buffers, 2, "LargeList shape: validity + offsets");
        assert!((*c0.buffers.add(0)).is_null(), "map validity always null");
        let offsets = *c0.buffers.add(1) as *const i64;
        assert_eq!(*offsets, 0, "leading 0");
        assert_eq!(*offsets.add(2), 1, "empty row -> repeated offset");
        assert_eq!(*offsets.add(3), 3, "last offset == total entries");

        assert_eq!(c0.n_children, 1);
        let entries = &**c0.children.add(0);
        assert_eq!(entries.length, 3, "entry count == last offset");
        assert_eq!(entries.null_count, 0);
        assert_eq!(entries.n_buffers, 1, "struct: validity slot only");
        assert!((*entries.buffers.add(0)).is_null());
        assert_eq!(entries.n_children, 2);
        let key = &**entries.children.add(0);
        assert_eq!(key.length, 3);
        assert_eq!(key.n_buffers, 3, "utf8 keys: validity, offsets, data");
        let value = &**entries.children.add(1);
        assert_eq!(value.length, 3);
        let vals = *value.buffers.add(1) as *const i32;
        assert_eq!(*vals, 13);
        assert_eq!(*vals.add(2), 2);

        (array.release.unwrap())(&mut array);
    }
}

// -----------------------------------------------------------------------
// SimpleAggregateFunction / geo aliases / Nested Arrow export
// -----------------------------------------------------------------------

#[test]
fn test_arrow_format_name_decoration_delegates() {
    // Each alias reports the Arrow format of the physical type it delegates to.
    assert_eq!(
        arrow_format(&ChType::SimpleAggregateFunction {
            func: "sum".into(),
            inner: Box::new(ChType::Float64),
        }),
        "g"
    );
    assert_eq!(
        arrow_format(&ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::LowCardinality(Box::new(ChType::String))),
        }),
        "i" // dictionary index type
    );
    assert_eq!(arrow_format(&ChType::Geo(GeoKind::Point)), "+s");
    assert_eq!(arrow_format(&ChType::Geo(GeoKind::Ring)), "+L");
    assert_eq!(arrow_format(&ChType::Geo(GeoKind::MultiPolygon)), "+L");
    assert_eq!(
        arrow_format(&ChType::Nested(vec![("a".into(), ChType::UInt32)])),
        "+L"
    );
}

#[test]
fn test_export_point_schema_and_array() {
    // Point exports as an Arrow struct of two float64 children (unnamed tuple
    // elements are named by 1-based position).
    let schema = Schema::new(vec![Field {
        name: "p".into(),
        ch_type: ChType::Geo(GeoKind::Point),
    }]);
    let columns = vec![Column::Tuple(TupleColumn::new(
        vec![
            Column::Float64(PrimitiveColumn::new(vec![1.0, 3.0])),
            Column::Float64(PrimitiveColumn::new(vec![2.0, 4.0])),
        ],
        2,
    ))];
    let batch = Arc::new(ColBatch::new(schema, columns, 2));
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let p = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(p.format).to_str().unwrap(), "+s");
        assert_eq!(p.n_children, 2);
        let x = &**p.children.add(0);
        let y = &**p.children.add(1);
        assert_eq!(CStr::from_ptr(x.format).to_str().unwrap(), "g");
        assert_eq!(CStr::from_ptr(y.format).to_str().unwrap(), "g");
        assert_eq!(CStr::from_ptr(x.name).to_str().unwrap(), "1");
        assert_eq!(CStr::from_ptr(y.name).to_str().unwrap(), "2");
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array);
        let p = &**array.children.add(0);
        assert_eq!(p.length, 2);
        assert_eq!(p.n_children, 2);
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_nullable_point_schema() {
    // Nullable(Point) sets the struct's nullable flag while still emitting its
    // two element children.
    let schema = Schema::new(vec![Field {
        name: "p".into(),
        ch_type: ChType::Nullable(Box::new(ChType::Geo(GeoKind::Point))),
    }]);
    let columns = vec![Column::Tuple(TupleColumn::new_nullable(
        vec![
            Column::Float64(PrimitiveColumn::new(vec![1.0, 0.0])),
            Column::Float64(PrimitiveColumn::new(vec![2.0, 0.0])),
        ],
        2,
        Bitmap::from_ch_null_map(&[0, 1]),
    ))];
    let batch = Arc::new(ColBatch::new(schema, columns, 2));
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let p = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(p.format).to_str().unwrap(), "+s");
        assert_eq!(p.flags & 2, 2, "nullable flag set for Nullable(Point)");
        assert_eq!(p.n_children, 2);
        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_nested_schema() {
    // Nested exports as a LargeList of a struct whose children carry the
    // Nested field names verbatim.
    let schema = Schema::new(vec![Field {
        name: "n".into(),
        ch_type: ChType::Nested(vec![
            ("a".into(), ChType::UInt32),
            ("b".into(), ChType::String),
        ]),
    }]);
    let entries = Column::Tuple(TupleColumn::new(
        vec![
            Column::UInt32(PrimitiveColumn::new(vec![10, 20])),
            Column::Utf8(Utf8Column::new(vec![0, 1, 2], b"xy".to_vec())),
        ],
        2,
    ));
    let columns = vec![Column::Array(ArrayColumn::new(vec![0, 2], entries))];
    let batch = Arc::new(ColBatch::new(schema, columns, 1));
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let n = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(n.format).to_str().unwrap(), "+L");
        assert_eq!(n.n_children, 1);
        let item = &**n.children.add(0);
        assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "+s");
        assert_eq!(CStr::from_ptr(item.name).to_str().unwrap(), "item");
        assert_eq!(item.n_children, 2);
        let a = &**item.children.add(0);
        let b = &**item.children.add(1);
        assert_eq!(CStr::from_ptr(a.name).to_str().unwrap(), "a");
        assert_eq!(CStr::from_ptr(a.format).to_str().unwrap(), "I");
        assert_eq!(CStr::from_ptr(b.name).to_str().unwrap(), "b");
        assert_eq!(CStr::from_ptr(b.format).to_str().unwrap(), "u");
        (schema_out.release.unwrap())(&mut schema_out);
    }
}

#[test]
fn test_export_simple_aggregate_function_over_low_cardinality_schema() {
    // SAF over LowCardinality(Nullable(String)) exports as a dictionary field
    // (index format `i`, a `u` dictionary child, the nullable flag set).
    let schema = Schema::new(vec![Field {
        name: "s".into(),
        ch_type: ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::LowCardinality(Box::new(ChType::Nullable(
                Box::new(ChType::String),
            )))),
        },
    }]);
    let columns = vec![Column::Dictionary(DictionaryColumn::new_nullable(
        vec![1, 0],
        Column::Utf8(Utf8Column::new(vec![0, 0, 6], b"user_1".to_vec())),
        Bitmap::from_ch_null_map(&[0, 1]),
    ))];
    let batch = Arc::new(ColBatch::new(schema, columns, 2));
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let s = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(s.format).to_str().unwrap(), "i");
        assert_eq!(
            s.flags & 2,
            2,
            "nullable flag set for SAF over LC(Nullable)"
        );
        assert!(!s.dictionary.is_null(), "dictionary child present");
        let dict = &*s.dictionary;
        assert_eq!(CStr::from_ptr(dict.format).to_str().unwrap(), "u");
        (schema_out.release.unwrap())(&mut schema_out);
    }
}
