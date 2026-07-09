use std::io;
use std::sync::Arc;

use crate::batch::{ChunkedBatch, ColBatch};
use crate::bitmap::Bitmap;
use crate::column::{
    ArrayColumn, BoolColumn, Column, DecimalColumn, DictionaryColumn, FixedBinaryColumn, MapColumn,
    PrimitiveColumn, TupleColumn, Utf8Column,
};
use crate::native::varint::ByteReader;
use crate::schema::{ChType, Field, Schema};

/// Errors that can occur during Native format decoding.
#[derive(Debug)]
pub enum DecodeError {
    Io(io::Error),
    UnsupportedType {
        column: String,
        type_name: String,
    },
    /// A `BlockInfo` preamble carried a field number this decoder does not know.
    /// The server rejects unknown field numbers the same way.
    InvalidBlockInfo {
        field_num: u64,
    },
    /// A column advertised a custom (non-default) serialization, whose wire
    /// layout this crate does not decode. `serialization_byte` is the raw
    /// custom-serialization marker the server wrote (0 would mean default).
    UnsupportedSerialization {
        column: String,
        serialization_byte: u8,
    },
    /// A later block's schema (column names or types) differs from the first
    /// block's. Every block of a query result shares one schema, so a
    /// mismatch means a corrupt or mixed payload. `block_index` is zero-based.
    BlockSchemaMismatch {
        block_index: usize,
    },
    /// A `LowCardinality` column carried a dictionary or index layout this
    /// decoder does not accept for the Native format: a key version other than
    /// 1 (`SharedDictionariesWithAdditionalKeys`), the `NeedGlobalDictionaryBit`
    /// set (Native never uses a shared global dictionary), an index width tag
    /// outside `0..=3`, or an index value that does not fit Arrow's i32 index.
    InvalidLowCardinality {
        column: String,
        reason: &'static str,
    },
    /// An `Array` column carried an offset run this decoder rejects: offsets that
    /// are not monotonically non-decreasing (the server enforces this in Native
    /// mode, a decrease is `INCORRECT_DATA`), or an absolute offset that exceeds
    /// `i64::MAX` (the Arrow LargeList offset width the column widens into).
    InvalidArray {
        column: String,
        reason: &'static str,
    },
    /// A `Tuple` column decoded element columns of unequal lengths. The server
    /// enforces the same invariant (`INCORRECT_DATA` in Native mode). Every
    /// element decode here is driven by the same row count, so this is a
    /// defensive mirror of that check rather than a reachable state.
    InvalidTuple {
        column: String,
        reason: &'static str,
    },
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
                write!(
                    f,
                    "Unsupported ClickHouse type '{type_name}' for column '{column}'"
                )
            }
            DecodeError::InvalidBlockInfo { field_num } => {
                write!(f, "Unknown BlockInfo field number {field_num}")
            }
            DecodeError::UnsupportedSerialization {
                column,
                serialization_byte,
            } => {
                write!(
                    f,
                    "Unsupported custom serialization (marker {serialization_byte}) for column '{column}'"
                )
            }
            DecodeError::BlockSchemaMismatch { block_index } => {
                write!(f, "Block {block_index} schema differs from the first block")
            }
            DecodeError::InvalidLowCardinality { column, reason } => {
                write!(
                    f,
                    "Invalid LowCardinality layout for column '{column}': {reason}"
                )
            }
            DecodeError::InvalidArray { column, reason } => {
                write!(f, "Invalid Array layout for column '{column}': {reason}")
            }
            DecodeError::InvalidTuple { column, reason } => {
                write!(f, "Invalid Tuple layout for column '{column}': {reason}")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// Server protocol revision this crate has been validated against
/// (ClickHouse v26.6.1.1193-stable). Pass this as `DecodeOptions::protocol_revision`
/// when decoding a Native stream produced by a current server over the native
/// TCP protocol.
pub const DBMS_TCP_PROTOCOL_VERSION: u64 = 54485;

/// Protocol revision at which every column header carries a one-byte
/// custom-serialization marker before its data (server constant
/// `DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION`).
pub const DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION: u64 = 54454;

/// Protocol revision at which a data block's `BlockInfo` carries the
/// `out_of_order_buckets` field (server
/// `DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS_IN_AGGREGATION` in
/// `src/Core/ProtocolDefines.h`). At or above it, `BlockInfo::write` emits field 3
/// with an empty vector for a plain data block, which [`read_block_info`] consumes
/// and [`super::encode`] re-emits, so both stay byte-identical to the server writer.
pub(crate) const DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS: u64 = 54480;

/// Options for Native format decoding.
#[derive(Default)]
pub struct DecodeOptions {
    /// Negotiated server protocol revision the Native stream was produced with.
    ///
    /// Native block framing is revision gated, and the revision is negotiated
    /// out of band (in the TCP handshake), so the decoder must be told it:
    ///
    /// - A `BlockInfo` preamble precedes every block when this is > 0.
    /// - A per-column custom-serialization marker byte is present when this is
    ///   >= [`DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION`].
    ///
    /// Use [`DBMS_TCP_PROTOCOL_VERSION`] for a stream from a current server over
    /// the native TCP protocol. Use 0 for a bare Native stream with no protocol
    /// framing, for example HTTP `FORMAT Native` with no `client_protocol_version`
    /// set.
    pub protocol_revision: u64,
}

// ---------------------------------------------------------------------------
// Type name parsing
// ---------------------------------------------------------------------------

/// Maximum wrapper/container nesting depth the type-name parser accepts.
///
/// The type string is attacker-controlled wire input, and every wrapper level
/// (`Nullable`, `LowCardinality`, `Array`, `Tuple`, `Map`) recurses one
/// stack frame in [`parse_ch_type_depth`]. An unbounded string like
/// `Array(Array(...Array(Int32)...))` would overflow the stack and abort the
/// process (SIGABRT is uncatchable), violating the "malformed bytes return an
/// error, never crash" invariant. Capping the PARSER caps every downstream
/// recursion too: decode, the completeness scan, and the Arrow export only recurse
/// as deep as the parsed `ChType`, so a rejected over-deep header never reaches
/// them. 100 far exceeds any real ClickHouse schema (real nested types are a
/// handful of levels deep) and stays safe even on small worker-thread stacks.
/// ClickHouse's own analogous guard is `max_parser_depth` (default 1000).
/// `pub(crate)` so the encoder's `validate_column` can enforce the same cap on
/// caller-constructed types, which never pass through this parser.
pub(crate) const MAX_TYPE_DEPTH: usize = 100;

/// Parse a ClickHouse type name string into a [`crate::schema::ChType`].
///
/// Accepts the canonical spellings the server writes in Native block headers,
/// including the `Nullable`, `LowCardinality`, `Array`, `Tuple`, and `Map`
/// container forms. Returns `None` for an unsupported or malformed name. The
/// input is treated as untrusted wire data: parsing is depth-bounded (see
/// `MAX_TYPE_DEPTH`) and never panics. Also used by the encoder to confirm a
/// rendered type string round-trips (a header this parser rejects is one the
/// server rejects too), and by bindings that map type names to columns without
/// decoding a block.
pub fn parse_ch_type(type_name: &str) -> Option<ChType> {
    parse_ch_type_depth(type_name, 0)
}

/// Parse a ClickHouse type name at nesting depth `depth`, rejecting anything past
/// [`MAX_TYPE_DEPTH`] so an unbounded hostile type string cannot overflow the
/// stack. Each recursing arm (`Nullable`, `LowCardinality`, `Array`) calls this
/// with `depth + 1`; the non-recursive leaf arms are depth independent.
fn parse_ch_type_depth(type_name: &str, depth: usize) -> Option<ChType> {
    // Bound recursion before doing any work at this level. A rejected over-deep
    // type surfaces as `UnsupportedType`, exactly like any other unparseable
    // header.
    if depth > MAX_TYPE_DEPTH {
        return None;
    }

    // Nullable wrapper. ClickHouse forbids a `Nullable`, a `LowCardinality`, or an
    // `Array` directly inside a `Nullable`: the only legal nesting with
    // LowCardinality is `LowCardinality(Nullable(T))`, never the reverse,
    // `Nullable(Nullable(T))` does not exist at all, and
    // `DataTypeArray::canBeInsideNullable()` is false so `Nullable(Array(T))` is
    // not constructible. An honest server never emits any of these, but the type
    // string is untrusted wire input, so reject them here. `decode_column` and
    // `skip_column_data` unwrap exactly one `Nullable` and handle `LowCardinality`
    // and `Array` only at the top level; accepting a nested wrapper would let those
    // inner wrappers reach an `unreachable!` on malformed bytes (AGENTS.md invariant
    // 2: no panics on malformed input). `Nullable(Map(K, V))` is likewise not
    // constructible (`DataTypeMap::canBeInsideNullable()` is false), so `Map` is
    // rejected here too. `Nullable(Tuple(...))` IS legal
    // (`DataTypeTuple::canBeInsideNullable()` is true at v26.6.1.1193-stable; the
    // `enable_nullable_tuple_type` DDL gate is a creation-time concern with no wire
    // effect), so `Tuple` deliberately passes this guard.
    if let Some(inner) = type_name.strip_prefix("Nullable(") {
        if let Some(inner) = inner.strip_suffix(')') {
            let inner_type = parse_ch_type_depth(inner, depth + 1)?;
            if matches!(
                inner_type,
                ChType::Nullable(_)
                    | ChType::LowCardinality(_)
                    | ChType::Array(_)
                    | ChType::Map(..)
            ) {
                return None;
            }
            return Some(ChType::Nullable(Box::new(inner_type)));
        }
    }

    // LowCardinality wrapper. The inner type is parsed recursively, so
    // LowCardinality(Nullable(String)) yields LowCardinality(Nullable(String)).
    if let Some(inner) = type_name.strip_prefix("LowCardinality(") {
        if let Some(inner) = inner.strip_suffix(')') {
            return parse_ch_type_depth(inner, depth + 1)
                .map(|t| ChType::LowCardinality(Box::new(t)));
        }
    }

    // Array wrapper. The element type is parsed recursively, so
    // `Array(LowCardinality(String))` yields `Array(LowCardinality(String))`. The
    // element may itself be `Nullable`, `LowCardinality`, or a further `Array`;
    // there is no inner-type restriction. The array is never wrapped in an outer
    // `Nullable` (rejected by the guard in the `Nullable(` arm above).
    if let Some(inner) = type_name.strip_prefix("Array(") {
        if let Some(inner) = inner.strip_suffix(')') {
            let inner_type = parse_ch_type_depth(inner, depth + 1)?;
            return Some(ChType::Array(Box::new(inner_type)));
        }
    }

    // Tuple(...). Unnamed (`Tuple(Int32, String)`), named
    // (`Tuple(a Int32, b String)`), and backtick-quoted-name
    // (`Tuple(`a b` Int32)`) forms, per `DataTypeTuple::doGetName` (confirmed at
    // v26.6.1.1193-stable). The inner list cannot be split naively on `,`: an
    // element type may carry commas inside its own parentheses
    // (`Decimal(9, 4)`, a nested `Tuple`), inside an Enum's single-quoted names,
    // or inside a backtick-quoted element name, so the splitter is
    // paren/quote-aware. Each element recurses at depth + 1, so the
    // MAX_TYPE_DEPTH cap bounds a hostile deeply-nested header exactly like the
    // single-child wrappers. `Tuple()` (zero elements) is accepted: it is
    // constructible and emittable, with its own one-byte-per-row wire layout.
    if let Some(inner) = type_name.strip_prefix("Tuple(") {
        if let Some(inner) = inner.strip_suffix(')') {
            return parse_tuple_elements(inner, depth).map(ChType::Tuple);
        }
    }

    // Map(K, V). Always exactly the two type arguments (`DataTypeMap::doGetName`,
    // confirmed at v26.6.1.1193-stable); the nested tuple's "keys"/"values" names
    // never appear in the type string. Split on the top-level comma with the same
    // paren/quote-aware splitter the Tuple arm uses, since either argument can
    // carry commas (`Decimal(9, 4)`, a nested `Map`/`Tuple`, an Enum). Key-type
    // legality (no `Nullable` or `LowCardinality(Nullable(...))` keys) is
    // enforced by `validate_header_type` at header-read time, not here, so the
    // encoder's round-trip check and the decoder agree on what parses.
    if let Some(inner) = type_name.strip_prefix("Map(") {
        if let Some(inner) = inner.strip_suffix(')') {
            let parts = split_top_level_commas(inner.trim_matches(' '))?;
            if parts.len() != 2 {
                return None;
            }
            let key = parse_ch_type_depth(parts[0].trim_matches(' '), depth + 1)?;
            let value = parse_ch_type_depth(parts[1].trim_matches(' '), depth + 1)?;
            return Some(ChType::Map(Box::new(key), Box::new(value)));
        }
    }

    // FixedString(N). N must be positive: the server rejects FixedString(0) at
    // table-creation time, and a zero width cannot be represented in the
    // contiguous `width * num_rows` buffer (row count would be unrecoverable),
    // so treat it as unsupported rather than decode an inconsistent column.
    if let Some(n_str) = type_name.strip_prefix("FixedString(") {
        if let Some(n_str) = n_str.strip_suffix(')') {
            if let Ok(n) = n_str.trim().parse::<usize>() {
                if n > 0 {
                    return Some(ChType::FixedString(n));
                }
            }
        }
    }

    // DateTime64(P) and DateTime64(P, '<tz>'). Checked before the DateTime(
    // prefix so a DateTime64(...) string never falls into the DateTime arm.
    if let Some(inner) = type_name.strip_prefix("DateTime64(") {
        if let Some(inner) = inner.strip_suffix(')') {
            // Inner is "P" or "P, '<tz>'". Split on the first comma into the
            // precision and the optional timezone.
            let (precision_str, timezone) = match inner.split_once(',') {
                Some((p, tz)) => (p.trim(), Some(strip_quotes(tz.trim()).to_string())),
                None => (inner.trim(), None),
            };
            // Precision must be a valid DateTime64 scale (0..=9); anything else
            // surfaces as UnsupportedType rather than a wrong decode.
            return match precision_str.parse::<u8>() {
                Ok(precision) if precision <= 9 => Some(ChType::DateTime64 {
                    precision,
                    timezone,
                }),
                _ => None,
            };
        }
    }

    // Enum8(...) / Enum16(...). The server always emits the concrete keyword
    // with explicit values, never a bare `Enum(...)`, so only these two
    // spellings are parsed. The inner `'name' = value, ...` list is walked by a
    // quote-aware parser (`parse_enum_variants`) because a name can contain `,`
    // and `=` unescaped. An out-of-range value or any syntax error returns None
    // (UnsupportedType) rather than panicking on this untrusted string.
    if let Some(inner) = type_name.strip_prefix("Enum8(") {
        if let Some(inner) = inner.strip_suffix(')') {
            return parse_enum_variants::<i8>(inner).map(|variants| ChType::Enum8 { variants });
        }
    }
    if let Some(inner) = type_name.strip_prefix("Enum16(") {
        if let Some(inner) = inner.strip_suffix(')') {
            return parse_enum_variants::<i16>(inner).map(|variants| ChType::Enum16 { variants });
        }
    }

    // Decimal(P, S). The server always emits the canonical `Decimal(P, S)` form
    // on the wire via `DataTypeDecimal::doGetName` (comma-space, both fields
    // present), never `Decimal32(S)`/`Decimal64(S)`/etc., so only this spelling
    // is parsed (mirroring the decision to accept only `Enum8(`/`Enum16(`). The
    // byte width is derived from P, not carried in the per-row data. Any
    // out-of-range P/S or non-numeric field surfaces as None (UnsupportedType);
    // the type string is untrusted, so this never panics.
    if let Some(inner) = type_name.strip_prefix("Decimal(") {
        if let Some(inner) = inner.strip_suffix(')') {
            if let Some((p_str, s_str)) = inner.split_once(',') {
                let precision = p_str.trim().parse::<u8>().ok()?;
                let scale = s_str.trim().parse::<u8>().ok()?;
                // Constraint (server `DataTypeDecimal`): 1 <= P <= 76 and
                // 0 <= S <= P. The byte width follows from P.
                let bits = decimal_bits_from_precision(precision)?;
                if scale > precision {
                    return None;
                }
                return Some(ChType::Decimal {
                    precision,
                    scale,
                    bits,
                });
            }
            // Missing comma: not the canonical `Decimal(P, S)` the server emits.
            return None;
        }
    }

    // DateTime('<tz>'). The bare DateTime is handled by the exact-match block
    // below. Only the parameterized, timezone-carrying form reaches here.
    if let Some(inner) = type_name.strip_prefix("DateTime(") {
        if let Some(inner) = inner.strip_suffix(')') {
            let timezone = strip_quotes(inner.trim()).to_string();
            return Some(ChType::DateTime {
                timezone: Some(timezone),
            });
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
        // Wide integers. The server emits exactly these case-sensitive spellings
        // (no parameters, no aliases) via `DataTypeNumber<T>::doGetName`.
        "Int128" => Some(ChType::Int128),
        "UInt128" => Some(ChType::UInt128),
        "Int256" => Some(ChType::Int256),
        "UInt256" => Some(ChType::UInt256),
        "Date" => Some(ChType::Date),
        "Date32" => Some(ChType::Date32),
        "DateTime" => Some(ChType::DateTime { timezone: None }),
        "String" => Some(ChType::String),
        "UUID" => Some(ChType::Uuid),
        "IPv4" => Some(ChType::Ipv4),
        "IPv6" => Some(ChType::Ipv6),
        _ => None,
    }
}

/// Derive a `Decimal`'s on-wire byte width (as a bit count) from its precision.
///
/// ClickHouse backs a `Decimal(P, S)` with a fixed-width signed integer chosen
/// by P (server `DataTypesDecimal` / `createDecimal`, confirmed at
/// v26.6.1.1193-stable):
///
/// - P in  1..=9  -> Int32  (32 bits, 4 bytes/row)
/// - P in 10..=18 -> Int64  (64 bits, 8 bytes/row)
/// - P in 19..=38 -> Int128 (128 bits, 16 bytes/row)
/// - P in 39..=76 -> Int256 (256 bits, 32 bytes/row)
///
/// P must be in 1..=76; a precision of 0 or above 76 has no backing integer and
/// returns `None` (the caller maps it to `UnsupportedType`).
pub(crate) fn decimal_bits_from_precision(precision: u8) -> Option<u16> {
    match precision {
        1..=9 => Some(32),
        10..=18 => Some(64),
        19..=38 => Some(128),
        39..=76 => Some(256),
        _ => None,
    }
}

/// Strip a single pair of surrounding single quotes from a timezone string.
///
/// ClickHouse emits timezones inside single quotes, for example
/// `DateTime('UTC')`. A well-formed server string always has both quotes; if
/// either is missing the input is returned unchanged rather than guessed at.
fn strip_quotes(s: &str) -> &str {
    s.strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(s)
}

/// Parse the inner `'name' = value, ...` list of an `Enum8`/`Enum16` type
/// string into `(name, value)` pairs, preserving the server's emitted order
/// (ascending by value).
///
/// The list cannot be split on `,` or `=`: a quoted name passes those bytes
/// through unescaped (server `writeQuotedString`, confirmed at
/// v26.6.1.1193-stable). So this walks the string byte by byte: skip spaces,
/// require `'`, read the name until the closing unescaped `'` unescaping the
/// server's set (`\\`, `\'`, `\b`, `\f`, `\n`, `\r`, `\t`, `\0`), skip spaces,
/// require `=`, parse a signed integer that must fit `T` (`i8` for `Enum8`,
/// `i16` for `Enum16`), then skip spaces and require `,` or end of input.
///
/// Returns `None` (-> UnsupportedType) on any malformed escape, out-of-range
/// value, or syntax error. The type string is untrusted wire input, so this
/// never panics. An empty list (`Enum8()`) returns an empty `Vec`; the server
/// does not emit it, but it is harmless and not a decode error. Duplicate names or
/// values are accepted as written rather than rejected: the variants are metadata
/// carried on the `ChType` only (the wire payload is the raw underlying int), so a
/// duplicate cannot corrupt a decoded column, and a server that emits one is
/// preserved for faithful round-tripping through `Display`.
fn parse_enum_variants<T: TryFrom<i64>>(inner: &str) -> Option<Vec<(String, T)>> {
    let bytes = inner.as_bytes();
    let mut pos = 0usize;
    let mut variants: Vec<(String, T)> = Vec::new();

    skip_ascii_spaces(bytes, &mut pos);
    if pos >= bytes.len() {
        // Empty inner list: `Enum8()`. No variants.
        return Some(variants);
    }

    loop {
        skip_ascii_spaces(bytes, &mut pos);

        // Name: a single-quoted, escaped string.
        if bytes.get(pos) != Some(&b'\'') {
            return None;
        }
        pos += 1;
        let name = parse_enum_name(bytes, &mut pos)?;

        // ` = ` separator (spaces optional, the server writes exactly one each
        // side; accept any run of spaces to stay lenient on the untrusted input).
        skip_ascii_spaces(bytes, &mut pos);
        if bytes.get(pos) != Some(&b'=') {
            return None;
        }
        pos += 1;
        skip_ascii_spaces(bytes, &mut pos);

        // Signed integer value. Parse in i64 first, then narrow to T so an
        // out-of-range value for the concrete enum width is rejected.
        let value = parse_enum_value(bytes, &mut pos)?;
        let value = T::try_from(value).ok()?;
        variants.push((name, value));

        // Separator or end of input.
        skip_ascii_spaces(bytes, &mut pos);
        match bytes.get(pos) {
            None => return Some(variants),
            Some(&b',') => pos += 1,
            Some(_) => return None,
        }
    }
}

/// Advance `pos` past any run of ASCII space (0x20) bytes.
fn skip_ascii_spaces(bytes: &[u8], pos: &mut usize) {
    while bytes.get(*pos) == Some(&b' ') {
        *pos += 1;
    }
}

/// Read an enum variant name from `bytes` starting just after the opening `'`,
/// advancing `pos` past the closing `'`. Applies the server's unescape set; any
/// unknown escape or a missing closing quote returns `None`.
fn parse_enum_name(bytes: &[u8], pos: &mut usize) -> Option<String> {
    let mut name = Vec::new();
    loop {
        let b = *bytes.get(*pos)?;
        *pos += 1;
        match b {
            b'\'' => {
                // Closing quote. The name bytes are UTF-8 (the whole type string
                // came from a `String` validated as UTF-8 in the header), so
                // this conversion succeeds for any well-formed input.
                return String::from_utf8(name).ok();
            }
            b'\\' => {
                let esc = *bytes.get(*pos)?;
                *pos += 1;
                let decoded = match esc {
                    b'\\' => b'\\',
                    b'\'' => b'\'',
                    b'b' => 0x08,
                    b'f' => 0x0C,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'0' => 0x00,
                    _ => return None, // unknown escape
                };
                name.push(decoded);
            }
            other => name.push(other),
        }
    }
}

/// Parse a signed decimal integer (optional leading `-`) from `bytes`,
/// advancing `pos` past the digits. Returns `None` on no digits or overflow.
fn parse_enum_value(bytes: &[u8], pos: &mut usize) -> Option<i64> {
    let start = *pos;
    if bytes.get(*pos) == Some(&b'-') {
        *pos += 1;
    }
    let digits_start = *pos;
    while matches!(bytes.get(*pos), Some(b'0'..=b'9')) {
        *pos += 1;
    }
    if *pos == digits_start {
        return None; // no digits
    }
    // The slice is `[-]?[0-9]+`, all ASCII, so it is valid UTF-8 and parses as
    // i64 unless it overflows, which `parse` reports as an error.
    std::str::from_utf8(&bytes[start..*pos]).ok()?.parse().ok()
}

/// Parse the inner element list of a `Tuple(...)` type string into
/// `(optional name, element type)` pairs, preserving declaration order.
///
/// An empty (or all-spaces) list is `Tuple()`. Otherwise the list is split on
/// top-level commas ([`split_top_level_commas`]) and each element is parsed by
/// [`parse_tuple_element`] at `depth + 1`. Any malformed element (unterminated
/// quoting, a name without a type, an unknown element type) returns `None`
/// (-> `UnsupportedType`); the type string is untrusted wire input, so this
/// never panics. The server rejects empty names, the literal name `null`, and
/// duplicate names at creation time, so an honest header never carries them;
/// this parser accepts them as written rather than second-guessing (they are
/// metadata only and round-trip through `Display`).
fn parse_tuple_elements(inner: &str, depth: usize) -> Option<Vec<(Option<String>, ChType)>> {
    let trimmed = inner.trim_matches(' ');
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    let parts = split_top_level_commas(trimmed)?;
    let mut elements = Vec::with_capacity(parts.len());
    for part in parts {
        elements.push(parse_tuple_element(part.trim_matches(' '), depth)?);
    }
    Some(elements)
}

/// Split a tuple element list on the commas at nesting depth 0, skipping over
/// parenthesized groups, single-quoted strings (an Enum element's variant
/// names, a DateTime timezone), and backtick-quoted element names. Inside
/// either quote form a backslash consumes the next byte (the server's lexer
/// never lets `\` close a quote), and inside backticks a doubled backtick is
/// the escaped-backtick form, not a close-then-reopen. Returns `None` on
/// unbalanced parentheses or an unterminated quote, which only a malformed
/// header can produce.
fn split_top_level_commas(s: &str) -> Option<Vec<&str>> {
    let bytes = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                i += 1;
            }
            b',' if depth == 0 => {
                parts.push(&s[start..i]);
                i += 1;
                start = i;
            }
            quote @ (b'\'' | b'`') => {
                i += 1;
                loop {
                    let b = *bytes.get(i)?; // unterminated quote -> malformed
                    i += 1;
                    if b == b'\\' {
                        // A backslash escape consumes the next byte, so an
                        // escaped quote cannot close the string. A trailing
                        // lone backslash is unterminated input.
                        if i >= bytes.len() {
                            return None;
                        }
                        i += 1;
                    } else if b == quote {
                        if quote == b'`' && bytes.get(i) == Some(&b'`') {
                            i += 1; // doubled backtick: escaped, stay inside
                        } else {
                            break;
                        }
                    }
                }
            }
            _ => i += 1,
        }
    }
    // All split points are ASCII delimiters, so every slice boundary is a char
    // boundary.
    parts.push(&s[start..]);
    Some(parts)
}

