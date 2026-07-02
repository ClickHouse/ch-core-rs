//! Encode columnar data into ClickHouse `FORMAT Native` block bytes.
//!
//! This is the inverse of [`super::decode`]: it turns a [`ColBatch`] (or each
//! block of a [`ChunkedBatch`]) back into the Native wire bytes the server
//! accepts for `INSERT`. The framing mirrors [`super::decode::decode_next_block`]
//! exactly, so bytes produced here decode back through this crate unchanged and
//! match what the server's `NativeWriter::write` emits at the same protocol
//! revision (confirmed against `src/Formats/NativeWriter.cpp`,
//! `src/Core/BlockInfo.cpp`, and `src/Processors/Formats/Impl/NativeFormat.cpp`
//! at v26.6.1.1193-stable).
//!
//! Scope: this encodes `Bool`, the fixed-width numeric types (`Int8`..`Int64`,
//! `UInt8`..`UInt64`, `Float32`, `Float64`), the temporal types (`Date`,
//! `Date32`, `DateTime`, `DateTime64`), `UUID`, `IPv4`, `IPv6`, `String`,
//! `FixedString(N)`, `Enum8`/`Enum16`, and `Decimal(P, S)`, each also inside a
//! `Nullable(T)` wrapper (a per-row null map precedes the inner values).
//! Every other column type returns [`EncodeError::UnsupportedType`] until its
//! encoder lands, the same one-type-at-a-time growth the decode path follows.

use crate::batch::{ChunkedBatch, ColBatch};
use crate::column::{BoolColumn, Column, DecimalColumn, FixedBinaryColumn, Utf8Column};
use crate::schema::{ChType, Field};

use super::decode::{
    decimal_bits_from_precision, parse_ch_type, DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION,
    DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS,
};
use super::varint::write_varint;

/// Options for Native format encoding, the mirror of
/// [`super::decode::DecodeOptions`].
#[derive(Default)]
pub struct EncodeOptions {
    /// Negotiated protocol revision the produced Native stream targets. It gates
    /// the same framing the decoder's `protocol_revision` gates and must match
    /// the revision the consumer reads with:
    ///
    /// - A `BlockInfo` preamble precedes every block when this is > 0.
    /// - A per-column custom-serialization marker byte (0 = default) is written
    ///   when this is >= [`DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION`].
    ///
    /// Use 0 for HTTP `INSERT ... FORMAT Native`: the server parses the request
    /// body with `server_revision = 0` (its `NativeInputFormat` constructs the
    /// `NativeReader` with revision 0), so it expects neither the preamble nor the
    /// marker byte, and the stream simply ends at EOF. Use the negotiated TCP
    /// revision for the native protocol path.
    pub protocol_revision: u64,
}

/// Error returned when a batch cannot be encoded to Native bytes.
///
/// Unlike [`super::decode::DecodeError`], the input here is trusted in-memory
/// buffers this crate produced, not untrusted wire bytes, so the failure modes
/// are structural: a batch whose columns disagree with its schema or row count,
/// or a column type the encoder does not yet support.
#[derive(Debug, Clone, PartialEq)]
pub enum EncodeError {
    /// A column this encoder cannot yet write: an unsupported physical type, or a
    /// `Nullable(T)` whose inner type is not yet encodable. Grows narrower as
    /// encode coverage catches up to decode coverage.
    UnsupportedType { column: String, ch_type: ChType },
    /// The batch is internally inconsistent: the column count does not match the
    /// schema field count, or a column's length does not match `num_rows`.
    InconsistentBatch { detail: String },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::UnsupportedType { column, ch_type } => {
                write!(f, "cannot encode column {column:?} of type {ch_type}")
            }
            EncodeError::InconsistentBatch { detail } => {
                write!(f, "inconsistent batch: {detail}")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// Encode a single batch as one Native block, framed for `options.protocol_revision`.
///
/// The result is a complete, standalone Native block: the optional `BlockInfo`
/// preamble, the column and row counts, then each column's header and data. It is
/// exactly what [`super::decode::decode_next_block`] reads at the same revision.
///
/// A not-yet-encodable column type is rejected at every row count, including zero.
/// This is deliberately asymmetric with the decoder, whose `empty_column` builds an
/// empty column for any decodable type in a zero-row block: encode coverage is a
/// subset of decode coverage, and the encoder fails fast and consistently rather
/// than emitting a header for a type it cannot write rows of.
pub fn encode_block(batch: &ColBatch, options: &EncodeOptions) -> Result<Vec<u8>, EncodeError> {
    let mut buf = Vec::new();
    encode_block_into(&mut buf, batch, options)?;
    Ok(buf)
}

/// Encode every block of a [`ChunkedBatch`] back to Native bytes, one Native
/// block per chunk, in order. This is the inverse of
/// [`super::decode::decode_all_bytes`]: feeding the result back through it at the
/// same revision yields the same chunks, modulo any zero-row block (the decoder
/// drops those from `chunks` but keeps the schema).
///
/// For HTTP `INSERT ... FORMAT Native` the concatenated blocks are the whole
/// request body; the server stops at EOF, so no terminating empty block is
/// written. (The native TCP protocol needs an explicit empty-block terminator;
/// that path is out of the current HTTP scope.)
pub fn encode_chunked(
    batch: &ChunkedBatch,
    options: &EncodeOptions,
) -> Result<Vec<u8>, EncodeError> {
    // Validate every chunk before writing anything: each chunk must carry the
    // batch's schema (an inconsistent chunk would encode a stream the server
    // rejects mid-insert) and pass its own per-column checks. Doing this up front
    // means a rejected `ChunkedBatch` leaves no partial stream behind.
    for (i, chunk) in batch.chunks.iter().enumerate() {
        if chunk.schema != batch.schema {
            return Err(EncodeError::InconsistentBatch {
                detail: format!("chunk {i} schema differs from the batch schema"),
            });
        }
        validate_block(chunk)?;
    }
    let mut buf = Vec::new();
    for chunk in &batch.chunks {
        write_block_into(&mut buf, chunk, options)?;
    }
    Ok(buf)
}

/// Append one framed Native block for `batch` to `buf`.
///
/// [`validate_block`] runs fully before any bytes are written, so a rejected batch
/// leaves `buf` untouched and [`write_block_into`] cannot fail on a structural
/// problem it already checked.
fn encode_block_into(
    buf: &mut Vec<u8>,
    batch: &ColBatch,
    options: &EncodeOptions,
) -> Result<(), EncodeError> {
    validate_block(batch)?;
    write_block_into(buf, batch, options)
}

/// Validate that `batch` can be encoded, without writing anything. Every rejection
/// condition lives here, so a caller can validate a whole [`ChunkedBatch`] up front
/// (see [`encode_chunked`]) and then write every block knowing none will fail
/// partway and leave a partial stream.
fn validate_block(batch: &ColBatch) -> Result<(), EncodeError> {
    let num_cols = batch.schema.num_fields();
    if num_cols != batch.columns.len() {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "schema has {num_cols} fields but batch carries {} columns",
                batch.columns.len()
            ),
        });
    }
    for (field, column) in batch.schema.fields.iter().zip(&batch.columns) {
        validate_column(field, column, batch.num_rows)?;
    }
    Ok(())
}

/// Validate one column against its field and the block row count.
///
/// All per-column rejection logic is here so [`write_block_into`] is structurally
/// infallible once validation passes. Runs at every row count, including zero: a
/// not-yet-encodable type is rejected even in a zero-row block (see the note on
/// [`encode_block`]).
fn validate_column(field: &Field, column: &Column, num_rows: usize) -> Result<(), EncodeError> {
    // Row count: the column must carry exactly the rows the block declares.
    if column.len() != num_rows {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} has {} rows but the block declares {num_rows}",
                field.name,
                column.len()
            ),
        });
    }

    // The concrete value type is the inner of a `Nullable`, else the type itself.
    let value_type = field.ch_type.inner();

    // Supported, matching (type, buffer) pair. A not-yet-encodable type, or a
    // supported type under a mismatched buffer variant, is rejected here rather
    // than during the write, so a rejected batch never leaves partial bytes. This
    // runs before the round-trip check below so a genuinely unsupported type is
    // reported as `UnsupportedType`, not misclassified as a bad-type-string
    // `InconsistentBatch` when its rendered string also fails to round-trip (e.g.
    // `Decimal { precision: 100, .. }`).
    if !column_variant_matches(value_type, column) {
        return Err(column_error(field, value_type));
    }

    // The rendered type string must round-trip through the decoder's parser. Some
    // `ChType`s are constructible that render a header this crate's own parser and
    // the server reject (`FixedString(0)`, a `DateTime64` precision above 9, a
    // timezone whose bytes break the type grammar); catching it here fails at the
    // source rather than letting `decode(encode(x))` fail downstream. Reached only
    // for an encodable, buffer-matched type, so any failure is a bad parameter on a
    // supported type, which `InconsistentBatch` describes correctly. This validates
    // the type inside a `Nullable` wrapper too, since `Display`/`parse` are total on
    // the wrapper.
    let rendered = field.ch_type.to_string();
    if parse_ch_type(&rendered).as_ref() != Some(&field.ch_type) {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} type {rendered} does not round-trip through the type parser; the server would reject this header",
                field.name
            ),
        });
    }

    // The round-trip check above does NOT catch a `DateTime`/`DateTime64` timezone
    // that contains a single quote. `ChType::Display` renders the timezone
    // unescaped (`DateTime('{tz}')`), and `strip_quotes` on decode only strips the
    // outer pair, so a timezone like `UTC')` renders `DateTime('UTC')')` and the
    // crate's own lenient parser recovers the same timezone, passing the round-trip.
    // But that header closes its quote early and is syntactically broken for the
    // server. A valid IANA timezone name never contains a quote, so reject one here.
    if let ChType::DateTime { timezone: Some(tz) }
    | ChType::DateTime64 {
        timezone: Some(tz), ..
    } = value_type
    {
        if tz.contains('\'') {
            return Err(EncodeError::InconsistentBatch {
                detail: format!(
                    "column {:?} timezone {tz:?} contains a quote, which renders a malformed type header the server rejects",
                    field.name
                ),
            });
        }
    }

    // Any present validity bitmap must have a backing buffer long enough for its
    // own bit length. `Bitmap::from_raw` is `pub` and only debug-asserts this, so a
    // release-mode caller could hand over a short buffer that would then panic when
    // `null_count()` counts it below or `encode_null_map` unpacks it. Reject it, the
    // same way the `Bool` bitmap is guarded further down.
    if let Some(validity) = column.validity() {
        let needed = validity.len().div_ceil(8);
        if validity.as_bytes().len() < needed {
            return Err(EncodeError::InconsistentBatch {
                detail: format!(
                    "column {:?} validity bitmap covers {} rows but its buffer holds only {} bytes ({needed} needed)",
                    field.name,
                    validity.len(),
                    validity.as_bytes().len()
                ),
            });
        }
    }

    // Nullability. A `Nullable` field writes a per-row null map from the validity
    // bitmap, so the bitmap (when present) must cover exactly `num_rows`; this also
    // keeps `encode_null_map`'s reads in range. A non-`Nullable` field writes no
    // null map, so a null in its validity bitmap would be silently dropped and the
    // row's placeholder value encoded as real data; reject that.
    if matches!(field.ch_type, ChType::Nullable(_)) {
        if let Some(validity) = column.validity() {
            if validity.len() != num_rows {
                return Err(EncodeError::InconsistentBatch {
                    detail: format!(
                        "column {:?} declares {num_rows} rows but its validity bitmap covers {}",
                        field.name,
                        validity.len()
                    ),
                });
            }
        }
    } else if column.null_count() > 0 {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is not Nullable but its validity bitmap marks {} rows null; the null map is not written, so those rows would encode a placeholder value as real data",
                field.name,
                column.null_count()
            ),
        });
    }

    // A `Bool` column is unpacked from its packed bitmap positionally, so the
    // bitmap must hold at least `len.div_ceil(8)` bytes. `BoolColumn`'s fields are
    // public and `ColBatch::new` only debug-asserts, so a release-mode caller could
    // hand over a `len` that overruns the bitmap; reject it rather than let
    // `encode_bool_data` panic.
    if let Column::Bool(c) = column {
        let needed = c.len.div_ceil(8);
        if c.bitmap.len() < needed {
            return Err(EncodeError::InconsistentBatch {
                detail: format!(
                    "column {:?} declares {} bool rows but its bitmap holds only {} bytes ({needed} needed)",
                    field.name,
                    c.len,
                    c.bitmap.len()
                ),
            });
        }
    }

    // The fixed-width binary bodies are written verbatim with no per-row framing,
    // so guard each against a misframed buffer: `FixedString(N)` against its
    // declared N, `UUID` and `IPv6` against their implied width 16.
    match (value_type, column) {
        (ChType::FixedString(width), Column::FixedBinary(c)) => {
            validate_fixed_binary(field, value_type, c, *width, num_rows)?;
        }
        (ChType::Uuid, Column::Uuid(c)) | (ChType::Ipv6, Column::Ipv6(c)) => {
            validate_fixed_binary(field, value_type, c, 16, num_rows)?;
        }
        (
            ChType::Decimal {
                precision,
                scale,
                bits,
            },
            Column::Decimal(c),
        ) => validate_decimal(field, c, *precision, *scale, *bits, num_rows)?,
        _ => {}
    }

    // A `String` body is written by slicing `data[offsets[i]..offsets[i+1]]` per
    // row. Validate the Arrow offset invariants so a hand-built column cannot make
    // that slice panic or silently drop leading/trailing bytes.
    if let (ChType::String, Column::Utf8(c)) = (value_type, column) {
        validate_utf8_column(field, c, num_rows)?;
    }

    Ok(())
}

