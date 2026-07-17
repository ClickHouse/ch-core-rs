//! ClickHouse binary data-type descriptors used by Dynamic shared values and
//! the opt-in Native binary-type-header setting.
//!
//! The tag grammar mirrors `DataTypesBinaryEncoding` at
//! `v26.6.1.1193-stable`. Parsing is complexity- and depth-bounded because the
//! descriptors are untrusted recursive input. Unsupported server types are
//! reported explicitly instead of being guessed or partially consumed.

use std::io;

use crate::native::aggregate_function::aggregate_state_codec;
use crate::native::protocol::MAX_TYPE_DEPTH;
use crate::native::type_parser::{normalize_variant_alternatives, parse_ch_type};
use crate::native::varint::{write_varint, ByteReader};
use crate::schema::{
    ChType, IntervalKind, QBitElementType, JSON_MAX_DYNAMIC_PATHS, JSON_MAX_DYNAMIC_TYPES,
    JSON_MAX_TYPED_PATHS, QBIT_MAX_DIMENSION,
};

const MAX_BINARY_TYPE_COMPLEXITY: usize = 1_000;
const MAX_BINARY_TYPE_LIST: usize = 1_000_000;

#[derive(Debug)]
pub(crate) enum BinaryTypeError {
    Io(io::Error),
    Invalid(String),
    Unsupported(String),
}

impl From<io::Error> for BinaryTypeError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl std::fmt::Display for BinaryTypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinaryTypeError::Io(err) => write!(f, "{err}"),
            BinaryTypeError::Invalid(reason) => {
                write!(f, "invalid binary type descriptor: {reason}")
            }
            BinaryTypeError::Unsupported(reason) => {
                write!(f, "unsupported binary type descriptor: {reason}")
            }
        }
    }
}

pub(crate) fn read_binary_type(reader: &mut ByteReader) -> Result<ChType, BinaryTypeError> {
    let mut complexity = 0usize;
    let ch_type = read_binary_type_inner(reader, 0, &mut complexity)?;

    // Structural tags bypass the textual parser that normally enforces server
    // constructor rules. Round-tripping the canonical spelling through that
    // parser applies the same Nullable/container/Variant legality checks and
    // rejects malformed binary headers before zero-row `empty_column` or a body
    // decoder can reach an impossible arm.
    let canonical = ch_type.to_string();
    match parse_ch_type(&canonical) {
        Some(parsed) if parsed == ch_type => Ok(ch_type),
        _ => Err(BinaryTypeError::Invalid(format!(
            "type {canonical} violates ClickHouse type-construction rules"
        ))),
    }
}