/// Parse one tuple element, `part` already trimmed: either a bare type
/// (`Int32`), a bare-identifier name then the type (`a Int32`), or a
/// backtick-quoted name then the type (`` `a b` Int32 ``).
///
/// The bare named form is unambiguous because no ClickHouse type name contains
/// a space outside parentheses or quotes: if the text before the first space is
/// a valid bare identifier (the same [`crate::schema::is_bare_identifier`]
/// predicate `Display` quotes by, so parse and render agree), the element is
/// named; otherwise the whole text is parsed as an unnamed type (e.g.
/// `Decimal(9, 4)`, whose space sits inside its parentheses).
fn parse_tuple_element(part: &str, depth: usize) -> Option<(Option<String>, ChType)> {
    let bytes = part.as_bytes();
    if bytes.first() == Some(&b'`') {
        let mut pos = 1usize;
        let name = parse_back_quoted_name(bytes, &mut pos)?;
        // `pos` sits just past the closing backtick, an ASCII boundary. The
        // rest, after the separating spaces, must be a parseable element type.
        let rest = part[pos..].trim_matches(' ');
        if rest.is_empty() {
            return None; // a name with no type
        }
        let ch_type = parse_ch_type_depth(rest, depth + 1)?;
        return Some((Some(name), ch_type));
    }
    if let Some((first, rest)) = part.split_once(' ') {
        if crate::schema::is_bare_identifier(first) {
            let rest = rest.trim_matches(' ');
            if rest.is_empty() {
                return None; // a name with no type
            }
            let ch_type = parse_ch_type_depth(rest, depth + 1)?;
            return Some((Some(first.to_string()), ch_type));
        }
    }
    let ch_type = parse_ch_type_depth(part, depth + 1)?;
    Some((None, ch_type))
}

