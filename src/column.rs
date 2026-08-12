use crate::bitmap::Bitmap;
use crate::schema::ChType;

/// A ClickHouse `Nothing` column.
///
/// Nothing has no value buffer. `len` carries the row count, while `validity`
/// retains the structural null map of `Nullable(Nothing)` for Native
/// decode-to-encode fidelity. Arrow exports both forms as its Null type and
/// therefore ignores this bitmap.
#[derive(Debug, Clone, PartialEq)]
pub struct NothingColumn {
    pub len: usize,
    pub validity: Option<Bitmap>,
}

impl NothingColumn {
    pub fn new(len: usize) -> Self {
        Self {
            len,
            validity: None,
        }
    }

    pub fn new_nullable(len: usize, validity: Bitmap) -> Self {
        Self {
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

    /// Structural nulls per the retained ClickHouse null map, like every
    /// other column. The Arrow rule that a Null array reports every row as
    /// null lives at the FFI export site.
    pub fn null_count(&self) -> usize {
        self.validity.as_ref().map_or(0, |b| b.null_count())
    }
}

/// A fixed-width column of primitive values.
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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

/// Serialized ClickHouse `AggregateFunction(...)` states in Arrow LargeBinary
/// layout.
///
/// Row `i` is the exact Native state byte slice
/// `data[offsets[i]..offsets[i + 1]]`. Offsets are i64 because aggregate states
/// have no generic size bound and Arrow LargeBinary (`Z`) is the honest
/// zero-copy representation. The logical aggregate function, arguments, and
/// state version remain in [`crate::schema::ChType`].
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateStateColumn {
    pub offsets: Vec<i64>,
    pub data: Vec<u8>,
}

impl AggregateStateColumn {
    pub fn new(offsets: Vec<i64>, data: Vec<u8>) -> Self {
        Self { offsets, data }
    }

    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn null_count(&self) -> usize {
        0
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
#[derive(Debug, Clone, PartialEq)]
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

/// A ClickHouse `QBit(T, N)` column in Arrow FixedSizeList layout.
///
/// `values` is the row-major flattened child column with exactly `N` scalar
/// values per logical row. It is one of `Column::BFloat16`, `Column::Float32`,
/// or `Column::Float64` and never carries child validity. A nullable QBit wraps
/// whole vectors, so its validity bitmap lives here at the list level. Native's
/// bit-transposed plane representation is converted once during decode; Arrow
/// export then borrows these buffers without another transpose or copy.
#[derive(Debug, Clone, PartialEq)]
pub struct QBitColumn {
    pub values: Box<Column>,
    pub dimension: usize,
    pub validity: Option<Bitmap>,
}

impl QBitColumn {
    pub fn new(values: Column, dimension: usize) -> Self {
        Self {
            values: Box::new(values),
            dimension,
            validity: None,
        }
    }

    pub fn new_nullable(values: Column, dimension: usize, validity: Bitmap) -> Self {
        Self {
            values: Box::new(values),
            dimension,
            validity: Some(validity),
        }
    }

    pub fn len(&self) -> usize {
        self.values.len().checked_div(self.dimension).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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

/// Maximum number of child type codes in one Arrow union node.
///
/// Arrow union type codes are signed Int8 values restricted to `0..=127`.
/// ClickHouse Variant supports 255 alternatives plus its implicit NULL, so a
/// Variant with 128 or more alternatives uses a two-level dense-union tree.
pub const ARROW_UNION_MAX_CHILDREN: usize = 128;

/// One inner Arrow dense-union node for a Variant with 128 or more alternatives.
///
/// `first_variant` is the global ClickHouse discriminator represented by local
/// type id 0. `type_ids` and `offsets` contain only the rows routed to this
/// group by the outer union node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantGroup {
    pub first_variant: usize,
    pub type_ids: Vec<i8>,
    pub offsets: Vec<i32>,
}

/// Arrow Dense Union routing buffers for a ClickHouse Variant column.
///
/// Up to 127 ClickHouse alternatives fit in one union node together with the
/// implicit NULL child. At 128 through 255 alternatives, `Nested` uses an outer
/// union whose children are groups of at most 128 alternatives plus the NULL
/// child. This is the Arrow-prescribed union-of-unions representation for more
/// than 128 possible types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariantLayout {
    Flat {
        type_ids: Vec<i8>,
        offsets: Vec<i32>,
    },
    Nested {
        type_ids: Vec<i8>,
        offsets: Vec<i32>,
        groups: Vec<VariantGroup>,
    },
}

impl VariantLayout {
    pub fn len(&self) -> usize {
        match self {
            VariantLayout::Flat { type_ids, .. } | VariantLayout::Nested { type_ids, .. } => {
                type_ids.len()
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Invalid input to [`VariantColumn::try_new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariantColumnError {
    InvalidAlternativeCount {
        count: usize,
    },
    InvalidDiscriminator {
        row: usize,
        discriminator: u8,
        alternatives: usize,
    },
    ChildOffsetOverflow {
        row: usize,
    },
    ChildLength {
        alternative: usize,
        expected: usize,
        actual: usize,
    },
}

impl std::fmt::Display for VariantColumnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VariantColumnError::InvalidAlternativeCount { count } => write!(
                f,
                "Variant must have between 1 and 255 alternatives, got {count}"
            ),
            VariantColumnError::InvalidDiscriminator {
                row,
                discriminator,
                alternatives,
            } => write!(
                f,
                "Variant row {row} has discriminator {discriminator}, but only {alternatives} alternatives exist"
            ),
            VariantColumnError::ChildOffsetOverflow { row } => write!(
                f,
                "Variant row {row} exceeds Arrow Dense Union's i32 child-offset range"
            ),
            VariantColumnError::ChildLength {
                alternative,
                expected,
                actual,
            } => write!(
                f,
                "Variant alternative {alternative} has {actual} values, expected {expected} from the discriminators"
            ),
        }
    }
}

impl std::error::Error for VariantColumnError {}

/// A ClickHouse `Variant(T1, ...)` column in Arrow Dense Union layout.
///
/// ClickHouse writes one global UInt8 discriminator per row, then one compact
/// child column per alternative. The decoder derives Arrow's signed Int8 type
/// ids and i32 dense offsets while counting those discriminators. NULL uses the
/// server's reserved discriminator 255 and is represented by an Arrow Null
/// child (`nulls`), because Arrow unions have no top-level validity bitmap.
///
/// `discriminators` is the exact Native wire run (one byte per logical row,
/// 255 = NULL) and the single source of truth for routing; `layout` is the
/// deterministic Arrow view derived from it. `variants` stays in the canonical
/// ClickHouse alternative order stored by `ChType::Variant`. Each child
/// contains only its selected rows. `layout` is flat for at most 127
/// alternatives and a two-level union for 128 through 255, preserving both
/// ClickHouse's full range and Arrow's 128-code-per-node limit.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantColumn {
    pub discriminators: Vec<u8>,
    pub layout: VariantLayout,
    pub variants: Vec<Column>,
    pub nulls: NothingColumn,
}

/// One physical child of a self-describing ClickHouse `Dynamic` column.
///
/// Typed children are decoded in bulk into the same [`Column`] buffers their
/// standalone type uses. `SharedVariant` is the server's overflow child: every
/// cell is an opaque binary blob containing a binary type descriptor followed
/// by one value's `serializeBinary` payload. It deliberately stays binary here;
/// parsing or materializing those row payloads is not part of the hot bulk path.
#[derive(Debug, Clone, PartialEq)]
pub enum DynamicChild {
    Typed { ch_type: ChType, values: Column },
    Shared(Utf8Column),
}

impl DynamicChild {
    pub fn len(&self) -> usize {
        match self {
            DynamicChild::Typed { values, .. } => values.len(),
            DynamicChild::Shared(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn ch_type(&self) -> Option<&ChType> {
        match self {
            DynamicChild::Typed { ch_type, .. } => Some(ch_type),
            DynamicChild::Shared(_) => None,
        }
    }
}

/// Invalid input to [`DynamicColumn::try_new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynamicColumnError {
    DuplicateChild {
        name: String,
    },
    ChildOffsetOverflow {
        row: usize,
    },
    InvalidTypeId {
        row: usize,
        type_id: u32,
        children: usize,
    },
    ChildLength {
        child: usize,
        expected: usize,
        actual: usize,
    },
}

impl std::fmt::Display for DynamicColumnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DynamicColumnError::DuplicateChild { name } => {
                write!(f, "Dynamic has more than one child named {name}")
            }
            DynamicColumnError::ChildOffsetOverflow { row } => write!(
                f,
                "Dynamic row {row} exceeds Arrow Dense Union's i32 child-offset range"
            ),
            DynamicColumnError::InvalidTypeId {
                row,
                type_id,
                children,
            } => write!(
                f,
                "Dynamic row {row} has type id {type_id}, but only {children} children exist"
            ),
            DynamicColumnError::ChildLength {
                child,
                expected,
                actual,
            } => write!(
                f,
                "Dynamic child {child} has {actual} values, expected {expected} from the type ids"
            ),
        }
    }
}

impl std::error::Error for DynamicColumnError {}

/// A block-local ClickHouse `Dynamic` column.
///
/// `type_ids` uses one `u32` per row. Values `0..children.len()` select a dense
/// child and `u32::MAX` is intrinsic NULL. `offsets` is the zero-based occurrence
/// index inside the selected child, already in Arrow's i32 Dense Union width.
/// The server's wire ids are local to each block, so this structure intentionally
/// does not pretend the child set is part of the logical [`ChType::Dynamic`]
/// schema.
///
/// V1/V2 columns include exactly one [`DynamicChild::Shared`] in the server's
/// canonical global discriminator order. FLATTENED word 3 has typed children
/// only, in its transmitted list order. This distinction is enough for the
/// encoder to preserve the accepted Native representation without storing a
/// wire-version flag on the public buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct DynamicColumn {
    pub type_ids: Vec<u32>,
    pub offsets: Vec<i32>,
    pub children: Vec<DynamicChild>,
    pub nulls: NothingColumn,
}

impl DynamicColumn {
    /// Build a Dynamic column from block-local child ids and dense children.
    /// `u32::MAX` denotes NULL; every other id must index `children`.
    pub fn try_new(
        type_ids: &[u32],
        children: Vec<DynamicChild>,
    ) -> Result<Self, DynamicColumnError> {
        let mut child_names = std::collections::BTreeSet::new();
        for child in &children {
            let name = match child {
                DynamicChild::Typed { ch_type, .. } => ch_type.to_string(),
                DynamicChild::Shared(_) => "SharedVariant".to_string(),
            };
            if !child_names.insert(name.clone()) {
                return Err(DynamicColumnError::DuplicateChild { name });
            }
        }
        let (offsets, counts, null_count) =
            dynamic_offsets_from_type_ids(type_ids, children.len())?;
        for (child, (values, expected)) in children.iter().zip(counts).enumerate() {
            let actual = values.len();
            if actual != expected {
                return Err(DynamicColumnError::ChildLength {
                    child,
                    expected,
                    actual,
                });
            }
        }
        Ok(Self::from_parts(
            type_ids.to_vec(),
            offsets,
            children,
            null_count,
        ))
    }

    pub(crate) fn from_parts(
        type_ids: Vec<u32>,
        offsets: Vec<i32>,
        children: Vec<DynamicChild>,
        null_count: usize,
    ) -> Self {
        Self {
            type_ids,
            offsets,
            children,
            nulls: NothingColumn::new(null_count),
        }
    }

    pub fn len(&self) -> usize {
        self.type_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.type_ids.is_empty()
    }

    pub fn null_count(&self) -> usize {
        self.nulls.len
    }

    pub fn shared_child_index(&self) -> Option<usize> {
        self.children
            .iter()
            .position(|child| matches!(child, DynamicChild::Shared(_)))
    }

    /// Resolve one row to its local child and dense offset. NULL is returned as
    /// `(u32::MAX, offset_in_null_child)`.
    pub fn value_position(&self, row: usize) -> Option<(u32, i32)> {
        let type_id = *self.type_ids.get(row)?;
        let offset = *self.offsets.get(row)?;
        if offset < 0 {
            return None;
        }
        if type_id == u32::MAX {
            return ((offset as usize) < self.nulls.len).then_some((type_id, offset));
        }
        let child = self.children.get(type_id as usize)?;
        ((offset as usize) < child.len()).then_some((type_id, offset))
    }
}

/// Derive dense child offsets and counts from block-local Dynamic type ids.
pub(crate) fn dynamic_offsets_from_type_ids(
    type_ids: &[u32],
    num_children: usize,
) -> Result<(Vec<i32>, Vec<usize>, usize), DynamicColumnError> {
    if type_ids.len() > i32::MAX as usize {
        return Err(DynamicColumnError::ChildOffsetOverflow {
            row: i32::MAX as usize,
        });
    }

    let mut counts = vec![0usize; num_children];
    let mut null_count = 0usize;
    let mut offsets = Vec::with_capacity(type_ids.len());
    for (row, &type_id) in type_ids.iter().enumerate() {
        if type_id == u32::MAX {
            offsets.push(null_count as i32);
            null_count += 1;
            continue;
        }
        let child = type_id as usize;
        if child >= num_children {
            return Err(DynamicColumnError::InvalidTypeId {
                row,
                type_id,
                children: num_children,
            });
        }
        offsets.push(counts[child] as i32);
        counts[child] += 1;
    }
    Ok((offsets, counts, null_count))
}

impl VariantColumn {
    /// Build a Variant column from ClickHouse global discriminator bytes and
    /// already-dense alternative columns.
    ///
    /// `255` denotes NULL; every other byte must index `variants`. Child lengths
    /// must equal their discriminator counts. The resulting routing buffers are
    /// immediately suitable for Arrow Dense Union export.
    pub fn try_new(
        discriminators: &[u8],
        variants: Vec<Column>,
    ) -> Result<Self, VariantColumnError> {
        let (layout, counts, null_count) =
            variant_layout_from_discriminators(discriminators, variants.len())?;
        for (alternative, (column, expected)) in variants.iter().zip(counts).enumerate() {
            let actual = column.len();
            if actual != expected {
                return Err(VariantColumnError::ChildLength {
                    alternative,
                    expected,
                    actual,
                });
            }
        }
        Ok(Self::from_parts(
            discriminators.to_vec(),
            layout,
            variants,
            null_count,
        ))
    }

    pub(crate) fn from_parts(
        discriminators: Vec<u8>,
        layout: VariantLayout,
        variants: Vec<Column>,
        null_count: usize,
    ) -> Self {
        Self {
            discriminators,
            layout,
            variants,
            nulls: NothingColumn::new(null_count),
        }
    }

    /// The Native wire discriminator run: one byte per logical row, 255 = NULL.
    pub fn discriminators(&self) -> &[u8] {
        &self.discriminators
    }

    pub fn len(&self) -> usize {
        self.layout.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layout.is_empty()
    }

    /// Number of rows carrying Variant's intrinsic NULL discriminator.
    pub fn null_count(&self) -> usize {
        self.nulls.len
    }

    /// Resolve one row to its global ClickHouse discriminator and dense child
    /// offset. NULL is returned as discriminator 255.
    ///
    /// Returns `None` for a malformed hand-built layout. Every returned offset
    /// is bounds-checked in O(1): it must be non-negative and index a real row
    /// of the child it routes to (the selected alternative column, or the NULL
    /// child for discriminator 255). Decoded columns always satisfy this shape;
    /// encode validation rejects a malformed layout before the writer calls
    /// this method.
    pub fn value_position(&self, row: usize) -> Option<(u8, i32)> {
        match &self.layout {
            VariantLayout::Flat { type_ids, offsets } => {
                let type_id = usize::try_from(*type_ids.get(row)?).ok()?;
                let offset = *offsets.get(row)?;
                // A dense offset is an occurrence ordinal, so it must be
                // non-negative and inside the child it selects.
                if offset < 0 {
                    return None;
                }
                if type_id < self.variants.len() {
                    ((offset as usize) < self.variants[type_id].len())
                        .then_some((type_id as u8, offset))
                } else if type_id == self.variants.len() {
                    ((offset as usize) < self.nulls.len).then_some((u8::MAX, offset))
                } else {
                    None
                }
            }
            VariantLayout::Nested {
                type_ids,
                offsets,
                groups,
            } => {
                let outer_id = usize::try_from(*type_ids.get(row)?).ok()?;
                let outer_offset = usize::try_from(*offsets.get(row)?).ok()?;
                if outer_id == groups.len() {
                    // Outer NULL: the outer offset indexes the NULL child.
                    if outer_offset >= self.nulls.len {
                        return None;
                    }
                    return Some((u8::MAX, i32::try_from(outer_offset).ok()?));
                }
                let group = groups.get(outer_id)?;
                let local_id = usize::try_from(*group.type_ids.get(outer_offset)?).ok()?;
                let discriminator = group.first_variant.checked_add(local_id)?;
                if discriminator >= self.variants.len() {
                    return None;
                }
                // The group offset indexes the selected dense child; it must be
                // non-negative and inside that child.
                let child_offset = *group.offsets.get(outer_offset)?;
                if child_offset < 0 || (child_offset as usize) >= self.variants[discriminator].len()
                {
                    return None;
                }
                Some((u8::try_from(discriminator).ok()?, child_offset))
            }
        }
    }
}

/// Count each alternative's dense rows and the NULL rows in one pass over
/// ClickHouse's one-byte global discriminator run (255 = NULL).
///
/// Shared by the layout builder and the streaming skip scan so both paths
/// reject an out-of-range discriminator with the same error.
pub(crate) fn variant_child_counts(
    discriminators: &[u8],
    num_variants: usize,
) -> Result<(Vec<usize>, usize), VariantColumnError> {
    if !(1..=u8::MAX as usize).contains(&num_variants) {
        return Err(VariantColumnError::InvalidAlternativeCount {
            count: num_variants,
        });
    }

    let mut counts = vec![0usize; num_variants];
    let mut null_count = 0usize;
    for (row, &discriminator) in discriminators.iter().enumerate() {
        if discriminator == u8::MAX {
            null_count += 1;
        } else if let Some(count) = counts.get_mut(discriminator as usize) {
            *count += 1;
        } else {
            return Err(VariantColumnError::InvalidDiscriminator {
                row,
                discriminator,
                alternatives: num_variants,
            });
        }
    }
    Ok((counts, null_count))
}

/// Derive Arrow Dense Union routing buffers and child counts from ClickHouse's
/// one-byte global discriminator stream.
pub(crate) fn variant_layout_from_discriminators(
    discriminators: &[u8],
    num_variants: usize,
) -> Result<(VariantLayout, Vec<usize>, usize), VariantColumnError> {
    if !(1..=u8::MAX as usize).contains(&num_variants) {
        return Err(VariantColumnError::InvalidAlternativeCount {
            count: num_variants,
        });
    }

    // Every dense offset (per child and the NULL child) is a monotonic counter
    // that each row advances by exactly one, so no counter ever exceeds the row
    // count. Guarding the row count against `i32::MAX` once here establishes the
    // invariant that every `as i32` in the loops below is non-negative and
    // lossless, so those hot per-row conversions need no fallible check.
    if discriminators.len() > i32::MAX as usize {
        return Err(VariantColumnError::ChildOffsetOverflow {
            row: i32::MAX as usize,
        });
    }

    let mut counts = vec![0usize; num_variants];
    let mut null_count = 0usize;

    if num_variants < ARROW_UNION_MAX_CHILDREN {
        let mut type_ids = Vec::with_capacity(discriminators.len());
        let mut offsets = Vec::with_capacity(discriminators.len());
        for (row, &discriminator) in discriminators.iter().enumerate() {
            if discriminator == u8::MAX {
                type_ids.push(num_variants as i8);
                // `null_count <= discriminators.len() <= i32::MAX` (guarded
                // above), so this `as i32` is non-negative and lossless.
                offsets.push(null_count as i32);
                null_count += 1;
                continue;
            }

            let alternative = discriminator as usize;
            if alternative >= num_variants {
                return Err(VariantColumnError::InvalidDiscriminator {
                    row,
                    discriminator,
                    alternatives: num_variants,
                });
            }
            type_ids.push(discriminator as i8);
            // `counts[alternative] <= row < i32::MAX` (guarded above).
            offsets.push(counts[alternative] as i32);
            counts[alternative] += 1;
        }
        return Ok((
            VariantLayout::Flat { type_ids, offsets },
            counts,
            null_count,
        ));
    }

    let num_groups = num_variants.div_ceil(ARROW_UNION_MAX_CHILDREN);
    let mut groups = (0..num_groups)
        .map(|group| VariantGroup {
            first_variant: group * ARROW_UNION_MAX_CHILDREN,
            type_ids: Vec::new(),
            offsets: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut group_counts = vec![0usize; num_groups];
    let mut type_ids = Vec::with_capacity(discriminators.len());
    let mut offsets = Vec::with_capacity(discriminators.len());

    for (row, &discriminator) in discriminators.iter().enumerate() {
        if discriminator == u8::MAX {
            type_ids.push(num_groups as i8);
            // Guarded above: every counter stays within `i32::MAX`, so the
            // `as i32` casts in this loop are non-negative and lossless.
            offsets.push(null_count as i32);
            null_count += 1;
            continue;
        }

        let alternative = discriminator as usize;
        if alternative >= num_variants {
            return Err(VariantColumnError::InvalidDiscriminator {
                row,
                discriminator,
                alternatives: num_variants,
            });
        }
        let group_index = alternative / ARROW_UNION_MAX_CHILDREN;
        let local_id = alternative % ARROW_UNION_MAX_CHILDREN;
        type_ids.push(group_index as i8);
        offsets.push(group_counts[group_index] as i32);
        group_counts[group_index] += 1;

        let group = &mut groups[group_index];
        group.type_ids.push(local_id as i8);
        group.offsets.push(counts[alternative] as i32);
        counts[alternative] += 1;
    }

    Ok((
        VariantLayout::Nested {
            type_ids,
            offsets,
            groups,
        },
        counts,
        null_count,
    ))
}

/// Invalid input to [`StructuredJson::try_new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonColumnError {
    /// A typed-path child column does not carry `len` rows.
    TypedPathLength {
        path: String,
        expected: usize,
        actual: usize,
    },
    /// A dynamic-path child column does not carry `len` rows.
    DynamicPathLength {
        path: String,
        expected: usize,
        actual: usize,
    },
    /// Dynamic path names are not strictly increasing (sorted and unique).
    UnsortedDynamicPath { path: String },
    /// The shared `paths` and `values` string columns disagree on pair count.
    SharedPairMismatch { paths: usize, values: usize },
    /// The shared `paths` or `values` string column carries nulls; shared data
    /// has no wire null map.
    SharedNulls { which: &'static str },
    /// The shared-data offsets are not a valid Arrow list-offset run.
    SharedOffsets { reason: &'static str },
}

impl std::fmt::Display for JsonColumnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JsonColumnError::TypedPathLength {
                path,
                expected,
                actual,
            } => write!(
                f,
                "JSON typed path {path} has {actual} rows, expected {expected}"
            ),
            JsonColumnError::DynamicPathLength {
                path,
                expected,
                actual,
            } => write!(
                f,
                "JSON dynamic path {path} has {actual} rows, expected {expected}"
            ),
            JsonColumnError::UnsortedDynamicPath { path } => write!(
                f,
                "JSON dynamic paths are not strictly sorted and unique at {path}"
            ),
            JsonColumnError::SharedPairMismatch { paths, values } => {
                write!(f, "JSON shared data has {paths} paths but {values} values")
            }
            JsonColumnError::SharedNulls { which } => {
                write!(f, "JSON shared {which} column carries nulls")
            }
            JsonColumnError::SharedOffsets { reason } => {
                write!(f, "JSON shared data offsets are invalid: {reason}")
            }
        }
    }
}

impl std::error::Error for JsonColumnError {}

/// The structured (non-text) body of a `JSON` column: the declared typed paths,
/// the block-local discovered dynamic paths, and the shared-data overflow.
///
/// `typed` holds one child column per declared typed path, in the same sorted
/// order as [`crate::schema::ChType::Json`]'s `typed_paths`, each of length
/// `len`. `dynamic` holds one [`DynamicColumn`] per block-local dynamic path,
/// sorted by path, each of length `len`; the set is column data (like a
/// `Dynamic`'s children), not part of the logical schema.
///
/// Shared data is the server's `SharedData` overflow, physically an
/// `Array(Tuple(String, String))`: `shared_offsets` is the Arrow list-offset run
/// (leading `0`, length `len + 1`) over the flattened `(path, value)` pairs, and
/// `shared_paths`/`shared_values` are the two flattened string columns. The
/// `values` strings are opaque binary-encoded values (a binary type descriptor
/// plus a `serializeBinary` payload, the same shape as a `SharedVariant` cell),
/// kept as raw bytes and never materialized. A `FLATTENED`-wire block carries no
/// shared data, so it decodes with empty shared columns (`shared_offsets` is
/// `[0]`).
#[derive(Debug, Clone, PartialEq)]
pub struct StructuredJson {
    pub typed: Vec<(String, Column)>,
    pub dynamic: Vec<(String, DynamicColumn)>,
    pub shared_offsets: Vec<i64>,
    pub shared_paths: Utf8Column,
    pub shared_values: Utf8Column,
    pub len: usize,
}

impl StructuredJson {
    /// Build a structured JSON body from already-validated parts (the decode
    /// path, which validated the wire framing as it read). Does not re-check the
    /// invariants; use [`StructuredJson::try_new`] for untrusted parts.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        typed: Vec<(String, Column)>,
        dynamic: Vec<(String, DynamicColumn)>,
        shared_offsets: Vec<i64>,
        shared_paths: Utf8Column,
        shared_values: Utf8Column,
        len: usize,
    ) -> Self {
        Self {
            typed,
            dynamic,
            shared_offsets,
            shared_paths,
            shared_values,
            len,
        }
    }

    /// Build a structured JSON body, validating the child lengths, the strictly
    /// sorted dynamic path names, the null-free shared string columns, and the
    /// shared-data offset run.
    pub fn try_new(
        typed: Vec<(String, Column)>,
        dynamic: Vec<(String, DynamicColumn)>,
        shared_offsets: Vec<i64>,
        shared_paths: Utf8Column,
        shared_values: Utf8Column,
        len: usize,
    ) -> Result<Self, JsonColumnError> {
        for (path, column) in &typed {
            if column.len() != len {
                return Err(JsonColumnError::TypedPathLength {
                    path: path.clone(),
                    expected: len,
                    actual: column.len(),
                });
            }
        }
        let mut previous: Option<&str> = None;
        for (path, column) in &dynamic {
            if previous.is_some_and(|prior| prior >= path.as_str()) {
                return Err(JsonColumnError::UnsortedDynamicPath { path: path.clone() });
            }
            previous = Some(path);
            if column.len() != len {
                return Err(JsonColumnError::DynamicPathLength {
                    path: path.clone(),
                    expected: len,
                    actual: column.len(),
                });
            }
        }
        if shared_paths.len() != shared_values.len() {
            return Err(JsonColumnError::SharedPairMismatch {
                paths: shared_paths.len(),
                values: shared_values.len(),
            });
        }
        if shared_paths.null_count() > 0 {
            return Err(JsonColumnError::SharedNulls { which: "paths" });
        }
        if shared_values.null_count() > 0 {
            return Err(JsonColumnError::SharedNulls { which: "values" });
        }
        if shared_offsets.first() != Some(&0) {
            return Err(JsonColumnError::SharedOffsets {
                reason: "offsets do not start at 0",
            });
        }
        // `len` comes from an untrusted-parts caller, so `len + 1` is checked: a
        // `len` of `usize::MAX` cannot have a matching `len + 1`-entry offset
        // vector, so it is rejected here rather than panicking in debug or
        // wrapping in release.
        let expected_offsets_len = len.checked_add(1).ok_or(JsonColumnError::SharedOffsets {
            reason: "row count overflows usize",
        })?;
        if shared_offsets.len() != expected_offsets_len {
            return Err(JsonColumnError::SharedOffsets {
                reason: "offsets length is not len + 1",
            });
        }
        for pair in shared_offsets.windows(2) {
            if pair[1] < pair[0] {
                return Err(JsonColumnError::SharedOffsets {
                    reason: "offsets are not monotonically non-decreasing",
                });
            }
        }
        // `len < shared_offsets.len()` (the length check above passed), so this
        // index is in bounds.
        if shared_offsets[len] != shared_paths.len() as i64 {
            return Err(JsonColumnError::SharedOffsets {
                reason: "final offset does not equal the shared pair count",
            });
        }
        Ok(Self::from_parts(
            typed,
            dynamic,
            shared_offsets,
            shared_paths,
            shared_values,
            len,
        ))
    }
}

/// The two physical shapes a decoded `JSON` column can take.
///
/// `Structured` is the V1/V2/FLATTENED wire form (typed paths, dynamic paths,
/// and shared-data overflow). `Text` is the `STRING`-mode form: one re-serialized
/// JSON document string per row, from the
/// `output_format_native_write_json_as_string` setting.
///
/// [`StructuredJson`] is boxed because it is several times larger than a
/// `Utf8Column`; keeping it behind a pointer stops the size of the whole
/// [`Column`] enum (and every enum that embeds a `Column`) from ballooning.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonBody {
    Structured(Box<StructuredJson>),
    Text(Utf8Column),
}

/// A ClickHouse `JSON` column (`DataTypeObject`, confirmed at
/// v26.6.1.1193-stable).
///
/// `validity` is the top-level null map, populated ONLY under a `Nullable(JSON)`
/// wrapper (the server serializes the null map first, then the full JSON body);
/// a bare `JSON` column always leaves it `None`. This mirrors how
/// [`TupleColumn`] carries `Nullable(Tuple(...))` validity independent of its
/// children.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonColumn {
    pub body: JsonBody,
    pub validity: Option<Bitmap>,
}

impl JsonColumn {
    /// A structured JSON column with no top-level null map.
    pub fn structured(body: StructuredJson) -> Self {
        Self {
            body: JsonBody::Structured(Box::new(body)),
            validity: None,
        }
    }

    /// A `STRING`-mode JSON column (one document string per row).
    pub fn text(values: Utf8Column) -> Self {
        Self {
            body: JsonBody::Text(values),
            validity: None,
        }
    }

    /// Attach a top-level `Nullable(JSON)` validity bitmap.
    pub fn with_validity(mut self, validity: Option<Bitmap>) -> Self {
        self.validity = validity;
        self
    }

    pub fn body(&self) -> &JsonBody {
        &self.body
    }

    /// The declared typed paths and their child columns, or an empty slice for a
    /// `Text`-mode column.
    pub fn typed_paths(&self) -> &[(String, Column)] {
        match &self.body {
            JsonBody::Structured(structured) => &structured.typed,
            JsonBody::Text(_) => &[],
        }
    }

    /// The block-local dynamic paths and their `Dynamic` columns, or an empty
    /// slice for a `Text`-mode column.
    pub fn dynamic_paths(&self) -> &[(String, DynamicColumn)] {
        match &self.body {
            JsonBody::Structured(structured) => &structured.dynamic,
            JsonBody::Text(_) => &[],
        }
    }

    pub fn len(&self) -> usize {
        match &self.body {
            JsonBody::Structured(structured) => structured.len,
            JsonBody::Text(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Top-level nulls, present only under a `Nullable(JSON)` wrapper.
    pub fn null_count(&self) -> usize {
        self.validity.as_ref().map_or(0, |b| b.null_count())
    }
}

/// Enum over all supported column types.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    Nothing(NothingColumn),
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
    // ClickHouse BFloat16 is a raw little-endian 16-bit floating-point word.
    // One `[u8; 2]` per row makes the width invariant structural, keeps the
    // layout host-independent, and exports zero-copy as FixedSizeBinary(2).
    BFloat16(PrimitiveColumn<[u8; 2]>),
    // QBit vectors materialized as one row-major scalar child buffer. The
    // logical element type and dimension live in ChType; the column repeats the
    // dimension so its public buffer shape is self-describing and can be
    // validated before Native encode.
    QBit(QBitColumn),
    // Temporal types decoded at their faithful native width. No widening or
    // rescaling happens here; the type metadata (timezone, precision) lives in
    // the schema's ChType, not in these buffers.
    Date(PrimitiveColumn<u16>),
    Date32(PrimitiveColumn<i32>),
    DateTime(PrimitiveColumn<u32>),
    DateTime64(PrimitiveColumn<i64>),
    Time(PrimitiveColumn<i32>),
    Time64(PrimitiveColumn<i64>),
    // All 11 ClickHouse Interval* types are signed Int64 counts. The unit stays
    // in the schema's ChType::Interval tag, mirroring Time64 precision metadata.
    Interval(PrimitiveColumn<i64>),
    Utf8(Utf8Column),
    AggregateState(AggregateStateColumn),
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
    // Variant(T1, ...): Arrow Dense Union routing buffers plus one compact child
    // column per alternative and an implicit Arrow Null child.
    Variant(VariantColumn),
    // Dynamic: block-local self-describing typed children, optional raw binary
    // SharedVariant overflow child, and dense routing buffers.
    Dynamic(DynamicColumn),
    // JSON: declared typed-path child columns, block-local dynamic-path Dynamic
    // columns, and the shared-data string overflow (or a single re-serialized
    // document string per row in STRING mode). Top-level nulls only under a
    // Nullable(JSON) wrapper.
    Json(JsonColumn),
}

impl Column {
    pub fn len(&self) -> usize {
        match self {
            Column::Nothing(c) => c.len(),
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
            Column::BFloat16(c) => c.len(),
            Column::QBit(c) => c.len(),
            Column::Date(c) => c.len(),
            Column::Date32(c) => c.len(),
            Column::DateTime(c) => c.len(),
            Column::DateTime64(c) => c.len(),
            Column::Time(c) => c.len(),
            Column::Time64(c) => c.len(),
            Column::Interval(c) => c.len(),
            Column::Utf8(c) => c.len(),
            Column::AggregateState(c) => c.len(),
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
            Column::Variant(c) => c.len(),
            Column::Dynamic(c) => c.len(),
            Column::Json(c) => c.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn null_count(&self) -> usize {
        match self {
            Column::Nothing(c) => c.null_count(),
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
            Column::BFloat16(c) => c.null_count(),
            Column::QBit(c) => c.null_count(),
            Column::Date(c) => c.null_count(),
            Column::Date32(c) => c.null_count(),
            Column::DateTime(c) => c.null_count(),
            Column::DateTime64(c) => c.null_count(),
            Column::Time(c) => c.null_count(),
            Column::Time64(c) => c.null_count(),
            Column::Interval(c) => c.null_count(),
            Column::Utf8(c) => c.null_count(),
            Column::AggregateState(c) => c.null_count(),
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
            Column::Variant(c) => c.null_count(),
            Column::Dynamic(c) => c.null_count(),
            Column::Json(c) => c.null_count(),
        }
    }

    pub fn validity(&self) -> Option<&Bitmap> {
        match self {
            Column::Nothing(c) => c.validity.as_ref(),
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
            Column::BFloat16(c) => c.validity.as_ref(),
            Column::QBit(c) => c.validity.as_ref(),
            Column::Date(c) => c.validity.as_ref(),
            Column::Date32(c) => c.validity.as_ref(),
            Column::DateTime(c) => c.validity.as_ref(),
            Column::DateTime64(c) => c.validity.as_ref(),
            Column::Time(c) => c.validity.as_ref(),
            Column::Time64(c) => c.validity.as_ref(),
            Column::Interval(c) => c.validity.as_ref(),
            Column::Utf8(c) => c.validity.as_ref(),
            Column::AggregateState(_) => None,
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
            // Arrow unions have no validity bitmap. Variant's intrinsic NULL is
            // represented by its Arrow Null child.
            Column::Variant(_) => None,
            // Dynamic has the same intrinsic-NULL union semantics as Variant.
            Column::Dynamic(_) => None,
            // JSON carries a top-level validity bitmap only under a
            // Nullable(JSON) wrapper, like Tuple; a bare JSON column has None.
            Column::Json(c) => c.validity.as_ref(),
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
    fn test_structured_json_try_new_len_overflow_is_error_not_panic() {
        // An untrusted-parts caller could pass `len == usize::MAX`; the `len + 1`
        // offset-length check must return an error, not panic in debug or wrap in
        // release.
        let result = StructuredJson::try_new(
            Vec::new(),
            Vec::new(),
            vec![0i64],
            Utf8Column::new(vec![0], Vec::new()),
            Utf8Column::new(vec![0], Vec::new()),
            usize::MAX,
        );
        assert!(matches!(result, Err(JsonColumnError::SharedOffsets { .. })));
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
        assert_eq!(
            Column::BFloat16(PrimitiveColumn::new(vec![[0x80, 0x3f]])).len(),
            1
        );
    }

    #[test]
    fn test_value_position_rejects_malformed_layout() {
        // Flat: an offset that points past the selected child's only row. A
        // hand-built layout can express this; decode never produces it.
        let column = VariantColumn::from_parts(
            vec![0],
            VariantLayout::Flat {
                type_ids: vec![0],
                offsets: vec![1],
            },
            vec![
                Column::UInt8(PrimitiveColumn::new(vec![13u8])),
                Column::UInt8(PrimitiveColumn::new(Vec::new())),
            ],
            0,
        );
        assert_eq!(column.value_position(0), None);

        // Flat: a negative offset is out of range for any child.
        let column = VariantColumn::from_parts(
            vec![0],
            VariantLayout::Flat {
                type_ids: vec![0],
                offsets: vec![-1],
            },
            vec![Column::UInt8(PrimitiveColumn::new(vec![13u8]))],
            0,
        );
        assert_eq!(column.value_position(0), None);

        // Flat: a NULL offset past the NULL child's length.
        let column = VariantColumn::from_parts(
            vec![u8::MAX],
            VariantLayout::Flat {
                type_ids: vec![1],
                offsets: vec![0],
            },
            vec![Column::UInt8(PrimitiveColumn::new(Vec::new()))],
            0,
        );
        assert_eq!(column.value_position(0), None);

        // Nested: a group offset past the selected child's only row.
        let column = VariantColumn::from_parts(
            vec![0],
            VariantLayout::Nested {
                type_ids: vec![0],
                offsets: vec![0],
                groups: vec![VariantGroup {
                    first_variant: 0,
                    type_ids: vec![0],
                    offsets: vec![5],
                }],
            },
            vec![Column::UInt8(PrimitiveColumn::new(vec![13u8]))],
            0,
        );
        assert_eq!(column.value_position(0), None);

        // Nested: an outer NULL offset past the NULL child's length.
        let column = VariantColumn::from_parts(
            vec![u8::MAX],
            VariantLayout::Nested {
                type_ids: vec![1],
                offsets: vec![3],
                groups: vec![VariantGroup {
                    first_variant: 0,
                    type_ids: Vec::new(),
                    offsets: Vec::new(),
                }],
            },
            vec![Column::UInt8(PrimitiveColumn::new(Vec::new()))],
            0,
        );
        assert_eq!(column.value_position(0), None);
    }
}