fn read_binary_type_inner(
    reader: &mut ByteReader,
    depth: usize,
    complexity: &mut usize,
) -> Result<ChType, BinaryTypeError> {
    if depth > MAX_TYPE_DEPTH {
        return Err(BinaryTypeError::Invalid(format!(
            "nesting exceeds {MAX_TYPE_DEPTH}"
        )));
    }
    *complexity = complexity
        .checked_add(1)
        .ok_or_else(|| BinaryTypeError::Invalid("complexity overflow".into()))?;
    if *complexity > MAX_BINARY_TYPE_COMPLEXITY {
        return Err(BinaryTypeError::Invalid(format!(
            "complexity exceeds {MAX_BINARY_TYPE_COMPLEXITY}"
        )));
    }

    let tag = reader.read_u8()?;
    let ty =
        match tag {
            0x00 => ChType::Nothing,
            0x01 => ChType::UInt8,
            0x02 => ChType::UInt16,
            0x03 => ChType::UInt32,
            0x04 => ChType::UInt64,
            0x05 => ChType::UInt128,
            0x06 => ChType::UInt256,
            0x07 => ChType::Int8,
            0x08 => ChType::Int16,
            0x09 => ChType::Int32,
            0x0a => ChType::Int64,
            0x0b => ChType::Int128,
            0x0c => ChType::Int256,
            0x0d => ChType::Float32,
            0x0e => ChType::Float64,
            0x0f => ChType::Date,
            0x10 => ChType::Date32,
            0x11 => ChType::DateTime { timezone: None },
            0x12 => ChType::DateTime {
                timezone: Some(reader.read_varint_string()?),
            },
            0x13 => ChType::DateTime64 {
                precision: read_precision(reader, "DateTime64")?,
                timezone: None,
            },
            0x14 => ChType::DateTime64 {
                precision: read_precision(reader, "DateTime64")?,
                timezone: Some(reader.read_varint_string()?),
            },
            0x15 => ChType::String,
            0x16 => {
                let width = usize_from_varint(reader.read_varint()?, "FixedString width")?;
                if !(1..=0x00ff_ffff).contains(&width) {
                    return Err(BinaryTypeError::Invalid(format!(
                        "FixedString width {width} is outside 1..=16777215"
                    )));
                }
                ChType::FixedString(width)
            }
            0x17 => ChType::Enum8 {
                variants: read_enum8(reader)?,
            },
            0x18 => ChType::Enum16 {
                variants: read_enum16(reader)?,
            },
            0x19..=0x1c => read_decimal(reader, tag)?,
            0x1d => ChType::Uuid,
            0x1e => ChType::Array(Box::new(read_binary_type_inner(
                reader,
                depth + 1,
                complexity,
            )?)),
            0x1f => ChType::Tuple(read_tuple(reader, depth, complexity, false)?),
            0x20 => ChType::Tuple(read_tuple(reader, depth, complexity, true)?),
            0x21 => return Err(BinaryTypeError::Unsupported("Set".into())),
            0x22 => ChType::Interval(read_interval(reader.read_u8()?)?),
            0x23 => ChType::Nullable(Box::new(read_binary_type_inner(
                reader,
                depth + 1,
                complexity,
            )?)),
            0x24 => return Err(BinaryTypeError::Unsupported("Function".into())),
            0x25 => read_aggregate_function(reader, depth, complexity)?,
            0x26 => ChType::LowCardinality(Box::new(read_binary_type_inner(
                reader,
                depth + 1,
                complexity,
            )?)),
            0x27 => ChType::Map(
                Box::new(read_binary_type_inner(reader, depth + 1, complexity)?),
                Box::new(read_binary_type_inner(reader, depth + 1, complexity)?),
            ),
            0x28 => ChType::Ipv4,
            0x29 => ChType::Ipv6,
            0x2a => {
                let count = read_count(reader, "Variant alternatives")?;
                ensure_type_budget(count, *complexity, "Variant alternatives")?;
                let mut alternatives = Vec::with_capacity(reader.capacity_for(count, 1));
                for _ in 0..count {
                    alternatives.push(read_binary_type_inner(reader, depth + 1, complexity)?);
                }
                ChType::Variant(normalize_variant_alternatives(alternatives).ok_or_else(|| {
                    BinaryTypeError::Invalid("invalid Variant alternatives".into())
                })?)
            }
            0x2b => {
                let max_types = reader.read_u8()?;
                if max_types > 254 {
                    return Err(BinaryTypeError::Invalid(format!(
                        "Dynamic max_types {max_types} exceeds 254"
                    )));
                }
                ChType::Dynamic { max_types }
            }
            0x2c => {
                let name = reader.read_varint_string()?;
                // ClickHouse's generic Custom decoder hands the name to a fresh
                // DataTypeFactory text parse; it does not propagate the outer
                // binary descriptor depth. Match that boundary here. The public
                // `read_binary_type` still renders and reparses the complete
                // constructed type afterward, applying this crate's aggregate
                // MAX_TYPE_DEPTH safety cap before any body traversal.
                parse_ch_type(&name).ok_or(BinaryTypeError::Unsupported(name))?
            }
            0x2d => ChType::Bool,
            0x2e => read_simple_aggregate_function(reader, depth, complexity)?,
            0x2f => ChType::Nested(read_nested(reader, depth, complexity)?),
            0x30 => read_json(reader, depth, complexity)?,
            0x31 => ChType::BFloat16,
            0x32 => ChType::Time,
            0x33 | 0x35 => {
                return Err(BinaryTypeError::Invalid(format!(
                    "reserved removed type tag 0x{tag:02x}"
                )))
            }
            0x34 => ChType::Time64 {
                precision: read_precision(reader, "Time64")?,
            },
            0x36 => {
                // T is a scalar parameter, not a structural child: textual
                // parsing and encode depth validation both treat QBit as a
                // leaf. Read exactly one legal scalar descriptor here so a
                // QBit at MAX_TYPE_DEPTH remains valid without allowing a
                // hostile chain of nested QBit descriptors to evade the depth
                // cap. The child still consumes one complexity-budget node.
                let element_type = read_qbit_element_type(reader, complexity)?;
                let dimension = usize_from_varint(reader.read_varint()?, "QBit dimension")?;
                if !(1..=QBIT_MAX_DIMENSION).contains(&dimension) {
                    return Err(BinaryTypeError::Invalid(format!(
                        "QBit dimension {dimension} is outside 1..={QBIT_MAX_DIMENSION}"
                    )));
                }
                ChType::QBit {
                    element_type,
                    dimension,
                }
            }
            _ => return Err(BinaryTypeError::Invalid(format!("unknown tag 0x{tag:02x}"))),
        };
    Ok(ty)
}

fn read_qbit_element_type(
    reader: &mut ByteReader,
    complexity: &mut usize,
) -> Result<QBitElementType, BinaryTypeError> {
    *complexity = complexity
        .checked_add(1)
        .ok_or_else(|| BinaryTypeError::Invalid("complexity overflow".into()))?;
    if *complexity > MAX_BINARY_TYPE_COMPLEXITY {
        return Err(BinaryTypeError::Invalid(format!(
            "complexity exceeds {MAX_BINARY_TYPE_COMPLEXITY}"
        )));
    }

    match reader.read_u8()? {
        0x31 => Ok(QBitElementType::BFloat16),
        0x0d => Ok(QBitElementType::Float32),
        0x0e => Ok(QBitElementType::Float64),
        // Preserve the generic Custom descriptor behavior without recursively
        // parsing an arbitrary structural child. The server can resolve a
        // custom canonical scalar name before QBit validates T.
        0x2c => match parse_ch_type(&reader.read_varint_string()?) {
            Some(ChType::BFloat16) => Ok(QBitElementType::BFloat16),
            Some(ChType::Float32) => Ok(QBitElementType::Float32),
            Some(ChType::Float64) => Ok(QBitElementType::Float64),
            _ => Err(BinaryTypeError::Invalid(
                "QBit Custom element is not BFloat16, Float32, or Float64".into(),
            )),
        },
        tag => Err(BinaryTypeError::Invalid(format!(
            "QBit element descriptor tag 0x{tag:02x} is not BFloat16, Float32, or Float64"
        ))),
    }
}

fn read_precision(reader: &mut ByteReader, type_name: &str) -> Result<u8, BinaryTypeError> {
    let precision = reader.read_u8()?;
    if precision > 9 {
        return Err(BinaryTypeError::Invalid(format!(
            "{type_name} precision {precision} exceeds 9"
        )));
    }
    Ok(precision)
}

fn read_count(reader: &mut ByteReader, what: &str) -> Result<usize, BinaryTypeError> {
    let count = usize_from_varint(reader.read_varint()?, what)?;
    if count > MAX_BINARY_TYPE_LIST {
        return Err(BinaryTypeError::Invalid(format!(
            "{what} count {count} exceeds {MAX_BINARY_TYPE_LIST}"
        )));
    }
    Ok(count)
}

