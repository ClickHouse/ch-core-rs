//! Single-value ClickHouse binary decoding and encoding for Dynamic
//! `SharedVariant` cells and JSON shared-data values.
//!
//! Each cell is a binary type descriptor (`DataTypesBinaryEncoding`, parsed by
//! [`read_binary_type_prefix`]) followed by exactly one value in the server's
//! `serializeBinary` row format (confirmed at v26.3):
//!
//! - Numerics, temporals, Decimal, BFloat16, IPv4/IPv6, UUID, Enum: raw
//!   little-endian fixed-width bytes, the same per-value bytes as the bulk
//!   body (UUID keeps the bulk swapped-64-bit-halves layout verbatim).
//! - String: VarUInt length + raw bytes. FixedString(N): exactly N bytes.
//!   QBit(T, N): VarUInt N followed by N ordinary little-endian T values.
//! - Array: VarUInt count + each element recursively. Map: VarUInt pair
//!   count + per pair key then value. Tuple: elements back-to-back, no
//!   count (`Tuple()` is zero bytes, unlike its one-byte-per-row bulk form).
//! - Nullable (nested only): one flag byte, 0x00 = value follows, nonzero =
//!   null with no value bytes.
//! - LowCardinality(T): no framing, just T's value; the decoded column is the
//!   plain inner column, not a dictionary.
//! - SimpleAggregateFunction/geo/Geometry/Nested expand through
//!   [`ChType::physical_delegate`], like every bulk dispatcher.
//!
//! - JSON: VarUInt path count, then path strings and row values. Declared
//!   typed paths use their declared serializer. Other paths carry a binary type
//!   descriptor and remain opaque cells in `StructuredJson::shared_values`.
//!
//! `Variant`, `Dynamic`, and `AggregateFunction` values report `Unsupported`.
//! JSON typed paths require supported single-value codecs. Untyped path cells
//! may contain Dynamic or Variant values, which are framed without materializing
//! their contents. Truncated payloads and trailing bytes after the
//! value are `Invalid`.
//!
//! [`write_binary_type_prefix`] and [`encode_binary_value`] are the inverses:
//! they write the descriptor and one value of a decoded column, so a binding
//! can build SharedVariant cells and JSON shared data for `encode_block`.

use std::io;

use crate::bitmap::Bitmap;
use crate::column::{
    ArrayColumn, BoolColumn, Column, DecimalColumn, DynamicChild, FixedBinaryColumn, JsonBody,
    JsonColumn, MapColumn, NothingColumn, PrimitiveColumn, QBitColumn, StructuredJson, TupleColumn,
    Utf8Column,
};
use crate::native::protocol::MAX_TYPE_DEPTH;
use crate::native::type_binary::{
    read_binary_type, write_binary_type, BinaryTypeError, MAX_BINARY_TYPE_COMPLEXITY,
};
use crate::native::varint::{write_varint, ByteReader};
use crate::schema::{ChType, QBitElementType, GEOMETRY_EXPANSION_DEPTH};

/// Cap for element counts whose element type consumes zero payload bytes
/// (`Tuple()`, `Nothing`); every other count is bounded by the bytes present.
const MAX_ZERO_WIDTH_ELEMENTS: usize = 1_000_000;

/// Maximum cumulative null-placeholder bytes and zero-width default work
/// while decoding one single value.
///
/// A Nullable single value carries only its one-byte null flag, but Arrow-shaped
/// output still needs the inner type's placeholder buffers. Keep the cap at 16
/// MiB so one maximum-width `FixedString(0x00ff_ffff)` remains representable,
/// while nested containers cannot amplify a few null flags into unbounded QBit
/// or FixedString allocations.
const MAX_NULL_DEFAULT_BYTES: usize = 1 << 24;

#[cfg(test)]
thread_local! {
    static VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Count one per-value step, so tests can bound work without timing.
#[inline]
fn count_visit() {
    #[cfg(test)]
    VISITS.with(|visits| visits.set(visits.get() + 1));
}

struct NullDefaultBudget {
    remaining: usize,
}

impl NullDefaultBudget {
    fn new() -> Self {
        Self {
            remaining: MAX_NULL_DEFAULT_BYTES,
        }
    }

    fn charge(&mut self, bytes: usize) -> Result<(), BinaryValueError> {
        if bytes > self.remaining {
            return Err(BinaryValueError::Invalid(format!(
                "null placeholder expansion exceeds the {MAX_NULL_DEFAULT_BYTES}-byte per-value limit"
            )));
        }
        self.remaining -= bytes;
        Ok(())
    }
}

/// Errors from single-value binary decoding and encoding.
#[derive(Debug)]
pub enum BinaryValueError {
    /// The value violates the binary value contract: truncated or trailing bytes,
    /// a descriptor mismatch, an out-of-range count, an illegal type shape, or
    /// a resource limit.
    Invalid(String),
    /// The type has no defined or implemented single-value codec.
    Unsupported(String),
}

impl From<io::Error> for BinaryValueError {
    fn from(err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::UnexpectedEof {
            BinaryValueError::Invalid("truncated value payload".into())
        } else {
            BinaryValueError::Invalid(err.to_string())
        }
    }
}

impl std::fmt::Display for BinaryValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinaryValueError::Invalid(reason) => {
                write!(f, "invalid binary value: {reason}")
            }
            BinaryValueError::Unsupported(reason) => {
                write!(f, "unsupported binary value type: {reason}")
            }
        }
    }
}

impl std::error::Error for BinaryValueError {}

/// Parse the leading binary type descriptor of a SharedVariant cell.
///
/// Returns the type and the number of descriptor bytes consumed, so the caller
/// can hand the rest of the cell to [`decode_binary_value`]. An unparseable
/// server type (Set, Function) is `Unsupported`; a malformed
/// descriptor is `Invalid`.
pub fn read_binary_type_prefix(bytes: &[u8]) -> Result<(ChType, usize), BinaryValueError> {
    let mut reader = ByteReader::new(bytes);
    let ch_type = read_binary_type(&mut reader).map_err(|err| match err {
        BinaryTypeError::Unsupported(reason) => BinaryValueError::Unsupported(reason),
        BinaryTypeError::Invalid(reason) => BinaryValueError::Invalid(reason),
        BinaryTypeError::Io(err) => BinaryValueError::from(err),
    })?;
    Ok((ch_type, reader.position()))
}

/// Decode exactly one `serializeBinary` value of `ch_type` from `bytes` into a
/// one-row [`Column`], the same column shape the bulk decoder builds for that
/// type. `bytes` must contain the value and nothing else: truncation and
/// trailing bytes are both `Invalid`. Null placeholders that have no backing
/// value bytes share a cumulative 16 MiB materialization budget for this call.
/// Missing typed paths and zero-width default nodes also consume budget units.
pub fn decode_binary_value(ch_type: &ChType, bytes: &[u8]) -> Result<Column, BinaryValueError> {
    check_type_depth(ch_type)?;
    let mut builder = ValueBuilder::new(ch_type, 0)?;
    let mut reader = ByteReader::new(bytes);
    let mut null_budget = NullDefaultBudget::new();
    builder.append_value(&mut reader, &mut null_budget)?;
    if reader.remaining() != 0 {
        return Err(BinaryValueError::Invalid(format!(
            "{} trailing bytes after the value",
            reader.remaining()
        )));
    }
    builder.finish(None)
}

/// Whether single values of this type consume zero payload bytes (`Nothing`,
/// or a `Tuple` of only such types). Their element counts cannot be bounded by
/// the remaining input.
fn has_zero_width_values(ch_type: &ChType) -> bool {
    needs_element_count_cap(ch_type, false)
}

/// Encoding also caps Nullable wrappers around unbacked values. Nullable(Nothing)
/// nulls have validity bits, and a non-null Nothing fails on its first value.
fn needs_element_count_cap(ch_type: &ChType, encoding: bool) -> bool {
    if let Some(delegate) = ch_type.physical_delegate_ref() {
        return needs_element_count_cap(delegate.as_ref(), encoding);
    }
    match ch_type {
        ChType::Nothing => true,
        ChType::Nullable(inner) if encoding => {
            !matches!(inner.as_ref(), ChType::Nothing) && needs_element_count_cap(inner, encoding)
        }
        ChType::LowCardinality(inner) => needs_element_count_cap(inner, encoding),
        ChType::Tuple(elements) => elements
            .iter()
            .all(|(_, t)| needs_element_count_cap(t, encoding)),
        _ => false,
    }
}

/// Read a container element count and bound it before any element loop runs:
/// non-zero-width elements consume at least one byte each, so the remaining
/// input caps the count; zero-width elements use a fixed cap.
fn read_element_count(
    reader: &mut ByteReader,
    zero_width: bool,
    what: &str,
) -> Result<usize, BinaryValueError> {
    let count = usize::try_from(reader.read_varint()?)
        .map_err(|_| BinaryValueError::Invalid(format!("{what} count overflows usize")))?;
    let cap = if zero_width {
        MAX_ZERO_WIDTH_ELEMENTS
    } else {
        reader.remaining()
    };
    if count > cap {
        return Err(BinaryValueError::Invalid(format!(
            "{what} count {count} exceeds the {cap} decodable from the remaining input"
        )));
    }
    Ok(count)
}

/// Read exactly N little-endian bytes.
fn read_le<const N: usize>(reader: &mut ByteReader) -> Result<[u8; N], BinaryValueError> {
    Ok(reader
        .read_slice(N)?
        .try_into()
        .expect("read_slice returns exactly N bytes"))
}

/// The distinct `Column` variants sharing the `FixedBinaryColumn` buffer.
enum FixedKind {
    FixedString,
    Uuid,
    Ipv6,
    Int128,
    UInt128,
    Int256,
    UInt256,
}

/// Incremental per-value column builder. Values append one at a time because
/// the single-value format interleaves per row (an inner array's count sits
/// between its siblings), unlike the columnar bulk bodies.
enum ValueBuilder {
    Nothing {
        rows: usize,
    },
    Bool(Vec<u8>),
    Int8(Vec<i8>),
    Int16(Vec<i16>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    UInt8(Vec<u8>),
    UInt16(Vec<u16>),
    UInt32(Vec<u32>),
    UInt64(Vec<u64>),
    Float32(Vec<f32>),
    Float64(Vec<f64>),
    BFloat16(Vec<[u8; 2]>),
    QBit {
        values: Box<ValueBuilder>,
        dimension: usize,
    },
    Date(Vec<u16>),
    Date32(Vec<i32>),
    DateTime(Vec<u32>),
    DateTime64(Vec<i64>),
    Time(Vec<i32>),
    Time64(Vec<i64>),
    Interval(Vec<i64>),
    Enum8(Vec<i8>),
    Enum16(Vec<i16>),
    Ipv4(Vec<u32>),
    Utf8 {
        offsets: Vec<i32>,
        data: Vec<u8>,
    },
    Fixed {
        data: Vec<u8>,
        width: usize,
        kind: FixedKind,
    },
    Decimal {
        data: Vec<u8>,
        width: usize,
        precision: u8,
        scale: u8,
    },
    Nullable {
        /// ClickHouse null-map bytes: 1 = null.
        flags: Vec<u8>,
        inner: Box<ValueBuilder>,
    },
    Array {
        offsets: Vec<i64>,
        elements: Box<ValueBuilder>,
        zero_width: bool,
        /// Elements are zero-width tuples, appended without a per-element loop.
        fill: bool,
    },
    Map {
        offsets: Vec<i64>,
        keys: Box<ValueBuilder>,
        values: Box<ValueBuilder>,
        zero_width: bool,
        /// Keys and values are zero-width tuples, appended without a loop.
        fill: bool,
    },
    Tuple {
        fields: Vec<ValueBuilder>,
        rows: usize,
    },
    Json(Box<JsonValueBuilder>),
}

struct JsonValueBuilder {
    typed: Vec<(String, ValueBuilder)>,
    offsets: Vec<i64>,
    paths: Utf8Column,
    values: Utf8Column,
    depth: usize,
    seen: Vec<bool>,
    path_order: Vec<(std::ops::Range<usize>, std::ops::Range<usize>)>,
}

impl ValueBuilder {
    fn new(ch_type: &ChType, depth: usize) -> Result<Self, BinaryValueError> {
        if let Some(delegate) = ch_type.physical_delegate_ref() {
            return Self::new(
                delegate.as_ref(),
                depth + usize::from(matches!(ch_type, ChType::SimpleAggregateFunction { .. })),
            );
        }
        Ok(match ch_type {
            ChType::Nothing => Self::Nothing { rows: 0 },
            ChType::Bool => Self::Bool(Vec::new()),
            ChType::Int8 => Self::Int8(Vec::new()),
            ChType::Int16 => Self::Int16(Vec::new()),
            ChType::Int32 => Self::Int32(Vec::new()),
            ChType::Int64 => Self::Int64(Vec::new()),
            ChType::UInt8 => Self::UInt8(Vec::new()),
            ChType::UInt16 => Self::UInt16(Vec::new()),
            ChType::UInt32 => Self::UInt32(Vec::new()),
            ChType::UInt64 => Self::UInt64(Vec::new()),
            ChType::Float32 => Self::Float32(Vec::new()),
            ChType::Float64 => Self::Float64(Vec::new()),
            ChType::BFloat16 => Self::BFloat16(Vec::new()),
            ChType::QBit { dimension: 0, .. } => {
                return Err(BinaryValueError::Invalid(
                    "QBit dimension must be positive".into(),
                ))
            }
            ChType::FixedString(0) => {
                return Err(BinaryValueError::Invalid(
                    "FixedString width must be positive".into(),
                ))
            }
            ChType::QBit {
                element_type,
                dimension,
            } => Self::QBit {
                values: Box::new(Self::new(&element_type.ch_type(), depth + 1)?),
                dimension: *dimension,
            },
            ChType::Date => Self::Date(Vec::new()),
            ChType::Date32 => Self::Date32(Vec::new()),
            ChType::DateTime { .. } => Self::DateTime(Vec::new()),
            ChType::DateTime64 { .. } => Self::DateTime64(Vec::new()),
            ChType::Time => Self::Time(Vec::new()),
            ChType::Time64 { .. } => Self::Time64(Vec::new()),
            ChType::Interval(_) => Self::Interval(Vec::new()),
            ChType::Enum8 { .. } => Self::Enum8(Vec::new()),
            ChType::Enum16 { .. } => Self::Enum16(Vec::new()),
            ChType::Ipv4 => Self::Ipv4(Vec::new()),
            ChType::String => Self::Utf8 {
                offsets: vec![0],
                data: Vec::new(),
            },
            ChType::FixedString(width) => Self::fixed(*width, FixedKind::FixedString),
            ChType::Uuid => Self::fixed(16, FixedKind::Uuid),
            ChType::Ipv6 => Self::fixed(16, FixedKind::Ipv6),
            ChType::Int128 => Self::fixed(16, FixedKind::Int128),
            ChType::UInt128 => Self::fixed(16, FixedKind::UInt128),
            ChType::Int256 => Self::fixed(32, FixedKind::Int256),
            ChType::UInt256 => Self::fixed(32, FixedKind::UInt256),
            ChType::Decimal {
                precision,
                scale,
                bits,
            } => {
                if !matches!(bits, 32 | 64 | 128 | 256) {
                    return Err(BinaryValueError::Invalid(format!(
                        "Decimal bit width {bits} is not 32/64/128/256"
                    )));
                }
                Self::Decimal {
                    data: Vec::new(),
                    width: (*bits / 8) as usize,
                    precision: *precision,
                    scale: *scale,
                }
            }
            ChType::Nullable(inner) => Self::Nullable {
                flags: Vec::new(),
                inner: Box::new(Self::new(inner, depth + 1)?),
            },
            // A LowCardinality single value carries no dictionary framing; the
            // decoded column is the plain inner column.
            ChType::LowCardinality(inner) => Self::new(inner, depth + 1)?,
            ChType::Array(inner) => {
                let elements = Self::new(inner, depth + 1)?;
                Self::Array {
                    offsets: vec![0],
                    fill: elements.is_zero_width(),
                    elements: Box::new(elements),
                    zero_width: has_zero_width_values(inner),
                }
            }
            ChType::Map(key, value) => {
                let (keys, values) = (Self::new(key, depth + 1)?, Self::new(value, depth + 1)?);
                Self::Map {
                    offsets: vec![0],
                    fill: keys.is_zero_width() && values.is_zero_width(),
                    keys: Box::new(keys),
                    values: Box::new(values),
                    zero_width: has_zero_width_values(key) && has_zero_width_values(value),
                }
            }
            ChType::Tuple(elements) => Self::Tuple {
                fields: elements
                    .iter()
                    .map(|(_, t)| Self::new(t, depth + 1))
                    .collect::<Result<_, _>>()?,
                rows: 0,
            },
            ChType::Json { typed_paths, .. } => {
                if typed_paths.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
                    return Err(BinaryValueError::Invalid(
                        "JSON typed paths are not strictly sorted".into(),
                    ));
                }
                Self::Json(Box::new(JsonValueBuilder {
                    typed: typed_paths
                        .iter()
                        .map(|(path, ty)| Ok((path.clone(), Self::new(ty, depth + 1)?)))
                        .collect::<Result<_, BinaryValueError>>()?,
                    offsets: vec![0],
                    paths: Utf8Column::new(vec![0], Vec::new()),
                    values: Utf8Column::new(vec![0], Vec::new()),
                    depth,
                    seen: vec![false; typed_paths.len()],
                    path_order: Vec::new(),
                }))
            }
            ChType::Variant(_) | ChType::Dynamic { .. } | ChType::AggregateFunction { .. } => {
                return Err(BinaryValueError::Unsupported(ch_type.to_string()))
            }
            // physical_delegate expanded these above.
            ChType::SimpleAggregateFunction { .. }
            | ChType::Geo(_)
            | ChType::Geometry
            | ChType::Nested(_) => {
                unreachable!("name-decoration aliases expand through physical_delegate")
            }
        })
    }

    fn fixed(width: usize, kind: FixedKind) -> Self {
        Self::Fixed {
            data: Vec::new(),
            width,
            kind,
        }
    }

    /// Whether values of this builder are zero bytes: `Tuple()` or a Tuple of
    /// only such builders.
    fn is_zero_width(&self) -> bool {
        matches!(self, ValueBuilder::Tuple { fields, .. } if fields.iter().all(Self::is_zero_width))
    }

    /// Append `n` values of a zero-width builder in one step per tuple node.
    fn append_zero_width(&mut self, n: usize) -> Result<(), BinaryValueError> {
        if n == 0 {
            return Ok(());
        }
        count_visit();
        let ValueBuilder::Tuple { fields, rows } = self else {
            return Err(BinaryValueError::Invalid(
                "internal: zero-width fill on non-Tuple".into(),
            ));
        };
        for field in fields.iter_mut() {
            field.append_zero_width(n)?;
        }
        *rows = rows
            .checked_add(n)
            .ok_or_else(|| BinaryValueError::Invalid("Tuple row count overflows usize".into()))?;
        Ok(())
    }

