use super::*;
use crate::native::varint::write_varint;

struct BlockBuilder {
    buf: Vec<u8>,
    revision: u64,
}

impl BlockBuilder {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            revision: 0,
        }
    }

    /// Frame the block for a protocol revision. A revision > 0 makes
    /// `header` emit a BlockInfo preamble; a revision >= 54454 makes
    /// `column_header` emit the default (0x00) custom-serialization byte.
    fn revision(mut self, revision: u64) -> Self {
        self.revision = revision;
        self
    }

    fn header(mut self, num_cols: usize, num_rows: usize) -> Self {
        if self.revision > 0 {
            Self::push_block_info(&mut self.buf, self.revision);
        }
        write_varint(&mut self.buf, num_cols as u64);
        write_varint(&mut self.buf, num_rows as u64);
        self
    }

    fn column_header(mut self, name: &str, type_name: &str) -> Self {
        Self::push_string(&mut self.buf, name);
        Self::push_string(&mut self.buf, type_name);
        if self.revision >= DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION {
            self.buf.push(0x00); // default serialization
        }
        self
    }

    /// Column header with an explicit custom-serialization marker (and any
    /// trailing kind bytes), regardless of revision. For framing tests.
    fn column_header_with_custom(
        mut self,
        name: &str,
        type_name: &str,
        marker: u8,
        kind_bytes: &[u8],
    ) -> Self {
        Self::push_string(&mut self.buf, name);
        Self::push_string(&mut self.buf, type_name);
        self.buf.push(marker);
        self.buf.extend_from_slice(kind_bytes);
        self
    }

    fn push_string(buf: &mut Vec<u8>, s: &str) {
        write_varint(buf, s.len() as u64);
        buf.extend_from_slice(s.as_bytes());
    }

    /// Standard BlockInfo: is_overflows=false, bucket_num=-1, and an empty
    /// out_of_order_buckets vector at revision >= 54480.
    fn push_block_info(buf: &mut Vec<u8>, revision: u64) {
        write_varint(buf, 1);
        buf.push(0x00); // is_overflows = false
        write_varint(buf, 2);
        buf.extend_from_slice(&(-1i32).to_le_bytes()); // bucket_num = -1
        if revision >= DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS {
            write_varint(buf, 3);
            write_varint(buf, 0); // empty out_of_order_buckets
        }
        write_varint(buf, 0); // terminator
    }

    fn raw_bytes(mut self, bytes: &[u8]) -> Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    fn int64_data(mut self, values: &[i64]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn int32_data(mut self, values: &[i32]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn int16_data(mut self, values: &[i16]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn int8_data(mut self, values: &[i8]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn uint64_data(mut self, values: &[u64]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn uint32_data(mut self, values: &[u32]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn date_data(mut self, values: &[u16]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn float32_data(mut self, values: &[f32]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn float64_data(mut self, values: &[f64]) -> Self {
        for &v in values {
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
        self
    }

    fn string_data(mut self, values: &[&str]) -> Self {
        for &s in values {
            write_varint(&mut self.buf, s.len() as u64);
            self.buf.extend_from_slice(s.as_bytes());
        }
        self
    }

    /// IPv4 column body: raw 4-byte LE UInt32 per row, exactly the UInt32
    /// body, so it shares `uint32_data`'s shape.
    fn ipv4_data(self, values: &[u32]) -> Self {
        self.uint32_data(values)
    }

    /// UUID / IPv6 column body: raw 16-byte rows, no length prefix, exactly
    /// a FixedString(16) body. Each entry must be 16 bytes.
    fn fixed16_data(mut self, values: &[[u8; 16]]) -> Self {
        for v in values {
            self.buf.extend_from_slice(v);
        }
        self
    }

    /// Decimal column body: raw fixed-width little-endian two's-complement
    /// integers, `width` bytes per row, no per-row framing. Each entry must
    /// already be exactly `width` bytes wide.
    fn decimal_data(mut self, rows: &[&[u8]], width: usize) -> Self {
        for r in rows {
            assert_eq!(r.len(), width, "decimal row must be {width} bytes");
            self.buf.extend_from_slice(r);
        }
        self
    }

    /// Wide-integer column body: raw contiguous little-endian fixed-width
    /// integers, `width` bytes per row (16 for Int128/UInt128, 32 for
    /// Int256/UInt256), no per-row framing. Byte-identical to a
    /// Decimal128/256 body, so it shares `decimal_data`'s shape; kept as its
    /// own name for test readability. Each entry must be exactly `width`
    /// bytes.
    fn wide_int_data(self, rows: &[&[u8]], width: usize) -> Self {
        self.decimal_data(rows, width)
    }

    fn null_map(mut self, nulls: &[bool]) -> Self {
        for &is_null in nulls {
            self.buf.push(if is_null { 0x01 } else { 0x00 });
        }
        self
    }

    /// `Array(T)` offsets: exactly `num_rows` raw little-endian u64 cumulative
    /// absolute end-offsets, with NO leading zero and no count, exactly what
    /// `SerializationArray` writes ahead of the flattened element body. The
    /// element body is appended afterward with the ordinary typed helpers
    /// (`int32_data`, `string_data`, `null_map`, `low_cardinality_*`, or a
    /// further `array_offsets` for a nested `Array`).
    fn array_offsets(mut self, offsets: &[u64]) -> Self {
        for &o in offsets {
            self.buf.extend_from_slice(&o.to_le_bytes());
        }
        self
    }

    /// Append a full `LowCardinality(T)` column block payload around an
    /// already-serialized dictionary body: the per-column key-version prefix,
    /// the index type word with the chosen index width, the dictionary entry
    /// count and `dict_bytes`, the row count, and the raw index array.
    /// `index_width` is 1/2/4/8 bytes (UInt8..UInt64); indices are written
    /// little-endian at that width. The typed `low_cardinality_*` helpers
    /// build `dict_bytes` for a given inner type and call this.
    fn low_cardinality_block(
        mut self,
        num_keys: usize,
        dict_bytes: &[u8],
        indices: &[u64],
        index_width: usize,
    ) -> Self {
        // Per-column state prefix: key version = 1.
        self.buf.extend_from_slice(&1u64.to_le_bytes());

        // Index type word: width tag in the low bits, HasAdditionalKeysBit set.
        let width_tag: u64 = match index_width {
            1 => 0,
            2 => 1,
            4 => 2,
            8 => 3,
            other => panic!("unsupported test index width {other}"),
        };
        let index_word = width_tag | LC_HAS_ADDITIONAL_KEYS_BIT;
        self.buf.extend_from_slice(&index_word.to_le_bytes());

        // Per-block dictionary: entry count then the inner-type body bytes.
        self.buf.extend_from_slice(&(num_keys as u64).to_le_bytes());
        self.buf.extend_from_slice(dict_bytes);

        // Row count, then the raw index array at the chosen width.
        self.buf
            .extend_from_slice(&(indices.len() as u64).to_le_bytes());
        for &idx in indices {
            match index_width {
                1 => self.buf.push(idx as u8),
                2 => self.buf.extend_from_slice(&(idx as u16).to_le_bytes()),
                4 => self.buf.extend_from_slice(&(idx as u32).to_le_bytes()),
                8 => self.buf.extend_from_slice(&idx.to_le_bytes()),
                _ => unreachable!(),
            }
        }
        self
    }

    /// `LowCardinality(String)` block: dictionary entries are varint len +
    /// raw bytes, exactly a plain `String` column body.
    fn low_cardinality_string(
        self,
        dictionary: &[&str],
        indices: &[u64],
        index_width: usize,
    ) -> Self {
        let mut dict_bytes = Vec::new();
        for &s in dictionary {
            write_varint(&mut dict_bytes, s.len() as u64);
            dict_bytes.extend_from_slice(s.as_bytes());
        }
        self.low_cardinality_block(dictionary.len(), &dict_bytes, indices, index_width)
    }

    /// `LowCardinality(UInt32)` block: dictionary entries are raw 4-byte LE
    /// primitives, exactly a plain `UInt32` column body. The `DateTime` and
    /// other 4-byte numeric inners share this body shape.
    fn low_cardinality_u32(self, dictionary: &[u32], indices: &[u64], index_width: usize) -> Self {
        let mut dict_bytes = Vec::new();
        for &v in dictionary {
            dict_bytes.extend_from_slice(&v.to_le_bytes());
        }
        self.low_cardinality_block(dictionary.len(), &dict_bytes, indices, index_width)
    }

    /// `LowCardinality(Date)` block: dictionary entries are raw 2-byte LE
    /// `UInt16` days, exactly a plain `Date`/`UInt16` column body.
    fn low_cardinality_u16(self, dictionary: &[u16], indices: &[u64], index_width: usize) -> Self {
        let mut dict_bytes = Vec::new();
        for &v in dictionary {
            dict_bytes.extend_from_slice(&v.to_le_bytes());
        }
        self.low_cardinality_block(dictionary.len(), &dict_bytes, indices, index_width)
    }

    /// `LowCardinality(FixedString(N))` block: dictionary entries are raw
    /// fixed-width bytes, exactly a plain `FixedString(N)` column body. Each
    /// entry must already be `N` bytes wide.
    fn low_cardinality_fixed(
        self,
        dictionary: &[&[u8]],
        indices: &[u64],
        index_width: usize,
    ) -> Self {
        let mut dict_bytes = Vec::new();
        for &entry in dictionary {
            dict_bytes.extend_from_slice(entry);
        }
        self.low_cardinality_block(dictionary.len(), &dict_bytes, indices, index_width)
    }

    /// `LowCardinality(UUID)` / `LowCardinality(IPv6)` block: dictionary
    /// entries are raw 16-byte rows, exactly a plain UUID/IPv6 column body.
    fn low_cardinality_fixed16(
        self,
        dictionary: &[[u8; 16]],
        indices: &[u64],
        index_width: usize,
    ) -> Self {
        let mut dict_bytes = Vec::new();
        for entry in dictionary {
            dict_bytes.extend_from_slice(entry);
        }
        self.low_cardinality_block(dictionary.len(), &dict_bytes, indices, index_width)
    }

    fn build(self) -> Vec<u8> {
        self.buf
    }
}

#[test]
fn test_decode_all_int_widths() {
    let data = BlockBuilder::new()
        .header(4, 2)
        .column_header("a", "Int8")
        .int8_data(&[1, -1])
        .column_header("b", "Int16")
        .int16_data(&[256, -256])
        .column_header("c", "Int32")
        .int32_data(&[70000, -70000])
        .column_header("d", "Int64")
        .int64_data(&[1_000_000_000, -1])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 2);
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Int8(c) => assert_eq!(c.values, vec![1i8, -1]),
        _ => panic!("expected Int8"),
    }
    match batch.column(1) {
        Column::Int16(c) => assert_eq!(c.values, vec![256i16, -256]),
        _ => panic!("expected Int16"),
    }
}

#[test]
fn test_decode_all_uint_widths() {
    let data = BlockBuilder::new()
        .header(4, 1)
        .column_header("a", "UInt8")
        .raw_bytes(&[255u8])
        .column_header("b", "UInt16")
        .raw_bytes(&200u16.to_le_bytes())
        .column_header("c", "UInt32")
        .uint32_data(&[4_000_000_000])
        .column_header("d", "UInt64")
        .uint64_data(&[u64::MAX])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::UInt8(c) => assert_eq!(c.values[0], 255),
        _ => panic!(),
    }
    match batch.column(3) {
        Column::UInt64(c) => assert_eq!(c.values[0], u64::MAX),
        _ => panic!(),
    }
}

#[test]
fn test_decode_float32() {
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("f", "Float32")
        .float32_data(&[1.5f32, -2.25])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Float32(c) => {
            assert_eq!(c.values[0], 1.5f32);
            assert_eq!(c.values[1], -2.25f32);
        }
        _ => panic!("expected Float32"),
    }
}

#[test]
fn test_decode_float64() {
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("f", "Float64")
        .float64_data(&[3.5f64, -7.25, 0.0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Float64(c) => {
            assert_eq!(c.values[0], 3.5f64);
            assert_eq!(c.values[1], -7.25f64);
            assert_eq!(c.values[2], 0.0f64);
        }
        _ => panic!("expected Float64"),
    }
}

#[test]
fn test_decode_bool() {
    let data = BlockBuilder::new()
        .header(1, 5)
        .column_header("b", "Bool")
        .raw_bytes(&[1, 0, 1, 0, 1])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Bool(c) => {
            assert_eq!(c.len(), 5);
            assert!(c.get(0));
            assert!(!c.get(1));
            assert!(c.get(2));
            assert!(!c.get(3));
            assert!(c.get(4));
        }
        _ => panic!("expected Bool"),
    }
}

#[test]
fn test_decode_nullable_int32() {
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("n", "Nullable(Int32)")
        .null_map(&[false, true, false])
        .int32_data(&[100, 0, 300])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Int32(c) => {
            assert_eq!(c.null_count(), 1);
            assert_eq!(c.values, vec![100, 0, 300]);
        }
        _ => panic!("expected Int32"),
    }
}

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
fn test_block_end_scans_string_column() {
    // The completeness scan must return the exact end offset of a block whose
    // String column it walks via the per-value length prefixes, and report a
    // one-byte-short buffer as "need more bytes" (UnexpectedEof).
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("s", "String")
        .string_data(&["user_1", "", "user_2"])
        .build();

    let end = block_end(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(end, Some(data.len()));

    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_block_end_zero_rows() {
    // A zero-row block is complete once its headers are buffered.
    let data = BlockBuilder::new()
        .header(2, 0)
        .column_header("a", "Int32")
        .column_header("b", "String")
        .build();
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_block_end_rejects_unsupported_type() {
    // An unsupported type inside an otherwise-complete block must surface as
    // a DecodeError from the scan, not be silently skipped or reported as
    // incomplete. `Nothing` is not decoded yet, so it serves as the example.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("id", "Nothing")
        .build();
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_block_end_no_block_at_clean_boundary() {
    // No-framing stream: an empty buffer is a clean boundary, not a block.
    assert_eq!(block_end(&[], &DecodeOptions::default()).unwrap(), None);
}

#[test]
fn test_unsupported_type() {
    // `Nothing` is not decoded yet, so it serves as the unsupported example
    // now that the wide integers, Decimal, and UUID/IPv4/IPv6 are decoded.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("id", "Nothing")
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_nested_nullable_type_is_rejected() {
    // `Nullable(Nullable(T))` is not a type ClickHouse emits, and the
    // single-`Nullable` unwrap in decode/scan cannot handle it, so it must be
    // rejected at header parse time rather than panic on the inner wrapper.
    // Exercise both a row-bearing and a zero-row block, and both the allocating
    // decode and the completeness scan, since the review found a distinct panic
    // site on each path.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("n", "Nullable(Nullable(Int32))")
            .build();
        assert!(
            matches!(
                decode_all_bytes(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ),
            "decode should reject nested Nullable at {num_rows} rows"
        );
        assert!(
            matches!(
                block_end(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ),
            "scan should reject nested Nullable at {num_rows} rows"
        );
    }
}

#[test]
fn test_nullable_low_cardinality_type_is_rejected() {
    // `Nullable(LowCardinality(T))` is the illegal nesting direction (only
    // `LowCardinality(Nullable(T))` is legal). The inner `LowCardinality` is
    // checked only at the top level, so accepting this shape would reach an
    // `unreachable!`; it must be rejected at header parse time instead.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("n", "Nullable(LowCardinality(String))")
            .build();
        assert!(
            matches!(
                decode_all_bytes(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ),
            "decode should reject Nullable(LowCardinality) at {num_rows} rows"
        );
        assert!(
            matches!(
                block_end(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ),
            "scan should reject Nullable(LowCardinality) at {num_rows} rows"
        );
    }
}

#[test]
fn test_low_cardinality_nullable_still_accepted() {
    // The legal direction, `LowCardinality(Nullable(T))`, must still parse: the
    // parse-time rejection only refuses wrappers nested inside `Nullable`, not
    // this one. A zero-row block is enough to prove the type parses and is
    // decodable without needing a full dictionary body.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("lc", "LowCardinality(Nullable(String))")
        .build();
    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String))))
    );
    assert!(block_end(&data, &DecodeOptions::default()).is_ok());
}

#[test]
fn test_multi_block_kept_as_chunks() {
    // Two Int32 blocks must be kept as two separate chunks, NOT merged.
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("n", "Int32")
        .int32_data(&[1, 2])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 3)
            .column_header("n", "Int32")
            .int32_data(&[3, 4, 5])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
    match cb.chunks[0].column(0) {
        Column::Int32(c) => assert_eq!(c.values, vec![1, 2]),
        _ => panic!(),
    }
    match cb.chunks[1].column(0) {
        Column::Int32(c) => assert_eq!(c.values, vec![3, 4, 5]),
        _ => panic!(),
    }
}

#[test]
fn test_block_schema_mismatch_rejected() {
    // Every block of a result shares one schema. A later block with a
    // different column count, type, or name is a corrupt payload.
    let first = BlockBuilder::new()
        .header(2, 1)
        .column_header("a", "Int32")
        .int32_data(&[13])
        .column_header("b", "Int32")
        .int32_data(&[79])
        .build();

    let fewer_columns = BlockBuilder::new()
        .header(1, 1)
        .column_header("a", "Int32")
        .int32_data(&[5])
        .build();
    let different_type = BlockBuilder::new()
        .header(2, 1)
        .column_header("a", "Int32")
        .int32_data(&[5])
        .column_header("b", "String")
        .string_data(&["u1"])
        .build();
    let different_name = BlockBuilder::new()
        .header(2, 1)
        .column_header("a", "Int32")
        .int32_data(&[5])
        .column_header("c", "Int32")
        .int32_data(&[7])
        .build();

    for second in [fewer_columns, different_type, different_name] {
        let mut data = first.clone();
        data.extend(second);
        match decode_all_bytes(&data, &DecodeOptions::default()) {
            Err(DecodeError::BlockSchemaMismatch { block_index }) => {
                assert_eq!(block_index, 1)
            }
            other => panic!("expected BlockSchemaMismatch, got {other:?}"),
        }
    }
}

#[test]
fn test_zero_row_trailer_with_matching_schema_accepted() {
    // The server re-emits the column headers in a zero-row trailer block;
    // a matching trailer must decode cleanly and contribute no chunk.
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("n", "Int32")
        .int32_data(&[1, 2])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 0)
            .column_header("n", "Int32")
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 1);
    assert_eq!(cb.num_rows(), 2);
}

#[test]
fn test_multi_block_bool_kept_as_chunks() {
    // Bool blocks stay separate — no O(n^2) bitmap re-packing.
    let mut data = BlockBuilder::new()
        .header(1, 3)
        .column_header("b", "Bool")
        .raw_bytes(&[1, 0, 1])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 2)
            .column_header("b", "Bool")
            .raw_bytes(&[0, 1])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
    match cb.chunks[0].column(0) {
        Column::Bool(c) => {
            assert!(c.get(0));
            assert!(!c.get(1));
            assert!(c.get(2));
        }
        _ => panic!(),
    }
    match cb.chunks[1].column(0) {
        Column::Bool(c) => {
            assert!(!c.get(0));
            assert!(c.get(1));
        }
        _ => panic!(),
    }
}

#[test]
fn test_empty_block() {
    // A zero-row block contributes the schema but no chunks.
    let data = BlockBuilder::new()
        .header(2, 0)
        .column_header("a", "Int32")
        .column_header("b", "String")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 2);
}

#[test]
fn test_modern_framing_roundtrip() {
    // Full v26.6.1.1193 framing: a BlockInfo preamble plus a per-column
    // custom-serialization byte (0 = default) ahead of the data.
    let data = BlockBuilder::new()
        .revision(DBMS_TCP_PROTOCOL_VERSION)
        .header(1, 2)
        .column_header("v", "Int64")
        .int64_data(&[77, 88])
        .build();

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    let cb = decode_all_bytes(&data, &options).unwrap();
    assert_eq!(cb.num_rows(), 2);
    match cb.chunks[0].column(0) {
        Column::Int64(c) => assert_eq!(c.values, vec![77, 88]),
        _ => panic!("expected Int64"),
    }
}

#[test]
fn test_two_field_block_info_parses() {
    // A revision between the custom-serialization (54454) and
    // out-of-order-buckets (54480) cutoffs: the BlockInfo has only fields 1
    // and 2 (8 bytes), and the custom-serialization byte is present. The
    // self-describing parser handles the shorter preamble.
    let revision = 54460;
    let data = BlockBuilder::new()
        .revision(revision)
        .header(1, 1)
        .column_header("n", "Int32")
        .int32_data(&[91])
        .build();

    let options = DecodeOptions {
        protocol_revision: revision,
    };
    let cb = decode_all_bytes(&data, &options).unwrap();
    match cb.chunks[0].column(0) {
        Column::Int32(c) => assert_eq!(c.values, vec![91]),
        _ => panic!("expected Int32"),
    }
}

#[test]
fn test_multi_block_modern_framing() {
    // Each block carries its own BlockInfo preamble at revision > 0, and the
    // boundary between them is found by parsing, not a fixed skip.
    let mut data = BlockBuilder::new()
        .revision(DBMS_TCP_PROTOCOL_VERSION)
        .header(1, 2)
        .column_header("n", "Int32")
        .int32_data(&[1, 2])
        .build();
    data.extend(
        BlockBuilder::new()
            .revision(DBMS_TCP_PROTOCOL_VERSION)
            .header(1, 3)
            .column_header("n", "Int32")
            .int32_data(&[3, 4, 5])
            .build(),
    );

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    let cb = decode_all_bytes(&data, &options).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
}

#[test]
fn test_zero_row_modern_block() {
    // A zero-row block still carries the per-column custom-serialization
    // byte, which must be consumed even though no data follows.
    let data = BlockBuilder::new()
        .revision(DBMS_TCP_PROTOCOL_VERSION)
        .header(2, 0)
        .column_header("a", "Int32")
        .column_header("b", "String")
        .build();

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    let cb = decode_all_bytes(&data, &options).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 2);
}

#[test]
fn test_custom_serialization_rejected() {
    // A nonzero custom-serialization marker selects a layout this crate does
    // not decode. Reject it rather than misread the column.
    let data = BlockBuilder::new()
        .revision(DBMS_TCP_PROTOCOL_VERSION)
        .header(1, 1)
        .column_header_with_custom("c", "Int32", 0x01, &[0x01]) // 0x01 = SPARSE kind stack
        .build();

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    assert!(matches!(
        decode_all_bytes(&data, &options),
        Err(DecodeError::UnsupportedSerialization {
            serialization_byte: 1,
            ..
        })
    ));
}

#[test]
fn test_unknown_block_info_field_rejected() {
    // An unknown BlockInfo field number is rejected, matching the server.
    let mut data = Vec::new();
    write_varint(&mut data, 7); // unknown field number

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    assert!(matches!(
        decode_all_bytes(&data, &options),
        Err(DecodeError::InvalidBlockInfo { field_num: 7 })
    ));
}