fn ensure_type_budget(count: usize, complexity: usize, what: &str) -> Result<(), BinaryTypeError> {
    let remaining = MAX_BINARY_TYPE_COMPLEXITY.saturating_sub(complexity);
    if count > remaining {
        return Err(BinaryTypeError::Invalid(format!(
            "{what} count {count} exceeds remaining type complexity {remaining}"
        )));
    }
    Ok(())
}

fn usize_from_varint(value: u64, what: &str) -> Result<usize, BinaryTypeError> {
    usize::try_from(value).map_err(|_| BinaryTypeError::Invalid(format!("{what} overflows usize")))
}

fn read_enum8(reader: &mut ByteReader) -> Result<Vec<(String, i8)>, BinaryTypeError> {
    let count = read_count(reader, "Enum8 variants")?;
    let mut variants = Vec::with_capacity(reader.capacity_for(count, 2));
    for _ in 0..count {
        let name = reader.read_varint_string()?;
        let value = reader.read_u8()? as i8;
        variants.push((name, value));
    }
    Ok(variants)
}

fn read_enum16(reader: &mut ByteReader) -> Result<Vec<(String, i16)>, BinaryTypeError> {
    let count = read_count(reader, "Enum16 variants")?;
    let mut variants = Vec::with_capacity(reader.capacity_for(count, 3));
    for _ in 0..count {
        let name = reader.read_varint_string()?;
        let bytes = reader.read_slice(2)?;
        variants.push((name, i16::from_le_bytes([bytes[0], bytes[1]])));
    }
    Ok(variants)
}

fn read_decimal(reader: &mut ByteReader, tag: u8) -> Result<ChType, BinaryTypeError> {
    let precision = reader.read_u8()?;
    let scale = reader.read_u8()?;
    let (bits, max_precision) = match tag {
        0x19 => (32, 9),
        0x1a => (64, 18),
        0x1b => (128, 38),
        0x1c => (256, 76),
        _ => unreachable!("caller restricts decimal tags"),
    };
    if precision == 0 || precision > max_precision || scale > precision {
        return Err(BinaryTypeError::Invalid(format!(
            "Decimal{bits} precision/scale ({precision}, {scale}) is invalid"
        )));
    }
    Ok(ChType::Decimal {
        precision,
        scale,
        bits,
    })
}

fn read_tuple(
    reader: &mut ByteReader,
    depth: usize,
    complexity: &mut usize,
    named: bool,
) -> Result<Vec<(Option<String>, ChType)>, BinaryTypeError> {
    let count = read_count(reader, "Tuple elements")?;
    ensure_type_budget(count, *complexity, "Tuple elements")?;
    let mut elements = Vec::with_capacity(reader.capacity_for(count, 1));
    for _ in 0..count {
        let name = if named {
            Some(reader.read_varint_string()?)
        } else {
            None
        };
        let ch_type = read_binary_type_inner(reader, depth + 1, complexity)?;
        elements.push((name, ch_type));
    }
    Ok(elements)
}

fn read_nested(
    reader: &mut ByteReader,
    depth: usize,
    complexity: &mut usize,
) -> Result<Vec<(String, ChType)>, BinaryTypeError> {
    let count = read_count(reader, "Nested fields")?;
    ensure_type_budget(count, *complexity, "Nested fields")?;
    let mut fields = Vec::with_capacity(reader.capacity_for(count, 2));
    for _ in 0..count {
        let name = reader.read_varint_string()?;
        let ch_type = read_binary_type_inner(reader, depth + 2, complexity)?;
        fields.push((name, ch_type));
    }
    Ok(fields)
}

/// Read a `JSON` binary type descriptor (tag 0x30, `DataTypesBinaryEncoding`,
/// confirmed at v26.6.1.1193-stable): a `TYPE_JSON_SERIALIZATION_VERSION` byte
/// (currently 0; a nonzero value throws `INCORRECT_DATA` server-side, rejected
/// here), a VarUInt `max_dynamic_paths` (<= 10000), a raw u8 `max_dynamic_types`
/// (NOT a varint; <= 254), then a VarUInt-counted list of typed paths (each a
/// varint string plus a recursive type descriptor, <= 1000), a VarUInt-counted
/// list of skip paths, and a VarUInt-counted list of skip regexps.
///
/// The server iterates an `unordered_map`/`set` here, so paths arrive in
/// arbitrary order. This canonicalizes into the sorted [`ChType::Json`] form
/// (duplicate typed paths collapse, last wins; duplicate skip entries collapse)
/// so the outer round-trip through `parse_ch_type` accepts it, matching how the
/// server accepts any order on its own read side.
fn read_json(
    reader: &mut ByteReader,
    depth: usize,
    complexity: &mut usize,
) -> Result<ChType, BinaryTypeError> {
    let version = reader.read_u8()?;
    if version != 0 {
        return Err(BinaryTypeError::Invalid(format!(
            "JSON serialization version {version} exceeds 0"
        )));
    }
    let max_dynamic_paths = usize_from_varint(reader.read_varint()?, "JSON max_dynamic_paths")?;
    if max_dynamic_paths > JSON_MAX_DYNAMIC_PATHS as usize {
        return Err(BinaryTypeError::Invalid(format!(
            "JSON max_dynamic_paths {max_dynamic_paths} exceeds {JSON_MAX_DYNAMIC_PATHS}"
        )));
    }
    let max_dynamic_types = reader.read_u8()?;
    if max_dynamic_types > JSON_MAX_DYNAMIC_TYPES {
        return Err(BinaryTypeError::Invalid(format!(
            "JSON max_dynamic_types {max_dynamic_types} exceeds {JSON_MAX_DYNAMIC_TYPES}"
        )));
    }

    let typed_count = usize_from_varint(reader.read_varint()?, "JSON typed path count")?;
    if typed_count > JSON_MAX_TYPED_PATHS {
        return Err(BinaryTypeError::Invalid(format!(
            "JSON typed path count {typed_count} exceeds {JSON_MAX_TYPED_PATHS}"
        )));
    }
    ensure_type_budget(typed_count, *complexity, "JSON typed paths")?;
    let mut typed = std::collections::BTreeMap::<String, ChType>::new();
    for _ in 0..typed_count {
        let path = reader.read_varint_string()?;
        let ch_type = read_binary_type_inner(reader, depth + 1, complexity)?;
        typed.insert(path, ch_type);
    }

    let skip_count = read_count(reader, "JSON skip paths")?;
    let mut skip_paths = std::collections::BTreeSet::<String>::new();
    for _ in 0..skip_count {
        skip_paths.insert(reader.read_varint_string()?);
    }

    let regexp_count = read_count(reader, "JSON skip regexps")?;
    let mut skip_regexps = std::collections::BTreeSet::<String>::new();
    for _ in 0..regexp_count {
        skip_regexps.insert(reader.read_varint_string()?);
    }

    Ok(ChType::Json {
        max_dynamic_paths: max_dynamic_paths as u32,
        max_dynamic_types,
        typed_paths: typed.into_iter().collect(),
        skip_paths: skip_paths.into_iter().collect(),
        skip_regexps: skip_regexps.into_iter().collect(),
    })
}