    /// Append one value from the reader.
    fn append_value(
        &mut self,
        reader: &mut ByteReader,
        null_budget: &mut NullDefaultBudget,
    ) -> Result<(), BinaryValueError> {
        count_visit();
        match self {
            // The server cannot serialize a Nothing value; only null rows
            // (which take the default path) are representable.
            ValueBuilder::Nothing { .. } => {
                return Err(BinaryValueError::Invalid("non-null Nothing value".into()))
            }
            ValueBuilder::Bool(values) => values.push(reader.read_u8()?),
            ValueBuilder::Int8(values) => values.push(reader.read_u8()? as i8),
            ValueBuilder::Int16(values) => values.push(i16::from_le_bytes(read_le(reader)?)),
            ValueBuilder::Int32(values) => values.push(i32::from_le_bytes(read_le(reader)?)),
            ValueBuilder::Int64(values) => values.push(i64::from_le_bytes(read_le(reader)?)),
            ValueBuilder::UInt8(values) => values.push(reader.read_u8()?),
            ValueBuilder::UInt16(values) => values.push(u16::from_le_bytes(read_le(reader)?)),
            ValueBuilder::UInt32(values) => values.push(u32::from_le_bytes(read_le(reader)?)),
            ValueBuilder::UInt64(values) => values.push(u64::from_le_bytes(read_le(reader)?)),
            ValueBuilder::Float32(values) => values.push(f32::from_le_bytes(read_le(reader)?)),
            ValueBuilder::Float64(values) => values.push(f64::from_le_bytes(read_le(reader)?)),
            ValueBuilder::BFloat16(values) => values.push(read_le(reader)?),
            ValueBuilder::QBit { values, dimension } => {
                let encoded_dimension = usize::try_from(reader.read_varint()?).map_err(|_| {
                    BinaryValueError::Invalid("QBit value dimension overflows usize".into())
                })?;
                if encoded_dimension != *dimension {
                    return Err(BinaryValueError::Invalid(format!(
                        "QBit value dimension {encoded_dimension} does not match type dimension {dimension}"
                    )));
                }
                for _ in 0..*dimension {
                    values.append_value(reader, null_budget)?;
                }
            }
            ValueBuilder::Date(values) => values.push(u16::from_le_bytes(read_le(reader)?)),
            ValueBuilder::Date32(values) => values.push(i32::from_le_bytes(read_le(reader)?)),
            ValueBuilder::DateTime(values) => values.push(u32::from_le_bytes(read_le(reader)?)),
            ValueBuilder::DateTime64(values) => values.push(i64::from_le_bytes(read_le(reader)?)),
            ValueBuilder::Time(values) => values.push(i32::from_le_bytes(read_le(reader)?)),
            ValueBuilder::Time64(values) => values.push(i64::from_le_bytes(read_le(reader)?)),
            ValueBuilder::Interval(values) => values.push(i64::from_le_bytes(read_le(reader)?)),
            ValueBuilder::Enum8(values) => values.push(reader.read_u8()? as i8),
            ValueBuilder::Enum16(values) => values.push(i16::from_le_bytes(read_le(reader)?)),
            ValueBuilder::Ipv4(values) => values.push(u32::from_le_bytes(read_le(reader)?)),
            ValueBuilder::Utf8 { offsets, data } => {
                let len = usize::try_from(reader.read_varint()?).map_err(|_| {
                    BinaryValueError::Invalid("String value length overflows usize".into())
                })?;
                let last = *offsets.last().expect("offsets start with 0");
                let end = i32::try_from(len)
                    .ok()
                    .and_then(|len| last.checked_add(len))
                    .ok_or_else(|| {
                        BinaryValueError::Invalid("String value exceeds i32 offset range".into())
                    })?;
                data.extend_from_slice(reader.read_slice(len)?);
                offsets.push(end);
            }
            ValueBuilder::Fixed { data, width, .. } => {
                data.extend_from_slice(reader.read_slice(*width)?);
            }
            ValueBuilder::Decimal { data, width, .. } => {
                data.extend_from_slice(reader.read_slice(*width)?);
            }
            ValueBuilder::Nullable { flags, inner } => {
                // Mirror the bulk null map: nonzero = null, no value bytes.
                if reader.read_u8()? != 0 {
                    flags.push(1);
                    inner.append_default(null_budget)?;
                } else {
                    flags.push(0);
                    inner.append_value(reader, null_budget)?;
                }
            }
            ValueBuilder::Array {
                offsets,
                elements,
                zero_width,
                fill,
            } => {
                let count = read_element_count(reader, *zero_width, "Array element")?;
                if *fill {
                    elements.append_zero_width(count)?;
                } else {
                    for _ in 0..count {
                        elements.append_value(reader, null_budget)?;
                    }
                }
                push_end_offset(offsets, count)?;
            }
            ValueBuilder::Map {
                offsets,
                keys,
                values,
                zero_width,
                fill,
            } => {
                let count = read_element_count(reader, *zero_width, "Map entry")?;
                if *fill {
                    keys.append_zero_width(count)?;
                    values.append_zero_width(count)?;
                } else {
                    for _ in 0..count {
                        keys.append_value(reader, null_budget)?;
                        values.append_value(reader, null_budget)?;
                    }
                }
                push_end_offset(offsets, count)?;
            }
            ValueBuilder::Tuple { fields, rows } => {
                for field in fields.iter_mut() {
                    field.append_value(reader, null_budget)?;
                }
                *rows += 1;
            }
            ValueBuilder::Json(json) => {
                let JsonValueBuilder {
                    typed,
                    offsets,
                    paths,
                    values,
                    depth,
                    seen,
                    path_order,
                } = json.as_mut();
                let row_start = reader.position();
                let row_bytes = reader.remaining_slice();
                let count = read_element_count(reader, false, "JSON path")?;
                seen.fill(false);
                let mut missing_typed = typed.len();
                path_order.clear();
                for _ in 0..count {
                    let path = read_json_path(reader)?;
                    let path_end = reader.position() - row_start;
                    if let Ok(index) = typed.binary_search_by(|(name, _)| name.as_bytes().cmp(path))
                    {
                        if seen[index] {
                            return Err(BinaryValueError::Invalid(
                                "duplicate JSON typed path".into(),
                            ));
                        }
                        seen[index] = true;
                        missing_typed -= 1;
                        typed[index].1.append_value(reader, null_budget)?;
                    } else {
                        let start = reader.position();
                        let ty = read_value_type(reader, *depth + 1)?;
                        ty.skip(reader)?;
                        path_order.push((
                            path_end - path.len()..path_end,
                            start - row_start..reader.position() - row_start,
                        ));
                    }
                }
                path_order
                    .sort_unstable_by(|a, b| row_bytes[a.0.clone()].cmp(&row_bytes[b.0.clone()]));
                if path_order
                    .windows(2)
                    .any(|pair| row_bytes[pair[0].0.clone()] == row_bytes[pair[1].0.clone()])
                {
                    return Err(BinaryValueError::Invalid("duplicate JSON path".into()));
                }
                for (path, cell) in path_order.iter() {
                    append_utf8(paths, &row_bytes[path.clone()])?;
                    append_utf8(values, &row_bytes[cell.clone()])?;
                }
                null_budget.charge(missing_typed)?;
                for ((_, builder), seen) in typed.iter_mut().zip(seen.iter()) {
                    if !seen {
                        builder.append_default(null_budget)?;
                    }
                }
                push_end_offset(offsets, path_order.len())?;
            }
        }
        Ok(())
    }

    /// Append the type's default placeholder for a null row, matching the
    /// placeholder values the bulk decoder stores under a null map bit.
    fn append_default(
        &mut self,
        null_budget: &mut NullDefaultBudget,
    ) -> Result<(), BinaryValueError> {
        count_visit();
        match self {
            ValueBuilder::Nothing { rows } => {
                null_budget.charge(1)?;
                *rows += 1;
            }
            ValueBuilder::Bool(values) => {
                null_budget.charge(1)?;
                values.push(0);
            }
            ValueBuilder::Int8(values) => {
                null_budget.charge(1)?;
                values.push(0);
            }
            ValueBuilder::Int16(values) => {
                null_budget.charge(2)?;
                values.push(0);
            }
            ValueBuilder::Int32(values) => {
                null_budget.charge(4)?;
                values.push(0);
            }
            ValueBuilder::Int64(values) => {
                null_budget.charge(8)?;
                values.push(0);
            }
            ValueBuilder::UInt8(values) => {
                null_budget.charge(1)?;
                values.push(0);
            }
            ValueBuilder::UInt16(values) => {
                null_budget.charge(2)?;
                values.push(0);
            }
            ValueBuilder::UInt32(values) => {
                null_budget.charge(4)?;
                values.push(0);
            }
            ValueBuilder::UInt64(values) => {
                null_budget.charge(8)?;
                values.push(0);
            }
            ValueBuilder::Float32(values) => {
                null_budget.charge(4)?;
                values.push(0.0);
            }
            ValueBuilder::Float64(values) => {
                null_budget.charge(8)?;
                values.push(0.0);
            }
            ValueBuilder::BFloat16(values) => {
                null_budget.charge(2)?;
                values.push([0; 2]);
            }
            ValueBuilder::QBit { values, dimension } => {
                let (bytes_per_value, len) = match values.as_mut() {
                    ValueBuilder::BFloat16(values) => (2, values.len()),
                    ValueBuilder::Float32(values) => (4, values.len()),
                    ValueBuilder::Float64(values) => (8, values.len()),
                    _ => unreachable!("QBit builder always contains its declared scalar type"),
                };
                let bytes = dimension.checked_mul(bytes_per_value).ok_or_else(|| {
                    BinaryValueError::Invalid("QBit null placeholder size overflows usize".into())
                })?;
                null_budget.charge(bytes)?;
                let new_len = len.checked_add(*dimension).ok_or_else(|| {
                    BinaryValueError::Invalid("QBit null placeholder length overflows usize".into())
                })?;
                match values.as_mut() {
                    ValueBuilder::BFloat16(values) => values.resize(new_len, [0; 2]),
                    ValueBuilder::Float32(values) => values.resize(new_len, 0.0),
                    ValueBuilder::Float64(values) => values.resize(new_len, 0.0),
                    _ => unreachable!("QBit builder always contains its declared scalar type"),
                }
            }
            ValueBuilder::Date(values) => {
                null_budget.charge(2)?;
                values.push(0);
            }
            ValueBuilder::Date32(values) => {
                null_budget.charge(4)?;
                values.push(0);
            }
            ValueBuilder::DateTime(values) => {
                null_budget.charge(4)?;
                values.push(0);
            }
            ValueBuilder::DateTime64(values) => {
                null_budget.charge(8)?;
                values.push(0);
            }
            ValueBuilder::Time(values) => {
                null_budget.charge(4)?;
                values.push(0);
            }
            ValueBuilder::Time64(values) => {
                null_budget.charge(8)?;
                values.push(0);
            }
            ValueBuilder::Interval(values) => {
                null_budget.charge(8)?;
                values.push(0);
            }
            ValueBuilder::Enum8(values) => {
                null_budget.charge(1)?;
                values.push(0);
            }
            ValueBuilder::Enum16(values) => {
                null_budget.charge(2)?;
                values.push(0);
            }
            ValueBuilder::Ipv4(values) => {
                null_budget.charge(4)?;
                values.push(0);
            }
            ValueBuilder::Utf8 { offsets, .. } => {
                null_budget.charge(4)?;
                let last = *offsets.last().expect("offsets start with 0");
                offsets.push(last);
            }
            ValueBuilder::Fixed { data, width, .. } | ValueBuilder::Decimal { data, width, .. } => {
                null_budget.charge(*width)?;
                let new_len = data.len().checked_add(*width).ok_or_else(|| {
                    BinaryValueError::Invalid("null placeholder length overflows usize".into())
                })?;
                data.resize(new_len, 0);
            }
            ValueBuilder::Nullable { flags, inner } => {
                null_budget.charge(1)?;
                flags.push(1);
                inner.append_default(null_budget)?;
            }
            ValueBuilder::Array { offsets, .. } | ValueBuilder::Map { offsets, .. } => {
                null_budget.charge(8)?;
                let last = *offsets.last().expect("offsets start with 0");
                offsets.push(last);
            }
            ValueBuilder::Tuple { fields, rows } => {
                null_budget.charge(1)?;
                for field in fields.iter_mut() {
                    field.append_default(null_budget)?;
                }
                *rows += 1;
            }
            ValueBuilder::Json(json) => {
                let JsonValueBuilder { typed, offsets, .. } = json.as_mut();
                null_budget.charge(typed.len())?;
                null_budget.charge(8)?;
                for (_, field) in typed {
                    field.append_default(null_budget)?;
                }
                push_end_offset(offsets, 0)?;
            }
        }
        Ok(())
    }

    /// Consume the builder into a Column. `validity` is supplied only by the
    /// enclosing `Nullable` wrapper.
    fn finish(self, validity: Option<Bitmap>) -> Result<Column, BinaryValueError> {
        fn reject_validity(validity: &Option<Bitmap>, what: &str) -> Result<(), BinaryValueError> {
            if validity.is_some() {
                return Err(BinaryValueError::Invalid(format!(
                    "{what} cannot be inside Nullable"
                )));
            }
            Ok(())
        }
        Ok(match self {
            ValueBuilder::Nothing { rows } => Column::Nothing(match validity {
                Some(bm) => NothingColumn::new_nullable(rows, bm),
                None => NothingColumn::new(rows),
            }),
            ValueBuilder::Bool(bytes) => Column::Bool(match validity {
                Some(bm) => BoolColumn::from_wire_bytes_nullable(&bytes, bm),
                None => BoolColumn::from_wire_bytes(&bytes),
            }),
            ValueBuilder::Int8(values) => Column::Int8(PrimitiveColumn { values, validity }),
            ValueBuilder::Int16(values) => Column::Int16(PrimitiveColumn { values, validity }),
            ValueBuilder::Int32(values) => Column::Int32(PrimitiveColumn { values, validity }),
            ValueBuilder::Int64(values) => Column::Int64(PrimitiveColumn { values, validity }),
            ValueBuilder::UInt8(values) => Column::UInt8(PrimitiveColumn { values, validity }),
            ValueBuilder::UInt16(values) => Column::UInt16(PrimitiveColumn { values, validity }),
            ValueBuilder::UInt32(values) => Column::UInt32(PrimitiveColumn { values, validity }),
            ValueBuilder::UInt64(values) => Column::UInt64(PrimitiveColumn { values, validity }),
            ValueBuilder::Float32(values) => Column::Float32(PrimitiveColumn { values, validity }),
            ValueBuilder::Float64(values) => Column::Float64(PrimitiveColumn { values, validity }),
            ValueBuilder::BFloat16(values) => {
                Column::BFloat16(PrimitiveColumn { values, validity })
            }
            ValueBuilder::QBit { values, dimension } => Column::QBit(match validity {
                Some(validity) => {
                    QBitColumn::new_nullable(values.finish(None)?, dimension, validity)
                }
                None => QBitColumn::new(values.finish(None)?, dimension),
            }),
            ValueBuilder::Date(values) => Column::Date(PrimitiveColumn { values, validity }),
            ValueBuilder::Date32(values) => Column::Date32(PrimitiveColumn { values, validity }),
            ValueBuilder::DateTime(values) => {
                Column::DateTime(PrimitiveColumn { values, validity })
            }
            ValueBuilder::DateTime64(values) => {
                Column::DateTime64(PrimitiveColumn { values, validity })
            }
            ValueBuilder::Time(values) => Column::Time(PrimitiveColumn { values, validity }),
            ValueBuilder::Time64(values) => Column::Time64(PrimitiveColumn { values, validity }),
            ValueBuilder::Interval(values) => {
                Column::Interval(PrimitiveColumn { values, validity })
            }
            ValueBuilder::Enum8(values) => Column::Enum8(PrimitiveColumn { values, validity }),
            ValueBuilder::Enum16(values) => Column::Enum16(PrimitiveColumn { values, validity }),
            ValueBuilder::Ipv4(values) => Column::Ipv4(PrimitiveColumn { values, validity }),
            ValueBuilder::Utf8 { offsets, data } => Column::Utf8(match validity {
                Some(bm) => Utf8Column::new_nullable(offsets, data, bm),
                None => Utf8Column::new(offsets, data),
            }),
            ValueBuilder::Fixed { data, width, kind } => {
                let column = match validity {
                    Some(bm) => FixedBinaryColumn::new_nullable(data, width, bm),
                    None => FixedBinaryColumn::new(data, width),
                };
                match kind {
                    FixedKind::FixedString => Column::FixedBinary(column),
                    FixedKind::Uuid => Column::Uuid(column),
                    FixedKind::Ipv6 => Column::Ipv6(column),
                    FixedKind::Int128 => Column::Int128(column),
                    FixedKind::UInt128 => Column::UInt128(column),
                    FixedKind::Int256 => Column::Int256(column),
                    FixedKind::UInt256 => Column::UInt256(column),
                }
            }
            ValueBuilder::Decimal {
                data,
                width,
                precision,
                scale,
            } => Column::Decimal(match validity {
                Some(bm) => DecimalColumn::new_nullable(data, width, precision, scale, bm),
                None => DecimalColumn::new(data, width, precision, scale),
            }),
            ValueBuilder::Nullable { flags, inner } => {
                reject_validity(&validity, "Nullable")?;
                return inner.finish(Some(Bitmap::from_ch_null_map(&flags)));
            }
            ValueBuilder::Array {
                offsets, elements, ..
            } => {
                reject_validity(&validity, "Array")?;
                Column::Array(ArrayColumn::new(offsets, elements.finish(None)?))
            }
            ValueBuilder::Map {
                offsets,
                keys,
                values,
                ..
            } => {
                reject_validity(&validity, "Map")?;
                let total = usize::try_from(*offsets.last().expect("offsets start with 0"))
                    .map_err(|_| {
                        BinaryValueError::Invalid("Map entry count overflows usize".into())
                    })?;
                let entries = Column::Tuple(TupleColumn::new(
                    vec![keys.finish(None)?, values.finish(None)?],
                    total,
                ));
                Column::Map(MapColumn::new(offsets, entries))
            }
            ValueBuilder::Json(json) => {
                let JsonValueBuilder {
                    typed,
                    offsets,
                    paths,
                    values,
                    ..
                } = *json;
                let len = offsets.len() - 1;
                let typed = typed
                    .into_iter()
                    .map(|(path, builder)| Ok((path, builder.finish(None)?)))
                    .collect::<Result<_, BinaryValueError>>()?;
                Column::Json(
                    JsonColumn::structured(StructuredJson::from_parts(
                        typed,
                        Vec::new(),
                        offsets,
                        paths,
                        values,
                        len,
                    ))
                    .with_validity(validity),
                )
            }
            ValueBuilder::Tuple { fields, rows } => {
                let columns = fields
                    .into_iter()
                    .map(|field| field.finish(None))
                    .collect::<Result<Vec<_>, _>>()?;
                Column::Tuple(match validity {
                    Some(bm) => TupleColumn::new_nullable(columns, rows, bm),
                    None => TupleColumn::new(columns, rows),
                })
            }
        })
    }
}

/// Push a container's next Arrow end offset, `count` past the previous one.
fn push_end_offset(offsets: &mut Vec<i64>, count: usize) -> Result<(), BinaryValueError> {
    let last = *offsets.last().expect("offsets start with 0");
    let end = i64::try_from(count)
        .ok()
        .and_then(|count| last.checked_add(count))
        .ok_or_else(|| BinaryValueError::Invalid("element count overflows i64 offsets".into()))?;
    offsets.push(end);
    Ok(())
}

fn append_utf8(column: &mut Utf8Column, bytes: &[u8]) -> Result<(), BinaryValueError> {
    let end = column
        .data
        .len()
        .checked_add(bytes.len())
        .and_then(|end| i32::try_from(end).ok())
        .ok_or_else(|| BinaryValueError::Invalid("JSON data exceeds i32 offsets".into()))?;
    column.data.extend_from_slice(bytes);
    column.offsets.push(end);
    Ok(())
}

fn read_json_path<'a>(reader: &mut ByteReader<'a>) -> Result<&'a [u8], BinaryValueError> {
    let len = usize::try_from(reader.read_varint()?)
        .map_err(|_| BinaryValueError::Invalid("JSON path length overflows usize".into()))?;
    Ok(reader.read_slice(len)?)
}

fn read_value_type(reader: &mut ByteReader, depth: usize) -> Result<SkipPlan, BinaryValueError> {
    let ty = read_binary_type(reader).map_err(|error| match error {
        BinaryTypeError::Unsupported(reason) => BinaryValueError::Unsupported(reason),
        BinaryTypeError::Invalid(reason) => BinaryValueError::Invalid(reason),
        BinaryTypeError::Io(error) => error.into(),
    })?;
    check_type(&ty, None, depth).ok_or_else(too_deep)?;
    if ty == ChType::Nothing {
        // Dynamic NULL has only its Nothing descriptor.
        Ok(SkipPlan::Fixed(0))
    } else {
        SkipPlan::new(&ty, depth)
    }
}

