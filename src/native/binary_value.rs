//! Single-value ClickHouse binary decoding for Dynamic `SharedVariant` cells.
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

use std::io;

use crate::bitmap::Bitmap;
use crate::column::{
    ArrayColumn, BoolColumn, Column, DecimalColumn, FixedBinaryColumn, MapColumn, NothingColumn,
    PrimitiveColumn, QBitColumn, TupleColumn, Utf8Column,
};
use crate::native::type_binary::{read_binary_type, BinaryTypeError};
use crate::native::varint::ByteReader;
use crate::schema::ChType;

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

/// Errors from single-value binary decoding.
#[derive(Debug)]
pub enum BinaryValueError {
    /// The value violates the decoding contract: truncated or trailing bytes,
    /// a descriptor mismatch, an out-of-range count, an illegal type shape, or
    /// a decoder resource limit.
    Invalid(String),
    /// The type has no defined or implemented single-value decoding.
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
    if let Some(delegate) = ch_type.physical_delegate_ref() {
        return has_zero_width_values(delegate.as_ref());
    }
    match ch_type {
        ChType::Nothing => true,
        ChType::LowCardinality(inner) => has_zero_width_values(inner),
        ChType::Tuple(elements) => elements.iter().all(|(_, t)| has_zero_width_values(t)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::type_binary::write_binary_type;
    use crate::native::varint::write_varint;
    use crate::schema::{GeoKind, QBitElementType};

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
}