// A row/column count that big would make `Vec::with_capacity` abort the
// process. The hardened decoder must return an error instead of panicking.
// `1 << 61` also overflows the primitive byte-length multiply (`* 8`), so
// these cover both the count guard and the `checked_mul` path.
const HOSTILE_COUNT: usize = 1 << 61;

#[test]
fn test_oversized_row_count_primitive_rejected() {
    let data = BlockBuilder::new()
        .header(1, HOSTILE_COUNT)
        .column_header("v", "Int64")
        .build();
    match decode_all_bytes(&data, &DecodeOptions::default()) {
        Err(DecodeError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

#[test]
fn test_oversized_row_count_string_rejected() {
    let data = BlockBuilder::new()
        .header(1, HOSTILE_COUNT)
        .column_header("s", "String")
        .build();
    match decode_all_bytes(&data, &DecodeOptions::default()) {
        Err(DecodeError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

#[test]
fn test_oversized_column_count_rejected() {
    let data = BlockBuilder::new()
        .header(HOSTILE_COUNT, 1)
        .column_header("n", "Int8")
        .raw_bytes(&[13])
        .build();
    match decode_all_bytes(&data, &DecodeOptions::default()) {
        Err(DecodeError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Temporal types
// -----------------------------------------------------------------------

#[test]
fn test_decode_temporal_plain() {
    // Date is UInt16 days, Date32 is Int32 days (signed, can be pre-epoch),
    // DateTime is UInt32 seconds, DateTime64(3) is Int64 epoch ticks,
    // Time is signed Int32 seconds, and Time64(3) is signed Int64 time ticks.
    // Timezone and precision are type metadata only, never in the bytes.
    let data = BlockBuilder::new()
        .header(6, 4)
        .column_header("d", "Date")
        .date_data(&[0, 19737, 49710, 65535])
        .column_header("d32", "Date32")
        .int32_data(&[-7227, 0, 19737, 84370])
        .column_header("dt", "DateTime")
        .uint32_data(&[0, 1705322096, 961056000, 4294967295])
        .column_header("dt64", "DateTime64(3)")
        .int64_data(&[-877, 0, 1705322096789, 4102444799999])
        .column_header("t", "Time")
        .int32_data(&[-3_599_999, -13, 0, 3_599_999])
        .column_header("t64", "Time64(3)")
        .int64_data(&[-3_599_999_999, -13_000, 0, 3_599_999_999])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Date(c) => assert_eq!(c.values, vec![0u16, 19737, 49710, 65535]),
        other => panic!("expected Date, got {other:?}"),
    }
    match batch.column(1) {
        Column::Date32(c) => assert_eq!(c.values, vec![-7227i32, 0, 19737, 84370]),
        other => panic!("expected Date32, got {other:?}"),
    }
    match batch.column(2) {
        Column::DateTime(c) => {
            assert_eq!(c.values, vec![0u32, 1705322096, 961056000, 4294967295])
        }
        other => panic!("expected DateTime, got {other:?}"),
    }
    match batch.column(3) {
        Column::DateTime64(c) => {
            assert_eq!(c.values, vec![-877i64, 0, 1705322096789, 4102444799999])
        }
        other => panic!("expected DateTime64, got {other:?}"),
    }
    match batch.column(4) {
        Column::Time(c) => assert_eq!(c.values, vec![-3_599_999i32, -13, 0, 3_599_999]),
        other => panic!("expected Time, got {other:?}"),
    }
    match batch.column(5) {
        Column::Time64(c) => {
            assert_eq!(c.values, vec![-3_599_999_999i64, -13_000, 0, 3_599_999_999])
        }
        other => panic!("expected Time64, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_datetime64() {
    // Nullable(DateTime64(3)): null map then the Int64 ticks payload, with
    // null rows still carrying a placeholder value on the wire.
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("ts", "Nullable(DateTime64(3))")
        .null_map(&[false, true, false, true])
        .int64_data(&[-877, 0, 1705322096789, 0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::DateTime64(c) => {
            assert_eq!(c.null_count(), 2);
            assert_eq!(c.values, vec![-877i64, 0, 1705322096789, 0]);
        }
        other => panic!("expected DateTime64, got {other:?}"),
    }
    assert!(batch.column(0).validity().unwrap().is_valid(0));
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
}

#[test]
fn test_decode_nullable_time_types() {
    let data = BlockBuilder::new()
        .header(2, 4)
        .column_header("t", "Nullable(Time)")
        .null_map(&[false, true, false, true])
        .int32_data(&[-13, 0, 79, 0])
        .column_header("t64", "Nullable(Time64(6))")
        .null_map(&[false, true, false, true])
        .int64_data(&[-13_000_000, 0, 79_000_000, 0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Time(c) => {
            assert_eq!(c.values, vec![-13, 0, 79, 0]);
            assert_eq!(c.null_count(), 2);
        }
        other => panic!("expected nullable Time, got {other:?}"),
    }
    match batch.column(1) {
        Column::Time64(c) => {
            assert_eq!(c.values, vec![-13_000_000, 0, 79_000_000, 0]);
            assert_eq!(c.null_count(), 2);
        }
        other => panic!("expected nullable Time64, got {other:?}"),
    }
}

#[test]
fn test_decode_temporal_zero_rows() {
    // A zero-row block carrying temporal columns contributes the
    // schema but no chunks, and the empty columns have length 0.
    let data = BlockBuilder::new()
        .header(4, 0)
        .column_header("d", "Date")
        .column_header("dt", "DateTime")
        .column_header("t", "Time")
        .column_header("t64", "Time64(3)")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 4);
    assert_eq!(cb.schema.fields[0].ch_type, ChType::Date);
    assert_eq!(
        cb.schema.fields[1].ch_type,
        ChType::DateTime { timezone: None }
    );
    assert_eq!(cb.schema.fields[2].ch_type, ChType::Time);
    assert_eq!(cb.schema.fields[3].ch_type, ChType::Time64 { precision: 3 });
}

#[test]
fn test_multi_block_date_kept_as_chunks() {
    // Date blocks stay separate chunks, never concatenated.
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("d", "Date")
        .date_data(&[0, 19737])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 3)
            .column_header("d", "Date")
            .date_data(&[49710, 65535, 13])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
    match cb.chunks[0].column(0) {
        Column::Date(c) => assert_eq!(c.values, vec![0u16, 19737]),
        other => panic!("expected Date, got {other:?}"),
    }
    match cb.chunks[1].column(0) {
        Column::Date(c) => assert_eq!(c.values, vec![49710u16, 65535, 13]),
        other => panic!("expected Date, got {other:?}"),
    }
}

#[test]
fn test_multi_block_time_types_kept_as_chunks() {
    let mut data = BlockBuilder::new()
        .header(2, 2)
        .column_header("t", "Time")
        .int32_data(&[-13, 0])
        .column_header("t64", "Time64(3)")
        .int64_data(&[-13_000, 0])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(2, 2)
            .column_header("t", "Time")
            .int32_data(&[79, 3_599_999])
            .column_header("t64", "Time64(3)")
            .int64_data(&[79_000, 3_599_999_999])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    match cb.chunks[0].column(0) {
        Column::Time(c) => assert_eq!(c.values, vec![-13, 0]),
        other => panic!("expected Time, got {other:?}"),
    }
    match cb.chunks[1].column(1) {
        Column::Time64(c) => assert_eq!(c.values, vec![79_000, 3_599_999_999]),
        other => panic!("expected Time64, got {other:?}"),
    }
}

#[test]
fn test_block_end_scans_time_types() {
    let data = BlockBuilder::new()
        .header(2, 2)
        .column_header("t", "Time")
        .int32_data(&[-13, 79])
        .column_header("t64", "Time64(9)")
        .int64_data(&[-13_000_000_000, 79_000_000_000])
        .build();
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    assert!(matches!(
        block_end(truncated, &DecodeOptions::default()),
        Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_parse_ch_type_temporal() {
    assert_eq!(parse_ch_type("Date"), Some(ChType::Date));
    assert_eq!(parse_ch_type("Date32"), Some(ChType::Date32));
    assert_eq!(
        parse_ch_type("DateTime"),
        Some(ChType::DateTime { timezone: None })
    );
    assert_eq!(
        parse_ch_type("DateTime('UTC')"),
        Some(ChType::DateTime {
            timezone: Some("UTC".to_string())
        })
    );
    assert_eq!(
        parse_ch_type("DateTime64(3)"),
        Some(ChType::DateTime64 {
            precision: 3,
            timezone: None
        })
    );
    assert_eq!(
        parse_ch_type("DateTime64(3, 'UTC')"),
        Some(ChType::DateTime64 {
            precision: 3,
            timezone: Some("UTC".to_string())
        })
    );
    assert_eq!(
        parse_ch_type("DateTime64(9)"),
        Some(ChType::DateTime64 {
            precision: 9,
            timezone: None
        })
    );
    // Precision above the 0..=9 range is unsupported, surfaced as None so
    // the caller reports UnsupportedType.
    assert_eq!(parse_ch_type("DateTime64(10)"), None);
    assert_eq!(parse_ch_type("Time"), Some(ChType::Time));
    assert_eq!(
        parse_ch_type("Time64(3)"),
        Some(ChType::Time64 { precision: 3 })
    );
    assert_eq!(
        parse_ch_type("Time64(9)"),
        Some(ChType::Time64 { precision: 9 })
    );
    // Accept only canonical strings emitted in Native headers. Bare Time64
    // is an input shorthand for Time64(3), not an emitted spelling.
    for unsupported in [
        "Time()",
        "Time(3)",
        "Time('UTC')",
        "Time64",
        "Time64()",
        "Time64(03)",
        "Time64(10)",
        "Time64(3, 'UTC')",
        "Time64(3, '')",
    ] {
        assert_eq!(parse_ch_type(unsupported), None, "accepted {unsupported}");
    }
}

#[test]
fn test_ch_type_display_round_trips_through_parser() {
    // Display renders the canonical ClickHouse type name, which is the
    // string bindings hand to users. Every representative variant must
    // parse back to the exact same ChType.
    let cases = vec![
        ChType::Bool,
        ChType::Int8,
        ChType::Int16,
        ChType::Int32,
        ChType::Int64,
        ChType::UInt8,
        ChType::UInt16,
        ChType::UInt32,
        ChType::UInt64,
        ChType::Float32,
        ChType::Float64,
        ChType::String,
        ChType::FixedString(16),
        ChType::Date,
        ChType::Date32,
        ChType::DateTime { timezone: None },
        ChType::DateTime {
            timezone: Some("UTC".to_string()),
        },
        ChType::DateTime64 {
            precision: 3,
            timezone: None,
        },
        ChType::DateTime64 {
            precision: 9,
            timezone: Some("Asia/Istanbul".to_string()),
        },
        ChType::Time,
        ChType::Time64 { precision: 0 },
        ChType::Time64 { precision: 9 },
        ChType::Nullable(Box::new(ChType::String)),
        ChType::Nullable(Box::new(ChType::DateTime64 {
            precision: 6,
            timezone: Some("UTC".to_string()),
        })),
    ];
    for t in cases {
        let rendered = t.to_string();
        assert_eq!(
            parse_ch_type(&rendered),
            Some(t.clone()),
            "Display output {rendered:?} did not parse back to {t:?}"
        );
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

#[test]
fn test_block_info_out_of_order_buckets_skipped() {
    // A BlockInfo carrying a nonzero out_of_order_buckets vector (field 3,
    // present at server revision >= 54480): a varint count then that many
    // Int32 values, confirmed against BlockInfo::write at v26.6.1.1193-stable.
    // The committed fixtures only ever exercise the empty-vector case, so
    // assemble a nonzero one by hand and confirm the decoder skips the whole
    // vector and lands exactly on the block body.
    let mut data = Vec::new();
    write_varint(&mut data, 1); // field 1: is_overflows
    data.push(0x00);
    write_varint(&mut data, 2); // field 2: bucket_num
    data.extend_from_slice(&(-1i32).to_le_bytes());
    write_varint(&mut data, 3); // field 3: out_of_order_buckets
    write_varint(&mut data, 2); // count = 2
    data.extend_from_slice(&7i32.to_le_bytes());
    data.extend_from_slice(&9i32.to_le_bytes());
    write_varint(&mut data, 0); // terminator
                                // Block body: one Int32 column, one row.
    write_varint(&mut data, 1); // num_cols
    write_varint(&mut data, 1); // num_rows
    write_varint(&mut data, 1); // name length
    data.extend_from_slice(b"n");
    write_varint(&mut data, 5); // type length
    data.extend_from_slice(b"Int32");
    data.push(0x00); // default serialization (revision >= 54454)
    data.extend_from_slice(&13i32.to_le_bytes());

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    let cb = decode_all_bytes(&data, &options).unwrap();
    match cb.chunks[0].column(0) {
        Column::Int32(c) => assert_eq!(c.values, vec![13]),
        other => panic!("expected Int32, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// LowCardinality
// -----------------------------------------------------------------------

/// Resolve a dictionary column's row `i` to its dictionary value bytes,
/// treating a null index as `None`. Used by the LowCardinality tests to
/// assert the observable per-row values the way a consumer would read them.
fn lc_value(col: &Column, row: usize) -> Option<Vec<u8>> {
    match col {
        Column::Dictionary(d) => {
            if let Some(bm) = &d.validity {
                if !bm.is_valid(row) {
                    return None;
                }
            }
            let idx = d.indices[row] as usize;
            match d.values.as_ref() {
                Column::Utf8(v) => Some(v.value(idx).to_vec()),
                other => panic!("expected Utf8 dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_parse_ch_type_low_cardinality() {
    assert_eq!(
        parse_ch_type("LowCardinality(String)"),
        Some(ChType::LowCardinality(Box::new(ChType::String)))
    );
    assert_eq!(
        parse_ch_type("LowCardinality(Nullable(String))"),
        Some(ChType::LowCardinality(Box::new(ChType::Nullable(
            Box::new(ChType::String)
        ))))
    );
}

#[test]
fn test_ch_type_display_round_trips_low_cardinality() {
    for t in [
        ChType::LowCardinality(Box::new(ChType::String)),
        ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String)))),
    ] {
        assert_eq!(parse_ch_type(&t.to_string()), Some(t.clone()));
    }
}

#[test]
fn test_decode_low_cardinality_string() {
    // Six rows over the dictionary the server actually emits: even for a
    // non-nullable inner type the wire reserves slot 0 with an empty string
    // (the ColumnUnique default), and the per-row indexes start at 1. Slot 0
    // is simply never referenced here; it is not a null sentinel. This
    // mirrors the live `lc` fixture (`['', 'user_1', ...]`, indices from 1)
    // rather than a slot-0-less layout the server never produces.
    let dictionary = ["", "user_1", "user_2", "user_3"];
    let indices = [1u64, 2, 3, 1, 2, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(String)")
        .low_cardinality_string(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), 6);
            assert_eq!(d.null_count(), 0);
            assert!(d.validity.is_none());
            assert_eq!(d.indices, vec![1, 2, 3, 1, 2, 1]);
            match d.values.as_ref() {
                Column::Utf8(v) => {
                    assert_eq!(v.len(), 4);
                    assert_eq!(v.value(0), b"");
                    assert_eq!(v.value(1), b"user_1");
                    assert_eq!(v.value(2), b"user_2");
                    assert_eq!(v.value(3), b"user_3");
                }
                other => panic!("expected Utf8 values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    let expected: [&[u8]; 6] = [
        b"user_1", b"user_2", b"user_3", b"user_1", b"user_2", b"user_1",
    ];
    for (row, want) in expected.iter().enumerate() {
        assert_eq!(lc_value(batch.column(0), row).as_deref(), Some(*want));
    }
}

#[test]
fn test_decode_low_cardinality_nullable_string() {
    // For Nullable(String) the dictionary's index 0 is the NULL sentinel,
    // with an empty-string on-wire value. Rows whose index is 0 are null.
    let dictionary = ["", "user_1", "user_2"];
    let indices = [1u64, 0, 2, 0, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(Nullable(String))")
        .low_cardinality_string(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), 5);
            assert_eq!(d.null_count(), 2);
            let bm = d.validity.as_ref().expect("nullable dictionary validity");
            assert!(bm.is_valid(0));
            assert!(!bm.is_valid(1));
            assert!(bm.is_valid(2));
            assert!(!bm.is_valid(3));
            assert!(bm.is_valid(4));
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    assert_eq!(
        lc_value(batch.column(0), 0).as_deref(),
        Some(b"user_1" as &[u8])
    );
    assert_eq!(lc_value(batch.column(0), 1), None);
    assert_eq!(
        lc_value(batch.column(0), 2).as_deref(),
        Some(b"user_2" as &[u8])
    );
    assert_eq!(lc_value(batch.column(0), 3), None);
    assert_eq!(
        lc_value(batch.column(0), 4).as_deref(),
        Some(b"user_1" as &[u8])
    );
}

#[test]
fn test_decode_low_cardinality_saf_nullable_string() {
    // `LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))` is
    // a real server header, hexdump-confirmed live at v26.6.1.1193-stable. The
    // SAF is a pure name decoration, so the wire body is byte-identical to
    // `LowCardinality(Nullable(String))`: a per-block dictionary whose slot 0
    // is the NULL sentinel, then per-row indexes. Decode must see through the
    // SAF chain and treat the column as nullable. Exercised at a bare stream
    // (rev 0) and full modern framing (rev 54485), with one nulls block and
    // one all-valid block in the same stream.
    let type_name = "LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))";
    let expected_type = ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
        func: "anyLast".to_string(),
        inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
    }));

    for revision in [0u64, DBMS_TCP_PROTOCOL_VERSION] {
        // Block 1: rows with nulls. Index 0 is the NULL sentinel.
        let nulls_dict = ["", "user_1", "user_2"];
        let nulls_indices = [1u64, 0, 2, 0, 1];
        let mut data = BlockBuilder::new()
            .revision(revision)
            .header(1, nulls_indices.len())
            .column_header("lc_nsaf", type_name)
            .low_cardinality_string(&nulls_dict, &nulls_indices, 1)
            .build();
        // Block 2: all valid, no index-0 references, still nullable at the type
        // level.
        let valid_dict = ["", "user_3", "user_4"];
        let valid_indices = [1u64, 2, 1];
        data.extend(
            BlockBuilder::new()
                .revision(revision)
                .header(1, valid_indices.len())
                .column_header("lc_nsaf", type_name)
                .low_cardinality_string(&valid_dict, &valid_indices, 1)
                .build(),
        );

        let options = DecodeOptions {
            protocol_revision: revision,
        };
        let cb = decode_all_bytes(&data, &options).unwrap();
        assert_eq!(cb.schema.fields[0].ch_type, expected_type);
        assert_eq!(cb.num_chunks(), 2);

        let nulls = &cb.chunks[0];
        match nulls.column(0) {
            Column::Dictionary(d) => {
                assert_eq!(d.len(), 5);
                assert_eq!(d.null_count(), 2);
                assert!(d.validity.is_some());
            }
            other => panic!("expected Dictionary, got {other:?}"),
        }
        let want: [Option<&[u8]>; 5] = [
            Some(b"user_1"),
            None,
            Some(b"user_2"),
            None,
            Some(b"user_1"),
        ];
        for (row, w) in want.iter().enumerate() {
            assert_eq!(lc_value(nulls.column(0), row).as_deref(), *w);
        }

        let valid = &cb.chunks[1];
        match valid.column(0) {
            Column::Dictionary(d) => {
                assert_eq!(d.len(), 3);
                assert_eq!(d.null_count(), 0);
            }
            other => panic!("expected Dictionary, got {other:?}"),
        }
        let want_valid: [&[u8]; 3] = [b"user_3", b"user_4", b"user_3"];
        for (row, w) in want_valid.iter().enumerate() {
            assert_eq!(lc_value(valid.column(0), row).as_deref(), Some(*w));
        }
    }
}

#[test]
fn test_decode_low_cardinality_saf_nullable_string_zero_rows() {
    // A zero-row block with the SAF-aliased LC header must build the same
    // empty column shape as `LowCardinality(Nullable(String))` (an empty
    // nullable dictionary), exercising the `empty_column` delegate path that
    // resolves the SAF chain through `low_cardinality_dict_value_type`.
    let type_name = "LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))";
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("lc_nsaf", type_name)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".to_string(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        }))
    );
    // The empty column is a nullable dictionary of non-nullable String values,
    // matching what the populated blocks decode.
    let empty = empty_column(&cb.schema.fields[0].ch_type);
    match empty {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), 0);
            assert!(d.validity.is_some());
            assert!(matches!(d.values.as_ref(), Column::Utf8(_)));
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_decode_chained_simple_aggregate_function() {
    // A chained SAF resolves through the full delegate chain to its physical
    // inner. `SimpleAggregateFunction(anyLast, SimpleAggregateFunction(sum,
    // UInt64))` is constructible live at v26.6.1.1193-stable; its wire body is
    // a plain UInt64 column. Both the standalone chain and the same chain as a
    // LowCardinality inner must decode.
    let chain = ChType::SimpleAggregateFunction {
        func: "anyLast".to_string(),
        inner: Box::new(ChType::SimpleAggregateFunction {
            func: "sum".to_string(),
            inner: Box::new(ChType::UInt64),
        }),
    };
    assert_eq!(
        parse_ch_type("SimpleAggregateFunction(anyLast, SimpleAggregateFunction(sum, UInt64))"),
        Some(chain.clone())
    );

    // Standalone chained SAF: decodes as a UInt64 primitive body.
    let values = [13u64, 79, 8_589_934_592];
    let data = BlockBuilder::new()
        .header(1, values.len())
        .column_header("saf_chain", &chain.to_string())
        .uint64_data(&values)
        .build();
    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.schema.fields[0].ch_type, chain);
    match cb.chunks[0].column(0) {
        Column::UInt64(c) => assert_eq!(c.values, values.to_vec()),
        other => panic!("expected UInt64, got {other:?}"),
    }

    // Same chain as a LowCardinality inner: the dictionary body is a plain
    // UInt64 run and the column is non-nullable.
    let lc_chain = ChType::LowCardinality(Box::new(chain));
    let (nullable, dict_value_type) = match &lc_chain {
        ChType::LowCardinality(inner) => low_cardinality_dict_value_type(inner),
        _ => unreachable!(),
    };
    assert!(!nullable);
    assert_eq!(dict_value_type, &ChType::UInt64);
}

#[test]
fn test_decode_low_cardinality_zero_rows() {
    // A zero-row block reads no LowCardinality prefix or data; it contributes
    // the schema and an empty dictionary column.
    let data = BlockBuilder::new()
        .header(2, 0)
        .column_header("lc", "LowCardinality(String)")
        .column_header("lcn", "LowCardinality(Nullable(String))")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 2);
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::String))
    );
    assert_eq!(
        cb.schema.fields[1].ch_type,
        ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::String))))
    );
}

#[test]
fn test_multi_block_low_cardinality_separate_dictionaries() {
    // Each Native block carries its own per-block dictionary. The two blocks
    // here use DIFFERENT dictionaries and different index widths, and stay
    // separate chunks. A consumer must resolve each chunk against its own
    // dictionary, never a shared one.
    let mut data = BlockBuilder::new()
        .header(1, 3)
        .column_header("lc", "LowCardinality(String)")
        .low_cardinality_string(&["red", "green"], &[0, 1, 0], 1)
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 2)
            .column_header("lc", "LowCardinality(String)")
            .low_cardinality_string(&["blue", "amber"], &[1, 0], 2)
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);

    let chunk0 = &cb.chunks[0];
    assert_eq!(
        lc_value(chunk0.column(0), 0).as_deref(),
        Some(b"red" as &[u8])
    );
    assert_eq!(
        lc_value(chunk0.column(0), 1).as_deref(),
        Some(b"green" as &[u8])
    );
    assert_eq!(
        lc_value(chunk0.column(0), 2).as_deref(),
        Some(b"red" as &[u8])
    );

    let chunk1 = &cb.chunks[1];
    assert_eq!(
        lc_value(chunk1.column(0), 0).as_deref(),
        Some(b"amber" as &[u8])
    );
    assert_eq!(
        lc_value(chunk1.column(0), 1).as_deref(),
        Some(b"blue" as &[u8])
    );
}

#[test]
fn test_low_cardinality_wider_index() {
    // A u32-wide index array (width tag 2) must widen correctly into i32.
    let dictionary = ["a", "b", "c"];
    let indices = [2u64, 0, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(String)")
        .low_cardinality_string(&dictionary, &indices, 4)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Dictionary(d) => assert_eq!(d.indices, vec![2, 0, 1]),
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_low_cardinality_rejects_bad_key_version() {
    // Key version != 1 is rejected (only SharedDictionariesWithAdditionalKeys
    // is valid in Native).
    let mut payload = Vec::new();
    payload.extend_from_slice(&2u64.to_le_bytes()); // bad key version
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("lc", "LowCardinality(String)")
        .raw_bytes(&payload)
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidLowCardinality { .. })
    ));
}

#[test]
fn test_low_cardinality_rejects_global_dictionary_bit() {
    // NeedGlobalDictionaryBit must be clear in Native; set it and the decoder
    // must reject rather than misread.
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u64.to_le_bytes()); // key version
    let index_word = LC_HAS_ADDITIONAL_KEYS_BIT | LC_NEED_GLOBAL_DICTIONARY_BIT;
    payload.extend_from_slice(&index_word.to_le_bytes());
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("lc", "LowCardinality(String)")
        .raw_bytes(&payload)
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidLowCardinality { .. })
    ));
}