#[cfg(test)]
thread_local! {
    static SKIP_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Compiled row framing for one binary descriptor.
///
/// At v26.8.1.2041-lts, SerializationObject::serializeBinary writes a path
/// count, then path/value pairs. Declared typed paths use their serializer;
/// every other value starts with a DataTypesBinaryEncoding descriptor.
enum SkipPlan {
    Nothing,
    Fixed(usize),
    String,
    Nullable(Box<SkipPlan>),
    Array(Box<SkipPlan>),
    Map(Box<SkipPlan>),
    Tuple(Vec<SkipPlan>),
    QBit {
        dimension: usize,
        bytes: usize,
    },
    Json {
        typed: Vec<(String, SkipPlan)>,
        depth: usize,
    },
    Dynamic {
        depth: usize,
    },
    Variant(Vec<SkipPlan>),
    Unsupported(String),
}

impl SkipPlan {
    fn new(ch_type: &ChType, depth: usize) -> Result<Self, BinaryValueError> {
        if depth > MAX_TYPE_DEPTH {
            return Err(too_deep());
        }
        if let Some(delegate) = ch_type.physical_delegate_ref() {
            return Self::new(
                delegate.as_ref(),
                depth + usize::from(matches!(ch_type, ChType::SimpleAggregateFunction { .. })),
            );
        }
        let width = match ch_type {
            ChType::Nothing => return Ok(Self::Nothing),
            ChType::Bool | ChType::Int8 | ChType::UInt8 | ChType::Enum8 { .. } => 1,
            ChType::Int16
            | ChType::UInt16
            | ChType::Date
            | ChType::Enum16 { .. }
            | ChType::BFloat16 => 2,
            ChType::Int32
            | ChType::UInt32
            | ChType::Float32
            | ChType::Date32
            | ChType::DateTime { .. }
            | ChType::Time
            | ChType::Ipv4 => 4,
            ChType::Int64
            | ChType::UInt64
            | ChType::Float64
            | ChType::DateTime64 { .. }
            | ChType::Time64 { .. }
            | ChType::Interval(_) => 8,
            ChType::Int128 | ChType::UInt128 | ChType::Uuid | ChType::Ipv6 => 16,
            ChType::Int256 | ChType::UInt256 => 32,
            ChType::Decimal { bits, .. } if matches!(bits, 32 | 64 | 128 | 256) => {
                usize::from(*bits / 8)
            }
            ChType::FixedString(width) if *width != 0 => *width,
            ChType::String => return Ok(Self::String),
            ChType::Nullable(inner) => {
                return Ok(Self::Nullable(Box::new(Self::new(inner, depth + 1)?)))
            }
            ChType::LowCardinality(inner) => return Self::new(inner, depth + 1),
            ChType::Array(inner) => return Ok(Self::Array(Box::new(Self::new(inner, depth + 1)?))),
            ChType::Map(key, value) => {
                return Ok(Self::Map(Box::new(Self::sequence([
                    Self::new(key, depth + 1)?,
                    Self::new(value, depth + 1)?,
                ])?)))
            }
            ChType::Tuple(fields) => {
                return Self::sequence(
                    fields
                        .iter()
                        .map(|(_, ty)| Self::new(ty, depth + 1))
                        .collect::<Result<Vec<_>, _>>()?,
                )
            }
            ChType::QBit {
                element_type,
                dimension,
            } => {
                let bytes = dimension
                    .checked_mul(element_type.byte_width())
                    .ok_or_else(|| {
                        BinaryValueError::Invalid("QBit payload size overflows usize".into())
                    })?;
                return Ok(Self::QBit {
                    dimension: *dimension,
                    bytes,
                });
            }
            ChType::Json { typed_paths, .. } => {
                return Ok(Self::Json {
                    typed: typed_paths
                        .iter()
                        .map(|(path, ty)| Ok((path.clone(), Self::new(ty, depth + 1)?)))
                        .collect::<Result<_, BinaryValueError>>()?,
                    depth,
                })
            }
            ChType::Dynamic { .. } => return Ok(Self::Dynamic { depth }),
            ChType::Variant(alternatives) => {
                return Ok(Self::Variant(
                    alternatives
                        .iter()
                        .map(|ty| Self::new(ty, depth + 1))
                        .collect::<Result<_, _>>()?,
                ))
            }
            ChType::AggregateFunction { .. } => return Ok(Self::Unsupported(ch_type.to_string())),
            _ => {
                return Err(BinaryValueError::Invalid(
                    "invalid binary value type".into(),
                ))
            }
        };
        Ok(Self::Fixed(width))
    }

    fn sequence(parts: impl IntoIterator<Item = Self>) -> Result<Self, BinaryValueError> {
        let mut sequence = Vec::new();
        let mut width = 0usize;
        for part in parts {
            if let Self::Fixed(bytes) = part {
                width = width.checked_add(bytes).ok_or_else(|| {
                    BinaryValueError::Invalid("tuple payload size overflows usize".into())
                })?;
            } else {
                if width != 0 {
                    sequence.push(Self::Fixed(width));
                    width = 0;
                }
                sequence.push(part);
            }
        }
        if width != 0 {
            sequence.push(Self::Fixed(width));
        }
        Ok(match sequence.len() {
            0 => Self::Fixed(0),
            1 => sequence.remove(0),
            _ => Self::Tuple(sequence),
        })
    }

    fn skip(&self, reader: &mut ByteReader) -> Result<(), BinaryValueError> {
        #[cfg(test)]
        SKIP_STEPS.with(|steps| steps.set(steps.get() + 1));
        match self {
            Self::Nothing => {
                return Err(BinaryValueError::Invalid("non-null Nothing value".into()))
            }
            Self::Fixed(bytes) => reader.skip(*bytes)?,
            Self::String => {
                let len = usize::try_from(reader.read_varint()?).map_err(|_| {
                    BinaryValueError::Invalid("String length overflows usize".into())
                })?;
                reader.skip(len)?;
            }
            Self::Nullable(inner) => {
                if reader.read_u8()? == 0 {
                    inner.skip(reader)?;
                }
            }
            Self::Array(inner) | Self::Map(inner) => {
                let what = if matches!(self, Self::Array(_)) {
                    "Array element"
                } else {
                    "Map entry"
                };
                let count =
                    read_element_count(reader, matches!(inner.as_ref(), Self::Fixed(0)), what)?;
                if let Self::Fixed(width) = inner.as_ref() {
                    let bytes = count.checked_mul(*width).ok_or_else(|| {
                        BinaryValueError::Invalid("container payload size overflows usize".into())
                    })?;
                    reader.skip(bytes)?;
                } else {
                    for _ in 0..count {
                        inner.skip(reader)?;
                    }
                }
            }
            Self::Tuple(fields) => {
                for field in fields {
                    field.skip(reader)?;
                }
            }
            Self::QBit { dimension, bytes } => {
                if reader.read_varint()? != *dimension as u64 {
                    return Err(BinaryValueError::Invalid("QBit dimension mismatch".into()));
                }
                reader.skip(*bytes)?;
            }
            Self::Json { typed, depth } => {
                let count = read_element_count(reader, false, "JSON path")?;
                for _ in 0..count {
                    let path = read_json_path(reader)?;
                    match typed.binary_search_by(|(name, _)| name.as_bytes().cmp(path)) {
                        Ok(index) => typed[index].1.skip(reader)?,
                        Err(_) => read_value_type(reader, depth + 1)?.skip(reader)?,
                    }
                }
            }
            Self::Dynamic { depth } => read_value_type(reader, depth + 1)?.skip(reader)?,
            Self::Variant(alternatives) => {
                let discriminator = reader.read_u8()?;
                if discriminator != u8::MAX {
                    let plan = alternatives
                        .get(usize::from(discriminator))
                        .ok_or_else(|| {
                            BinaryValueError::Invalid("invalid Variant discriminator".into())
                        })?;
                    plan.skip(reader)?;
                }
            }
            Self::Unsupported(name) => return Err(BinaryValueError::Unsupported(name.clone())),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Append the binary type descriptor (`DataTypesBinaryEncoding`) for
/// `ch_type`, the inverse of [`read_binary_type_prefix`].
///
/// A SharedVariant cell or a JSON shared-data value is this descriptor
/// followed by [`encode_binary_value`] for the same type. For these cells,
/// strip a top-level `Nullable(T)` to `T`, including
/// `LowCardinality(Nullable(T))` to `LowCardinality(T)`. Alias-wrapped nullable
/// types normalize to their physical value type: at v26.8.1.2041-lts,
/// `SimpleAggregateFunction(anyLast, Nullable(String))` becomes `String`, and
/// `SimpleAggregateFunction(anyLast, LowCardinality(Nullable(String)))` becomes
/// `LowCardinality(String)`. Non-nullable SAF descriptors retain the alias.
/// Route null rows through the containing Dynamic NULL representation instead
/// of writing a cell, and omit null paths from JSON shared data. Nested Nullable
/// types keep their flag bytes.
///
/// Invalid or unsupported descriptors return an error and leave `buf` unchanged.
/// SimpleAggregateFunction and AggregateFunction use the server-accepted Custom
/// name descriptor, rather than the server's specialized descriptor. Value bytes
/// use the same format. This is a per-type operation: validating the descriptor
/// by reading it back allocates metadata, so it is far costlier than encoding a
/// value. Cache the prefix per type or column instead of writing it again for
/// each row.
pub fn write_binary_type_prefix(
    buf: &mut Vec<u8>,
    ch_type: &ChType,
) -> Result<(), BinaryValueError> {
    check_type_depth(ch_type)?;
    let start = buf.len();
    write_binary_type(buf, ch_type);
    let result = read_binary_type_prefix(&buf[start..]).and_then(|(parsed, consumed)| {
        if parsed == *ch_type && consumed == buf.len() - start {
            Ok(())
        } else {
            Err(BinaryValueError::Invalid(
                "type does not round-trip through its binary descriptor".into(),
            ))
        }
    });
    if result.is_err() {
        buf.truncate(start);
    }
    result
}

/// Append row `row` of `column`, read as `ch_type`, in the server's
/// `serializeBinary` row format: the inverse of [`decode_binary_value`], with
/// the same layout and type coverage (see the module docs).
///
/// `column` has the shape the bulk decoder builds for `ch_type`, so a column
/// from a decoded block can be passed directly with any row index. For a
/// `LowCardinality(T)` type both the bulk `Dictionary` column and the plain
/// inner column that [`decode_binary_value`] returns are accepted. A `Nullable`
/// type reads the row's null bit from the column's validity bitmap.
///
/// When building SharedVariant or JSON shared cells, strip top-level Nullable
/// wrappers as described in [`write_binary_type_prefix`], and route null rows
/// through the containing Dynamic NULL representation. Omit null paths from JSON
/// shared data. Nullable flags are for nested values, such as elements of
/// `Array(Nullable(T))`.
///
/// `Variant`, `Dynamic`, and `AggregateFunction` values are `Unsupported`,
/// matching the decoder. Non-null `Nothing` values are `Invalid`. A row out of
/// range or a column whose shape does not match `ch_type`, or a null row under a
/// non-nullable type, is `Invalid`. The column is caller memory rather than
/// wire input, but malformed buffers still return an error instead of panicking.
/// On error `buf` is left as it was.
pub fn encode_binary_value(
    buf: &mut Vec<u8>,
    ch_type: &ChType,
    column: &Column,
    row: usize,
) -> Result<(), BinaryValueError> {
    let check = check_type(ch_type, Some(column), 0).ok_or_else(too_deep)?;
    if !check.supported {
        return Err(unsupported(ch_type));
    }
    if !check.matches {
        return Err(mismatch(ch_type));
    }
    let start = buf.len();
    let result = encode_value(buf, ch_type, column, row, 0);
    if result.is_err() {
        buf.truncate(start);
    }
    result
}

/// The result of a complete bounded type/column traversal. Keep mismatch and
/// coverage separate so an unsupported sibling wins over a malformed buffer.
#[derive(Clone, Copy)]
struct TypeCheck {
    supported: bool,
    matches: bool,
    descriptor_nodes: usize,
}

impl TypeCheck {
    #[inline]
    fn combine(&mut self, other: Self) {
        self.supported &= other.supported;
        self.matches &= other.matches;
        self.descriptor_nodes = self.descriptor_nodes.saturating_add(other.descriptor_nodes);
    }

    fn check_descriptor_complexity(self) -> Result<(), BinaryValueError> {
        if self.descriptor_nodes > MAX_BINARY_TYPE_COMPLEXITY {
            return Err(BinaryValueError::Invalid(format!(
                "complexity exceeds {MAX_BINARY_TYPE_COMPLEXITY}"
            )));
        }
        Ok(())
    }
}

/// Check depth, coverage, descriptor nodes, and optional column shape without
/// allocations. None means too deep. Every type child is visited even if its
/// column shape is already wrong, before any error can render the type.
/// Depth charges match `encode::validate::type_depth`, including empty aliases.
///
/// Single-child wrappers (`Nullable`, `LowCardinality`, and `Array`) are walked
/// in a loop, so a chain like
/// `Array(LowCardinality(Nullable(String)))` costs one frame and a hostile
/// million-deep chain is rejected without recursion. Branching nodes and
/// `SimpleAggregateFunction` recurse, bounded by the depth gate.
#[inline]
fn check_type(
    mut ch_type: &ChType,
    mut column: Option<&Column>,
    mut depth: usize,
) -> Option<TypeCheck> {
    let mut check = TypeCheck {
        supported: true,
        matches: true,
        descriptor_nodes: 0,
    };
    loop {
        if depth > MAX_TYPE_DEPTH {
            return None;
        }
        check.descriptor_nodes = check.descriptor_nodes.saturating_add(1);
        match ch_type {
            ChType::Nullable(inner) => {
                ch_type = inner;
            }
            ChType::SimpleAggregateFunction { inner, .. } => {
                let inner = check_type(inner, column, depth + 1)?;
                check.supported &= inner.supported;
                check.matches &= inner.matches;
                // The descriptor writer uses one Custom tag for this alias.
                return Some(check);
            }
            ChType::LowCardinality(inner) => {
                if let Some(Column::Dictionary(dict)) = column {
                    column = Some(dict.values.as_ref());
                }
                ch_type = inner;
            }
            ChType::Array(inner) => {
                column = match column {
                    Some(Column::Array(array)) => Some(array.values.as_ref()),
                    Some(_) => {
                        check.matches = false;
                        None
                    }
                    None => None,
                };
                ch_type = inner;
            }
            ChType::Tuple(fields) => {
                let columns = match column {
                    Some(Column::Tuple(tuple)) => {
                        check.matches &= fields.len() == tuple.fields.len();
                        Some(tuple.fields.as_slice())
                    }
                    Some(_) => {
                        check.matches = false;
                        None
                    }
                    None => None,
                };
                for (index, (_, ty)) in fields.iter().enumerate() {
                    let field = columns.and_then(|cols| cols.get(index));
                    check.combine(check_type(ty, field, depth + 1)?);
                }
                return Some(check);
            }
            ChType::Nested(fields) => {
                let columns = match column {
                    Some(Column::Array(array)) => match array.values.as_ref() {
                        Column::Tuple(tuple) => {
                            check.matches &= fields.len() == tuple.fields.len();
                            Some(tuple.fields.as_slice())
                        }
                        _ => {
                            check.matches = false;
                            None
                        }
                    },
                    Some(_) => {
                        check.matches = false;
                        None
                    }
                    None => None,
                };
                for (index, (_, ty)) in fields.iter().enumerate() {
                    let field = columns.and_then(|cols| cols.get(index));
                    check.combine(check_type(ty, field, depth + 2)?);
                }
                return Some(check);
            }
            ChType::Map(key, value) => {
                let entries = match column {
                    Some(Column::Map(map)) => match map.entries.as_ref() {
                        Column::Tuple(tuple) if tuple.fields.len() == 2 => {
                            Some(tuple.fields.as_slice())
                        }
                        _ => {
                            check.matches = false;
                            None
                        }
                    },
                    Some(_) => {
                        check.matches = false;
                        None
                    }
                    None => None,
                };
                check.combine(check_type(key, entries.map(|cols| &cols[0]), depth + 1)?);
                check.combine(check_type(value, entries.map(|cols| &cols[1]), depth + 1)?);
                return Some(check);
            }
            ChType::Geo(kind) => {
                if depth + kind.expansion_depth() > MAX_TYPE_DEPTH {
                    return None;
                }
                if let Some(mut point) = column {
                    for _ in 1..kind.expansion_depth() {
                        let Column::Array(array) = point else {
                            check.matches = false;
                            return Some(check);
                        };
                        point = &array.values;
                    }
                    check.matches &= matches!(point, Column::Tuple(tuple)
                        if matches!(tuple.fields.as_slice(), [Column::Float64(_), Column::Float64(_)]));
                }
                return Some(check);
            }
            ChType::Geometry => {
                if depth + GEOMETRY_EXPANSION_DEPTH > MAX_TYPE_DEPTH {
                    return None;
                }
                check.supported = false;
                return Some(check);
            }
            ChType::Variant(alternatives) => {
                for ty in alternatives {
                    check.combine(check_type(ty, None, depth + 1)?);
                }
                check.supported = false;
                return Some(check);
            }
            ChType::AggregateFunction { arguments, .. } => {
                for ty in arguments {
                    check_type(ty, None, depth + 1)?;
                }
                check.supported = false;
                return Some(check);
            }
            ChType::Json { typed_paths, .. } => {
                let body = match column {
                    Some(Column::Json(JsonColumn {
                        body: JsonBody::Structured(body),
                        ..
                    })) => Some(body.as_ref()),
                    Some(Column::Json(_)) => {
                        check.supported = false;
                        None
                    }
                    Some(_) => {
                        check.matches = false;
                        None
                    }
                    None => None,
                };
                check.matches &= !typed_paths.windows(2).any(|pair| pair[0].0 >= pair[1].0);
                if let Some(body) = body {
                    check.matches &= body.typed.len() == typed_paths.len()
                        && !body.typed.windows(2).any(|pair| pair[0].0 >= pair[1].0);
                }
                for (path, ty) in typed_paths {
                    let child = body.and_then(|body| {
                        body.typed
                            .binary_search_by(|(name, _)| name.as_bytes().cmp(path.as_bytes()))
                            .ok()
                            .map(|index| &body.typed[index].1)
                    });
                    if body.is_some() && child.is_none() {
                        check.matches = false;
                    }
                    check.combine(check_type(ty, child, depth + 1)?);
                }
                return Some(check);
            }
            ChType::Dynamic { .. } => {
                check.supported = false;
                return Some(check);
            }
            _ => {
                if matches!(ch_type, ChType::QBit { .. }) {
                    check.descriptor_nodes = check.descriptor_nodes.saturating_add(1);
                }
                if let Some(column) = column {
                    check.matches &= scalar_variant_matches(ch_type, column);
                }
                return Some(check);
            }
        }
        depth += 1;
    }
}

/// Check descriptor depth without column shape or single-value coverage.
fn check_type_depth(ch_type: &ChType) -> Result<(), BinaryValueError> {
    check_type(ch_type, None, 0)
        .map(|_| ())
        .ok_or_else(too_deep)
}

/// Scalar leaf matching stays local to avoid the block validator's delegate
/// expansion and container dispatch on every leaf of this already-walked tree.
/// Fixed-width leaves also compare the stored width or dimension, so a shape
/// mismatch is reported before any bytes are written.
#[inline]
fn scalar_variant_matches(ch_type: &ChType, column: &Column) -> bool {
    match (ch_type, column) {
        (ChType::FixedString(width), Column::FixedBinary(c)) => *width != 0 && c.width == *width,
        (ChType::Uuid, Column::Uuid(c))
        | (ChType::Ipv6, Column::Ipv6(c))
        | (ChType::Int128, Column::Int128(c))
        | (ChType::UInt128, Column::UInt128(c)) => c.width == 16,
        (ChType::Int256, Column::Int256(c)) | (ChType::UInt256, Column::UInt256(c)) => {
            c.width == 32
        }
        (
            ChType::Decimal {
                bits,
                precision,
                scale,
            },
            Column::Decimal(c),
        ) => {
            matches!(bits, 32 | 64 | 128 | 256)
                && c.width == *bits as usize / 8
                && c.precision == *precision
                && c.scale == *scale
        }
        (ChType::QBit { dimension, .. }, Column::QBit(c)) => {
            *dimension != 0 && c.dimension == *dimension
        }
        _ => matches!(
            (ch_type, column),
            (ChType::Nothing, Column::Nothing(_))
                | (ChType::Bool, Column::Bool(_))
                | (ChType::Int8, Column::Int8(_))
                | (ChType::Int16, Column::Int16(_))
                | (ChType::Int32, Column::Int32(_))
                | (ChType::Int64, Column::Int64(_))
                | (ChType::UInt8, Column::UInt8(_))
                | (ChType::UInt16, Column::UInt16(_))
                | (ChType::UInt32, Column::UInt32(_))
                | (ChType::UInt64, Column::UInt64(_))
                | (ChType::Float32, Column::Float32(_))
                | (ChType::Float64, Column::Float64(_))
                | (ChType::BFloat16, Column::BFloat16(_))
                | (ChType::Date, Column::Date(_))
                | (ChType::Date32, Column::Date32(_))
                | (ChType::DateTime { .. }, Column::DateTime(_))
                | (ChType::DateTime64 { .. }, Column::DateTime64(_))
                | (ChType::Time, Column::Time(_))
                | (ChType::Time64 { .. }, Column::Time64(_))
                | (ChType::Interval(_), Column::Interval(_))
                | (ChType::Ipv4, Column::Ipv4(_))
                | (ChType::String, Column::Utf8(_))
                | (ChType::Enum8 { .. }, Column::Enum8(_))
                | (ChType::Enum16 { .. }, Column::Enum16(_))
        ),
    }
}

// Error constructors stay out of line and cold so the hot encode path keeps
// only a compare-and-branch per check; the `format!` and `to_string` bodies
// would otherwise block inlining of the small bounds helpers.

#[cold]
#[inline(never)]
fn too_deep() -> BinaryValueError {
    BinaryValueError::Invalid(format!("nesting exceeds {MAX_TYPE_DEPTH}"))
}

#[cold]
#[inline(never)]
fn unsupported(ch_type: &ChType) -> BinaryValueError {
    BinaryValueError::Unsupported(ch_type.to_string())
}

#[cold]
#[inline(never)]
fn mismatch(ch_type: &ChType) -> BinaryValueError {
    BinaryValueError::Invalid(format!("column does not match type {ch_type}"))
}

#[cold]
#[inline(never)]
fn out_of_range(row: usize, len: usize) -> BinaryValueError {
    BinaryValueError::Invalid(format!("row {row} is out of range for {len} rows"))
}

#[cold]
#[inline(never)]
fn null_row() -> BinaryValueError {
    BinaryValueError::Invalid("null row under a non-nullable type".into())
}

#[cold]
#[inline(never)]
fn invalid_offsets(row: usize, start: i64, end: i64) -> BinaryValueError {
    BinaryValueError::Invalid(format!("row {row} has invalid offsets {start}..{end}"))
}

/// The null bit of `row` from an optional validity bitmap, bounds checked.
#[inline]
fn is_null(validity: Option<&Bitmap>, row: usize) -> Result<bool, BinaryValueError> {
    let Some(bitmap) = validity else {
        return Ok(false);
    };
    match bitmap.as_bytes().get(row / 8) {
        Some(byte) if row < bitmap.len() => Ok((byte >> (row % 8)) & 1 == 0),
        _ => Err(out_of_range(row, bitmap.len())),
    }
}

/// Reject a null row when the dispatched physical type is non-nullable. Under
/// a `Nullable` wrapper this rereads the bit the wrapper just saw set, which is
/// the cost of keeping the leaf arms free of a null-proof flag.
#[inline]
fn reject_null(validity: Option<&Bitmap>, row: usize) -> Result<(), BinaryValueError> {
    if is_null(validity, row)? {
        return Err(null_row());
    }
    Ok(())
}

/// `[start, end)` element range of `row` in an Arrow offsets buffer.
#[inline]
fn offset_range<T: Copy + Into<i64>>(
    offsets: &[T],
    row: usize,
) -> Result<(usize, usize), BinaryValueError> {
    let next = row.checked_add(1).and_then(|next| offsets.get(next));
    let (Some(&start), Some(&end)) = (offsets.get(row), next) else {
        return Err(out_of_range(row, offsets.len().saturating_sub(1)));
    };
    let (start, end) = (start.into(), end.into());
    match (usize::try_from(start), usize::try_from(end)) {
        (Ok(start), Ok(end)) if start <= end => Ok((start, end)),
        _ => Err(invalid_offsets(row, start, end)),
    }
}

#[cold]
#[inline(never)]
fn zero_width_count_exceeded(count: usize) -> BinaryValueError {
    BinaryValueError::Invalid(format!(
        "zero-width element count {count} exceeds the {MAX_ZERO_WIDTH_ELEMENTS}-element limit"
    ))
}

/// Bound element counts that no payload byte can bound, computing the
/// zero-width property only once a count is suspicious.
#[inline]
fn check_zero_width_count(
    count: usize,
    zero_width: impl FnOnce() -> bool,
) -> Result<(), BinaryValueError> {
    if count > MAX_ZERO_WIDTH_ELEMENTS && zero_width() {
        return Err(zero_width_count_exceeded(count));
    }
    Ok(())
}

/// Rows below the returned bound hold valid zero-width values (`Tuple()` or a
/// Tuple of only such types), which encode to no bytes, so the per-row loop can
/// start there. `None` when the type is not zero-width, or a column shape, tuple
/// validity bitmap, or LowCardinality dictionary needs per-row checks. Errors
/// always come from the per-row path.
fn zero_width_bound(ch_type: &ChType, column: &Column, end: usize) -> Option<usize> {
    count_visit();
    match ch_type {
        ChType::SimpleAggregateFunction { inner, .. } => zero_width_bound(inner, column, end),
        ChType::LowCardinality(_) if matches!(column, Column::Dictionary(_)) => None,
        ChType::LowCardinality(inner) => zero_width_bound(inner, column, end),
        ChType::Tuple(elements) => match column {
            Column::Tuple(c) => zero_width_tuple_bound(elements.iter().map(|(_, ty)| ty), c, end),
            _ => None,
        },
        _ => None,
    }
}

/// [`zero_width_bound`] for one `Tuple` column under `elements`, shared with
/// the `Nested` alias.
fn zero_width_tuple_bound<'a>(
    elements: impl ExactSizeIterator<Item = &'a ChType>,
    c: &TupleColumn,
    end: usize,
) -> Option<usize> {
    if elements.len() != c.fields.len() || c.validity.is_some() {
        return None;
    }
    let mut bound = c.len.min(end);
    for (element, field) in elements.zip(&c.fields) {
        bound = bound.min(zero_width_bound(element, field, end)?);
    }
    Some(bound)
}

/// First row of `start..end` the per-row loop must visit.
#[inline]
fn first_checked_row(start: usize, end: usize, bound: impl FnOnce() -> Option<usize>) -> usize {
    if start == end {
        return end;
    }
    bound().map_or(start, |bound| bound.max(start))
}

fn fixed_slice(data: &[u8], width: usize, row: usize) -> Result<&[u8], BinaryValueError> {
    row.checked_mul(width)
        .and_then(|start| data.get(start..start.checked_add(width)?))
        .ok_or_else(|| out_of_range(row, data.len().checked_div(width).unwrap_or(0)))
}

fn utf8_slice(column: &Utf8Column, row: usize) -> Result<&[u8], BinaryValueError> {
    reject_null(column.validity.as_ref(), row)?;
    let (start, end) = offset_range(&column.offsets, row)?;
    column
        .data
        .get(start..end)
        .ok_or_else(|| BinaryValueError::Invalid("invalid JSON string offsets".into()))
}

fn write_string(buf: &mut Vec<u8>, bytes: &[u8]) {
    write_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

fn write_shared_value(
    buf: &mut Vec<u8>,
    cell: &[u8],
    depth: usize,
) -> Result<(), BinaryValueError> {
    let mut reader = ByteReader::new(cell);
    let ty = read_value_type(&mut reader, depth)?;
    ty.skip(&mut reader)?;
    if reader.remaining() != 0 {
        return Err(BinaryValueError::Invalid(
            "trailing JSON shared value bytes".into(),
        ));
    }
    buf.extend_from_slice(cell);
    Ok(())
}

fn encode_json_value(
    buf: &mut Vec<u8>,
    typed_paths: &[(String, ChType)],
    column: &JsonColumn,
    row: usize,
    depth: usize,
) -> Result<(), BinaryValueError> {
    reject_null(column.validity.as_ref(), row)?;
    let JsonBody::Structured(body) = &column.body else {
        return Err(BinaryValueError::Unsupported("JSON text body".into()));
    };
    if row >= body.len {
        return Err(out_of_range(row, body.len));
    }
    let (start, end) = offset_range(&body.shared_offsets, row)?;
    if end > body.shared_paths.len() || end > body.shared_values.len() {
        return Err(BinaryValueError::Invalid(
            "invalid JSON shared offsets".into(),
        ));
    }
    let mut count = typed_paths
        .len()
        .checked_add(end - start)
        .ok_or_else(|| BinaryValueError::Invalid("JSON path count overflows usize".into()))?;
    for (_, dynamic) in &body.dynamic {
        let id = dynamic
            .type_ids
            .get(row)
            .ok_or_else(|| out_of_range(row, dynamic.len()))?;
        if *id != u32::MAX {
            count = count.checked_add(1).ok_or_else(|| {
                BinaryValueError::Invalid("JSON path count overflows usize".into())
            })?;
        }
    }
    let mut paths = Vec::with_capacity(count);
    paths.extend(body.typed.iter().map(|(path, _)| path.as_bytes()));
    for (path, dynamic) in &body.dynamic {
        if dynamic.type_ids[row] != u32::MAX {
            paths.push(path.as_bytes());
        }
    }
    for index in start..end {
        let path = utf8_slice(&body.shared_paths, index)?;
        paths.push(path);
    }
    paths.sort_unstable();
    if paths.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(BinaryValueError::Invalid("duplicate JSON path".into()));
    }
    write_varint(buf, count as u64);
    for (path, values) in &body.typed {
        let index = typed_paths
            .binary_search_by(|(name, _)| name.as_bytes().cmp(path.as_bytes()))
            .map_err(|_| BinaryValueError::Invalid("unexpected JSON typed path".into()))?;
        let ty = &typed_paths[index].1;
        write_string(buf, path.as_bytes());
        encode_value(buf, ty, values, row, depth + 1)?;
    }
    for (path, dynamic) in &body.dynamic {
        let id = *dynamic
            .type_ids
            .get(row)
            .ok_or_else(|| out_of_range(row, dynamic.len()))?;
        if id == u32::MAX {
            continue;
        }
        let offset = dynamic
            .offsets
            .get(row)
            .and_then(|offset| usize::try_from(*offset).ok())
            .ok_or_else(|| BinaryValueError::Invalid("invalid JSON dynamic offset".into()))?;
        let child = dynamic
            .children
            .get(id as usize)
            .ok_or_else(|| BinaryValueError::Invalid("invalid JSON dynamic type id".into()))?;
        write_string(buf, path.as_bytes());
        match child {
            DynamicChild::Typed { ch_type, values } => {
                let check = check_type(ch_type, Some(values), depth + 1).ok_or_else(too_deep)?;
                check.check_descriptor_complexity()?;
                if !check.matches {
                    return Err(mismatch(ch_type));
                }
                write_binary_type(buf, ch_type);
                encode_value(buf, ch_type, values, offset, depth + 1)?;
            }
            DynamicChild::Shared(values) => {
                write_shared_value(buf, utf8_slice(values, offset)?, depth + 1)?;
            }
        }
    }
    for index in start..end {
        let path = utf8_slice(&body.shared_paths, index)?;
        write_string(buf, path);
        write_shared_value(buf, utf8_slice(&body.shared_values, index)?, depth + 1)?;
    }
    Ok(())
}

/// One row of a `Tuple` column under `elements`: shared by `Tuple` and the
/// `Nested` alias, whose elements are an `Array` of this shape.
#[inline]
fn encode_tuple<'a>(
    buf: &mut Vec<u8>,
    ch_type: &ChType,
    elements: impl ExactSizeIterator<Item = &'a ChType>,
    c: &TupleColumn,
    row: usize,
    depth: usize,
) -> Result<(), BinaryValueError> {
    if elements.len() != c.fields.len() {
        return Err(mismatch(ch_type));
    }
    reject_null(c.validity.as_ref(), row)?;
    if row >= c.len {
        return Err(out_of_range(row, c.len));
    }
    for (element, field) in elements.zip(&c.fields) {
        encode_value(buf, element, field, row, depth + 1)?;
    }
    Ok(())
}

/// Every physical leaf rejects a null row from its own validity bitmap; the
/// `Nullable` and `LowCardinality(Nullable)` arms are the only writers of a
/// null flag byte.
fn encode_value(
    buf: &mut Vec<u8>,
    ch_type: &ChType,
    column: &Column,
    row: usize,
    depth: usize,
) -> Result<(), BinaryValueError> {
    count_visit();
    if depth > MAX_TYPE_DEPTH {
        return Err(too_deep());
    }
    macro_rules! le {
        ($c:expr) => {{
            reject_null($c.validity.as_ref(), row)?;
            let v = $c
                .values
                .get(row)
                .ok_or_else(|| out_of_range(row, $c.values.len()))?;
            buf.extend_from_slice(&v.to_le_bytes());
        }};
    }

    match (ch_type, column) {
        // Name-decoration aliases encode as their physical type. `Nested` has
        // its own arm below so its `Array(Tuple(..))` form is never built.
        (ChType::SimpleAggregateFunction { inner, .. }, _) => {
            encode_value(buf, inner, column, row, depth + 1)?
        }
        (ChType::Geo(kind), _) => {
            encode_value(buf, kind.underlying_type_ref(), column, row, depth)?
        }

        // Mirror the bulk null map: 0x00 then the value, or 0x01 and nothing.
        (ChType::Nullable(inner), _) => {
            if is_null(column.validity(), row)? {
                if row >= column.len() {
                    return Err(out_of_range(row, column.len()));
                }
                buf.push(1);
            } else {
                buf.push(0);
                encode_value(buf, inner, column, row, depth + 1)?;
            }
        }
        (ChType::LowCardinality(inner), Column::Dictionary(dict)) => {
            let index = *dict
                .indices
                .get(row)
                .ok_or_else(|| out_of_range(row, dict.indices.len()))?;
            let (nullable, value_type) =
                crate::native::type_parser::low_cardinality_dict_value_type(inner);
            if nullable {
                // LowCardinality(Nullable(T)) keeps nulls in the index validity;
                // its single value is the Nullable(T) form.
                if is_null(dict.validity.as_ref(), row)? {
                    buf.push(1);
                    return Ok(());
                }
                buf.push(0);
            } else {
                reject_null(dict.validity.as_ref(), row)?;
            }
            let index = usize::try_from(index).map_err(|_| {
                BinaryValueError::Invalid(format!("row {row} has negative dictionary index"))
            })?;
            encode_value(buf, value_type, &dict.values, index, depth + 1)?;
        }
        // The plain inner column, as decode_binary_value returns it.
        (ChType::LowCardinality(inner), _) => encode_value(buf, inner, column, row, depth + 1)?,

        (ChType::Nothing, Column::Nothing(c)) => {
            reject_null(c.validity.as_ref(), row)?;
            return Err(BinaryValueError::Invalid("non-null Nothing value".into()));
        }
        (ChType::Bool, Column::Bool(c)) => {
            reject_null(c.validity.as_ref(), row)?;
            if row >= c.len {
                return Err(out_of_range(row, c.len));
            }
            let byte = c
                .bitmap
                .get(row / 8)
                .ok_or_else(|| out_of_range(row, c.len))?;
            buf.push((byte >> (row % 8)) & 1);
        }
        (ChType::Int8, Column::Int8(c)) | (ChType::Enum8 { .. }, Column::Enum8(c)) => le!(c),
        (ChType::Int16, Column::Int16(c)) | (ChType::Enum16 { .. }, Column::Enum16(c)) => le!(c),
        (ChType::Int32, Column::Int32(c))
        | (ChType::Date32, Column::Date32(c))
        | (ChType::Time, Column::Time(c)) => le!(c),
        (ChType::Int64, Column::Int64(c))
        | (ChType::DateTime64 { .. }, Column::DateTime64(c))
        | (ChType::Time64 { .. }, Column::Time64(c))
        | (ChType::Interval(_), Column::Interval(c)) => le!(c),
        (ChType::UInt8, Column::UInt8(c)) => le!(c),
        (ChType::UInt16, Column::UInt16(c)) | (ChType::Date, Column::Date(c)) => le!(c),
        (ChType::UInt32, Column::UInt32(c))
        | (ChType::DateTime { .. }, Column::DateTime(c))
        | (ChType::Ipv4, Column::Ipv4(c)) => le!(c),
        (ChType::UInt64, Column::UInt64(c)) => le!(c),
        (ChType::Float32, Column::Float32(c)) => le!(c),
        (ChType::Float64, Column::Float64(c)) => le!(c),
        (ChType::BFloat16, Column::BFloat16(c)) => {
            reject_null(c.validity.as_ref(), row)?;
            let v = c
                .values
                .get(row)
                .ok_or_else(|| out_of_range(row, c.values.len()))?;
            buf.extend_from_slice(v);
        }
        (
            ChType::QBit {
                element_type,
                dimension,
            },
            Column::QBit(c),
        ) => {
            reject_null(c.validity.as_ref(), row)?;
            if c.dimension != *dimension {
                return Err(mismatch(ch_type));
            }
            let first = row
                .checked_mul(*dimension)
                .ok_or_else(|| out_of_range(row, c.len()))?;
            let end = first
                .checked_add(*dimension)
                .ok_or_else(|| out_of_range(row, c.len()))?;
            if c.values.validity().is_some() {
                return Err(BinaryValueError::Invalid(
                    "QBit elements must not carry a validity bitmap".into(),
                ));
            }
            write_varint(buf, *dimension as u64);
            macro_rules! elements {
                ($values:expr) => {
                    $values
                        .get(first..end)
                        .ok_or_else(|| out_of_range(row, c.len()))?
                };
            }
            match (element_type, c.values.as_ref()) {
                (QBitElementType::BFloat16, Column::BFloat16(values)) => {
                    for value in elements!(values.values) {
                        buf.extend_from_slice(value);
                    }
                }
                (QBitElementType::Float32, Column::Float32(values)) => {
                    for value in elements!(values.values) {
                        buf.extend_from_slice(&value.to_le_bytes());
                    }
                }
                (QBitElementType::Float64, Column::Float64(values)) => {
                    for value in elements!(values.values) {
                        buf.extend_from_slice(&value.to_le_bytes());
                    }
                }
                _ => return Err(mismatch(ch_type)),
            }
        }
        (ChType::String, Column::Utf8(c)) => {
            reject_null(c.validity.as_ref(), row)?;
            let (start, end) = offset_range(&c.offsets, row)?;
            let bytes = c.data.get(start..end).ok_or_else(|| {
                BinaryValueError::Invalid(format!("row {row} has invalid String offsets"))
            })?;
            write_varint(buf, bytes.len() as u64);
            buf.extend_from_slice(bytes);
        }
        (ChType::FixedString(width), Column::FixedBinary(c)) if c.width == *width => {
            reject_null(c.validity.as_ref(), row)?;
            buf.extend_from_slice(fixed_slice(&c.data, c.width, row)?);
        }
        (ChType::Uuid, Column::Uuid(c))
        | (ChType::Ipv6, Column::Ipv6(c))
        | (ChType::Int128, Column::Int128(c))
        | (ChType::UInt128, Column::UInt128(c))
            if c.width == 16 =>
        {
            reject_null(c.validity.as_ref(), row)?;
            buf.extend_from_slice(fixed_slice(&c.data, 16, row)?);
        }
        (ChType::Int256, Column::Int256(c)) | (ChType::UInt256, Column::UInt256(c))
            if c.width == 32 =>
        {
            reject_null(c.validity.as_ref(), row)?;
            buf.extend_from_slice(fixed_slice(&c.data, 32, row)?);
        }
        (
            ChType::Decimal {
                bits,
                precision,
                scale,
            },
            Column::Decimal(c),
        ) if matches!(bits, 32 | 64 | 128 | 256)
            && c.width == *bits as usize / 8
            && c.precision == *precision
            && c.scale == *scale =>
        {
            reject_null(c.validity.as_ref(), row)?;
            buf.extend_from_slice(fixed_slice(&c.data, c.width, row)?);
        }
        (ChType::Array(inner), Column::Array(c)) => {
            let (start, end) = offset_range(&c.offsets, row)?;
            check_zero_width_count(end - start, || needs_element_count_cap(inner, true))?;
            write_varint(buf, (end - start) as u64);
            let from = first_checked_row(start, end, || zero_width_bound(inner, &c.values, end));
            for i in from..end {
                encode_value(buf, inner, &c.values, i, depth + 1)?;
            }
        }
        (ChType::Nested(fields), Column::Array(c)) => {
            let Column::Tuple(tuple) = c.values.as_ref() else {
                return Err(mismatch(ch_type));
            };
            let (start, end) = offset_range(&c.offsets, row)?;
            check_zero_width_count(end - start, || {
                fields
                    .iter()
                    .all(|(_, ty)| needs_element_count_cap(ty, true))
            })?;
            write_varint(buf, (end - start) as u64);
            let elements = || fields.iter().map(|(_, ty)| ty);
            let from = first_checked_row(start, end, || {
                zero_width_tuple_bound(elements(), tuple, end)
            });
            for i in from..end {
                encode_tuple(buf, ch_type, elements(), tuple, i, depth + 1)?;
            }
        }
        (ChType::Map(key, value), Column::Map(c)) => {
            let Column::Tuple(entries) = c.entries.as_ref() else {
                return Err(mismatch(ch_type));
            };
            let [keys, values] = entries.fields.as_slice() else {
                return Err(mismatch(ch_type));
            };
            let (start, end) = offset_range(&c.offsets, row)?;
            check_zero_width_count(end - start, || {
                needs_element_count_cap(key, true) && needs_element_count_cap(value, true)
            })?;
            write_varint(buf, (end - start) as u64);
            let from = first_checked_row(start, end, || {
                let key = zero_width_bound(key, keys, end)?;
                Some(key.min(zero_width_bound(value, values, end)?))
            });
            for i in from..end {
                encode_value(buf, key, keys, i, depth + 1)?;
                encode_value(buf, value, values, i, depth + 1)?;
            }
        }
        (ChType::Tuple(elements), Column::Tuple(c)) => encode_tuple(
            buf,
            ch_type,
            elements.iter().map(|(_, ty)| ty),
            c,
            row,
            depth,
        )?,
        (ChType::Json { typed_paths, .. }, Column::Json(c)) => {
            encode_json_value(buf, typed_paths, c, row, depth)?;
        }
        (
            ChType::Variant(_)
            | ChType::Dynamic { .. }
            | ChType::AggregateFunction { .. }
            | ChType::Geometry,
            _,
        ) => return Err(unsupported(ch_type)),
        _ => return Err(mismatch(ch_type)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::GeoKind;

    fn parse(name: &str) -> ChType {
        crate::native::type_parser::parse_ch_type(name).expect("test type parses")
    }

    fn varint(value: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        write_varint(&mut buf, value);
        buf
    }

    #[test]
    fn decodes_fixed_width_scalars() {
        let column = decode_binary_value(&ChType::Int32, &(-79i32).to_le_bytes()).unwrap();
        let Column::Int32(c) = column else {
            panic!("expected Int32")
        };
        assert_eq!(c.values, vec![-79]);
        assert!(c.validity.is_none());

        let Column::UInt64(c) = decode_binary_value(&ChType::UInt64, &13u64.to_le_bytes()).unwrap()
        else {
            panic!("expected UInt64")
        };
        assert_eq!(c.values, vec![13]);

        let Column::Float64(c) =
            decode_binary_value(&ChType::Float64, &1.5f64.to_le_bytes()).unwrap()
        else {
            panic!("expected Float64")
        };
        assert_eq!(c.values, vec![1.5]);

        let Column::BFloat16(c) = decode_binary_value(&ChType::BFloat16, &[0x80, 0x3f]).unwrap()
        else {
            panic!("expected BFloat16")
        };
        assert_eq!(c.values, vec![[0x80, 0x3f]]);

        let Column::Bool(c) = decode_binary_value(&ChType::Bool, &[0x01]).unwrap() else {
            panic!("expected Bool")
        };
        assert!(c.get(0));
    }

    #[test]
    fn decodes_qbit_single_value_row_binary_layout() {
        // Single-value QBit is intentionally different from Native bulk: a
        // VarUInt dimension followed by ordinary little-endian scalar values.
        let mut bytes = varint(3);
        bytes.extend_from_slice(&1.5f32.to_le_bytes());
        bytes.extend_from_slice(&(-2.5f32).to_le_bytes());
        bytes.extend_from_slice(&13f32.to_le_bytes());
        let Column::QBit(qbit) = decode_binary_value(&parse("QBit(Float32, 3)"), &bytes).unwrap()
        else {
            panic!("expected QBit")
        };
        assert_eq!(qbit.dimension, 3);
        let Column::Float32(values) = qbit.values.as_ref() else {
            panic!("expected Float32 child")
        };
        assert_eq!(values.values, vec![1.5, -2.5, 13.0]);

        let mut wrong_dimension = varint(2);
        wrong_dimension.extend_from_slice(&[0; 12]);
        assert!(matches!(
            decode_binary_value(&parse("QBit(Float32, 3)"), &wrong_dimension),
            Err(BinaryValueError::Invalid(_))
        ));
    }

    #[test]
    fn nullable_qbit_null_defaults_are_cumulatively_bounded() {
        let Column::QBit(qbit) =
            decode_binary_value(&parse("Nullable(QBit(Float64, 3))"), &[1]).unwrap()
        else {
            panic!("expected QBit")
        };
        assert_eq!(qbit.null_count(), 1);
        let Column::Float64(values) = qbit.values.as_ref() else {
            panic!("expected Float64 child")
        };
        assert_eq!(values.values, vec![0.0; 3]);

        let over_limit = ChType::Nullable(Box::new(ChType::QBit {
            element_type: QBitElementType::Float64,
            dimension: MAX_NULL_DEFAULT_BYTES / std::mem::size_of::<f64>() + 1,
        }));
        assert!(matches!(
            decode_binary_value(&over_limit, &[1]),
            Err(BinaryValueError::Invalid(ref reason))
                if reason.contains("null placeholder expansion")
        ));

        // Two null QBits each require the entire budget. The first is legal,
        // but the second must fail against the same per-call budget rather than
        // multiplying one input flag into another 16 MiB allocation.
        let cumulative = ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::QBit {
            element_type: QBitElementType::Float64,
            dimension: MAX_NULL_DEFAULT_BYTES / std::mem::size_of::<f64>(),
        }))));
        assert!(matches!(
            decode_binary_value(&cumulative, &[2, 1, 1]),
            Err(BinaryValueError::Invalid(ref reason))
                if reason.contains("null placeholder expansion")
        ));
    }

    #[test]
    fn decodes_temporals_as_raw_units() {
        let Column::Date(c) = decode_binary_value(&ChType::Date, &19724u16.to_le_bytes()).unwrap()
        else {
            panic!("expected Date")
        };
        assert_eq!(c.values, vec![19724]);

        let Column::Date32(c) =
            decode_binary_value(&ChType::Date32, &(-100i32).to_le_bytes()).unwrap()
        else {
            panic!("expected Date32")
        };
        assert_eq!(c.values, vec![-100]);

        let dt = ChType::DateTime {
            timezone: Some("UTC".into()),
        };
        let Column::DateTime(c) = decode_binary_value(&dt, &1_000u32.to_le_bytes()).unwrap() else {
            panic!("expected DateTime")
        };
        assert_eq!(c.values, vec![1_000]);

        let dt64 = ChType::DateTime64 {
            precision: 3,
            timezone: None,
        };
        let Column::DateTime64(c) = decode_binary_value(&dt64, &(-5i64).to_le_bytes()).unwrap()
        else {
            panic!("expected DateTime64")
        };
        assert_eq!(c.values, vec![-5]);

        let Column::Time(c) = decode_binary_value(&ChType::Time, &(-7i32).to_le_bytes()).unwrap()
        else {
            panic!("expected Time")
        };
        assert_eq!(c.values, vec![-7]);

        let t64 = ChType::Time64 { precision: 6 };
        let Column::Time64(c) = decode_binary_value(&t64, &79i64.to_le_bytes()).unwrap() else {
            panic!("expected Time64")
        };
        assert_eq!(c.values, vec![79]);
    }

    #[test]
    fn decodes_strings_and_fixed_strings() {
        let mut bytes = varint(5);
        bytes.extend_from_slice(b"hello");
        let Column::Utf8(c) = decode_binary_value(&ChType::String, &bytes).unwrap() else {
            panic!("expected Utf8")
        };
        assert_eq!(c.value(0), b"hello");

        // FixedString: exactly N raw bytes, padding included, no prefix.
        let Column::FixedBinary(c) =
            decode_binary_value(&ChType::FixedString(4), b"ab\x00\x00").unwrap()
        else {
            panic!("expected FixedBinary")
        };
        assert_eq!(c.value(0), b"ab\x00\x00");
    }

    #[test]
    fn decodes_uuid_ip_enum_decimal_and_wide_ints() {
        // UUID: verbatim bulk layout passthrough.
        let wire: [u8; 16] = *b"\x01\x02\x03\x04\x05\x06\x07\x08ABCDEFGH";
        let Column::Uuid(c) = decode_binary_value(&ChType::Uuid, &wire).unwrap() else {
            panic!("expected Uuid")
        };
        assert_eq!(c.value(0), wire);

        let Column::Ipv4(c) =
            decode_binary_value(&ChType::Ipv4, &0x7f000001u32.to_le_bytes()).unwrap()
        else {
            panic!("expected Ipv4")
        };
        assert_eq!(c.values, vec![0x7f000001]);

        let v6 = [0u8; 16];
        let Column::Ipv6(c) = decode_binary_value(&ChType::Ipv6, &v6).unwrap() else {
            panic!("expected Ipv6")
        };
        assert_eq!(c.value(0), v6);

        let Column::Enum8(c) = decode_binary_value(&parse("Enum8('a' = -1)"), &[0xff]).unwrap()
        else {
            panic!("expected Enum8")
        };
        assert_eq!(c.values, vec![-1]);

        let Column::Enum16(c) =
            decode_binary_value(&parse("Enum16('a' = -2)"), &(-2i16).to_le_bytes()).unwrap()
        else {
            panic!("expected Enum16")
        };
        assert_eq!(c.values, vec![-2]);

        let Column::Decimal(c) =
            decode_binary_value(&parse("Decimal(9, 2)"), &(-1234i32).to_le_bytes()).unwrap()
        else {
            panic!("expected Decimal")
        };
        assert_eq!(c.value(0), (-1234i32).to_le_bytes());
        assert_eq!((c.precision, c.scale, c.width), (9, 2, 4));

        let Column::Decimal(c) =
            decode_binary_value(&parse("Decimal(50, 3)"), &[0x13; 32]).unwrap()
        else {
            panic!("expected Decimal")
        };
        assert_eq!(c.width, 32);

        let Column::Int128(c) = decode_binary_value(&ChType::Int128, &[0xff; 16]).unwrap() else {
            panic!("expected Int128")
        };
        assert_eq!(c.value(0), [0xff; 16]);

        let Column::UInt256(c) = decode_binary_value(&ChType::UInt256, &[0x01; 32]).unwrap() else {
            panic!("expected UInt256")
        };
        assert_eq!(c.value(0), [0x01; 32]);
    }

    #[test]
    fn decodes_array_with_nested_nullable() {
        // Array(Nullable(Int32)) value [1, NULL, 3].
        let mut bytes = varint(3);
        bytes.push(0);
        bytes.extend_from_slice(&1i32.to_le_bytes());
        bytes.push(1);
        bytes.push(0);
        bytes.extend_from_slice(&3i32.to_le_bytes());
        let Column::Array(c) =
            decode_binary_value(&parse("Array(Nullable(Int32))"), &bytes).unwrap()
        else {
            panic!("expected Array")
        };
        assert_eq!(c.offsets, vec![0, 3]);
        let Column::Int32(elems) = c.values.as_ref() else {
            panic!("expected Int32 elements")
        };
        assert_eq!(elems.values, vec![1, 0, 3]);
        let bm = elems.validity.as_ref().expect("nullable elements");
        assert!(bm.is_valid(0) && !bm.is_valid(1) && bm.is_valid(2));
    }

    #[test]
    fn decodes_low_cardinality_without_framing() {
        // LowCardinality(String): just the String value.
        let mut bytes = varint(6);
        bytes.extend_from_slice(b"user_1");
        let Column::Utf8(c) =
            decode_binary_value(&parse("LowCardinality(String)"), &bytes).unwrap()
        else {
            panic!("expected Utf8")
        };
        assert_eq!(c.value(0), b"user_1");

        // Nested LowCardinality(Nullable(String)) inside an Array keeps the
        // per-value null flag.
        let mut bytes = varint(2);
        bytes.push(1);
        bytes.push(0);
        bytes.extend(varint(1));
        bytes.push(b'x');
        let Column::Array(c) =
            decode_binary_value(&parse("Array(LowCardinality(Nullable(String)))"), &bytes).unwrap()
        else {
            panic!("expected Array")
        };
        let Column::Utf8(elems) = c.values.as_ref() else {
            panic!("expected Utf8 elements")
        };
        assert_eq!(elems.value(1), b"x");
        let bm = elems.validity.as_ref().expect("nullable elements");
        assert!(!bm.is_valid(0) && bm.is_valid(1));
    }

    #[test]
    fn decodes_map_as_pairs() {
        // Map(String, UInt64) value {"a": 1, "b": 2}.
        let mut bytes = varint(2);
        bytes.extend(varint(1));
        bytes.push(b'a');
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend(varint(1));
        bytes.push(b'b');
        bytes.extend_from_slice(&2u64.to_le_bytes());
        let Column::Map(c) = decode_binary_value(&parse("Map(String, UInt64)"), &bytes).unwrap()
        else {
            panic!("expected Map")
        };
        assert_eq!(c.offsets, vec![0, 2]);
        let Column::Tuple(entries) = c.entries.as_ref() else {
            panic!("expected Tuple entries")
        };
        let (Column::Utf8(keys), Column::UInt64(values)) = (&entries.fields[0], &entries.fields[1])
        else {
            panic!("expected String keys, UInt64 values")
        };
        assert_eq!((keys.value(0), keys.value(1)), (&b"a"[..], &b"b"[..]));
        assert_eq!(values.values, vec![1, 2]);
    }

    #[test]
    fn decodes_tuples_including_empty() {
        // Tuple(Int32, String): elements back to back, no count.
        let mut bytes = 13i32.to_le_bytes().to_vec();
        bytes.extend(varint(1));
        bytes.push(b'x');
        let Column::Tuple(c) = decode_binary_value(&parse("Tuple(Int32, String)"), &bytes).unwrap()
        else {
            panic!("expected Tuple")
        };
        assert_eq!(c.len, 1);
        let Column::Int32(first) = &c.fields[0] else {
            panic!("expected Int32 field")
        };
        assert_eq!(first.values, vec![13]);

        // Tuple() single value: zero bytes (unlike the bulk one byte per row).
        let Column::Tuple(c) = decode_binary_value(&parse("Tuple()"), &[]).unwrap() else {
            panic!("expected Tuple")
        };
        assert_eq!((c.len, c.fields.len()), (1, 0));
    }

    #[test]
    fn decodes_nullable_inside_tuple() {
        // Tuple(Nullable(String)) with a NULL element: flag only, no value.
        let Column::Tuple(c) =
            decode_binary_value(&parse("Tuple(Nullable(String))"), &[0x01]).unwrap()
        else {
            panic!("expected Tuple")
        };
        let Column::Utf8(field) = &c.fields[0] else {
            panic!("expected Utf8 field")
        };
        assert_eq!(field.value(0), b"");
        assert!(!field.validity.as_ref().expect("nullable field").is_valid(0));
    }

    #[test]
    fn decodes_geo_via_physical_delegate() {
        // Point = Tuple(Float64, Float64): two raw f64.
        let mut bytes = 1.5f64.to_le_bytes().to_vec();
        bytes.extend_from_slice(&(-2.5f64).to_le_bytes());
        let Column::Tuple(c) = decode_binary_value(&ChType::Geo(GeoKind::Point), &bytes).unwrap()
        else {
            panic!("expected Tuple")
        };
        let (Column::Float64(x), Column::Float64(y)) = (&c.fields[0], &c.fields[1]) else {
            panic!("expected Float64 fields")
        };
        assert_eq!((x.values[0], y.values[0]), (1.5, -2.5));
    }

    #[test]
    fn decodes_simple_aggregate_function_as_inner() {
        let Column::UInt64(c) = decode_binary_value(
            &parse("SimpleAggregateFunction(sum, UInt64)"),
            &79u64.to_le_bytes(),
        )
        .unwrap() else {
            panic!("expected UInt64")
        };
        assert_eq!(c.values, vec![79]);
    }

    #[test]
    fn json_typed_paths_use_names_and_default_missing_values() {
        let ty = parse("Array(JSON(a Int64, b Nullable(String)))");
        let mut bytes = vec![2, 2];
        write_string(&mut bytes, b"b");
        bytes.push(0);
        write_string(&mut bytes, b"user_1");
        write_string(&mut bytes, b"a");
        bytes.extend_from_slice(&13i64.to_le_bytes());
        bytes.push(0);
        let column = decode_binary_value(&ty, &bytes).unwrap();
        let Column::Array(array) = &column else {
            panic!("expected Array")
        };
        let Column::Json(json) = array.values.as_ref() else {
            panic!("expected JSON")
        };
        let JsonBody::Structured(body) = &json.body else {
            panic!("expected structured")
        };
        let Column::Int64(a) = &body.typed[0].1 else {
            panic!("expected Int64")
        };
        assert_eq!(a.values, [13, 0]);
        let Column::Utf8(b) = &body.typed[1].1 else {
            panic!("expected String")
        };
        assert_eq!(b.value(0), b"user_1");
        assert_eq!(b.value(1), b"");
        assert_eq!(b.validity.as_ref().unwrap().null_count(), 1);
        let mut encoded = Vec::new();
        encode_binary_value(&mut encoded, &ty, &column, 0).unwrap();
        assert_eq!(decode_binary_value(&ty, &encoded).unwrap(), column);
    }

    #[test]
    fn json_nullable_defaults_share_the_materialization_budget() {
        let ty = parse("Nullable(JSON(a FixedString(16777215), b FixedString(16777215)))");
        assert!(matches!(
            decode_binary_value(&ty, &[1]),
            Err(BinaryValueError::Invalid(reason)) if reason.contains("placeholder expansion")
        ));
        let ty = parse("Nullable(JSON(a Nullable(Int64)))");
        let column = decode_binary_value(&ty, &[1]).unwrap();
        assert_eq!(column.null_count(), 1);
        let mut encoded = Vec::new();
        encode_binary_value(&mut encoded, &ty, &column, 0).unwrap();
        assert_eq!(encoded, [1]);
        assert!(matches!(
            decode_binary_value(&parse("JSON(v Variant(Int64, String))"), &[0]),
            Err(BinaryValueError::Unsupported(_))
        ));
    }

    #[test]
    fn json_missing_zero_width_defaults_charge_the_budget() {
        for name in [
            "JSON(a Tuple(), b Tuple())",
            "JSON(a Tuple(Tuple(), Tuple()))",
            "JSON(a Tuple(Nothing, Nothing))",
        ] {
            let mut builder = ValueBuilder::new(&parse(name), 0).unwrap();
            let mut reader = ByteReader::new(&[0]);
            let mut budget = NullDefaultBudget { remaining: 1 };
            assert!(
                matches!(
                    builder.append_value(&mut reader, &mut budget),
                    Err(BinaryValueError::Invalid(reason)) if reason.contains("placeholder expansion")
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn json_wide_missing_defaults_stop_before_exhausting_the_input() {
        for (count, value_type) in [
            (999, ChType::Tuple(Vec::new())),
            (1000, ChType::Tuple(vec![(None, ChType::Tuple(Vec::new()))])),
        ] {
            let json = ChType::Json {
                max_dynamic_paths: 0,
                max_dynamic_types: 0,
                typed_paths: (0..count)
                    .map(|index| (format!("p{index:04}"), value_type.clone()))
                    .collect(),
                skip_paths: Vec::new(),
                skip_regexps: Vec::new(),
            };
            for (inner, row) in [(json.clone(), 0), (ChType::Nullable(Box::new(json)), 1)] {
                let ty = ChType::Array(Box::new(inner));
                let mut bytes = varint(1_000_000);
                bytes.resize(bytes.len() + 1_000_000, row);
                assert!(matches!(
                    decode_binary_value(&ty, &bytes),
                    Err(BinaryValueError::Invalid(reason)) if reason.contains("placeholder expansion")
                ));
            }
        }
    }

    #[test]
    fn json_shared_cells_reject_non_null_nothing_values() {
        for (name, payload) in [
            ("Array(Nothing)", vec![1]),
            ("Array(Nothing)", varint(1_000_000)),
            ("Array(Nothing)", varint(u64::MAX)),
            ("Nullable(Nothing)", vec![0]),
            ("Tuple(Nothing)", vec![]),
            ("Map(Nothing, UInt8)", vec![1, 79]),
            ("Map(String, Nothing)", vec![1, 1, b'k']),
        ] {
            let mut cell = Vec::new();
            write_binary_type(&mut cell, &parse(name));
            cell.extend(payload);
            let mut bytes = vec![1];
            write_string(&mut bytes, b"x");
            bytes.extend_from_slice(&cell);
            assert!(
                matches!(
                    decode_binary_value(&parse("JSON"), &bytes),
                    Err(BinaryValueError::Invalid(_))
                ),
                "{name}"
            );
            let column = Column::Json(JsonColumn::structured(StructuredJson::from_parts(
                Vec::new(),
                Vec::new(),
                vec![0, 1],
                Utf8Column::new(vec![0, 1], vec![b'x']),
                Utf8Column::new(vec![0, cell.len() as i32], cell),
                1,
            )));
            let mut encoded = vec![0xab];
            assert!(
                matches!(
                    encode_binary_value(&mut encoded, &parse("JSON"), &column, 0),
                    Err(BinaryValueError::Invalid(_))
                ),
                "{name}"
            );
            assert_eq!(encoded, [0xab]);
        }
    }

    #[test]
    fn json_shared_cells_allow_nothing_only_when_null_or_empty() {
        for (name, payload) in [
            ("Nothing", vec![]),
            ("Array(Nothing)", vec![0]),
            ("Map(Nothing, UInt8)", vec![0]),
            ("Map(String, Nothing)", vec![0]),
            ("Nullable(Nothing)", vec![1]),
            ("Array(Nullable(Nothing))", vec![3, 1, 1, 1]),
            ("Array(Array(Nothing))", vec![2, 0, 0]),
            ("Array(Dynamic)", vec![3, 0, 0, 0]),
            ("Variant(String, UInt8)", vec![255]),
        ] {
            let mut bytes = vec![1];
            write_string(&mut bytes, b"x");
            write_binary_type(&mut bytes, &parse(name));
            bytes.extend(payload);
            let column = decode_binary_value(&parse("JSON"), &bytes)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let mut encoded = Vec::new();
            encode_binary_value(&mut encoded, &parse("JSON"), &column, 0).unwrap();
            assert_eq!(encoded, bytes, "{name}");
        }
    }

    #[test]
    fn json_rejects_truncation_trailing_bytes_and_duplicate_typed_paths() {
        let ty = parse("JSON(a Int64)");
        let mut bytes = vec![2];
        write_string(&mut bytes, b"a");
        bytes.extend_from_slice(&13i64.to_le_bytes());
        write_string(&mut bytes, b"x");
        write_binary_type(&mut bytes, &parse("Array(Nullable(String))"));
        bytes.extend_from_slice(&[2, 1, 0]);
        write_string(&mut bytes, b"user_1");
        assert!(decode_binary_value(&ty, &bytes).is_ok());
        for end in 0..bytes.len() {
            assert!(
                matches!(
                    decode_binary_value(&ty, &bytes[..end]),
                    Err(BinaryValueError::Invalid(_))
                ),
                "{end}"
            );
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(matches!(
            decode_binary_value(&ty, &trailing),
            Err(BinaryValueError::Invalid(_))
        ));
        let mut duplicate = vec![2];
        for _ in 0..2 {
            write_string(&mut duplicate, b"a");
            duplicate.extend_from_slice(&13i64.to_le_bytes());
        }
        assert!(matches!(
            decode_binary_value(&ty, &duplicate),
            Err(BinaryValueError::Invalid(_))
        ));
        assert!(matches!(
            decode_binary_value(&parse("JSON"), &[1, 1, 255]),
            Err(BinaryValueError::Invalid(_))
        ));
    }

    #[test]
    fn json_embedded_descriptors_share_the_value_depth_limit() {
        let ty = parse("JSON");
        let descriptor = {
            let mut bytes = Vec::new();
            write_binary_type(&mut bytes, &ty);
            bytes
        };
        for (depth, valid) in [(5, true), (MAX_TYPE_DEPTH + 1, false)] {
            let mut bytes = Vec::new();
            for _ in 0..depth {
                bytes.extend_from_slice(&[1, 1, b'x']);
                bytes.extend_from_slice(&descriptor);
            }
            bytes.push(0);
            let decoded = decode_binary_value(&ty, &bytes);
            assert_eq!(decoded.is_ok(), valid);
            if let Ok(column) = decoded {
                let mut encoded = Vec::new();
                encode_binary_value(&mut encoded, &ty, &column, 0).unwrap();
                assert_eq!(encoded, bytes);
            }
        }
    }

    #[test]
    fn json_rejects_colliding_writer_paths_and_repeated_shared_reader_paths() {
        let ty = parse("JSON(a Int64)");
        for (dynamic_names, shared_names) in [
            (&["a"][..], &[][..]),
            (&[][..], &["a"][..]),
            (&["x"][..], &["x"][..]),
            (&["x", "x"][..], &[][..]),
            (&[][..], &["x", "x"][..]),
        ] {
            let mut paths = Utf8Column::new(vec![0], Vec::new());
            let mut values = Utf8Column::new(vec![0], Vec::new());
            let mut cell = Vec::new();
            write_binary_type(&mut cell, &ChType::UInt8);
            cell.push(79);
            for path in shared_names {
                append_utf8(&mut paths, path.as_bytes()).unwrap();
                append_utf8(&mut values, &cell).unwrap();
            }
            let dynamic = dynamic_names
                .iter()
                .map(|path| {
                    (
                        (*path).to_string(),
                        crate::column::DynamicColumn::try_new(
                            &[0],
                            vec![DynamicChild::Typed {
                                ch_type: ChType::UInt8,
                                values: Column::UInt8(PrimitiveColumn::new(vec![79])),
                            }],
                        )
                        .unwrap(),
                    )
                })
                .collect();
            let column = Column::Json(JsonColumn::structured(StructuredJson::from_parts(
                vec![("a".into(), Column::Int64(PrimitiveColumn::new(vec![13])))],
                dynamic,
                vec![0, shared_names.len() as i64],
                paths,
                values,
                1,
            )));
            let mut encoded = vec![0xab];
            assert!(matches!(encode_binary_value(&mut encoded, &ty, &column, 0),
                Err(BinaryValueError::Invalid(reason)) if reason.contains("duplicate JSON path")));
            assert_eq!(encoded, [0xab]);
        }
        let mut bytes = vec![2];
        for _ in 0..2 {
            write_string(&mut bytes, b"x");
            write_binary_type(&mut bytes, &ChType::UInt8);
            bytes.push(13);
        }
        assert!(matches!(decode_binary_value(&parse("JSON"), &bytes),
            Err(BinaryValueError::Invalid(reason)) if reason.contains("duplicate JSON path")));
    }

    #[test]
    fn geo_and_json_geo_values_roundtrip_at_the_type_depth_limit() {
        for leaf in [
            "Point",
            "JSON(a Point)",
            "SimpleAggregateFunction(any, Point)",
            "Nested(a Point)",
        ] {
            let mut ty = parse(leaf);
            let leaf_depth = match leaf {
                "Point" => 1,
                "Nested(a Point)" => 3,
                _ => 2,
            };
            let mut bytes = vec![1; MAX_TYPE_DEPTH - leaf_depth];
            for _ in leaf_depth..MAX_TYPE_DEPTH {
                ty = ChType::Array(Box::new(ty));
            }
            if leaf.starts_with("JSON") {
                bytes.push(1);
                write_string(&mut bytes, b"a");
            } else if leaf.starts_with("Nested") {
                bytes.push(1);
            }
            bytes.extend_from_slice(&1.25f64.to_le_bytes());
            bytes.extend_from_slice(&(-3.5f64).to_le_bytes());
            let column = decode_binary_value(&ty, &bytes).unwrap();
            let mut encoded = Vec::new();
            encode_binary_value(&mut encoded, &ty, &column, 0).unwrap();
            assert_eq!(encoded, bytes, "{leaf}");
        }
    }

    #[test]
    fn json_qbit_path_roundtrips_at_the_type_depth_limit() {
        let mut ty = parse("JSON");
        let mut bytes = vec![1; MAX_TYPE_DEPTH - 1];
        for _ in 1..MAX_TYPE_DEPTH {
            ty = ChType::Array(Box::new(ty));
        }
        bytes.push(1);
        write_string(&mut bytes, b"x");
        write_binary_type(&mut bytes, &parse("QBit(Float64, 1)"));
        bytes.push(1);
        bytes.extend_from_slice(&1.25f64.to_le_bytes());
        let column = decode_binary_value(&ty, &bytes).unwrap();
        let mut encoded = Vec::new();
        encode_binary_value(&mut encoded, &ty, &column, 0).unwrap();
        assert_eq!(encoded, bytes);
    }

    #[test]
    fn descriptor_node_counts_follow_binary_writer() {
        for (name, nodes) in [
            ("UInt8", 1),
            ("Array(Nullable(UInt8))", 3),
            ("Tuple(UInt8, Array(UInt8))", 4),
            ("Map(String, UInt8)", 3),
            ("Nested(x UInt8, y Array(UInt8))", 4),
            ("Point", 1),
            ("SimpleAggregateFunction(any, Array(UInt8))", 1),
            ("Array(SimpleAggregateFunction(any, Array(UInt8)))", 2),
            ("QBit(Float64, 1)", 2),
            ("Tuple(QBit(Float64, 1), UInt8)", 4),
            ("JSON(x Nullable(UInt8))", 3),
            ("Variant(String, UInt8)", 3),
        ] {
            let ty = parse(name);
            assert_eq!(check_type(&ty, None, 0).unwrap().descriptor_nodes, nodes);
            let tuple = ChType::Tuple(vec![(None, ty); MAX_BINARY_TYPE_COMPLEXITY / nodes]);
            let check = check_type(&tuple, None, 0).unwrap();
            let mut bytes = Vec::new();
            write_binary_type(&mut bytes, &tuple);
            assert_eq!(
                read_binary_type_prefix(&bytes).is_ok(),
                check.check_descriptor_complexity().is_ok(),
                "{name}"
            );
        }
    }

    #[test]
    fn json_dynamic_child_descriptor_complexity_matches_decode() {
        for count in [2000, 2001] {
            let child_type = ChType::Tuple(vec![(None, ChType::UInt8); count]);
            let child_values = Column::Tuple(TupleColumn::new(
                vec![Column::UInt8(PrimitiveColumn::new(vec![79])); count],
                1,
            ));
            let dynamic = crate::column::DynamicColumn::try_new(
                &[0],
                vec![DynamicChild::Typed {
                    ch_type: child_type.clone(),
                    values: child_values,
                }],
            )
            .unwrap();
            let column = Column::Json(JsonColumn::structured(StructuredJson::from_parts(
                vec![],
                vec![("x".into(), dynamic)],
                vec![0, 0],
                Utf8Column::new(vec![0], vec![]),
                Utf8Column::new(vec![0], vec![]),
                1,
            )));
            let ty = parse("JSON");
            let mut encoded = vec![0xab];
            let result = encode_binary_value(&mut encoded, &ty, &column, 0);
            if count == 2000 {
                result.unwrap();
                let decoded = decode_binary_value(&ty, &encoded[1..]).unwrap();
                let Column::Json(json) = &decoded else {
                    panic!("expected JSON")
                };
                let JsonBody::Structured(body) = &json.body else {
                    panic!("expected structured JSON")
                };
                let cell = body.shared_values.value(0);
                let (decoded_type, prefix_len) = read_binary_type_prefix(cell).unwrap();
                assert_eq!(decoded_type, child_type);
                let Column::Tuple(tuple) =
                    decode_binary_value(&decoded_type, &cell[prefix_len..]).unwrap()
                else {
                    panic!("expected Tuple")
                };
                assert_eq!(tuple.fields.len(), count);
                assert!(tuple
                    .fields
                    .iter()
                    .all(|field| matches!(field, Column::UInt8(values) if values.values == [79])));
                let mut reencoded = vec![0xab];
                encode_binary_value(&mut reencoded, &ty, &decoded, 0).unwrap();
                assert_eq!(reencoded, encoded);
            } else {
                assert!(matches!(result,
                    Err(BinaryValueError::Invalid(reason)) if reason.contains("complexity")));
                assert_eq!(encoded, [0xab]);
            }
        }
    }

    #[test]
    fn json_preserves_tagged_nothing_shared_values() {
        let ty = parse("JSON");
        let bytes = [1, 1, b'x', 0];
        let column = decode_binary_value(&ty, &bytes).unwrap();
        let Column::Json(json) = &column else {
            panic!("expected JSON")
        };
        let JsonBody::Structured(body) = &json.body else {
            panic!("expected structured JSON")
        };
        assert_eq!(body.shared_offsets, [0, 1]);
        assert_eq!(body.shared_paths.value(0), b"x");
        assert_eq!(body.shared_values.value(0), [0]);
        let mut encoded = Vec::new();
        encode_binary_value(&mut encoded, &ty, &column, 0).unwrap();
        assert_eq!(encoded, bytes);
        assert!(matches!(
            decode_binary_value(&ChType::Nothing, &[]),
            Err(BinaryValueError::Invalid(reason)) if reason == "non-null Nothing value"
        ));
    }

    #[test]
    fn json_shared_paths_are_sorted_with_values() {
        let ty = parse("JSON");
        let mut bytes = vec![4];
        for (path, value) in [(b"z", 13u8), (&[255], 17), (b"a", 79), (&[128], 31)] {
            write_string(&mut bytes, path);
            write_binary_type(&mut bytes, &ChType::UInt8);
            bytes.push(value);
        }
        let Column::Json(json) = decode_binary_value(&ty, &bytes).unwrap() else {
            panic!("expected JSON")
        };
        let JsonBody::Structured(body) = json.body else {
            panic!("expected structured JSON")
        };
        assert_eq!(body.shared_paths.value(0), b"a");
        assert_eq!(body.shared_values.value(0), [1, 79]);
        assert_eq!(body.shared_paths.value(1), b"z");
        assert_eq!(body.shared_values.value(1), [1, 13]);
        assert_eq!(body.shared_paths.value(2), [128]);
        assert_eq!(body.shared_values.value(2), [1, 31]);
        assert_eq!(body.shared_paths.value(3), [255]);
        assert_eq!(body.shared_values.value(3), [1, 17]);
        let column = Column::Json(JsonColumn::structured(*body));
        let mut encoded = Vec::new();
        encode_binary_value(&mut encoded, &ty, &column, 0).unwrap();
        assert_eq!(decode_binary_value(&ty, &encoded).unwrap(), column);
    }

    #[test]
    fn json_rejects_unsorted_or_duplicate_typed_metadata() {
        for names in [["z", "a"], ["a", "a"]] {
            let ty = ChType::Json {
                max_dynamic_paths: 0,
                max_dynamic_types: 0,
                typed_paths: names
                    .iter()
                    .map(|name| ((*name).into(), ChType::UInt8))
                    .collect(),
                skip_paths: Vec::new(),
                skip_regexps: Vec::new(),
            };
            assert!(matches!(
                decode_binary_value(&ty, &[0]),
                Err(BinaryValueError::Invalid(_))
            ));
            let column = Column::Json(JsonColumn::structured(StructuredJson::from_parts(
                names
                    .iter()
                    .map(|name| {
                        (
                            (*name).into(),
                            Column::UInt8(PrimitiveColumn::new(vec![13])),
                        )
                    })
                    .collect(),
                Vec::new(),
                vec![0, 0],
                Utf8Column::new(vec![0], Vec::new()),
                Utf8Column::new(vec![0], Vec::new()),
                1,
            )));
            let mut encoded = vec![0xab];
            assert!(matches!(
                encode_binary_value(&mut encoded, &parse("JSON(a UInt8, z UInt8)"), &column, 0),
                Err(BinaryValueError::Invalid(_))
            ));
            assert_eq!(encoded, [0xab]);
        }
    }

    #[test]
    fn json_repeated_values_do_not_repeat_type_walks() {
        let fields = (0..990)
            .map(|index| (format!("v{index:03}"), ChType::UInt8))
            .collect();
        let nested = ChType::Array(Box::new(ChType::Nested(fields)));
        let mut fields = vec![(None, ChType::Tuple(Vec::new())); 990];
        fields.push((None, ChType::Int8));
        let tuple = ChType::Array(Box::new(ChType::Tuple(fields)));
        for (ty, value) in [(nested, 0), (tuple, 13)] {
            let mut bytes = vec![1];
            write_string(&mut bytes, b"x");
            write_binary_type(&mut bytes, &ty);
            write_varint(&mut bytes, 79);
            bytes.extend(std::iter::repeat(value).take(79));
            SKIP_STEPS.with(|steps| steps.set(0));
            decode_binary_value(&parse("JSON"), &bytes).unwrap();
            let steps = SKIP_STEPS.with(|steps| steps.get());
            assert!(steps <= 79 + 2, "{steps} repeated type steps");
        }
    }

    #[test]
    fn json_caps_zero_width_tuple_shared_value_counts() {
        let ty = parse("JSON");
        for (count, valid) in [
            (MAX_ZERO_WIDTH_ELEMENTS, true),
            (MAX_ZERO_WIDTH_ELEMENTS + 1, false),
            (usize::MAX, false),
        ] {
            let mut bytes = vec![1];
            write_string(&mut bytes, b"x");
            write_binary_type(&mut bytes, &parse("Array(Tuple())"));
            write_varint(&mut bytes, count as u64);
            match decode_binary_value(&ty, &bytes) {
                Ok(column) => {
                    assert!(valid);
                    let mut encoded = Vec::new();
                    encode_binary_value(&mut encoded, &ty, &column, 0).unwrap();
                    assert_eq!(encoded, bytes);
                }
                Err(BinaryValueError::Invalid(reason)) => {
                    assert!(!valid, "{reason}");
                    assert!(reason.contains("Array element count"));
                }
                Err(error) => panic!("{error}"),
            }
        }
        let mut bytes = vec![1];
        write_string(&mut bytes, b"x");
        write_binary_type(&mut bytes, &parse("Array(Array(Tuple()))"));
        write_varint(&mut bytes, 13);
        for _ in 0..13 {
            write_varint(&mut bytes, MAX_ZERO_WIDTH_ELEMENTS as u64);
        }
        let column = decode_binary_value(&ty, &bytes).unwrap();
        let mut encoded = Vec::new();
        encode_binary_value(&mut encoded, &ty, &column, 0).unwrap();
        assert_eq!(encoded, bytes);
    }

    #[test]
    fn rejects_truncated_and_trailing_payloads() {
        assert!(matches!(
            decode_binary_value(&ChType::Int32, &[0x01, 0x02]),
            Err(BinaryValueError::Invalid(_))
        ));
        assert!(matches!(
            decode_binary_value(&ChType::Int32, &[0x01, 0x02, 0x03, 0x04, 0x05]),
            Err(BinaryValueError::Invalid(_))
        ));
        // Array element run cut short.
        let mut bytes = varint(3);
        bytes.push(0x13);
        assert!(matches!(
            decode_binary_value(&parse("Array(UInt8)"), &bytes),
            Err(BinaryValueError::Invalid(_))
        ));
        // Truncated String length prefix.
        assert!(matches!(
            decode_binary_value(&ChType::String, &[0x80]),
            Err(BinaryValueError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_hostile_counts_before_looping() {
        // A declared count far beyond the remaining bytes fails up front.
        let mut bytes = varint(1 << 40);
        bytes.push(0x13);
        assert!(matches!(
            decode_binary_value(&parse("Array(UInt8)"), &bytes),
            Err(BinaryValueError::Invalid(_))
        ));
        // Zero-width elements cannot be bounded by bytes; the fixed cap bites.
        let bytes = varint((MAX_ZERO_WIDTH_ELEMENTS + 1) as u64);
        assert!(matches!(
            decode_binary_value(&parse("Array(Tuple())"), &bytes),
            Err(BinaryValueError::Invalid(_))
        ));
        // A legal zero-width run under the cap decodes.
        let bytes = varint(5);
        let Column::Array(c) = decode_binary_value(&parse("Array(Tuple())"), &bytes).unwrap()
        else {
            panic!("expected Array")
        };
        assert_eq!(c.offsets, vec![0, 5]);
    }

    /// Run `f` and return its result with the number of per-value steps taken.
    fn with_visits<T>(f: impl FnOnce() -> T) -> (T, usize) {
        VISITS.with(|visits| visits.set(0));
        let out = f();
        (out, VISITS.with(|visits| visits.get()))
    }

    /// `depth` single-field Tuples around `Tuple()`.
    fn nested_empty_tuple(depth: usize) -> String {
        format!("{}Tuple(){}", "Tuple(".repeat(depth), ")".repeat(depth))
    }

    #[test]
    fn zero_width_runs_take_constant_steps() {
        let max = MAX_ZERO_WIDTH_ELEMENTS as u64;
        let mut nested_arrays = varint(100);
        for _ in 0..100 {
            nested_arrays.extend(varint(max));
        }
        let cases = [
            (format!("Array({})", nested_empty_tuple(97)), varint(max)),
            (
                format!(
                    "Map({}, {})",
                    nested_empty_tuple(89),
                    nested_empty_tuple(89)
                ),
                varint(max),
            ),
            ("Array(Array(Tuple()))".to_string(), nested_arrays),
            (
                "Array(SimpleAggregateFunction(any, SimpleAggregateFunction(any, Tuple(Tuple()))))"
                    .to_string(),
                varint(max),
            ),
            (
                "Nested(a Tuple(), b Tuple(Tuple()))".to_string(),
                varint(max),
            ),
        ];
        for (name, bytes) in cases {
            let ch_type = parse(&name);
            let (column, decode_steps) = with_visits(|| decode_binary_value(&ch_type, &bytes));
            let column = column.unwrap_or_else(|e| panic!("decode {name}: {e}"));
            let (encoded, encode_steps) = with_visits(|| encode_one(&ch_type, &column, 0));
            assert_eq!(encoded, bytes, "{name}");
            assert!(decode_steps < 1_000, "{name}: {decode_steps} decode steps");
            assert!(encode_steps < 1_000, "{name}: {encode_steps} encode steps");
        }

        let ch_type = parse(&format!("Array({})", nested_empty_tuple(97)));
        let mut column = &decode_binary_value(&ch_type, &varint(max)).unwrap();
        let Column::Array(array) = column else {
            panic!("expected Array")
        };
        column = &array.values;
        for _ in 0..=97 {
            let Column::Tuple(tuple) = column else {
                panic!("expected Tuple")
            };
            assert_eq!(tuple.len, MAX_ZERO_WIDTH_ELEMENTS);
            column = tuple.fields.first().unwrap_or(column);
        }
    }

    #[test]
    fn empty_zero_width_runs_take_constant_steps() {
        let n = 10_000;
        let wide = format!("Tuple({})", vec!["Tuple()"; 990].join(", "));
        for name in [
            format!("Array(Array({}))", nested_empty_tuple(97)),
            format!(
                "Array(Map({}, {}))",
                nested_empty_tuple(97),
                nested_empty_tuple(97)
            ),
            format!("Array(Array({wide}))"),
        ] {
            let ch_type = parse(&name);
            let bytes = [varint(n as u64), vec![0; n]].concat();
            let (column, decode_steps) = with_visits(|| decode_binary_value(&ch_type, &bytes));
            let column = column.unwrap();
            let (encoded, encode_steps) = with_visits(|| encode_one(&ch_type, &column, 0));
            assert_eq!(encoded, bytes);
            assert!(decode_steps < 2 * n, "{decode_steps} decode steps");
            assert!(encode_steps < 2 * n, "{encode_steps} encode steps");
        }
    }

    #[test]
    fn encode_zero_width_errors_match_the_per_row_path() {
        fn tuple(len: usize, fields: Vec<Column>) -> Column {
            Column::Tuple(TupleColumn::new(fields, len))
        }
        fn array(offsets: Vec<i64>, values: Column) -> Column {
            Column::Array(ArrayColumn::new(offsets, values))
        }
        fn map(offsets: Vec<i64>, keys: Column, values: Column, len: usize) -> Column {
            Column::Map(MapColumn::new(offsets, tuple(len, vec![keys, values])))
        }
        fn dict(indices: Vec<i32>) -> Column {
            Column::Dictionary(crate::column::DictionaryColumn::new(
                indices,
                tuple(1, vec![]),
            ))
        }
        let nothing = || Column::Nothing(NothingColumn::new(3));
        let null_at_3 = Column::Tuple(TupleColumn::new_nullable(
            vec![],
            5,
            Bitmap::from_ch_null_map(&[0, 0, 0, 1, 0]),
        ));
        let cases = [
            (
                "Array(Tuple())",
                array(vec![2, 6], tuple(4, vec![])),
                "row 4 is out of range for 4 rows",
            ),
            (
                "Map(Tuple(), Tuple())",
                map(vec![0, 5], tuple(2, vec![]), tuple(5, vec![]), 5),
                "row 2 is out of range for 2 rows",
            ),
            (
                "Map(Tuple(), Tuple())",
                map(vec![0, 5], tuple(5, vec![]), tuple(3, vec![]), 5),
                "row 3 is out of range for 3 rows",
            ),
            (
                "Map(Tuple(), Tuple(Tuple()))",
                map(
                    vec![0, 5],
                    tuple(5, vec![]),
                    tuple(5, vec![tuple(3, vec![])]),
                    5,
                ),
                "row 3 is out of range for 3 rows",
            ),
            (
                "Nested(a Tuple())",
                array(vec![0, 5], tuple(2, vec![tuple(5, vec![])])),
                "row 2 is out of range for 2 rows",
            ),
            (
                "Nested(a Tuple())",
                array(vec![0, 5], tuple(5, vec![tuple(3, vec![])])),
                "row 3 is out of range for 3 rows",
            ),
            (
                "Array(Tuple(Tuple()))",
                array(vec![0, 5], tuple(5, vec![tuple(1, vec![])])),
                "row 1 is out of range for 1 rows",
            ),
            (
                "Array(Tuple(Tuple()))",
                array(vec![0, 5], tuple(5, vec![null_at_3])),
                "null row under a non-nullable type",
            ),
            (
                "Array(LowCardinality(Tuple()))",
                array(vec![0, 3], dict(vec![0, 7, 0])),
                "row 7 is out of range for 1 rows",
            ),
            (
                "Array(LowCardinality(Tuple()))",
                array(vec![0, 3], dict(vec![0, -1, 0])),
                "row 1 has negative dictionary index",
            ),
            (
                "Array(Tuple(Nothing))",
                array(vec![0, 3], tuple(1, vec![nothing()])),
                "non-null Nothing value",
            ),
            (
                "Array(Tuple(Tuple(), Nothing))",
                array(vec![0, 3], tuple(1, vec![tuple(3, vec![]), nothing()])),
                "non-null Nothing value",
            ),
            (
                "Map(Tuple(), Tuple(Nothing))",
                map(vec![0, 3], tuple(1, vec![]), tuple(3, vec![nothing()]), 3),
                "non-null Nothing value",
            ),
        ];
        for (name, column, expected) in cases {
            let mut buf = vec![0xaa];
            let err = encode_binary_value(&mut buf, &parse(name), &column, 0)
                .expect_err(name)
                .to_string();
            assert!(err.contains(expected), "{name}: {err}");
            assert_eq!(buf, [0xaa], "{name}");
        }

        // The dictionary fallback still encodes valid indexes.
        let ch_type = parse("Array(LowCardinality(Tuple()))");
        assert_eq!(
            encode_one(&ch_type, &array(vec![0, 3], dict(vec![0, 0, 0])), 0),
            [3]
        );
    }

    #[test]
    fn byte_backed_zero_width_values_keep_their_limits() {
        let n = MAX_ZERO_WIDTH_ELEMENTS + 1;
        let with_count = |count: usize, tail: Vec<u8>| [varint(count as u64), tail].concat();
        let flags: Vec<u8> = (0..n).map(|i| (i % 2) as u8).collect();
        // (type, value, whether encode accepts it back)
        let cases = [
            ("Array(Nullable(Nothing))", with_count(n, vec![1; n]), true),
            // Encode caps Nullable(Tuple()) runs at the zero-width limit.
            ("Array(Nullable(Tuple()))", with_count(n, flags), false),
            ("Map(UInt32, Tuple())", with_count(n, vec![13; 4 * n]), true),
            (
                "Array(Tuple(UInt8, Tuple()))",
                with_count(n, vec![79; n]),
                true,
            ),
            (
                "Array(Tuple(Tuple(), Tuple()))",
                with_count(n - 1, vec![]),
                true,
            ),
            ("Map(Tuple(), Tuple())", with_count(n - 1, vec![]), true),
            (
                "Tuple(Array(Tuple()), Array(Tuple()))",
                [varint(999_999), varint(999_999)].concat(),
                true,
            ),
        ];
        for (name, bytes, reencodes) in cases {
            let ch_type = parse(name);
            let column = decode_binary_value(&ch_type, &bytes)
                .unwrap_or_else(|e| panic!("decode {name}: {e}"));
            if reencodes {
                assert_eq!(encode_one(&ch_type, &column, 0), bytes, "{name}");
            }
        }
        // The per-container cap is unchanged for zero-width runs.
        for name in ["Array(Tuple(Tuple(), Tuple()))", "Map(Tuple(), Tuple())"] {
            assert!(matches!(
                decode_binary_value(&parse(name), &varint(n as u64)),
                Err(BinaryValueError::Invalid(_))
            ));
        }
    }

    #[test]
    fn rejects_zero_width_fixed_string_and_qbit() {
        let fixed = ChType::FixedString(0);
        let qbit = ChType::QBit {
            element_type: QBitElementType::Float32,
            dimension: 0,
        };
        let nested = ChType::Tuple(vec![
            (
                None,
                ChType::Array(Box::new(ChType::Array(Box::new(fixed.clone())))),
            ),
            (None, ChType::String),
        ]);
        for ch_type in [fixed, qbit, nested] {
            assert!(matches!(
                decode_binary_value(&ch_type, &[0]),
                Err(BinaryValueError::Invalid(ref reason)) if reason.contains("must be positive")
            ));
        }
    }

    #[test]
    fn encode_zero_width_runs_validate_child_lengths() {
        let ch_type = parse("Array(Tuple())");
        let array = |offsets: Vec<i64>, tuple: TupleColumn| {
            Column::Array(ArrayColumn::new(offsets, Column::Tuple(tuple)))
        };
        let short = array(vec![0, 5], TupleColumn::new(vec![], 3));
        let mut buf = vec![0xaa];
        assert!(matches!(
            encode_binary_value(&mut buf, &ch_type, &short, 0),
            Err(BinaryValueError::Invalid(_))
        ));
        assert_eq!(buf, [0xaa]);
        // An empty run is not checked against the child, as before.
        let empty = array(vec![7, 7], TupleColumn::new(vec![], 3));
        assert_eq!(encode_one(&ch_type, &empty, 0), [0]);

        // A tuple validity bitmap takes the per-row path.
        let valid = TupleColumn::new_nullable(vec![], 3, Bitmap::all_valid(3));
        assert_eq!(encode_one(&ch_type, &array(vec![0, 3], valid), 0), [3]);
        let null = TupleColumn::new_nullable(vec![], 3, Bitmap::from_ch_null_map(&[0, 1, 0]));
        assert!(matches!(
            encode_binary_value(&mut Vec::new(), &ch_type, &array(vec![0, 3], null), 0),
            Err(BinaryValueError::Invalid(_))
        ));
    }

    #[test]
    fn reports_undecodable_types_as_unsupported() {
        assert!(matches!(
            decode_binary_value(&parse("AggregateFunction(sum, UInt64)"), &[0x00]),
            Err(BinaryValueError::Unsupported(_))
        ));
        assert!(matches!(
            decode_binary_value(&parse("Variant(String, UInt64)"), &[0x00]),
            Err(BinaryValueError::Unsupported(_))
        ));
        assert!(matches!(
            decode_binary_value(&parse("Dynamic"), &[0x00]),
            Err(BinaryValueError::Unsupported(_))
        ));
        // A Nothing VALUE is unrepresentable even though Array(Nothing) with a
        // zero count decodes to an empty child.
        let mut bytes = varint(1);
        bytes.push(0x00);
        assert!(matches!(
            decode_binary_value(&parse("Array(Nothing)"), &bytes),
            Err(BinaryValueError::Invalid(reason)) if reason == "non-null Nothing value"
        ));
        let bytes = varint(0);
        assert!(decode_binary_value(&parse("Array(Nothing)"), &bytes).is_ok());
    }

    #[test]
    fn parses_descriptor_prefix_and_reports_consumed_bytes() {
        for name in [
            "Int32",
            "String",
            "Array(Nullable(UInt64))",
            "Map(String, Tuple(a Int8, b String))",
            "DateTime64(3, 'UTC')",
        ] {
            let ch_type = parse(name);
            let mut bytes = Vec::new();
            write_binary_type(&mut bytes, &ch_type);
            bytes.extend_from_slice(b"tail");
            let (parsed, consumed) = read_binary_type_prefix(&bytes).unwrap();
            assert_eq!(parsed, ch_type);
            assert_eq!(consumed, bytes.len() - 4);
        }
        let json_type = parse("JSON");
        let mut json_bytes = Vec::new();
        write_binary_type(&mut json_bytes, &json_type);
        let (parsed, consumed) = read_binary_type_prefix(&json_bytes).unwrap();
        assert_eq!(parsed, json_type);
        assert_eq!(consumed, json_bytes.len());
        assert!(matches!(
            decode_binary_value(&json_type, &[]),
            Err(BinaryValueError::Invalid(_))
        ));
        // A truncated descriptor is invalid.
        assert!(matches!(
            read_binary_type_prefix(&[0x1e]),
            Err(BinaryValueError::Invalid(_))
        ));
    }

    #[test]
    fn decodes_full_cells_descriptor_plus_value() {
        // The exact shared-cell shape: descriptor then one value.
        let ch_type = parse("Array(UInt8)");
        let mut cell = Vec::new();
        write_binary_type(&mut cell, &ch_type);
        cell.extend(varint(3));
        cell.extend_from_slice(&[1, 2, 3]);
        let (parsed, consumed) = read_binary_type_prefix(&cell).unwrap();
        assert_eq!(parsed, ch_type);
        let Column::Array(c) = decode_binary_value(&parsed, &cell[consumed..]).unwrap() else {
            panic!("expected Array")
        };
        let Column::UInt8(elems) = c.values.as_ref() else {
            panic!("expected UInt8 elements")
        };
        assert_eq!(elems.values, vec![1, 2, 3]);
    }

    // -----------------------------------------------------------------------
    // Encoding
    // -----------------------------------------------------------------------

    fn string_value(s: &[u8]) -> Vec<u8> {
        let mut out = varint(s.len() as u64);
        out.extend_from_slice(s);
        out
    }

    fn encode_one(ch_type: &ChType, column: &Column, row: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_binary_value(&mut buf, ch_type, column, row)
            .unwrap_or_else(|e| panic!("encode {ch_type} row {row}: {e}"));
        buf
    }

    /// Value bytes laid out per the module docs, for every type the decoder
    /// supports. Each must survive decode -> encode unchanged.
    fn value_cases() -> Vec<(&'static str, Vec<u8>)> {
        let mut uuid = Vec::new();
        uuid.extend_from_slice(&0x11e7_5cb3_61f0_c404u64.to_le_bytes());
        uuid.extend_from_slice(&0xa600_6ad3_dba0_907bu64.to_le_bytes());
        let mut ipv6 = vec![0x20, 0x01, 0x0d, 0xb8];
        ipv6.extend_from_slice(&[0; 11]);
        ipv6.push(0x01);
        vec![
            ("Bool", vec![1]),
            ("Bool", vec![0]),
            ("Int8", (-13i8).to_le_bytes().to_vec()),
            ("Int16", (-1300i16).to_le_bytes().to_vec()),
            ("Int32", (-79i32).to_le_bytes().to_vec()),
            ("Int64", i64::MIN.to_le_bytes().to_vec()),
            ("UInt8", vec![255]),
            ("UInt16", 65535u16.to_le_bytes().to_vec()),
            ("UInt32", 13u32.to_le_bytes().to_vec()),
            ("UInt64", u64::MAX.to_le_bytes().to_vec()),
            ("Float32", 1.5f32.to_le_bytes().to_vec()),
            ("Float64", (-0.125f64).to_le_bytes().to_vec()),
            ("BFloat16", vec![0x80, 0x3f]),
            ("Int128", (0..16).collect()),
            ("UInt128", vec![0xff; 16]),
            ("Int256", (0..32).collect()),
            ("UInt256", vec![0x7f; 32]),
            ("Date", 19782u16.to_le_bytes().to_vec()),
            ("Date32", (-25567i32).to_le_bytes().to_vec()),
            ("DateTime", 1_704_164_645u32.to_le_bytes().to_vec()),
            ("DateTime('UTC')", 13u32.to_le_bytes().to_vec()),
            (
                "DateTime64(3, 'UTC')",
                1_704_164_645_678i64.to_le_bytes().to_vec(),
            ),
            ("Time", (-3600i32).to_le_bytes().to_vec()),
            ("Time64(6)", 45_296_789_000i64.to_le_bytes().to_vec()),
            ("IntervalSecond", 79i64.to_le_bytes().to_vec()),
            ("Decimal(9, 2)", (-314i32).to_le_bytes().to_vec()),
            ("Decimal(18, 3)", 123_456_789i64.to_le_bytes().to_vec()),
            ("Decimal(38, 10)", (0..16).collect()),
            ("Decimal(76, 20)", (0..32).collect()),
            ("UUID", uuid),
            ("IPv4", 0xc0a8_01feu32.to_le_bytes().to_vec()),
            ("IPv6", ipv6),
            ("Enum8('a' = -1, 'b' = 13)", vec![13]),
            (
                "Enum16('y' = 1000, 'z' = -1000)",
                (-1000i16).to_le_bytes().to_vec(),
            ),
            ("String", string_value(b"")),
            ("String", string_value("user_1 \u{4e16}".as_bytes())),
            ("FixedString(4)", b"ab\0\0".to_vec()),
            (
                "QBit(Float32, 3)",
                [
                    varint(3),
                    1.5f32.to_le_bytes().to_vec(),
                    (-2.5f32).to_le_bytes().to_vec(),
                    13f32.to_le_bytes().to_vec(),
                ]
                .concat(),
            ),
            (
                "QBit(BFloat16, 2)",
                [varint(2), vec![0x80, 0x3f, 0, 0]].concat(),
            ),
            ("Nullable(Int32)", vec![1]),
            (
                "Nullable(Int32)",
                [vec![0], 13i32.to_le_bytes().to_vec()].concat(),
            ),
            (
                "Nullable(String)",
                [vec![0], string_value(b"user_2")].concat(),
            ),
            ("Nullable(Nothing)", vec![1]),
            ("LowCardinality(String)", string_value(b"user_1")),
            ("LowCardinality(Nullable(String))", vec![1]),
            (
                "LowCardinality(Nullable(String))",
                [vec![0], string_value(b"x")].concat(),
            ),
            ("Array(UInt8)", [varint(3), vec![1, 2, 3]].concat()),
            ("Array(String)", varint(0)),
            (
                "Array(Nullable(Int64))",
                [varint(2), vec![1], vec![0], 79i64.to_le_bytes().to_vec()].concat(),
            ),
            (
                "Array(Array(Int32))",
                [
                    varint(2),
                    varint(1),
                    13i32.to_le_bytes().to_vec(),
                    varint(0),
                ]
                .concat(),
            ),
            (
                "Array(LowCardinality(Nullable(String)))",
                [varint(2), vec![1], vec![0], string_value(b"x")].concat(),
            ),
            (
                "Map(String, UInt64)",
                [
                    varint(2),
                    string_value(b"k1"),
                    13u64.to_le_bytes().to_vec(),
                    string_value(b"k2"),
                    79u64.to_le_bytes().to_vec(),
                ]
                .concat(),
            ),
            (
                "Map(UInt8, Array(String))",
                [varint(1), vec![7], varint(1), string_value(b"v")].concat(),
            ),
            ("Tuple()", vec![]),
            (
                "Tuple(UInt8, String)",
                [vec![13], string_value(b"user_1")].concat(),
            ),
            ("Tuple(a UInt8, b Nullable(String))", vec![79, 1]),
            ("Nullable(Tuple(a UInt8))", vec![1]),
            ("Nullable(Tuple(a UInt8))", vec![0, 13]),
            (
                "Point",
                [
                    1.5f64.to_le_bytes().to_vec(),
                    (-2.5f64).to_le_bytes().to_vec(),
                ]
                .concat(),
            ),
            (
                "Ring",
                [
                    varint(1),
                    0f64.to_le_bytes().to_vec(),
                    1f64.to_le_bytes().to_vec(),
                ]
                .concat(),
            ),
            (
                "SimpleAggregateFunction(sum, UInt64)",
                13u64.to_le_bytes().to_vec(),
            ),
            (
                "Nested(id UInt8, s String)",
                [varint(1), vec![13], string_value(b"s")].concat(),
            ),
        ]
    }

    #[test]
    fn encode_values_and_full_cells_round_trip_for_every_supported_type() {
        for (name, bytes) in value_cases() {
            let ch_type = parse(name);
            let column = decode_binary_value(&ch_type, &bytes)
                .unwrap_or_else(|e| panic!("decode {name} {bytes:?}: {e}"));
            assert_eq!(encode_one(&ch_type, &column, 0), bytes, "{name}");
            let mut cell = Vec::new();
            write_binary_type_prefix(&mut cell, &ch_type).unwrap();
            encode_binary_value(&mut cell, &ch_type, &column, 0).unwrap();
            let (parsed, consumed) = read_binary_type_prefix(&cell).unwrap();
            assert_eq!(&cell[consumed..], bytes.as_slice(), "{name}");
            let again = decode_binary_value(&parsed, &cell[consumed..]).unwrap();
            assert_eq!(again, column, "{name}");
        }
    }

    #[test]
    fn skip_plan_consumption_matches_supported_value_decoding() {
        let mut cases = value_cases();
        cases.extend([
            ("JSON", vec![0]),
            ("JSON(a Int64)", vec![0]),
            ("JSON", vec![2, 1, b'z', 0, 1, b'a', 1, 79]),
        ]);
        for (name, bytes) in cases {
            let ty = parse(name);
            let mut array = varint(2);
            array.extend_from_slice(&bytes);
            array.extend_from_slice(&bytes);
            let mut tuple = bytes.clone();
            tuple.push(79);
            let mut map = vec![1, 1, b'k'];
            map.extend_from_slice(&bytes);
            for (ty, bytes) in [
                (ty.clone(), bytes),
                (ChType::Array(Box::new(ty.clone())), array),
                (
                    ChType::Tuple(vec![(None, ty.clone()), (None, ChType::UInt8)]),
                    tuple,
                ),
                (ChType::Map(Box::new(ChType::String), Box::new(ty)), map),
            ] {
                decode_binary_value(&ty, &bytes).unwrap_or_else(|error| panic!("{ty}: {error}"));
                let plan = SkipPlan::new(&ty, 0).unwrap();
                let mut input = bytes.clone();
                input.extend_from_slice(&[0xab, 0xcd]);
                let mut reader = ByteReader::new(&input);
                plan.skip(&mut reader)
                    .unwrap_or_else(|error| panic!("{ty}: {error}"));
                assert_eq!(reader.position(), bytes.len(), "{ty}");
                assert_eq!(reader.remaining_slice(), [0xab, 0xcd], "{ty}");
                for end in 0..bytes.len() {
                    assert!(
                        decode_binary_value(&ty, &bytes[..end]).is_err(),
                        "{ty}: {end}"
                    );
                    assert!(
                        plan.skip(&mut ByteReader::new(&bytes[..end])).is_err(),
                        "{ty}: {end}"
                    );
                }
            }
        }
    }

    #[test]
    fn encode_reads_any_row_of_bulk_columns() {
        // LowCardinality(Nullable(String)) in its bulk Dictionary shape: index 0
        // is the NULL slot, nulls live in the index validity.
        let dict = Column::Dictionary(crate::column::DictionaryColumn::new_nullable(
            vec![1, 0, 2, 1],
            Column::Utf8(Utf8Column::new(vec![0, 0, 6, 12], b"user_1user_2".to_vec())),
            Bitmap::from_ch_null_map(&[0, 1, 0, 0]),
        ));
        let lc = parse("LowCardinality(Nullable(String))");
        assert_eq!(
            encode_one(&lc, &dict, 0),
            [vec![0], string_value(b"user_1")].concat()
        );
        assert_eq!(encode_one(&lc, &dict, 1), vec![1]);
        assert_eq!(
            encode_one(&lc, &dict, 2),
            [vec![0], string_value(b"user_2")].concat()
        );

        let plain = Column::Dictionary(crate::column::DictionaryColumn::new(
            vec![1, 0],
            Column::Utf8(Utf8Column::new(vec![0, 1, 3], b"abc".to_vec())),
        ));
        assert_eq!(
            encode_one(&parse("LowCardinality(String)"), &plain, 0),
            string_value(b"bc")
        );

        // Array(Nullable(Int32)) with rows [13, NULL], [], [-79].
        let array = Column::Array(ArrayColumn::new(
            vec![0, 2, 2, 3],
            Column::Int32(PrimitiveColumn::new_nullable(
                vec![13, 0, -79],
                Bitmap::from_ch_null_map(&[0, 1, 0]),
            )),
        ));
        let at = parse("Array(Nullable(Int32))");
        assert_eq!(
            encode_one(&at, &array, 0),
            [varint(2), vec![0], 13i32.to_le_bytes().to_vec(), vec![1]].concat()
        );
        assert_eq!(encode_one(&at, &array, 1), varint(0));
        assert_eq!(
            encode_one(&at, &array, 2),
            [varint(1), vec![0], (-79i32).to_le_bytes().to_vec()].concat()
        );

        // Bool past the first bitmap byte.
        let bools = Column::Bool(BoolColumn::from_wire_bytes(&[0, 0, 0, 0, 0, 0, 0, 0, 1, 0]));
        assert_eq!(encode_one(&ChType::Bool, &bools, 8), vec![1]);
        assert_eq!(encode_one(&ChType::Bool, &bools, 9), vec![0]);

        // QBit row 1 reads its own dimension-sized slice.
        let qbit = Column::QBit(QBitColumn::new(
            Column::Float32(PrimitiveColumn::new(vec![1.0, 2.0, 13.0, 79.0])),
            2,
        ));
        assert_eq!(
            encode_one(&parse("QBit(Float32, 2)"), &qbit, 1),
            [
                varint(2),
                13f32.to_le_bytes().to_vec(),
                79f32.to_le_bytes().to_vec()
            ]
            .concat()
        );
    }

    #[test]
    fn encode_qbit_slices_preserve_width_bounds_and_null_checks() {
        let cases = [
            (
                parse("QBit(BFloat16, 2)"),
                Column::BFloat16(PrimitiveColumn::new(vec![
                    [0, 0],
                    [0, 0],
                    [0x80, 0x3f],
                    [0, 0x40],
                ])),
                vec![2, 0x80, 0x3f, 0, 0x40],
            ),
            (
                parse("QBit(Float64, 2)"),
                Column::Float64(PrimitiveColumn::new(vec![0.0, 0.0, 13.0, 79.0])),
                [
                    vec![2],
                    13f64.to_le_bytes().to_vec(),
                    79f64.to_le_bytes().to_vec(),
                ]
                .concat(),
            ),
        ];
        for (ch_type, values, expected) in cases {
            let column = Column::QBit(QBitColumn::new(values, 2));
            assert_eq!(encode_one(&ch_type, &column, 1), expected);
            let mut buf = vec![0xaa];
            assert!(encode_binary_value(&mut buf, &ch_type, &column, 2).is_err());
            assert_eq!(buf, [0xaa]);
        }
        let ch_type = parse("QBit(Float32, 2)");
        let column = Column::QBit(QBitColumn::new(
            Column::Float32(PrimitiveColumn::new_nullable(
                vec![13.0, 79.0],
                Bitmap::from_ch_null_map(&[0, 1]),
            )),
            2,
        ));
        let mut buf = vec![0xaa];
        assert!(matches!(
            encode_binary_value(&mut buf, &ch_type, &column, 0),
            Err(BinaryValueError::Invalid(_))
        ));
        assert_eq!(buf, [0xaa]);
    }

    #[test]
    fn encode_qbit_rejects_even_all_valid_child_bitmaps() {
        let column = Column::QBit(QBitColumn::new(
            Column::Float32(PrimitiveColumn::new_nullable(
                vec![13.0, 79.0],
                Bitmap::all_valid(2),
            )),
            2,
        ));
        let mut buf = vec![0xaa];
        assert!(matches!(
            encode_binary_value(&mut buf, &parse("QBit(Float32, 2)"), &column, 0),
            Err(BinaryValueError::Invalid(_))
        ));
        assert_eq!(buf, [0xaa]);
    }

    #[test]
    fn bounded_type_walk_matches_block_depth_at_alias_and_container_limits() {
        use crate::native::encode::validate::type_depth;

        let leaves = [
            ChType::UInt8,
            ChType::Tuple(vec![]),
            ChType::Nested(vec![]),
            parse("Nested(value UInt8)"),
            parse("Nested(value Point, extra MultiPolygon)"),
            parse("SimpleAggregateFunction(anyLast, UInt8)"),
            parse("Tuple(UInt8, Map(String, Array(UInt8)))"),
            parse("JSON(path Array(UInt8))"),
            ChType::Geometry,
            ChType::Geo(GeoKind::Point),
            ChType::Geo(GeoKind::Ring),
            ChType::Geo(GeoKind::LineString),
            ChType::Geo(GeoKind::MultiPoint),
            ChType::Geo(GeoKind::Polygon),
            ChType::Geo(GeoKind::MultiLineString),
            ChType::Geo(GeoKind::MultiPolygon),
            parse("Variant(String, Array(UInt8))"),
            parse("AggregateFunction(sum, UInt64)"),
        ];
        for leaf in leaves {
            for wrappers in [0, 1, 94, 95, 96, 97, 98, 99, 100, 101] {
                let mut ch_type = leaf.clone();
                for _ in 0..wrappers {
                    ch_type = ChType::Array(Box::new(ch_type));
                }
                assert_eq!(
                    check_type_depth(&ch_type).is_ok(),
                    type_depth(&ch_type) <= MAX_TYPE_DEPTH,
                    "{ch_type}"
                );
            }
        }
        // Wide siblings do not consume a shared depth budget.
        let wide = ChType::Tuple((0..1_000).map(|_| (None, ChType::UInt8)).collect());
        assert!(check_type_depth(&wide).is_ok());
        assert_eq!(type_depth(&wide), 1);
    }

    #[test]
    fn bounded_type_walk_rejects_million_deep_types_before_formatting() {
        let mut ch_type = ChType::UInt8;
        for _ in 0..1_000_000 {
            ch_type = ChType::Array(Box::new(ch_type));
        }
        // Also put the hostile subtree below an unsupported node. Coverage
        // checks must not render that node before completing the depth gate.
        let ch_type = ChType::Variant(vec![ch_type]);
        let mut buf = vec![0xaa];
        let column = Column::UInt8(PrimitiveColumn::new(vec![13]));
        let value_result = encode_binary_value(&mut buf, &ch_type, &column, 0);
        let prefix_result = write_binary_type_prefix(&mut buf, &ch_type);
        let ChType::Variant(mut alternatives) = ch_type else {
            unreachable!()
        };
        let mut current = alternatives.pop().unwrap();
        while let ChType::Array(inner) = current {
            current = *inner;
        }
        for result in [value_result, prefix_result] {
            assert!(matches!(result, Err(BinaryValueError::Invalid(reason))
                if reason == format!("nesting exceeds {MAX_TYPE_DEPTH}")));
        }
        assert_eq!(buf, [0xaa]);
    }

    #[test]
    fn encode_rejects_unsupported_types_inside_empty_or_null_values() {
        let empty = Column::Array(ArrayColumn::new(
            vec![0, 0],
            Column::UInt8(PrimitiveColumn::new(vec![])),
        ));
        let json = Column::Json(
            crate::column::JsonColumn::text(Utf8Column::new(vec![0, 2], b"{}".to_vec()))
                .with_validity(Some(Bitmap::from_ch_null_map(&[1]))),
        );
        for (ch_type, column) in [
            (parse("Array(Variant(String, UInt64))"), empty),
            (parse("Nullable(JSON)"), json),
        ] {
            let mut buf = vec![0xaa];
            assert!(matches!(
                encode_binary_value(&mut buf, &ch_type, &column, 0),
                Err(BinaryValueError::Unsupported(_))
            ));
            assert_eq!(buf, [0xaa]);
        }
    }

    #[test]
    fn encode_checks_empty_container_child_shapes_before_writing() {
        let column = Column::Array(ArrayColumn::new(
            vec![0, 0],
            Column::Int32(PrimitiveColumn::new(vec![])),
        ));
        let mut buf = vec![0xaa];
        assert!(matches!(
            encode_binary_value(&mut buf, &parse("Array(String)"), &column, 0),
            Err(BinaryValueError::Invalid(_))
        ));
        assert_eq!(buf, [0xaa]);
    }

    #[test]
    fn prefix_rejects_invalid_descriptors_transactionally() {
        let invalid = [
            parse("Tuple(a UInt8, String)"),
            ChType::FixedString(0),
            ChType::DateTime64 {
                precision: 12,
                timezone: None,
            },
            ChType::Nullable(Box::new(ChType::Array(Box::new(ChType::UInt8)))),
            ChType::Dynamic { max_types: 255 },
            ChType::Decimal {
                precision: 9,
                scale: 2,
                bits: 17,
            },
        ];
        for ch_type in invalid {
            let mut buf = vec![0xaa, 0xbb];
            assert!(write_binary_type_prefix(&mut buf, &ch_type).is_err());
            assert_eq!(buf, [0xaa, 0xbb], "{ch_type}");
        }
    }

    #[test]
    fn encode_rejects_null_under_nonnullable_type_and_wrong_null_shape() {
        let ints = Column::Int32(PrimitiveColumn::new_nullable(
            vec![13, 79],
            Bitmap::from_ch_null_map(&[1, 0]),
        ));
        let dict = Column::Dictionary(crate::column::DictionaryColumn::new_nullable(
            vec![0, 1],
            Column::Utf8(Utf8Column::new(vec![0, 0, 1], b"x".to_vec())),
            Bitmap::from_ch_null_map(&[1, 0]),
        ));
        let wrong_dict = Column::Dictionary(crate::column::DictionaryColumn::new_nullable(
            vec![0],
            Column::Int32(PrimitiveColumn::new(vec![13])),
            Bitmap::from_ch_null_map(&[1]),
        ));
        for (ch_type, column) in [
            (ChType::Int32, &ints),
            (parse("LowCardinality(String)"), &dict),
            (parse("Nullable(String)"), &ints),
            (parse("LowCardinality(Nullable(String))"), &wrong_dict),
        ] {
            let mut buf = vec![0xaa];
            assert!(matches!(
                encode_binary_value(&mut buf, &ch_type, column, 0),
                Err(BinaryValueError::Invalid(_))
            ));
            assert_eq!(buf, [0xaa]);
        }
        // A binding can strip LC(Nullable(T)) to LC(T) for a non-null cell.
        assert_eq!(encode_one(&ChType::Int32, &ints, 1), 79i32.to_le_bytes());
        assert_eq!(
            encode_one(&parse("LowCardinality(String)"), &dict, 1),
            [1, b'x']
        );
    }

    #[test]
    fn encode_low_cardinality_nullable_alias_chains_for_plain_and_dictionary() {
        let plain = Column::Utf8(Utf8Column::new_nullable(
            vec![0, 1, 2],
            b"xx".to_vec(),
            Bitmap::from_ch_null_map(&[1, 0]),
        ));
        let dict = Column::Dictionary(crate::column::DictionaryColumn::new_nullable(
            vec![0, 1],
            Column::Utf8(Utf8Column::new(vec![0, 0, 1], b"x".to_vec())),
            Bitmap::from_ch_null_map(&[1, 0]),
        ));
        for name in [
            "LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))",
            "LowCardinality(SimpleAggregateFunction(anyLast, SimpleAggregateFunction(anyLast, Nullable(String))))",
            "LowCardinality(SimpleAggregateFunction(anyLast, Nullable(SimpleAggregateFunction(anyLast, String))))",
        ] {
            let ch_type = parse(name);
            let decoded_null = decode_binary_value(&ch_type, &[1]).unwrap();
            for column in [&plain, &dict, &decoded_null] {
                assert_eq!(encode_one(&ch_type, column, 0), [1], "{name}");
            }
            for column in [&plain, &dict] {
                assert_eq!(encode_one(&ch_type, column, 1), [0, 1, b'x'], "{name}");
            }
        }
    }

    #[test]
    fn encode_rejects_max_row_for_offset_columns_without_panicking() {
        let strings = Column::Utf8(Utf8Column::new(vec![0, 1], b"x".to_vec()));
        let array = Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::UInt8(PrimitiveColumn::new(vec![13])),
        ));
        let map = Column::Map(MapColumn::new(
            vec![0, 0],
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::UInt8(PrimitiveColumn::new(vec![])),
                    Column::UInt8(PrimitiveColumn::new(vec![])),
                ],
                0,
            )),
        ));
        for (ch_type, column) in [
            (ChType::String, strings),
            (parse("Array(UInt8)"), array),
            (parse("Map(UInt8, UInt8)"), map),
        ] {
            let mut buf = vec![0xaa];
            assert!(matches!(
                encode_binary_value(&mut buf, &ch_type, &column, usize::MAX),
                Err(BinaryValueError::Invalid(_))
            ));
            assert_eq!(buf, [0xaa]);
        }
    }

    #[test]
    fn encode_gates_deep_types_before_recursive_error_formatting() {
        let mut ch_type = ChType::Int32;
        for _ in 0..2_000 {
            ch_type = ChType::Array(Box::new(ch_type));
        }
        let column = Column::Int32(PrimitiveColumn::new(vec![13]));
        let mut buf = vec![0xaa];
        let result = encode_binary_value(&mut buf, &ch_type, &column, 0);
        // Avoid testing Rust's recursive drop implementation for caller-built
        // types. The encoder must reject without recursively formatting one.
        std::mem::forget(ch_type);
        assert!(
            matches!(result, Err(BinaryValueError::Invalid(reason)) if reason == format!("nesting exceeds {MAX_TYPE_DEPTH}"))
        );
        assert_eq!(buf, [0xaa]);
    }

    #[test]
    fn prefix_gates_deep_types_before_writing() {
        let mut ch_type = ChType::UInt8;
        for _ in 0..1_000 {
            ch_type = ChType::Array(Box::new(ch_type));
        }
        let mut buf = vec![0xaa];
        let result = write_binary_type_prefix(&mut buf, &ch_type);
        std::mem::forget(ch_type);
        assert!(matches!(result, Err(BinaryValueError::Invalid(reason))
            if reason == format!("nesting exceeds {MAX_TYPE_DEPTH}")));
        assert_eq!(buf, [0xaa]);
    }

    #[test]
    fn encode_nested_aliases_use_the_parser_depth_budget() {
        let mut ch_type = ChType::UInt8;
        let mut bytes = vec![13];
        for _ in 0..40 {
            ch_type = ChType::Nested(vec![("value".into(), ch_type)]);
            bytes.insert(0, 1);
        }
        assert_eq!(
            crate::native::type_parser::parse_ch_type(&ch_type.to_string()),
            Some(ch_type.clone())
        );
        let column = decode_binary_value(&ch_type, &bytes).unwrap();
        assert_eq!(encode_one(&ch_type, &column, 0), bytes);
    }

    #[test]
    fn encode_caps_zero_width_array_and_map_counts() {
        let count = MAX_ZERO_WIDTH_ELEMENTS + 1;
        let empty = || Column::Tuple(TupleColumn::new(vec![], count));
        let array = Column::Array(ArrayColumn::new(vec![0, count as i64], empty()));
        let map = Column::Map(MapColumn::new(
            vec![0, count as i64],
            Column::Tuple(TupleColumn::new(vec![empty(), empty()], count)),
        ));
        for (ch_type, column) in [
            (ChType::Array(Box::new(ChType::Tuple(vec![]))), array),
            (
                ChType::Map(
                    Box::new(ChType::Tuple(vec![])),
                    Box::new(ChType::Tuple(vec![])),
                ),
                map,
            ),
        ] {
            let mut buf = vec![0xaa];
            assert!(matches!(
                encode_binary_value(&mut buf, &ch_type, &column, 0),
                Err(BinaryValueError::Invalid(_))
            ));
            assert_eq!(buf, [0xaa]);
        }
    }

    #[test]
    fn encode_rejects_decimal_metadata_mismatches() {
        let ch_type = parse("Decimal(9, 2)");
        let bytes = (-314i32).to_le_bytes();
        let matching = Column::Decimal(DecimalColumn::new(bytes.to_vec(), 4, 9, 2));
        assert_eq!(encode_one(&ch_type, &matching, 0), bytes);
        let decoded = decode_binary_value(&ch_type, &bytes).unwrap();
        assert_eq!(encode_one(&ch_type, &decoded, 0), bytes);

        let mut rejected = Vec::new();
        for (precision, scale) in [(9, 4), (8, 2)] {
            let column = Column::Decimal(DecimalColumn::new(bytes.to_vec(), 4, precision, scale));
            let mut buf = vec![0xaa];
            let result = encode_binary_value(&mut buf, &ch_type, &column, 0);
            rejected.push((
                matches!(result, Err(BinaryValueError::Invalid(_))),
                buf == [0xaa],
            ));
        }
        assert_eq!(rejected, [(true, true); 2]);
    }

    #[test]
    fn encode_caps_nullable_zero_width_container_counts() {
        let count = MAX_ZERO_WIDTH_ELEMENTS + 1;
        let empty = || Column::Tuple(TupleColumn::new(vec![], count));
        let wrapped = || Column::Tuple(TupleColumn::new(vec![empty()], count));
        let array = |values| Column::Array(ArrayColumn::new(vec![0, count as i64], values));
        let mut rejected = Vec::new();
        for (ch_type, column) in [
            (parse("Array(Nullable(Tuple()))"), array(empty())),
            (parse("Array(Tuple(Nullable(Tuple())))"), array(wrapped())),
            (
                parse("Array(LowCardinality(Nullable(Tuple())))"),
                array(empty()),
            ),
            (
                parse("Array(SimpleAggregateFunction(anyLast, Nullable(Tuple())))"),
                array(empty()),
            ),
            (
                parse("Map(Tuple(), Nullable(Tuple()))"),
                Column::Map(MapColumn::new(
                    vec![0, count as i64],
                    Column::Tuple(TupleColumn::new(vec![empty(), empty()], count)),
                )),
            ),
            (parse("Nested(value Nullable(Tuple()))"), array(wrapped())),
        ] {
            let mut buf = vec![0xaa];
            let result = encode_binary_value(&mut buf, &ch_type, &column, 0);
            rejected.push((
                ch_type.to_string(),
                matches!(result, Err(BinaryValueError::Invalid(_))) && buf == [0xaa],
            ));
        }

        let nulls = Column::Nothing(NothingColumn::new_nullable(
            count,
            Bitmap::from_ch_null_map(&vec![1; count]),
        ));
        let mut buf = vec![0xaa];
        encode_binary_value(
            &mut buf,
            &parse("Array(Nullable(Nothing))"),
            &array(nulls),
            0,
        )
        .unwrap();
        let mut expected = vec![0xaa];
        expected.extend(varint(count as u64));
        expected.resize(expected.len() + count, 1);
        assert_eq!(buf, expected);
        assert!(rejected.iter().all(|(_, ok)| *ok), "{rejected:?}");
    }

    #[test]
    fn encode_rejects_zero_width_scalar_descriptors() {
        let fixed = Column::FixedBinary(FixedBinaryColumn::new(vec![], 0));
        let qbit = Column::QBit(QBitColumn::new(
            Column::Float32(PrimitiveColumn::new(vec![])),
            0,
        ));
        let qbit_type = ChType::QBit {
            element_type: QBitElementType::Float32,
            dimension: 0,
        };
        let mut rejected = Vec::new();
        for (ch_type, column) in [(ChType::FixedString(0), fixed), (qbit_type, qbit)] {
            for row in [0, usize::MAX] {
                let mut buf = vec![0xaa];
                let result = encode_binary_value(&mut buf, &ch_type, &column, row);
                rejected.push((
                    matches!(result, Err(BinaryValueError::Invalid(_))),
                    buf == [0xaa],
                ));
            }
            let array_type = ChType::Array(Box::new(ch_type));
            for end in [0, 1] {
                let array = Column::Array(ArrayColumn::new(vec![0, end], column.clone()));
                let mut buf = vec![0xaa];
                let result = encode_binary_value(&mut buf, &array_type, &array, 0);
                rejected.push((
                    matches!(result, Err(BinaryValueError::Invalid(_))),
                    buf == [0xaa],
                ));
            }
        }
        assert_eq!(rejected, [(true, true); 8]);
    }

    #[test]
    fn encode_rejects_bad_input_and_leaves_buf_unchanged() {
        let ints = Column::Int32(PrimitiveColumn::new(vec![13]));
        let strings = Column::Utf8(Utf8Column::new(vec![0, 4], b"oops".to_vec()));
        let check = |ch_type: &ChType, column: &Column, row: usize, unsupported: bool| {
            let mut buf = vec![0xaa];
            let err = encode_binary_value(&mut buf, ch_type, column, row).unwrap_err();
            assert_eq!(buf, vec![0xaa], "{ch_type}: {err}");
            assert_eq!(
                matches!(err, BinaryValueError::Unsupported(_)),
                unsupported,
                "{ch_type}: {err}"
            );
        };
        // Row out of range, including a Nullable whose bitmap is too short.
        check(&ChType::Int32, &ints, 1, false);
        check(&parse("Nullable(Int32)"), &ints, 1, false);
        // Column shape that does not match the type.
        check(&ChType::Int64, &ints, 0, false);
        check(&ChType::String, &ints, 0, false);
        check(&parse("FixedString(3)"), &strings, 0, false);
        check(&parse("Array(Int32)"), &ints, 0, false);
        // An error deep inside a container still restores buf.
        let nested = Column::Array(ArrayColumn::new(vec![0, 2], ints.clone()));
        check(&parse("Array(Int32)"), &nested, 0, false);
        // Corrupt offsets.
        let backwards = Column::Array(ArrayColumn::new(vec![1, 0], ints.clone()));
        check(&parse("Array(Int32)"), &backwards, 0, false);
        let bad_string = Column::Utf8(Utf8Column::new(vec![0, 9], b"short".to_vec()));
        check(&ChType::String, &bad_string, 0, false);
        let bad_index = Column::Dictionary(crate::column::DictionaryColumn::new(
            vec![-1],
            strings.clone(),
        ));
        check(&parse("LowCardinality(String)"), &bad_index, 0, false);
        // Types with no single-value encoding, as on the decode side.
        check(&parse("Variant(String, UInt64)"), &ints, 0, true);
        check(&parse("Dynamic"), &ints, 0, true);
        check(
            &ChType::Nothing,
            &Column::Nothing(NothingColumn::new(1)),
            0,
            false,
        );
        check(
            &parse("Nullable(Nothing)"),
            &Column::Nothing(NothingColumn::new_nullable(1, Bitmap::all_valid(1))),
            0,
            false,
        );
    }
}