/// Read a backtick-quoted tuple element name from `bytes` starting just after
/// the opening backtick, advancing `pos` past the closing backtick.
///
/// The server WRITES only `writeBackQuotedString`'s escapes (`\``, `\\`, and
/// the C0 letter escapes; see `escape_back_quoted` in `crate::schema`), but its
/// own READERS are more permissive, and this parser mirrors the reader side
/// (confirmed at v26.6.1.1193-stable: `Lexer.cpp` `quotedString<'`'>`,
/// `ReadHelpers.cpp` `readBackQuotedStringWithSQLStyle`, `ReadHelpers.h`
/// `parseEscapeSequence`): a doubled backtick is one literal backtick, and a
/// backslash escape accepts `\xAA` (the hex byte), `\N` (empty, nothing
/// appended), the control escapes `\a \b \e \f \n \r \t \v \0`, and ANY other
/// `\c` as the literal `c` with the backslash dropped. A lone backslash always
/// consumes the following byte, so an escaped backtick never closes the name.
/// Unterminated or truncated input returns `None` (-> `UnsupportedType`),
/// never a panic. The decoded bytes must be valid UTF-8 (a `\xAA` escape can
/// break that), or the name is rejected.
fn parse_back_quoted_name(bytes: &[u8], pos: &mut usize) -> Option<String> {
    let mut name = Vec::new();
    loop {
        let b = *bytes.get(*pos)?;
        *pos += 1;
        match b {
            b'`' => {
                if bytes.get(*pos) == Some(&b'`') {
                    // Doubled backtick: the SQL-style escaped form.
                    *pos += 1;
                    name.push(b'`');
                } else {
                    return String::from_utf8(name).ok();
                }
            }
            b'\\' => {
                let esc = *bytes.get(*pos)?;
                *pos += 1;
                match esc {
                    b'x' => {
                        // \xAA: exactly two hex digits for one raw byte.
                        let hi = hex_digit(*bytes.get(*pos)?)?;
                        let lo = hex_digit(*bytes.get(*pos + 1)?)?;
                        *pos += 2;
                        name.push((hi << 4) | lo);
                    }
                    b'N' => {} // \N is the empty escape; nothing appended
                    b'a' => name.push(0x07),
                    b'b' => name.push(0x08),
                    b'e' => name.push(0x1B),
                    b'f' => name.push(0x0C),
                    b'n' => name.push(b'\n'),
                    b'r' => name.push(b'\r'),
                    b't' => name.push(b'\t'),
                    b'v' => name.push(0x0B),
                    b'0' => name.push(0x00),
                    // Any other escaped byte (including ` and \) is itself.
                    other => name.push(other),
                }
            }
            other => name.push(other),
        }
    }
}

/// The value of one ASCII hex digit, or `None` for a non-hex byte.
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Column decoders
// ---------------------------------------------------------------------------

/// Read a null map: 1 byte per row, 0x01 = null.
fn decode_null_map(reader: &mut ByteReader, num_rows: usize) -> io::Result<Bitmap> {
    let null_bytes = reader.read_slice(num_rows)?;
    Ok(Bitmap::from_ch_null_map(null_bytes))
}

/// Decode fixed-width primitives by reading wire bytes straight into a typed
/// buffer.
///
/// On little-endian platforms (x86, ARM) the wire bytes already are the
/// in-memory representation, so we allocate the destination `Vec<T>` and copy
/// the wire bytes directly into its backing store with a single
/// `copy_nonoverlapping` — no per-element loop, no temporary buffer.
///
/// The destination is allocated as `Vec<T>` (not a `Vec<u8>` reinterpreted as
/// `Vec<T>`) so the allocation has T's alignment and is freed with T's layout;
/// reinterpreting a `Vec<u8>` allocation as `Vec<T>` is undefined behavior.
macro_rules! decode_primitive {
    ($reader:expr, $num_rows:expr, $ty:ty) => {{
        let num_rows = $num_rows;
        // A row count from an untrusted header can overflow `usize` when scaled
        // to bytes. `checked_mul` turns that into an error instead of a wrapping
        // multiply (and the debug-build overflow panic), so the decoder never
        // panics on a malformed length.
        let total_bytes = num_rows
            .checked_mul(std::mem::size_of::<$ty>())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "primitive column byte length overflows usize",
                )
            })?;
        // Borrow the exact wire bytes first; this bounds-checks the whole run
        // once and returns `UnexpectedEof` if the buffer is short. Reading
        // before allocating also caps the `with_capacity` below at the bytes
        // actually present, so a hostile row count cannot drive a giant
        // allocation.
        let src: &[u8] = $reader.read_slice(total_bytes)?;

        #[cfg(target_endian = "little")]
        {
            let mut values: Vec<$ty> = Vec::with_capacity(num_rows);
            // Safety: `with_capacity(num_rows)` reserves exactly `total_bytes`
            // bytes (`num_rows * size_of::<$ty>()`), correctly aligned for `$ty`.
            // `src` is a `&[u8]` of exactly `total_bytes` length returned by
            // `read_slice`, so the source and destination ranges are both valid
            // for `total_bytes` and cannot overlap (`src` borrows the input
            // buffer, `values` is a fresh allocation). We `set_len` to
            // `num_rows` only after every byte is written; on the EOF path above
            // we returned before allocating, so there is no partially
            // initialized `Vec` to drop.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr(),
                    values.as_mut_ptr() as *mut u8,
                    total_bytes,
                );
                values.set_len(num_rows);
            }
            values
        }

        // Big-endian fallback: byte-swap each little-endian wire element.
        #[cfg(target_endian = "big")]
        {
            let values: Vec<$ty> = src
                .chunks_exact(std::mem::size_of::<$ty>())
                .map(|chunk| <$ty>::from_le_bytes(chunk.try_into().unwrap()))
                .collect();
            values
        }
    }};
}

fn decode_bool_data(reader: &mut ByteReader, num_rows: usize) -> io::Result<BoolColumn> {
    let wire_bytes = reader.read_slice(num_rows)?;
    Ok(BoolColumn::from_wire_bytes(wire_bytes))
}

/// Decode a String column into Arrow offsets plus a single data buffer.
///
/// Each value is a varint length followed by that many raw bytes (server
/// `SerializationString::deserializeBinaryBulk`, confirmed at v26.6.1.1193-stable).
/// Each value's bytes are borrowed from the input as a sub-slice and appended to
/// `data` with one `extend_from_slice`: one copy per string, zero per-row heap
/// allocations.
fn decode_string_data(reader: &mut ByteReader, num_rows: usize) -> io::Result<(Vec<i32>, Vec<u8>)> {
    let mut offsets = Vec::with_capacity(num_rows + 1);
    // Reserve a lower bound of one byte per value so the common short-string
    // case does not start from a zero-capacity buffer and reallocate from
    // scratch on the first few pushes. `extend_from_slice` still grows it for
    // longer strings.
    let mut data = Vec::with_capacity(num_rows);
    let mut offset: i32 = 0;
    offsets.push(offset);

    for _ in 0..num_rows {
        let len = varint_usize(reader.read_varint()?, "String value length")?;
        // Arrow 32-bit offsets cap one chunk's string data at i32::MAX bytes.
        // Past that, `offset + len` would wrap to a negative value in release
        // builds (and panic in debug), producing corrupt offsets that then drive
        // out-of-bounds slicing. Reject it as InvalidData instead. Compute the
        // new offset from the length prefix before reading the bytes, so an
        // oversized value is rejected without first copying a >2 GiB payload
        // into `data`. This is a fatal error, not UnexpectedEof, so the
        // streaming decoder does not mistake it for "need more bytes". Blocks
        // stay separate chunks, so the 2 GiB cap is per chunk, not per result.
        offset = i32::try_from(len)
            .ok()
            .and_then(|len_i32| offset.checked_add(len_i32))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "String column chunk exceeds 2 GiB (i32 offset overflow)",
                )
            })?;
        let bytes = reader.read_slice(len)?;
        data.extend_from_slice(bytes);
        offsets.push(offset);
    }

    Ok((offsets, data))
}

fn decode_fixed_binary_data(
    reader: &mut ByteReader,
    num_rows: usize,
    width: usize,
) -> io::Result<Vec<u8>> {
    // `checked_mul` guards against a row count or width that overflows `usize`;
    // `read_slice` then bounds the result against the bytes actually present.
    let total = num_rows.checked_mul(width).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "fixed-width column byte length overflows usize",
        )
    })?;
    Ok(reader.read_slice(total)?.to_vec())
}

// ---------------------------------------------------------------------------
// Bulk-state prefix
// ---------------------------------------------------------------------------