/// Shared misframe guard for fixed-width binary bodies (`FixedString(N)`,
/// `UUID`, and `IPv6`).
///
/// The body is the contiguous `width * num_rows` data buffer written verbatim
/// with no per-row framing, so the reader consumes exactly `width` bytes per
/// row. `FixedBinaryColumn::len()` is `data.len() / width` (truncating), so a
/// buffer whose length is not exactly `width * num_rows` still reports
/// `num_rows` rows and passes the row-count check in [`validate_column`], yet
/// would put a different number of bytes on the wire: a silently misframed
/// stream. The stored buffer width must also equal the width the type declares
/// (`FixedString(N)`) or implies (16 for `UUID`/`IPv6`), or a truthful type
/// string would sit over a wrong-bytes-per-row body. `value_type` is used only
/// to render the type name in the error.
fn validate_fixed_binary(
    field: &Field,
    value_type: &ChType,
    col: &FixedBinaryColumn,
    declared_width: usize,
    num_rows: usize,
) -> Result<(), EncodeError> {
    if col.width != declared_width {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is declared {value_type} ({declared_width} bytes per row) but its buffer stores {}-byte rows",
                field.name, col.width
            ),
        });
    }
    if declared_width.checked_mul(num_rows) != Some(col.data.len()) {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is {value_type} over {num_rows} rows so its body must be {} bytes, but the buffer holds {}",
                field.name,
                declared_width.saturating_mul(num_rows),
                col.data.len()
            ),
        });
    }
    Ok(())
}

/// Guard the raw Decimal body before writing.
///
/// `Decimal(P, S)` carries precision and scale in the type string only, while
/// `DecimalColumn` carries them for direct buffer consumers. They must agree, and
/// the byte buffer must be exactly one precision-derived fixed-width integer per
/// row. This derives width from precision directly instead of trusting the
/// `ChType::Decimal::bits` field, so a truthful type string cannot sit over a
/// wrong-shaped body.
fn validate_decimal(
    field: &Field,
    col: &DecimalColumn,
    precision: u8,
    scale: u8,
    bits: u16,
    num_rows: usize,
) -> Result<(), EncodeError> {
    if scale > precision {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is declared Decimal({precision}, {scale}) but Decimal scale must not exceed precision",
                field.name
            ),
        });
    }
    let expected_bits =
        decimal_bits_from_precision(precision).ok_or_else(|| EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is declared Decimal({precision}, {scale}) but Decimal precision must be in 1..=76",
                field.name
            ),
        })?;
    if bits != expected_bits {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is declared Decimal({precision}, {scale}) with {bits} bits, but precision {precision} requires {expected_bits} bits",
                field.name
            ),
        });
    }
    let declared_width = (expected_bits / 8) as usize;
    if col.precision != precision || col.scale != scale {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is declared Decimal({precision}, {scale}) but its buffer metadata is Decimal({}, {})",
                field.name, col.precision, col.scale
            ),
        });
    }
    if col.width != declared_width {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is declared Decimal({precision}, {scale}) ({declared_width} bytes per row) but its buffer stores {}-byte rows",
                field.name, col.width
            ),
        });
    }
    if declared_width.checked_mul(num_rows) != Some(col.data.len()) {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is Decimal({precision}, {scale}) over {num_rows} rows so its body must be {} bytes, but the buffer holds {}",
                field.name,
                declared_width.saturating_mul(num_rows),
                col.data.len()
            ),
        });
    }
    Ok(())
}

/// Whether `value_type` (the unwrapped inner value type) and `column` form a
/// supported, matching pair this encoder can write.
///
/// This must list exactly the matching arms of [`encode_column_body`]; keep the two
/// in sync the same way [`is_encodable`] is. [`validate_column`] uses it to reject a
/// wrong-buffer or not-yet-encodable column before any bytes are written, which is
/// what lets [`write_block_into`] treat the body match as structurally infallible.
fn column_variant_matches(value_type: &ChType, column: &Column) -> bool {
    matches!(
        (value_type, column),
        (ChType::Bool, Column::Bool(_))
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
            | (ChType::Date, Column::Date(_))
            | (ChType::Date32, Column::Date32(_))
            | (ChType::DateTime { .. }, Column::DateTime(_))
            | (ChType::DateTime64 { .. }, Column::DateTime64(_))
            | (ChType::Uuid, Column::Uuid(_))
            | (ChType::Ipv4, Column::Ipv4(_))
            | (ChType::Ipv6, Column::Ipv6(_))
            | (ChType::String, Column::Utf8(_))
            | (ChType::FixedString(_), Column::FixedBinary(_))
            | (ChType::Enum8 { .. }, Column::Enum8(_))
            | (ChType::Enum16 { .. }, Column::Enum16(_))
            | (ChType::Decimal { .. }, Column::Decimal(_))
    )
}

/// Write one framed Native block for `batch` to `buf`.
///
/// Assumes `batch` has passed [`validate_block`]: the type string round-trips, and
/// the `(type, buffer)` pair matches a real arm of [`encode_column_body`], so no
/// structural error can occur partway through the write. The `Result` is retained
/// only for the defensive fall-through in [`encode_column_body`], which cannot fire
/// after validation, so this keeps the encoder panic-free without a partial-stream
/// window in practice.
fn write_block_into(
    buf: &mut Vec<u8>,
    batch: &ColBatch,
    options: &EncodeOptions,
) -> Result<(), EncodeError> {
    // BlockInfo preamble, only at revision > 0 (server `NativeWriter::write` gates
    // `block.info.write` on `client_revision > 0`).
    if options.protocol_revision > 0 {
        write_block_info(buf, options.protocol_revision);
    }
    write_varint(buf, batch.schema.num_fields() as u64);
    write_varint(buf, batch.num_rows as u64);

    for (field, column) in batch.schema.fields.iter().zip(&batch.columns) {
        write_string(buf, field.name.as_bytes());
        // The type string is the canonical name `ChType::Display` renders, the same
        // string `parse_ch_type` accepts on decode; `validate_block` confirmed it
        // round-trips.
        write_string(buf, field.ch_type.to_string().as_bytes());
        // Custom-serialization marker: one byte, 0 = default serialization,
        // written for every column even at zero rows, present only at
        // revision >= 54454 (server gates it the same way on read).
        if options.protocol_revision >= DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION {
            buf.push(0x00);
        }
        encode_column_data(buf, field, column)?;
    }
    Ok(())
}

/// Write the standard client `BlockInfo` preamble, the inverse of
/// `read_block_info` and identical to what `BlockInfo::write` emits for a plain
/// data block: field 1 `is_overflows` = false, field 2 `bucket_num` = -1, an
/// empty `out_of_order_buckets` vector at revision >= 54480 (field 3), then the
/// field-0 terminator.
fn write_block_info(buf: &mut Vec<u8>, revision: u64) {
    write_varint(buf, 1); // field 1: is_overflows
    buf.push(0x00); // false
    write_varint(buf, 2); // field 2: bucket_num
    buf.extend_from_slice(&(-1i32).to_le_bytes()); // -1
    if revision >= DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS {
        write_varint(buf, 3); // field 3: out_of_order_buckets
        write_varint(buf, 0); // empty vector (count 0)
    }
    write_varint(buf, 0); // terminator
}

/// Write a varint length prefix followed by the raw bytes, the inverse of
/// [`super::varint::ByteReader::read_varint_string`]. Used for the column name
/// and type-name headers.
fn write_string(buf: &mut Vec<u8>, bytes: &[u8]) {
    write_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

/// Append a fixed-width primitive column as a single contiguous little-endian
/// run, the inverse of `decode_primitive!`.
///
/// On little-endian targets the in-memory `[T]` already is its little-endian wire
/// image, so the whole run is one `extend_from_slice` with no per-element work. On
/// big-endian targets each element is byte-swapped through `to_le_bytes`, so the
/// bytes written are little-endian on every host, matching the wire format.
macro_rules! encode_primitive {
    ($buf:expr, $values:expr, $ty:ty) => {{
        let values: &[$ty] = $values;
        #[cfg(target_endian = "little")]
        {
            // Safety: `values` is a live `&[$ty]`, so its element storage is
            // `size_of_val(values)` bytes of initialized memory, valid to read for
            // the borrow. Reinterpreting the element pointer as `*const u8` is
            // always aligned (`u8` has alignment 1) and stays in bounds because the
            // byte length is exactly the slice's element storage. It is read-only,
            // so the borrow of `values` is not aliased mutably.
            let byte_len = std::mem::size_of_val(values);
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(values.as_ptr() as *const u8, byte_len) };
            $buf.extend_from_slice(bytes);
        }
        #[cfg(target_endian = "big")]
        {
            for v in values {
                $buf.extend_from_slice(&v.to_le_bytes());
            }
        }
    }};
}

/// Encode one column into `buf` (no header): the `Nullable` null map if the
/// declared type is a wrapper, then the value body.
///
/// A `Nullable(T)` is the inverse of [`super::decode::decode_column`]: the per-row
/// null map is written first, then the inner type's body from the same physical
/// column buffer, which carries the inner variant plus the validity bitmap. A
/// plain type goes straight to its body.
fn encode_column_data(
    buf: &mut Vec<u8>,
    field: &Field,
    column: &Column,
) -> Result<(), EncodeError> {
    if let ChType::Nullable(inner) = &field.ch_type {
        encode_null_map(buf, column);
        return encode_column_body(buf, field, inner, column);
    }
    encode_column_body(buf, field, &field.ch_type, column)
}

