use crate::bitmap::Bitmap;

/// A fixed-width column of primitive values.
#[derive(Debug, Clone)]
pub struct PrimitiveColumn<T: Clone> {
    pub values: Vec<T>,
    pub validity: Option<Bitmap>,
}

impl<T: Clone> PrimitiveColumn<T> {
    pub fn new(values: Vec<T>) -> Self {
        Self { values, validity: None }
    }

    pub fn new_nullable(values: Vec<T>, validity: Bitmap) -> Self {
        Self { values, validity: Some(validity) }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn null_count(&self) -> usize {
        self.validity.as_ref().map_or(0, |b| b.null_count())
    }
}

/// Boolean column stored as a packed bitmap (Arrow-compatible).
///
/// Each bit represents one row. Bit order is LSB within each byte.
/// This matches Arrow's boolean array layout for zero-copy export.
#[derive(Debug, Clone)]
pub struct BoolColumn {
    pub bitmap: Vec<u8>,
    pub len: usize,
    pub validity: Option<Bitmap>,
}

impl BoolColumn {
    /// Create from per-row bytes (ClickHouse wire format: 1 byte per row).
    /// Packs into Arrow-compatible bitmap.
    pub fn from_wire_bytes(bytes: &[u8]) -> Self {
        let len = bytes.len();
        let mut bitmap = vec![0u8; (len + 7) / 8];
        for (i, &b) in bytes.iter().enumerate() {
            if b != 0 {
                bitmap[i / 8] |= 1 << (i % 8);
            }
        }
        Self { bitmap, len, validity: None }
    }

    pub fn from_wire_bytes_nullable(bytes: &[u8], validity: Bitmap) -> Self {
        let mut col = Self::from_wire_bytes(bytes);
        col.validity = Some(validity);
        col
    }

    pub fn empty() -> Self {
        Self { bitmap: vec![], len: 0, validity: None }
    }

    pub fn empty_nullable() -> Self {
        Self { bitmap: vec![], len: 0, validity: Some(Bitmap::from_ch_null_map(&[])) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, index: usize) -> bool {
        (self.bitmap[index / 8] >> (index % 8)) & 1 == 1
    }

    pub fn null_count(&self) -> usize {
        self.validity.as_ref().map_or(0, |b| b.null_count())
    }
}

/// A variable-length UTF-8 string column using Arrow layout.
///
/// Row `i` data is `data[offsets[i]..offsets[i+1]]`.
/// `offsets` has length `num_rows + 1`, with `offsets[0] == 0`.
#[derive(Debug, Clone)]
pub struct Utf8Column {
    pub offsets: Vec<i32>,
    pub data: Vec<u8>,
    pub validity: Option<Bitmap>,
}

impl Utf8Column {
    pub fn new(offsets: Vec<i32>, data: Vec<u8>) -> Self {
        Self { offsets, data, validity: None }
    }

    pub fn new_nullable(offsets: Vec<i32>, data: Vec<u8>, validity: Bitmap) -> Self {
        Self { offsets, data, validity: Some(validity) }
    }

    pub fn len(&self) -> usize {
        if self.offsets.is_empty() { 0 } else { self.offsets.len() - 1 }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn null_count(&self) -> usize {
        self.validity.as_ref().map_or(0, |b| b.null_count())
    }

    pub fn value(&self, index: usize) -> &[u8] {
        let start = self.offsets[index] as usize;
        let end = self.offsets[index + 1] as usize;
        &self.data[start..end]
    }
}

/// Fixed-size binary column. Each row is exactly `width` bytes.
///
/// Arrow layout: contiguous buffer of `width * num_rows` bytes.
#[derive(Debug, Clone)]
pub struct FixedBinaryColumn {
    pub data: Vec<u8>,
    pub width: usize,
    pub validity: Option<Bitmap>,
}

impl FixedBinaryColumn {
    pub fn new(data: Vec<u8>, width: usize) -> Self {
        Self { data, width, validity: None }
    }

    pub fn new_nullable(data: Vec<u8>, width: usize, validity: Bitmap) -> Self {
        Self { data, width, validity: Some(validity) }
    }

