use std::io::{self, Read};
use std::sync::Arc;

use crate::batch::{ChunkedBatch, ColBatch};
use crate::bitmap::Bitmap;
use crate::column::{
    BoolColumn, Column, FixedBinaryColumn, PrimitiveColumn, Utf8Column,
};
use crate::native::varint::{read_varint, read_varint_string};
use crate::schema::{ChType, Field, Schema};

/// Errors that can occur during Native format decoding.
#[derive(Debug)]
pub enum DecodeError {
    Io(io::Error),
    UnsupportedType { column: String, type_name: String },
}

impl From<io::Error> for DecodeError {
    fn from(e: io::Error) -> Self {
        DecodeError::Io(e)
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Io(e) => write!(f, "IO error: {e}"),
            DecodeError::UnsupportedType { column, type_name } => {
                write!(f, "Unsupported ClickHouse type '{type_name}' for column '{column}'")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// Options for Native format decoding.
pub struct DecodeOptions {
    pub has_block_info: bool,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self { has_block_info: false }
    }
}

// ---------------------------------------------------------------------------
// Type name parsing
// ---------------------------------------------------------------------------

/// Parse a ClickHouse type name string into a ChType.
fn parse_ch_type(type_name: &str) -> Option<ChType> {
    // Nullable wrapper
    if let Some(inner) = type_name.strip_prefix("Nullable(") {
        if let Some(inner) = inner.strip_suffix(')') {
            return parse_ch_type(inner).map(|t| ChType::Nullable(Box::new(t)));
        }
    }

    // FixedString(N)
    if let Some(n_str) = type_name.strip_prefix("FixedString(") {
        if let Some(n_str) = n_str.strip_suffix(')') {
            if let Ok(n) = n_str.trim().parse::<usize>() {
                return Some(ChType::FixedString(n));
            }
        }
    }

    match type_name {
        "Bool" | "Boolean" => Some(ChType::Bool),
        "Int8" => Some(ChType::Int8),
        "Int16" => Some(ChType::Int16),
        "Int32" => Some(ChType::Int32),
        "Int64" => Some(ChType::Int64),
        "UInt8" => Some(ChType::UInt8),
        "UInt16" => Some(ChType::UInt16),
        "UInt32" => Some(ChType::UInt32),
        "UInt64" => Some(ChType::UInt64),
        "Float32" => Some(ChType::Float32),
        "Float64" => Some(ChType::Float64),
        "String" => Some(ChType::String),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Column decoders
// ---------------------------------------------------------------------------

/// Read a null map: 1 byte per row, 0x01 = null.
fn decode_null_map<R: Read>(reader: &mut R, num_rows: usize) -> io::Result<Bitmap> {
    let mut null_bytes = vec![0u8; num_rows];
    reader.read_exact(&mut null_bytes)?;
    Ok(Bitmap::from_ch_null_map(&null_bytes))
}

/// Decode fixed-width primitives by reading bytes straight into a typed buffer.
///
/// On little-endian platforms (x86, ARM) the wire bytes already are the
/// in-memory representation, so we allocate the destination `Vec<T>` and read
/// the bytes directly into its backing store — no per-element loop, no copy.
///
/// The destination is allocated as `Vec<T>` (not a `Vec<u8>` reinterpreted as
/// `Vec<T>`) so the allocation has T's alignment and is freed with T's layout;
/// reinterpreting a `Vec<u8>` allocation as `Vec<T>` is undefined behavior.
macro_rules! decode_primitive {
    ($reader:expr, $num_rows:expr, $ty:ty) => {{
        let num_rows = $num_rows;
        let total_bytes = num_rows * std::mem::size_of::<$ty>();

        #[cfg(target_endian = "little")]
        {
            let mut values: Vec<$ty> = Vec::with_capacity(num_rows);
            // Safety: `with_capacity(num_rows)` reserves exactly
            // `num_rows * size_of::<$ty>()` bytes, correctly aligned for `$ty`.
            // We fill every one of those bytes via `read_exact` before calling
            // `set_len`; on a read error `values` stays length 0 and drops
            // cleanly with the correct layout.
            unsafe {
                let byte_dst = std::slice::from_raw_parts_mut(
                    values.as_mut_ptr() as *mut u8,
                    total_bytes,
                );
                $reader.read_exact(byte_dst)?;
                values.set_len(num_rows);
            }
            values
        }

        // Big-endian fallback: read raw bytes, byte-swap each element.
        #[cfg(target_endian = "big")]
        {
            let mut buf = vec![0u8; total_bytes];
            $reader.read_exact(&mut buf)?;
            let values: Vec<$ty> = buf
                .chunks_exact(std::mem::size_of::<$ty>())
                .map(|chunk| <$ty>::from_le_bytes(chunk.try_into().unwrap()))
                .collect();
            values
        }
    }};
}

fn decode_bool_data<R: Read>(reader: &mut R, num_rows: usize) -> io::Result<BoolColumn> {
    let mut wire_bytes = vec![0u8; num_rows];
    reader.read_exact(&mut wire_bytes)?;
    Ok(BoolColumn::from_wire_bytes(&wire_bytes))
}

fn decode_string_data<R: Read>(reader: &mut R, num_rows: usize) -> io::Result<(Vec<i32>, Vec<u8>)> {
    let mut offsets = Vec::with_capacity(num_rows + 1);
    let mut data = Vec::new();
    let mut offset: i32 = 0;
    offsets.push(offset);

    for _ in 0..num_rows {
        let len = read_varint(reader)? as usize;
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf)?;
        data.extend_from_slice(&buf);
        offset += len as i32;
        offsets.push(offset);
    }

    Ok((offsets, data))
}

fn decode_fixed_binary_data<R: Read>(
    reader: &mut R,
    num_rows: usize,
    width: usize,
) -> io::Result<Vec<u8>> {
    let mut data = vec![0u8; num_rows * width];
    reader.read_exact(&mut data)?;
    Ok(data)
}

/// Decode a single column given its ChType.
fn decode_column<R: Read>(
    reader: &mut R,
    col_name: &str,
    ch_type: &ChType,
    num_rows: usize,
) -> Result<Column, DecodeError> {
    let (nullable, inner) = match ch_type {
        ChType::Nullable(inner) => (true, inner.as_ref()),
        other => (false, other),
    };

    let validity = if nullable {
        Some(decode_null_map(reader, num_rows)?)
    } else {
        None
    };

    let column = match inner {
        ChType::Bool => {
            let mut col = decode_bool_data(reader, num_rows)?;
            col.validity = validity;
            Column::Bool(col)
        }
        ChType::Int8 => {
            let values = decode_primitive!(reader, num_rows, i8);
            Column::Int8(PrimitiveColumn { values, validity })
        }
        ChType::Int16 => {
            let values = decode_primitive!(reader, num_rows, i16);
            Column::Int16(PrimitiveColumn { values, validity })
        }
        ChType::Int32 => {
            let values = decode_primitive!(reader, num_rows, i32);
            Column::Int32(PrimitiveColumn { values, validity })
        }
        ChType::Int64 => {
            let values = decode_primitive!(reader, num_rows, i64);
            Column::Int64(PrimitiveColumn { values, validity })
        }
        ChType::UInt8 => {
            let values = decode_primitive!(reader, num_rows, u8);
            Column::UInt8(PrimitiveColumn { values, validity })
        }
        ChType::UInt16 => {
            let values = decode_primitive!(reader, num_rows, u16);
            Column::UInt16(PrimitiveColumn { values, validity })
        }
        ChType::UInt32 => {
            let values = decode_primitive!(reader, num_rows, u32);
            Column::UInt32(PrimitiveColumn { values, validity })
        }
        ChType::UInt64 => {
            let values = decode_primitive!(reader, num_rows, u64);
            Column::UInt64(PrimitiveColumn { values, validity })
        }
        ChType::Float32 => {
            let values = decode_primitive!(reader, num_rows, f32);
            Column::Float32(PrimitiveColumn { values, validity })
        }
        ChType::Float64 => {
            let values = decode_primitive!(reader, num_rows, f64);
            Column::Float64(PrimitiveColumn { values, validity })
        }
        ChType::String => {
            let (offsets, data) = decode_string_data(reader, num_rows)?;
            match validity {
                Some(bm) => Column::Utf8(Utf8Column::new_nullable(offsets, data, bm)),
                None => Column::Utf8(Utf8Column::new(offsets, data)),
            }
        }
        ChType::FixedString(width) => {
            let data = decode_fixed_binary_data(reader, num_rows, *width)?;
            match validity {
                Some(bm) => Column::FixedBinary(FixedBinaryColumn::new_nullable(data, *width, bm)),
                None => Column::FixedBinary(FixedBinaryColumn::new(data, *width)),
            }
        }
        ChType::Nullable(_) => unreachable!("Nullable already unwrapped"),
    };

    Ok(column)
}

/// Build an empty column for a given ChType (used for zero-row blocks).
fn empty_column(ch_type: &ChType) -> Column {
    let (nullable, inner) = match ch_type {
        ChType::Nullable(inner) => (true, inner.as_ref()),
        other => (false, other),
    };
    let empty_validity = if nullable {
        Some(Bitmap::from_ch_null_map(&[]))
    } else {
        None
    };

    match inner {
        ChType::Bool => Column::Bool(if nullable {
            BoolColumn::empty_nullable()
        } else {
            BoolColumn::empty()
        }),
        ChType::Int8 => Column::Int8(PrimitiveColumn { values: vec![], validity: empty_validity }),
        ChType::Int16 => Column::Int16(PrimitiveColumn { values: vec![], validity: empty_validity }),
        ChType::Int32 => Column::Int32(PrimitiveColumn { values: vec![], validity: empty_validity }),
        ChType::Int64 => Column::Int64(PrimitiveColumn { values: vec![], validity: empty_validity }),
        ChType::UInt8 => Column::UInt8(PrimitiveColumn { values: vec![], validity: empty_validity }),
        ChType::UInt16 => Column::UInt16(PrimitiveColumn { values: vec![], validity: empty_validity }),
        ChType::UInt32 => Column::UInt32(PrimitiveColumn { values: vec![], validity: empty_validity }),
        ChType::UInt64 => Column::UInt64(PrimitiveColumn { values: vec![], validity: empty_validity }),
        ChType::Float32 => Column::Float32(PrimitiveColumn { values: vec![], validity: empty_validity }),
        ChType::Float64 => Column::Float64(PrimitiveColumn { values: vec![], validity: empty_validity }),
        ChType::String => Column::Utf8(match empty_validity {
            Some(bm) => Utf8Column::new_nullable(vec![0], vec![], bm),
            None => Utf8Column::new(vec![0], vec![]),
        }),
        ChType::FixedString(width) => Column::FixedBinary(match empty_validity {
            Some(bm) => FixedBinaryColumn::new_nullable(vec![], *width, bm),
            None => FixedBinaryColumn::new(vec![], *width),
        }),
        ChType::Nullable(_) => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Block decode
// ---------------------------------------------------------------------------

/// Decode a single Native format block from a reader.
pub fn decode_next_block<R: Read>(
    reader: &mut R,
    options: &DecodeOptions,
) -> Result<Option<ColBatch>, DecodeError> {
    if options.has_block_info {
        let mut skip = [0u8; 8];
        match reader.read_exact(&mut skip) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }

    let num_cols = match read_varint(reader) {
        Ok(n) => n as usize,
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let num_rows = read_varint(reader)? as usize;

    let mut fields = Vec::with_capacity(num_cols);
    let mut columns = Vec::with_capacity(num_cols);

    for _ in 0..num_cols {
        let col_name = read_varint_string(reader)?;
        let type_name = read_varint_string(reader)?;

        let ch_type = parse_ch_type(&type_name).ok_or_else(|| DecodeError::UnsupportedType {
            column: col_name.clone(),
            type_name: type_name.clone(),
        })?;

        if num_rows == 0 {
            columns.push(empty_column(&ch_type));
        } else {
            columns.push(decode_column(reader, &col_name, &ch_type, num_rows)?);
        }

        fields.push(Field { name: col_name, ch_type });
    }

    let schema = Schema::new(fields);
    Ok(Some(ColBatch::new(schema, columns, num_rows)))
}

/// Decode all blocks from a complete byte buffer into a `ChunkedBatch`.
///
/// Each Native block becomes its own chunk — blocks are NOT concatenated.
/// The schema is taken from the first decoded block (every block of a query
/// shares the same schema). Zero-row blocks contribute the schema but are
/// dropped from the chunk list to keep the chunk stream free of empty batches.
pub fn decode_all_bytes(data: &[u8], options: &DecodeOptions) -> Result<ChunkedBatch, DecodeError> {
    let mut cursor = io::Cursor::new(data);
    let mut schema: Option<Schema> = None;
    let mut chunks: Vec<Arc<ColBatch>> = Vec::new();

    while let Some(batch) = decode_next_block(&mut cursor, options)? {
        if schema.is_none() {
            schema = Some(batch.schema.clone());
        }
        if batch.num_rows > 0 {
            chunks.push(Arc::new(batch));
        }
    }

    let schema = schema.ok_or_else(|| {
        DecodeError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "no blocks in response"))
    })?;

    Ok(ChunkedBatch { schema, chunks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::varint::write_varint;

    struct BlockBuilder {
        buf: Vec<u8>,
    }

    impl BlockBuilder {
        fn new() -> Self { Self { buf: Vec::new() } }

        fn with_block_info(mut self) -> Self {
            self.buf.extend_from_slice(&[0u8; 8]);
            self
        }

        fn header(mut self, num_cols: usize, num_rows: usize) -> Self {
            write_varint(&mut self.buf, num_cols as u64).unwrap();
            write_varint(&mut self.buf, num_rows as u64).unwrap();
            self
        }

        fn column_header(mut self, name: &str, type_name: &str) -> Self {
            let name_bytes = name.as_bytes();
            write_varint(&mut self.buf, name_bytes.len() as u64).unwrap();
            self.buf.extend_from_slice(name_bytes);
            let type_bytes = type_name.as_bytes();
            write_varint(&mut self.buf, type_bytes.len() as u64).unwrap();
            self.buf.extend_from_slice(type_bytes);
            self
        }

        fn raw_bytes(mut self, bytes: &[u8]) -> Self {
            self.buf.extend_from_slice(bytes);
            self
        }

        fn int64_data(mut self, values: &[i64]) -> Self {
            for &v in values { self.buf.extend_from_slice(&v.to_le_bytes()); }
            self
        }

        fn int32_data(mut self, values: &[i32]) -> Self {
            for &v in values { self.buf.extend_from_slice(&v.to_le_bytes()); }
            self
        }

        fn int16_data(mut self, values: &[i16]) -> Self {
            for &v in values { self.buf.extend_from_slice(&v.to_le_bytes()); }
            self
        }

        fn int8_data(mut self, values: &[i8]) -> Self {
            for &v in values { self.buf.extend_from_slice(&v.to_le_bytes()); }
            self
        }

        fn uint64_data(mut self, values: &[u64]) -> Self {
            for &v in values { self.buf.extend_from_slice(&v.to_le_bytes()); }
            self
        }

        fn uint32_data(mut self, values: &[u32]) -> Self {
            for &v in values { self.buf.extend_from_slice(&v.to_le_bytes()); }
            self
        }

        fn float32_data(mut self, values: &[f32]) -> Self {
            for &v in values { self.buf.extend_from_slice(&v.to_le_bytes()); }
            self
        }

        fn float64_data(mut self, values: &[f64]) -> Self {
            for &v in values { self.buf.extend_from_slice(&v.to_le_bytes()); }
            self
        }

        fn string_data(mut self, values: &[&str]) -> Self {
            for &s in values {
                write_varint(&mut self.buf, s.len() as u64).unwrap();
                self.buf.extend_from_slice(s.as_bytes());
            }
            self
        }

        fn null_map(mut self, nulls: &[bool]) -> Self {
            for &is_null in nulls {
                self.buf.push(if is_null { 0x01 } else { 0x00 });
            }
            self
        }

        fn build(self) -> Vec<u8> { self.buf }
    }

    #[test]
    fn test_decode_all_int_widths() {
        let data = BlockBuilder::new()
            .header(4, 2)
            .column_header("a", "Int8").int8_data(&[1, -1])
            .column_header("b", "Int16").int16_data(&[256, -256])
            .column_header("c", "Int32").int32_data(&[70000, -70000])
            .column_header("d", "Int64").int64_data(&[1_000_000_000, -1])
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
            .column_header("a", "UInt8").raw_bytes(&[255u8])
            .column_header("b", "UInt16").raw_bytes(&200u16.to_le_bytes())
            .column_header("c", "UInt32").uint32_data(&[4_000_000_000])
            .column_header("d", "UInt64").uint64_data(&[u64::MAX])
            .build();

        let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
        let batch = &cb.chunks[0];
        match batch.column(0) { Column::UInt8(c) => assert_eq!(c.values[0], 255), _ => panic!() }
        match batch.column(3) { Column::UInt64(c) => assert_eq!(c.values[0], u64::MAX), _ => panic!() }
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
    fn test_unsupported_type() {
        let data = BlockBuilder::new()
            .header(1, 1)
            .column_header("ts", "DateTime")
            .build();
        assert!(matches!(
            decode_all_bytes(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
    }

    #[test]
    fn test_multi_block_kept_as_chunks() {
        // Two Int32 blocks must be kept as two separate chunks, NOT merged.
        let mut data = BlockBuilder::new()
            .header(1, 2)
            .column_header("n", "Int32")
            .int32_data(&[1, 2])
            .build();
        data.extend(BlockBuilder::new()
            .header(1, 3)
            .column_header("n", "Int32")
            .int32_data(&[3, 4, 5])
            .build());

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
    fn test_multi_block_bool_kept_as_chunks() {
        // Bool blocks stay separate — no O(n^2) bitmap re-packing.
        let mut data = BlockBuilder::new()
            .header(1, 3)
            .column_header("b", "Bool")
            .raw_bytes(&[1, 0, 1])
            .build();
        data.extend(BlockBuilder::new()
            .header(1, 2)
            .column_header("b", "Bool")
            .raw_bytes(&[0, 1])
            .build());

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
    fn test_with_block_info() {
        let data = BlockBuilder::new()
            .with_block_info()
            .header(1, 2)
            .column_header("v", "Int64")
            .int64_data(&[77, 88])
            .build();

        let options = DecodeOptions { has_block_info: true };
        let cb = decode_all_bytes(&data, &options).unwrap();
        assert_eq!(cb.num_rows(), 2);
    }
}