#[test]
fn test_low_cardinality_rejects_out_of_range_index() {
    // An index value past the block dictionary size is corrupt data.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("lc", "LowCardinality(String)")
        .low_cardinality_string(&["x", "y"], &[0, 5], 1)
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidLowCardinality { .. })
    ));
}

#[test]
fn test_low_cardinality_modern_framing_roundtrip() {
    // Full v26.6.1.1193 framing (BlockInfo + per-column custom-serialization
    // byte) ahead of the LowCardinality payload.
    let data = BlockBuilder::new()
        .revision(DBMS_TCP_PROTOCOL_VERSION)
        .header(1, 4)
        .column_header("lc", "LowCardinality(String)")
        .low_cardinality_string(&["alpha", "beta"], &[0, 1, 1, 0], 1)
        .build();

    let options = DecodeOptions {
        protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
    };
    let cb = decode_all_bytes(&data, &options).unwrap();
    let batch = &cb.chunks[0];
    let expected: [&[u8]; 4] = [b"alpha", b"beta", b"beta", b"alpha"];
    for (row, want) in expected.iter().enumerate() {
        assert_eq!(lc_value(batch.column(0), row).as_deref(), Some(*want));
    }
}

#[test]
fn test_block_end_scans_low_cardinality() {
    // The completeness scan must walk a LowCardinality column to the exact
    // block end, and report a one-byte-short buffer as "need more bytes".
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("lc", "LowCardinality(String)")
        .low_cardinality_string(&["user_1", "user_2"], &[0, 1, 0], 1)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

// -----------------------------------------------------------------------
// LowCardinality over non-String inner types
// -----------------------------------------------------------------------

/// Resolve a `UInt32`-valued dictionary column's row `i` to its value,
/// treating a null index as `None`, the way a consumer reads it.
fn lc_u32_value(col: &Column, row: usize) -> Option<u32> {
    match col {
        Column::Dictionary(d) => {
            if d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row)) {
                return None;
            }
            let idx = d.indices[row] as usize;
            match d.values.as_ref() {
                Column::UInt32(v) => Some(v.values[idx]),
                other => panic!("expected UInt32 dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

/// Resolve a `Date` (UInt16) dictionary column's row `i` to its value.
fn lc_date_value(col: &Column, row: usize) -> Option<u16> {
    match col {
        Column::Dictionary(d) => {
            if d.validity.as_ref().is_some_and(|bm| !bm.is_valid(row)) {
                return None;
            }
            let idx = d.indices[row] as usize;
            match d.values.as_ref() {
                Column::Date(v) => Some(v.values[idx]),
                other => panic!("expected Date dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_parse_ch_type_low_cardinality_non_string() {
    // The parser records any inner type; legality is enforced at decode.
    assert_eq!(
        parse_ch_type("LowCardinality(UInt32)"),
        Some(ChType::LowCardinality(Box::new(ChType::UInt32)))
    );
    assert_eq!(
        parse_ch_type("LowCardinality(Nullable(Date))"),
        Some(ChType::LowCardinality(Box::new(ChType::Nullable(
            Box::new(ChType::Date)
        ))))
    );
    // Display round-trips back through the parser.
    for t in [
        ChType::LowCardinality(Box::new(ChType::UInt32)),
        ChType::LowCardinality(Box::new(ChType::FixedString(4))),
        ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::Date)))),
    ] {
        assert_eq!(parse_ch_type(&t.to_string()), Some(t.clone()));
    }
}

#[test]
fn test_decode_low_cardinality_uint32() {
    // The dictionary values are a plain UInt32 column body (raw 4-byte LE),
    // decoded via the shared per-type body decoder. Slot 0 is the server's
    // reserved default (0) and the per-row indexes start at 1, mirroring the
    // String layout.
    let dictionary = [0u32, 13, 79, 4_294_967_295];
    let indices = [1u64, 2, 3, 1, 2, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(UInt32)")
        .low_cardinality_u32(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), 6);
            assert_eq!(d.null_count(), 0);
            assert!(d.validity.is_none());
            assert_eq!(d.indices, vec![1, 2, 3, 1, 2, 1]);
            match d.values.as_ref() {
                Column::UInt32(v) => {
                    assert_eq!(v.values, vec![0, 13, 79, 4_294_967_295]);
                    assert!(v.validity.is_none());
                }
                other => panic!("expected UInt32 values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    let expected = [13u32, 79, 4_294_967_295, 13, 79, 13];
    for (row, want) in expected.iter().enumerate() {
        assert_eq!(lc_u32_value(batch.column(0), row), Some(*want));
    }
}

#[test]
fn test_decode_low_cardinality_ipv4() {
    // IPv4 is allowlisted inside LowCardinality and its dictionary body is a
    // plain UInt32 column body (raw 4-byte LE), so it shares the UInt32 layout.
    // Slot 0 is the reserved default; the per-row indexes start at 1.
    let dictionary = [0u32, 0x7F00_0001, 0x0808_0808];
    let indices = [1u64, 2, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(IPv4)")
        .low_cardinality_u32(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![1, 2, 1]);
            assert!(d.validity.is_none());
            match d.values.as_ref() {
                Column::Ipv4(v) => {
                    assert_eq!(v.values, vec![0, 0x7F00_0001, 0x0808_0808]);
                }
                other => panic!("expected Ipv4 values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    // The scan must consume exactly the same bytes.
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_decode_low_cardinality_ipv6() {
    // IPv6 is allowlisted inside LowCardinality; its dictionary body is raw
    // 16-byte rows, the same shape as UUID and FixedString(16).
    let zero = [0u8; 16];
    let loopback = {
        let mut v = [0u8; 16];
        v[15] = 1;
        v
    };
    let dictionary = [zero, loopback];
    let indices = [1u64, 0, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(IPv6)")
        .low_cardinality_fixed16(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![1, 0, 1]);
            assert!(d.validity.is_none());
            match d.values.as_ref() {
                Column::Ipv6(v) => {
                    assert_eq!(v.width, 16);
                    assert_eq!(v.value(0), &zero);
                    assert_eq!(v.value(1), &loopback);
                }
                other => panic!("expected Ipv6 values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_scan_low_cardinality_rejects_bad_flags() {
    // The completeness scan must reject the same index-type-word bits the decode
    // rejects (NeedGlobalDictionaryBit set, HasAdditionalKeysBit clear), so a
    // hostile flags word cannot make the scan walk framing the decode refuses
    // and stall the StreamDecoder with a misleading truncation error.
    for index_word in [
        LC_HAS_ADDITIONAL_KEYS_BIT | LC_NEED_GLOBAL_DICTIONARY_BIT,
        0, // HasAdditionalKeysBit clear
    ] {
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u64.to_le_bytes()); // key version
        payload.extend_from_slice(&index_word.to_le_bytes());
        let data = BlockBuilder::new()
            .header(1, 1)
            .column_header("lc", "LowCardinality(String)")
            .raw_bytes(&payload)
            .build();
        assert!(
            matches!(
                block_end(&data, &DecodeOptions::default()),
                Err(DecodeError::InvalidLowCardinality { .. })
            ),
            "scan should reject index word {index_word:#x}"
        );
    }
}

#[test]
fn test_low_cardinality_bad_inner_rejected_at_zero_rows() {
    // A zero-row LowCardinality(Decimal(9, 4)) must be rejected as consistently
    // as the row-bearing form: the inner allowlist is checked at header time, so
    // the zero-row `empty_column` path no longer silently accepts an inner the
    // row-bearing decode rejects.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("lc", "LowCardinality(Decimal(9, 4))")
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_decode_low_cardinality_nullable_uint32() {
    // For a Nullable inner, dictionary slot 0 is the NULL sentinel (its
    // on-wire value is the inner default 0). Rows whose index is 0 are null;
    // the dictionary itself still decodes as a bare non-nullable UInt32.
    let dictionary = [0u32, 13, 79];
    let indices = [1u64, 0, 2, 0, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("lc", "LowCardinality(Nullable(UInt32))")
        .low_cardinality_u32(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), 5);
            assert_eq!(d.null_count(), 2);
            let bm = d.validity.as_ref().expect("nullable dictionary validity");
            assert!(bm.is_valid(0));
            assert!(!bm.is_valid(1));
            assert!(bm.is_valid(2));
            assert!(!bm.is_valid(3));
            assert!(bm.is_valid(4));
            // The dictionary values column carries no validity of its own.
            match d.values.as_ref() {
                Column::UInt32(v) => assert!(v.validity.is_none()),
                other => panic!("expected UInt32 values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    assert_eq!(lc_u32_value(batch.column(0), 0), Some(13));
    assert_eq!(lc_u32_value(batch.column(0), 1), None);
    assert_eq!(lc_u32_value(batch.column(0), 2), Some(79));
    assert_eq!(lc_u32_value(batch.column(0), 3), None);
    assert_eq!(lc_u32_value(batch.column(0), 4), Some(13));
}

#[test]
fn test_decode_low_cardinality_date() {
    // Date is a UInt16-backed number, a legal LowCardinality inner. The
    // dictionary is a plain Date (UInt16) column body.
    let dictionary = [0u16, 19737, 49710];
    let indices = [1u64, 2, 1, 0];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("d", "LowCardinality(Date)")
        .low_cardinality_u16(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert!(d.validity.is_none());
            match d.values.as_ref() {
                Column::Date(v) => assert_eq!(v.values, vec![0u16, 19737, 49710]),
                other => panic!("expected Date values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    let expected = [19737u16, 49710, 19737, 0];
    for (row, want) in expected.iter().enumerate() {
        assert_eq!(lc_date_value(batch.column(0), row), Some(*want));
    }
}

#[test]
fn test_decode_low_cardinality_nullable_date() {
    // Nullable(Date) inner: index 0 is the NULL sentinel.
    let dictionary = [0u16, 19737, 49710];
    let indices = [0u64, 1, 0, 2];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("d", "LowCardinality(Nullable(Date))")
        .low_cardinality_u16(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    assert_eq!(lc_date_value(batch.column(0), 0), None);
    assert_eq!(lc_date_value(batch.column(0), 1), Some(19737));
    assert_eq!(lc_date_value(batch.column(0), 2), None);
    assert_eq!(lc_date_value(batch.column(0), 3), Some(49710));
    match batch.column(0) {
        Column::Dictionary(d) => assert_eq!(d.null_count(), 2),
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_decode_low_cardinality_fixed_string() {
    // FixedString(N) is a legal inner; the dictionary body is raw N-byte
    // entries with no length prefix, decoded as a FixedBinary values column.
    let dictionary: [&[u8]; 3] = [b"\0\0\0\0", b"abcd", b"wxyz"];
    let indices = [1u64, 2, 1];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("fs", "LowCardinality(FixedString(4))")
        .low_cardinality_fixed(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => match d.values.as_ref() {
            Column::FixedBinary(v) => {
                assert_eq!(v.width, 4);
                assert_eq!(v.len(), 3);
                assert_eq!(v.value(0), b"\0\0\0\0");
                assert_eq!(v.value(1), b"abcd");
                assert_eq!(v.value(2), b"wxyz");
                let idx1 = d.indices[0] as usize;
                assert_eq!(v.value(idx1), b"abcd");
            }
            other => panic!("expected FixedBinary values, got {other:?}"),
        },
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_decode_low_cardinality_bool_and_date32_inner() {
    // Bool (UInt8-backed) and Date32 (Int32-backed) are number-backed types
    // whose canBeInsideLowCardinality is true at v26.6.1.1193-stable, so both
    // are legal LowCardinality inners and decode through the shared body.
    let data = BlockBuilder::new()
        .header(2, 3)
        .column_header("b", "LowCardinality(Bool)")
        .low_cardinality_block(2, &[0u8, 1], &[0, 1, 1], 1)
        .column_header("d32", "LowCardinality(Date32)")
        .low_cardinality_block(
            2,
            &[(-7227i32).to_le_bytes(), 84370i32.to_le_bytes()].concat(),
            &[1, 0, 1],
            1,
        )
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => match d.values.as_ref() {
            Column::Bool(v) => {
                assert_eq!(v.len(), 2);
                assert!(!v.get(0));
                assert!(v.get(1));
                // Rows resolve to false, true, true.
                assert!(!v.get(d.indices[0] as usize));
                assert!(v.get(d.indices[1] as usize));
            }
            other => panic!("expected Bool values, got {other:?}"),
        },
        other => panic!("expected Dictionary, got {other:?}"),
    }
    match batch.column(1) {
        Column::Dictionary(d) => match d.values.as_ref() {
            Column::Date32(v) => {
                assert_eq!(v.values, vec![-7227i32, 84370]);
                assert_eq!(v.values[d.indices[0] as usize], 84370);
            }
            other => panic!("expected Date32 values, got {other:?}"),
        },
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_decode_low_cardinality_time() {
    // Time is Int32-number-backed and legal inside LowCardinality. The
    // dictionary body is the same raw signed seconds run as a plain Time.
    let dictionary = [0i32, -13, 79];
    let dictionary_bytes: Vec<u8> = dictionary
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("t", "LowCardinality(Time)")
        .low_cardinality_block(3, &dictionary_bytes, &[1, 2, 1, 0], 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![1, 2, 1, 0]);
            match d.values.as_ref() {
                Column::Time(values) => assert_eq!(values.values, dictionary),
                other => panic!("expected Time dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_low_cardinality_rejects_datetime64_inner() {
    // DateTime64 is DataTypeDecimalBase, whose canBeInsideLowCardinality is
    // false, so the server never emits LowCardinality(DateTime64). The crate
    // decodes DateTime64 as an ordinary column but must reject it as a LC
    // inner rather than mis-decode a payload the server cannot produce.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("lc", "LowCardinality(DateTime64(3))")
        // A well-formed-looking prefix and index word; decode must reject on
        // the inner type before consuming the dictionary body.
        .low_cardinality_block(1, &0i64.to_le_bytes(), &[0], 1)
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
    // The completeness scan must reject it identically, so block_end agrees
    // with decode on which columns are accepted.
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_low_cardinality_rejects_time64_inner() {
    // Time64 is DecimalBase-backed, so canBeInsideLowCardinality is false.
    // Decode and the completeness scan must reject the same header.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("lc", "LowCardinality(Time64(3))")
        .low_cardinality_block(1, &0i64.to_le_bytes(), &[0], 1)
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_decode_low_cardinality_numeric_zero_rows() {
    // A zero-row block reads no LowCardinality prefix or data for a numeric
    // inner either; it contributes the schema and an empty dictionary column
    // whose empty values carry the inner type.
    let data = BlockBuilder::new()
        .header(2, 0)
        .column_header("lc", "LowCardinality(UInt32)")
        .column_header("lcn", "LowCardinality(Nullable(UInt32))")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::UInt32))
    );
    assert_eq!(
        cb.schema.fields[1].ch_type,
        ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(ChType::UInt32))))
    );
}

#[test]
fn test_multi_block_low_cardinality_uint32_separate_dictionaries() {
    // Each Native block carries its own per-block UInt32 dictionary and may
    // use a different index width; the blocks stay separate chunks and each
    // resolves against its own dictionary.
    let mut data = BlockBuilder::new()
        .header(1, 3)
        .column_header("lc", "LowCardinality(UInt32)")
        .low_cardinality_u32(&[0, 13, 79], &[1, 2, 1], 1)
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 2)
            .column_header("lc", "LowCardinality(UInt32)")
            .low_cardinality_u32(&[0, 4_294_967_295], &[1, 1], 2)
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
    assert_eq!(lc_u32_value(cb.chunks[0].column(0), 0), Some(13));
    assert_eq!(lc_u32_value(cb.chunks[0].column(0), 1), Some(79));
    assert_eq!(lc_u32_value(cb.chunks[0].column(0), 2), Some(13));
    assert_eq!(lc_u32_value(cb.chunks[1].column(0), 0), Some(4_294_967_295));
    assert_eq!(lc_u32_value(cb.chunks[1].column(0), 1), Some(4_294_967_295));
}

#[test]
fn test_block_end_scans_numeric_low_cardinality() {
    // The completeness scan must walk a numeric LowCardinality column (raw
    // fixed-width dictionary body, no varint prefixes) to the exact block
    // end, and report a one-byte-short buffer as "need more bytes".
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("lc", "LowCardinality(UInt32)")
        .low_cardinality_u32(&[0, 13, 79], &[1, 2, 1], 1)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

// -----------------------------------------------------------------------
// UUID / IPv4 / IPv6
// -----------------------------------------------------------------------

/// The 16 wire bytes for RFC UUID `00112233-4455-6677-8899-aabbccddeeff`.
///
/// ClickHouse `SerializationUUID` dumps the UInt128 POD (items[0] then
/// items[1], each little-endian on LE servers), which is NOT RFC-4122 byte
/// order. The wire->RFC mapping a binding applies is `rfc[i] = wire[7-i]` for
/// i in 0..7 and `rfc[i] = wire[23-i]` for i in 8..15 (reverse the first 8
/// bytes, reverse the last 8). The decoder itself does no reordering; these
/// are the bytes the server emits and the bytes the decoder must return.
/// Confirmed against the live server at v26.6.1.1193-stable.
const UUID_00112233_WIRE: [u8; 16] = [
    0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88,
];

#[test]
fn test_parse_ch_type_uuid_ipv4_ipv6() {
    assert_eq!(parse_ch_type("UUID"), Some(ChType::Uuid));
    assert_eq!(parse_ch_type("IPv4"), Some(ChType::Ipv4));
    assert_eq!(parse_ch_type("IPv6"), Some(ChType::Ipv6));
}

#[test]
fn test_ch_type_display_round_trips_uuid_ipv4_ipv6() {
    for t in [ChType::Uuid, ChType::Ipv4, ChType::Ipv6] {
        assert_eq!(parse_ch_type(&t.to_string()), Some(t.clone()));
    }
    assert_eq!(ChType::Uuid.to_string(), "UUID");
    assert_eq!(ChType::Ipv4.to_string(), "IPv4");
    assert_eq!(ChType::Ipv6.to_string(), "IPv6");
}

#[test]
fn test_decode_uuid_byte_order_passthrough() {
    // Decode is raw passthrough: the 16 wire bytes for RFC UUID
    // 00112233-4455-6677-8899-aabbccddeeff come back unchanged, in wire order
    // (NOT RFC order). A binding applies the wire->RFC mapping; the core does
    // not reorder. The second row is all-zero (the nil UUID).
    let nil = [0u8; 16];
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("u", "UUID")
        .fixed16_data(&[UUID_00112233_WIRE, nil])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Uuid(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.len(), 2);
            assert_eq!(c.value(0), UUID_00112233_WIRE);
            assert_eq!(c.value(1), nil);
        }
        other => panic!("expected Uuid, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_uuid() {
    // Nullable(UUID): null map first, then the 16-byte rows (null rows still
    // carry placeholder bytes on the wire).
    let nil = [0u8; 16];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("u", "Nullable(UUID)")
        .null_map(&[false, true, false])
        .fixed16_data(&[UUID_00112233_WIRE, nil, [0xffu8; 16]])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Uuid(c) => {
            assert_eq!(c.len(), 3);
            assert_eq!(c.null_count(), 1);
            assert_eq!(c.value(0), UUID_00112233_WIRE);
            assert_eq!(c.value(2), [0xffu8; 16]);
        }
        other => panic!("expected Uuid, got {other:?}"),
    }
    assert!(batch.column(0).validity().unwrap().is_valid(0));
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
    assert!(batch.column(0).validity().unwrap().is_valid(2));
}

#[test]
fn test_decode_uuid_zero_rows() {
    // A zero-row UUID block contributes the schema and an empty width-16
    // column.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("u", "UUID")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.schema.fields[0].ch_type, ChType::Uuid);
}

#[test]
fn test_multi_block_uuid_kept_as_chunks() {
    // UUID blocks stay separate chunks, never concatenated.
    let a = [0x01u8; 16];
    let b = [0x02u8; 16];
    let c = [0x03u8; 16];
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("u", "UUID")
        .fixed16_data(&[a, b])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("u", "UUID")
            .fixed16_data(&[c])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);
    match cb.chunks[0].column(0) {
        Column::Uuid(col) => {
            assert_eq!(col.value(0), a);
            assert_eq!(col.value(1), b);
        }
        other => panic!("expected Uuid, got {other:?}"),
    }
    match cb.chunks[1].column(0) {
        Column::Uuid(col) => assert_eq!(col.value(0), c),
        other => panic!("expected Uuid, got {other:?}"),
    }
}

#[test]
fn test_decode_ipv4_plain() {
    // IPv4 is a UInt32 in bulk; reading 4 LE bytes yields the standard IPv4
    // numeric value (a<<24 | b<<16 | c<<8 | d). 192.0.2.235 = 3221226219.
    let values = [0u32, 3221226219, 169090600, u32::MAX];
    let data = BlockBuilder::new()
        .header(1, values.len())
        .column_header("ip", "IPv4")
        .ipv4_data(&values)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Ipv4(c) => {
            assert_eq!(c.values, vec![0, 3221226219, 169090600, u32::MAX]);
            assert!(c.validity.is_none());
        }
        other => panic!("expected Ipv4, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_ipv4() {
    // Nullable(IPv4): null map first, then the UInt32 payload.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("ip", "Nullable(IPv4)")
        .null_map(&[false, true, false])
        .ipv4_data(&[3221226219, 0, 169090600])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Ipv4(c) => {
            assert_eq!(c.null_count(), 1);
            assert_eq!(c.values, vec![3221226219, 0, 169090600]);
        }
        other => panic!("expected Ipv4, got {other:?}"),
    }
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
}

#[test]
fn test_decode_ipv4_zero_rows() {
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("ip", "IPv4")
        .build();
    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.schema.fields[0].ch_type, ChType::Ipv4);
}

#[test]
fn test_multi_block_ipv4_kept_as_chunks() {
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("ip", "IPv4")
        .ipv4_data(&[0, 3221226219])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 3)
            .column_header("ip", "IPv4")
            .ipv4_data(&[169090600, u32::MAX, 13])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
    match cb.chunks[0].column(0) {
        Column::Ipv4(c) => assert_eq!(c.values, vec![0, 3221226219]),
        other => panic!("expected Ipv4, got {other:?}"),
    }
    match cb.chunks[1].column(0) {
        Column::Ipv4(c) => assert_eq!(c.values, vec![169090600, u32::MAX, 13]),
        other => panic!("expected Ipv4, got {other:?}"),
    }
}

#[test]
fn test_decode_ipv6_plain() {
    // IPv6 is 16 raw bytes in network byte order, passed through verbatim.
    // 2001:db8::68 and the all-zero (::) address.
    let db8: [u8; 16] = [
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x68,
    ];
    let unspecified = [0u8; 16];
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("ip", "IPv6")
        .fixed16_data(&[db8, unspecified])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Ipv6(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.len(), 2);
            assert_eq!(c.value(0), db8);
            assert_eq!(c.value(1), unspecified);
        }
        other => panic!("expected Ipv6, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_ipv6() {
    let db8: [u8; 16] = [
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x68,
    ];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("ip", "Nullable(IPv6)")
        .null_map(&[false, true, false])
        .fixed16_data(&[db8, [0u8; 16], [0xffu8; 16]])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Ipv6(c) => {
            assert_eq!(c.null_count(), 1);
            assert_eq!(c.value(0), db8);
            assert_eq!(c.value(2), [0xffu8; 16]);
        }
        other => panic!("expected Ipv6, got {other:?}"),
    }
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
}

#[test]
fn test_decode_ipv6_zero_rows() {
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("ip", "IPv6")
        .build();
    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.schema.fields[0].ch_type, ChType::Ipv6);
}

#[test]
fn test_multi_block_ipv6_kept_as_chunks() {
    let a = [0x0au8; 16];
    let b = [0x0bu8; 16];
    let c = [0x0cu8; 16];
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("ip", "IPv6")
        .fixed16_data(&[a, b])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("ip", "IPv6")
            .fixed16_data(&[c])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);
    match cb.chunks[1].column(0) {
        Column::Ipv6(col) => assert_eq!(col.value(0), c),
        other => panic!("expected Ipv6, got {other:?}"),
    }
}

#[test]
fn test_decode_low_cardinality_uuid() {
    // UUID is a legal LowCardinality inner (unconditionally allowed by the
    // server). The dictionary body is raw 16-byte UUID rows, decoded as a
    // FixedBinary (width 16) values column via the shared per-type body. Slot
    // 0 is the reserved default (all-zero); the per-row indexes start at 1.
    let nil = [0u8; 16];
    let one = [0x11u8; 16];
    let dictionary = [nil, UUID_00112233_WIRE, one];
    let indices = [1u64, 2, 1, 2];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("u", "LowCardinality(UUID)")
        .low_cardinality_fixed16(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.len(), 4);
            assert!(d.validity.is_none());
            assert_eq!(d.indices, vec![1, 2, 1, 2]);
            match d.values.as_ref() {
                Column::Uuid(v) => {
                    assert_eq!(v.width, 16);
                    assert_eq!(v.len(), 3);
                    assert_eq!(v.value(0), nil);
                    assert_eq!(v.value(1), UUID_00112233_WIRE);
                    assert_eq!(v.value(2), one);
                    // Row 0 resolves to the 00112233... UUID, in wire order.
                    assert_eq!(v.value(d.indices[0] as usize), UUID_00112233_WIRE);
                }
                other => panic!("expected Uuid dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_decode_low_cardinality_nullable_uuid() {
    // LowCardinality(Nullable(UUID)): dictionary slot 0 is the NULL sentinel
    // (all-zero on the wire). Rows whose index is 0 are null.
    let nil = [0u8; 16];
    let dictionary = [nil, UUID_00112233_WIRE, [0x22u8; 16]];
    let indices = [1u64, 0, 2, 0];
    let data = BlockBuilder::new()
        .header(1, indices.len())
        .column_header("u", "LowCardinality(Nullable(UUID))")
        .low_cardinality_fixed16(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.null_count(), 2);
            let bm = d.validity.as_ref().expect("nullable dictionary validity");
            assert!(bm.is_valid(0));
            assert!(!bm.is_valid(1));
            assert!(bm.is_valid(2));
            assert!(!bm.is_valid(3));
            match d.values.as_ref() {
                Column::Uuid(v) => {
                    assert_eq!(v.value(d.indices[0] as usize), UUID_00112233_WIRE)
                }
                other => panic!("expected Uuid values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

#[test]
fn test_block_end_scans_uuid_ipv4_ipv6() {
    // The completeness scan must walk UUID (16/row), IPv4 (4/row), and IPv6
    // (16/row) to the exact block end, and report a one-byte-short buffer as
    // "need more bytes".
    let data = BlockBuilder::new()
        .header(3, 2)
        .column_header("u", "UUID")
        .fixed16_data(&[UUID_00112233_WIRE, [0u8; 16]])
        .column_header("ip4", "IPv4")
        .ipv4_data(&[3221226219, 0])
        .column_header("ip6", "IPv6")
        .fixed16_data(&[[0x20u8; 16], [0u8; 16]])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

// -----------------------------------------------------------------------
// Enum8 / Enum16
// -----------------------------------------------------------------------

#[test]
fn test_parse_ch_type_enum8() {
    // Enum8 maps names to Int8 values; the parser preserves order and
    // accepts negatives. Values must fit i8.
    assert_eq!(
        parse_ch_type("Enum8('pending' = 1, 'active' = 2, 'closed' = -1)"),
        Some(ChType::Enum8 {
            variants: vec![
                ("pending".to_string(), 1),
                ("active".to_string(), 2),
                ("closed".to_string(), -1),
            ],
        })
    );
    // i8 range edges decode; one past the edge is rejected.
    assert_eq!(
        parse_ch_type("Enum8('lo' = -128, 'hi' = 127)"),
        Some(ChType::Enum8 {
            variants: vec![("lo".to_string(), -128), ("hi".to_string(), 127)],
        })
    );
    assert_eq!(parse_ch_type("Enum8('over' = 128)"), None);
    assert_eq!(parse_ch_type("Enum8('under' = -129)"), None);
}

#[test]
fn test_parse_ch_type_enum16() {
    // Enum16 maps names to Int16 values; same parser, wider range.
    assert_eq!(
        parse_ch_type("Enum16('pending' = 1, 'active' = 2, 'closed' = -1)"),
        Some(ChType::Enum16 {
            variants: vec![
                ("pending".to_string(), 1),
                ("active".to_string(), 2),
                ("closed".to_string(), -1),
            ],
        })
    );
    assert_eq!(
        parse_ch_type("Enum16('lo' = -32768, 'hi' = 32767)"),
        Some(ChType::Enum16 {
            variants: vec![("lo".to_string(), -32768), ("hi".to_string(), 32767)],
        })
    );
    assert_eq!(parse_ch_type("Enum16('over' = 32768)"), None);
}

#[test]
fn test_parse_ch_type_enum_malformed_rejected() {
    // Each of these is a syntax error on the untrusted type string and must
    // surface as None (UnsupportedType), never a panic.
    for bad in [
        "Enum8('pending' 1)",       // missing '='
        "Enum8(pending = 1)",       // name not quoted
        "Enum8('pending' = )",      // missing value
        "Enum8('pending' = abc)",   // non-numeric value
        "Enum8('unterminated = 1)", // name never closed
        "Enum8('bad\\x' = 1)",      // unknown escape
        "Enum8('a' = 1 'b' = 2)",   // missing comma between pairs
    ] {
        assert_eq!(parse_ch_type(bad), None, "expected None for {bad:?}");
    }
}

#[test]
fn test_enum_type_string_round_trips_escaping_and_order() {
    // Display is the inverse of the parser, so parse(display(t)) == t for the
    // tricky cases: a name containing a comma and an equals sign (both pass
    // through unescaped on the wire), a name with an escaped quote and a
    // backslash, negative values, and ascending multi-value ordering.
    let cases = vec![
        ChType::Enum8 {
            variants: vec![
                ("closed".to_string(), -1),
                ("pending".to_string(), 1),
                ("active".to_string(), 2),
            ],
        },
        // A name with a comma and an equals sign: the parser cannot split on
        // those, it walks the quotes. Display escapes neither.
        ChType::Enum8 {
            variants: vec![("a,b=c".to_string(), 7)],
        },
        // A name with an escaped quote and a backslash.
        ChType::Enum16 {
            variants: vec![
                ("x'y".to_string(), -3),
                ("back\\slash".to_string(), 4),
                ("tab\tnl\n".to_string(), 9),
            ],
        },
    ];
    for t in cases {
        let rendered = t.to_string();
        assert_eq!(
            parse_ch_type(&rendered),
            Some(t.clone()),
            "Display output {rendered:?} did not parse back to {t:?}"
        );
    }
    // Pin the exact rendering of the comma/equals case so a regression in the
    // escaping (e.g. accidentally escaping `,` or `=`) is caught.
    assert_eq!(
        ChType::Enum8 {
            variants: vec![("a,b=c".to_string(), 7)],
        }
        .to_string(),
        "Enum8('a,b=c' = 7)"
    );
    // And the escaped quote / backslash rendering.
    assert_eq!(
        ChType::Enum8 {
            variants: vec![("x'y".to_string(), 1), ("a\\b".to_string(), 2)],
        }
        .to_string(),
        "Enum8('x\\'y' = 1, 'a\\\\b' = 2)"
    );
}

#[test]
fn test_decode_enum8_plain() {
    // Enum8 is raw Int8 on the wire (1 byte/row); the name->value map is in
    // the ChType only, never in the per-row data.
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header(
            "status",
            "Enum8('pending' = 1, 'active' = 2, 'closed' = -1)",
        )
        .int8_data(&[1, 2, -1, 1])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Enum8(c) => {
            assert_eq!(c.values, vec![1i8, 2, -1, 1]);
            assert!(c.validity.is_none());
        }
        other => panic!("expected Enum8, got {other:?}"),
    }
    // The variants live in the schema ChType.
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Enum8 {
            variants: vec![
                ("pending".to_string(), 1),
                ("active".to_string(), 2),
                ("closed".to_string(), -1),
            ],
        }
    );
}

#[test]
fn test_decode_enum16_plain() {
    // Enum16 is raw Int16 on the wire (2 bytes/row).
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header(
            "status",
            "Enum16('pending' = 1, 'active' = 2, 'closed' = -1)",
        )
        .int16_data(&[1, -1, 2])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Enum16(c) => assert_eq!(c.values, vec![1i16, -1, 2]),
        other => panic!("expected Enum16, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_enum8() {
    // Nullable(Enum8): the null map first, then the Int8 buffer, exactly like
    // Nullable(Int8). Null rows still carry a placeholder byte on the wire.
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("status", "Nullable(Enum8('pending' = 1, 'active' = 2))")
        .null_map(&[false, true, false, true])
        .int8_data(&[1, 0, 2, 0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Enum8(c) => {
            assert_eq!(c.null_count(), 2);
            assert_eq!(c.values, vec![1i8, 0, 2, 0]);
        }
        other => panic!("expected Enum8, got {other:?}"),
    }
    assert!(batch.column(0).validity().unwrap().is_valid(0));
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Nullable(Box::new(ChType::Enum8 {
            variants: vec![("pending".to_string(), 1), ("active".to_string(), 2)],
        }))
    );
}

#[test]
fn test_decode_enum_zero_rows() {
    // A zero-row block carrying an Enum8 and an Enum16 contributes the schema
    // (variants and all) but no chunks, and the empty columns have length 0.
    let data = BlockBuilder::new()
        .header(2, 0)
        .column_header("e8", "Enum8('a' = 1, 'b' = 2)")
        .column_header("e16", "Enum16('a' = 1, 'b' = -2)")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 2);
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::Enum8 {
            variants: vec![("a".to_string(), 1), ("b".to_string(), 2)],
        }
    );
    assert_eq!(
        cb.schema.fields[1].ch_type,
        ChType::Enum16 {
            variants: vec![("a".to_string(), 1), ("b".to_string(), -2)],
        }
    );
}

#[test]
fn test_multi_block_enum8_kept_as_chunks() {
    // Enum8 blocks stay separate chunks, never concatenated.
    let type_str = "Enum8('pending' = 1, 'active' = 2, 'closed' = -1)";
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("status", type_str)
        .int8_data(&[1, 2])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 3)
            .column_header("status", type_str)
            .int8_data(&[-1, 1, 2])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 5);
    match cb.chunks[0].column(0) {
        Column::Enum8(c) => assert_eq!(c.values, vec![1i8, 2]),
        other => panic!("expected Enum8, got {other:?}"),
    }
    match cb.chunks[1].column(0) {
        Column::Enum8(c) => assert_eq!(c.values, vec![-1i8, 1, 2]),
        other => panic!("expected Enum8, got {other:?}"),
    }
}

#[test]
fn test_block_end_scans_enum_columns() {
    // The completeness scan must walk Enum8 (1/row) and Enum16 (2/row) to the
    // exact block end, and report a one-byte-short buffer as "need more bytes".
    let data = BlockBuilder::new()
        .header(2, 3)
        .column_header("e8", "Enum8('a' = 1, 'b' = 2)")
        .int8_data(&[1, 2, 1])
        .column_header("e16", "Enum16('a' = 1, 'b' = -2)")
        .int16_data(&[1, -2, 1])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

// -----------------------------------------------------------------------
// Decimal
// -----------------------------------------------------------------------

#[test]
fn test_decimal_bits_from_precision_boundaries() {
    // Width is derived from P alone (server `createDecimal`): the byte width
    // jumps at every boundary. Pin each edge so a derivation regression is
    // caught.
    assert_eq!(decimal_bits_from_precision(1), Some(32));
    assert_eq!(decimal_bits_from_precision(9), Some(32));
    assert_eq!(decimal_bits_from_precision(10), Some(64));
    assert_eq!(decimal_bits_from_precision(18), Some(64));
    assert_eq!(decimal_bits_from_precision(19), Some(128));
    assert_eq!(decimal_bits_from_precision(38), Some(128));
    assert_eq!(decimal_bits_from_precision(39), Some(256));
    assert_eq!(decimal_bits_from_precision(76), Some(256));
    // Out of range: P = 0 and P > 76 have no backing integer.
    assert_eq!(decimal_bits_from_precision(0), None);
    assert_eq!(decimal_bits_from_precision(77), None);
}

#[test]
fn test_parse_ch_type_decimal() {
    // The server emits the canonical `Decimal(P, S)`; the parser derives the
    // bit width from P and validates 0 <= S <= P, 1 <= P <= 76.
    assert_eq!(
        parse_ch_type("Decimal(9, 4)"),
        Some(ChType::Decimal {
            precision: 9,
            scale: 4,
            bits: 32,
        })
    );
    assert_eq!(
        parse_ch_type("Decimal(18, 0)"),
        Some(ChType::Decimal {
            precision: 18,
            scale: 0,
            bits: 64,
        })
    );
    assert_eq!(
        parse_ch_type("Decimal(38, 38)"),
        Some(ChType::Decimal {
            precision: 38,
            scale: 38,
            bits: 128,
        })
    );
    assert_eq!(
        parse_ch_type("Decimal(76, 50)"),
        Some(ChType::Decimal {
            precision: 76,
            scale: 50,
            bits: 256,
        })
    );
    // Width derivation at every boundary, parsed end to end.
    for (p, bits) in [
        (1u8, 32u16),
        (9, 32),
        (10, 64),
        (18, 64),
        (19, 128),
        (38, 128),
        (39, 256),
        (76, 256),
    ] {
        assert_eq!(
            parse_ch_type(&format!("Decimal({p}, 0)")),
            Some(ChType::Decimal {
                precision: p,
                scale: 0,
                bits,
            }),
            "precision {p} should derive {bits} bits"
        );
    }
}

#[test]
fn test_parse_ch_type_decimal_invalid_rejected() {
    // Each is rejected as None (UnsupportedType), never a panic, on the
    // untrusted type string.
    for bad in [
        "Decimal(0, 0)",     // P below 1: no backing integer
        "Decimal(77, 0)",    // P above 76
        "Decimal(9, 10)",    // S > P
        "Decimal(5)",        // missing scale (server always emits both)
        "Decimal(a, 2)",     // non-numeric precision
        "Decimal(9, b)",     // non-numeric scale
        "Decimal(9, )",      // missing scale value
        "Decimal(, 2)",      // missing precision value
        "Decimal(9, 4, 32)", // extra field is not the canonical form
    ] {
        assert_eq!(parse_ch_type(bad), None, "expected None for {bad:?}");
    }
}

#[test]
fn test_decimal_type_string_round_trips() {
    // Display emits the canonical `Decimal(P, S)` the server writes, so
    // parse(display(t)) == t at every width, including S = 0 and S = P.
    for t in [
        ChType::Decimal {
            precision: 9,
            scale: 4,
            bits: 32,
        },
        ChType::Decimal {
            precision: 10,
            scale: 0,
            bits: 64,
        },
        ChType::Decimal {
            precision: 38,
            scale: 38,
            bits: 128,
        },
        ChType::Decimal {
            precision: 50,
            scale: 10,
            bits: 256,
        },
    ] {
        let rendered = t.to_string();
        assert_eq!(
            parse_ch_type(&rendered),
            Some(t.clone()),
            "Display {rendered:?} did not parse back to {t:?}"
        );
    }
}

#[test]
fn test_decode_decimal32_plain() {
    // Decimal32 is a raw 4-byte LE Int32 per row. Include a negative unscaled
    // value (-1) to exercise two's-complement: it must decode to all-0xFF
    // bytes of the width. Precision/scale live in the ChType, not the data.
    let rows: Vec<[u8; 4]> = vec![
        13i32.to_le_bytes(),
        (-1i32).to_le_bytes(),
        0i32.to_le_bytes(),
        i32::MIN.to_le_bytes(),
    ];
    let row_refs: Vec<&[u8]> = rows.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("d", "Decimal(9, 4)")
        .decimal_data(&row_refs, 4)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.width, 4);
            assert_eq!(c.precision, 9);
            assert_eq!(c.scale, 4);
            assert_eq!(c.len(), 4);
            // The raw unscaled integers, read back as i32 LE.
            assert_eq!(i32::from_le_bytes(c.value(0).try_into().unwrap()), 13);
            assert_eq!(i32::from_le_bytes(c.value(1).try_into().unwrap()), -1);
            // -1 is all-0xFF bytes of the width (two's-complement).
            assert_eq!(c.value(1), &[0xFF, 0xFF, 0xFF, 0xFF]);
            assert_eq!(i32::from_le_bytes(c.value(2).try_into().unwrap()), 0);
            assert_eq!(i32::from_le_bytes(c.value(3).try_into().unwrap()), i32::MIN);
            assert!(c.validity.is_none());
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Decimal {
            precision: 9,
            scale: 4,
            bits: 32,
        }
    );
}

#[test]
fn test_decode_decimal64_plain() {
    // Decimal64 is a raw 8-byte LE Int64 per row, including a negative.
    let rows: Vec<[u8; 8]> = vec![
        79i64.to_le_bytes(),
        (-1i64).to_le_bytes(),
        i64::MAX.to_le_bytes(),
    ];
    let row_refs: Vec<&[u8]> = rows.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("d", "Decimal(18, 9)")
        .decimal_data(&row_refs, 8)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.width, 8);
            assert_eq!(c.precision, 18);
            assert_eq!(c.scale, 9);
            assert_eq!(i64::from_le_bytes(c.value(0).try_into().unwrap()), 79);
            assert_eq!(i64::from_le_bytes(c.value(1).try_into().unwrap()), -1);
            assert_eq!(c.value(1), &[0xFF; 8]);
            assert_eq!(i64::from_le_bytes(c.value(2).try_into().unwrap()), i64::MAX);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
}

#[test]
fn test_decode_decimal128_plain() {
    // Decimal128 is a raw 16-byte LE Int128 per row. The core has no native
    // i128, so assert the raw little-endian byte pattern directly. An
    // unscaled 1 is byte 0 = 0x01, the rest zero; an unscaled -1 is all
    // 0xFF (two's-complement of the full 16-byte width).
    let one: [u8; 16] = {
        let mut b = [0u8; 16];
        b[0] = 0x01;
        b
    };
    let neg_one: [u8; 16] = [0xFF; 16];
    let zero: [u8; 16] = [0u8; 16];
    let row_refs: Vec<&[u8]> = vec![one.as_slice(), neg_one.as_slice(), zero.as_slice()];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("d", "Decimal(20, 2)")
        .decimal_data(&row_refs, 16)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.precision, 20);
            assert_eq!(c.scale, 2);
            assert_eq!(c.value(0), one);
            assert_eq!(c.value(1), neg_one);
            assert_eq!(c.value(2), zero);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Decimal {
            precision: 20,
            scale: 2,
            bits: 128,
        }
    );
}

#[test]
fn test_decode_decimal256_plain() {
    // Decimal256 is a raw 32-byte LE Int256 per row. Assert the raw
    // little-endian byte pattern: an unscaled 1, an unscaled -1 (all 0xFF),
    // and a larger value 258 (0x0102 little-endian -> bytes 0x02, 0x01).
    let one: [u8; 32] = {
        let mut b = [0u8; 32];
        b[0] = 0x01;
        b
    };
    let neg_one: [u8; 32] = [0xFF; 32];
    let two_fifty_eight: [u8; 32] = {
        let mut b = [0u8; 32];
        b[0] = 0x02;
        b[1] = 0x01;
        b
    };
    let row_refs: Vec<&[u8]> = vec![
        one.as_slice(),
        neg_one.as_slice(),
        two_fifty_eight.as_slice(),
    ];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("d", "Decimal(50, 10)")
        .decimal_data(&row_refs, 32)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.width, 32);
            assert_eq!(c.precision, 50);
            assert_eq!(c.scale, 10);
            assert_eq!(c.value(0), one);
            assert_eq!(c.value(1), neg_one);
            assert_eq!(c.value(2), two_fifty_eight);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Decimal {
            precision: 50,
            scale: 10,
            bits: 256,
        }
    );
}

#[test]
fn test_decode_nullable_decimal64() {
    // Nullable(Decimal64): the null map first, then the Int64 buffer, exactly
    // like Nullable(Int64). Null rows still carry a placeholder on the wire.
    let rows: Vec<[u8; 8]> = vec![
        13i64.to_le_bytes(),
        0i64.to_le_bytes(),
        (-1i64).to_le_bytes(),
        0i64.to_le_bytes(),
    ];
    let row_refs: Vec<&[u8]> = rows.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("d", "Nullable(Decimal(18, 9))")
        .null_map(&[false, true, false, true])
        .decimal_data(&row_refs, 8)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.null_count(), 2);
            assert_eq!(i64::from_le_bytes(c.value(0).try_into().unwrap()), 13);
            assert_eq!(i64::from_le_bytes(c.value(2).try_into().unwrap()), -1);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
    assert!(batch.column(0).validity().unwrap().is_valid(0));
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Nullable(Box::new(ChType::Decimal {
            precision: 18,
            scale: 9,
            bits: 64,
        }))
    );
}

#[test]
fn test_decode_decimal_zero_rows() {
    // A zero-row block carrying decimals of every width contributes the
    // schema but no chunks; the empty columns have length 0 and keep their
    // width/precision/scale.
    let data = BlockBuilder::new()
        .header(4, 0)
        .column_header("d32", "Decimal(9, 2)")
        .column_header("d64", "Decimal(18, 4)")
        .column_header("d128", "Decimal(38, 10)")
        .column_header("d256", "Decimal(76, 20)")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 4);
    for (i, (p, s, bits)) in [(9u8, 2u8, 32u16), (18, 4, 64), (38, 10, 128), (76, 20, 256)]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            cb.schema.fields[i].ch_type,
            ChType::Decimal {
                precision: p,
                scale: s,
                bits,
            }
        );
    }
}

#[test]
fn test_multi_block_decimal_kept_as_chunks() {
    // Decimal blocks stay separate chunks, never concatenated.
    let block_a: Vec<[u8; 4]> = vec![13i32.to_le_bytes(), (-1i32).to_le_bytes()];
    let block_b: Vec<[u8; 4]> = vec![79i32.to_le_bytes()];
    let refs_a: Vec<&[u8]> = block_a.iter().map(|r| r.as_slice()).collect();
    let refs_b: Vec<&[u8]> = block_b.iter().map(|r| r.as_slice()).collect();
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("d", "Decimal(9, 4)")
        .decimal_data(&refs_a, 4)
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("d", "Decimal(9, 4)")
            .decimal_data(&refs_b, 4)
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);
    match cb.chunks[0].column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.len(), 2);
            assert_eq!(i32::from_le_bytes(c.value(1).try_into().unwrap()), -1);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
    match cb.chunks[1].column(0) {
        Column::Decimal(c) => {
            assert_eq!(c.len(), 1);
            assert_eq!(i32::from_le_bytes(c.value(0).try_into().unwrap()), 79);
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
}

#[test]
fn test_block_end_scans_decimal_columns() {
    // The completeness scan must walk every Decimal width (4/8/16/32 bytes
    // per row) to the exact block end, and report a one-byte-short buffer as
    // "need more bytes".
    let d32: Vec<[u8; 4]> = vec![13i32.to_le_bytes(), (-1i32).to_le_bytes()];
    let d256: Vec<[u8; 32]> = vec![[0x01u8; 32], [0xFFu8; 32]];
    let refs32: Vec<&[u8]> = d32.iter().map(|r| r.as_slice()).collect();
    let refs256: Vec<&[u8]> = d256.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(2, 2)
        .column_header("d32", "Decimal(9, 4)")
        .decimal_data(&refs32, 4)
        .column_header("d256", "Decimal(50, 10)")
        .decimal_data(&refs256, 32)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_decimal_rejected_as_low_cardinality_inner() {
    // The server forbids Decimal as a LowCardinality inner
    // (`canBeInsideLowCardinality()` is false on DataTypeDecimalBase), so it
    // never appears on the wire and the decoder rejects it as
    // UnsupportedType rather than mis-decoding.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("lc", "LowCardinality(Decimal(9, 4))")
        // key-version prefix then an index word; decode rejects before
        // reaching the body, so the exact trailing bytes do not matter.
        .raw_bytes(&1u64.to_le_bytes())
        .raw_bytes(&(LC_HAS_ADDITIONAL_KEYS_BIT).to_le_bytes())
        .raw_bytes(&0u64.to_le_bytes())
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_enum_rejected_as_low_cardinality_inner() {
    // The server forbids Enum as a LowCardinality inner
    // (`canBeInsideLowCardinality()` is false), so it never appears on the
    // wire and the decoder rejects it as UnsupportedType rather than
    // mis-decoding. This is independent of Enum decode support.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("e", "LowCardinality(Enum8('a' = 1))")
        // key-version prefix then an index word; decode rejects before
        // reaching the body, so the exact trailing bytes do not matter.
        .raw_bytes(&1u64.to_le_bytes())
        .raw_bytes(&(LC_HAS_ADDITIONAL_KEYS_BIT).to_le_bytes())
        .raw_bytes(&0u64.to_le_bytes())
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

// -----------------------------------------------------------------------
// Wide integers (Int128 / UInt128 / Int256 / UInt256)
// -----------------------------------------------------------------------

/// A 16-byte little-endian buffer with `b[0] = low`, the rest zero.
fn w16(low: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0] = low;
    b
}

/// A 32-byte little-endian buffer with `b[0] = low`, the rest zero.
fn w32(low: u8) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[0] = low;
    b
}

#[test]
fn test_parse_ch_type_wide_int() {
    // The four exact, case-sensitive spellings the server emits; no
    // parameters, no aliases. Display round-trips each.
    for (name, ty) in [
        ("Int128", ChType::Int128),
        ("UInt128", ChType::UInt128),
        ("Int256", ChType::Int256),
        ("UInt256", ChType::UInt256),
    ] {
        assert_eq!(parse_ch_type(name), Some(ty.clone()));
        assert_eq!(ty.to_string(), name);
        assert_eq!(parse_ch_type(&ty.to_string()), Some(ty));
    }
    // No case-folding or alias forms are accepted.
    for bad in ["int128", "UINT128", "Int 128", "Int512", "UInt128(1)"] {
        assert_eq!(parse_ch_type(bad), None, "{bad} must not parse");
    }
}

#[test]
fn test_decode_int128_plain() {
    // Int128 is a raw 16-byte LE two's-complement integer per row. The core
    // has no native i128, so assert the raw byte pattern directly. Sign and
    // boundary values: 13, -1 (all 0xFF), i128::MIN (only the MSB set:
    // b[15] = 0x80), and i128::MAX (all 0xFF then b[15] = 0x7F).
    let thirteen = w16(13);
    let neg_one = [0xFFu8; 16];
    let min = {
        let mut b = [0u8; 16];
        b[15] = 0x80;
        b
    };
    let max = {
        let mut b = [0xFFu8; 16];
        b[15] = 0x7F;
        b
    };
    let rows = [
        thirteen.as_slice(),
        neg_one.as_slice(),
        min.as_slice(),
        max.as_slice(),
    ];
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("w", "Int128")
        .wide_int_data(&rows, 16)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Int128(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.len(), 4);
            assert_eq!(c.value(0), thirteen);
            assert_eq!(c.value(1), neg_one);
            assert_eq!(c.value(2), min);
            assert_eq!(c.value(3), max);
            assert!(c.validity.is_none());
        }
        other => panic!("expected Int128, got {other:?}"),
    }
    assert_eq!(batch.schema.fields[0].ch_type, ChType::Int128);
}

#[test]
fn test_decode_uint128_plain() {
    // UInt128 shares the 16-byte LE layout; the type is unsigned, so a
    // high-bit-set value must survive verbatim (it is NOT a negative). Cover
    // 79, 2^127 (b[15] = 0x80, the high bit), and u128::MAX (all 0xFF).
    let seventy_nine = w16(79);
    let two_pow_127 = {
        let mut b = [0u8; 16];
        b[15] = 0x80;
        b
    };
    let max = [0xFFu8; 16];
    let rows = [
        seventy_nine.as_slice(),
        two_pow_127.as_slice(),
        max.as_slice(),
    ];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("w", "UInt128")
        .wide_int_data(&rows, 16)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::UInt128(c) => {
            assert_eq!(c.width, 16);
            assert_eq!(c.value(0), seventy_nine);
            assert_eq!(c.value(1), two_pow_127);
            assert_eq!(c.value(2), max);
        }
        other => panic!("expected UInt128, got {other:?}"),
    }
    assert_eq!(batch.schema.fields[0].ch_type, ChType::UInt128);
}

#[test]
fn test_decode_int256_plain() {
    // Int256 is a raw 32-byte LE two's-complement integer per row. Cover 13,
    // -1 (all 0xFF), i256::MIN (b[31] = 0x80) and i256::MAX (all 0xFF then
    // b[31] = 0x7F).
    let thirteen = w32(13);
    let neg_one = [0xFFu8; 32];
    let min = {
        let mut b = [0u8; 32];
        b[31] = 0x80;
        b
    };
    let max = {
        let mut b = [0xFFu8; 32];
        b[31] = 0x7F;
        b
    };
    let rows = [
        thirteen.as_slice(),
        neg_one.as_slice(),
        min.as_slice(),
        max.as_slice(),
    ];
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("w", "Int256")
        .wide_int_data(&rows, 32)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Int256(c) => {
            assert_eq!(c.width, 32);
            assert_eq!(c.len(), 4);
            assert_eq!(c.value(0), thirteen);
            assert_eq!(c.value(1), neg_one);
            assert_eq!(c.value(2), min);
            assert_eq!(c.value(3), max);
        }
        other => panic!("expected Int256, got {other:?}"),
    }
    assert_eq!(batch.schema.fields[0].ch_type, ChType::Int256);
}

#[test]
fn test_decode_uint256_plain() {
    // UInt256 is a raw 32-byte LE unsigned integer per row. A high-bit-set
    // value (b[31] = 0x80 = 2^255) must survive verbatim as a positive value.
    let seventy_nine = w32(79);
    let two_pow_255 = {
        let mut b = [0u8; 32];
        b[31] = 0x80;
        b
    };
    let max = [0xFFu8; 32];
    let rows = [
        seventy_nine.as_slice(),
        two_pow_255.as_slice(),
        max.as_slice(),
    ];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("w", "UInt256")
        .wide_int_data(&rows, 32)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::UInt256(c) => {
            assert_eq!(c.width, 32);
            assert_eq!(c.value(1), two_pow_255);
            assert_eq!(c.value(2), max);
        }
        other => panic!("expected UInt256, got {other:?}"),
    }
    assert_eq!(batch.schema.fields[0].ch_type, ChType::UInt256);
}

#[test]
fn test_decode_nullable_int128() {
    // Nullable(Int128): the null map first, then the 16-byte body, exactly
    // like Nullable(Decimal128). Null rows still carry a placeholder.
    let rows = [
        w16(13),
        w16(0),
        [0xFFu8; 16], // -1
        w16(0),
    ];
    let row_refs: Vec<&[u8]> = rows.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("w", "Nullable(Int128)")
        .null_map(&[false, true, false, true])
        .wide_int_data(&row_refs, 16)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Int128(c) => {
            assert_eq!(c.null_count(), 2);
            assert_eq!(c.value(0), w16(13));
            assert_eq!(c.value(2), [0xFFu8; 16]);
        }
        other => panic!("expected Int128, got {other:?}"),
    }
    assert!(batch.column(0).validity().unwrap().is_valid(0));
    assert!(!batch.column(0).validity().unwrap().is_valid(1));
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::Nullable(Box::new(ChType::Int128))
    );
}