fn read_interval(kind: u8) -> Result<IntervalKind, BinaryTypeError> {
    match kind {
        0x00 => Ok(IntervalKind::Nanosecond),
        0x01 => Ok(IntervalKind::Microsecond),
        0x02 => Ok(IntervalKind::Millisecond),
        0x03 => Ok(IntervalKind::Second),
        0x04 => Ok(IntervalKind::Minute),
        0x05 => Ok(IntervalKind::Hour),
        0x06 => Ok(IntervalKind::Day),
        0x07 => Ok(IntervalKind::Week),
        0x08 => Ok(IntervalKind::Month),
        0x09 => Ok(IntervalKind::Quarter),
        0x0a => Ok(IntervalKind::Year),
        _ => Err(BinaryTypeError::Invalid(format!(
            "Interval kind {kind} exceeds 10"
        ))),
    }
}

fn read_aggregate_function(
    reader: &mut ByteReader,
    depth: usize,
    complexity: &mut usize,
) -> Result<ChType, BinaryTypeError> {
    // The version VarUInt selects the runtime state layout only (functions
    // with `IAggregateFunction::getDefaultVersion` overrides, e.g. sumMap and
    // groupBitmap at v26.3, emit 1, and combinators propagate the nested
    // version); the descriptor's parameter/argument grammar is identical for
    // every version. A function whose state this crate parses has a v0-layout
    // codec registered in `aggregate_function.rs`, so a nonzero version there
    // is unsupported rather than misframed. A function without a codec is not
    // framed by this crate, so its version passes through unchecked and its
    // support is decided downstream.
    let version = reader.read_varint()?;
    let (function, arguments) = read_aggregate_signature(reader, depth, complexity)?;
    let ch_type = ChType::AggregateFunction {
        function,
        arguments,
    };
    if version != 0 && aggregate_state_codec(&ch_type).is_some() {
        return Err(BinaryTypeError::Unsupported(format!(
            "{ch_type} with state version {version}"
        )));
    }
    Ok(ch_type)
}

fn read_simple_aggregate_function(
    reader: &mut ByteReader,
    depth: usize,
    complexity: &mut usize,
) -> Result<ChType, BinaryTypeError> {
    let (func, mut arguments) = read_aggregate_signature(reader, depth, complexity)?;
    if arguments.len() != 1 {
        return Err(BinaryTypeError::Unsupported(format!(
            "SimpleAggregateFunction({func}) with {} arguments",
            arguments.len()
        )));
    }
    Ok(ChType::SimpleAggregateFunction {
        func,
        inner: Box::new(arguments.remove(0)),
    })
}

fn read_aggregate_signature(
    reader: &mut ByteReader,
    depth: usize,
    complexity: &mut usize,
) -> Result<(String, Vec<ChType>), BinaryTypeError> {
    let mut function = reader.read_varint_string()?;
    let parameter_count = read_count(reader, "aggregate parameters")?;
    ensure_type_budget(parameter_count, *complexity, "aggregate parameters")?;
    if parameter_count > 0 {
        let mut parameters = Vec::with_capacity(reader.capacity_for(parameter_count, 1));
        for _ in 0..parameter_count {
            parameters.push(read_field_literal(reader, depth + 1, complexity)?);
        }
        function.push('(');
        function.push_str(&parameters.join(", "));
        function.push(')');
    }
    let argument_count = read_count(reader, "aggregate arguments")?;
    ensure_type_budget(argument_count, *complexity, "aggregate arguments")?;
    let mut arguments = Vec::with_capacity(reader.capacity_for(argument_count, 1));
    for _ in 0..argument_count {
        arguments.push(read_binary_type_inner(reader, depth + 1, complexity)?);
    }
    Ok((function, arguments))
}

