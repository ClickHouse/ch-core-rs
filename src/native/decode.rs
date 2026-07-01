use std::io;
use std::sync::Arc;

use crate::batch::{ChunkedBatch, ColBatch};
use crate::bitmap::Bitmap;
use crate::column::{
    BoolColumn, Column, DecimalColumn, DictionaryColumn, FixedBinaryColumn, PrimitiveColumn,
    Utf8Column,
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

/// Parse a ClickHouse type name string into a ChType.
///
/// `pub(crate)` so the encoder can confirm a rendered type string round-trips
/// (a header this parser rejects is one the server rejects too).
pub(crate) fn parse_ch_type(type_name: &str) -> Option<ChType> {
    // Nullable wrapper. ClickHouse forbids a `Nullable` or a `LowCardinality`
    // directly inside a `Nullable`: the only legal nesting with LowCardinality is
    // `LowCardinality(Nullable(T))`, never the reverse, and `Nullable(Nullable(T))`
    // does not exist at all. An honest server never emits either, but the type
    // string is untrusted wire input, so reject both here. `decode_column` and
    // `skip_column_data` unwrap exactly one `Nullable` and handle `LowCardinality`
    // only at the top level; accepting a nested wrapper would let those inner
    // wrappers reach an `unreachable!` on malformed bytes (AGENTS.md invariant 2:
    // no panics on malformed input).
    if let Some(inner) = type_name.strip_prefix("Nullable(") {
        if let Some(inner) = inner.strip_suffix(')') {
            let inner_type = parse_ch_type(inner)?;
            if matches!(inner_type, ChType::Nullable(_) | ChType::LowCardinality(_)) {
                return None;
            }
            return Some(ChType::Nullable(Box::new(inner_type)));
        }
    }

    // LowCardinality wrapper. The inner type is parsed recursively, so
    // LowCardinality(Nullable(String)) yields LowCardinality(Nullable(String)).
    if let Some(inner) = type_name.strip_prefix("LowCardinality(") {
        if let Some(inner) = inner.strip_suffix(')') {
            return parse_ch_type(inner).map(|t| ChType::LowCardinality(Box::new(t)));
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
fn decimal_bits_from_precision(precision: u8) -> Option<u16> {
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
            "FixedString column byte length overflows usize",
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
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// LowCardinality
// ---------------------------------------------------------------------------

/// Key serialization version this decoder accepts. The Native format always
/// uses `SharedDictionariesWithAdditionalKeys` (server
/// `KeysSerializationVersion`).
const LOW_CARDINALITY_KEY_VERSION: u64 = 1;

/// `NeedGlobalDictionaryBit` of the per-block index type word. Native never sets
/// it (the server rejects it for `native_format`), so the decoder rejects it too.
const LC_NEED_GLOBAL_DICTIONARY_BIT: u64 = 1 << 8;
/// `HasAdditionalKeysBit` of the per-block index type word. Always set in Native:
/// each block carries its own dictionary as "additional keys".
const LC_HAS_ADDITIONAL_KEYS_BIT: u64 = 1 << 9;

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
fn is_low_cardinality_inner(dict_value_type: &ChType) -> bool {
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
    // LowCardinality, which reads its key version here.
    read_state_prefix(reader, ch_type, column)?;

    // LowCardinality carries its own dictionary, indexes, and (for a Nullable
    // inner type) null handling, so it is decoded as a unit rather than going
    // through the Nullable null-map unwrap below.
    if let ChType::LowCardinality(inner) = ch_type {
        return decode_low_cardinality(reader, inner, num_rows, column);
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

    decode_column_body(reader, inner, num_rows, validity)
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
        // is documented in DECODER_CONTRACT.md for bindings only.
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
        // Defense in depth: `parse_ch_type` rejects a `Nullable`/`LowCardinality`
        // nested where the single-level unwrap in `decode_column` cannot handle it,
        // and both wrappers are stripped by the callers before reaching here, so
        // these arms cannot occur for any type this decoder produces. Return an
        // error rather than panic so a future regression degrades to a clean decode
        // error instead of undefined behavior at an FFI boundary.
        ChType::Nullable(_) | ChType::LowCardinality(_) => {
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
/// Currently this is a `LowCardinality` whose (removeNullable) inner type is not in
/// [`is_low_cardinality_inner`]. Checking here, in the header path shared by the
/// allocating decode, the completeness scan, and the zero-row `empty_column` path,
/// makes all three agree on which columns are accepted. Without it a zero-row
/// `LowCardinality(Decimal(9, 4))` block would decode (its `empty_column` never
/// consults the allowlist) while the same type with rows errors, an inconsistency
/// the streaming decoder could hit as a block fills.
fn validate_header_type(col_name: &str, ch_type: &ChType) -> Result<(), DecodeError> {
    if let ChType::LowCardinality(inner) = ch_type {
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
    }
    Ok(())
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
    // bytes for every type except LowCardinality.
    read_state_prefix(reader, ch_type, column)?;

    if let ChType::LowCardinality(inner) = ch_type {
        return skip_low_cardinality_data(reader, inner, num_rows, column);
    }

    let inner = match ch_type {
        ChType::Nullable(inner) => {
            reader.skip(num_rows)?; // null map: 1 byte per row
            inner.as_ref()
        }
        other => other,
    };

    skip_column_body(reader, inner, num_rows)
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
        ChType::String => {
            for _ in 0..num_rows {
                let len = varint_usize(reader.read_varint()?, "String value length")?;
                reader.skip(len)?;
            }
        }
        // `read_column_header` already rejected unsupported types, Nullable is
        // unwrapped by the callers, and LowCardinality is handled above. Defense in
        // depth: `parse_ch_type` also rejects a wrapper nested where the callers'
        // single-level unwrap cannot reach it, so these arms cannot occur. Return an
        // error rather than panic to keep the streaming scan panic-free even if that
        // guarantee ever regresses (a panic here is undefined behavior across FFI).
        ChType::Nullable(_) | ChType::LowCardinality(_) => {
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

        fn null_map(mut self, nulls: &[bool]) -> Self {
            for &is_null in nulls {
                self.buf.push(if is_null { 0x01 } else { 0x00 });
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
        // incomplete. Int128 is not decoded yet, so it serves as the example.
        let data = BlockBuilder::new()
            .header(1, 1)
            .column_header("id", "Int128")
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
        // Int128 is not decoded yet, so it serves as the unsupported example now
        // that Decimal and UUID/IPv4/IPv6 are decoded.
        let data = BlockBuilder::new()
            .header(1, 1)
            .column_header("id", "Int128")
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
}