#[test]
fn test_decode_wide_int_zero_rows() {
    // A zero-row block carrying every wide-int type contributes the schema
    // but no chunks; the empty columns have length 0 and keep their width.
    let data = BlockBuilder::new()
        .header(4, 0)
        .column_header("i128", "Int128")
        .column_header("u128", "UInt128")
        .column_header("i256", "Int256")
        .column_header("u256", "UInt256")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 4);
    assert_eq!(cb.schema.fields[0].ch_type, ChType::Int128);
    assert_eq!(cb.schema.fields[1].ch_type, ChType::UInt128);
    assert_eq!(cb.schema.fields[2].ch_type, ChType::Int256);
    assert_eq!(cb.schema.fields[3].ch_type, ChType::UInt256);
}

#[test]
fn test_multi_block_wide_int_kept_as_chunks() {
    // Wide-int blocks stay separate chunks, never concatenated.
    let a = [w32(13), [0xFFu8; 32]];
    let b = [w32(79)];
    let refs_a: Vec<&[u8]> = a.iter().map(|r| r.as_slice()).collect();
    let refs_b: Vec<&[u8]> = b.iter().map(|r| r.as_slice()).collect();
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("w", "Int256")
        .wide_int_data(&refs_a, 32)
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("w", "Int256")
            .wide_int_data(&refs_b, 32)
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);
    match cb.chunks[0].column(0) {
        Column::Int256(c) => {
            assert_eq!(c.len(), 2);
            assert_eq!(c.value(1), [0xFFu8; 32]);
        }
        other => panic!("expected Int256, got {other:?}"),
    }
    match cb.chunks[1].column(0) {
        Column::Int256(c) => {
            assert_eq!(c.len(), 1);
            assert_eq!(c.value(0), w32(79));
        }
        other => panic!("expected Int256, got {other:?}"),
    }
}