fn read_field_literal(
    reader: &mut ByteReader,
    depth: usize,
    complexity: &mut usize,
) -> Result<String, BinaryTypeError> {
    // This covers the Field tags needed by the AggregateFunction state codecs
    // registered in this crate and by valid SimpleAggregateFunction types at
    // the pinned server. Wider integer/decimal/map/object/state Field tags are
    // rejected as unsupported until a decodable aggregate type needs them.
    if depth > MAX_TYPE_DEPTH {
        return Err(BinaryTypeError::Invalid(format!(
            "aggregate parameter nesting exceeds {MAX_TYPE_DEPTH}"
        )));
    }
    *complexity = complexity
        .checked_add(1)
        .ok_or_else(|| BinaryTypeError::Invalid("complexity overflow".into()))?;
    if *complexity > MAX_BINARY_TYPE_COMPLEXITY {
        return Err(BinaryTypeError::Invalid(format!(
            "complexity exceeds {MAX_BINARY_TYPE_COMPLEXITY}"
        )));
    }
    match reader.read_u8()? {
        0x00 => Ok("NULL".into()),
        0x01 => Ok(reader.read_varint()?.to_string()),
        0x02 => {
            let encoded = reader.read_varint()?;
            let value = ((encoded >> 1) as i64) ^ (-((encoded & 1) as i64));
            Ok(value.to_string())
        }
        0x07 => {
            let bytes: [u8; 8] = reader
                .read_slice(8)?
                .try_into()
                .map_err(|_| BinaryTypeError::Invalid("truncated Float64 field".into()))?;
            Ok(f64::from_le_bytes(bytes).to_string())
        }
        0x0c => Ok(quote_string_literal(&reader.read_varint_string()?)),
        tag @ (0x0d | 0x0e) => {
            let count = read_count(reader, "aggregate field elements")?;
            ensure_type_budget(count, *complexity, "aggregate field elements")?;
            let mut values = Vec::with_capacity(reader.capacity_for(count, 1));
            for _ in 0..count {
                values.push(read_field_literal(reader, depth + 1, complexity)?);
            }
            let (open, close) = if tag == 0x0d { ('[', ']') } else { ('(', ')') };
            Ok(format!("{open}{}{close}", values.join(", ")))
        }
        0x13 => Ok(if reader.read_u8()? == 0 {
            "false"
        } else {
            "true"
        }
        .into()),
        tag => Err(BinaryTypeError::Unsupported(format!(
            "aggregate Field tag 0x{tag:02x}"
        ))),
    }
}

fn quote_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\0' => out.push_str("\\0"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

/// Write one valid binary descriptor. The structural tags are used for the
/// core scalar/container types. Name-decoration and aggregate types use the
/// server's Custom tag so their already-canonical `Display` spelling remains
/// the single source of truth for function parameters and aliases.
pub(crate) fn write_binary_type(buf: &mut Vec<u8>, ch_type: &ChType) {
    match ch_type {
        ChType::Nothing => buf.push(0x00),
        ChType::UInt8 => buf.push(0x01),
        ChType::UInt16 => buf.push(0x02),
        ChType::UInt32 => buf.push(0x03),
        ChType::UInt64 => buf.push(0x04),
        ChType::UInt128 => buf.push(0x05),
        ChType::UInt256 => buf.push(0x06),
        ChType::Int8 => buf.push(0x07),
        ChType::Int16 => buf.push(0x08),
        ChType::Int32 => buf.push(0x09),
        ChType::Int64 => buf.push(0x0a),
        ChType::Int128 => buf.push(0x0b),
        ChType::Int256 => buf.push(0x0c),
        ChType::Float32 => buf.push(0x0d),
        ChType::Float64 => buf.push(0x0e),
        ChType::Date => buf.push(0x0f),
        ChType::Date32 => buf.push(0x10),
        ChType::DateTime { timezone: None } => buf.push(0x11),
        ChType::DateTime { timezone: Some(tz) } => {
            buf.push(0x12);
            write_string(buf, tz.as_bytes());
        }
        ChType::DateTime64 {
            precision,
            timezone: None,
        } => {
            buf.extend_from_slice(&[0x13, *precision]);
        }
        ChType::DateTime64 {
            precision,
            timezone: Some(tz),
        } => {
            buf.extend_from_slice(&[0x14, *precision]);
            write_string(buf, tz.as_bytes());
        }
        ChType::String => buf.push(0x15),
        ChType::FixedString(width) => {
            buf.push(0x16);
            write_varint(buf, *width as u64);
        }
        ChType::Enum8 { variants } => {
            buf.push(0x17);
            write_varint(buf, variants.len() as u64);
            for (name, value) in variants {
                write_string(buf, name.as_bytes());
                buf.push(*value as u8);
            }
        }
        ChType::Enum16 { variants } => {
            buf.push(0x18);
            write_varint(buf, variants.len() as u64);
            for (name, value) in variants {
                write_string(buf, name.as_bytes());
                buf.extend_from_slice(&value.to_le_bytes());
            }
        }
        ChType::Decimal {
            precision,
            scale,
            bits,
        } => {
            buf.push(match bits {
                32 => 0x19,
                64 => 0x1a,
                128 => 0x1b,
                256 => 0x1c,
                _ => 0x2c,
            });
            if matches!(bits, 32 | 64 | 128 | 256) {
                buf.extend_from_slice(&[*precision, *scale]);
            } else {
                write_string(buf, ch_type.to_string().as_bytes());
            }
        }
        ChType::Uuid => buf.push(0x1d),
        ChType::Array(inner) => {
            buf.push(0x1e);
            write_binary_type(buf, inner);
        }
        ChType::Tuple(elements) => {
            // Canonical SQL Tuple() uses DataTypeTuple's unnamed constructor;
            // `Iterator::all` is vacuously true for zero elements, so keep the
            // explicit nonempty guard when choosing the named 0x20 tag.
            let named = !elements.is_empty() && elements.iter().all(|(name, _)| name.is_some());
            buf.push(if named { 0x20 } else { 0x1f });
            write_varint(buf, elements.len() as u64);
            for (name, element) in elements {
                if let Some(name) = name {
                    write_string(buf, name.as_bytes());
                }
                write_binary_type(buf, element);
            }
        }
        ChType::Interval(kind) => {
            buf.extend_from_slice(&[0x22, interval_tag(*kind)]);
        }
        ChType::Nullable(inner) => {
            buf.push(0x23);
            write_binary_type(buf, inner);
        }
        ChType::LowCardinality(inner) => {
            buf.push(0x26);
            write_binary_type(buf, inner);
        }
        ChType::Map(key, value) => {
            buf.push(0x27);
            write_binary_type(buf, key);
            write_binary_type(buf, value);
        }
        ChType::Ipv4 => buf.push(0x28),
        ChType::Ipv6 => buf.push(0x29),
        ChType::Variant(alternatives) => {
            buf.push(0x2a);
            write_varint(buf, alternatives.len() as u64);
            for alternative in alternatives {
                write_binary_type(buf, alternative);
            }
        }
        ChType::Dynamic { max_types } => buf.extend_from_slice(&[0x2b, *max_types]),
        ChType::Bool => buf.push(0x2d),
        ChType::BFloat16 => buf.push(0x31),
        ChType::Time => buf.push(0x32),
        ChType::Time64 { precision } => buf.extend_from_slice(&[0x34, *precision]),
        ChType::QBit {
            element_type,
            dimension,
        } => {
            buf.push(0x36);
            write_binary_type(buf, &element_type.ch_type());
            write_varint(buf, *dimension as u64);
        }
        ChType::Nested(fields) => {
            buf.push(0x2f);
            write_varint(buf, fields.len() as u64);
            for (name, field) in fields {
                write_string(buf, name.as_bytes());
                write_binary_type(buf, field);
            }
        }
        // JSON (tag 0x30): the serialization-version byte (0), then
        // max_dynamic_paths (VarUInt), max_dynamic_types (raw u8), and the sorted
        // typed-path / skip-path / skip-regexp lists. The ChType keeps these
        // sorted, so the descriptor is emitted deterministically; the server
        // accepts any order on read since it rebuilds maps.
        ChType::Json {
            max_dynamic_paths,
            max_dynamic_types,
            typed_paths,
            skip_paths,
            skip_regexps,
        } => {
            buf.push(0x30);
            buf.push(0);
            write_varint(buf, *max_dynamic_paths as u64);
            buf.push(*max_dynamic_types);
            write_varint(buf, typed_paths.len() as u64);
            for (path, ch_type) in typed_paths {
                write_string(buf, path.as_bytes());
                write_binary_type(buf, ch_type);
            }
            write_varint(buf, skip_paths.len() as u64);
            for path in skip_paths {
                write_string(buf, path.as_bytes());
            }
            write_varint(buf, skip_regexps.len() as u64);
            for regex in skip_regexps {
                write_string(buf, regex.as_bytes());
            }
        }
        ChType::SimpleAggregateFunction { .. }
        | ChType::AggregateFunction { .. }
        | ChType::Geo(_)
        | ChType::Geometry => {
            buf.push(0x2c);
            write_string(buf, ch_type.to_string().as_bytes());
        }
    }
}

