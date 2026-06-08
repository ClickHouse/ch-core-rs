use std::io;
use std::sync::Arc;

use crate::batch::{ChunkedBatch, ColBatch};
use crate::bitmap::Bitmap;
use crate::column::{BoolColumn, Column, FixedBinaryColumn, PrimitiveColumn, Utf8Column};
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
        }
    }
}

impl std::error::Error for DecodeError {}

/// Server protocol revision this crate has been validated against
/// (ClickHouse v26.2.4.23-stable). Pass this as `DecodeOptions::protocol_revision`
/// when decoding a Native stream produced by a current server over the native
/// TCP protocol.
pub const DBMS_TCP_PROTOCOL_VERSION: u64 = 54483;

/// Protocol revision at which every column header carries a one-byte
/// custom-serialization marker before its data (server constant
/// `DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION`).
pub const DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION: u64 = 54454;

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
fn parse_ch_type(type_name: &str) -> Option<ChType> {
    // Nullable wrapper
    if let Some(inner) = type_name.strip_prefix("Nullable(") {
        if let Some(inner) = inner.strip_suffix(')') {
            return parse_ch_type(inner).map(|t| ChType::Nullable(Box::new(t)));
        }
    }

    // FixedString(N)
    if let Some(n_str) = type_name.strip_prefix("FixedString(") {
        if let Some(n_str) = n_str.strip_suffix(')') {
            if let Ok(n) = n_str.trim().parse::<usize>() {
                return Some(ChType::FixedString(n));
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
/// `SerializationString::deserializeBinaryBulk`, confirmed at v26.2.4.23-stable).
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
        let len = reader.read_varint()? as usize;
        let bytes = reader.read_slice(len)?;
        data.extend_from_slice(bytes);
        offset += len as i32;
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

/// Decode a single column given its ChType.
fn decode_column(
    reader: &mut ByteReader,
    ch_type: &ChType,
    num_rows: usize,
) -> Result<Column, DecodeError> {
    let (nullable, inner) = match ch_type {
        ChType::Nullable(inner) => (true, inner.as_ref()),
        other => (false, other),
    };

    let validity = if nullable {
        Some(decode_null_map(reader, num_rows)?)
    } else {
        None
    };

    let column = match inner {
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
        ChType::Nullable(_) => unreachable!("Nullable already unwrapped"),
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
        ChType::Nullable(_) => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Block info preamble
// ---------------------------------------------------------------------------

/// Consume the `BlockInfo` preamble that precedes each block when the producer
/// used a protocol revision > 0 (server `BlockInfo::read` in
/// `src/Core/BlockInfo.cpp`, confirmed at v26.2.4.23-stable).
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
                let count = reader.read_varint()? as usize; // out_of_order_buckets
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
        let num_cols = reader.read_varint()? as usize;
        let num_rows = reader.read_varint()? as usize;
        return Ok(Some(decode_block_body(
            reader, options, num_cols, num_rows,
        )?));
    }

    // No protocol framing. End of stream falls on the column-count varint.
    let num_cols = match reader.read_varint() {
        Ok(n) => n as usize,
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let num_rows = reader.read_varint()? as usize;
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

    Ok((col_name, ch_type))
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
            columns.push(decode_column(reader, &ch_type, num_rows)?);
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

    let num_cols = reader.read_varint()? as usize;
    let num_rows = reader.read_varint()? as usize;

    for _ in 0..num_cols {
        let (_name, ch_type) = read_column_header(&mut reader, options)?;
        if num_rows > 0 {
            skip_column_data(&mut reader, &ch_type, num_rows)?;
        }
    }

    Ok(Some(reader.position()))
}

/// Advance `reader` past one column's data without materializing it.
///
/// Fixed-width types have a computable byte length; String scans the per-value
/// varint length prefixes. This must consume exactly the bytes the matching
/// decoder in `decode_column` consumes.
fn skip_column_data(
    reader: &mut ByteReader,
    ch_type: &ChType,
    num_rows: usize,
) -> Result<(), DecodeError> {
    let inner = match ch_type {
        ChType::Nullable(inner) => {
            reader.skip(num_rows)?; // null map: 1 byte per row
            inner.as_ref()
        }
        other => other,
    };

    match inner {
        ChType::Bool | ChType::Int8 | ChType::UInt8 => reader.skip(num_rows)?,
        ChType::Int16 | ChType::UInt16 | ChType::Date => reader.skip(num_rows.saturating_mul(2))?,
        ChType::Int32
        | ChType::UInt32
        | ChType::Float32
        | ChType::Date32
        | ChType::DateTime { .. } => reader.skip(num_rows.saturating_mul(4))?,
        ChType::Int64 | ChType::UInt64 | ChType::Float64 | ChType::DateTime64 { .. } => {
            reader.skip(num_rows.saturating_mul(8))?
        }
        ChType::FixedString(width) => reader.skip(num_rows.saturating_mul(*width))?,
        ChType::String => {
            for _ in 0..num_rows {
                let len = reader.read_varint()? as usize;
                reader.skip(len)?;
            }
        }
        // `read_column_header` already rejected unsupported types, and Nullable
        // was unwrapped above.
        ChType::Nullable(_) => unreachable!("Nullable already unwrapped"),
    }

    Ok(())
}

/// Decode all blocks from a complete byte buffer into a `ChunkedBatch`.
///
/// Each Native block becomes its own chunk — blocks are NOT concatenated.
/// The schema is taken from the first decoded block (every block of a query
/// shares the same schema). Zero-row blocks contribute the schema but are
/// dropped from the chunk list to keep the chunk stream free of empty batches.
pub fn decode_all_bytes(data: &[u8], options: &DecodeOptions) -> Result<ChunkedBatch, DecodeError> {
    let mut reader = ByteReader::new(data);
    let mut schema: Option<Schema> = None;
    let mut chunks: Vec<Arc<ColBatch>> = Vec::new();

    while let Some(batch) = decode_next_block(&mut reader, options)? {
        if schema.is_none() {
            schema = Some(batch.schema.clone());
        }
        if batch.num_rows > 0 {
            chunks.push(Arc::new(batch));
        }
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
            write_varint(&mut self.buf, num_cols as u64).unwrap();
            write_varint(&mut self.buf, num_rows as u64).unwrap();
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
            write_varint(buf, s.len() as u64).unwrap();
            buf.extend_from_slice(s.as_bytes());
        }

        /// Standard BlockInfo: is_overflows=false, bucket_num=-1, and an empty
        /// out_of_order_buckets vector at revision >= 54480.
        fn push_block_info(buf: &mut Vec<u8>, revision: u64) {
            write_varint(buf, 1).unwrap();
            buf.push(0x00); // is_overflows = false
            write_varint(buf, 2).unwrap();
            buf.extend_from_slice(&(-1i32).to_le_bytes()); // bucket_num = -1
            if revision >= 54480 {
                write_varint(buf, 3).unwrap();
                write_varint(buf, 0).unwrap(); // empty out_of_order_buckets
            }
            write_varint(buf, 0).unwrap(); // terminator
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
                write_varint(&mut self.buf, s.len() as u64).unwrap();
                self.buf.extend_from_slice(s.as_bytes());
            }
            self
        }

        fn null_map(mut self, nulls: &[bool]) -> Self {
            for &is_null in nulls {
                self.buf.push(if is_null { 0x01 } else { 0x00 });
            }
            self
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
        // incomplete.
        let data = BlockBuilder::new()
            .header(1, 1)
            .column_header("id", "UUID")
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
        let data = BlockBuilder::new()
            .header(1, 1)
            .column_header("id", "UUID")
            .build();
        assert!(matches!(
            decode_all_bytes(&data, &DecodeOptions::default()),
            Err(DecodeError::UnsupportedType { .. })
        ));
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
        // Full v26.2.4.23 framing: a BlockInfo preamble plus a per-column
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
        write_varint(&mut data, 7).unwrap(); // unknown field number

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
}
