use super::*;

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
        export_batch_array(&batch, &mut array).unwrap();

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
        export_batch_array(&batch, &mut array).unwrap();

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

#[test]
fn test_export_bfloat16_as_fixed_size_binary_zero_copy() {
    // Arrow has no BFloat16 primitive. `e` means the incompatible IEEE
    // binary16 layout, so expose the exact two-byte words as FixedSizeBinary(2).
    assert_eq!(arrow_format(&ChType::BFloat16), "w:2");

    let raw_data = vec![[0x80, 0x3f], [0xa0, 0xbf], [0x80, 0x7f]];
    let raw_ptr = raw_data.as_ptr() as *const c_void;
    let validity = Bitmap::from_ch_null_map(&[0, 1, 0]);
    let validity_ptr = validity.as_bytes().as_ptr() as *const c_void;
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "bf".into(),
            ch_type: ChType::Nullable(Box::new(ChType::BFloat16)),
        }]),
        vec![Column::BFloat16(PrimitiveColumn::new_nullable(
            raw_data, validity,
        ))],
        3,
    ));

    // Safety: the zeroed FFI outputs are writable and the batch remains alive
    // until each matching release callback is invoked below.
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let child = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(child.format).to_str().unwrap(), "w:2");
        assert_eq!(child.flags, 2);
        assert_eq!(child.n_children, 0);
        assert!(child.dictionary.is_null());
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let child = &**array.children.add(0);
        assert_eq!(child.length, 3);
        assert_eq!(child.null_count, 1);
        assert_eq!(child.n_buffers, 2);
        assert_eq!(*child.buffers.add(0), validity_ptr);
        assert_eq!(*child.buffers.add(1), raw_ptr);
        assert_eq!(child.n_children, 0);
        assert!(child.dictionary.is_null());
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_zero_row_bfloat16_as_fixed_size_binary() {
    let raw_data = Vec::<[u8; 2]>::new();
    let raw_ptr = raw_data.as_ptr() as *const c_void;
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "bf".into(),
            ch_type: ChType::BFloat16,
        }]),
        vec![Column::BFloat16(PrimitiveColumn::new(raw_data))],
        0,
    ));

    // Safety: the zeroed FFI output is writable and the batch remains alive
    // until the matching release callback is invoked below.
    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let child = &**array.children.add(0);
        assert_eq!(child.length, 0);
        assert_eq!(child.null_count, 0);
        assert_eq!(child.n_buffers, 2);
        assert!((*child.buffers.add(0)).is_null());
        assert_eq!(*child.buffers.add(1), raw_ptr);
        assert_eq!(child.n_children, 0);
        assert!(child.dictionary.is_null());
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
        export_batch_array(&batch, &mut array).unwrap();

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
        export_batch_array(&batch, &mut array).unwrap();
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
        export_batch_array(&batch, &mut array).unwrap();
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
fn test_arrow_format_intervals() {
    for kind in [
        IntervalKind::Year,
        IntervalKind::Quarter,
        IntervalKind::Month,
        IntervalKind::Week,
        IntervalKind::Day,
        IntervalKind::Hour,
        IntervalKind::Minute,
    ] {
        assert_eq!(arrow_format(&ChType::Interval(kind)), "l");
    }
    assert_eq!(arrow_format(&ChType::Interval(IntervalKind::Second)), "tDs");
    assert_eq!(
        arrow_format(&ChType::Interval(IntervalKind::Millisecond)),
        "tDm"
    );
    assert_eq!(
        arrow_format(&ChType::Interval(IntervalKind::Microsecond)),
        "tDu"
    );
    assert_eq!(
        arrow_format(&ChType::Interval(IntervalKind::Nanosecond)),
        "tDn"
    );
}