    pub fn len(&self) -> usize {
        if self.width == 0 { 0 } else { self.data.len() / self.width }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn value(&self, index: usize) -> &[u8] {
        let start = index * self.width;
        &self.data[start..start + self.width]
    }

    pub fn null_count(&self) -> usize {
        self.validity.as_ref().map_or(0, |b| b.null_count())
    }
}

/// Enum over all supported column types.
#[derive(Debug, Clone)]
pub enum Column {
    Bool(BoolColumn),
    Int8(PrimitiveColumn<i8>),
    Int16(PrimitiveColumn<i16>),
    Int32(PrimitiveColumn<i32>),
    Int64(PrimitiveColumn<i64>),
    UInt8(PrimitiveColumn<u8>),
    UInt16(PrimitiveColumn<u16>),
    UInt32(PrimitiveColumn<u32>),
    UInt64(PrimitiveColumn<u64>),
    Float32(PrimitiveColumn<f32>),
    Float64(PrimitiveColumn<f64>),
    Utf8(Utf8Column),
    FixedBinary(FixedBinaryColumn),
}

impl Column {
    pub fn len(&self) -> usize {
        match self {
            Column::Bool(c) => c.len(),
            Column::Int8(c) => c.len(),
            Column::Int16(c) => c.len(),
            Column::Int32(c) => c.len(),
            Column::Int64(c) => c.len(),
            Column::UInt8(c) => c.len(),
            Column::UInt16(c) => c.len(),
            Column::UInt32(c) => c.len(),
            Column::UInt64(c) => c.len(),
            Column::Float32(c) => c.len(),
            Column::Float64(c) => c.len(),
            Column::Utf8(c) => c.len(),
            Column::FixedBinary(c) => c.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn null_count(&self) -> usize {
        match self {
            Column::Bool(c) => c.null_count(),
            Column::Int8(c) => c.null_count(),
            Column::Int16(c) => c.null_count(),
            Column::Int32(c) => c.null_count(),
            Column::Int64(c) => c.null_count(),
            Column::UInt8(c) => c.null_count(),
            Column::UInt16(c) => c.null_count(),
            Column::UInt32(c) => c.null_count(),
            Column::UInt64(c) => c.null_count(),
            Column::Float32(c) => c.null_count(),
            Column::Float64(c) => c.null_count(),
            Column::Utf8(c) => c.null_count(),
            Column::FixedBinary(c) => c.null_count(),
        }
    }

    pub fn validity(&self) -> Option<&Bitmap> {
        match self {
            Column::Bool(c) => c.validity.as_ref(),
            Column::Int8(c) => c.validity.as_ref(),
            Column::Int16(c) => c.validity.as_ref(),
            Column::Int32(c) => c.validity.as_ref(),
            Column::Int64(c) => c.validity.as_ref(),
            Column::UInt8(c) => c.validity.as_ref(),
            Column::UInt16(c) => c.validity.as_ref(),
            Column::UInt32(c) => c.validity.as_ref(),
            Column::UInt64(c) => c.validity.as_ref(),
            Column::Float32(c) => c.validity.as_ref(),
            Column::Float64(c) => c.validity.as_ref(),
            Column::Utf8(c) => c.validity.as_ref(),
            Column::FixedBinary(c) => c.validity.as_ref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitmap::Bitmap;

    #[test]
    fn test_primitive_column() {
        let col = PrimitiveColumn::new(vec![1i64, 2, 3]);
        assert_eq!(col.len(), 3);
        assert_eq!(col.null_count(), 0);
    }

    #[test]
    fn test_nullable_primitive() {
        let validity = Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]);
        let col = PrimitiveColumn::new_nullable(vec![1i64, 0, 3], validity);
        assert_eq!(col.len(), 3);
        assert_eq!(col.null_count(), 1);
    }

    #[test]
    fn test_bool_column() {
        // Wire: [0x01, 0x00, 0x01, 0x00, 0x01]
        let col = BoolColumn::from_wire_bytes(&[1, 0, 1, 0, 1]);
        assert_eq!(col.len(), 5);
        assert!(col.get(0));
        assert!(!col.get(1));
        assert!(col.get(2));
        assert!(!col.get(3));
        assert!(col.get(4));
        // Packed: 0b10101 = 0x15
        assert_eq!(col.bitmap[0], 0x15);
    }

    #[test]
    fn test_utf8_column() {
        let data = b"helloworld".to_vec();
        let offsets = vec![0i32, 5, 10];
        let col = Utf8Column::new(offsets, data);
        assert_eq!(col.len(), 2);
        assert_eq!(col.value(0), b"hello");
        assert_eq!(col.value(1), b"world");
    }

    #[test]
    fn test_fixed_binary_column() {
        let data = vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let col = FixedBinaryColumn::new(data, 3);
        assert_eq!(col.len(), 2);
        assert_eq!(col.value(0), &[0x01, 0x02, 0x03]);
        assert_eq!(col.value(1), &[0x04, 0x05, 0x06]);
    }

    #[test]
    fn test_column_enum() {
        let col = Column::Int32(PrimitiveColumn::new(vec![10, 20]));
        assert_eq!(col.len(), 2);
        assert_eq!(col.null_count(), 0);
    }

    #[test]
    fn test_all_primitive_widths() {
        assert_eq!(Column::Int8(PrimitiveColumn::new(vec![1i8])).len(), 1);
        assert_eq!(Column::Int16(PrimitiveColumn::new(vec![1i16])).len(), 1);
        assert_eq!(Column::UInt8(PrimitiveColumn::new(vec![1u8])).len(), 1);
        assert_eq!(Column::UInt16(PrimitiveColumn::new(vec![1u16])).len(), 1);
        assert_eq!(Column::UInt32(PrimitiveColumn::new(vec![1u32])).len(), 1);
        assert_eq!(Column::UInt64(PrimitiveColumn::new(vec![1u64])).len(), 1);
        assert_eq!(Column::Float32(PrimitiveColumn::new(vec![1.0f32])).len(), 1);
    }
}
