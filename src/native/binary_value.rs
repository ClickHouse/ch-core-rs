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
//! `Variant`, `Dynamic`, and `AggregateFunction` values (an opaque
//! per-function state) have no decoding here and report `Unsupported`, as do
//! descriptors the crate does not parse (JSON), so a caller can keep
//! those cells as raw bytes. Truncated payloads and trailing bytes after the
//! value are `Invalid`.
//!
//! [`write_binary_type_prefix`] and [`encode_binary_value`] are the inverses:
//! they write the descriptor and one value of a decoded column, so a binding
//! can build SharedVariant cells and JSON shared data for `encode_block`.

use std::io;

use crate::bitmap::Bitmap;
use crate::column::{
    ArrayColumn, BoolColumn, Column, DecimalColumn, FixedBinaryColumn, MapColumn, NothingColumn,
    PrimitiveColumn, QBitColumn, TupleColumn, Utf8Column,
};
use crate::native::protocol::MAX_TYPE_DEPTH;
use crate::native::type_binary::{read_binary_type, write_binary_type, BinaryTypeError};
use crate::native::varint::{write_varint, ByteReader};
use crate::schema::{ChType, QBitElementType, GEOMETRY_EXPANSION_DEPTH};

/// Cap for element counts whose element type consumes zero payload bytes
/// (`Tuple()`, `Nothing`); every other count is bounded by the bytes present.
const MAX_ZERO_WIDTH_ELEMENTS: usize = 1_000_000;

/// Maximum cumulative logical buffer bytes materialized for null placeholders
/// while decoding one single value.
///
/// A Nullable single value carries only its one-byte null flag, but Arrow-shaped
/// output still needs the inner type's placeholder buffers. Keep the cap at 16
/// MiB so one maximum-width `FixedString(0x00ff_ffff)` remains representable,
/// while nested containers cannot amplify a few null flags into unbounded QBit
/// or FixedString allocations.
const MAX_NULL_DEFAULT_BYTES: usize = 1 << 24;

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
/// server type (JSON, Set, Function) is `Unsupported`; a malformed
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
pub fn decode_binary_value(ch_type: &ChType, bytes: &[u8]) -> Result<Column, BinaryValueError> {
    let mut builder = ValueBuilder::new(ch_type)?;
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
    },
    Map {
        offsets: Vec<i64>,
        keys: Box<ValueBuilder>,
        values: Box<ValueBuilder>,
        zero_width: bool,
    },
    Tuple {
        fields: Vec<ValueBuilder>,
        rows: usize,
    },
}