#[test]
fn test_block_end_scans_wide_int_columns() {
    // The completeness scan must walk both wide-int widths (16 and 32 bytes
    // per row) to the exact block end, and report a one-byte-short buffer as
    // "need more bytes".
    let i128_rows = [w16(13), [0xFFu8; 16]];
    let u256_rows = [w32(79), [0xFFu8; 32]];
    let refs128: Vec<&[u8]> = i128_rows.iter().map(|r| r.as_slice()).collect();
    let refs256: Vec<&[u8]> = u256_rows.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(2, 2)
        .column_header("i128", "Int128")
        .wide_int_data(&refs128, 16)
        .column_header("u256", "UInt256")
        .wide_int_data(&refs256, 32)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_decode_low_cardinality_int256() {
    // LowCardinality(Int256) IS legal on the wire (a DataTypeNumberBase
    // subclass, canBeInsideLowCardinality is true). The dictionary body is
    // the plain 32-byte-per-entry Int256 run; indices resolve into it. This
    // exercises the wide-int entry in the LC allowlist end to end.
    let dict = [w32(13), [0xFFu8; 32]]; // entry 0 = 13, entry 1 = -1
    let dict_refs: Vec<&[u8]> = dict.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("lc", "LowCardinality(Int256)")
        .low_cardinality_fixed(&dict_refs, &[0, 1, 0], 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    match batch.column(0) {
        Column::Dictionary(c) => {
            assert_eq!(c.indices, vec![0, 1, 0]);
            assert!(c.validity.is_none());
            match c.values.as_ref() {
                Column::Int256(v) => {
                    assert_eq!(v.width, 32);
                    assert_eq!(v.len(), 2);
                    assert_eq!(v.value(0), w32(13));
                    assert_eq!(v.value(1), [0xFFu8; 32]);
                }
                other => panic!("expected Int256 dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
    assert_eq!(
        batch.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::Int256))
    );
}

#[test]
fn test_decode_low_cardinality_uint128() {
    // LowCardinality(UInt128): unsigned, 16-byte dictionary entries. Include
    // a high-bit-set entry to prove the dictionary body is a raw passthrough.
    let high_bit = {
        let mut b = [0u8; 16];
        b[15] = 0x80;
        b
    };
    let dict = [w16(79), high_bit];
    let dict_refs: Vec<&[u8]> = dict.iter().map(|r| r.as_slice()).collect();
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("lc", "LowCardinality(UInt128)")
        .low_cardinality_fixed(&dict_refs, &[0, 1, 1, 0], 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Dictionary(c) => {
            assert_eq!(c.indices, vec![0, 1, 1, 0]);
            match c.values.as_ref() {
                Column::UInt128(v) => {
                    assert_eq!(v.value(0), w16(79));
                    assert_eq!(v.value(1), high_bit);
                }
                other => panic!("expected UInt128 dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Array(T)
// -----------------------------------------------------------------------

/// Borrow the inner `ArrayColumn` of a decoded `Array` column, panicking with
/// a useful message on any other variant. Keeps the assertions below terse.
fn as_array(col: &Column) -> &ArrayColumn {
    match col {
        Column::Array(a) => a,
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn test_parse_ch_type_array() {
    assert_eq!(
        parse_ch_type("Array(Int32)"),
        Some(ChType::Array(Box::new(ChType::Int32)))
    );
    // Element may itself be Nullable, LowCardinality, or a further Array; there
    // is no inner-type restriction on the parser.
    assert_eq!(
        parse_ch_type("Array(Nullable(Int32))"),
        Some(ChType::Array(Box::new(ChType::Nullable(Box::new(
            ChType::Int32
        )))))
    );
    assert_eq!(
        parse_ch_type("Array(LowCardinality(String))"),
        Some(ChType::Array(Box::new(ChType::LowCardinality(Box::new(
            ChType::String
        )))))
    );
    assert_eq!(
        parse_ch_type("Array(Array(Int32))"),
        Some(ChType::Array(Box::new(ChType::Array(Box::new(
            ChType::Int32
        )))))
    );
}

#[test]
fn test_ch_type_display_round_trips_array() {
    for t in [
        ChType::Array(Box::new(ChType::Int32)),
        ChType::Array(Box::new(ChType::String)),
        ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::Int32)))),
        ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
        ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
    ] {
        assert_eq!(parse_ch_type(&t.to_string()), Some(t.clone()));
    }
}

#[test]
fn test_parse_ch_type_nullable_array_is_rejected() {
    // `Nullable(Array(T))` is not a constructible server type
    // (`DataTypeArray::canBeInsideNullable()` is false), so the parser rejects
    // it rather than producing a shape decode/scan cannot unwrap.
    assert_eq!(parse_ch_type("Nullable(Array(Int32))"), None);

    // The rejection must hold on both the allocating decode and the scan, at
    // zero and nonzero rows, matching the other illegal-nesting guards.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("a", "Nullable(Array(Int32))")
            .build();
        assert!(matches!(
            decode_all_bytes(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
        assert!(matches!(
            block_end(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
    }
}

#[test]
fn test_decode_array_int32() {
    // Three rows, cumulative absolute end-offsets [2, 2, 5] (no leading zero):
    // row 0 has two elements, row 1 is EMPTY (equal adjacent offsets), row 2
    // has three. The flattened element body is the five Int32 values.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("a", "Array(Int32)")
        .array_offsets(&[2, 2, 5])
        .int32_data(&[13, 79, 21, 34, 55])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    // Arrow list offsets carry the leading 0 and are i64.
    assert_eq!(arr.offsets, vec![0i64, 2, 2, 5]);
    assert_eq!(arr.len(), 3);
    assert_eq!(arr.null_count(), 0);
    match arr.values.as_ref() {
        Column::Int32(v) => {
            assert!(v.validity.is_none());
            assert_eq!(v.values, vec![13, 79, 21, 34, 55]);
        }
        other => panic!("expected Int32 element values, got {other:?}"),
    }
}

#[test]
fn test_decode_array_string() {
    // Variable-length element body after the offsets: row 0 has two strings,
    // row 1 has one.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(String)")
        .array_offsets(&[2, 3])
        .string_data(&["user_1", "user_2", "user_3"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    match arr.values.as_ref() {
        Column::Utf8(v) => {
            assert_eq!(v.len(), 3);
            assert_eq!(v.value(0), b"user_1");
            assert_eq!(v.value(1), b"user_2");
            assert_eq!(v.value(2), b"user_3");
        }
        other => panic!("expected Utf8 element values, got {other:?}"),
    }
}

#[test]
fn test_decode_array_nullable_int32() {
    // For `Array(Nullable(Int32))` the element body is a `total_elements` null
    // map then the `total_elements` values. Element-level nulls live on the
    // element column's validity, never on the array itself.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(Nullable(Int32))")
        .array_offsets(&[2, 3])
        .null_map(&[false, true, false]) // element 1 is null
        .int32_data(&[13, 0, 79])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    // The array level is never nullable.
    assert_eq!(arr.null_count(), 0);
    match arr.values.as_ref() {
        Column::Int32(v) => {
            assert_eq!(v.values, vec![13, 0, 79]);
            let bm = v.validity.as_ref().expect("nullable element validity");
            assert!(bm.is_valid(0));
            assert!(!bm.is_valid(1));
            assert!(bm.is_valid(2));
            assert_eq!(v.null_count(), 1);
        }
        other => panic!("expected Int32 element values, got {other:?}"),
    }
}

#[test]
fn test_decode_array_low_cardinality_string() {
    // `Array(LowCardinality(String))`: SerializationArray recurses into the
    // element prefix, so the LC 8-byte key version is written FIRST, before the
    // offsets. The LC body (index word / dictionary / row count / indexes) is
    // the flattened element column and comes AFTER the offsets. Build a full
    // LC(String) block, then move its leading 8-byte key version ahead of the
    // offsets to match the wire order.
    let dictionary = ["", "user_1", "user_2"];
    let element_indices = [1u64, 2, 1]; // three flattened elements
    let lc_full = BlockBuilder::new()
        .low_cardinality_string(&dictionary, &element_indices, 1)
        .build();
    let (key_version, lc_body) = lc_full.split_at(8);

    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(LowCardinality(String))")
        .raw_bytes(key_version) // element state prefix, ahead of the offsets
        .array_offsets(&[2, 3]) // row 0: two elements, row 1: one element
        .raw_bytes(lc_body) // LC index word / dict / row count / indexes
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    // The element column is a per-block dictionary.
    match arr.values.as_ref() {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![1, 2, 1]);
            assert!(d.validity.is_none());
            match d.values.as_ref() {
                Column::Utf8(v) => {
                    assert_eq!(v.value(0), b"");
                    assert_eq!(v.value(1), b"user_1");
                    assert_eq!(v.value(2), b"user_2");
                }
                other => panic!("expected Utf8 dictionary values, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary element values, got {other:?}"),
    }
}

#[test]
fn test_decode_array_low_cardinality_all_empty() {
    // Rows > 0 but every array empty: the server writes the hoisted LC key
    // version, then all-zero offsets, then NOTHING for the LC element run
    // (`SerializationLowCardinality::serializeBinaryBulkWithMultipleStreams`
    // early-returns at limit == 0, confirmed at v26.6.1.1193-stable). The
    // decoder must read zero LC body bytes rather than fail with EOF.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(LowCardinality(String))")
        .raw_bytes(&1u64.to_le_bytes()) // hoisted LC key version
        .array_offsets(&[0, 0]) // both rows empty
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 0, 0]);
    match arr.values.as_ref() {
        Column::Dictionary(d) => {
            assert!(d.indices.is_empty());
            assert!(d.validity.is_none());
            assert_eq!(d.values.len(), 0);
        }
        other => panic!("expected empty Dictionary element values, got {other:?}"),
    }

    // The completeness scan must consume exactly the same zero LC body bytes.
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_decode_array_of_array_int32() {
    // `Array(Array(Int32))`: outer offsets, then the inner array's offsets (its
    // flattened element column), then the leaf Int32 body. The Int32 leaf has
    // no state prefix, so nothing precedes the outer offsets.
    //
    // Outer 2 rows: row 0 = [[13, 79], [21]], row 1 = [[34, 55, 89]].
    // Outer offsets count inner arrays: [2, 3] -> 3 inner arrays total.
    // Inner offsets count leaf ints: [2, 3, 6] -> 6 leaf ints total.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(Array(Int32))")
        .array_offsets(&[2, 3]) // outer
        .array_offsets(&[2, 3, 6]) // inner
        .int32_data(&[13, 79, 21, 34, 55, 89]) // leaf
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let outer = as_array(cb.chunks[0].column(0));
    assert_eq!(outer.offsets, vec![0i64, 2, 3]);
    let inner = as_array(outer.values.as_ref());
    assert_eq!(inner.offsets, vec![0i64, 2, 3, 6]);
    match inner.values.as_ref() {
        Column::Int32(v) => assert_eq!(v.values, vec![13, 79, 21, 34, 55, 89]),
        other => panic!("expected Int32 leaf values, got {other:?}"),
    }
}

#[test]
fn test_decode_array_zero_rows() {
    // A zero-row Array block contributes the schema but no chunk, and reads no
    // offsets or element body. `empty_column` builds the offsets `[0]` shape.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("a", "Array(Int32)")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 1);
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::Array(Box::new(ChType::Int32))
    );

    // The zero-row column shape: offsets `[0]` (len 0) over an empty element
    // column.
    let empty = empty_column(&ChType::Array(Box::new(ChType::Int32)));
    let arr = as_array(&empty);
    assert_eq!(arr.offsets, vec![0i64]);
    assert_eq!(arr.len(), 0);
    match arr.values.as_ref() {
        Column::Int32(v) => assert!(v.values.is_empty()),
        other => panic!("expected empty Int32 element values, got {other:?}"),
    }
}

#[test]
fn test_multi_block_array_separate_chunks() {
    // Native blocks stay separate chunks; two Array(Int32) blocks must not be
    // concatenated.
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(Int32)")
        .array_offsets(&[1, 3])
        .int32_data(&[13, 79, 21])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("a", "Array(Int32)")
            .array_offsets(&[2])
            .int32_data(&[34, 55])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);

    let chunk0 = as_array(cb.chunks[0].column(0));
    assert_eq!(chunk0.offsets, vec![0i64, 1, 3]);
    match chunk0.values.as_ref() {
        Column::Int32(v) => assert_eq!(v.values, vec![13, 79, 21]),
        other => panic!("expected Int32, got {other:?}"),
    }
    let chunk1 = as_array(cb.chunks[1].column(0));
    assert_eq!(chunk1.offsets, vec![0i64, 2]);
    match chunk1.values.as_ref() {
        Column::Int32(v) => assert_eq!(v.values, vec![34, 55]),
        other => panic!("expected Int32, got {other:?}"),
    }
}

#[test]
fn test_block_end_scans_array() {
    // The completeness scan must walk an Array column to the exact block end
    // and report a one-byte-short buffer as "need more bytes".
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("a", "Array(Int32)")
        .array_offsets(&[2, 2, 5])
        .int32_data(&[13, 79, 21, 34, 55])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    let truncated = &data[..data.len() - 1];
    let err = block_end(truncated, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::Io(ref e) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_array_rejects_non_monotonic_offsets() {
    // Decreasing offsets are INCORRECT_DATA on the server and corrupt here; the
    // decoder rejects them as InvalidArray rather than slicing out of bounds.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(Int32)")
        .array_offsets(&[3, 1]) // 1 < 3 -> reject
        .int32_data(&[13, 79, 21])
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidArray { .. })
    ));
    // The completeness scan surfaces the same error rather than stalling.
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidArray { .. })
    ));
}

#[test]
fn test_parse_ch_type_rejects_over_deep_nesting() {
    // The type string is untrusted wire input; an unbounded nesting like
    // `Array(Array(...Array(Int32)...))` would overflow the stack (an
    // uncatchable SIGABRT) without a depth cap. Past MAX_TYPE_DEPTH the parser
    // returns None, which surfaces as UnsupportedType, never a crash.
    let n = MAX_TYPE_DEPTH + 100;
    let over_deep = format!("{}Int32{}", "Array(".repeat(n), ")".repeat(n));
    assert_eq!(parse_ch_type(&over_deep), None);

    // A zero-row block carrying that over-deep type must degrade to a clean
    // UnsupportedType error, not abort the process.
    let data = BlockBuilder::new()
        .header(1, 0)
        .column_header("a", &over_deep)
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_decode_array_nested_three_deep() {
    // A modestly nested type (well within MAX_TYPE_DEPTH) still parses and
    // decodes: `Array(Array(Array(Int32)))` with one outer row -> one middle
    // array -> one inner array -> two leaf ints. Each level writes its own
    // absolute end-offsets; the Int32 leaf has no state prefix.
    assert_eq!(
        parse_ch_type("Array(Array(Array(Int32)))"),
        Some(ChType::Array(Box::new(ChType::Array(Box::new(
            ChType::Array(Box::new(ChType::Int32))
        )))))
    );

    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("a", "Array(Array(Array(Int32)))")
        .array_offsets(&[1]) // outer: 1 middle array
        .array_offsets(&[1]) // middle: 1 inner array
        .array_offsets(&[2]) // inner: 2 leaf ints
        .int32_data(&[13, 79]) // leaf
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let outer = as_array(cb.chunks[0].column(0));
    assert_eq!(outer.offsets, vec![0i64, 1]);
    let middle = as_array(outer.values.as_ref());
    assert_eq!(middle.offsets, vec![0i64, 1]);
    let inner = as_array(middle.values.as_ref());
    assert_eq!(inner.offsets, vec![0i64, 2]);
    match inner.values.as_ref() {
        Column::Int32(v) => assert_eq!(v.values, vec![13, 79]),
        other => panic!("expected Int32 leaf values, got {other:?}"),
    }
}

#[test]
fn test_array_rejects_offset_above_i64_max() {
    // An absolute offset in (i64::MAX, u64::MAX] cannot widen into the i64
    // LargeList offset. Both the allocating decode and the completeness scan
    // must reject it as InvalidArray for the SAME bytes; if the scan instead
    // returned UnexpectedEof, StreamDecoder would treat a fully-present corrupt
    // block as "need more bytes" and stall.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("a", "Array(Int32)")
        .array_offsets(&[u64::MAX])
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidArray { .. })
    ));
    assert!(matches!(
        block_end(&data, &DecodeOptions::default()),
        Err(DecodeError::InvalidArray { .. })
    ));
}

#[test]
fn test_array_forbidden_low_cardinality_inner_rejected_regardless_of_rows() {
    // A forbidden LowCardinality inner (Decimal is not `canBeInsideLowCardinality`)
    // nested inside an Array must be rejected at header-read time on BOTH the
    // zero-row and the with-rows paths, so `empty_column` (which never consults
    // the allowlist) cannot silently accept what a row-bearing block rejects.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("a", "Array(LowCardinality(Decimal(9, 4)))")
            .build();
        assert!(
            matches!(
                decode_all_bytes(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ),
            "decode should reject forbidden LC-in-Array at {num_rows} rows"
        );
        assert!(
            matches!(
                block_end(&data, &DecodeOptions::default()),
                Err(DecodeError::UnsupportedType { .. })
            ),
            "scan should reject forbidden LC-in-Array at {num_rows} rows"
        );
    }
}

#[test]
fn test_parse_ch_type_tuple() {
    // Unnamed elements.
    assert_eq!(
        parse_ch_type("Tuple(Int32, String)"),
        Some(ChType::Tuple(vec![
            (None, ChType::Int32),
            (None, ChType::String),
        ]))
    );
    // Zero elements.
    assert_eq!(parse_ch_type("Tuple()"), Some(ChType::Tuple(vec![])));
    // Named elements, bare identifiers.
    assert_eq!(
        parse_ch_type("Tuple(a Int32, user_2 Nullable(String))"),
        Some(ChType::Tuple(vec![
            (Some("a".to_string()), ChType::Int32),
            (
                Some("user_2".to_string()),
                ChType::Nullable(Box::new(ChType::String)),
            ),
        ]))
    );
    // A comma inside an element type's own parentheses must not split.
    assert_eq!(
        parse_ch_type("Tuple(Decimal(9, 4), Int8)"),
        Some(ChType::Tuple(vec![
            (
                None,
                ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 32,
                },
            ),
            (None, ChType::Int8),
        ]))
    );
    // A comma inside an Enum element's quoted names must not split either.
    assert_eq!(
        parse_ch_type("Tuple(e Enum8('a,b' = 1), s String)"),
        Some(ChType::Tuple(vec![
            (
                Some("e".to_string()),
                ChType::Enum8 {
                    variants: vec![("a,b".to_string(), 1)],
                },
            ),
            (Some("s".to_string()), ChType::String),
        ]))
    );
    // Backtick-quoted names: spaces and commas just force quoting; a
    // backtick inside escapes as \` (the server's writeBackQuotedString
    // form) or as a doubled `` (accepted for parser leniency).
    assert_eq!(
        parse_ch_type("Tuple(`a b` Int8, `c,d` Int8, `e\\`f` Int8, `g``h` Int8)"),
        Some(ChType::Tuple(vec![
            (Some("a b".to_string()), ChType::Int8),
            (Some("c,d".to_string()), ChType::Int8),
            (Some("e`f".to_string()), ChType::Int8),
            (Some("g`h".to_string()), ChType::Int8),
        ]))
    );
    // Keyword names arrive backtick-quoted from the server.
    assert_eq!(
        parse_ch_type("Tuple(`select` Int8)"),
        Some(ChType::Tuple(vec![(
            Some("select".to_string()),
            ChType::Int8,
        )]))
    );
    // Containers compose: Tuple in Array, Array in Tuple, nested Tuple,
    // Nullable(Tuple).
    assert_eq!(
        parse_ch_type("Array(Tuple(Int32, Int32))"),
        Some(ChType::Array(Box::new(ChType::Tuple(vec![
            (None, ChType::Int32),
            (None, ChType::Int32),
        ]))))
    );
    assert_eq!(
        parse_ch_type("Tuple(a Tuple(b Int8), c Array(String))"),
        Some(ChType::Tuple(vec![
            (
                Some("a".to_string()),
                ChType::Tuple(vec![(Some("b".to_string()), ChType::Int8)]),
            ),
            (
                Some("c".to_string()),
                ChType::Array(Box::new(ChType::String)),
            ),
        ]))
    );
    assert_eq!(
        parse_ch_type("Nullable(Tuple(Int32, String))"),
        Some(ChType::Nullable(Box::new(ChType::Tuple(vec![
            (None, ChType::Int32),
            (None, ChType::String),
        ]))))
    );

    // Malformed inputs are rejected, never panicked on: an unterminated
    // backtick, a name with no type, an empty element, unbalanced
    // parentheses, an unknown element type, and a trailing lone backslash.
    for bad in [
        "Tuple(`a Int8)",
        "Tuple(`a`)",
        "Tuple(a )",
        "Tuple(Int32,, String)",
        "Tuple(Int32, )",
        "Tuple(Int32))",
        "Tuple(NotAType)",
        "Tuple(`a\\",
    ] {
        assert_eq!(parse_ch_type(bad), None, "should reject {bad:?}");
    }

    // The depth cap applies through Tuple nesting like the other containers.
    let mut deep = String::from("Int8");
    for _ in 0..(MAX_TYPE_DEPTH + 1) {
        deep = format!("Tuple({deep})");
    }
    assert_eq!(parse_ch_type(&deep), None);
}

#[test]
fn test_ch_type_display_round_trips_tuple() {
    // parse(display(t)) == t for every tuple the parser accepts, and
    // display(parse(s)) == s for the canonical server strings.
    for s in [
        "Tuple(Int32, String)",
        "Tuple()",
        "Tuple(a Int32, b Nullable(String))",
        "Tuple(`a b` Int8, `c,d` Int8, `e\\`f` Int8)",
        "Tuple(`select` Int8, selected Int8)",
        "Tuple(`NULL` Int8, nullable Int8)",
        "Tuple(a Tuple(b Int8), c Array(String))",
        "Nullable(Tuple(Int32, String))",
        "Array(Tuple(Int32, Int32))",
        "Tuple(e Enum8('a,b' = 1), d Decimal(9, 4))",
        "Tuple(lc LowCardinality(String), n Nullable(Int32))",
    ] {
        let parsed = parse_ch_type(s).unwrap_or_else(|| panic!("should parse {s:?}"));
        assert_eq!(parsed.to_string(), s, "display must render canonically");
        assert_eq!(
            parse_ch_type(&parsed.to_string()),
            Some(parsed),
            "round-trip failed for {s:?}"
        );
    }
    // The doubled-backtick escape parses but re-renders in the server's
    // backslash form, so it round-trips by value, not by string.
    let parsed = parse_ch_type("Tuple(`g``h` Int8)").unwrap();
    assert_eq!(parsed.to_string(), "Tuple(`g\\`h` Int8)");
    assert_eq!(parse_ch_type(&parsed.to_string()), Some(parsed));
}

/// Borrow the inner `TupleColumn` of a decoded `Tuple` column.
fn as_tuple(column: &Column) -> &crate::column::TupleColumn {
    match column {
        Column::Tuple(c) => c,
        other => panic!("expected Tuple column, got {other:?}"),
    }
}

#[test]
fn test_decode_tuple_plain() {
    // Tuple(Int32, String): element 0's full Int32 run, then element 1's
    // full String run, column-of-columns with no interleaving and no
    // tuple-level framing.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("t", "Tuple(Int32, String)")
        .int32_data(&[13, 79, -7])
        .string_data(&["user_1", "user_2", ""])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let batch = &cb.chunks[0];
    let t = as_tuple(batch.column(0));
    assert_eq!(t.len(), 3);
    assert_eq!(t.fields.len(), 2);
    assert!(t.validity.is_none());
    match &t.fields[0] {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), &[13, 79, -7]),
        other => panic!("expected Int32 element, got {other:?}"),
    }
    match &t.fields[1] {
        Column::Utf8(c) => {
            assert_eq!(c.value(0), b"user_1");
            assert_eq!(c.value(1), b"user_2");
            assert_eq!(c.value(2), b"");
        }
        other => panic!("expected Utf8 element, got {other:?}"),
    }
}

#[test]
fn test_decode_named_tuple_with_nullable_element() {
    // Tuple(a Int32, b Nullable(String)): the names live in the schema's
    // ChType only; element b's body is its own per-row null map then the
    // string run, the ordinary Nullable framing at element level.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("t", "Tuple(a Int32, b Nullable(String))")
        .int32_data(&[1, 2, 3])
        .null_map(&[false, true, false])
        .string_data(&["user_1", "", "user_2"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::Tuple(vec![
            (Some("a".to_string()), ChType::Int32),
            (
                Some("b".to_string()),
                ChType::Nullable(Box::new(ChType::String)),
            ),
        ])
    );
    let t = as_tuple(cb.chunks[0].column(0));
    assert_eq!(t.len(), 3);
    match &t.fields[1] {
        Column::Utf8(c) => {
            assert_eq!(c.null_count(), 1);
            let bm = c.validity.as_ref().expect("element validity");
            assert!(bm.is_valid(0) && !bm.is_valid(1) && bm.is_valid(2));
        }
        other => panic!("expected Utf8 element, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_tuple() {
    // Nullable(Tuple(Int32, String)): the ordinary Nullable framing, the
    // per-row null map first, then the tuple body. Null rows still carry
    // placeholder element values.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("t", "Nullable(Tuple(Int32, String))")
        .null_map(&[false, true, false])
        .int32_data(&[13, 0, 79])
        .string_data(&["user_1", "", "user_2"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let t = as_tuple(cb.chunks[0].column(0));
    assert_eq!(t.len(), 3);
    assert_eq!(t.null_count(), 1);
    let bm = t.validity.as_ref().expect("tuple-level validity");
    assert!(bm.is_valid(0) && !bm.is_valid(1) && bm.is_valid(2));
    match &t.fields[0] {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), &[13, 0, 79]),
        other => panic!("expected Int32 element, got {other:?}"),
    }
}

#[test]
fn test_decode_tuple_low_cardinality_element_prefix_is_hoisted() {
    // Tuple(Int32, LowCardinality(String)): the element state prefixes are
    // written at the very FRONT of the whole Tuple column in declaration
    // order (SerializationTuple delegates), so the LC 8-byte key version
    // precedes even element 0's Int32 run, and the LC body itself (element
    // 1) carries no key version of its own.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("t", "Tuple(Int32, LowCardinality(String))")
        // Hoisted prefix: element 1's LC key version.
        .uint64_data(&[LOW_CARDINALITY_KEY_VERSION])
        // Element 0: the full Int32 run.
        .int32_data(&[13, 79, -7])
        // Element 1: the LC body WITHOUT its key version: index word
        // (width tag 0 = u8, additional keys), dictionary, row count,
        // indices.
        .uint64_data(&[LC_HAS_ADDITIONAL_KEYS_BIT])
        .uint64_data(&[2]) // num_keys
        .string_data(&["red", "green"])
        .uint64_data(&[3]) // num_rows restated
        .raw_bytes(&[0, 1, 0]) // u8 indices
        .build();

    // The completeness scan and the decode must agree on the framing.
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let t = as_tuple(cb.chunks[0].column(0));
    assert_eq!(t.len(), 3);
    match &t.fields[1] {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![0, 1, 0]);
            match d.values.as_ref() {
                Column::Utf8(c) => {
                    assert_eq!(c.value(0), b"red");
                    assert_eq!(c.value(1), b"green");
                }
                other => panic!("expected Utf8 dictionary, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary element, got {other:?}"),
    }
}

#[test]
fn test_decode_array_of_tuple() {
    // Array(Tuple(Int32, Int32)): offsets first (the tuple elements write no
    // prefix), then the flattened tuple body: element 0's full run of
    // total_elements rows, then element 1's. Rows: [], [(13, 79)],
    // [(1, 2), (3, 4)], [(-1, -2)].
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("a", "Array(Tuple(Int32, Int32))")
        .array_offsets(&[0, 1, 3, 4])
        .int32_data(&[13, 1, 3, -1])
        .int32_data(&[79, 2, 4, -2])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Array(arr) => {
            assert_eq!(arr.offsets, vec![0i64, 0, 1, 3, 4]);
            let t = as_tuple(arr.values.as_ref());
            assert_eq!(t.len(), 4);
            match (&t.fields[0], &t.fields[1]) {
                (Column::Int32(a), Column::Int32(b)) => {
                    assert_eq!(a.values.as_slice(), &[13, 1, 3, -1]);
                    assert_eq!(b.values.as_slice(), &[79, 2, 4, -2]);
                }
                other => panic!("expected Int32 elements, got {other:?}"),
            }
        }
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn test_decode_nested_tuple() {
    // Tuple(p Tuple(Int8, Int8), s String): the inner tuple's body is its
    // own two element runs, nested in declaration order inside the outer
    // tuple's element sequence.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("t", "Tuple(p Tuple(Int8, Int8), s String)")
        .int8_data(&[1, 3])
        .int8_data(&[2, 4])
        .string_data(&["user_1", "user_2"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let outer = as_tuple(cb.chunks[0].column(0));
    assert_eq!(outer.len(), 2);
    let inner = as_tuple(&outer.fields[0]);
    assert_eq!(inner.len(), 2);
    match (&inner.fields[0], &inner.fields[1]) {
        (Column::Int8(a), Column::Int8(b)) => {
            assert_eq!(a.values.as_slice(), &[1, 3]);
            assert_eq!(b.values.as_slice(), &[2, 4]);
        }
        other => panic!("expected Int8 elements, got {other:?}"),
    }
}

#[test]
fn test_decode_empty_tuple() {
    // Tuple(): exactly one placeholder byte per row and nothing else. The
    // server writes ASCII '0' and ignores the values on read (tryIgnore),
    // so arbitrary byte values decode too.
    for body in [b"0000".as_slice(), &[0xAB, 0x00, 0x30, 0xFF]] {
        let data = BlockBuilder::new()
            .header(1, 4)
            .column_header("t", "Tuple()")
            .raw_bytes(body)
            .build();

        assert_eq!(
            block_end(&data, &DecodeOptions::default()).unwrap(),
            Some(data.len())
        );
        let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
        let t = as_tuple(cb.chunks[0].column(0));
        assert_eq!(t.len(), 4);
        assert!(t.fields.is_empty());
    }

    // Truncated placeholder bytes are "need more bytes", on both paths.
    let short = BlockBuilder::new()
        .header(1, 4)
        .column_header("t", "Tuple()")
        .raw_bytes(b"000")
        .build();
    assert!(matches!(
        decode_all_bytes(&short, &DecodeOptions::default()),
        Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
    assert!(matches!(
        block_end(&short, &DecodeOptions::default()),
        Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
    ));
}

#[test]
fn test_decode_tuple_zero_rows() {
    // A zero-row block carries only the headers: no element bodies and no
    // Tuple() placeholder bytes.
    for type_name in ["Tuple(Int32, String)", "Tuple()", "Nullable(Tuple(Int8))"] {
        let data = BlockBuilder::new()
            .header(1, 0)
            .column_header("t", type_name)
            .build();
        let batch = decode_next_block(&mut ByteReader::new(&data), &DecodeOptions::default())
            .unwrap()
            .unwrap();
        let t = as_tuple(batch.column(0));
        assert_eq!(t.len(), 0);
        assert_eq!(
            block_end(&data, &DecodeOptions::default()).unwrap(),
            Some(data.len())
        );
    }
}

#[test]
fn test_multi_block_tuple_kept_as_chunks() {
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("t", "Tuple(Int32, String)")
        .int32_data(&[13, 79])
        .string_data(&["user_1", "user_2"])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("t", "Tuple(Int32, String)")
            .int32_data(&[-7])
            .string_data(&["user_3"])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);
    let t0 = as_tuple(cb.chunks[0].column(0));
    let t1 = as_tuple(cb.chunks[1].column(0));
    assert_eq!(t0.len(), 2);
    assert_eq!(t1.len(), 1);
    match &t1.fields[0] {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), &[-7]),
        other => panic!("expected Int32 element, got {other:?}"),
    }
}

#[test]
fn test_tuple_truncated_element_body_is_eof_not_panic() {
    // Truncate inside element 1's String run: both the decode and the
    // completeness scan must report "need more bytes" (UnexpectedEof), the
    // signal the streaming decoder waits on, and never panic.
    let full = BlockBuilder::new()
        .header(1, 3)
        .column_header("t", "Tuple(Int32, String)")
        .int32_data(&[13, 79, -7])
        .string_data(&["user_1", "user_2", "user_3"])
        .build();

    for end in [full.len() - 1, full.len() - 8, full.len() - 20] {
        let truncated = &full[..end];
        assert!(matches!(
            decode_all_bytes(truncated, &DecodeOptions::default()),
            Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert!(matches!(
            block_end(truncated, &DecodeOptions::default()),
            Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
    }
}

#[test]
fn test_array_of_tuple_all_empty_passes_zero_limit_to_elements() {
    // Array(Tuple(LowCardinality(String), Int32)) with rows > 0 but every
    // array empty: the hoisted prefix walk still runs (the LC key version
    // is at the very front), the offsets are all zero, and the element
    // bodies are entirely absent; the LC element's limit == 0 early-return
    // gate must fire through the Tuple element path.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("a", "Array(Tuple(LowCardinality(String), Int32))")
        .uint64_data(&[LOW_CARDINALITY_KEY_VERSION]) // hoisted LC prefix
        .array_offsets(&[0, 0, 0])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Array(arr) => {
            assert_eq!(arr.offsets, vec![0i64, 0, 0, 0]);
            let t = as_tuple(arr.values.as_ref());
            assert_eq!(t.len(), 0);
            match &t.fields[0] {
                Column::Dictionary(d) => {
                    assert!(d.indices.is_empty());
                    assert_eq!(d.values.len(), 0);
                }
                other => panic!("expected empty Dictionary element, got {other:?}"),
            }
        }
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn test_parse_ch_type_map() {
    assert_eq!(
        parse_ch_type("Map(String, Int32)"),
        Some(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Int32),
        ))
    );
    // Either argument can carry top-level-looking commas inside its own
    // parentheses or quotes; the paren/quote-aware splitter must not split
    // there.
    assert_eq!(
        parse_ch_type("Map(String, Decimal(9, 4))"),
        Some(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            }),
        ))
    );
    // Containers compose: LC key, Nullable value, Array value, nested Map,
    // Map inside Array, Map inside Tuple.
    assert_eq!(
        parse_ch_type("Map(LowCardinality(String), UInt8)"),
        Some(ChType::Map(
            Box::new(ChType::LowCardinality(Box::new(ChType::String))),
            Box::new(ChType::UInt8),
        ))
    );
    assert_eq!(
        parse_ch_type("Map(Int32, Nullable(String))"),
        Some(ChType::Map(
            Box::new(ChType::Int32),
            Box::new(ChType::Nullable(Box::new(ChType::String))),
        ))
    );
    assert_eq!(
        parse_ch_type("Map(String, Map(String, Int32))"),
        Some(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Int32),
            )),
        ))
    );
    assert_eq!(
        parse_ch_type("Array(Map(String, Int32))"),
        Some(ChType::Array(Box::new(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::Int32),
        ))))
    );
    assert_eq!(
        parse_ch_type("Tuple(m Map(String, Int32))"),
        Some(ChType::Tuple(vec![(
            Some("m".to_string()),
            ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        )]))
    );

    // Malformed or illegal-at-parse-time forms. Nullable(Map) is not
    // constructible (DataTypeMap::canBeInsideNullable() is false), so the
    // Nullable arm rejects it outright.
    for bad in [
        "Map(String)",
        "Map(String, Int32, Int8)",
        "Map()",
        "Map(String, NotAType)",
        "Map(String, Int32))",
        "Nullable(Map(String, Int32))",
    ] {
        assert_eq!(parse_ch_type(bad), None, "should reject {bad:?}");
    }

    // The depth cap applies through Map nesting like the other containers.
    let mut deep = String::from("Int8");
    for _ in 0..(MAX_TYPE_DEPTH + 1) {
        deep = format!("Map(String, {deep})");
    }
    assert_eq!(parse_ch_type(&deep), None);
}

#[test]
fn test_ch_type_display_round_trips_map() {
    for s in [
        "Map(String, Int32)",
        "Map(LowCardinality(String), UInt8)",
        "Map(Int32, Nullable(String))",
        "Map(String, Array(Int32))",
        "Map(String, Map(String, Int32))",
        "Array(Map(String, Int32))",
        "Map(String, Tuple(a Int32, b String))",
    ] {
        let parsed = parse_ch_type(s).unwrap_or_else(|| panic!("should parse {s:?}"));
        assert_eq!(parsed.to_string(), s, "display must render canonically");
        assert_eq!(parse_ch_type(&parsed.to_string()), Some(parsed));
    }
}

/// Borrow the inner `MapColumn` of a decoded `Map` column.
fn as_map(column: &Column) -> &crate::column::MapColumn {
    match column {
        Column::Map(c) => c,
        other => panic!("expected Map column, got {other:?}"),
    }
}

/// Borrow a `MapColumn`'s keys and values columns out of its two-field
/// entries tuple.
fn map_entries(map: &crate::column::MapColumn) -> (&Column, &Column) {
    let t = as_tuple(map.entries.as_ref());
    assert_eq!(t.fields.len(), 2, "entries must be the (keys, values) pair");
    (&t.fields[0], &t.fields[1])
}

#[test]
fn test_decode_map_plain() {
    // Map(String, Int32): the Array(Tuple(keys, values)) wire layout, the
    // cumulative end-offsets then the flattened key run then the flattened
    // value run. Rows: {} / {a: 13} / {a: 1, b: 2} / {k: -7}.
    let data = BlockBuilder::new()
        .header(1, 4)
        .column_header("m", "Map(String, Int32)")
        .array_offsets(&[0, 1, 3, 4])
        .string_data(&["a", "a", "b", "k"])
        .int32_data(&[13, 1, 2, -7])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let m = as_map(cb.chunks[0].column(0));
    assert_eq!(m.len(), 4);
    assert_eq!(m.offsets, vec![0i64, 0, 1, 3, 4]);
    assert_eq!(m.null_count(), 0);
    let (keys, values) = map_entries(m);
    match keys {
        Column::Utf8(c) => {
            assert_eq!(c.len(), 4);
            assert_eq!(c.value(0), b"a");
            assert_eq!(c.value(2), b"b");
            assert_eq!(c.value(3), b"k");
        }
        other => panic!("expected Utf8 keys, got {other:?}"),
    }
    match values {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), &[13, 1, 2, -7]),
        other => panic!("expected Int32 values, got {other:?}"),
    }
}