fn interval_tag(kind: IntervalKind) -> u8 {
    match kind {
        IntervalKind::Nanosecond => 0x00,
        IntervalKind::Microsecond => 0x01,
        IntervalKind::Millisecond => 0x02,
        IntervalKind::Second => 0x03,
        IntervalKind::Minute => 0x04,
        IntervalKind::Hour => 0x05,
        IntervalKind::Day => 0x06,
        IntervalKind::Week => 0x07,
        IntervalKind::Month => 0x08,
        IntervalKind::Quarter => 0x09,
        IntervalKind::Year => 0x0a,
    }
}

fn write_string(buf: &mut Vec<u8>, bytes: &[u8]) {
    write_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_supported_structural_descriptors() {
        let types = vec![
            ChType::Nothing,
            ChType::Bool,
            ChType::Int8,
            ChType::Int16,
            ChType::Int32,
            ChType::Int64,
            ChType::Int128,
            ChType::Int256,
            ChType::UInt8,
            ChType::UInt16,
            ChType::UInt32,
            ChType::UInt64,
            ChType::UInt128,
            ChType::UInt256,
            ChType::Float32,
            ChType::Float64,
            ChType::BFloat16,
            ChType::QBit {
                element_type: QBitElementType::BFloat16,
                dimension: 8,
            },
            ChType::QBit {
                element_type: QBitElementType::Float32,
                dimension: 9,
            },
            ChType::QBit {
                element_type: QBitElementType::Float64,
                dimension: 300,
            },
            ChType::String,
            ChType::FixedString(13),
            ChType::Date,
            ChType::Date32,
            ChType::DateTime { timezone: None },
            ChType::DateTime {
                timezone: Some("UTC".into()),
            },
            ChType::DateTime64 {
                precision: 3,
                timezone: None,
            },
            ChType::DateTime64 {
                precision: 6,
                timezone: Some("UTC".into()),
            },
            ChType::Time,
            ChType::Time64 { precision: 6 },
            ChType::Interval(IntervalKind::Nanosecond),
            ChType::Interval(IntervalKind::Year),
            ChType::Uuid,
            ChType::Ipv4,
            ChType::Ipv6,
            ChType::Enum8 {
                variants: vec![("off".into(), -1), ("on".into(), 13)],
            },
            ChType::Enum16 {
                variants: vec![("off".into(), -1), ("on".into(), 79)],
            },
            ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            },
            ChType::Decimal {
                precision: 18,
                scale: 9,
                bits: 64,
            },
            ChType::Decimal {
                precision: 38,
                scale: 10,
                bits: 128,
            },
            ChType::Decimal {
                precision: 76,
                scale: 20,
                bits: 256,
            },
            ChType::Dynamic { max_types: 1 },
            ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::UInt64)))),
            ChType::LowCardinality(Box::new(ChType::String)),
            ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Time64 { precision: 3 }),
            ),
            ChType::Tuple(Vec::new()),
            ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
            ChType::Tuple(vec![
                (Some("user_1".into()), ChType::String),
                (Some("n".into()), ChType::Int32),
            ]),
            ChType::Variant(vec![ChType::String, ChType::UInt64]),
            ChType::Geometry,
            ChType::Nested(vec![("n".into(), ChType::UInt32)]),
            ChType::SimpleAggregateFunction {
                func: "anyLast".into(),
                inner: Box::new(ChType::String),
            },
            ChType::AggregateFunction {
                function: "sum".into(),
                arguments: vec![ChType::UInt64],
            },
        ];
        for ch_type in types {
            let mut bytes = Vec::new();
            write_binary_type(&mut bytes, &ch_type);
            let mut reader = ByteReader::new(&bytes);
            assert_eq!(read_binary_type(&mut reader).unwrap(), ch_type);
            assert_eq!(reader.remaining(), 0);
        }
    }

    #[test]
    fn qbit_binary_descriptor_exact_bytes_and_rejections() {
        let mut f32_9 = Vec::new();
        write_binary_type(
            &mut f32_9,
            &ChType::QBit {
                element_type: QBitElementType::Float32,
                dimension: 9,
            },
        );
        assert_eq!(f32_9, [0x36, 0x0d, 0x09]);

        let mut f64_300 = Vec::new();
        write_binary_type(
            &mut f64_300,
            &ChType::QBit {
                element_type: QBitElementType::Float64,
                dimension: 300,
            },
        );
        assert_eq!(f64_300, [0x36, 0x0e, 0xac, 0x02]);

        let mut custom_f32 = vec![0x36, 0x2c];
        write_string(&mut custom_f32, b"Float32");
        custom_f32.push(9);
        assert_eq!(
            read_binary_type(&mut ByteReader::new(&custom_f32)).unwrap(),
            ChType::QBit {
                element_type: QBitElementType::Float32,
                dimension: 9,
            }
        );

        assert!(matches!(
            read_binary_type(&mut ByteReader::new(&[0x36, 0x09, 0x01])),
            Err(BinaryTypeError::Invalid(_))
        ));
        assert!(matches!(
            read_binary_type(&mut ByteReader::new(&[0x36, 0x0d, 0x00])),
            Err(BinaryTypeError::Invalid(_))
        ));
        assert!(matches!(
            read_binary_type(&mut ByteReader::new(&[0x36, 0x0d])),
            Err(BinaryTypeError::Io(ref error)) if error.kind() == io::ErrorKind::UnexpectedEof
        ));

        // QBit's scalar parameter is not a structural nesting level. Keep
        // binary descriptors in parity with the text parser and encode depth
        // validation exactly at the shared cap, while rejecting one more
        // enclosing Array.
        for (arrays, accepted) in [(MAX_TYPE_DEPTH, true), (MAX_TYPE_DEPTH + 1, false)] {
            let mut bytes = vec![0x1e; arrays];
            bytes.extend_from_slice(&[0x36, 0x0d, 0x09]);
            assert_eq!(
                read_binary_type(&mut ByteReader::new(&bytes)).is_ok(),
                accepted,
                "{arrays} enclosing Arrays"
            );
        }
    }

    #[test]
    fn rejects_over_deep_and_invalid_dynamic_descriptors() {
        let mut bytes = vec![0x1e; MAX_TYPE_DEPTH + 1];
        bytes.push(0x01);
        assert!(matches!(
            read_binary_type(&mut ByteReader::new(&bytes)),
            Err(BinaryTypeError::Invalid(_))
        ));
        assert!(matches!(
            read_binary_type(&mut ByteReader::new(&[0x2b, 0xff])),
            Err(BinaryTypeError::Invalid(_))
        ));
    }

    #[test]
    fn geometry_custom_name_uses_fresh_text_depth_then_whole_type_cap() {
        let mut geometry = Vec::new();
        write_binary_type(&mut geometry, &ChType::Geometry);
        assert_eq!(geometry, b"\x2c\x08Geometry");

        // The server reparses a generic Custom name with a fresh text depth
        // budget. Confirm the inner descriptor reader does the same even at the
        // maximum enclosing structural depth.
        let mut bytes = vec![0x1e; MAX_TYPE_DEPTH];
        bytes.extend_from_slice(&geometry);
        let mut reader = ByteReader::new(&bytes);
        let mut complexity = 0;
        assert!(read_binary_type_inner(&mut reader, 0, &mut complexity).is_ok());
        assert_eq!(reader.remaining(), 0);

        // The public reader subsequently validates the complete canonical type
        // under this crate's aggregate cap. Geometry charges five physical
        // levels there, so 95 enclosing Arrays land exactly at MAX_TYPE_DEPTH
        // and one more is rejected before any body traversal.
        for (arrays, accepted) in [(MAX_TYPE_DEPTH - 5, true), (MAX_TYPE_DEPTH - 5 + 1, false)] {
            let mut bytes = vec![0x1e; arrays];
            bytes.extend_from_slice(&geometry);
            let result = read_binary_type(&mut ByteReader::new(&bytes));
            assert_eq!(result.is_ok(), accepted, "{arrays} enclosing Arrays");
        }
    }

    #[test]
    fn rejects_illegal_wrappers_and_oversized_structural_lists_before_allocation() {
        // Nullable(Nullable(UInt8)) is structurally decodable but illegal in
        // ClickHouse. Binary headers must apply the same constructor rules as
        // textual headers before the zero-row path builds an empty column.
        assert!(matches!(
            read_binary_type(&mut ByteReader::new(&[0x23, 0x23, 0x01])),
            Err(BinaryTypeError::Invalid(_))
        ));

        // A hostile Tuple count must fail against the descriptor complexity
        // budget before reserving a Vec for the declared million elements.
        let mut bytes = vec![0x1f];
        write_varint(&mut bytes, MAX_BINARY_TYPE_LIST as u64);
        assert!(matches!(
            read_binary_type(&mut ByteReader::new(&bytes)),
            Err(BinaryTypeError::Invalid(_))
        ));
    }

    #[test]
    fn reads_server_structural_aggregate_and_simple_aggregate_tags() {
        let aggregate = [0x25, 0x00, 0x03, b's', b'u', b'm', 0x00, 0x01, 0x04];
        assert_eq!(
            read_binary_type(&mut ByteReader::new(&aggregate)).unwrap(),
            ChType::AggregateFunction {
                function: "sum".into(),
                arguments: vec![ChType::UInt64],
            }
        );

        // A nonzero state version changes the runtime state layout. `sum` has
        // a registered v0 state codec, so its versioned descriptor must be
        // rejected instead of silently decoding v1 states with v0 framing.
        let versioned = [0x25, 0x01, 0x03, b's', b'u', b'm', 0x00, 0x01, 0x04];
        assert!(matches!(
            read_binary_type(&mut ByteReader::new(&versioned)),
            Err(BinaryTypeError::Unsupported(reason))
                if reason.contains("sum") && reason.contains("version 1")
        ));

        // The version gate applies only to functions with a registered codec.
        // `sumMap` (which emits version 1 at v26.3) passes the gate at any
        // version; its classification stays the textual round-trip check's
        // no-codec rejection, identical for version 0 and 1.
        for version in [0x00, 0x01] {
            let opaque = [
                0x25, version, 0x06, b's', b'u', b'm', b'M', b'a', b'p', 0x00, 0x01, 0x04,
            ];
            assert!(matches!(
                read_binary_type(&mut ByteReader::new(&opaque)),
                Err(BinaryTypeError::Invalid(reason)) if reason.contains("sumMap")
            ));
        }

        let simple = [
            0x2e, 0x07, b'a', b'n', b'y', b'L', b'a', b's', b't', 0x00, 0x01, 0x15,
        ];
        assert_eq!(
            read_binary_type(&mut ByteReader::new(&simple)).unwrap(),
            ChType::SimpleAggregateFunction {
                func: "anyLast".into(),
                inner: Box::new(ChType::String),
            }
        );
    }

    #[test]
    fn json_binary_descriptor_roundtrips_and_canonicalizes() {
        // A JSON type with non-default parameters, typed paths, skip paths, and
        // skip regexps round-trips through the structural tag 0x30.
        let json = parse_ch_type(
            "JSON(max_dynamic_types=8, max_dynamic_paths=64, a Int64, b String, SKIP secret, SKIP REGEXP '^tmp')",
        )
        .unwrap();
        let mut bytes = Vec::new();
        write_binary_type(&mut bytes, &json);
        let mut reader = ByteReader::new(&bytes);
        assert_eq!(read_binary_type(&mut reader).unwrap(), json);
        assert_eq!(reader.remaining(), 0);

        // The server iterates unordered maps/sets, so a descriptor may present
        // typed paths out of order. Hand-build one with `b` before `a` and
        // confirm it canonicalizes into the sorted ChType. Descriptor layout:
        // tag, version 0, max_dynamic_paths (VarUInt), max_dynamic_types (u8),
        // typed count + entries, skip count, skip-regexp count.
        let mut unsorted = vec![0x30, 0x00];
        write_varint(&mut unsorted, 1024); // max_dynamic_paths (default)
        unsorted.push(32); // max_dynamic_types (default)
        write_varint(&mut unsorted, 2); // typed path count
        write_string(&mut unsorted, b"b");
        write_binary_type(&mut unsorted, &ChType::String);
        write_string(&mut unsorted, b"a");
        write_binary_type(&mut unsorted, &ChType::Int64);
        write_varint(&mut unsorted, 0); // skip paths
        write_varint(&mut unsorted, 0); // skip regexps
        assert_eq!(
            read_binary_type(&mut ByteReader::new(&unsorted)).unwrap(),
            parse_ch_type("JSON(a Int64, b String)").unwrap()
        );

        // A nonzero TYPE_JSON_SERIALIZATION_VERSION byte is rejected (the server
        // throws INCORRECT_DATA on > 0).
        let mut nonzero_version = vec![0x30, 0x01];
        write_varint(&mut nonzero_version, 1024);
        nonzero_version.push(32);
        write_varint(&mut nonzero_version, 0);
        write_varint(&mut nonzero_version, 0);
        write_varint(&mut nonzero_version, 0);
        assert!(matches!(
            read_binary_type(&mut ByteReader::new(&nonzero_version)),
            Err(BinaryTypeError::Invalid(_))
        ));

        // A parameter past the construction limits is rejected.
        let mut over_paths = vec![0x30, 0x00];
        write_varint(&mut over_paths, 10001); // max_dynamic_paths > 10000
        over_paths.push(32);
        write_varint(&mut over_paths, 0);
        write_varint(&mut over_paths, 0);
        write_varint(&mut over_paths, 0);
        assert!(matches!(
            read_binary_type(&mut ByteReader::new(&over_paths)),
            Err(BinaryTypeError::Invalid(_))
        ));
    }
}