/// Consume a column's per-block `deserializeBinaryBulkStatePrefix` bytes.
///
/// In the Native format the server runs `readData` once per column per block,
/// which calls `deserializeBinaryBulkStatePrefix` immediately before the column
/// payload, and only when the block has rows (`NativeReader::readData`, gated by
/// `if (rows)`). Every currently supported type reads zero prefix bytes;
/// `LowCardinality` is the first that reads a real prefix, the 8-byte key
/// version. Centralizing it here means a later type with a real prefix (Array,
/// Map, and so on) declares its prefix in one place rather than special-casing
/// the per-column loop.
///
/// Returns the parsed key version for `LowCardinality` (so the decoder does not
/// re-read it), `None` for every other type.
fn read_state_prefix(
    reader: &mut ByteReader,
    ch_type: &ChType,
    column: &str,
) -> Result<Option<u64>, DecodeError> {
    match ch_type {
        ChType::LowCardinality(_) => {
            let key_version = reader.read_u64_le()?;
            if key_version != LOW_CARDINALITY_KEY_VERSION {
                return Err(DecodeError::InvalidLowCardinality {
                    column: column.to_string(),
                    reason: "key version is not 1 (SharedDictionariesWithAdditionalKeys)",
                });
            }
            Ok(Some(key_version))
        }
        // Array writes no prefix of its own; `SerializationArray`'s
        // `deserializeBinaryBulkStatePrefix` recurses into the element type's
        // prefix (confirmed at v26.6.1.1193-stable). This is how a leaf
        // `LowCardinality`'s 8-byte key version is consumed here, at the front of
        // the whole Array column, before the offsets.
        ChType::Array(inner) => read_state_prefix(reader, inner, column),
        // Tuple writes no prefix of its own; `SerializationTuple`'s
        // `deserializeBinaryBulkStatePrefix` loops over the elements in
        // declaration order and delegates to each (confirmed at
        // v26.6.1.1193-stable). So Tuple(LowCardinality(String), Int32) has the
        // LC 8-byte key version here, at the front of the whole Tuple column,
        // and nothing for the Int32.
        ChType::Tuple(elements) => {
            for (_, element_type) in elements {
                read_state_prefix(reader, element_type, column)?;
            }
            Ok(None)
        }
        // Map writes no prefix of its own; its prefix chain is
        // Map -> Array (nothing) -> Tuple -> key's prefix then value's prefix,
        // in that order (confirmed at v26.6.1.1193-stable, `SerializationMap`
        // delegating to the nested `Array(Tuple(...))` serialization). So
        // Map(LowCardinality(String), Int32) has the LC 8-byte key version at
        // the very front of the whole column, before the offsets.
        ChType::Map(key, value) => {
            read_state_prefix(reader, key, column)?;
            read_state_prefix(reader, value, column)
        }
        // Nullable writes no prefix of its own either;
        // `SerializationNullable::deserializeBinaryBulkStatePrefix` delegates to
        // the nested type (confirmed at v26.6.1.1193-stable,
        // `src/DataTypes/Serializations/SerializationNullable.cpp`). Only a
        // `Nullable(Tuple(...))` can nest a prefix-bearing type today (a
        // LowCardinality element), but recursing unconditionally keeps this
        // faithful to the server for any future nullable-wrappable container.
        ChType::Nullable(inner) => read_state_prefix(reader, inner, column),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// LowCardinality
// ---------------------------------------------------------------------------

/// Key serialization version this decoder accepts. The Native format always
/// uses `SharedDictionariesWithAdditionalKeys` (server
/// `KeysSerializationVersion`).
pub(crate) const LOW_CARDINALITY_KEY_VERSION: u64 = 1;

/// `NeedGlobalDictionaryBit` of the per-block index type word. Native never sets
/// it (the server rejects it for `native_format`), so the decoder rejects it too.
const LC_NEED_GLOBAL_DICTIONARY_BIT: u64 = 1 << 8;
/// `HasAdditionalKeysBit` of the per-block index type word. Always set in Native:
/// each block carries its own dictionary as "additional keys".
pub(crate) const LC_HAS_ADDITIONAL_KEYS_BIT: u64 = 1 << 9;
/// `NeedUpdateDictionary` of the per-block index type word. Native writes it
/// alongside `HasAdditionalKeysBit` for the per-block dictionary. The decoder
/// does not require it so older or synthetic payloads with only
/// `HasAdditionalKeysBit` still decode.
pub(crate) const LC_NEED_UPDATE_DICTIONARY_BIT: u64 = 1 << 10;

/// Decode one `LowCardinality(T)` column block into a dictionary `Column`.
///
/// Wire layout per block (server `SerializationLowCardinality`, confirmed at
/// v26.6.1.1193-stable; the per-column key-version prefix was already consumed by
/// [`read_state_prefix`]):
///
/// ```text
/// [8 bytes LE u64]  index_type_word   // bits 1:0 = index width (0=u8..3=u64),
///                                     // bit 9 = HasAdditionalKeysBit (set),
///                                     // bit 8 = NeedGlobalDictionaryBit (clear).
///                                     // Higher bits exist and are ignored on
///                                     // purpose: real payloads also set bit 10
///                                     // (NeedUpdateDictionary), so the decoder
///                                     // masks only the bits it acts on rather
///                                     // than rejecting a word it does not fully
///                                     // model.
/// [8 bytes LE u64]  num_keys          // dictionary entry count for THIS block
/// [num_keys values] dictionary        // inner-type serialized (String: varint
///                                     // len + bytes)
/// [8 bytes LE u64]  num_rows
/// [num_rows * w]    indexes           // raw LE, each an index into the block
///                                     // dictionary
/// ```
///
/// The dictionary is per block (additional keys); this core never concatenates
/// blocks, so each chunk gets its own dictionary as the `values` column.
///
/// For `LowCardinality(Nullable(T))` the removeNullable inner type is decoded
/// for the dictionary, and dictionary index 0 is the NULL sentinel (its on-wire
/// value is the inner default). Rows whose index is 0 become null in the Arrow
/// index validity bitmap, matching how Arrow represents a null in a dictionary
/// array, rather than carrying a dictionary entry.
fn decode_low_cardinality(
    reader: &mut ByteReader,
    inner: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<Column, DecodeError> {
    // Unwrap an inner Nullable. ClickHouse always nests Nullable inside
    // LowCardinality, never the reverse, so this is the only nullable form.
    let (nullable, dict_value_type) = match inner {
        ChType::Nullable(t) => (true, t.as_ref()),
        other => (false, other),
    };

    // Index type word. Native must not request a global dictionary, and must
    // request additional keys (the per-block dictionary). The low two bits are
    // the index width.
    let index_word = reader.read_u64_le()?;
    if index_word & LC_NEED_GLOBAL_DICTIONARY_BIT != 0 {
        return Err(DecodeError::InvalidLowCardinality {
            column: column.to_string(),
            reason: "NeedGlobalDictionaryBit is set; Native never uses a global dictionary",
        });
    }
    if index_word & LC_HAS_ADDITIONAL_KEYS_BIT == 0 {
        return Err(DecodeError::InvalidLowCardinality {
            column: column.to_string(),
            reason: "HasAdditionalKeysBit is clear; Native always carries a per-block dictionary",
        });
    }
    let index_width = match index_word & 0xFF {
        0 => 1usize, // UInt8
        1 => 2,      // UInt16
        2 => 4,      // UInt32
        3 => 8,      // UInt64
        _ => {
            return Err(DecodeError::InvalidLowCardinality {
                column: column.to_string(),
                reason: "index width tag is outside 0..=3",
            })
        }
    };

    // Per-block dictionary ("additional keys"): a count then that many inner
    // values. The dictionary value count comes from the wire, so bound it by the
    // bytes available before reserving, like the block header counts.
    let num_keys = usize::try_from(reader.read_u64_le()?).map_err(|_| {
        DecodeError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "LowCardinality dictionary size overflows usize",
        ))
    })?;
    check_header_count(num_keys, "LowCardinality dictionary size", reader)?;
    let values = decode_low_cardinality_dictionary(reader, dict_value_type, num_keys, column)?;

    // num_rows for this block, written again in the indexes stream. The block
    // header num_rows is authoritative; this must match it.
    let wire_rows = usize::try_from(reader.read_u64_le()?).map_err(|_| {
        DecodeError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "LowCardinality row count overflows usize",
        ))
    })?;
    if wire_rows != num_rows {
        return Err(DecodeError::InvalidLowCardinality {
            column: column.to_string(),
            reason: "indexes row count disagrees with the block row count",
        });
    }

    // Indexes: a raw LE array of `index_width` bytes per row.
    let (indices, validity) =
        decode_low_cardinality_indices(reader, index_width, num_rows, num_keys, nullable, column)?;

    let dict = match validity {
        Some(bm) => DictionaryColumn::new_nullable(indices, values, bm),
        None => DictionaryColumn::new(indices, values),
    };
    Ok(Column::Dictionary(dict))
}

/// Whether `dict_value_type` (the removeNullable inner of a `LowCardinality`) is
/// an inner type this crate decodes and ClickHouse permits inside
/// `LowCardinality`.
///
/// ClickHouse gates LowCardinality inners on
/// `IDataType::canBeInsideLowCardinality()`, checked in the
/// `DataTypeLowCardinality` constructor after `removeNullable` (confirmed at
/// v26.6.1.1193-stable). That predicate is true for `String`, `FixedString`, the
/// fixed-width numerics, and the number-backed temporals `Date`/`Date32`/
/// `DateTime` (`Bool` is a `UInt8`-backed number and also qualifies). It is false
/// for `DateTime64` and every `Decimal`, which are `DataTypeDecimalBase`
/// subclasses, so those are rejected here even though the crate decodes them as
/// ordinary columns. `UUID`/`IPv4`/`IPv6` are permitted by the server and decoded
/// by this crate, so they are in the allowlist: the dictionary body is the inner
/// type's plain bulk form (4 raw bytes per entry for `IPv4`, 16 raw bytes per
/// entry for `UUID`/`IPv6`), decoded through the shared per-type body decoder.
///
/// The fixed-width numeric and temporal inners require the server's
/// `allow_suspicious_low_cardinality_types=1` at table-creation time; that is a
/// server-side creation guard only and has no effect on the wire bytes or on
/// decoding a column the server already produced. `UUID` (like `String` and
/// `FixedString`) is allowed unconditionally; `IPv4`/`IPv6` need the suspicious
/// setting at creation, again with no wire effect.
pub(crate) fn is_low_cardinality_inner(dict_value_type: &ChType) -> bool {
    matches!(
        dict_value_type,
        ChType::Bool
            | ChType::Int8
            | ChType::Int16
            | ChType::Int32
            | ChType::Int64
            | ChType::UInt8
            | ChType::UInt16
            | ChType::UInt32
            | ChType::UInt64
            | ChType::Float32
            | ChType::Float64
            // The wide integers are `DataTypeNumberBase` subclasses whose
            // `canBeInsideLowCardinality()` is final-true, so
            // `LowCardinality(Int128)` etc. are legal on the wire (the server
            // ships tests `02125_low_cardinality_int256` and
            // `02459_low_cardinality_uint128_aggregator`). Unlike `Decimal`/
            // `Enum` (forbidden inners), they belong in this allowlist. Their
            // dictionary body is the plain fixed-width run (16/32 bytes per
            // entry), decoded through the shared per-type body decoder. The
            // `allow_suspicious_low_cardinality_types` setting gates CREATE only,
            // not the wire.
            | ChType::Int128
            | ChType::UInt128
            | ChType::Int256
            | ChType::UInt256
            | ChType::String
            | ChType::FixedString(_)
            | ChType::Date
            | ChType::Date32
            | ChType::DateTime { .. }
            | ChType::Uuid
            | ChType::Ipv4
            | ChType::Ipv6
    )
}

/// Decode the per-block dictionary values for a `LowCardinality(T)` column.
///
/// The dictionary is a plain column of the removeNullable inner type, serialized
/// with the inner type's `serializeBinaryBulk` (confirmed against
/// `SerializationLowCardinality` at v26.6.1.1193-stable): the same body bytes as a
/// normal column of T, carrying no per-column state prefix and no null map
/// (nullability is the index-0 sentinel in the index stream). So this defers to
/// the shared [`decode_column_body`] with `validity: None`, for any inner type in
/// the LowCardinality allowlist ([`is_low_cardinality_inner`]). An inner type the
/// crate does not decode or ClickHouse does not permit is rejected as
/// `UnsupportedType` rather than mis-decoded.
fn decode_low_cardinality_dictionary(
    reader: &mut ByteReader,
    dict_value_type: &ChType,
    num_keys: usize,
    column: &str,
) -> Result<Column, DecodeError> {
    if !is_low_cardinality_inner(dict_value_type) {
        return Err(DecodeError::UnsupportedType {
            column: column.to_string(),
            type_name: format!("LowCardinality({dict_value_type})"),
        });
    }
    decode_column_body(reader, dict_value_type, num_keys, None)
}

/// Read the raw index array and widen each native-width index into i32.
///
/// For a nullable inner type, wire index 0 is the NULL sentinel: those rows
/// become null in the returned validity bitmap, and their index is left at 0
/// (pointing at the harmless sentinel dictionary entry). An index value that
/// does not fit i32 is rejected, since Arrow dictionary indices are i32 and a
/// per-block dictionary that large is not a real Native payload.
fn decode_low_cardinality_indices(
    reader: &mut ByteReader,
    index_width: usize,
    num_rows: usize,
    num_keys: usize,
    nullable: bool,
    column: &str,
) -> Result<(Vec<i32>, Option<Bitmap>), DecodeError> {
    let total = num_rows.checked_mul(index_width).ok_or_else(|| {
        DecodeError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "LowCardinality index byte length overflows usize",
        ))
    })?;
    let raw = reader.read_slice(total)?;

    let mut indices = Vec::with_capacity(num_rows);
    // Per-row null map, only allocated for the nullable inner type. One byte per
    // row, 0x00 = present, 0x01 = null, the same encoding `from_ch_null_map`
    // consumes, so wire-index-0 rows are turned into Arrow nulls.
    let mut null_map = if nullable {
        Some(vec![0u8; num_rows])
    } else {
        None
    };

    for (row, chunk) in raw.chunks_exact(index_width).enumerate() {
        let raw_index: u64 = match index_width {
            1 => chunk[0] as u64,
            2 => u16::from_le_bytes([chunk[0], chunk[1]]) as u64,
            4 => u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as u64,
            8 => u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ]),
            // index_width came from the validated width tag; nothing else reaches here.
            _ => unreachable!("index width validated to 1/2/4/8"),
        };
        // Compare in u64 space: `num_keys` is a usize but `raw_index` can be a
        // full u64 (width 8), so an `as usize` cast on the index would truncate
        // on a 32-bit target and could let an out-of-range index slip the bound.
        if raw_index >= num_keys as u64 {
            return Err(DecodeError::InvalidLowCardinality {
                column: column.to_string(),
                reason: "index value points outside the block dictionary",
            });
        }
        if nullable && raw_index == 0 {
            // Sentinel: row is null. Keep the i32 index at 0; the validity
            // bitmap marks it null and the value is never read.
            if let Some(nm) = null_map.as_mut() {
                nm[row] = 0x01;
            }
            indices.push(0);
        } else {
            // Bounded by num_keys above, which `check_header_count` capped at the
            // remaining bytes, so this fits i32 for any real Native payload.
            let idx = i32::try_from(raw_index).map_err(|_| DecodeError::InvalidLowCardinality {
                column: column.to_string(),
                reason: "index value exceeds i32::MAX",
            })?;
            indices.push(idx);
        }
    }

    let validity = null_map.map(|nm| Bitmap::from_ch_null_map(&nm));
    Ok((indices, validity))
}