#[test]
fn test_export_interval_buffers_zero_copy() {
    let raw_values = vec![-79i64, 13, i64::MAX];
    let validity = Bitmap::from_ch_null_map(&[0, 1, 0]);
    let values = vec![-13i64, 0, 79];
    let raw_values_ptr = raw_values.as_ptr() as *const c_void;
    let validity_ptr = validity.as_bytes().as_ptr() as *const c_void;
    let values_ptr = values.as_ptr() as *const c_void;
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![
            Field {
                name: "raw".into(),
                ch_type: ChType::Interval(IntervalKind::Month),
            },
            Field {
                name: "duration".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Interval(IntervalKind::Second))),
            },
        ]),
        vec![
            Column::Interval(PrimitiveColumn::new(raw_values)),
            Column::Interval(PrimitiveColumn::new_nullable(values, validity)),
        ],
        3,
    ));

    // Safety: the zeroed FFI outputs are writable and the batch remains alive
    // until each matching release callback is invoked below.
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let raw_schema = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(raw_schema.format).to_str().unwrap(), "l");
        assert_eq!(raw_schema.flags, 0);
        let duration_schema = &**schema_out.children.add(1);
        assert_eq!(
            CStr::from_ptr(duration_schema.format).to_str().unwrap(),
            "tDs"
        );
        assert_eq!(duration_schema.flags, 2);
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let raw = &**array.children.add(0);
        assert_eq!(raw.length, 3);
        assert_eq!(raw.null_count, 0);
        assert_eq!(raw.n_buffers, 2);
        assert!((*raw.buffers.add(0)).is_null());
        assert_eq!(*raw.buffers.add(1), raw_values_ptr);
        assert_eq!(raw.n_children, 0);
        assert!(raw.dictionary.is_null());

        let duration = &**array.children.add(1);
        assert_eq!(duration.length, 3);
        assert_eq!(duration.null_count, 1);
        assert_eq!(duration.n_buffers, 2);
        assert_eq!(*duration.buffers.add(0), validity_ptr);
        assert_eq!(*duration.buffers.add(1), values_ptr);
        assert_eq!(duration.n_children, 0);
        assert!(duration.dictionary.is_null());
        (array.release.unwrap())(&mut array);
    }
}

#[test]
fn test_export_zero_row_interval() {
    let values = Vec::<i64>::new();
    let values_ptr = values.as_ptr() as *const c_void;
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "i".into(),
            ch_type: ChType::Interval(IntervalKind::Nanosecond),
        }]),
        vec![Column::Interval(PrimitiveColumn::new(values))],
        0,
    ));

    // Safety: the zeroed FFI outputs are writable and the batch remains alive
    // until each matching release callback is invoked below. The length-zero
    // values pointer is compared but never dereferenced.
    unsafe {
        let mut schema_out: ArrowSchema = std::mem::zeroed();
        export_schema(&batch.schema, &mut schema_out);
        let child = &**schema_out.children.add(0);
        assert_eq!(CStr::from_ptr(child.format).to_str().unwrap(), "tDn");
        assert_eq!(child.flags, 0);
        (schema_out.release.unwrap())(&mut schema_out);

        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let child = &**array.children.add(0);
        assert_eq!(child.length, 0);
        assert_eq!(child.null_count, 0);
        assert_eq!(child.n_buffers, 2);
        assert!((*child.buffers.add(0)).is_null());
        assert_eq!(*child.buffers.add(1), values_ptr);
        assert_eq!(child.n_children, 0);
        assert!(child.children.is_null());
        assert!(child.dictionary.is_null());
        (array.release.unwrap())(&mut array);
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
        export_batch_array(&batch, &mut array).unwrap();
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
        export_batch_array(&batch, &mut array).unwrap();
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
fn export_empty_schema_uses_null_children_pointer() {
    unsafe {
        let mut out: ArrowSchema = std::mem::zeroed();
        export_schema(&Schema::new(Vec::new()), &mut out);
        assert_eq!(out.n_children, 0);
        assert!(out.children.is_null());
        (out.release.unwrap())(&mut out);
    }
}

#[test]
fn export_empty_offsets_utf8_has_valid_leading_zero_offset() {
    // A hand-built String column with an empty offsets Vec is a zero-row column
    // (Utf8Column::len is offsets.len().saturating_sub(1)). The decoder always
    // emits [0], but Column fields are public. The export must not hand out the
    // dangling as_ptr of a zero-capacity Vec; it substitutes a 'static single
    // zero so the i32 offsets buffer satisfies Arrow's length + 1 contract.
    let batch = Arc::new(ColBatch::new(
        Schema::new(vec![Field {
            name: "s".into(),
            ch_type: ChType::String,
        }]),
        vec![Column::Utf8(Utf8Column::new(vec![], vec![]))],
        0,
    ));

    // Safety: output is a writable zeroed C Data array, released below.
    unsafe {
        let mut array: ArrowArray = std::mem::zeroed();
        export_batch_array(&batch, &mut array).unwrap();
        let child = &**array.children.add(0);
        assert_eq!(child.length, 0);
        assert_eq!(child.null_count, 0);
        assert_eq!(child.n_buffers, 3);
        let offsets = *child.buffers.add(1) as *const i32;
        assert!(!offsets.is_null());
        assert_eq!(*offsets, 0);
        (array.release.unwrap())(&mut array);
    }
}