#[test]
fn test_decode_map_low_cardinality_key_prefix_is_hoisted() {
    // Map(LowCardinality(String), Int32): the prefix chain is Map -> Array
    // (nothing) -> Tuple -> key then value, so the LC 8-byte key version
    // sits at the very FRONT of the whole column, before the offsets; the
    // LC key run itself carries no key version.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("m", "Map(LowCardinality(String), Int32)")
        .uint64_data(&[LOW_CARDINALITY_KEY_VERSION]) // hoisted key prefix
        .array_offsets(&[1, 1, 3])
        // Flattened LC key run (3 entries), WITHOUT its key version.
        .uint64_data(&[LC_HAS_ADDITIONAL_KEYS_BIT])
        .uint64_data(&[2]) // num_keys
        .string_data(&["red", "green"])
        .uint64_data(&[3]) // entry count restated
        .raw_bytes(&[0, 1, 0]) // u8 indices
        // Flattened Int32 value run.
        .int32_data(&[13, 79, -7])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let m = as_map(cb.chunks[0].column(0));
    assert_eq!(m.offsets, vec![0i64, 1, 1, 3]);
    let (keys, values) = map_entries(m);
    match keys {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![0, 1, 0]);
            match d.values.as_ref() {
                Column::Utf8(c) => {
                    assert_eq!(c.value(0), b"red");
                    assert_eq!(c.value(1), b"green");
                }
                other => panic!("expected Utf8 dictionary, got {other:?}"),
            }
        }
        other => panic!("expected Dictionary keys, got {other:?}"),
    }
    match values {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), &[13, 79, -7]),
        other => panic!("expected Int32 values, got {other:?}"),
    }
}