impl ValueBuilder {
    fn new(ch_type: &ChType) -> Result<Self, BinaryValueError> {
        if let Some(delegate) = ch_type.physical_delegate_ref() {
            return Self::new(delegate.as_ref());
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
            ChType::QBit {
                element_type,
                dimension,
            } => Self::QBit {
                values: Box::new(Self::new(&element_type.ch_type())?),
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
                inner: Box::new(Self::new(inner)?),
            },
            // A LowCardinality single value carries no dictionary framing; the
            // decoded column is the plain inner column.
            ChType::LowCardinality(inner) => Self::new(inner)?,
            ChType::Array(inner) => Self::Array {
                offsets: vec![0],
                elements: Box::new(Self::new(inner)?),
                zero_width: has_zero_width_values(inner),
            },
            ChType::Map(key, value) => Self::Map {
                offsets: vec![0],
                keys: Box::new(Self::new(key)?),
                values: Box::new(Self::new(value)?),
                zero_width: has_zero_width_values(key) && has_zero_width_values(value),
            },
            ChType::Tuple(elements) => Self::Tuple {
                fields: elements
                    .iter()
                    .map(|(_, t)| Self::new(t))
                    .collect::<Result<_, _>>()?,
                rows: 0,
            },
            // JSON values are not materialized here: shared-data and
            // SharedVariant cells stay opaque, so a JSON-typed binary value is
            // reported unsupported rather than partially decoded.
            ChType::Variant(_)
            | ChType::Dynamic { .. }
            | ChType::Json { .. }
            | ChType::AggregateFunction { .. } => {
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

    /// Append one value from the reader.
    fn append_value(
        &mut self,
        reader: &mut ByteReader,
        null_budget: &mut NullDefaultBudget,
    ) -> Result<(), BinaryValueError> {
        match self {
            // The server cannot serialize a Nothing value; only null rows
            // (which take the default path) are representable.
            ValueBuilder::Nothing { .. } => {
                return Err(BinaryValueError::Unsupported("Nothing".into()))
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
            } => {
                let count = read_element_count(reader, *zero_width, "Array element")?;
                for _ in 0..count {
                    elements.append_value(reader, null_budget)?;
                }
                push_end_offset(offsets, count)?;
            }
            ValueBuilder::Map {
                offsets,
                keys,
                values,
                zero_width,
            } => {
                let count = read_element_count(reader, *zero_width, "Map entry")?;
                for _ in 0..count {
                    keys.append_value(reader, null_budget)?;
                    values.append_value(reader, null_budget)?;
                }
                push_end_offset(offsets, count)?;
            }
            ValueBuilder::Tuple { fields, rows } => {
                for field in fields.iter_mut() {
                    field.append_value(reader, null_budget)?;
                }
                *rows += 1;
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
        match self {
            ValueBuilder::Nothing { rows } => *rows += 1,
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
                for field in fields.iter_mut() {
                    field.append_default(null_budget)?;
                }
                *rows += 1;
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
/// `Variant`, `Dynamic`, `JSON`, `AggregateFunction`, and non-null `Nothing`
/// values are `Unsupported`, matching the decoder. A row out of range or a
/// column whose shape does not match `ch_type`, or a null row under a
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
    let result = encode_value(buf, ch_type, column, row);
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
}

impl TypeCheck {
    #[inline]
    fn combine(&mut self, other: Self) {
        self.supported &= other.supported;
        self.matches &= other.matches;
    }
}

/// Check depth, coverage, and optional column shape in one allocation-free
/// traversal. None means too deep. Every type child is visited even if its
/// column shape is already wrong, before any error can render the type.
/// Depth charges match `encode::validate::type_depth`, including empty aliases.
///
/// Single-child wrappers (`Nullable`, `LowCardinality`, `Array`, and the
/// `SimpleAggregateFunction` alias) are walked in a loop, so a chain like
/// `Array(LowCardinality(Nullable(String)))` costs one frame and a hostile
/// million-deep chain is rejected without recursion. Only branching nodes
/// (`Tuple`, `Map`, `Nested`, and the unsupported families) recurse, and the
/// depth gate bounds that recursion.
#[inline]
fn check_type(
    mut ch_type: &ChType,
    mut column: Option<&Column>,
    mut depth: usize,
) -> Option<TypeCheck> {
    let mut check = TypeCheck {
        supported: true,
        matches: true,
    };
    loop {
        if depth > MAX_TYPE_DEPTH {
            return None;
        }
        match ch_type {
            ChType::Nullable(inner) | ChType::SimpleAggregateFunction { inner, .. } => {
                ch_type = inner;
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
                    check_type(ty, None, depth + 1)?;
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
                for (_, ty) in typed_paths {
                    check_type(ty, None, depth + 1)?;
                }
                check.supported = false;
                return Some(check);
            }
            ChType::Dynamic { .. } => {
                check.supported = false;
                return Some(check);
            }
            _ => {
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
    BinaryValueError::Invalid("type nesting too deep".into())
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

fn fixed_slice(data: &[u8], width: usize, row: usize) -> Result<&[u8], BinaryValueError> {
    row.checked_mul(width)
        .and_then(|start| data.get(start..start.checked_add(width)?))
        .ok_or_else(|| out_of_range(row, data.len().checked_div(width).unwrap_or(0)))
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
) -> Result<(), BinaryValueError> {
    if elements.len() != c.fields.len() {
        return Err(mismatch(ch_type));
    }
    reject_null(c.validity.as_ref(), row)?;
    if row >= c.len {
        return Err(out_of_range(row, c.len));
    }
    for (element, field) in elements.zip(&c.fields) {
        encode_value(buf, element, field, row)?;
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
) -> Result<(), BinaryValueError> {
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
            encode_value(buf, inner, column, row)?
        }
        (ChType::Geo(kind), _) => encode_value(buf, kind.underlying_type_ref(), column, row)?,

        // Mirror the bulk null map: 0x00 then the value, or 0x01 and nothing.
        (ChType::Nullable(inner), _) => {
            if is_null(column.validity(), row)? {
                if row >= column.len() {
                    return Err(out_of_range(row, column.len()));
                }
                buf.push(1);
            } else {
                buf.push(0);
                encode_value(buf, inner, column, row)?;
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
            encode_value(buf, value_type, &dict.values, index)?;
        }
        // The plain inner column, as decode_binary_value returns it.
        (ChType::LowCardinality(inner), _) => encode_value(buf, inner, column, row)?,

        (ChType::Nothing, Column::Nothing(c)) => {
            reject_null(c.validity.as_ref(), row)?;
            return Err(BinaryValueError::Unsupported("Nothing".into()));
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
            for i in start..end {
                encode_value(buf, inner, &c.values, i)?;
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
            for i in start..end {
                encode_tuple(buf, ch_type, fields.iter().map(|(_, ty)| ty), tuple, i)?;
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
            for i in start..end {
                encode_value(buf, key, keys, i)?;
                encode_value(buf, value, values, i)?;
            }
        }
        (ChType::Tuple(elements), Column::Tuple(c)) => {
            encode_tuple(buf, ch_type, elements.iter().map(|(_, ty)| ty), c, row)?
        }
        (
            ChType::Variant(_)
            | ChType::Dynamic { .. }
            | ChType::Json { .. }
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
            Err(BinaryValueError::Unsupported(_))
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
        // A JSON descriptor now parses (type_binary supports tag 0x30), so the
        // prefix reader accepts it, but a JSON value is never materialized as a
        // shared cell: decode_binary_value stays Unsupported rather than panicking.
        let json_type = parse("JSON");
        let mut json_bytes = Vec::new();
        write_binary_type(&mut json_bytes, &json_type);
        let (parsed, consumed) = read_binary_type_prefix(&json_bytes).unwrap();
        assert_eq!(parsed, json_type);
        assert_eq!(consumed, json_bytes.len());
        assert!(matches!(
            decode_binary_value(&json_type, &[]),
            Err(BinaryValueError::Unsupported(_))
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
                if reason == "type nesting too deep"));
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
            matches!(result, Err(BinaryValueError::Invalid(reason)) if reason == "type nesting too deep")
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
            if reason == "type nesting too deep"));
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
            true,
        );
        check(
            &parse("Nullable(Nothing)"),
            &Column::Nothing(NothingColumn::new_nullable(1, Bitmap::all_valid(1))),
            0,
            true,
        );
    }
}