/// Encode a `Nullable(T)` null map: one byte per row, 0x00 = valid, 0x01 = NULL,
/// written before the inner values, the inverse of
/// [`super::decode::decode_null_map`].
///
/// [`crate::bitmap::Bitmap::from_ch_null_map`] packs that per-row byte into the
/// Arrow validity convention (bit 1 = valid), so here we unpack it and flip the
/// polarity back: valid -> 0x00, NULL -> 0x01. A column with no validity bitmap is
/// all-valid, so an all-zero map is written. The bitmap length is validated against
/// `num_rows` in [`validate_column`] before any bytes are written, so every byte
/// read here is in range.
///
/// Walks the packed validity bitmap one byte at a time rather than one row at a
/// time (the same shape as [`encode_bool_data`] and the inverse of
/// [`crate::bitmap::Bitmap::from_ch_null_map`]), so the per-row `index / 8` and
/// `index % 8` recompute is amortized to one shift per bit.
fn encode_null_map(buf: &mut Vec<u8>, column: &Column) {
    let num_rows = column.len();
    buf.reserve(num_rows);
    match column.validity() {
        None => buf.resize(buf.len() + num_rows, 0x00),
        Some(validity) => {
            let bytes = validity.as_bytes();
            let full_bytes = num_rows / 8;
            for &byte in &bytes[..full_bytes] {
                for bit in 0..8 {
                    // Arrow bit 1 = valid; the null map is 0x01 = NULL, so flip.
                    buf.push(((byte >> bit) & 1) ^ 1);
                }
            }
            let trailing = num_rows % 8;
            if trailing > 0 {
                let byte = bytes[full_bytes];
                for bit in 0..trailing {
                    buf.push(((byte >> bit) & 1) ^ 1);
                }
            }
        }
    }
}

/// Encode one column's value body (no header, no null map) into `buf`.
///
/// Matches on the `(ch_type, column)` pair so the on-wire type string (written
/// from `field.ch_type`) and the body (written from the `Column` buffer) can
/// never disagree. `ch_type` is the concrete value type: for a `Nullable(T)`
/// column it is the already-unwrapped inner `T`, so a null map is never handled
/// here. A supported type declared under a mismatched buffer variant (e.g.
/// `Int64` paired with a `Column::Int32`) is an [`EncodeError::InconsistentBatch`],
/// not a wrong-width column on the wire. Any not-yet-supported type falls through
/// to [`EncodeError::UnsupportedType`].
fn encode_column_body(
    buf: &mut Vec<u8>,
    field: &Field,
    ch_type: &ChType,
    column: &Column,
) -> Result<(), EncodeError> {
    match (ch_type, column) {
        (ChType::Bool, Column::Bool(c)) => encode_bool_data(buf, c),
        (ChType::Int8, Column::Int8(c)) => encode_primitive!(buf, &c.values, i8),
        (ChType::Int16, Column::Int16(c)) => encode_primitive!(buf, &c.values, i16),
        (ChType::Int32, Column::Int32(c)) => encode_primitive!(buf, &c.values, i32),
        (ChType::Int64, Column::Int64(c)) => encode_primitive!(buf, &c.values, i64),
        (ChType::UInt8, Column::UInt8(c)) => encode_primitive!(buf, &c.values, u8),
        (ChType::UInt16, Column::UInt16(c)) => encode_primitive!(buf, &c.values, u16),
        (ChType::UInt32, Column::UInt32(c)) => encode_primitive!(buf, &c.values, u32),
        (ChType::UInt64, Column::UInt64(c)) => encode_primitive!(buf, &c.values, u64),
        (ChType::Float32, Column::Float32(c)) => encode_primitive!(buf, &c.values, f32),
        (ChType::Float64, Column::Float64(c)) => encode_primitive!(buf, &c.values, f64),
        // Temporal types are plain little-endian primitives at their native width;
        // timezone and precision live only in the type string (rendered by
        // `ChType::Display`), never in the per-row data, so each is just the
        // matching `encode_primitive!` run, the inverse of `decode_primitive!`.
        (ChType::Date, Column::Date(c)) => encode_primitive!(buf, &c.values, u16),
        (ChType::Date32, Column::Date32(c)) => encode_primitive!(buf, &c.values, i32),
        (ChType::DateTime { .. }, Column::DateTime(c)) => encode_primitive!(buf, &c.values, u32),
        (ChType::DateTime64 { .. }, Column::DateTime64(c)) => {
            encode_primitive!(buf, &c.values, i64)
        }
        // Enum8/Enum16 are byte-identical to Int8/Int16 on the wire
        // (`SerializationEnum` inherits `SerializationNumber` and overrides no
        // bulk method); the name->value map lives only in the type string
        // (rendered by `ChType::Display`), never in the per-row data, so each is
        // the matching `encode_primitive!` run over the underlying signed-int
        // buffer, the exact inverse of the decoder's `Enum8`/`Enum16` arms.
        //
        // The enum map itself is not semantically validated here: a degenerate
        // `ChType::Enum8` (empty variant list, or duplicate names/values) renders
        // a type string the decode parser accepts by design (it round-trips
        // whatever the server emitted), so the header round-trip check in
        // `validate_column` does not reject it, and a per-row value outside the
        // declared set still encodes as a plain Int8/Int16. Both are semantic
        // legality the server owns, not wire framing: the same trusted-input
        // boundary as a `DateTime64` precision above 9, so the server rejects a
        // malformed map on INSERT rather than the encoder rejecting it locally.
        (ChType::Enum8 { .. }, Column::Enum8(c)) => encode_primitive!(buf, &c.values, i8),
        (ChType::Enum16 { .. }, Column::Enum16(c)) => encode_primitive!(buf, &c.values, i16),
        // UUID and IPv6 bodies are 16 raw bytes per row written verbatim from the
        // width-16 fixed-binary buffer, with NO reordering, the inverse of the
        // decoder's passthrough `Uuid`/`Ipv6` arms over
        // `decode_fixed_binary_data`. The bytes stay in wire order (UUID: the
        // UInt128 POD dump, not RFC-4122; IPv6: network byte order); any host
        // byte-order mapping is a binding concern on both directions.
        (ChType::Uuid, Column::Uuid(c)) => encode_fixed_binary_data(buf, c),
        (ChType::Ipv6, Column::Ipv6(c)) => encode_fixed_binary_data(buf, c),
        // IPv4 is a UInt32 in bulk (`SerializationIP<IPv4>` serializes identically
        // to `SerializationNumber<UInt32>`), so it is the same contiguous
        // little-endian run as `UInt32`, the inverse of the decoder's `Ipv4`
        // `decode_primitive!` arm.
        (ChType::Ipv4, Column::Ipv4(c)) => encode_primitive!(buf, &c.values, u32),
        (ChType::String, Column::Utf8(c)) => encode_string_data(buf, c),
        (ChType::FixedString(_), Column::FixedBinary(c)) => encode_fixed_binary_data(buf, c),
        (ChType::Decimal { .. }, Column::Decimal(c)) => encode_decimal_data(buf, c),
        // Defensive: `validate_column` rejects every unsupported type and every
        // mismatched `(type, buffer)` pair before the write phase, so this arm
        // cannot occur for a validated batch. It returns the same error validation
        // would rather than panic, so the encoder stays panic-free even if a caller
        // reaches `encode_column_body` without validating first.
        _ => return Err(column_error(field, ch_type)),
    }
    Ok(())
}

/// Encode a `Bool` column body: one byte per row, 0x00 = false, 0x01 = true, the
/// inverse of [`BoolColumn::from_wire_bytes`] packing per-row bytes into the Arrow
/// bitmap. Reads each bit back out LSB-first and writes the canonical 0/1 byte;
/// the decoder treats any nonzero byte as true, but the server emits 0/1, so we do
/// too.
///
/// Walks the packed bitmap one byte at a time rather than one row at a time, so
/// the per-row `index / 8` and `index % 8` recompute is amortized to one shift per
/// bit. [`validate_column`] checks `col.bitmap.len() >= col.len.div_ceil(8)` before
/// any bytes are written, so every index read here is in range.
fn encode_bool_data(buf: &mut Vec<u8>, col: &BoolColumn) {
    buf.reserve(col.len);
    let full_bytes = col.len / 8;
    for &byte in &col.bitmap[..full_bytes] {
        for bit in 0..8 {
            buf.push((byte >> bit) & 1);
        }
    }
    let trailing = col.len % 8;
    if trailing > 0 {
        let byte = col.bitmap[full_bytes];
        for bit in 0..trailing {
            buf.push((byte >> bit) & 1);
        }
    }
}

/// Validate a `Utf8Column`'s Arrow offsets before its body is written.
///
/// [`encode_string_data`] slices `data[offsets[i]..offsets[i+1]]` per row, so a
/// caller-constructed column with non-monotonic offsets, an offset past
/// `data.len()`, or a negative offset (which `as usize` wraps to a huge value)
/// would panic mid-write, and offsets that do not cover `data` exactly would
/// silently drop leading or trailing bytes from the wire. `Column` fields are
/// public and bindings build these by hand for the insert path, so reject all of
/// these as [`EncodeError::InconsistentBatch`] here rather than trust the buffer.
/// This is O(num_rows) once per column, off the per-byte write path.
fn validate_utf8_column(
    field: &Field,
    col: &Utf8Column,
    num_rows: usize,
) -> Result<(), EncodeError> {
    let reject = |detail: String| Err(EncodeError::InconsistentBatch { detail });

    // A zero-row column carries no data and either the single sentinel `[0]` (what
    // the decoder emits) or no offsets at all. Accept both, reject anything else.
    if num_rows == 0 {
        let well_formed_empty =
            col.data.is_empty() && (col.offsets.is_empty() || col.offsets == [0]);
        if well_formed_empty {
            return Ok(());
        }
        return reject(format!(
            "column {:?} declares 0 rows but carries {} offsets and {} data bytes",
            field.name,
            col.offsets.len(),
            col.data.len()
        ));
    }

    // Arrow layout: one offset per row plus a trailing end offset. This is already
    // implied by the `column.len() == num_rows` check earlier in `validate_column`
    // (`Utf8Column::len()` is `offsets.len() - 1`), but assert it explicitly so the
    // `offsets[num_rows]` index below is in range regardless of check ordering.
    if col.offsets.len() != num_rows + 1 {
        return reject(format!(
            "column {:?} declares {num_rows} rows so it needs {} offsets, but carries {}",
            field.name,
            num_rows + 1,
            col.offsets.len()
        ));
    }
    // Offsets must start at 0 (Arrow convention) and be monotonic non-decreasing.
    // Checking monotonicity from a zero start also proves every offset is
    // non-negative, so the `as usize` casts in `encode_string_data` cannot wrap.
    if col.offsets[0] != 0 {
        return reject(format!(
            "column {:?} has a nonzero first offset {}; Arrow string offsets start at 0",
            field.name, col.offsets[0]
        ));
    }
    for pair in col.offsets.windows(2) {
        if pair[1] < pair[0] {
            return reject(format!(
                "column {:?} has non-monotonic offsets ({} then {})",
                field.name, pair[0], pair[1]
            ));
        }
    }
    // The final offset must cover the data buffer exactly: a smaller value would
    // leave trailing bytes that never reach the wire (silent data loss), a larger
    // one would slice out of bounds. `offsets[0] == 0` and monotonicity above make
    // this final offset non-negative, so the cast is sound.
    let end = col.offsets[num_rows];
    if end as usize != col.data.len() {
        return reject(format!(
            "column {:?} offsets end at {end} but the data buffer holds {} bytes",
            field.name,
            col.data.len()
        ));
    }
    Ok(())
}

