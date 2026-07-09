use crate::bitmap::Bitmap;

/// A fixed-width column of primitive values.
#[derive(Debug, Clone)]
pub struct PrimitiveColumn<T: Clone> {
    pub values: Vec<T>,
    pub validity: Option<Bitmap>,
}

impl<T: Clone> PrimitiveColumn<T> {
    pub fn new(values: Vec<T>) -> Self {
        Self {
            values,
            validity: None,
        }
    }

    pub fn new_nullable(values: Vec<T>, validity: Bitmap) -> Self {
        Self {
            values,
            validity: Some(validity),
        }
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
        let mut bitmap = Vec::with_capacity(len.div_ceil(8));

        // Pack 8 wire bytes (one per row, nonzero = true) into one Arrow bitmap
        // byte at a time, LSB-first. Same chunked, branchless pack as the
        // validity bitmap; here the set bit is `b != 0` rather than `b == 0`.
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            let byte = (c[0] != 0) as u8
                | (((c[1] != 0) as u8) << 1)
                | (((c[2] != 0) as u8) << 2)
                | (((c[3] != 0) as u8) << 3)
                | (((c[4] != 0) as u8) << 4)
                | (((c[5] != 0) as u8) << 5)
                | (((c[6] != 0) as u8) << 6)
                | (((c[7] != 0) as u8) << 7);
            bitmap.push(byte);
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            let mut byte = 0u8;
            for (k, &b) in rem.iter().enumerate() {
                byte |= ((b != 0) as u8) << k;
            }
            bitmap.push(byte);
        }