#[test]
fn test_decode_map_nullable_value() {
    // Map(Int32, Nullable(String)): the flattened value run is its own
    // per-entry null map then the strings, ordinary Nullable framing at the
    // value level. Rows: {1: user_1} / {2: NULL, 3: user_2}.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("m", "Map(Int32, Nullable(String))")
        .array_offsets(&[1, 3])
        .int32_data(&[1, 2, 3])
        .null_map(&[false, true, false])
        .string_data(&["user_1", "", "user_2"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let m = as_map(cb.chunks[0].column(0));
    assert_eq!(m.offsets, vec![0i64, 1, 3]);
    let (_, values) = map_entries(m);
    match values {
        Column::Utf8(c) => {
            assert_eq!(c.null_count(), 1);
            let bm = c.validity.as_ref().expect("value validity");
            assert!(bm.is_valid(0) && !bm.is_valid(1) && bm.is_valid(2));
            assert_eq!(c.value(0), b"user_1");
            assert_eq!(c.value(2), b"user_2");
        }
        other => panic!("expected Utf8 values, got {other:?}"),
    }
}

#[test]
fn test_decode_map_array_value() {
    // Map(String, Array(Int32)): the flattened value run is itself an
    // Array column over the entries: its own offsets then the leaf ints.
    // Rows: {a: [13]} / {b: [], c: [1, 2]}.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("m", "Map(String, Array(Int32))")
        .array_offsets(&[1, 3]) // map offsets: 3 entries
        .string_data(&["a", "b", "c"])
        .array_offsets(&[1, 1, 3]) // value-array offsets over 3 entries
        .int32_data(&[13, 1, 2])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let m = as_map(cb.chunks[0].column(0));
    assert_eq!(m.offsets, vec![0i64, 1, 3]);
    let (_, values) = map_entries(m);
    match values {
        Column::Array(arr) => {
            assert_eq!(arr.offsets, vec![0i64, 1, 1, 3]);
            match arr.values.as_ref() {
                Column::Int32(c) => assert_eq!(c.values.as_slice(), &[13, 1, 2]),
                other => panic!("expected Int32 leaf, got {other:?}"),
            }
        }
        other => panic!("expected Array values, got {other:?}"),
    }
}

#[test]
fn test_decode_array_of_map() {
    // Array(Map(String, Int32)): the outer Array offsets count maps; the
    // flattened element column is a Map over the total, with its own
    // offsets counting entries. Rows: [] / [{a: 1}] / [{b: 2}, {}].
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("a", "Array(Map(String, Int32))")
        .array_offsets(&[0, 1, 3]) // outer: 3 flattened maps
        .array_offsets(&[1, 2, 2]) // map offsets over the 3 maps
        .string_data(&["a", "b"])
        .int32_data(&[1, 2])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Array(arr) => {
            assert_eq!(arr.offsets, vec![0i64, 0, 1, 3]);
            let m = as_map(arr.values.as_ref());
            assert_eq!(m.len(), 3);
            assert_eq!(m.offsets, vec![0i64, 1, 2, 2]);
            let (keys, values) = map_entries(m);
            match (keys, values) {
                (Column::Utf8(k), Column::Int32(v)) => {
                    assert_eq!(k.value(0), b"a");
                    assert_eq!(k.value(1), b"b");
                    assert_eq!(v.values.as_slice(), &[1, 2]);
                }
                other => panic!("expected (Utf8, Int32) entries, got {other:?}"),
            }
        }
        other => panic!("expected Array, got {other:?}"),
    }
}

#[test]
fn test_decode_nested_map() {
    // Map(String, Map(String, Int32)): the flattened value run is itself a
    // Map over the outer entries. Rows: {a: {x: 1}} / {b: {y: 2, z: 3}}.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("m", "Map(String, Map(String, Int32))")
        .array_offsets(&[1, 2]) // outer: 2 entries
        .string_data(&["a", "b"]) // outer keys
        .array_offsets(&[1, 3]) // inner map offsets over the 2 entries
        .string_data(&["x", "y", "z"]) // inner keys
        .int32_data(&[1, 2, 3]) // inner values
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let outer = as_map(cb.chunks[0].column(0));
    assert_eq!(outer.offsets, vec![0i64, 1, 2]);
    let (_, outer_values) = map_entries(outer);
    let inner = as_map(outer_values);
    assert_eq!(inner.offsets, vec![0i64, 1, 3]);
    let (inner_keys, inner_values) = map_entries(inner);
    match (inner_keys, inner_values) {
        (Column::Utf8(k), Column::Int32(v)) => {
            assert_eq!(k.value(0), b"x");
            assert_eq!(k.value(2), b"z");
            assert_eq!(v.values.as_slice(), &[1, 2, 3]);
        }
        other => panic!("expected (Utf8, Int32) inner entries, got {other:?}"),
    }
}

#[test]
fn test_decode_map_zero_rows() {
    // A zero-row block carries only the header: no prefix, no offsets, no
    // entry runs.
    for type_name in ["Map(String, Int32)", "Map(LowCardinality(String), UInt8)"] {
        let data = BlockBuilder::new()
            .header(1, 0)
            .column_header("m", type_name)
            .build();
        let batch = decode_next_block(&mut ByteReader::new(&data), &DecodeOptions::default())
            .unwrap()
            .unwrap();
        let m = as_map(batch.column(0));
        assert_eq!(m.len(), 0);
        assert_eq!(m.offsets, vec![0i64]);
        let (keys, values) = map_entries(m);
        assert_eq!(keys.len(), 0);
        assert_eq!(values.len(), 0);
        assert_eq!(
            block_end(&data, &DecodeOptions::default()).unwrap(),
            Some(data.len())
        );
    }
}

#[test]
fn test_multi_block_map_kept_as_chunks() {
    let mut data = BlockBuilder::new()
        .header(1, 2)
        .column_header("m", "Map(String, Int32)")
        .array_offsets(&[1, 2])
        .string_data(&["a", "b"])
        .int32_data(&[13, 79])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 1)
            .column_header("m", "Map(String, Int32)")
            .array_offsets(&[1])
            .string_data(&["c"])
            .int32_data(&[-7])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(cb.num_rows(), 3);
    let m1 = as_map(cb.chunks[1].column(0));
    assert_eq!(m1.offsets, vec![0i64, 1]);
    let (_, values) = map_entries(m1);
    match values {
        Column::Int32(c) => assert_eq!(c.values.as_slice(), &[-7]),
        other => panic!("expected Int32 values, got {other:?}"),
    }
}

#[test]
fn test_map_all_empty_lc_key_has_no_body() {
    // Map(LowCardinality(String), Int32) with rows > 0 but every map empty:
    // the hoisted LC key version and the all-zero offsets are the whole
    // column; the key and value runs are entirely absent (limit == 0 gates
    // through the Map path).
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("m", "Map(LowCardinality(String), Int32)")
        .uint64_data(&[LOW_CARDINALITY_KEY_VERSION])
        .array_offsets(&[0, 0, 0])
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let m = as_map(cb.chunks[0].column(0));
    assert_eq!(m.offsets, vec![0i64, 0, 0, 0]);
    let (keys, values) = map_entries(m);
    match keys {
        Column::Dictionary(d) => {
            assert!(d.indices.is_empty());
            assert_eq!(d.values.len(), 0);
        }
        other => panic!("expected empty Dictionary keys, got {other:?}"),
    }
    assert_eq!(values.len(), 0);
}