/// Encode a `String` column body: one varint length prefix then the raw value
/// bytes, per row, the inverse of [`super::decode::decode_string_data`].
///
/// The values are walked straight out of the Arrow offsets+data buffer, one
/// sub-slice of `data` per row, so there is no per-row allocation and the value
/// bytes are copied exactly once. A zero-row column has `offsets == [0]`, so
/// `windows(2)` yields nothing and no body is written.
fn encode_string_data(buf: &mut Vec<u8>, col: &Utf8Column) {
    // One varint length prefix (>= 1 byte) per value plus the value bytes, so this
    // is a tight lower bound on the body size and avoids reallocating for the
    // common short-string case.
    buf.reserve(col.data.len() + col.offsets.len());
    // `validate_column` validated these offsets via `validate_utf8_column`: they
    // start at 0, are monotonic non-decreasing, and end at `data.len()`, so every
    // sub-slice is in range and the `as usize` casts cannot wrap.
    for pair in col.offsets.windows(2) {
        let value = &col.data[pair[0] as usize..pair[1] as usize];
        write_varint(buf, value.len() as u64);
        buf.extend_from_slice(value);
    }
}

/// Encode a fixed-width binary column body (`FixedString(N)`, `UUID`, `IPv6`):
/// the contiguous `width * num_rows` data buffer written verbatim, the inverse
/// of [`super::decode::decode_fixed_binary_data`]. There is no per-row framing;
/// the width lives in the type string for `FixedString(N)` and is implied (16)
/// for `UUID` and `IPv6`. The bytes are not reordered: `UUID` stays in its wire
/// UInt128 POD order and `IPv6` in network byte order, matching the decode
/// passthrough (the RFC-4122 / host-address mapping is a binding concern).
///
/// [`validate_column`] already confirmed the stored width matches the declared
/// or implied width and `col.data.len() == width * num_rows`, so the buffer is
/// exactly the wire body and this is a single verbatim copy.
fn encode_fixed_binary_data(buf: &mut Vec<u8>, col: &FixedBinaryColumn) {
    buf.extend_from_slice(&col.data);
}

/// Encode a `Decimal(P, S)` column body: one contiguous fixed-width scaled
/// integer per row, written verbatim from `DecimalColumn::data`.
///
/// Confirmed at v26.6.1.1193-stable in `SerializationDecimalBase`: the body is
/// raw little-endian fixed-width integer bytes with no per-row framing and no
/// precision/scale in-band. `DecimalColumn::data` is already wire-order bytes, so
/// this is one copy. Negative values are inferred to be little-endian
/// two's-complement from signed backing types and raw integer storage.
fn encode_decimal_data(buf: &mut Vec<u8>, col: &DecimalColumn) {
    buf.extend_from_slice(&col.data);
}

/// Classify a column that did not match any supported `(ch_type, column)` pair.
///
/// `ch_type` is the concrete value type the body match failed on (the unwrapped
/// inner for a `Nullable(T)`). If that type is one this encoder supports, the
/// buffer must have been the wrong variant, so the type string and body would
/// disagree: that is an [`EncodeError::InconsistentBatch`]. Otherwise the type
/// itself is not yet supported (a type whose encoder has not landed, possibly
/// under a `Nullable` wrapper), which is an [`EncodeError::UnsupportedType`]
/// reporting the full declared type.
fn column_error(field: &Field, ch_type: &ChType) -> EncodeError {
    if is_encodable(ch_type) {
        EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is declared {} but its buffer is a mismatched column variant",
                field.name, field.ch_type
            ),
        }
    } else {
        EncodeError::UnsupportedType {
            column: field.name.clone(),
            ch_type: field.ch_type.clone(),
        }
    }
}