/// Decode a single column given its ChType.
///
/// Called only for blocks with `num_rows > 0`. The server runs
/// `deserializeBinaryBulkStatePrefix` per column per block, gated on the block
/// having rows, so the per-column state prefix is consumed here, not in the
/// zero-row path.
fn decode_column(
    reader: &mut ByteReader,
    ch_type: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<Column, DecodeError> {
    // Per-column bulk-state prefix. Zero bytes for every type except
    // LowCardinality, which reads its key version here; Array recurses into its
    // element type's prefix (so a leaf LowCardinality key version is consumed
    // here, before the offsets).
    read_state_prefix(reader, ch_type, column)?;
    decode_values(reader, ch_type, num_rows, column)
}

/// Decode a column's value payload once its per-column state prefix has been
/// consumed by [`read_state_prefix`].
///
/// Split out from [`decode_column`] so [`decode_array`] can decode its flattened
/// element column WITHOUT re-consuming a state prefix: `SerializationArray` emits
/// the element type's prefix once, at the very front of the Array column (before
/// the offsets), not again per element run.
fn decode_values(
    reader: &mut ByteReader,
    ch_type: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<Column, DecodeError> {
    // LowCardinality carries its own dictionary, indexes, and (for a Nullable
    // inner type) null handling, so it is decoded as a unit rather than going
    // through the Nullable null-map unwrap below.
    if let ChType::LowCardinality(inner) = ch_type {
        // A zero-length run carries no LowCardinality body at all: the server's
        // `SerializationLowCardinality::serializeBinaryBulkWithMultipleStreams`
        // early-returns whenever limit == 0, before writing the index-type word,
        // dictionary, row count, or indexes (confirmed at v26.6.1.1193-stable).
        // That early return is universal, not tied to any particular wrapper:
        // today the only zero-count entry point is an Array whose arrays are all
        // empty (a zero-row block skips column data entirely and never reaches
        // here), but any future one (Map values, Tuple elements) gets the same
        // absent body and takes this same gate.
        if num_rows == 0 {
            return Ok(empty_column(ch_type));
        }
        return decode_low_cardinality(reader, inner, num_rows, column);
    }

    // Array is offsets plus a flattened element column, decoded as a unit; its
    // element type's prefix was already consumed by the caller's
    // `read_state_prefix`.
    if let ChType::Array(inner) = ch_type {
        return decode_array(reader, inner, num_rows, column);
    }

    // Map is the Array(Tuple(keys, values)) wire layout decoded as a unit; like
    // Array it is never nullable at this level, so it dispatches before the
    // Nullable unwrap. The key/value prefixes were consumed by the caller's
    // `read_state_prefix`.
    if let ChType::Map(key, value) = ch_type {
        return decode_map(reader, key, value, num_rows, column);
    }

    let (nullable, inner) = match ch_type {
        ChType::Nullable(inner) => (true, inner.as_ref()),
        other => (false, other),
    };

    let validity = if nullable {
        Some(decode_null_map(reader, num_rows)?)
    } else {
        None
    };

    // Tuple is a container of element columns decoded as a unit (each element
    // recurses back through this function), dispatched after the Nullable
    // unwrap because `Nullable(Tuple(...))` is legal: its per-row null map
    // precedes the tuple body, the ordinary Nullable framing.
    if let ChType::Tuple(elements) = inner {
        return decode_tuple(reader, elements, num_rows, column, validity);
    }

    decode_column_body(reader, inner, num_rows, validity)
}

/// Decode one `Tuple(T1, ...)` column body into an Arrow struct `Column`.
///
/// Wire layout per block (server `SerializationTuple`, confirmed at
/// v26.6.1.1193-stable in `src/DataTypes/Serializations/SerializationTuple.cpp`;
/// any element state prefixes were already consumed by [`read_state_prefix`],
/// which recurses into every element in order):
///
/// ```text
/// [element 0 body]   // element 0's FULL run of num_rows rows, its normal bulk
/// [element 1 body]   // body WITHOUT its state prefix, then element 1's, ...
/// ...                // column-of-columns: no interleaving, no offsets, no
///                    // Tuple-level length framing
/// ```
///
/// Each element body is decoded recursively through [`decode_values`], so a
/// `Nullable`, `LowCardinality`, `Array`, or nested `Tuple` element composes.
/// The server asserts all element columns come out the same size
/// (`INCORRECT_DATA` in Native mode); that check is mirrored here, though every
/// element decode is driven by the same `num_rows` so it cannot fire in
/// practice.
///
/// The zero-element `Tuple()` has a special layout: exactly ONE literal ASCII
/// '0' byte (0x30) per row and nothing else. The server ignores the byte
/// values on read (`tryIgnore`), so they are skipped without validation;
/// truncation is still `UnexpectedEof`. A zero-length run (`num_rows == 0`,
/// reachable nested inside an empty `Array` run) writes and reads no bytes at
/// all, for the empty and non-empty element lists alike.
///
/// `validity` is the tuple-level null map of a `Nullable(Tuple(...))`, already
/// decoded by the caller; a null tuple row still carries placeholder values in
/// every element body.
fn decode_tuple(
    reader: &mut ByteReader,
    elements: &[(Option<String>, ChType)],
    num_rows: usize,
    column: &str,
    validity: Option<Bitmap>,
) -> Result<Column, DecodeError> {
    if elements.is_empty() {
        // Tuple(): one placeholder byte per row, values not validated (the
        // server writes '0' and ignores on read). `skip` bounds against the
        // bytes present, so truncation is UnexpectedEof.
        reader.skip(num_rows)?;
        return Ok(build_tuple_column(Vec::new(), num_rows, validity));
    }

    let mut fields = Vec::with_capacity(elements.len());
    for (_, element_type) in elements {
        let element = decode_values(reader, element_type, num_rows, column)?;
        // Mirror the server's equal-sizes assert. Unreachable in practice:
        // every element decode above is driven by the same num_rows.
        if element.len() != num_rows {
            return Err(DecodeError::InvalidTuple {
                column: column.to_string(),
                reason: "element column length disagrees with the block row count",
            });
        }
        fields.push(element);
    }
    Ok(build_tuple_column(fields, num_rows, validity))
}

/// Assemble a `Column::Tuple` through the `TupleColumn` constructors, keyed on
/// whether a tuple-level validity bitmap (a `Nullable(Tuple(...))`) is present.
/// The single construction point every decode path funnels through, so a
/// future caller cannot build a tuple column and forget to attach validity.
fn build_tuple_column(fields: Vec<Column>, len: usize, validity: Option<Bitmap>) -> Column {
    Column::Tuple(match validity {
        Some(bm) => TupleColumn::new_nullable(fields, len, bm),
        None => TupleColumn::new(fields, len),
    })
}

/// Decode one `Array(T)` column block into an Arrow list `Column`.
///
/// Wire layout per block (server `SerializationArray`, confirmed at
/// v26.6.1.1193-stable; the element type's state prefix was already consumed by
/// [`read_state_prefix`], which recurses into the element for an `Array`, so this
/// starts at the offsets):
///
/// ```text
/// [num_rows * 8]  offsets   // raw LE u64, cumulative ABSOLUTE end-offsets (the
///                           // element index one past this row's last element),
///                           // no leading zero, no count, monotonically
///                           // non-decreasing (equal adjacent = an empty row)
/// [element body]            // the flattened element column of length
///                           // `total_elements` (= the last offset), the element
///                           // type's normal bulk body WITHOUT its state prefix
/// ```
///
/// The decoded column prepends Arrow's leading `0` and widens each offset to
/// `i64`, so it exports as an Arrow LargeList (64-bit offsets). The element
/// column is decoded recursively through [`decode_values`] (its prefix already
/// consumed), so a nested `Array`, a `Nullable` element, or a `LowCardinality`
/// element all compose. The array itself is never nullable (server
/// `DataTypeArray::canBeInsideNullable()` is false), so there is no array-level
/// null map.
fn decode_array(
    reader: &mut ByteReader,
    inner: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<Column, DecodeError> {
    // Offsets: the shared walk reads and validates the run and builds the
    // Arrow-shaped offsets (leading 0, each wire offset widened to i64),
    // bounding both the allocation and the returned element count against the
    // bytes present.
    let mut offsets = Vec::new();
    let total_elements = read_array_offsets(reader, num_rows, column, Some(&mut offsets))?;

    // Element body: the flattened element column. The state prefix was consumed
    // by the caller's `read_state_prefix`, so decode the values only.
    let values = decode_values(reader, inner, total_elements, column)?;
    Ok(Column::Array(ArrayColumn::new(offsets, values)))
}

/// Decode one `Map(K, V)` column block into an Arrow list-of-struct `Column`.
///
/// On the Native wire a Map is ALWAYS the plain `Array(Tuple(keys, values))`
/// layout (server `SerializationMap`, confirmed at v26.6.1.1193-stable in
/// `src/DataTypes/Serializations/SerializationMap.cpp`): the same cumulative
/// `UInt64` end-offset run as `Array` (one per row, no leading zero), then the
/// flattened `Tuple(K, V)` body, i.e. K's full flattened run and then V's, per
/// the Tuple column-of-columns layout. The server's newer bucketed
/// `WITH_BUCKETS` on-disk serialization NEVER reaches the Native wire in
/// either direction: `NativeReader` builds its serializations via
/// `enableAllSupportedSerializations`, which leaves `map_serialization_version`
/// at `BASIC`, and `NativeWriter` goes through `IDataType::getSerializationInfo`'s
/// default, also `BASIC`. The key/value state prefixes were already consumed by
/// [`read_state_prefix`] (Map -> Array -> Tuple -> K then V), so this starts at
/// the offsets.
///
/// The nested tuple's "keys"/"values" names never appear on the wire; `entries`
/// is a two-field [`TupleColumn`] (keys then values) of length
/// `total_entries`. Both flattened runs are decoded recursively through
/// [`decode_values`], so a `LowCardinality` key, a `Nullable` or container
/// value, and a nested `Map` all compose, including the `limit == 0` gates for
/// an all-empty-maps block.
fn decode_map(
    reader: &mut ByteReader,
    key: &ChType,
    value: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<Column, DecodeError> {
    // Offsets: the shared Array walk (a Map's offsets are byte-identical to an
    // Array's), building the Arrow-shaped run with the leading 0 and bounding
    // the entry count against the bytes present.
    let mut offsets = Vec::new();
    let total_entries = read_array_offsets(reader, num_rows, column, Some(&mut offsets))?;

    // Flattened entries: the keys' full run then the values' full run, the
    // Tuple(K, V) body with prefixes already consumed. Both decodes are driven
    // by the same total, so the two fields cannot come out ragged.
    let keys = decode_values(reader, key, total_entries, column)?;
    let values = decode_values(reader, value, total_entries, column)?;
    // The entries tuple never carries validity: the wire has no null map here
    // (a map is never nullable at the entries level), so it goes through the
    // shared constructor with `None`.
    let entries = build_tuple_column(vec![keys, values], total_entries, None);
    Ok(Column::Map(MapColumn::new(offsets, entries)))
}

/// Read and validate one `Array` offsets run: exactly `num_rows` raw LE u64
/// cumulative absolute end-offsets. Shared by [`decode_array`] (which passes
/// `Some` and receives the Arrow-shaped offsets: the leading `0` plus one
/// i64-widened end-offset per row) and [`skip_array_data`] (which passes `None`
/// and only validates), so the allocating decode and the streaming completeness
/// scan can never drift apart on the framing or the rejection order.
///
/// Enforces, in order per offset: monotonically non-decreasing (the server
/// rejects a decrease as `INCORRECT_DATA` in Native mode), then representable
/// as i64 (the Arrow LargeList offset width). Returns `total_elements`, the
/// last offset (0 for a zero-row run, reachable for a nested empty inner
/// array), after bounding it against the remaining bytes via
/// [`check_header_count`] so a hostile offset cannot drive a huge element
/// decode in the caller.
fn read_array_offsets(
    reader: &mut ByteReader,
    num_rows: usize,
    column: &str,
    mut collect: Option<&mut Vec<i64>>,
) -> Result<usize, DecodeError> {
    // `checked_mul` guards a hostile num_rows that overflows usize when scaled
    // to bytes (mirrors `decode_primitive!`/`decode_fixed_binary_data`);
    // `read_slice` then bounds the run against the bytes present.
    let total_bytes = num_rows.checked_mul(8).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Array offset byte length overflows usize",
        )
    })?;
    let raw = reader.read_slice(total_bytes)?;
    if let Some(offsets) = collect.as_deref_mut() {
        // Reserve only after `read_slice` proved the bytes are present, so the
        // allocation is bounded by real input. `num_rows + 1` cannot overflow:
        // `num_rows * 8` did not just above.
        offsets.reserve(num_rows + 1);
        offsets.push(0i64);
    }

    let mut prev: u64 = 0;
    for chunk in raw.chunks_exact(8) {
        // `chunks_exact(8)` guarantees an 8-byte chunk, so the array conversion
        // cannot fail; this mirrors the big-endian arm of `decode_primitive!`,
        // which unwraps the same fixed-size `try_into`.
        let cur = u64::from_le_bytes(chunk.try_into().unwrap());
        if cur < prev {
            return Err(DecodeError::InvalidArray {
                column: column.to_string(),
                reason: "offsets are not monotonically non-decreasing",
            });
        }
        // Reject an offset in (i64::MAX, u64::MAX] on both paths identically.
        // Without this, the scan would only fail later via `check_header_count`
        // as `UnexpectedEof`, so `StreamDecoder` would treat a fully-present
        // corrupt block as "need more bytes" and stall instead of surfacing
        // `InvalidArray`.
        let widened = i64::try_from(cur).map_err(|_| DecodeError::InvalidArray {
            column: column.to_string(),
            reason: "offset exceeds i64::MAX",
        })?;
        if let Some(offsets) = collect.as_deref_mut() {
            offsets.push(widened);
        }
        prev = cur;
    }

    // total_elements is the last absolute offset. On a 32-bit target an offset
    // in (usize::MAX, i64::MAX] cannot index memory; it is reported as
    // `UnexpectedEof` under the same narrowing policy as `varint_usize` (an
    // element count that large can never be satisfied by the bytes present, so
    // the streaming decoder treats it as "need more bytes" rather than a
    // corruption it must surface). A no-op conversion on 64-bit targets. Bound
    // it against the remaining bytes BEFORE the caller recurses into the
    // element body.
    let total_elements = usize::try_from(prev).map_err(|_| {
        DecodeError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Array element count overflows usize",
        ))
    })?;
    check_header_count(total_elements, "Array element count", reader)?;
    Ok(total_elements)
}

