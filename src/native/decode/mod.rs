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

pub use crate::native::protocol::{
    DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION, DBMS_TCP_PROTOCOL_VERSION,
};
use crate::native::protocol::{
    LC_HAS_ADDITIONAL_KEYS_BIT, LC_NEED_GLOBAL_DICTIONARY_BIT, LOW_CARDINALITY_KEY_VERSION,
};
use crate::native::type_parser::{is_low_cardinality_inner, is_valid_map_key_type};
pub use crate::native::type_parser::{low_cardinality_dict_value_type, parse_ch_type};

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
    // A name-decoration alias (SimpleAggregateFunction, geo, Nested) has the
    // exact state prefix of the type it delegates to, so expand and recurse. For
    // Nested(a LowCardinality(String)) this reaches the leaf LowCardinality's
    // 8-byte key version through the delegated Array(Tuple(...)) chain, hoisting
    // it to the very front of the whole column, before the offsets.
    if let Some(under) = ch_type.physical_delegate() {
        return read_state_prefix(reader, &under, column);
    }
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
    // Resolve the inner through the shared helper: it strips the full
    // `SimpleAggregateFunction` chain (the only alias legal inside
    // LowCardinality, confirmed live at v26.6.1.1193-stable), unwraps the
    // removeNullable `Nullable`, and strips any further SAF chain beneath it, so
    // the dictionary body and null handling are those of the physical type for
    // both `LowCardinality(SAF(anyLast, Nullable(String)))` and a chained SAF.
    let (nullable, dict_value_type) = low_cardinality_dict_value_type(inner);

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
    // A name-decoration alias (SimpleAggregateFunction, geo, Nested) decodes
    // exactly as the physical type it delegates to, producing the underlying
    // Column variant (no new variant). Expand and recurse before the container
    // dispatch below so a geo/Nested alias that expands to an `Array` reaches the
    // Array fast-path, and a SimpleAggregateFunction over any inner delegates to
    // that inner.
    if let Some(under) = ch_type.physical_delegate() {
        return decode_values(reader, &under, num_rows, column);
    }
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

    // A geo alias legal directly inside `Nullable` is only `Nullable(Point)`
    // (the array-based kinds and `Nested` are rejected by the parser); expand it
    // to its `Tuple` so the Tuple arm below handles the body after the null map,
    // the ordinary `Nullable(Tuple(...))` framing.
    let delegate = inner.physical_delegate();
    let inner = delegate.as_ref().unwrap_or(inner);

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
///
/// At v26.6.1.1193-stable, `Time` is a contiguous raw signed Int32 seconds run:
/// `DataTypeTime::doGetSerialization` in `src/DataTypes/DataTypeTime.h` and
/// `src/DataTypes/DataTypeTime.cpp` selects `SerializationTime::create` in
/// `src/DataTypes/Serializations/SerializationDateTime.h` and
/// `src/DataTypes/Serializations/SerializationDateTime.cpp`, which uses the
/// `SerializationNumber<Int32>` bulk methods in
/// `src/DataTypes/Serializations/SerializationNumber.cpp`. `Time64(P)` is a
/// contiguous raw signed Int64 tick run with no scale conversion:
/// `DataTypeTime64` in `src/DataTypes/DataTypeTime64.h` and
/// `src/DataTypes/DataTypeTime64.cpp` selects `SerializationTime64` in
/// `src/DataTypes/Serializations/SerializationTime64.h` and
/// `src/DataTypes/Serializations/SerializationTime64.cpp`, whose bulk path is
/// `SerializationDecimalBase<Time64>` in
/// `src/DataTypes/Serializations/SerializationDecimalBase.cpp`. Both are
/// little-endian on the wire and use the primitive fast path below.
///
/// At the same tag, every `Interval*` type selects `SerializationInterval`
/// through `DataTypeInterval::doGetSerialization`; its bulk path is the same
/// contiguous signed `Int64` run as `SerializationNumber<Int64>`, with no unit
/// metadata in the body. See `src/DataTypes/DataTypeInterval.{h,cpp}`,
/// `src/DataTypes/Serializations/SerializationInterval.h`, and
/// `src/DataTypes/Serializations/SerializationNumber.cpp`.
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
        // Time is signed Int32 seconds, with no date or timezone metadata.
        ChType::Time => {
            let values = decode_primitive!(reader, num_rows, i32);
            Column::Time(PrimitiveColumn { values, validity })
        }
        // Time64(P) is signed Int64 ticks, with precision only in the ChType.
        ChType::Time64 { .. } => {
            let values = decode_primitive!(reader, num_rows, i64);
            Column::Time64(PrimitiveColumn { values, validity })
        }
        // All 11 Interval* kinds are signed Int64 counts. The kind is schema
        // metadata only, so every one shares this primitive hot path and one
        // distinct Column tag.
        ChType::Interval(_) => {
            let values = decode_primitive!(reader, num_rows, i64);
            Column::Interval(PrimitiveColumn { values, validity })
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
        // cannot occur for any type this decoder produces. The name-decoration
        // aliases (`SimpleAggregateFunction`, geo, `Nested`) are expanded to their
        // physical delegate by `decode_values` before reaching here too. None of
        // these is a legal inner of a `LowCardinality` dictionary either, the other
        // caller. Return an error rather than panic so a future regression degrades
        // to a clean decode error instead of undefined behavior at an FFI boundary.
        ChType::Nullable(_)
        | ChType::LowCardinality(_)
        | ChType::Array(_)
        | ChType::Tuple(_)
        | ChType::Map(..)
        | ChType::SimpleAggregateFunction { .. }
        | ChType::Geo(_)
        | ChType::Nested(_) => {
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
    // A name-decoration alias (SimpleAggregateFunction, geo, Nested) builds the
    // empty column its physical delegate would, so recurse on the delegate at the
    // very top, matching every other per-type dispatcher (`decode_values`,
    // `skip_values`, `validate_header_type`). Doing this before the `Nullable`
    // unwrap is what keeps a zero-row header like
    // `SimpleAggregateFunction(anyLast, Nullable(String))`, `SAF` over a geo/
    // `Nested` inner, or `SAF(_, Nullable(Point))` from reaching the `unreachable!`
    // arm below and panicking on untrusted wire input.
    if let Some(under) = ch_type.physical_delegate() {
        return empty_column(&under);
    }
    let (nullable, inner) = match ch_type {
        ChType::Nullable(inner) => (true, inner.as_ref()),
        other => (false, other),
    };
    let empty_validity = if nullable {
        Some(Bitmap::from_ch_null_map(&[]))
    } else {
        None
    };

    // Fully resolve any name-decoration alias remaining after the `Nullable`
    // unwrap, following the delegate chain to a physical type. A top-level alias
    // was already handled by the recursion at the top of this function, so this
    // covers an alias legal directly inside `Nullable`: `Nullable(Point)`
    // (`Geo(Point)` -> `Tuple`) and `Nullable(SimpleAggregateFunction(_, T))`
    // (SAF -> its inner), including a chain like `Nullable(SAF(_, Point))`
    // (SAF -> `Geo(Point)` -> `Tuple`). `empty_validity` is Some in these cases,
    // so the Tuple/primitive arm builds the matching nullable empty column, and
    // the `unreachable!` arm can never see an alias.
    let mut resolved = inner.physical_delegate();
    while let Some(under) = resolved.as_ref().and_then(|t| t.physical_delegate()) {
        resolved = Some(under);
    }
    let inner = resolved.as_ref().unwrap_or(inner);

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
        ChType::Time => Column::Time(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::Time64 { .. } => Column::Time64(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::Interval(_) => Column::Interval(PrimitiveColumn {
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
            // Resolve the inner through the shared helper so the empty dictionary
            // matches what the non-empty decode would build: a non-nullable
            // dictionary values column of the physical value type, plus an empty
            // index validity bitmap when the inner is nullable. This mirrors
            // `decode_low_cardinality`, so a zero-row
            // `LowCardinality(SAF(anyLast, Nullable(String)))` column decodes the
            // same shape as a populated one.
            let (nullable_inner, dict_value_type) = low_cardinality_dict_value_type(lc_inner);
            let empty_values = empty_column(dict_value_type);
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
        // The outer `Nullable` was unwrapped above, `parse_ch_type` never
        // produces a `Nullable` directly inside a `Nullable`, and any
        // name-decoration alias was expanded to its physical delegate above, so
        // `inner` is never a `Nullable` or an alias here. Unlike the decode and
        // scan paths this constructor is infallible (it returns a `Column`, not a
        // `Result`), so the invariant is asserted rather than surfaced as an error.
        ChType::Nullable(_)
        | ChType::SimpleAggregateFunction { .. }
        | ChType::Geo(_)
        | ChType::Nested(_) => {
            unreachable!("Nullable inner unwrapped and aliases expanded; parse_ch_type rejects nested Nullable")
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
/// [`parse_ch_type`] succeeds, and that parser caps nesting at
/// [`MAX_TYPE_DEPTH`](crate::native::protocol::MAX_TYPE_DEPTH).
fn validate_header_type(col_name: &str, ch_type: &ChType) -> Result<(), DecodeError> {
    // A name-decoration alias (SimpleAggregateFunction, geo, Nested) is legal
    // exactly when the physical type it delegates to is, so validate the
    // delegate. This catches a forbidden `LowCardinality` inner nested inside a
    // `Nested` element (e.g. `Nested(a LowCardinality(Decimal(9, 4)))`) on every
    // path, at header time, regardless of row count.
    if let Some(under) = ch_type.physical_delegate() {
        return validate_header_type(col_name, &under);
    }
    match ch_type {
        ChType::LowCardinality(inner) => {
            // Resolve through the shared helper (full SAF chain + optional
            // Nullable + inner SAF chain) so an aliased inner like
            // `SimpleAggregateFunction(anyLast, Nullable(String))` is validated on
            // its physical dictionary value type, not rejected because the raw
            // inner is not itself an allowed LC inner.
            let (_, dict_value_type) = low_cardinality_dict_value_type(inner);
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
    // Expand a name-decoration alias to its physical delegate, the scan-side
    // mirror of `decode_values`, so a geo/Nested alias reaches the Array
    // fast-path and a SimpleAggregateFunction walks its inner.
    if let Some(under) = ch_type.physical_delegate() {
        return skip_values(reader, &under, num_rows, column);
    }
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

    // Expand a geo alias legal directly inside `Nullable` (only `Nullable(Point)`
    // -> `Tuple`), the scan-side mirror of `decode_values`.
    let delegate = inner.physical_delegate();
    let inner = delegate.as_ref().unwrap_or(inner);

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
        | ChType::Time
        | ChType::Ipv4 => reader.skip(num_rows.saturating_mul(4))?,
        ChType::Int64
        | ChType::UInt64
        | ChType::Float64
        | ChType::DateTime64 { .. }
        | ChType::Time64 { .. }
        | ChType::Interval(_) => reader.skip(num_rows.saturating_mul(8))?,
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
        // are dispatched by `skip_values` above. The name-decoration aliases
        // (`SimpleAggregateFunction`, geo, `Nested`) are expanded to their
        // physical delegate by `skip_values` before reaching here too. Defense in
        // depth: `parse_ch_type` also rejects a wrapper nested where the callers'
        // single-level unwrap cannot reach it, so these arms cannot occur. Return
        // an error rather than panic to keep the streaming scan panic-free even if
        // that guarantee ever regresses (a panic here is undefined behavior across
        // FFI).
        ChType::Nullable(_)
        | ChType::LowCardinality(_)
        | ChType::Array(_)
        | ChType::Tuple(_)
        | ChType::Map(..)
        | ChType::SimpleAggregateFunction { .. }
        | ChType::Geo(_)
        | ChType::Nested(_) => {
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
    // Resolve the inner through the shared helper, the scan-side mirror of
    // [`decode_low_cardinality`], so the dictionary body walk matches the decode
    // for `LowCardinality(SAF(anyLast, Nullable(String)))` and chained SAF alike.
    // The scan does not need the nullability flag: nulls are index-0 sentinels in
    // the same raw index array it skips regardless.
    let (_, dict_value_type) = low_cardinality_dict_value_type(inner);

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
mod tests;