#[test]
fn test_map_truncated_is_eof_not_panic() {
    // Truncate at several points (inside the value run, inside the key
    // run, inside the offsets): decode and scan must both report "need
    // more bytes" (UnexpectedEof), never panic.
    let full = BlockBuilder::new()
        .header(1, 3)
        .column_header("m", "Map(String, Int32)")
        .array_offsets(&[1, 2, 4])
        .string_data(&["a", "b", "c", "d"])
        .int32_data(&[1, 2, 3, 4])
        .build();

    for end in [full.len() - 1, full.len() - 17, full.len() - 30] {
        let truncated = &full[..end];
        assert!(matches!(
            decode_all_bytes(truncated, &DecodeOptions::default()),
            Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert!(matches!(
            block_end(truncated, &DecodeOptions::default()),
            Err(DecodeError::Io(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
    }
}

#[test]
fn test_map_illegal_headers_rejected() {
    // Nullable-key and LowCardinality(Nullable)-key maps violate the
    // server's DataTypeMap::isValidKeyType and are rejected at header time
    // on both paths; LowCardinality(Map) violates
    // canBeInsideLowCardinality. All regardless of row count.
    // (Nullable(Map) is rejected by the parser itself; see
    // test_parse_ch_type_map.)
    for bad in [
        "Map(Nullable(String), Int32)",
        "Map(LowCardinality(Nullable(String)), Int32)",
        "LowCardinality(Map(String, Int32))",
        "Array(Map(Nullable(String), Int32))",
    ] {
        for num_rows in [0usize, 1] {
            let data = BlockBuilder::new()
                .header(1, num_rows)
                .column_header("m", bad)
                .build();
            assert!(
                matches!(
                    decode_all_bytes(&data, &DecodeOptions::default()),
                    Err(DecodeError::UnsupportedType { .. })
                ),
                "decode should reject {bad:?} at {num_rows} rows"
            );
            assert!(
                matches!(
                    block_end(&data, &DecodeOptions::default()),
                    Err(DecodeError::UnsupportedType { .. })
                ),
                "scan should reject {bad:?} at {num_rows} rows"
            );
        }
    }
}

#[test]
fn test_low_cardinality_tuple_inner_rejected() {
    // LowCardinality(Tuple(...)) is illegal (Tuple inherits
    // canBeInsideLowCardinality() == false), rejected at header time on
    // both paths regardless of row count.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("lc", "LowCardinality(Tuple(Int32, String))")
            .build();
        assert!(matches!(
            decode_all_bytes(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
        assert!(matches!(
            block_end(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
    }
}

// -----------------------------------------------------------------------
// SimpleAggregateFunction / geo aliases / Nested (name-decoration types)
// -----------------------------------------------------------------------

#[test]
fn test_parse_ch_type_simple_aggregate_function() {
    // Plain scalar inner.
    assert_eq!(
        parse_ch_type("SimpleAggregateFunction(sum, Float64)"),
        Some(ChType::SimpleAggregateFunction {
            func: "sum".to_string(),
            inner: Box::new(ChType::Float64),
        })
    );
    // Function name with parenthesized literal params: the split is on the
    // FIRST top-level comma, so the params stay with the function name.
    assert_eq!(
        parse_ch_type("SimpleAggregateFunction(groupArrayLastArray(5), Array(UInt64))"),
        Some(ChType::SimpleAggregateFunction {
            func: "groupArrayLastArray(5)".to_string(),
            inner: Box::new(ChType::Array(Box::new(ChType::UInt64))),
        })
    );
    // Inner Tuple whose own commas sit inside parentheses.
    assert_eq!(
        parse_ch_type("SimpleAggregateFunction(sumMap, Tuple(Array(Int32), Array(Int64)))"),
        Some(ChType::SimpleAggregateFunction {
            func: "sumMap".to_string(),
            inner: Box::new(ChType::Tuple(vec![
                (None, ChType::Array(Box::new(ChType::Int32))),
                (None, ChType::Array(Box::new(ChType::Int64))),
            ])),
        })
    );
    // A whitelisted underscore-bearing function name.
    assert_eq!(
        parse_ch_type("SimpleAggregateFunction(anyLast_respect_nulls, String)"),
        Some(ChType::SimpleAggregateFunction {
            func: "anyLast_respect_nulls".to_string(),
            inner: Box::new(ChType::String),
        })
    );
}

#[test]
fn test_simple_aggregate_function_display_round_trips() {
    for spelling in [
        "SimpleAggregateFunction(sum, Float64)",
        "SimpleAggregateFunction(anyLast, LowCardinality(Nullable(String)))",
        "SimpleAggregateFunction(groupArrayLastArray(5), Array(UInt64))",
        "SimpleAggregateFunction(sumMap, Tuple(Array(Int32), Array(Int64)))",
    ] {
        let parsed = parse_ch_type(spelling).expect("parses");
        assert_eq!(parsed.to_string(), spelling, "round-trip for {spelling}");
    }
}

#[test]
fn test_parse_simple_aggregate_function_rejections() {
    // Multi-type-arg form: only T1 is load-bearing and it is unobserved, so
    // reject rather than decode a guess.
    assert_eq!(
        parse_ch_type("SimpleAggregateFunction(sum, Int32, Int64)"),
        None
    );
    // Missing the type argument.
    assert_eq!(parse_ch_type("SimpleAggregateFunction(sum)"), None);
    // A non-identifier-shaped function name.
    assert_eq!(parse_ch_type("SimpleAggregateFunction(1sum, Int32)"), None);
}

#[test]
fn test_parse_simple_aggregate_function_inside_wrappers() {
    // SAF parses at any nesting position: the server emits the SAF spelling
    // verbatim inside wrappers and containers (confirmed live at
    // v26.6.1.1193-stable via CREATE + SELECT ... FORMAT Native hexdump for
    // each of these shapes).
    assert_eq!(
        parse_ch_type("Nullable(SimpleAggregateFunction(sum, UInt64))"),
        Some(ChType::Nullable(Box::new(
            ChType::SimpleAggregateFunction {
                func: "sum".to_string(),
                inner: Box::new(ChType::UInt64),
            }
        )))
    );
    assert_eq!(
        parse_ch_type("Array(SimpleAggregateFunction(sum, UInt64))"),
        Some(ChType::Array(Box::new(ChType::SimpleAggregateFunction {
            func: "sum".to_string(),
            inner: Box::new(ChType::UInt64),
        })))
    );
    assert_eq!(
        parse_ch_type("LowCardinality(SimpleAggregateFunction(anyLast, String))"),
        Some(ChType::LowCardinality(Box::new(
            ChType::SimpleAggregateFunction {
                func: "anyLast".to_string(),
                inner: Box::new(ChType::String),
            }
        )))
    );
    assert_eq!(
        parse_ch_type("Tuple(v SimpleAggregateFunction(sum, UInt64))"),
        Some(ChType::Tuple(vec![(
            Some("v".to_string()),
            ChType::SimpleAggregateFunction {
                func: "sum".to_string(),
                inner: Box::new(ChType::UInt64),
            }
        )]))
    );
    assert_eq!(
        parse_ch_type("Map(String, SimpleAggregateFunction(sum, UInt64))"),
        Some(ChType::Map(
            Box::new(ChType::String),
            Box::new(ChType::SimpleAggregateFunction {
                func: "sum".to_string(),
                inner: Box::new(ChType::UInt64),
            })
        ))
    );
    // Wrapper legality delegates to the inner: Nullable(SAF(Array(...))) is
    // illegal because Nullable(Array(...)) is (the delegate is an Array), and a
    // SAF whose delegate is a Nullable cannot sit inside another Nullable.
    assert_eq!(
        parse_ch_type("Nullable(SimpleAggregateFunction(groupArrayArray, Array(UInt64)))"),
        None
    );
    assert_eq!(
        parse_ch_type("Nullable(SimpleAggregateFunction(anyLast, Nullable(String)))"),
        None
    );
}

#[test]
fn test_parse_ch_type_geo() {
    assert_eq!(parse_ch_type("Point"), Some(ChType::Geo(GeoKind::Point)));
    assert_eq!(parse_ch_type("Ring"), Some(ChType::Geo(GeoKind::Ring)));
    assert_eq!(
        parse_ch_type("LineString"),
        Some(ChType::Geo(GeoKind::LineString))
    );
    assert_eq!(
        parse_ch_type("MultiLineString"),
        Some(ChType::Geo(GeoKind::MultiLineString))
    );
    assert_eq!(
        parse_ch_type("Polygon"),
        Some(ChType::Geo(GeoKind::Polygon))
    );
    assert_eq!(
        parse_ch_type("MultiPolygon"),
        Some(ChType::Geo(GeoKind::MultiPolygon))
    );
}

#[test]
fn test_geo_display_round_trips() {
    for spelling in [
        "Point",
        "Ring",
        "LineString",
        "MultiLineString",
        "Polygon",
        "MultiPolygon",
    ] {
        assert_eq!(parse_ch_type(spelling).unwrap().to_string(), spelling);
    }
    // A geo type composes inside containers and renders the bare alias.
    assert_eq!(
        parse_ch_type("Array(Point)").unwrap().to_string(),
        "Array(Point)"
    );
    assert_eq!(
        parse_ch_type("Map(Point, MultiPolygon)")
            .unwrap()
            .to_string(),
        "Map(Point, MultiPolygon)"
    );
}

#[test]
fn test_parse_geo_rejects_bad_casing() {
    // Registration is case-sensitive with no aliases.
    assert_eq!(parse_ch_type("point"), None);
    assert_eq!(parse_ch_type("ring"), None);
    assert_eq!(parse_ch_type("POLYGON"), None);
    assert_eq!(parse_ch_type("multipolygon"), None);
}

#[test]
fn test_geo_underlying_type_expansion() {
    // The one-directional structural mapping, confirmed against
    // DataTypeCustomGeo.
    let point = ChType::Tuple(vec![(None, ChType::Float64), (None, ChType::Float64)]);
    assert_eq!(GeoKind::Point.underlying_type(), point);
    assert_eq!(
        GeoKind::Ring.underlying_type(),
        ChType::Array(Box::new(point.clone()))
    );
    assert_eq!(
        GeoKind::LineString.underlying_type(),
        ChType::Array(Box::new(point.clone()))
    );
    assert_eq!(
        GeoKind::Polygon.underlying_type(),
        ChType::Array(Box::new(ChType::Array(Box::new(point.clone()))))
    );
    assert_eq!(
        GeoKind::MultiLineString.underlying_type(),
        ChType::Array(Box::new(ChType::Array(Box::new(point.clone()))))
    );
    assert_eq!(
        GeoKind::MultiPolygon.underlying_type(),
        ChType::Array(Box::new(ChType::Array(Box::new(ChType::Array(Box::new(
            point
        ))))))
    );
}

#[test]
fn test_parse_ch_type_nested() {
    assert_eq!(
        parse_ch_type("Nested(a UInt32, b String)"),
        Some(ChType::Nested(vec![
            ("a".to_string(), ChType::UInt32),
            ("b".to_string(), ChType::String),
        ]))
    );
    // A backtick-quoted name and a nested type argument.
    assert_eq!(
        parse_ch_type("Nested(`a b` UInt32, c Array(Nullable(String)))"),
        Some(ChType::Nested(vec![
            ("a b".to_string(), ChType::UInt32),
            (
                "c".to_string(),
                ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::String)))),
            ),
        ]))
    );
}

#[test]
fn test_nested_display_round_trips() {
    for spelling in [
        "Nested(a UInt32, b String)",
        "Nested(`a b` UInt32, c Array(Nullable(String)))",
        "Nested(inner Nested(x Int32, y Int32))",
    ] {
        assert_eq!(parse_ch_type(spelling).unwrap().to_string(), spelling);
    }
}

#[test]
fn test_parse_nested_rejections() {
    // Element names are mandatory.
    assert_eq!(parse_ch_type("Nested(UInt32)"), None);
    assert_eq!(parse_ch_type("Nested(a UInt32, String)"), None);
    // An empty field list is a parse error.
    assert_eq!(parse_ch_type("Nested()"), None);
}

#[test]
fn test_parse_nullable_geo_legality() {
    // Nullable(Point) is legal (Point is a Tuple, canBeInsideNullable true).
    assert_eq!(
        parse_ch_type("Nullable(Point)"),
        Some(ChType::Nullable(Box::new(ChType::Geo(GeoKind::Point))))
    );
    // Nullable of the five Array-based geo kinds is illegal (Array is not
    // nullable-able).
    assert_eq!(parse_ch_type("Nullable(Ring)"), None);
    assert_eq!(parse_ch_type("Nullable(LineString)"), None);
    assert_eq!(parse_ch_type("Nullable(Polygon)"), None);
    assert_eq!(parse_ch_type("Nullable(MultiLineString)"), None);
    assert_eq!(parse_ch_type("Nullable(MultiPolygon)"), None);
    // Nullable(Nested) is illegal (it is an Array).
    assert_eq!(parse_ch_type("Nullable(Nested(a UInt32))"), None);
}

#[test]
fn test_decode_simple_aggregate_function_scalar() {
    // Wire bytes are byte-identical to the bare inner Float64.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("s", "SimpleAggregateFunction(sum, Float64)")
        .float64_data(&[3.5, -7.25, 0.0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::SimpleAggregateFunction {
            func: "sum".to_string(),
            inner: Box::new(ChType::Float64),
        }
    );
    match cb.chunks[0].column(0) {
        Column::Float64(c) => assert_eq!(c.values, vec![3.5, -7.25, 0.0]),
        other => panic!("expected Float64 delegate column, got {other:?}"),
    }
}

#[test]
fn test_decode_simple_aggregate_function_nullable_string() {
    // SAF(anyLast, Nullable(String)) decodes exactly as Nullable(String):
    // the per-row null map then the string run.
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("s", "SimpleAggregateFunction(anyLast, Nullable(String))")
        .null_map(&[false, true, false])
        .string_data(&["user_1", "", "user_2"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Utf8(c) => {
            assert_eq!(c.value(0), b"user_1");
            assert_eq!(c.value(2), b"user_2");
            assert_eq!(c.null_count(), 1);
            let bm = c.validity.as_ref().expect("nullable validity");
            assert!(bm.is_valid(0) && !bm.is_valid(1) && bm.is_valid(2));
        }
        other => panic!("expected Utf8 delegate column, got {other:?}"),
    }
}

#[test]
fn test_decode_simple_aggregate_function_over_low_cardinality() {
    // Shared gate: SAF over LowCardinality(String) hoists the LC 8-byte key
    // version to the front through the delegate, then the LC body.
    let dictionary = ["", "user_1", "user_2"];
    let indices = [1u64, 2, 1];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header(
            "s",
            "SimpleAggregateFunction(anyLast, LowCardinality(String))",
        )
        .low_cardinality_string(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    match cb.chunks[0].column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![1, 2, 1]);
            assert_eq!(
                lc_value(cb.chunks[0].column(0), 0).as_deref(),
                Some(&b"user_1"[..])
            );
        }
        other => panic!("expected Dictionary delegate column, got {other:?}"),
    }
}

#[test]
fn test_decode_point_plain() {
    // Point = Tuple(Float64, Float64), field-major: all X then all Y.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("p", "Point")
        .float64_data(&[1.0, 3.0]) // X coordinates
        .float64_data(&[2.0, 4.0]) // Y coordinates
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.schema.fields[0].ch_type, ChType::Geo(GeoKind::Point));
    let t = as_tuple(cb.chunks[0].column(0));
    assert_eq!(t.fields.len(), 2);
    match (&t.fields[0], &t.fields[1]) {
        (Column::Float64(x), Column::Float64(y)) => {
            assert_eq!(x.values, vec![1.0, 3.0]);
            assert_eq!(y.values, vec![2.0, 4.0]);
        }
        other => panic!("expected two Float64 tuple fields, got {other:?}"),
    }
}

#[test]
fn test_decode_nullable_point() {
    // Nullable(Point): null map then the Tuple(Float64, Float64) body.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("p", "Nullable(Point)")
        .null_map(&[false, true])
        .float64_data(&[1.0, 0.0])
        .float64_data(&[2.0, 0.0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let t = as_tuple(cb.chunks[0].column(0));
    assert_eq!(t.len(), 2);
    let bm = t.validity.as_ref().expect("tuple-level validity");
    assert!(bm.is_valid(0) && !bm.is_valid(1));
}

#[test]
fn test_decode_ring() {
    // Ring = Array(Point): offsets then the flattened Point body (field-major
    // over all points).
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("r", "Ring")
        .array_offsets(&[2, 3]) // row 0 has 2 points, row 1 has 1 point
        .float64_data(&[1.0, 3.0, 5.0]) // X for all 3 points
        .float64_data(&[2.0, 4.0, 6.0]) // Y for all 3 points
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    let t = as_tuple(arr.values.as_ref());
    match (&t.fields[0], &t.fields[1]) {
        (Column::Float64(x), Column::Float64(y)) => {
            assert_eq!(x.values, vec![1.0, 3.0, 5.0]);
            assert_eq!(y.values, vec![2.0, 4.0, 6.0]);
        }
        other => panic!("expected Point tuple fields, got {other:?}"),
    }
}

#[test]
fn test_decode_multi_polygon() {
    // MultiPolygon = Array(Array(Array(Point))): three offset levels then the
    // Point body. One row holding one polygon of one ring of two points.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("mp", "MultiPolygon")
        .array_offsets(&[1]) // 1 polygon in the row
        .array_offsets(&[1]) // 1 ring in the polygon
        .array_offsets(&[2]) // 2 points in the ring
        .float64_data(&[1.0, 3.0])
        .float64_data(&[2.0, 4.0])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let l0 = as_array(cb.chunks[0].column(0));
    assert_eq!(l0.offsets, vec![0i64, 1]);
    let l1 = as_array(l0.values.as_ref());
    assert_eq!(l1.offsets, vec![0i64, 1]);
    let l2 = as_array(l1.values.as_ref());
    assert_eq!(l2.offsets, vec![0i64, 2]);
    let t = as_tuple(l2.values.as_ref());
    match (&t.fields[0], &t.fields[1]) {
        (Column::Float64(x), Column::Float64(y)) => {
            assert_eq!(x.values, vec![1.0, 3.0]);
            assert_eq!(y.values, vec![2.0, 4.0]);
        }
        other => panic!("expected Point tuple fields, got {other:?}"),
    }
}

#[test]
fn test_decode_nested_plain() {
    // Nested(a UInt32, b String) = Array(Tuple(a UInt32, b String)): offsets,
    // then the flattened tuple body (all a's then all b's), field-major.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("n", "Nested(a UInt32, b String)")
        .array_offsets(&[2, 3]) // row 0 has 2 elements, row 1 has 1
        .uint32_data(&[10, 20, 30])
        .string_data(&["x", "y", "z"])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::Nested(vec![
            ("a".to_string(), ChType::UInt32),
            ("b".to_string(), ChType::String),
        ])
    );
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    let t = as_tuple(arr.values.as_ref());
    match (&t.fields[0], &t.fields[1]) {
        (Column::UInt32(a), Column::Utf8(b)) => {
            assert_eq!(a.values, vec![10, 20, 30]);
            assert_eq!(b.value(0), b"x");
            assert_eq!(b.value(2), b"z");
        }
        other => panic!("expected (UInt32, Utf8) tuple fields, got {other:?}"),
    }
}

#[test]
fn test_decode_nested_with_low_cardinality_hoists_key_version() {
    // Shared gate: Nested(a LowCardinality(String)) delegates to
    // Array(Tuple(a LowCardinality(String))). The state prefix recurses
    // Array -> Tuple -> LowCardinality, so the LC 8-byte key version is
    // hoisted to the very front of the column, before the offsets, then the
    // LC body follows the offsets.
    let dictionary = ["", "user_1", "user_2"];
    let element_indices = [1u64, 2, 1];
    let lc_full = BlockBuilder::new()
        .low_cardinality_string(&dictionary, &element_indices, 1)
        .build();
    let (key_version, lc_body) = lc_full.split_at(8);

    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("n", "Nested(a LowCardinality(String))")
        .raw_bytes(key_version) // hoisted LC key version, ahead of the offsets
        .array_offsets(&[2, 3])
        .raw_bytes(lc_body)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    let t = as_tuple(arr.values.as_ref());
    match &t.fields[0] {
        Column::Dictionary(d) => assert_eq!(d.indices, vec![1, 2, 1]),
        other => panic!("expected LC dictionary element, got {other:?}"),
    }
    // The scan agrees on the framing (including the hoisted prefix).
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_decode_name_decoration_zero_rows() {
    // A zero-row block carrying the three alias groups contributes the schema
    // but no chunks; the empty columns delegate to the physical layout.
    let data = BlockBuilder::new()
        .header(3, 0)
        .column_header("s", "SimpleAggregateFunction(sum, Float64)")
        .column_header("p", "Point")
        .column_header("n", "Nested(a UInt32, b String)")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 3);
    assert_eq!(cb.schema.fields[1].ch_type, ChType::Geo(GeoKind::Point));
}

#[test]
fn test_decode_alias_over_wrapper_zero_rows() {
    // A zero-row block whose header is a name-decoration alias OVER a
    // Nullable/geo/Nested inner must build an empty column via the physical
    // delegate, never reach the `empty_column` `unreachable!` arm. Before the
    // Fix, `SimpleAggregateFunction(anyLast, Nullable(String))` (and the SAF
    // over Point/Nested shapes) panicked on this untrusted 0-row header.
    let data = BlockBuilder::new()
        .header(4, 0)
        // SAF over a Nullable inner: delegate is Nullable(String).
        .column_header("s", "SimpleAggregateFunction(anyLast, Nullable(String))")
        // SAF over a geo inner: delegate is Geo(Point) -> Tuple(Float64, Float64).
        .column_header("g", "SimpleAggregateFunction(anyLast, Point)")
        // A Nested field group.
        .column_header("n", "Nested(a UInt32, b String)")
        // Alias legal directly inside Nullable, expanded post-unwrap.
        .column_header("np", "Nullable(Point)")
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_rows(), 0);
    assert_eq!(cb.num_chunks(), 0);
    assert_eq!(cb.num_columns(), 4);
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::SimpleAggregateFunction {
            func: "anyLast".to_string(),
            inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
        }
    );
    // The block_end completeness scan agrees the zero-row block is complete.
    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
}

#[test]
fn test_multi_block_point_kept_as_chunks() {
    // Geo blocks stay separate chunks, never concatenated.
    let mut data = BlockBuilder::new()
        .header(1, 1)
        .column_header("p", "Point")
        .float64_data(&[1.0])
        .float64_data(&[2.0])
        .build();
    data.extend(
        BlockBuilder::new()
            .header(1, 2)
            .column_header("p", "Point")
            .float64_data(&[3.0, 5.0])
            .float64_data(&[4.0, 6.0])
            .build(),
    );

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_chunks(), 2);
    assert_eq!(as_tuple(cb.chunks[0].column(0)).len(), 1);
    assert_eq!(as_tuple(cb.chunks[1].column(0)).len(), 2);
}

#[test]
fn test_block_end_scans_name_decoration_types() {
    // The completeness scan walks the same bytes the decoders consume for all
    // three alias groups, ending exactly at the block boundary.
    let dictionary = ["", "user_1"];
    let lc_full = BlockBuilder::new()
        .low_cardinality_string(&dictionary, &[1u64], 1)
        .build();
    let (key_version, lc_body) = lc_full.split_at(8);
    let data = BlockBuilder::new()
        .header(3, 1)
        // Column 0: SAF body is one Float64 (each header is immediately
        // followed by its own data, per the Native per-column framing).
        .column_header("s", "SimpleAggregateFunction(sum, Float64)")
        .float64_data(&[3.5])
        // Column 1: MultiPolygon, three offset levels then one Point.
        .column_header("mp", "MultiPolygon")
        .array_offsets(&[1])
        .array_offsets(&[1])
        .array_offsets(&[1])
        .float64_data(&[1.0])
        .float64_data(&[2.0])
        // Column 2: Nested, hoisted LC key version, offsets, LC body.
        .column_header("n", "Nested(a LowCardinality(String))")
        .raw_bytes(key_version)
        .array_offsets(&[1])
        .raw_bytes(lc_body)
        .build();

    assert_eq!(
        block_end(&data, &DecodeOptions::default()).unwrap(),
        Some(data.len())
    );
    // And a full decode consumes it without error.
    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(cb.num_columns(), 3);
}

#[test]
fn test_decode_rejects_low_cardinality_geo() {
    // LowCardinality is illegal for all six geo kinds (no canBeInsideLowCardinality
    // override), rejected at header time regardless of row count.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("lc", "LowCardinality(Point)")
            .build();
        assert!(matches!(
            decode_all_bytes(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
        assert!(matches!(
            block_end(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
    }
}

#[test]
fn test_decode_nullable_simple_aggregate_function() {
    // Nullable(SAF(sum, UInt64)) decodes exactly as Nullable(UInt64): the
    // per-row null map then the UInt64 run. The SAF is name decoration inside
    // the Nullable (confirmed legal live at v26.6.1.1193-stable).
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header("s", "Nullable(SimpleAggregateFunction(sum, UInt64))")
        .null_map(&[false, true, false])
        .uint64_data(&[13, 0, 79])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::Nullable(Box::new(ChType::SimpleAggregateFunction {
            func: "sum".to_string(),
            inner: Box::new(ChType::UInt64),
        }))
    );
    match cb.chunks[0].column(0) {
        Column::UInt64(c) => {
            assert_eq!(c.values, vec![13, 0, 79]);
            let bm = c.validity.as_ref().expect("nullable validity");
            assert!(bm.is_valid(0) && !bm.is_valid(1) && bm.is_valid(2));
        }
        other => panic!("expected UInt64 delegate column, got {other:?}"),
    }
}

#[test]
fn test_decode_array_simple_aggregate_function() {
    // Array(SAF(sum, UInt64)) decodes exactly as Array(UInt64): offsets then
    // the flattened UInt64 element run.
    let data = BlockBuilder::new()
        .header(1, 2)
        .column_header("a", "Array(SimpleAggregateFunction(sum, UInt64))")
        .array_offsets(&[2, 3]) // row 0 has 2 elements, row 1 has 1
        .uint64_data(&[13, 79, 5])
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    let arr = as_array(cb.chunks[0].column(0));
    assert_eq!(arr.offsets, vec![0i64, 2, 3]);
    match arr.values.as_ref() {
        Column::UInt64(c) => assert_eq!(c.values, vec![13, 79, 5]),
        other => panic!("expected UInt64 element column, got {other:?}"),
    }
}

#[test]
fn test_decode_low_cardinality_simple_aggregate_function() {
    // LowCardinality(SAF(anyLast, String)) decodes exactly as
    // LowCardinality(String): the key version prefix (in the helper), the
    // per-block dictionary, and the indexes.
    let dictionary = ["", "user_1", "user_2"];
    let indices = [1u64, 2, 1];
    let data = BlockBuilder::new()
        .header(1, 3)
        .column_header(
            "s",
            "LowCardinality(SimpleAggregateFunction(anyLast, String))",
        )
        .low_cardinality_string(&dictionary, &indices, 1)
        .build();

    let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
    assert_eq!(
        cb.schema.fields[0].ch_type,
        ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
            func: "anyLast".to_string(),
            inner: Box::new(ChType::String),
        }))
    );
    match cb.chunks[0].column(0) {
        Column::Dictionary(d) => {
            assert_eq!(d.indices, vec![1, 2, 1]);
            assert_eq!(
                lc_value(cb.chunks[0].column(0), 0).as_deref(),
                Some(&b"user_1"[..])
            );
        }
        other => panic!("expected Dictionary delegate column, got {other:?}"),
    }
}

#[test]
fn test_decode_rejects_unnamed_nested_element_header() {
    // An unnamed Nested element makes the whole header unsupported.
    let data = BlockBuilder::new()
        .header(1, 1)
        .column_header("n", "Nested(UInt32)")
        .build();
    assert!(matches!(
        decode_all_bytes(&data, &DecodeOptions::default()),
        Err(DecodeError::UnsupportedType { .. })
    ));
}

#[test]
fn test_decode_rejects_nested_low_cardinality_bad_inner() {
    // A forbidden LowCardinality inner nested inside a Nested field is
    // rejected at header time on both paths, at every row count, because
    // validate_header_type expands the Nested delegate and recurses.
    for num_rows in [0usize, 1] {
        let data = BlockBuilder::new()
            .header(1, num_rows)
            .column_header("n", "Nested(a LowCardinality(Decimal(9, 4)))")
            .build();
        assert!(matches!(
            decode_all_bytes(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
        assert!(matches!(
            block_end(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
    }
}