/// The concrete value types this encoder can write. Encode coverage is kept a
/// subset of decode coverage, and this predicate is the single place that lists
/// it, so [`column_error`] can tell a wrong-buffer mismatch (`InconsistentBatch`)
/// apart from a genuinely unsupported type (`UnsupportedType`). It lists the
/// unwrapped value types only; the `Nullable` wrapper composes with any type here
/// via [`encode_null_map`]. Extend it as each new type's arm lands in
/// [`encode_column_body`].
fn is_encodable(ch_type: &ChType) -> bool {
    matches!(
        ch_type,
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
            | ChType::Date
            | ChType::Date32
            | ChType::DateTime { .. }
            | ChType::DateTime64 { .. }
            | ChType::Uuid
            | ChType::Ipv4
            | ChType::Ipv6
            | ChType::String
            | ChType::FixedString(_)
            | ChType::Enum8 { .. }
            | ChType::Enum16 { .. }
            | ChType::Decimal { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitmap::Bitmap;
    use crate::column::{DecimalColumn, DictionaryColumn, PrimitiveColumn};
    use crate::native::decode::{decode_all_bytes, DecodeOptions, DBMS_TCP_PROTOCOL_VERSION};
    use crate::schema::Schema;

    /// Build a `Utf8Column` from raw byte values, computing the Arrow offsets the
    /// same way the decoder does (starting at 0, one entry past each value).
    fn utf8_column(values: &[&[u8]]) -> Utf8Column {
        let mut offsets = Vec::with_capacity(values.len() + 1);
        let mut data = Vec::new();
        offsets.push(0i32);
        for v in values {
            data.extend_from_slice(v);
            offsets.push(data.len() as i32);
        }
        Utf8Column::new(offsets, data)
    }

    /// Build a `FixedBinaryColumn` of the given width from equal-width byte
    /// values, concatenated into the contiguous data buffer.
    fn fixed_binary_column(width: usize, values: &[&[u8]]) -> FixedBinaryColumn {
        let mut data = Vec::with_capacity(width * values.len());
        for v in values {
            assert_eq!(
                v.len(),
                width,
                "fixed-string test value must be {width} bytes"
            );
            data.extend_from_slice(v);
        }
        FixedBinaryColumn::new(data, width)
    }

    /// A `String` column and a `FixedString(4)` column over four rows. The string
    /// values include an empty string and a value longer than the fixed width to
    /// exercise the varint length framing; the fixed-string values include an
    /// all-zero row and a zero-padded row.
    fn string_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "s".into(),
                ch_type: ChType::String,
            },
            Field {
                name: "fs".into(),
                ch_type: ChType::FixedString(4),
            },
        ];
        let columns = vec![
            Column::Utf8(utf8_column(&[b"user_1", b"", b"n", b"user_2_longer"])),
            Column::FixedBinary(fixed_binary_column(
                4,
                &[b"road", b"1234", b"\x00\x00\x00\x00", b"n\x00\x00\x00"],
            )),
        ];
        ColBatch::new(Schema::new(fields), columns, 4)
    }

    /// All ten fixed-width numeric columns over four rows, one batch. Values pick
    /// each type's extremes plus a couple of neutral in-range values.
    fn numeric_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "i8".into(),
                ch_type: ChType::Int8,
            },
            Field {
                name: "i16".into(),
                ch_type: ChType::Int16,
            },
            Field {
                name: "i32".into(),
                ch_type: ChType::Int32,
            },
            Field {
                name: "i64".into(),
                ch_type: ChType::Int64,
            },
            Field {
                name: "u8".into(),
                ch_type: ChType::UInt8,
            },
            Field {
                name: "u16".into(),
                ch_type: ChType::UInt16,
            },
            Field {
                name: "u32".into(),
                ch_type: ChType::UInt32,
            },
            Field {
                name: "u64".into(),
                ch_type: ChType::UInt64,
            },
            Field {
                name: "f32".into(),
                ch_type: ChType::Float32,
            },
            Field {
                name: "f64".into(),
                ch_type: ChType::Float64,
            },
        ];
        let columns = vec![
            Column::Int8(PrimitiveColumn::new(vec![i8::MIN, -13, 0, i8::MAX])),
            Column::Int16(PrimitiveColumn::new(vec![i16::MIN, -13, 0, i16::MAX])),
            Column::Int32(PrimitiveColumn::new(vec![i32::MIN, -79, 0, i32::MAX])),
            Column::Int64(PrimitiveColumn::new(vec![i64::MIN, -79, 0, i64::MAX])),
            Column::UInt8(PrimitiveColumn::new(vec![0, 13, 79, u8::MAX])),
            Column::UInt16(PrimitiveColumn::new(vec![0, 13, 79, u16::MAX])),
            Column::UInt32(PrimitiveColumn::new(vec![0, 13, 79, u32::MAX])),
            Column::UInt64(PrimitiveColumn::new(vec![0, 13, 79, u64::MAX])),
            Column::Float32(PrimitiveColumn::new(vec![-1.25, 0.0, 3.5, 79.125])),
            Column::Float64(PrimitiveColumn::new(vec![-1.25, 0.0, 3.5, 79.125])),
        ];
        ColBatch::new(Schema::new(fields), columns, 4)
    }

    /// A single `Bool` column over five rows (a non-multiple of 8 so the packed
    /// bitmap's trailing partial byte is exercised).
    fn bool_batch() -> ColBatch {
        let fields = vec![Field {
            name: "b".into(),
            ch_type: ChType::Bool,
        }];
        let columns = vec![Column::Bool(BoolColumn::from_wire_bytes(&[1, 0, 1, 1, 0]))];
        ColBatch::new(Schema::new(fields), columns, 5)
    }

    /// A `Nullable` numeric, string, and bool over four rows. The null pattern is
    /// valid, null, valid, null, so the null map exercises both states and the
    /// inner-value buffers still carry a (placeholder) value for the null rows.
    fn nullable_batch() -> ColBatch {
        // 0x00 = valid, 0x01 = null (ClickHouse null-map polarity).
        let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
        let fields = vec![
            Field {
                name: "ni32".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Int32)),
            },
            Field {
                name: "ns".into(),
                ch_type: ChType::Nullable(Box::new(ChType::String)),
            },
            Field {
                name: "nb".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Bool)),
            },
        ];
        let mut ns = utf8_column(&[b"user_1", b"", b"user_2", b""]);
        ns.validity = Some(validity());
        let columns = vec![
            Column::Int32(PrimitiveColumn::new_nullable(
                vec![13, 0, 79, 0],
                validity(),
            )),
            Column::Utf8(ns),
            Column::Bool(BoolColumn::from_wire_bytes_nullable(
                &[1, 0, 1, 0],
                validity(),
            )),
        ];
        ColBatch::new(Schema::new(fields), columns, 4)
    }

    /// The four temporal columns over four rows. `DateTime` carries a timezone and
    /// `DateTime64` carries precision plus a timezone so the type-string rendering
    /// (which is where tz and precision live, never the per-row data) is exercised
    /// on the wire. Values pick each width's boundaries plus neutral in-range days,
    /// and `Date32`/`DateTime64` include a negative pre-epoch value to prove the
    /// signed little-endian round-trip.
    fn temporal_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "d".into(),
                ch_type: ChType::Date,
            },
            Field {
                name: "d32".into(),
                ch_type: ChType::Date32,
            },
            Field {
                name: "dt".into(),
                ch_type: ChType::DateTime {
                    timezone: Some("UTC".into()),
                },
            },
            Field {
                name: "dt64".into(),
                ch_type: ChType::DateTime64 {
                    precision: 3,
                    timezone: Some("UTC".into()),
                },
            },
        ];
        let columns = vec![
            Column::Date(PrimitiveColumn::new(vec![0, 19000, 19001, u16::MAX])),
            Column::Date32(PrimitiveColumn::new(vec![i32::MIN, -25567, 0, i32::MAX])),
            Column::DateTime(PrimitiveColumn::new(vec![
                0,
                1_600_000_000,
                1_700_000_000,
                u32::MAX,
            ])),
            Column::DateTime64(PrimitiveColumn::new(vec![
                i64::MIN,
                -1_000,
                1_700_000_000_000,
                i64::MAX,
            ])),
        ];
        ColBatch::new(Schema::new(fields), columns, 4)
    }

    /// A `Nullable(DateTime64(3, 'UTC'))` column over four rows with the valid,
    /// null, valid, null pattern, proving the `Nullable` wrapper composes with a
    /// temporal inner: the null map precedes the inner i64 values.
    fn nullable_temporal_batch() -> ColBatch {
        // 0x00 = valid, 0x01 = null (ClickHouse null-map polarity).
        let validity = Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
        let fields = vec![Field {
            name: "ndt64".into(),
            ch_type: ChType::Nullable(Box::new(ChType::DateTime64 {
                precision: 3,
                timezone: Some("UTC".into()),
            })),
        }];
        let columns = vec![Column::DateTime64(PrimitiveColumn::new_nullable(
            vec![1_700_000_000_000, 0, -1_000, 0],
            validity,
        ))];
        ColBatch::new(Schema::new(fields), columns, 4)
    }

    /// A `UUID`, `IPv4`, and `IPv6` column over four rows. The UUID and IPv6
    /// values are distinct 16-byte patterns (all-zero, an ascending run, a
    /// constant, all-0xFF) that must survive verbatim with no reordering; the
    /// IPv4 values hit the u32 boundaries plus two real addresses.
    fn uuid_ip_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "u".into(),
                ch_type: ChType::Uuid,
            },
            Field {
                name: "ip4".into(),
                ch_type: ChType::Ipv4,
            },
            Field {
                name: "ip6".into(),
                ch_type: ChType::Ipv6,
            },
        ];
        let columns = vec![
            Column::Uuid(fixed_binary_column(
                16,
                &[
                    &[0u8; 16],
                    b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x0C\x0D\x0E\x0F\x10",
                    &[0x79; 16],
                    &[0xFF; 16],
                ],
            )),
            // 0.0.0.0, 127.0.0.1, 192.168.0.1, 255.255.255.255 as the standard
            // numeric value (a<<24 | b<<16 | c<<8 | d).
            Column::Ipv4(PrimitiveColumn::new(vec![
                0,
                2_130_706_433,
                3_232_235_521,
                u32::MAX,
            ])),
            // ::, ::1, 2001:db8::13, all-0xFF, in network byte order.
            Column::Ipv6(fixed_binary_column(
                16,
                &[
                    &[0u8; 16],
                    b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01",
                    b"\x20\x01\x0D\xB8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x13",
                    &[0xFF; 16],
                ],
            )),
        ];
        ColBatch::new(Schema::new(fields), columns, 4)
    }

    /// `Nullable(UUID)`, `Nullable(IPv4)`, and `Nullable(IPv6)` over four rows
    /// with the valid, null, valid, null pattern, proving the `Nullable` wrapper
    /// composes with all three: the null map precedes the inner body.
    fn nullable_uuid_ip_batch() -> ColBatch {
        // 0x00 = valid, 0x01 = null (ClickHouse null-map polarity).
        let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
        let fields = vec![
            Field {
                name: "nu".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Uuid)),
            },
            Field {
                name: "nip4".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Ipv4)),
            },
            Field {
                name: "nip6".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Ipv6)),
            },
        ];
        let mut nu = fixed_binary_column(16, &[&[0x13; 16], &[0u8; 16], &[0x79; 16], &[0u8; 16]]);
        nu.validity = Some(validity());
        let mut nip6 = fixed_binary_column(
            16,
            &[
                b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01",
                &[0u8; 16],
                b"\x20\x01\x0D\xB8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x13",
                &[0u8; 16],
            ],
        );
        nip6.validity = Some(validity());
        let columns = vec![
            Column::Uuid(nu),
            Column::Ipv4(PrimitiveColumn::new_nullable(
                vec![2_130_706_433, 0, 3_232_235_521, 0],
                validity(),
            )),
            Column::Ipv6(nip6),
        ];
        ColBatch::new(Schema::new(fields), columns, 4)
    }

    /// An `Enum8` and an `Enum16` column over four rows. The variant lists carry
    /// distinct signed values in the server's ascending-by-value order, including
    /// a negative variant and each width's boundary (`i8::MIN`/`i8::MAX`,
    /// `i16::MIN`/`i16::MAX`), and the physical buffers pick those boundary and
    /// negative values so the little-endian byte order and sign of the underlying
    /// int are exercised. The name->value map lives only in the type string
    /// (`ChType::Display`), so this proves that string round-trips too.
    fn enum_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "e8".into(),
                ch_type: ChType::Enum8 {
                    variants: vec![
                        ("floor".into(), i8::MIN),
                        ("neg".into(), -13),
                        ("idle".into(), 0),
                        ("busy".into(), 13),
                        ("ceil".into(), i8::MAX),
                    ],
                },
            },
            Field {
                name: "e16".into(),
                ch_type: ChType::Enum16 {
                    variants: vec![
                        ("floor".into(), i16::MIN),
                        ("neg".into(), -79),
                        ("idle".into(), 0),
                        ("busy".into(), 79),
                        ("ceil".into(), i16::MAX),
                    ],
                },
            },
        ];
        let columns = vec![
            Column::Enum8(PrimitiveColumn::new(vec![i8::MIN, -13, 0, i8::MAX])),
            Column::Enum16(PrimitiveColumn::new(vec![i16::MIN, -79, 0, i16::MAX])),
        ];
        ColBatch::new(Schema::new(fields), columns, 4)
    }

    /// A `Nullable(Enum8(...))` and a `Nullable(Enum16(...))` column over four
    /// rows with the valid, null, valid, null pattern, proving the `Nullable`
    /// wrapper composes with an enum inner: the null map precedes the inner
    /// signed-int values.
    fn nullable_enum_batch() -> ColBatch {
        // 0x00 = valid, 0x01 = null (ClickHouse null-map polarity).
        let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
        let fields = vec![
            Field {
                name: "ne8".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Enum8 {
                    variants: vec![("neg".into(), -13), ("idle".into(), 0), ("busy".into(), 13)],
                })),
            },
            Field {
                name: "ne16".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Enum16 {
                    variants: vec![("neg".into(), -79), ("idle".into(), 0), ("busy".into(), 79)],
                })),
            },
        ];
        let columns = vec![
            Column::Enum8(PrimitiveColumn::new_nullable(
                vec![-13, 0, 13, 0],
                validity(),
            )),
            Column::Enum16(PrimitiveColumn::new_nullable(
                vec![-79, 0, 79, 0],
                validity(),
            )),
        ];
        ColBatch::new(Schema::new(fields), columns, 4)
    }

    /// Build a DecimalColumn of the given width from equal-width raw wire-order
    /// byte values.
    fn decimal_column(width: usize, precision: u8, scale: u8, values: &[&[u8]]) -> DecimalColumn {
        let mut data = Vec::with_capacity(width * values.len());
        for v in values {
            assert_eq!(v.len(), width, "decimal test value must be {width} bytes");
            data.extend_from_slice(v);
        }
        DecimalColumn::new(data, width, precision, scale)
    }

    /// Decimal columns covering all four precision-derived widths. The raw bytes
    /// include positive, zero, and negative two's-complement values, but the core
    /// treats them as already-wire-order bytes and does not materialize integers.
    fn decimal_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "d32".into(),
                ch_type: ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 32,
                },
            },
            Field {
                name: "d64".into(),
                ch_type: ChType::Decimal {
                    precision: 18,
                    scale: 9,
                    bits: 64,
                },
            },
            Field {
                name: "d128".into(),
                ch_type: ChType::Decimal {
                    precision: 38,
                    scale: 10,
                    bits: 128,
                },
            },
            Field {
                name: "d256".into(),
                ch_type: ChType::Decimal {
                    precision: 76,
                    scale: 20,
                    bits: 256,
                },
            },
        ];
        let d32_neg = (-13i32).to_le_bytes();
        let d32_pos = 79i32.to_le_bytes();
        let d64_neg = (-13i64).to_le_bytes();
        let d64_pos = 79i64.to_le_bytes();
        let d64_zero = [0u8; 8];
        let d128_neg = [0xFFu8; 16];
        let d128_zero = [0u8; 16];
        let d256_neg = [0xFFu8; 32];
        let d256_zero = [0u8; 32];
        let columns = vec![
            Column::Decimal(decimal_column(
                4,
                9,
                4,
                &[&d32_neg, &[0, 0, 0, 0], &d32_pos],
            )),
            Column::Decimal(decimal_column(
                8,
                18,
                9,
                &[&d64_neg, &d64_zero, &d64_pos],
            )),
            Column::Decimal(decimal_column(
                16,
                38,
                10,
                &[
                    &d128_neg,
                    &d128_zero,
                    b"\x4F\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
                ],
            )),
            Column::Decimal(decimal_column(
                32,
                76,
                20,
                &[
                    &d256_neg,
                    &d256_zero,
                    b"\x13\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
                ],
            )),
        ];
        ColBatch::new(Schema::new(fields), columns, 3)
    }

    /// A `Nullable(Decimal(18, 9))` column with valid, null, valid, null rows.
    fn nullable_decimal_batch() -> ColBatch {
        let validity = Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
        let neg = (-13i64).to_le_bytes();
        let zero = [0u8; 8];
        let pos = 79i64.to_le_bytes();
        let values = [&neg[..], &zero[..], &pos[..], &zero[..]];
        let mut col = decimal_column(8, 18, 9, &values);
        col.validity = Some(validity);
        ColBatch::new(
            Schema::new(vec![Field {
                name: "nd".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Decimal {
                    precision: 18,
                    scale: 9,
                    bits: 64,
                })),
            }]),
            vec![Column::Decimal(col)],
            4,
        )
    }

    /// Compare two batches column by column for the types this encoder covers
    /// (the numerics, the temporal types, `UUID`/`IPv4`/`IPv6`, `String`,
    /// `FixedString`). Panics on any other variant so a wrong decode is loud.
    fn assert_batches_eq(left: &ColBatch, right: &ColBatch) {
        assert_eq!(left.schema, right.schema, "schema mismatch");
        assert_eq!(left.num_rows, right.num_rows, "row count mismatch");
        assert_eq!(
            left.columns.len(),
            right.columns.len(),
            "column count mismatch"
        );
        for (i, (a, b)) in left.columns.iter().zip(&right.columns).enumerate() {
            macro_rules! eq {
                ($va:expr, $vb:expr) => {
                    assert_eq!($va.values, $vb.values, "column {i} values differ")
                };
            }
            match (a, b) {
                (Column::Bool(x), Column::Bool(y)) => {
                    assert_eq!(x.len, y.len, "column {i} bool len differs");
                    for row in 0..x.len {
                        assert_eq!(x.get(row), y.get(row), "column {i} bool row {row} differs");
                    }
                }
                (Column::Int8(x), Column::Int8(y)) => eq!(x, y),
                (Column::Int16(x), Column::Int16(y)) => eq!(x, y),
                (Column::Int32(x), Column::Int32(y)) => eq!(x, y),
                (Column::Int64(x), Column::Int64(y)) => eq!(x, y),
                (Column::UInt8(x), Column::UInt8(y)) => eq!(x, y),
                (Column::UInt16(x), Column::UInt16(y)) => eq!(x, y),
                (Column::UInt32(x), Column::UInt32(y)) => eq!(x, y),
                (Column::UInt64(x), Column::UInt64(y)) => eq!(x, y),
                (Column::Float32(x), Column::Float32(y)) => eq!(x, y),
                (Column::Float64(x), Column::Float64(y)) => eq!(x, y),
                (Column::Date(x), Column::Date(y)) => eq!(x, y),
                (Column::Date32(x), Column::Date32(y)) => eq!(x, y),
                (Column::DateTime(x), Column::DateTime(y)) => eq!(x, y),
                (Column::DateTime64(x), Column::DateTime64(y)) => eq!(x, y),
                (Column::Enum8(x), Column::Enum8(y)) => eq!(x, y),
                (Column::Enum16(x), Column::Enum16(y)) => eq!(x, y),
                (Column::Ipv4(x), Column::Ipv4(y)) => eq!(x, y),
                (Column::Uuid(x), Column::Uuid(y)) | (Column::Ipv6(x), Column::Ipv6(y)) => {
                    assert_eq!(x.width, y.width, "column {i} width differ");
                    assert_eq!(x.data, y.data, "column {i} data differ");
                }
                (Column::Utf8(x), Column::Utf8(y)) => {
                    assert_eq!(x.offsets, y.offsets, "column {i} offsets differ");
                    assert_eq!(x.data, y.data, "column {i} data differ");
                }
                (Column::FixedBinary(x), Column::FixedBinary(y)) => {
                    assert_eq!(x.width, y.width, "column {i} width differ");
                    assert_eq!(x.data, y.data, "column {i} data differ");
                }
                (Column::Decimal(x), Column::Decimal(y)) => {
                    assert_eq!(x.width, y.width, "column {i} width differ");
                    assert_eq!(x.precision, y.precision, "column {i} precision differs");
                    assert_eq!(x.scale, y.scale, "column {i} scale differs");
                    assert_eq!(x.data, y.data, "column {i} data differ");
                }
                (other_a, other_b) => panic!("column {i}: unexpected {other_a:?} vs {other_b:?}"),
            }
            // Validity (the null map) must survive the round-trip too. Both sides
            // must agree on presence and on every row's valid/null bit.
            match (a.validity(), b.validity()) {
                (None, None) => {}
                (Some(x), Some(y)) => {
                    assert_eq!(x.len(), y.len(), "column {i} validity len differs");
                    for row in 0..x.len() {
                        assert_eq!(
                            x.is_valid(row),
                            y.is_valid(row),
                            "column {i} validity row {row} differs"
                        );
                    }
                }
                (x, y) => panic!(
                    "column {i} validity presence differs: {} vs {}",
                    x.is_some(),
                    y.is_some()
                ),
            }
        }
    }

    /// Encode `batch` as one block at `revision`, decode it back, and assert the
    /// buffers survived unchanged.
    fn roundtrip(batch: &ColBatch, revision: u64) {
        let bytes = encode_block(
            batch,
            &EncodeOptions {
                protocol_revision: revision,
            },
        )
        .unwrap();
        let decoded = decode_all_bytes(
            &bytes,
            &DecodeOptions {
                protocol_revision: revision,
            },
        )
        .unwrap_or_else(|e| panic!("decode at rev {revision} failed: {e}"));
        assert_eq!(decoded.num_chunks(), 1);
        assert_batches_eq(batch, &decoded.chunks[0]);
    }

    #[test]
    fn roundtrip_numerics_rev0() {
        roundtrip(&numeric_batch(), 0);
    }

    #[test]
    fn roundtrip_numerics_tcp_revision() {
        roundtrip(&numeric_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_strings_rev0() {
        roundtrip(&string_batch(), 0);
    }

    #[test]
    fn roundtrip_strings_tcp_revision() {
        roundtrip(&string_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_bool_rev0() {
        roundtrip(&bool_batch(), 0);
    }

    #[test]
    fn roundtrip_bool_tcp_revision() {
        roundtrip(&bool_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_nullable_rev0() {
        roundtrip(&nullable_batch(), 0);
    }

    #[test]
    fn roundtrip_nullable_tcp_revision() {
        roundtrip(&nullable_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_temporal_rev0() {
        roundtrip(&temporal_batch(), 0);
    }

    #[test]
    fn roundtrip_temporal_tcp_revision() {
        roundtrip(&temporal_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_nullable_temporal_rev0() {
        roundtrip(&nullable_temporal_batch(), 0);
    }

    #[test]
    fn roundtrip_nullable_temporal_tcp_revision() {
        roundtrip(&nullable_temporal_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_uuid_ip_rev0() {
        roundtrip(&uuid_ip_batch(), 0);
    }

    #[test]
    fn roundtrip_uuid_ip_tcp_revision() {
        roundtrip(&uuid_ip_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_nullable_uuid_ip_rev0() {
        roundtrip(&nullable_uuid_ip_batch(), 0);
    }

    #[test]
    fn roundtrip_nullable_uuid_ip_tcp_revision() {
        roundtrip(&nullable_uuid_ip_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_enum_rev0() {
        roundtrip(&enum_batch(), 0);
    }

    #[test]
    fn roundtrip_enum_tcp_revision() {
        roundtrip(&enum_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_nullable_enum_rev0() {
        roundtrip(&nullable_enum_batch(), 0);
    }

    #[test]
    fn roundtrip_nullable_enum_tcp_revision() {
        roundtrip(&nullable_enum_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_decimal_rev0() {
        roundtrip(&decimal_batch(), 0);
    }

    #[test]
    fn roundtrip_decimal_tcp_revision() {
        roundtrip(&decimal_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_nullable_decimal_rev0() {
        roundtrip(&nullable_decimal_batch(), 0);
    }

    #[test]
    fn roundtrip_nullable_decimal_tcp_revision() {
        roundtrip(&nullable_decimal_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn zero_row_block_roundtrips_schema() {
        // A zero-row block still carries full column headers. The decoder keeps
        // the schema but drops the empty block from `chunks`.
        let fields = vec![
            Field {
                name: "n".into(),
                ch_type: ChType::Int32,
            },
            Field {
                name: "x".into(),
                ch_type: ChType::Float64,
            },
            Field {
                name: "u".into(),
                ch_type: ChType::Uuid,
            },
            Field {
                name: "ip4".into(),
                ch_type: ChType::Ipv4,
            },
            Field {
                name: "ip6".into(),
                ch_type: ChType::Ipv6,
            },
            Field {
                name: "dec".into(),
                ch_type: ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 32,
                },
            },
        ];
        let columns = vec![
            Column::Int32(PrimitiveColumn::new(vec![])),
            Column::Float64(PrimitiveColumn::new(vec![])),
            Column::Uuid(FixedBinaryColumn::new(Vec::new(), 16)),
            Column::Ipv4(PrimitiveColumn::new(Vec::new())),
            Column::Ipv6(FixedBinaryColumn::new(Vec::new(), 16)),
            Column::Decimal(DecimalColumn::new(Vec::new(), 4, 9, 4)),
        ];
        let batch = ColBatch::new(Schema::new(fields), columns, 0);
        for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
            let bytes = encode_block(
                &batch,
                &EncodeOptions {
                    protocol_revision: revision,
                },
            )
            .unwrap();
            let decoded = decode_all_bytes(
                &bytes,
                &DecodeOptions {
                    protocol_revision: revision,
                },
            )
            .unwrap();
            assert_eq!(decoded.num_rows(), 0);
            assert_eq!(decoded.num_chunks(), 0);
            assert_eq!(decoded.schema, batch.schema);
        }
    }

    #[test]
    fn encode_chunked_roundtrips_multiple_blocks() {
        let field = Field {
            name: "n".into(),
            ch_type: ChType::Int32,
        };
        let chunk = |vals: Vec<i32>| {
            let n = vals.len();
            std::sync::Arc::new(ColBatch::new(
                Schema::new(vec![field.clone()]),
                vec![Column::Int32(PrimitiveColumn::new(vals))],
                n,
            ))
        };
        let batch = ChunkedBatch {
            schema: Schema::new(vec![field.clone()]),
            chunks: vec![chunk(vec![13, 14]), chunk(vec![15, 16]), chunk(vec![17])],
        };
        let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
        let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
        assert_eq!(decoded.num_chunks(), 3);
        assert_eq!(decoded.num_rows(), 5);
        let got: Vec<Vec<i32>> = decoded
            .chunks
            .iter()
            .map(|c| match c.column(0) {
                Column::Int32(p) => p.values.clone(),
                other => panic!("expected Int32, got {other:?}"),
            })
            .collect();
        assert_eq!(got, vec![vec![13, 14], vec![15, 16], vec![17]]);
    }

    #[test]
    fn encode_chunked_roundtrips_uuid_ip_blocks() {
        // Two blocks of the UUID/IPv4/IPv6 schema round-trip through
        // `encode_chunked`, staying separate chunks with the buffers intact
        // (blocks are never merged; see AGENTS.md).
        let schema = uuid_ip_batch().schema.clone();
        let chunk = |uuid_byte: u8, ip4: u32, ip6_byte: u8| {
            std::sync::Arc::new(ColBatch::new(
                schema.clone(),
                vec![
                    Column::Uuid(FixedBinaryColumn::new(vec![uuid_byte; 32], 16)),
                    Column::Ipv4(PrimitiveColumn::new(vec![ip4, ip4 + 1])),
                    Column::Ipv6(FixedBinaryColumn::new(vec![ip6_byte; 32], 16)),
                ],
                2,
            ))
        };
        let batch = ChunkedBatch {
            schema: schema.clone(),
            chunks: vec![
                chunk(0x13, 2_130_706_433, 0x20),
                chunk(0x79, 3_232_235_521, 0x0D),
            ],
        };
        for revision in [0, DBMS_TCP_PROTOCOL_VERSION] {
            let bytes = encode_chunked(
                &batch,
                &EncodeOptions {
                    protocol_revision: revision,
                },
            )
            .unwrap();
            let decoded = decode_all_bytes(
                &bytes,
                &DecodeOptions {
                    protocol_revision: revision,
                },
            )
            .unwrap_or_else(|e| panic!("decode at rev {revision} failed: {e}"));
            assert_eq!(decoded.num_chunks(), 2);
            for (sent, got) in batch.chunks.iter().zip(&decoded.chunks) {
                assert_batches_eq(sent, got);
            }
        }
    }

    #[test]
    fn encode_chunked_roundtrips_decimal_blocks() {
        // Decimal blocks stay separate chunks, never concatenated.
        let field = Field {
            name: "dec".into(),
            ch_type: ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            },
        };
        let chunk = |vals: Vec<i32>| {
            let mut data = Vec::with_capacity(vals.len() * 4);
            for v in &vals {
                data.extend_from_slice(&v.to_le_bytes());
            }
            std::sync::Arc::new(ColBatch::new(
                Schema::new(vec![field.clone()]),
                vec![Column::Decimal(DecimalColumn::new(data, 4, 9, 4))],
                vals.len(),
            ))
        };
        let batch = ChunkedBatch {
            schema: Schema::new(vec![field.clone()]),
            chunks: vec![chunk(vec![-13, 0]), chunk(vec![79])],
        };
        let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
        let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
        assert_eq!(decoded.num_chunks(), 2);
        for (sent, got) in batch.chunks.iter().zip(&decoded.chunks) {
            assert_batches_eq(sent, got);
        }
    }

    #[test]
    fn rev0_frames_exact_bytes() {
        // Pin the rev-0 framing byte-for-byte: no BlockInfo, no marker. One Int32
        // column "n" with a single row = 1.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "n".into(),
                ch_type: ChType::Int32,
            }]),
            vec![Column::Int32(PrimitiveColumn::new(vec![1]))],
            1,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x01, // num_rows = 1
            0x01, b'n', // name "n"
            0x05, b'I', b'n', b't', b'3', b'2', // type "Int32"
            0x01, 0x00, 0x00, 0x00, // Int32 value 1, little-endian
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn rev_tcp_frames_block_info_and_marker() {
        // At the TCP revision the block leads with the BlockInfo preamble and each
        // column header carries the default (0) custom-serialization marker.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "n".into(),
                ch_type: ChType::UInt8,
            }]),
            vec![Column::UInt8(PrimitiveColumn::new(vec![79]))],
            1,
        );
        let bytes = encode_block(
            &batch,
            &EncodeOptions {
                protocol_revision: DBMS_TCP_PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let expected = [
            0x01, 0x00, // field 1 is_overflows = false
            0x02, 0xFF, 0xFF, 0xFF, 0xFF, // field 2 bucket_num = -1
            0x03, 0x00, // field 3 out_of_order_buckets = empty (rev >= 54480)
            0x00, // BlockInfo terminator
            0x01, // num_cols = 1
            0x01, // num_rows = 1
            0x01, b'n', // name "n"
            0x05, b'U', b'I', b'n', b't', b'8', // type "UInt8"
            0x00, // custom-serialization marker = default
            0x4F, // UInt8 value 79
        ];
        assert_eq!(bytes, expected);
    }

    /// A small `LowCardinality(String)` type for unsupported-type tests: decoded
    /// by this crate but not yet encodable.
    fn low_cardinality_string_type() -> ChType {
        ChType::LowCardinality(Box::new(ChType::String))
    }

    #[test]
    fn nullable_unsupported_inner_is_unsupported() {
        // `Nullable` is a supported wrapper now, but its inner type must also be
        // encodable. `LowCardinality(String)` is decoded yet not encodable, so the
        // wrapper is still rejected, and the reported type is the full declared
        // wrapper. The dictionary column is structurally valid, so the only reason
        // for rejection is the unsupported inner type.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "nlc".into(),
                ch_type: ChType::Nullable(Box::new(low_cardinality_string_type())),
            }]),
            vec![Column::Dictionary(DictionaryColumn::new_nullable(
                vec![0],
                Column::Utf8(utf8_column(&[b"user_1"])),
                Bitmap::from_ch_null_map(&[0]),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { column, ch_type } => {
                assert_eq!(column, "nlc");
                assert_eq!(
                    ch_type,
                    ChType::Nullable(Box::new(low_cardinality_string_type()))
                );
            }
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_type_reports_column_and_type() {
        // `LowCardinality(String)` is decoded but not yet encodable, so it reports
        // UnsupportedType with the column name and type.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "lc".into(),
                ch_type: low_cardinality_string_type(),
            }]),
            vec![Column::Dictionary(DictionaryColumn::new(
                vec![0],
                Column::Utf8(utf8_column(&[b"user_1"])),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { column, ch_type } => {
                assert_eq!(column, "lc");
                assert_eq!(ch_type, low_cardinality_string_type());
            }
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }

    #[test]
    fn rev0_frames_string_bytes() {
        // Pin the String body framing: one varint length prefix then the raw
        // bytes, per row. One String column "s" with a single row "hi".
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "s".into(),
                ch_type: ChType::String,
            }]),
            vec![Column::Utf8(utf8_column(&[b"hi"]))],
            1,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x01, // num_rows = 1
            0x01, b's', // name "s"
            0x06, b'S', b't', b'r', b'i', b'n', b'g', // type "String"
            0x02, b'h', b'i', // value: varint len 2 then "hi"
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn rev0_frames_fixed_string_bytes() {
        // Pin the FixedString body framing: contiguous width*num_rows bytes, no
        // per-row length prefix. One FixedString(4) column "fs", single row.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "fs".into(),
                ch_type: ChType::FixedString(4),
            }]),
            vec![Column::FixedBinary(fixed_binary_column(4, &[b"road"]))],
            1,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x01, // num_rows = 1
            0x02, b'f', b's', // name "fs"
            0x0E, b'F', b'i', b'x', b'e', b'd', b'S', b't', b'r', b'i', b'n', b'g', b'(', b'4',
            b')', // type "FixedString(4)"
            b'r', b'o', b'a', b'd', // 4 raw bytes, no length prefix
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn rev0_frames_uuid_bytes() {
        // Pin the UUID body framing: 16 raw bytes per row, passthrough in wire
        // (UInt128 POD) order, no reordering and no per-row framing. One UUID
        // column "u", single row with 16 distinct bytes, so any byte shuffle on
        // encode would break the exact comparison.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "u".into(),
                ch_type: ChType::Uuid,
            }]),
            vec![Column::Uuid(fixed_binary_column(
                16,
                &[b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x0C\x0D\x0E\x0F\x10"],
            ))],
            1,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x01, // num_rows = 1
            0x01, b'u', // name "u"
            0x04, b'U', b'U', b'I', b'D', // type "UUID"
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // 16 raw bytes,
            0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, // buffer order
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn rev0_frames_ipv4_bytes() {
        // Pin the IPv4 body framing: the standard numeric value written as a
        // little-endian u32, exactly like UInt32. One IPv4 column "ip4", single
        // row 192.168.0.1 = 0xC0A80001, so the wire bytes must be the reversed
        // 01 00 A8 C0 and any big-endian write would fail.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "ip4".into(),
                ch_type: ChType::Ipv4,
            }]),
            vec![Column::Ipv4(PrimitiveColumn::new(vec![3_232_235_521]))],
            1,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x01, // num_rows = 1
            0x03, b'i', b'p', b'4', // name "ip4"
            0x04, b'I', b'P', b'v', b'4', // type "IPv4"
            0x01, 0x00, 0xA8, 0xC0, // u32 0xC0A80001 (192.168.0.1), little-endian
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn rev0_frames_ipv6_bytes() {
        // Pin the IPv6 body framing: 16 raw bytes per row, verbatim in network
        // byte order, no per-row framing. One IPv6 column "ip6", single row with
        // 16 distinct bytes, so any byte shuffle on encode would break the exact
        // comparison.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "ip6".into(),
                ch_type: ChType::Ipv6,
            }]),
            vec![Column::Ipv6(fixed_binary_column(
                16,
                &[b"\x20\x01\x0D\xB8\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0A\x0B\x13"],
            ))],
            1,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x01, // num_rows = 1
            0x03, b'i', b'p', b'6', // name "ip6"
            0x04, b'I', b'P', b'v', b'6', // type "IPv6"
            0x20, 0x01, 0x0D, 0xB8, 0x01, 0x02, 0x03, 0x04, // 16 raw bytes,
            0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x13, // network order
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn rev0_frames_decimal_bytes() {
        // Pin the Decimal body framing: contiguous width*num_rows bytes, no
        // per-row length prefix, precision, or scale. The single Decimal(9, 4)
        // value is unscaled -13, little-endian two's-complement i32.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "d".into(),
                ch_type: ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 32,
                },
            }]),
            vec![Column::Decimal(DecimalColumn::new(
                (-13i32).to_le_bytes().to_vec(),
                4,
                9,
                4,
            ))],
            1,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x01, // num_rows = 1
            0x01, b'd', // name "d"
            0x0D, b'D', b'e', b'c', b'i', b'm', b'a', b'l', b'(', b'9', b',', b' ', b'4',
            b')', // type "Decimal(9, 4)"
            0xF3, 0xFF, 0xFF, 0xFF, // i32 -13, little-endian two's-complement
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn uuid_width_mismatch_is_rejected() {
        // A UUID buffer whose stored width is not 16 would put the wrong number of
        // bytes per row on the wire under a truthful type string; reject it before
        // any bytes are written, mirroring the FixedString width guard.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "u".into(),
                ch_type: ChType::Uuid,
            }]),
            vec![Column::Uuid(FixedBinaryColumn::new(vec![0u8; 8], 8))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn ipv6_ragged_data_is_rejected() {
        // An IPv6 buffer whose byte count is not exactly 16 * num_rows reports the
        // right row count via truncating division but would misframe the stream;
        // reject it, mirroring the FixedString ragged-data guard.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "ip6".into(),
                ch_type: ChType::Ipv6,
            }]),
            columns: vec![Column::Ipv6(FixedBinaryColumn::new(vec![0u8; 17], 16))],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn decimal_width_mismatch_is_rejected() {
        // Decimal(9, 4) is 4 bytes per row by precision, so a width-8 buffer
        // would misframe the body under the truthful type string.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "dec".into(),
                ch_type: ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 32,
                },
            }]),
            vec![Column::Decimal(DecimalColumn::new(vec![0u8; 8], 8, 9, 4))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn decimal_ragged_data_is_rejected() {
        // A Decimal buffer whose byte count is not exactly width * num_rows
        // reports the right row count via truncating division but would put too
        // many bytes on the wire.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "dec".into(),
                ch_type: ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 32,
                },
            }]),
            columns: vec![Column::Decimal(DecimalColumn::new(vec![0u8; 7], 4, 9, 4))],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn decimal_metadata_mismatch_is_rejected() {
        // The schema and DecimalColumn metadata must agree so downstream buffer
        // consumers see the same precision and scale as the type header.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "dec".into(),
                ch_type: ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 32,
                },
            }]),
            vec![Column::Decimal(DecimalColumn::new(vec![0u8; 4], 4, 9, 2))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn nullable_decimal_ragged_data_is_rejected() {
        // The Decimal body guard must apply inside `Nullable` too, after the
        // value type is unwrapped.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "dec".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 32,
                })),
            }]),
            columns: vec![Column::Decimal(DecimalColumn::new_nullable(
                vec![0u8; 7],
                4,
                9,
                4,
                Bitmap::from_ch_null_map(&[0]),
            ))],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn rev0_frames_bool_bytes() {
        // Pin the Bool body framing: one byte per row, 0x01 = true, 0x00 = false.
        // One Bool column "b" over three rows: true, false, true.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "b".into(),
                ch_type: ChType::Bool,
            }]),
            vec![Column::Bool(BoolColumn::from_wire_bytes(&[1, 0, 1]))],
            3,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x03, // num_rows = 3
            0x01, b'b', // name "b"
            0x04, b'B', b'o', b'o', b'l', // type "Bool"
            0x01, 0x00, 0x01, // one byte per row: true, false, true
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn rev0_frames_nullable_bytes() {
        // Pin the Nullable framing: the per-row null map (0x00 valid, 0x01 null)
        // precedes the inner values. One Nullable(Int32) column "n" over two rows:
        // 13 (valid), then a null row (inner value 0).
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "n".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Int32)),
            }]),
            vec![Column::Int32(PrimitiveColumn::new_nullable(
                vec![13, 0],
                Bitmap::from_ch_null_map(&[0, 1]),
            ))],
            2,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x02, // num_rows = 2
            0x01, b'n', // name "n"
            0x0F, b'N', b'u', b'l', b'l', b'a', b'b', b'l', b'e', b'(', b'I', b'n', b't', b'3',
            b'2', b')', // type "Nullable(Int32)"
            0x00, 0x01, // null map: row 0 valid, row 1 null
            0x0D, 0x00, 0x00, 0x00, // Int32 13, little-endian
            0x00, 0x00, 0x00, 0x00, // Int32 placeholder for the null row
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn nullable_validity_length_mismatch_is_rejected() {
        // A Nullable column whose validity bitmap does not cover num_rows would
        // write a null map of the wrong length; reject it as InconsistentBatch
        // before any bytes are written.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "n".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Int32)),
            }]),
            vec![Column::Int32(PrimitiveColumn::new_nullable(
                vec![13, 79],
                Bitmap::from_ch_null_map(&[0]), // covers one row, not two
            ))],
            2,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn bool_bitmap_length_mismatch_is_rejected() {
        // A Bool column whose `len` overruns its packed bitmap would panic when
        // unpacked positionally; reject it as InconsistentBatch before any bytes
        // are written. Construct the malformed column directly: `len` claims 100
        // rows but the bitmap holds one byte (room for 8). Row count is consistent
        // (`len` == num_rows), so it passes the row-count check and reaches the
        // bitmap-length guard.
        let col = BoolColumn {
            bitmap: vec![0x01],
            len: 100,
            validity: None,
        };
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "b".into(),
                ch_type: ChType::Bool,
            }]),
            vec![Column::Bool(col)],
            100,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn fixed_string_width_mismatch_is_rejected() {
        // A FixedString(4) type string paired with a width-3 buffer would emit a
        // body with the wrong bytes-per-row; reject it rather than corrupt.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "fs".into(),
                ch_type: ChType::FixedString(4),
            }]),
            columns: vec![Column::FixedBinary(fixed_binary_column(3, &[b"abc"]))],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn short_validity_buffer_is_rejected() {
        // A caller can build a `Bitmap` whose backing buffer is too short for its
        // bit length via the public `Bitmap::from_raw`, which only debug-asserts the
        // invariant. In a release build that bitmap would panic when
        // `encode_null_map` unpacks it (index out of bounds), so `validate_column`
        // must reject it as an inconsistent batch first. This test is release-only:
        // in a debug build `from_raw`'s `debug_assert!` fires at construction, so the
        // malformed state cannot be reached through the public API. 100 rows need 13
        // bitmap bytes; the buffer holds 1.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "n".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Int32)),
            }]),
            columns: vec![Column::Int32(PrimitiveColumn::new_nullable(
                vec![0i32; 100],
                Bitmap::from_raw(vec![0u8; 1], 100),
            ))],
            num_rows: 100,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn fixed_string_ragged_data_is_rejected() {
        // A FixedString(4) column whose data buffer is not a multiple of the width
        // (7 bytes) reports len() == 1 via truncating division, so it passes the
        // row-count check, but writing it verbatim would put 7 bytes where the
        // reader consumes 4, silently misframing the stream. Reject it before
        // writing. Construct directly so `ColBatch::new`'s debug_assert (which uses
        // the same truncating len()) does not mask it.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "fs".into(),
                ch_type: ChType::FixedString(4),
            }]),
            columns: vec![Column::FixedBinary(FixedBinaryColumn::new(
                b"road12X".to_vec(),
                4,
            ))],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn nullable_fixed_string_ragged_data_is_rejected() {
        // The same ragged-buffer misframe under a `Nullable(FixedString(4))` must
        // also be rejected: the value type is unwrapped before the width and
        // byte-count checks, so the guard applies inside the wrapper too.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "fs".into(),
                ch_type: ChType::Nullable(Box::new(ChType::FixedString(4))),
            }]),
            columns: vec![Column::FixedBinary(FixedBinaryColumn::new_nullable(
                b"road12X".to_vec(),
                4,
                Bitmap::from_ch_null_map(&[0]),
            ))],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    /// Build a one-column `String` batch directly from raw offsets and data so a
    /// malformed offset array reaches the encoder (the `utf8_column` helper always
    /// builds well-formed offsets). `num_rows` is set from the offsets so only the
    /// offset invariants, not the row-count check, are exercised.
    fn string_batch_from_parts(offsets: Vec<i32>, data: Vec<u8>, num_rows: usize) -> ColBatch {
        ColBatch {
            schema: Schema::new(vec![Field {
                name: "s".into(),
                ch_type: ChType::String,
            }]),
            columns: vec![Column::Utf8(Utf8Column::new(offsets, data))],
            num_rows,
        }
    }

    #[test]
    fn non_monotonic_string_offsets_are_rejected() {
        // Offsets that decrease would make `encode_string_data` slice `data[3..1]`,
        // which panics. Reject before writing.
        let batch = string_batch_from_parts(vec![0, 3, 1], b"abc".to_vec(), 2);
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn string_offset_past_data_is_rejected() {
        // A final offset past `data.len()` would slice out of bounds and panic.
        let batch = string_batch_from_parts(vec![0, 10], b"abc".to_vec(), 1);
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn negative_string_offset_is_rejected() {
        // A negative offset wraps to a huge `usize` in `encode_string_data`. The
        // monotonic check (from a zero start) catches it before that can happen.
        let batch = string_batch_from_parts(vec![0, -1], Vec::new(), 1);
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn trailing_string_data_is_rejected() {
        // Offsets that end before `data.len()` would silently drop the trailing
        // bytes from the wire. Reject rather than lose data (review item 4).
        let batch = string_batch_from_parts(vec![0, 2], b"abcd".to_vec(), 1);
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn leading_string_slack_is_rejected() {
        // A nonzero first offset would silently drop the leading data bytes and
        // violates the Arrow convention that offsets start at 0.
        let batch = string_batch_from_parts(vec![2, 4], b"abcd".to_vec(), 1);
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn type_string_buffer_mismatch_is_rejected() {
        // A supported numeric declared under a mismatched buffer variant must
        // error, never emit a wrong-width body under a truthful type string.
        // Here the type string would be "Int64" (8 bytes/row) but the buffer is a
        // 4-byte i32. Construct directly so `ColBatch::new`'s debug_assert on
        // column length (both are len 1) does not mask the type mismatch.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "n".into(),
                ch_type: ChType::Int64,
            }]),
            columns: vec![Column::Int32(PrimitiveColumn::new(vec![13]))],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn unrepresentable_type_string_is_rejected() {
        // A `DateTime64` precision above 9, invalid Decimal metadata, and a
        // `FixedString(0)` are constructible `ChType`s whose rendered type string
        // this crate's parser and the server reject or normalize differently.
        // Encoding must fail at the source (InconsistentBatch) rather than emit a
        // header that fails to decode downstream, or worse, a Decimal header whose
        // server-derived width disagrees with the body width.
        let dt64 = ColBatch {
            schema: Schema::new(vec![Field {
                name: "t".into(),
                ch_type: ChType::DateTime64 {
                    precision: 200,
                    timezone: None,
                },
            }]),
            columns: vec![Column::DateTime64(PrimitiveColumn::new(vec![0]))],
            num_rows: 1,
        };
        match encode_block(&dt64, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch for DateTime64(200), got {other:?}"),
        }

        let decimal_cases = [
            (
                "Decimal(9, 4) with 64 bits",
                ChType::Decimal {
                    precision: 9,
                    scale: 4,
                    bits: 64,
                },
                DecimalColumn::new(vec![0u8; 8], 8, 9, 4),
            ),
            (
                "Decimal(100, 4)",
                ChType::Decimal {
                    precision: 100,
                    scale: 4,
                    bits: 128,
                },
                DecimalColumn::new(vec![0u8; 16], 16, 100, 4),
            ),
            (
                "Decimal(9, 20)",
                ChType::Decimal {
                    precision: 9,
                    scale: 20,
                    bits: 32,
                },
                DecimalColumn::new(vec![0u8; 4], 4, 9, 20),
            ),
        ];
        for (label, ch_type, column) in decimal_cases {
            let batch = ColBatch {
                schema: Schema::new(vec![Field {
                    name: "dec".into(),
                    ch_type,
                }]),
                columns: vec![Column::Decimal(column)],
                num_rows: 1,
            };
            match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
                EncodeError::InconsistentBatch { .. } => {}
                other => panic!("expected InconsistentBatch for {label}, got {other:?}"),
            }
        }

        let fs0 = ColBatch {
            schema: Schema::new(vec![Field {
                name: "fs".into(),
                ch_type: ChType::FixedString(0),
            }]),
            columns: vec![Column::FixedBinary(FixedBinaryColumn::new(vec![], 0))],
            num_rows: 0,
        };
        match encode_block(&fs0, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch for FixedString(0), got {other:?}"),
        }
    }

    #[test]
    fn timezone_with_quote_is_rejected() {
        // A DateTime/DateTime64 timezone containing a single quote renders a
        // malformed header (`DateTime('UTC')')`) that the crate's lenient parser
        // round-trips but the server rejects. It must be caught at the source. Both
        // the bare and Nullable forms, and both temporal types, are covered.
        let cases = [
            ChType::DateTime {
                timezone: Some("UTC')".into()),
            },
            ChType::Nullable(Box::new(ChType::DateTime {
                timezone: Some("UTC')".into()),
            })),
            ChType::DateTime64 {
                precision: 3,
                timezone: Some("UTC')".into()),
            },
        ];
        for ch_type in cases {
            let is_nullable = matches!(ch_type, ChType::Nullable(_));
            let column = match ch_type.inner() {
                ChType::DateTime { .. } => {
                    let p = PrimitiveColumn::new(vec![0u32]);
                    Column::DateTime(p)
                }
                ChType::DateTime64 { .. } => Column::DateTime64(PrimitiveColumn::new(vec![0i64])),
                other => panic!("unexpected inner {other:?}"),
            };
            // Give a nullable case an all-valid bitmap so only the timezone check fires.
            let column = if is_nullable {
                match column {
                    Column::DateTime(mut p) => {
                        p.validity = Some(Bitmap::from_ch_null_map(&[0]));
                        Column::DateTime(p)
                    }
                    other => other,
                }
            } else {
                column
            };
            let batch = ColBatch {
                schema: Schema::new(vec![Field {
                    name: "t".into(),
                    ch_type,
                }]),
                columns: vec![column],
                num_rows: 1,
            };
            match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
                EncodeError::InconsistentBatch { .. } => {}
                other => panic!("expected InconsistentBatch, got {other:?}"),
            }
        }
    }

    #[test]
    fn non_nullable_with_nulls_is_rejected() {
        // A non-Nullable field with a validity bitmap that marks a row null writes
        // no null map, so the null would be silently dropped and its placeholder
        // value encoded as real data. Reject it.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "n".into(),
                ch_type: ChType::Int32,
            }]),
            columns: vec![Column::Int32(PrimitiveColumn::new_nullable(
                vec![13, 79],
                Bitmap::from_ch_null_map(&[0, 1]), // row 1 null under a non-Nullable field
            ))],
            num_rows: 2,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn non_nullable_all_valid_bitmap_is_accepted() {
        // A validity bitmap with no nulls under a non-Nullable field carries no null
        // information to lose, so it encodes fine (and round-trips: the decoder
        // produces a non-nullable column with no validity).
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "n".into(),
                ch_type: ChType::Int32,
            }]),
            columns: vec![Column::Int32(PrimitiveColumn::new_nullable(
                vec![13, 79],
                Bitmap::from_ch_null_map(&[0, 0]), // all valid
            ))],
            num_rows: 2,
        };
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
        match decoded.chunks[0].column(0) {
            Column::Int32(c) => assert_eq!(c.values, vec![13, 79]),
            other => panic!("expected Int32, got {other:?}"),
        }
    }

    #[test]
    fn encode_chunked_rejects_mismatched_chunk_schema() {
        // A chunk whose schema differs from the batch schema would encode a block
        // the server rejects mid-insert. Reject before writing anything.
        let field = |name: &str| Field {
            name: name.into(),
            ch_type: ChType::Int32,
        };
        let chunk = |name: &str, vals: Vec<i32>| {
            let n = vals.len();
            std::sync::Arc::new(ColBatch::new(
                Schema::new(vec![field(name)]),
                vec![Column::Int32(PrimitiveColumn::new(vals))],
                n,
            ))
        };
        let batch = ChunkedBatch {
            schema: Schema::new(vec![field("n")]),
            chunks: vec![chunk("n", vec![13, 14]), chunk("m", vec![15])],
        };
        match encode_chunked(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn inconsistent_batch_is_rejected() {
        // Build a batch whose column length disagrees with num_rows. Bypass
        // `ColBatch::new` (its debug_assert would fire) by constructing directly.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "n".into(),
                ch_type: ChType::Int32,
            }]),
            columns: vec![Column::Int32(PrimitiveColumn::new(vec![1, 2]))],
            num_rows: 3,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }
}