        Self {
            bitmap,
            len,
            validity: None,
        }
    }

    pub fn from_wire_bytes_nullable(bytes: &[u8], validity: Bitmap) -> Self {
        let mut col = Self::from_wire_bytes(bytes);
        col.validity = Some(validity);
        col
    }

    pub fn empty() -> Self {
        Self {
            bitmap: vec![],
            len: 0,
            validity: None,
        }
    }

    pub fn empty_nullable() -> Self {
        Self {
            bitmap: vec![],
            len: 0,
            validity: Some(Bitmap::from_ch_null_map(&[])),
        }
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
        Self {
            offsets,
            data,
            validity: None,
        }
    }

    pub fn new_nullable(offsets: Vec<i32>, data: Vec<u8>, validity: Bitmap) -> Self {
        Self {
            offsets,
            data,
            validity: Some(validity),
        }
    }

    pub fn len(&self) -> usize {
        if self.offsets.is_empty() {
            0
        } else {
            self.offsets.len() - 1
        }
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
        Self {
            data,
            width,
            validity: None,
        }
    }

    pub fn new_nullable(data: Vec<u8>, width: usize, validity: Bitmap) -> Self {
        Self {
            data,
            width,
            validity: Some(validity),
        }
    }

    pub fn len(&self) -> usize {
        self.data.len().checked_div(self.width).unwrap_or(0)
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

/// Decimal column: a contiguous little-endian two's-complement fixed-width
/// integer buffer, the same physical shape as a `FixedSizeBinary` of width
/// `bits / 8`.
///
/// ClickHouse serializes `Decimal(P, S)` as a raw fixed-width signed integer per
/// row (4/8/16/32 bytes by precision), little-endian, with no per-row framing
/// and no in-band precision or scale. Decode is a raw passthrough: the wire
/// bytes go into `data` unchanged, host-agnostic, so the buffer stays correct on
/// big-endian hosts and the core needs no native `i128`/`i256`. The host
/// representation (a Python `Decimal`, a JS `BigInt`, and so on) is a binding
/// concern; the unscaled value is `data` read as a little-endian signed integer
/// of `width` bytes, divided by `10^scale`.
///
/// `precision` and `scale` are the type metadata; `width` is the byte width
/// derived from the precision (`bits / 8`).
#[derive(Debug, Clone)]
pub struct DecimalColumn {
    pub data: Vec<u8>,
    pub width: usize,
    pub precision: u8,
    pub scale: u8,
    pub validity: Option<Bitmap>,
}

impl DecimalColumn {
    pub fn new(data: Vec<u8>, width: usize, precision: u8, scale: u8) -> Self {
        Self {
            data,
            width,
            precision,
            scale,
            validity: None,
        }
    }

    pub fn new_nullable(
        data: Vec<u8>,
        width: usize,
        precision: u8,
        scale: u8,
        validity: Bitmap,
    ) -> Self {
        Self {
            data,
            width,
            precision,
            scale,
            validity: Some(validity),
        }
    }

    pub fn len(&self) -> usize {
        self.data.len().checked_div(self.width).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The raw little-endian two's-complement bytes for row `index`, `width`
    /// bytes wide. The caller interprets them as a signed integer scaled by
    /// `10^scale`.
    pub fn value(&self, index: usize) -> &[u8] {
        let start = index * self.width;
        &self.data[start..start + self.width]
    }

    pub fn null_count(&self) -> usize {
        self.validity.as_ref().map_or(0, |b| b.null_count())
    }
}

/// Dictionary-encoded column (Arrow dictionary layout).
///
/// Used for `LowCardinality(T)`. The column is a pair of an index array and a
/// values (dictionary) array: row `i` resolves to `values[indices[i]]`.
///
/// - `indices` are 32-bit signed, the index type pyarrow accepts for a
///   dictionary array, regardless of the native ClickHouse per-block index
///   width (UInt8..UInt64). Decode widens the native width into i32.
/// - `validity` is the index array's validity bitmap (Arrow convention: bit 1 =
///   valid, bit 0 = null). It is `Some` only for a nullable inner type. Nulls
///   live in the index validity, not as a dictionary entry, matching how Arrow
///   represents a null in a dictionary array.
/// - `values` is the per-block dictionary as its own `Column` (a `Utf8Column`
///   for `LowCardinality(String)`). Each Native block carries its own
///   dictionary, and blocks stay separate chunks, so the values column is local
///   to this chunk.
#[derive(Debug, Clone)]
pub struct DictionaryColumn {
    pub indices: Vec<i32>,
    pub validity: Option<Bitmap>,
    pub values: Box<Column>,
}

impl DictionaryColumn {
    pub fn new(indices: Vec<i32>, values: Column) -> Self {
        Self {
            indices,
            validity: None,
            values: Box::new(values),
        }
    }

    pub fn new_nullable(indices: Vec<i32>, values: Column, validity: Bitmap) -> Self {
        Self {
            indices,
            validity: Some(validity),
            values: Box::new(values),
        }
    }

    pub fn len(&self) -> usize {
        self.indices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    pub fn null_count(&self) -> usize {
        self.validity.as_ref().map_or(0, |b| b.null_count())
    }
}

/// Array column in Arrow list layout (`Array(T)`).
///
/// Row `i`'s elements are `values[offsets[i]..offsets[i + 1]]`. `offsets` has
/// length `num_rows + 1` with `offsets[0] == 0`, exactly Arrow's list offset
/// layout.
///
/// ClickHouse serializes the cumulative end-offset per row as a raw `UInt64`
/// run with no leading zero (server `SerializationArray`); decode prepends the
/// `0` and widens each offset to `i64`, so the column exports as an Arrow
/// LargeList (`+L`, 64-bit offsets). i64 is used rather than i32 because
/// ClickHouse offsets are `UInt64` and count elements, not bytes, so a per-block
/// element count can legitimately exceed `i32::MAX` (unlike the `Utf8Column`
/// byte offsets, which cap a chunk at 2 GiB). No artificial cap is imposed.
///
/// `values` is the flattened element column of length `offsets[num_rows]`, its
/// own [`Column`] (recursively any supported element type, including a nested
/// `Array`, a `Nullable`, or a `LowCardinality`). Each Native block carries its
/// own element data, and blocks stay separate chunks, so `values` is local to
/// this chunk.
///
/// The array itself is never nullable (ClickHouse forbids `Nullable(Array(T))`),
/// so `ArrayColumn` carries no validity bitmap; a nullable *element* type keeps
/// its nulls in `values`' own validity (an `Array(Nullable(T))`).
#[derive(Debug, Clone)]
pub struct ArrayColumn {
    pub offsets: Vec<i64>,
    pub values: Box<Column>,
}

impl ArrayColumn {
    pub fn new(offsets: Vec<i64>, values: Column) -> Self {
        Self {
            offsets,
            values: Box::new(values),
        }
    }

    pub fn len(&self) -> usize {
        // offsets always carries the leading 0, so an empty column is `[0]`.
        self.offsets.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Arrays are never nullable in ClickHouse, so the array level has no nulls.
    /// Element-level nulls (an `Array(Nullable(T))`) are counted on `values`.
    pub fn null_count(&self) -> usize {
        0
    }
}

/// Tuple column in Arrow struct layout (`Tuple(T1, ...)`).
///
/// `fields` holds one child `Column` per tuple element, in declaration order,
/// each of length `len` (the element names live in the schema's
/// `ChType::Tuple`, not here). `len` is stored explicitly rather than derived
/// from the children so the zero-element `Tuple()` (which has no child columns
/// at all, only a placeholder byte per row on the wire) still knows its row
/// count.
///
/// `validity` is the tuple-level validity bitmap, populated only for a
/// `Nullable(Tuple(...))` (legal on the server:
/// `DataTypeTuple::canBeInsideNullable()` is true). Per the Arrow spec a
/// struct's validity is independent of its children; a null tuple row still
/// carries placeholder (default) values in every child column, exactly as the
/// server serializes it.
#[derive(Debug, Clone)]
pub struct TupleColumn {
    pub fields: Vec<Column>,
    pub len: usize,
    pub validity: Option<Bitmap>,
}

impl TupleColumn {
    pub fn new(fields: Vec<Column>, len: usize) -> Self {
        Self {
            fields,
            len,
            validity: None,
        }
    }

    pub fn new_nullable(fields: Vec<Column>, len: usize, validity: Bitmap) -> Self {
        Self {
            fields,
            len,
            validity: Some(validity),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Tuple-level nulls only (`Nullable(Tuple)`); element-level nulls are
    /// counted on the element columns.
    pub fn null_count(&self) -> usize {
        self.validity.as_ref().map_or(0, |b| b.null_count())
    }
}

/// Map column (`Map(K, V)`) in Arrow list-of-struct layout.
///
/// Row `i`'s entries are `entries[offsets[i]..offsets[i + 1]]`. `offsets` has
/// length `num_rows + 1` with the Arrow leading `0`, exactly the
/// [`ArrayColumn`] offset layout: on the wire a Map is the plain
/// `Array(Tuple(keys, values))` serialization, so the offsets are the same
/// cumulative `UInt64` end-offset run, widened to `i64`.
///
/// `entries` is the flattened key/value column of length `offsets[num_rows]`:
/// always a [`Column::Tuple`] with exactly two fields, the keys column then
/// the values column (the "keys"/"values" names live nowhere; they never
/// appear on the wire or in the type string). It is a distinct variant from
/// `Array` so encode and validation stay honest about the Map-specific
/// invariants (key-type legality), even though the physical layout matches.
///
/// A map is never nullable at the map level (`DataTypeMap::canBeInsideNullable()`
/// is false), so there is no map-level validity bitmap; a nullable VALUE type
/// keeps its nulls on the values column inside `entries`, and a nullable key
/// type is illegal.
#[derive(Debug, Clone)]
pub struct MapColumn {
    pub offsets: Vec<i64>,
    pub entries: Box<Column>,
}

impl MapColumn {
    pub fn new(offsets: Vec<i64>, entries: Column) -> Self {
        Self {
            offsets,
            entries: Box::new(entries),
        }
    }

    pub fn len(&self) -> usize {
        // offsets always carries the leading 0, so an empty column is `[0]`.
        self.offsets.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maps are never nullable at the map level; value-level nulls are counted
    /// on the values column inside `entries`.
    pub fn null_count(&self) -> usize {
        0
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
    // Temporal types decoded at their faithful native width. No widening or
    // rescaling happens here; the type metadata (timezone, precision) lives in
    // the schema's ChType, not in these buffers.
    Date(PrimitiveColumn<u16>),
    Date32(PrimitiveColumn<i32>),
    DateTime(PrimitiveColumn<u32>),
    DateTime64(PrimitiveColumn<i64>),
    Utf8(Utf8Column),
    FixedBinary(FixedBinaryColumn),
    // IPv4 is a UInt32 on the wire (the standard IPv4 numeric form), decoded at
    // its faithful native width like the other numerics. IPv6 and UUID are raw
    // 16-byte blobs stored verbatim in a FixedBinaryColumn (width 16); the host
    // value policy (in6_addr / uuid.UUID, any byte reordering) lives in the
    // bindings, never here.
    Ipv4(PrimitiveColumn<u32>),
    Ipv6(FixedBinaryColumn),
    Uuid(FixedBinaryColumn),
    // Enum8/Enum16 carry only the physical signed-int buffer; the name->value
    // map lives in the schema's ChType, the same Column-vs-ChType split the
    // temporals use for timezone/precision. Decoded through the primitive
    // little-endian fast path, identical on the wire to Int8/Int16.
    Enum8(PrimitiveColumn<i8>),
    Enum16(PrimitiveColumn<i16>),
    // Decimal(P, S) is a contiguous little-endian two's-complement fixed-width
    // integer buffer (4/8/16/32 bytes per row by precision), the same physical
    // shape as a FixedSizeBinary. precision/scale are metadata on the column;
    // host materialization is a binding concern.
    Decimal(DecimalColumn),
    // Wide integers (Int128/UInt128/Int256/UInt256): a contiguous little-endian
    // fixed-width integer buffer, 16 bytes/row for the 128-bit pair and 32
    // bytes/row for the 256-bit pair, the same physical passthrough shape as a
    // FixedSizeBinary (and UUID/IPv6), backed by a FixedBinaryColumn carrying the
    // byte width. There are four distinct variants (one per ChType) even though
    // the physical shape is shared, mirroring the Int8/UInt8/.. and UUID/IPv6
    // precedent; signedness lives in the ChType/type-name channel, not in the
    // buffer, so a binding reads the type name to recover the host value. Decode
    // is a host-agnostic verbatim byte copy, so the core needs no native
    // i128/i256 and the buffer stays correct on big-endian hosts.
    Int128(FixedBinaryColumn),
    UInt128(FixedBinaryColumn),
    Int256(FixedBinaryColumn),
    UInt256(FixedBinaryColumn),
    Dictionary(DictionaryColumn),
    // Array(T): Arrow list layout (offsets + a flattened element column). The
    // element column is itself a Column, so this is the first recursive variant.
    Array(ArrayColumn),
    // Tuple(T1, ...): Arrow struct layout (one child column per element plus an
    // explicit row count; tuple-level validity for Nullable(Tuple)).
    Tuple(TupleColumn),
    // Map(K, V): Arrow list-of-struct layout (Array offsets over a two-field
    // Tuple entries column), matching the wire's Array(Tuple(keys, values)).
    Map(MapColumn),
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
            Column::Date(c) => c.len(),
            Column::Date32(c) => c.len(),
            Column::DateTime(c) => c.len(),
            Column::DateTime64(c) => c.len(),
            Column::Utf8(c) => c.len(),
            Column::FixedBinary(c) => c.len(),
            Column::Ipv4(c) => c.len(),
            Column::Ipv6(c) => c.len(),
            Column::Uuid(c) => c.len(),
            Column::Enum8(c) => c.len(),
            Column::Enum16(c) => c.len(),
            Column::Decimal(c) => c.len(),
            Column::Int128(c) | Column::UInt128(c) | Column::Int256(c) | Column::UInt256(c) => {
                c.len()
            }
            Column::Dictionary(c) => c.len(),
            Column::Array(c) => c.len(),
            Column::Tuple(c) => c.len(),
            Column::Map(c) => c.len(),
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
            Column::Date(c) => c.null_count(),
            Column::Date32(c) => c.null_count(),
            Column::DateTime(c) => c.null_count(),
            Column::DateTime64(c) => c.null_count(),
            Column::Utf8(c) => c.null_count(),
            Column::FixedBinary(c) => c.null_count(),
            Column::Ipv4(c) => c.null_count(),
            Column::Ipv6(c) => c.null_count(),
            Column::Uuid(c) => c.null_count(),
            Column::Enum8(c) => c.null_count(),
            Column::Enum16(c) => c.null_count(),
            Column::Decimal(c) => c.null_count(),
            Column::Int128(c) | Column::UInt128(c) | Column::Int256(c) | Column::UInt256(c) => {
                c.null_count()
            }
            Column::Dictionary(c) => c.null_count(),
            Column::Array(c) => c.null_count(),
            Column::Tuple(c) => c.null_count(),
            Column::Map(c) => c.null_count(),
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
            Column::Date(c) => c.validity.as_ref(),
            Column::Date32(c) => c.validity.as_ref(),
            Column::DateTime(c) => c.validity.as_ref(),
            Column::DateTime64(c) => c.validity.as_ref(),
            Column::Utf8(c) => c.validity.as_ref(),
            Column::FixedBinary(c) => c.validity.as_ref(),
            Column::Ipv4(c) => c.validity.as_ref(),
            Column::Ipv6(c) => c.validity.as_ref(),
            Column::Uuid(c) => c.validity.as_ref(),
            Column::Enum8(c) => c.validity.as_ref(),
            Column::Enum16(c) => c.validity.as_ref(),
            Column::Decimal(c) => c.validity.as_ref(),
            Column::Int128(c) | Column::UInt128(c) | Column::Int256(c) | Column::UInt256(c) => {
                c.validity.as_ref()
            }
            Column::Dictionary(c) => c.validity.as_ref(),
            // Arrays are never nullable at the array level, so there is no
            // array validity bitmap; element nulls live on `values`.
            Column::Array(_) => None,
            // Tuple-level validity, populated only for Nullable(Tuple(...)).
            Column::Tuple(c) => c.validity.as_ref(),
            // Maps are never nullable at the map level; value nulls live on the
            // values column inside `entries`.
            Column::Map(_) => None,
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