/// Decode one column's value payload for a concrete inner type, after any
/// `Nullable` null map and `LowCardinality` state prefix have already been
/// consumed.
///
/// Shared by two callers, which is why it takes the post-unwrap `inner_type`
/// and a ready `validity` rather than the raw `ChType`:
///
/// - [`decode_column`] calls it for a normal column, passing the null map it
///   decoded for a `Nullable(T)` (`None` for a non-nullable column).
/// - [`decode_low_cardinality_dictionary`] calls it for a `LowCardinality(T)`
///   dictionary. The dictionary values are the inner type serialized with plain
///   `serializeBinaryBulk`, the same body bytes as a normal column of T, with no
///   state prefix and no null map (nullability is the index-0 sentinel in the
///   index stream), so it passes `validity: None`.
///
/// `inner_type` is always a concrete type: `Nullable` and `LowCardinality` are
/// unwrapped by the callers and only appear here as `unreachable!` arms.
fn decode_column_body(
    reader: &mut ByteReader,
    inner_type: &ChType,
    num_rows: usize,
    validity: Option<Bitmap>,
) -> Result<Column, DecodeError> {
    let column = match inner_type {
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
        // Temporal types are plain bulk integers on the wire; timezone and
        // precision are type metadata only and do not appear in the bytes. They
        // decode through the same primitive fast path as the numerics at their
        // faithful native width.
        ChType::Date => {
            let values = decode_primitive!(reader, num_rows, u16);
            Column::Date(PrimitiveColumn { values, validity })
        }
        ChType::Date32 => {
            let values = decode_primitive!(reader, num_rows, i32);
            Column::Date32(PrimitiveColumn { values, validity })
        }
        ChType::DateTime { .. } => {
            let values = decode_primitive!(reader, num_rows, u32);
            Column::DateTime(PrimitiveColumn { values, validity })
        }
        ChType::DateTime64 { .. } => {
            let values = decode_primitive!(reader, num_rows, i64);
            Column::DateTime64(PrimitiveColumn { values, validity })
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
        // IPv4 is a UInt32 in bulk: `SerializationIP<IPv4>` in
        // SerializationIPv4andIPv6.cpp serializes identically to
        // SerializationNumber<UInt32> (confirmed at v26.6.1.1193-stable). Reading 4
        // bytes as a little-endian u32 yields the standard IPv4 numeric value
        // (a<<24 | b<<16 | c<<8 | d), so it decodes through the same primitive
        // fast path as the numerics.
        ChType::Ipv4 => {
            let values = decode_primitive!(reader, num_rows, u32);
            Column::Ipv4(PrimitiveColumn { values, validity })
        }
        // IPv6 is num_rows * 16 raw bytes in network byte order (in6_addr,
        // big-endian), no per-row framing (`SerializationIP<IPv6>`, confirmed at
        // v26.6.1.1193-stable). The bytes pass through verbatim into a width-16
        // FixedBinaryColumn; byte reordering and host address objects are a
        // binding concern.
        ChType::Ipv6 => {
            let data = decode_fixed_binary_data(reader, num_rows, 16)?;
            match validity {
                Some(bm) => Column::Ipv6(FixedBinaryColumn::new_nullable(data, 16, bm)),
                None => Column::Ipv6(FixedBinaryColumn::new(data, 16)),
            }
        }
        // UUID is num_rows * 16 raw bytes, a POD dump of the UInt128 (items[0]
        // then items[1], each little-endian on LE servers), NOT RFC-4122 byte
        // order (`SerializationUUID.cpp`, confirmed at v26.6.1.1193-stable). Decode
        // is raw passthrough: the 16 wire bytes go into a width-16
        // FixedBinaryColumn unchanged, no reordering. The wire->RFC mapping
        // (rfc[i] = wire[7-i] for i in 0..7, rfc[i] = wire[23-i] for i in 8..15)
        // is documented in CODEC_CONTRACT.md for bindings only.
        ChType::Uuid => {
            let data = decode_fixed_binary_data(reader, num_rows, 16)?;
            match validity {
                Some(bm) => Column::Uuid(FixedBinaryColumn::new_nullable(data, 16, bm)),
                None => Column::Uuid(FixedBinaryColumn::new(data, 16)),
            }
        }
        // Enum8/Enum16 are byte-identical to Int8/Int16 on the wire
        // (`SerializationEnum` inherits `SerializationNumber` and overrides no
        // bulk method; confirmed at v26.6.1.1193-stable). The name->value map is
        // in the ChType only, so decode is the raw signed int through the same
        // primitive fast path.
        ChType::Enum8 { .. } => {
            let values = decode_primitive!(reader, num_rows, i8);
            Column::Enum8(PrimitiveColumn { values, validity })
        }
        ChType::Enum16 { .. } => {
            let values = decode_primitive!(reader, num_rows, i16);
            Column::Enum16(PrimitiveColumn { values, validity })
        }
        // Decimal(P, S) is a raw little-endian two's-complement fixed-width
        // integer per row (4/8/16/32 bytes by precision), no per-row framing and
        // no in-band precision/scale (`SerializationDecimalBase`'s final bulk
        // methods do a single contiguous read of sizeof(FieldType) * num_rows;
        // confirmed at v26.6.1.1193-stable). The physical buffer is identical to
        // a FixedSizeBinary of width bits/8, so it reuses the fixed-binary
        // single contiguous read. Decode is a host-agnostic passthrough: the
        // bytes are stored verbatim (correct on big-endian hosts too) and the
        // host value policy lives in the bindings, so the core needs no native
        // i128/i256.
        ChType::Decimal {
            precision,
            scale,
            bits,
        } => {
            let width = (*bits / 8) as usize;
            let data = decode_fixed_binary_data(reader, num_rows, width)?;
            match validity {
                Some(bm) => Column::Decimal(DecimalColumn::new_nullable(
                    data, width, *precision, *scale, bm,
                )),
                None => Column::Decimal(DecimalColumn::new(data, width, *precision, *scale)),
            }
        }
        // Wide integers are a raw contiguous little-endian fixed-width integer
        // per row (16 bytes for Int128/UInt128, 32 for Int256/UInt256), no
        // per-row framing (`SerializationNumber<T>`, the same template as
        // Int8..Int64; confirmed at v26.6.1.1193-stable, byte-identical to a
        // Decimal128/256 integer body). Decode is a host-agnostic passthrough
        // through the same fixed-binary single contiguous read as UUID/IPv6, so
        // the core needs no native i128/i256 and the bytes stay correct on
        // big-endian hosts; signedness lives in the ChType only. NOTE: this must
        // NOT go through `decode_primitive!` (which byte-swaps into a native
        // Vec<T> on big-endian hosts); the passthrough keeps the buffer verbatim.
        ChType::Int128 => {
            let data = decode_fixed_binary_data(reader, num_rows, 16)?;
            match validity {
                Some(bm) => Column::Int128(FixedBinaryColumn::new_nullable(data, 16, bm)),
                None => Column::Int128(FixedBinaryColumn::new(data, 16)),
            }
        }
        ChType::UInt128 => {
            let data = decode_fixed_binary_data(reader, num_rows, 16)?;
            match validity {
                Some(bm) => Column::UInt128(FixedBinaryColumn::new_nullable(data, 16, bm)),
                None => Column::UInt128(FixedBinaryColumn::new(data, 16)),
            }
        }
        ChType::Int256 => {
            let data = decode_fixed_binary_data(reader, num_rows, 32)?;
            match validity {
                Some(bm) => Column::Int256(FixedBinaryColumn::new_nullable(data, 32, bm)),
                None => Column::Int256(FixedBinaryColumn::new(data, 32)),
            }
        }
        ChType::UInt256 => {
            let data = decode_fixed_binary_data(reader, num_rows, 32)?;
            match validity {
                Some(bm) => Column::UInt256(FixedBinaryColumn::new_nullable(data, 32, bm)),
                None => Column::UInt256(FixedBinaryColumn::new(data, 32)),
            }
        }
        // Defense in depth: `parse_ch_type` rejects a wrapper nested where the
        // single-level unwrap in `decode_values` cannot handle it, and
        // `LowCardinality`, `Array`, and `Tuple` are dispatched by `decode_values`
        // before reaching here (a `Nullable` is unwrapped there too), so these arms
        // cannot occur for any type this decoder produces. None of them is a legal
        // inner of a `LowCardinality` dictionary either, the other caller. Return
        // an error rather than panic so a future regression degrades to a clean
        // decode error instead of undefined behavior at an FFI boundary.
        ChType::Nullable(_)
        | ChType::LowCardinality(_)
        | ChType::Array(_)
        | ChType::Tuple(_)
        | ChType::Map(..) => {
            return Err(DecodeError::UnsupportedType {
                column: String::new(),
                type_name: inner_type.to_string(),
            })
        }
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
        ChType::Int8 => Column::Int8(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::Int16 => Column::Int16(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::Int32 => Column::Int32(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::Int64 => Column::Int64(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::UInt8 => Column::UInt8(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::UInt16 => Column::UInt16(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::UInt32 => Column::UInt32(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::UInt64 => Column::UInt64(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::Float32 => Column::Float32(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::Float64 => Column::Float64(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::Date => Column::Date(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::Date32 => Column::Date32(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::DateTime { .. } => Column::DateTime(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::DateTime64 { .. } => Column::DateTime64(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::String => Column::Utf8(match empty_validity {
            Some(bm) => Utf8Column::new_nullable(vec![0], vec![], bm),
            None => Utf8Column::new(vec![0], vec![]),
        }),
        ChType::FixedString(width) => Column::FixedBinary(match empty_validity {
            Some(bm) => FixedBinaryColumn::new_nullable(vec![], *width, bm),
            None => FixedBinaryColumn::new(vec![], *width),
        }),
        ChType::Ipv4 => Column::Ipv4(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        // IPv6 and UUID are width-16 fixed binary; the empty column keeps the
        // width and the (nullable) empty validity bitmap, like FixedString.
        ChType::Ipv6 => Column::Ipv6(match empty_validity {
            Some(bm) => FixedBinaryColumn::new_nullable(vec![], 16, bm),
            None => FixedBinaryColumn::new(vec![], 16),
        }),
        ChType::Uuid => Column::Uuid(match empty_validity {
            Some(bm) => FixedBinaryColumn::new_nullable(vec![], 16, bm),
            None => FixedBinaryColumn::new(vec![], 16),
        }),
        // Enum8/Enum16 empty columns are the empty signed-int buffer, like the
        // matching Int8/Int16; the name->value map stays in the ChType.
        ChType::Enum8 { .. } => Column::Enum8(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::Enum16 { .. } => Column::Enum16(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        // Decimal empty column: an empty fixed-width buffer keeping precision,
        // scale, and width, like FixedString. width = bits / 8.
        ChType::Decimal {
            precision,
            scale,
            bits,
        } => {
            let width = (*bits / 8) as usize;
            Column::Decimal(match empty_validity {
                Some(bm) => DecimalColumn::new_nullable(vec![], width, *precision, *scale, bm),
                None => DecimalColumn::new(vec![], width, *precision, *scale),
            })
        }
        // Wide-int empty columns are an empty width-16/32 fixed-binary buffer
        // keeping the width (and the nullable empty validity bitmap), like the
        // UUID/IPv6/FixedString empties.
        ChType::Int128 => Column::Int128(match empty_validity {
            Some(bm) => FixedBinaryColumn::new_nullable(vec![], 16, bm),
            None => FixedBinaryColumn::new(vec![], 16),
        }),
        ChType::UInt128 => Column::UInt128(match empty_validity {
            Some(bm) => FixedBinaryColumn::new_nullable(vec![], 16, bm),
            None => FixedBinaryColumn::new(vec![], 16),
        }),
        ChType::Int256 => Column::Int256(match empty_validity {
            Some(bm) => FixedBinaryColumn::new_nullable(vec![], 32, bm),
            None => FixedBinaryColumn::new(vec![], 32),
        }),
        ChType::UInt256 => Column::UInt256(match empty_validity {
            Some(bm) => FixedBinaryColumn::new_nullable(vec![], 32, bm),
            None => FixedBinaryColumn::new(vec![], 32),
        }),
        // A zero-row block reads no LowCardinality prefix or data (the server
        // gates `readData` on having rows), so the empty dictionary column has no
        // indices and an empty values dictionary. The values column is an empty
        // column of the (removeNullable) inner type, built by recursing here; a
        // nullable inner type carries an empty index validity bitmap, matching the
        // other nullable empties.
        ChType::LowCardinality(lc_inner) => {
            let empty_values = match lc_inner.as_ref() {
                ChType::Nullable(t) => empty_column(t),
                other => empty_column(other),
            };
            let nullable_inner = matches!(lc_inner.as_ref(), ChType::Nullable(_));
            Column::Dictionary(if nullable_inner {
                DictionaryColumn::new_nullable(vec![], empty_values, Bitmap::from_ch_null_map(&[]))
            } else {
                DictionaryColumn::new(vec![], empty_values)
            })
        }
        // A zero-row block reads no Array offsets or element body (the server
        // gates `readData` on having rows), so the empty column is offsets `[0]`
        // (len 0) with an empty element column built by recursing here. The recursion
        // handles a `Nullable`, `LowCardinality`, or nested `Array` element.
        ChType::Array(array_inner) => {
            Column::Array(ArrayColumn::new(vec![0i64], empty_column(array_inner)))
        }
        // A zero-row block reads no Tuple element bodies (and no Tuple()
        // placeholder bytes), so the empty column is one empty element column
        // per declared element, built by recursing here, at length 0. A
        // `Nullable(Tuple)` carries the empty tuple-level validity bitmap like
        // the other nullable empties.
        ChType::Tuple(elements) => {
            let fields = elements.iter().map(|(_, t)| empty_column(t)).collect();
            build_tuple_column(fields, 0, empty_validity)
        }
        // A zero-row block reads no Map offsets or entries (the server gates
        // `readData` on having rows), so the empty column is offsets `[0]`
        // (len 0) over an empty two-field entries tuple built by recursing here.
        ChType::Map(key, value) => Column::Map(MapColumn::new(
            vec![0i64],
            build_tuple_column(vec![empty_column(key), empty_column(value)], 0, None),
        )),
        // The outer `Nullable` was unwrapped above, and `parse_ch_type` never
        // produces a `Nullable` directly inside a `Nullable`, so `inner` is never
        // `Nullable` here. Unlike the decode and scan paths this constructor is
        // infallible (it returns a `Column`, not a `Result`), so the invariant is
        // asserted rather than surfaced as an error.
        ChType::Nullable(_) => {
            unreachable!("Nullable inner already unwrapped; parse_ch_type rejects nested Nullable")
        }
    }
}

// ---------------------------------------------------------------------------
// Block info preamble
// ---------------------------------------------------------------------------

/// Consume the `BlockInfo` preamble that precedes each block when the producer
/// used a protocol revision > 0 (server `BlockInfo::read` in
/// `src/Core/BlockInfo.cpp`, confirmed at v26.6.1.1193-stable).
///
/// `BlockInfo` is a self-describing, field-tagged structure: each field is a
/// varint field number followed by the field value, and a field number of 0
/// terminates. The decoder reads the known fields and discards their values,
/// since the columnar decode does not use them:
///
/// - field 1 `is_overflows`: 1 byte.
/// - field 2 `bucket_num`: Int32, 4 bytes little-endian.
/// - field 3 `out_of_order_buckets`: a varint count then that many Int32 values
///   (written at server revision >= 54480).
///
/// Parsing by field number is revision independent: the older 8-byte two-field
/// preamble and the current 10-byte three-field preamble both decode correctly.
/// An unknown field number is rejected, matching the server, which throws.
///
/// Returns `Ok(false)` if the stream ends cleanly before any block info byte (a
/// block boundary at end of stream), or `Ok(true)` once a full preamble has been
/// consumed.
fn read_block_info(reader: &mut ByteReader) -> Result<bool, DecodeError> {
    let mut field_num = match reader.read_varint() {
        Ok(n) => n,
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(e.into()),
    };

    while field_num != 0 {
        match field_num {
            1 => reader.skip(1)?, // is_overflows
            2 => reader.skip(4)?, // bucket_num (Int32)
            3 => {
                // out_of_order_buckets: a varint count then that many Int32 values.
                let count = varint_usize(reader.read_varint()?, "out_of_order_buckets count")?;
                reader.skip(count.saturating_mul(4))?;
            }
            other => return Err(DecodeError::InvalidBlockInfo { field_num: other }),
        }
        field_num = reader.read_varint()?;
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// Block decode
// ---------------------------------------------------------------------------

/// Decode a single Native format block from a slice reader.
///
/// `reader` must be positioned at a block boundary. On success the reader has
/// advanced past exactly one block. If the block is not fully present in the
/// reader's bytes, the returned error is `DecodeError::Io` with kind
/// `UnexpectedEof`, which the streaming decoder reads as "need more bytes". A
/// clean end-of-stream at a block boundary returns `Ok(None)`.
///
/// `decode_all_bytes` and `StreamDecoder` both drive the decode through this
/// entry point. `StreamDecoder` first runs [`block_end`] to confirm a full
/// block is buffered, so it never reaches the allocating decode for a partial
/// block.
pub fn decode_next_block(
    reader: &mut ByteReader,
    options: &DecodeOptions,
) -> Result<Option<ColBatch>, DecodeError> {
    // A BlockInfo preamble precedes each block when the producer used a protocol
    // revision > 0. Its first byte is also where a clean end-of-stream boundary
    // falls, so `read_block_info` reports that case as `Ok(false)`.
    if options.protocol_revision > 0 {
        if !read_block_info(reader)? {
            return Ok(None);
        }
        let num_cols = varint_usize(reader.read_varint()?, "column count")?;
        let num_rows = varint_usize(reader.read_varint()?, "row count")?;
        return Ok(Some(decode_block_body(
            reader, options, num_cols, num_rows,
        )?));
    }

    // No protocol framing. End of stream falls on the column-count varint.
    let num_cols = match reader.read_varint() {
        Ok(n) => varint_usize(n, "column count")?,
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let num_rows = varint_usize(reader.read_varint()?, "row count")?;
    Ok(Some(decode_block_body(
        reader, options, num_cols, num_rows,
    )?))
}

/// Read one column header: name, type string, and the optional custom
/// serialization marker. Returns the parsed `ChType` plus the column name.
///
/// Shared by the allocating decode and the allocation-free completeness scan so
/// the two cannot drift on header framing or on which types and serializations
/// are accepted.
fn read_column_header(
    reader: &mut ByteReader,
    options: &DecodeOptions,
) -> Result<(String, ChType), DecodeError> {
    let col_name = reader.read_varint_string()?;
    let type_name = reader.read_varint_string()?;

    // Per-column custom-serialization marker, present at revision >= 54454, for
    // every column regardless of row count. One byte: 0 = default. A nonzero
    // value selects a custom serialization (sparse, detached, ...) whose layout
    // this crate does not decode, so reject it rather than misread the column
    // data that follows.
    if options.protocol_revision >= DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION {
        let marker = reader.read_u8()?;
        if marker != 0 {
            return Err(DecodeError::UnsupportedSerialization {
                column: col_name,
                serialization_byte: marker,
            });
        }
    }

    let ch_type = parse_ch_type(&type_name).ok_or_else(|| DecodeError::UnsupportedType {
        column: col_name.clone(),
        type_name: type_name.clone(),
    })?;

    validate_header_type(&col_name, &ch_type)?;

    Ok((col_name, ch_type))
}

/// Reject a header type this crate parses but cannot decode, at header-read time.
///
/// The core case is a `LowCardinality` whose (removeNullable) inner type is not in
/// [`is_low_cardinality_inner`]. Checking here, in the header path shared by the
/// allocating decode, the completeness scan, and the zero-row `empty_column` path,
/// makes all three agree on which columns are accepted. Without it a zero-row
/// `LowCardinality(Decimal(9, 4))` block would decode (its `empty_column` never
/// consults the allowlist) while the same type with rows errors, an inconsistency
/// the streaming decoder could hit as a block fills.
///
/// It recurses through container/wrapper types so a forbidden `LowCardinality`
/// inner nested inside an `Array` (e.g. `Array(LowCardinality(Decimal(9, 4)))`) is
/// rejected regardless of row count. A row-bearing block rejects it in
/// `decode_low_cardinality_dictionary`, but the zero-row `empty_column` path
/// recurses past the array without consulting the allowlist, so the two would
/// disagree without this recursion. The recursion is bounded: it runs only after
/// [`parse_ch_type`] succeeds, and that parser caps nesting at [`MAX_TYPE_DEPTH`].
fn validate_header_type(col_name: &str, ch_type: &ChType) -> Result<(), DecodeError> {
    match ch_type {
        ChType::LowCardinality(inner) => {
            let dict_value_type = match inner.as_ref() {
                ChType::Nullable(t) => t.as_ref(),
                other => other,
            };
            if !is_low_cardinality_inner(dict_value_type) {
                return Err(DecodeError::UnsupportedType {
                    column: col_name.to_string(),
                    type_name: format!("LowCardinality({dict_value_type})"),
                });
            }
            Ok(())
        }
        // Recurse into the element/inner so a forbidden LC nested inside a
        // container is caught at header time on every path. `Nullable`'s inner is
        // usually concrete (the parser rejects a wrapper inside `Nullable`, with
        // `Tuple` the one legal container), so its recursion mostly matters for a
        // `Nullable(Tuple(...))`.
        ChType::Array(inner) => validate_header_type(col_name, inner),
        ChType::Nullable(inner) => validate_header_type(col_name, inner),
        ChType::Tuple(elements) => {
            for (_, element_type) in elements {
                validate_header_type(col_name, element_type)?;
            }
            Ok(())
        }
        // A Map key must satisfy the server's key constraint; a header that
        // violates it never comes from an honest server, and accepting it
        // would decode a column the type system says cannot exist. Both
        // children then recurse like the Tuple elements.
        ChType::Map(key, value) => {
            if !is_valid_map_key_type(key) {
                return Err(DecodeError::UnsupportedType {
                    column: col_name.to_string(),
                    type_name: format!("Map({key}, {value})"),
                });
            }
            validate_header_type(col_name, key)?;
            validate_header_type(col_name, value)
        }
        _ => Ok(()),
    }
}

/// Whether `key` is a legal `Map(K, V)` key type, the server's
/// `DataTypeMap::isValidKeyType` (`!isNullableOrLowCardinalityNullable`,
/// confirmed at v26.6.1.1193-stable): `Nullable(K)` and
/// `LowCardinality(Nullable(K))` keys are forbidden; a plain
/// `LowCardinality(K)` key is legal. `pub(crate)` so the encoder's validation
/// enforces the same constraint on caller-constructed types.
pub(crate) fn is_valid_map_key_type(key: &ChType) -> bool {
    match key {
        ChType::Nullable(_) => false,
        ChType::LowCardinality(inner) => !matches!(inner.as_ref(), ChType::Nullable(_)),
        _ => true,
    }
}

/// Narrow a `u64` varint (a count or length read from the wire) to `usize`.
///
/// On a 32-bit target a value above `usize::MAX` would truncate under a raw `as
/// usize` cast and then misalign the row or byte walk instead of erroring cleanly;
/// `try_from` turns that into an error. Reported as `UnexpectedEof` (matching the
/// LowCardinality counts, which already do this): a value that large can never be
/// satisfied by the bytes present, and the streaming decoder treats it as "need
/// more bytes" rather than a corruption it must surface. On 64-bit targets this is
/// a no-op conversion the compiler removes.
fn varint_usize(value: u64, what: &str) -> io::Result<usize> {
    usize::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("{what} overflows usize"),
        )
    })
}

/// Reject a row or column count larger than the bytes still available.
///
/// `num_cols` and `num_rows` come from an untrusted block header. Every column
/// header and every row of data occupies at least one byte on the wire, so a
/// count larger than `reader.remaining()` cannot be satisfied. Catching it here
/// keeps a hostile count from reaching a `Vec::with_capacity` that would abort
/// the process on an oversized request, and bounds every capacity reservation
/// in the block body at the input size. Reported as `UnexpectedEof` so the
/// streaming decoder treats a truncated stream as "need more bytes".
fn check_header_count(count: usize, what: &str, reader: &ByteReader) -> Result<(), DecodeError> {
    let remaining = reader.remaining();
    if count > remaining {
        return Err(DecodeError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("{what} ({count}) exceeds remaining bytes ({remaining})"),
        )));
    }
    Ok(())
}

/// Decode one block body, the per-column headers and data, after the column and
/// row counts have already been read.
fn decode_block_body(
    reader: &mut ByteReader,
    options: &DecodeOptions,
    num_cols: usize,
    num_rows: usize,
) -> Result<ColBatch, DecodeError> {
    check_header_count(num_cols, "column count", reader)?;
    check_header_count(num_rows, "row count", reader)?;

    let mut fields = Vec::with_capacity(num_cols);
    let mut columns = Vec::with_capacity(num_cols);

    for _ in 0..num_cols {
        let (col_name, ch_type) = read_column_header(reader, options)?;

        if num_rows == 0 {
            columns.push(empty_column(&ch_type));
        } else {
            columns.push(decode_column(reader, &ch_type, num_rows, &col_name)?);
        }

        fields.push(Field {
            name: col_name,
            ch_type,
        });
    }

    let schema = Schema::new(fields);
    Ok(ColBatch::new(schema, columns, num_rows))
}

// ---------------------------------------------------------------------------
// Completeness scan
// ---------------------------------------------------------------------------

/// Walk the framing of one block without allocating column buffers, and report
/// where the block ends in `data`.
///
/// Returns:
/// - `Ok(Some(end))`: a complete block occupies `data[..end]`.
/// - `Ok(None)`: `data` ends cleanly at a block boundary (no block present).
/// - `Err(Io(UnexpectedEof))`: a block has started but is not fully buffered yet
///   (the caller should wait for more bytes).
/// - `Err(_)`: a real decode error (unsupported type/serialization, bad
///   BlockInfo field, varint overflow, invalid UTF-8 in a header), which is
///   surfaced even before the whole block is buffered, exactly as the real
///   decode would surface it.
///
/// The streaming decoder calls this before [`decode_next_block`] so it never
/// allocates and discards column buffers for a block that has not fully arrived.
/// It shares [`read_block_info`] and [`read_column_header`] with the real
/// decode; only [`skip_column_data`] is scan specific, and it walks the exact
/// same wire bytes the per-type decoders consume.
pub fn block_end(data: &[u8], options: &DecodeOptions) -> Result<Option<usize>, DecodeError> {
    let mut reader = ByteReader::new(data);

    if options.protocol_revision > 0 {
        if !read_block_info(&mut reader)? {
            return Ok(None);
        }
    } else if reader.remaining() == 0 {
        return Ok(None);
    }

    let num_cols = varint_usize(reader.read_varint()?, "column count")?;
    let num_rows = varint_usize(reader.read_varint()?, "row count")?;

    for _ in 0..num_cols {
        let (name, ch_type) = read_column_header(&mut reader, options)?;
        if num_rows > 0 {
            skip_column_data(&mut reader, &ch_type, num_rows, &name)?;
        }
    }

    Ok(Some(reader.position()))
}

/// Advance `reader` past one column's data without materializing it.
///
/// Fixed-width types have a computable byte length; String scans the per-value
/// varint length prefixes. This must consume exactly the bytes the matching
/// decoder in `decode_column` consumes, including the per-column state prefix.
/// Called only for blocks with `num_rows > 0`, matching `decode_column`.
fn skip_column_data(
    reader: &mut ByteReader,
    ch_type: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<(), DecodeError> {
    // Per-column bulk-state prefix, the same step `decode_column` runs. Zero
    // bytes for every type except LowCardinality; Array, Tuple, and Nullable
    // recurse into their element/inner prefixes.
    read_state_prefix(reader, ch_type, column)?;
    skip_values(reader, ch_type, num_rows, column)
}

/// Advance `reader` past one column's value payload once its per-column state
/// prefix has been consumed, the scan-side mirror of [`decode_values`]. Split out
/// so [`skip_array_data`] can walk its flattened element column without
/// re-consuming a state prefix, exactly as [`decode_array`] decodes it.
fn skip_values(
    reader: &mut ByteReader,
    ch_type: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<(), DecodeError> {
    if let ChType::LowCardinality(inner) = ch_type {
        // A zero-length run has no LowCardinality body bytes at all (see the
        // matching gate in `decode_values`), so there is nothing to walk.
        if num_rows == 0 {
            return Ok(());
        }
        return skip_low_cardinality_data(reader, inner, num_rows, column);
    }

    if let ChType::Array(inner) = ch_type {
        return skip_array_data(reader, inner, num_rows, column);
    }

    // Map before the Nullable unwrap, mirroring `decode_values`: a map is
    // never nullable at this level.
    if let ChType::Map(key, value) = ch_type {
        return skip_map_data(reader, key, value, num_rows, column);
    }

    let inner = match ch_type {
        ChType::Nullable(inner) => {
            reader.skip(num_rows)?; // null map: 1 byte per row
            inner.as_ref()
        }
        other => other,
    };

    // Tuple after the Nullable unwrap, mirroring `decode_values`: a
    // `Nullable(Tuple(...))` walks its per-row null map above, then the tuple
    // body.
    if let ChType::Tuple(elements) = inner {
        return skip_tuple_data(reader, elements, num_rows, column);
    }

    skip_column_body(reader, inner, num_rows)
}

/// Walk one `Tuple(T1, ...)` column body (after its element state prefixes and
/// any tuple-level null map) in the completeness scan, consuming exactly what
/// [`decode_tuple`] reads: each element's full `num_rows` run in declaration
/// order, or, for the zero-element `Tuple()`, the one placeholder byte per row
/// (skipped without validating its value, matching the decode).
fn skip_tuple_data(
    reader: &mut ByteReader,
    elements: &[(Option<String>, ChType)],
    num_rows: usize,
    column: &str,
) -> Result<(), DecodeError> {
    if elements.is_empty() {
        reader.skip(num_rows)?;
        return Ok(());
    }
    for (_, element_type) in elements {
        skip_values(reader, element_type, num_rows, column)?;
    }
    Ok(())
}

/// Walk one `Map(K, V)` column block (after its key/value state prefixes) in
/// the completeness scan, consuming exactly what [`decode_map`] reads: the
/// `num_rows` raw LE u64 offsets through the same validated
/// [`read_array_offsets`] walk (here with `collect: None`), then the flattened
/// key run and the flattened value run.
fn skip_map_data(
    reader: &mut ByteReader,
    key: &ChType,
    value: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<(), DecodeError> {
    let total_entries = read_array_offsets(reader, num_rows, column, None)?;
    skip_values(reader, key, total_entries, column)?;
    skip_values(reader, value, total_entries, column)
}

/// Walk one `Array(T)` column block (after its element state prefix) in the
/// completeness scan, consuming exactly what [`decode_array`] reads: the
/// `num_rows` raw LE u64 offsets and then the flattened element body.
///
/// The offsets are read and validated through the same [`read_array_offsets`]
/// walk the decode uses (here with `collect: None`, so nothing is
/// materialized), so the streaming scan surfaces the same
/// [`DecodeError::InvalidArray`] rejections in the same order rather than
/// walking framing the decode refuses (mirroring how
/// [`skip_low_cardinality_data`] mirrors [`decode_low_cardinality`]'s
/// rejections).
fn skip_array_data(
    reader: &mut ByteReader,
    inner: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<(), DecodeError> {
    let total_elements = read_array_offsets(reader, num_rows, column, None)?;
    skip_values(reader, inner, total_elements, column)
}

/// Advance `reader` past one column's value payload for a concrete inner type,
/// the scan-side mirror of [`decode_column_body`]. Called after any `Nullable`
/// null map and `LowCardinality` state prefix have been consumed, so it walks
/// exactly the body bytes the matching decode reads, and is shared by
/// [`skip_column_data`] (normal columns) and [`skip_low_cardinality_data`]
/// (dictionary values).
fn skip_column_body(
    reader: &mut ByteReader,
    inner_type: &ChType,
    num_rows: usize,
) -> Result<(), DecodeError> {
    match inner_type {
        // Enum8 is 1 byte/row (like Int8); Enum16 is 2 bytes/row (like Int16).
        ChType::Bool | ChType::Int8 | ChType::UInt8 | ChType::Enum8 { .. } => {
            reader.skip(num_rows)?
        }
        ChType::Int16 | ChType::UInt16 | ChType::Date | ChType::Enum16 { .. } => {
            reader.skip(num_rows.saturating_mul(2))?
        }
        ChType::Int32
        | ChType::UInt32
        | ChType::Float32
        | ChType::Date32
        | ChType::DateTime { .. }
        | ChType::Ipv4 => reader.skip(num_rows.saturating_mul(4))?,
        ChType::Int64 | ChType::UInt64 | ChType::Float64 | ChType::DateTime64 { .. } => {
            reader.skip(num_rows.saturating_mul(8))?
        }
        ChType::FixedString(width) => reader.skip(num_rows.saturating_mul(*width))?,
        // UUID and IPv6 are 16 raw bytes per row, the same body shape as
        // FixedString(16).
        ChType::Uuid | ChType::Ipv6 => reader.skip(num_rows.saturating_mul(16))?,
        // Decimal(P, S) is bits/8 raw bytes per row (4/8/16/32 by precision),
        // the same contiguous-buffer shape as FixedString(bits/8).
        ChType::Decimal { bits, .. } => {
            reader.skip(num_rows.saturating_mul((*bits / 8) as usize))?
        }
        // Wide integers are 16 raw bytes per row for the 128-bit pair and 32 for
        // the 256-bit pair, the same contiguous-buffer shape as FixedString.
        ChType::Int128 | ChType::UInt128 => reader.skip(num_rows.saturating_mul(16))?,
        ChType::Int256 | ChType::UInt256 => reader.skip(num_rows.saturating_mul(32))?,
        ChType::String => {
            for _ in 0..num_rows {
                let len = varint_usize(reader.read_varint()?, "String value length")?;
                reader.skip(len)?;
            }
        }
        // `read_column_header` already rejected unsupported types, Nullable is
        // unwrapped by the callers, and LowCardinality, Array, Tuple, and Map
        // are dispatched by `skip_values` above. Defense in depth:
        // `parse_ch_type` also rejects a wrapper nested where the callers'
        // single-level unwrap cannot reach it, so these arms cannot occur.
        // Return an error rather than panic to keep the streaming scan
        // panic-free even if that guarantee ever regresses (a panic here is
        // undefined behavior across FFI).
        ChType::Nullable(_)
        | ChType::LowCardinality(_)
        | ChType::Array(_)
        | ChType::Tuple(_)
        | ChType::Map(..) => {
            return Err(DecodeError::UnsupportedType {
                column: String::new(),
                type_name: inner_type.to_string(),
            })
        }
    }

    Ok(())
}

/// Walk one `LowCardinality(T)` column block (after its key-version prefix) in
/// the completeness scan, consuming exactly what [`decode_low_cardinality`]
/// reads: the index type word, the per-block dictionary, the row count, and the
/// raw index array. Mirrors the decode path so the two cannot drift.
fn skip_low_cardinality_data(
    reader: &mut ByteReader,
    inner: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<(), DecodeError> {
    let dict_value_type = match inner {
        ChType::Nullable(t) => t.as_ref(),
        other => other,
    };

    // Index type word. Mirror the decode-side rejections
    // ([`decode_low_cardinality`]) exactly, not just the index width: a hostile
    // flags word that sets `NeedGlobalDictionaryBit` or clears
    // `HasAdditionalKeysBit` would make the scan walk framing the decode refuses,
    // so the scan would either misreport the block length or stall the
    // `StreamDecoder` with a misleading truncation error instead of surfacing the
    // same `InvalidLowCardinality` the decode returns.
    let index_word = reader.read_u64_le()?;
    if index_word & LC_NEED_GLOBAL_DICTIONARY_BIT != 0 {
        return Err(DecodeError::InvalidLowCardinality {
            column: column.to_string(),
            reason: "NeedGlobalDictionaryBit is set; Native never uses a global dictionary",
        });
    }
    if index_word & LC_HAS_ADDITIONAL_KEYS_BIT == 0 {
        return Err(DecodeError::InvalidLowCardinality {
            column: column.to_string(),
            reason: "HasAdditionalKeysBit is clear; Native always carries a per-block dictionary",
        });
    }
    let index_width = match index_word & 0xFF {
        0 => 1usize,
        1 => 2,
        2 => 4,
        3 => 8,
        _ => {
            return Err(DecodeError::InvalidLowCardinality {
                column: column.to_string(),
                reason: "index width tag is outside 0..=3",
            })
        }
    };

    // Per-block dictionary: a count then that many inner values. Reject an inner
    // type outside the LowCardinality allowlist exactly as the decode path does,
    // so the scan and the decode agree on which columns are accepted.
    let num_keys = usize::try_from(reader.read_u64_le()?).map_err(|_| {
        DecodeError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "LowCardinality dictionary size overflows usize",
        ))
    })?;
    if !is_low_cardinality_inner(dict_value_type) {
        return Err(DecodeError::UnsupportedType {
            column: column.to_string(),
            type_name: format!("LowCardinality({dict_value_type})"),
        });
    }
    skip_column_body(reader, dict_value_type, num_keys)?;

    // Row count word, then the raw index array.
    reader.skip(8)?; // num_rows (re-stated in the indexes stream)
    reader.skip(num_rows.saturating_mul(index_width))?;
    Ok(())
}

/// Decode all blocks from a complete byte buffer into a `ChunkedBatch`.
///
/// Each Native block becomes its own chunk — blocks are NOT concatenated.
/// The schema is taken from the first decoded block, and every later block
/// (including zero-row trailers, which re-emit the column headers) must carry
/// the same column names and types or decoding fails with
/// [`DecodeError::BlockSchemaMismatch`]. Zero-row blocks contribute the
/// schema but are dropped from the chunk list to keep the chunk stream free
/// of empty batches.
pub fn decode_all_bytes(data: &[u8], options: &DecodeOptions) -> Result<ChunkedBatch, DecodeError> {
    let mut reader = ByteReader::new(data);
    let mut schema: Option<Schema> = None;
    let mut chunks: Vec<Arc<ColBatch>> = Vec::new();
    let mut block_index: usize = 0;

    while let Some(batch) = decode_next_block(&mut reader, options)? {
        match &schema {
            None => schema = Some(batch.schema.clone()),
            Some(first) => {
                if batch.schema != *first {
                    return Err(DecodeError::BlockSchemaMismatch { block_index });
                }
            }
        }
        if batch.num_rows > 0 {
            chunks.push(Arc::new(batch));
        }
        block_index += 1;
    }

    let schema = schema.ok_or_else(|| {
        DecodeError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "no blocks in response",
        ))
    })?;

    Ok(ChunkedBatch { schema, chunks })
}

#[cfg(test)]
mod tests {
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
        fn low_cardinality_u32(
            self,
            dictionary: &[u32],
            indices: &[u64],
            index_width: usize,
        ) -> Self {
            let mut dict_bytes = Vec::new();
            for &v in dictionary {
                dict_bytes.extend_from_slice(&v.to_le_bytes());
            }
            self.low_cardinality_block(dictionary.len(), &dict_bytes, indices, index_width)
        }

        /// `LowCardinality(Date)` block: dictionary entries are raw 2-byte LE
        /// `UInt16` days, exactly a plain `Date`/`UInt16` column body.
        fn low_cardinality_u16(
            self,
            dictionary: &[u16],
            indices: &[u64],
            index_width: usize,
        ) -> Self {
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
        // DateTime is UInt32 seconds, DateTime64(3) is Int64 ticks (ms here).
        // Timezone and precision are type metadata only, never in the bytes.
        let data = BlockBuilder::new()
            .header(4, 4)
            .column_header("d", "Date")
            .date_data(&[0, 19737, 49710, 65535])
            .column_header("d32", "Date32")
            .int32_data(&[-7227, 0, 19737, 84370])
            .column_header("dt", "DateTime")
            .uint32_data(&[0, 1705322096, 961056000, 4294967295])
            .column_header("dt64", "DateTime64(3)")
            .int64_data(&[-877, 0, 1705322096789, 4102444799999])
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
    fn test_decode_temporal_zero_rows() {
        // A zero-row block carrying a Date and a DateTime column contributes the
        // schema but no chunks, and the empty columns have length 0.
        let data = BlockBuilder::new()
            .header(2, 0)
            .column_header("d", "Date")
            .column_header("dt", "DateTime")
            .build();

        let cb = decode_all_bytes(&data, &DecodeOptions::default()).unwrap();
        assert_eq!(cb.num_rows(), 0);
        assert_eq!(cb.num_chunks(), 0);
        assert_eq!(cb.num_columns(), 2);
        assert_eq!(cb.schema.fields[0].ch_type, ChType::Date);
        assert_eq!(
            cb.schema.fields[1].ch_type,
            ChType::DateTime { timezone: None }
        );
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
        0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99,
        0x88,
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
}
