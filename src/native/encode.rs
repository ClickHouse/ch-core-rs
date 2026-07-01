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
//! `UInt8`..`UInt64`, `Float32`, `Float64`), `String`, and `FixedString(N)`, each
//! also inside a `Nullable(T)` wrapper (a per-row null map precedes the inner
//! values). Every other column type returns [`EncodeError::UnsupportedType`] until
//! its encoder lands, the same one-type-at-a-time growth the decode path follows.

use crate::batch::{ChunkedBatch, ColBatch};
use crate::column::{BoolColumn, Column, FixedBinaryColumn, Utf8Column};
use crate::schema::{ChType, Field};

use super::decode::DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION;
use super::varint::write_varint;

/// Protocol revision at which a data block's `BlockInfo` carries the
/// `out_of_order_buckets` field (server
/// `DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS_IN_AGGREGATION` in
/// `src/Core/ProtocolDefines.h`). At or above it, `BlockInfo::write` emits field
/// 3 with an empty vector for a plain data block, so the encoder does too, to
/// stay byte-identical to the server writer and to round-trip through the
/// decoder's `read_block_info`.
const DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS: u64 = 54480;

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
    let mut buf = Vec::new();
    for chunk in &batch.chunks {
        encode_block_into(&mut buf, chunk, options)?;
    }
    Ok(buf)
}

/// Append one framed Native block for `batch` to `buf`.
fn encode_block_into(
    buf: &mut Vec<u8>,
    batch: &ColBatch,
    options: &EncodeOptions,
) -> Result<(), EncodeError> {
    let num_cols = batch.schema.num_fields();
    if num_cols != batch.columns.len() {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "schema has {num_cols} fields but batch carries {} columns",
                batch.columns.len()
            ),
        });
    }
    let num_rows = batch.num_rows;
    // Validate every column before writing any bytes, so a rejected batch leaves
    // `buf` unchanged rather than half-written.
    for (field, column) in batch.schema.fields.iter().zip(&batch.columns) {
        if column.len() != num_rows {
            return Err(EncodeError::InconsistentBatch {
                detail: format!(
                    "column {:?} has {} rows but the block declares {num_rows}",
                    field.name,
                    column.len()
                ),
            });
        }
        // A `Nullable` column's validity bitmap is written verbatim as the per-row
        // null map, so it must cover exactly `num_rows`. A `None` bitmap means
        // all-valid and is fine. Validating here keeps `encode_null_map` infallible
        // and its `is_valid` reads in range.
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
        }
        // A `Bool` column is unpacked from its packed bitmap positionally, so the
        // bitmap must hold at least `len.div_ceil(8)` bytes. `BoolColumn`'s fields
        // are public and `ColBatch::new` only debug-asserts, so a release-mode
        // caller could hand over a `len` that overruns the bitmap; reject it as an
        // inconsistent batch rather than letting `encode_bool_data` panic.
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
    }

    // BlockInfo preamble, only at revision > 0 (server `NativeWriter::write` gates
    // `block.info.write` on `client_revision > 0`).
    if options.protocol_revision > 0 {
        write_block_info(buf, options.protocol_revision);
    }
    write_varint(buf, num_cols as u64);
    write_varint(buf, num_rows as u64);

    for (field, column) in batch.schema.fields.iter().zip(&batch.columns) {
        write_string(buf, field.name.as_bytes());
        // The type string is the canonical name `ChType::Display` renders, the
        // same string `parse_ch_type` accepts on decode.
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
/// all-valid, so an all-zero map is written. The bitmap length is validated
/// against `num_rows` in [`encode_block_into`] before any bytes are written, so
/// `is_valid` cannot go out of range here.
fn encode_null_map(buf: &mut Vec<u8>, column: &Column) {
    let num_rows = column.len();
    match column.validity() {
        None => buf.resize(buf.len() + num_rows, 0x00),
        Some(validity) => {
            for row in 0..num_rows {
                buf.push(u8::from(!validity.is_valid(row)));
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
        (ChType::String, Column::Utf8(c)) => encode_string_data(buf, c),
        (ChType::FixedString(n), Column::FixedBinary(c)) => {
            encode_fixed_string_data(buf, field, c, *n)?
        }
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
/// bit. [`encode_block_into`] validates `col.bitmap.len() >= col.len.div_ceil(8)`
/// before any bytes are written, so every index read here is in range.
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

/// Encode a `String` column body: one varint length prefix then the raw value
/// bytes, per row, the inverse of [`super::decode::decode_string_data`].
///
/// The values are walked straight out of the Arrow offsets+data buffer, one
/// sub-slice of `data` per row, so there is no per-row allocation and the value
/// bytes are copied exactly once. A zero-row column has `offsets == [0]`, so
/// `windows(2)` yields nothing and no body is written.
fn encode_string_data(buf: &mut Vec<u8>, col: &Utf8Column) {
    for pair in col.offsets.windows(2) {
        // Offsets are monotonic and bounded by `data.len()` for any column this
        // crate produces (the decoder builds them that way and they are not wire
        // input), so the slice is always in range.
        let value = &col.data[pair[0] as usize..pair[1] as usize];
        write_varint(buf, value.len() as u64);
        buf.extend_from_slice(value);
    }
}

/// Encode a `FixedString(N)` column body: the contiguous `N * num_rows` data
/// buffer written verbatim, the inverse of
/// [`super::decode::decode_fixed_binary_data`]. There is no per-row framing; the
/// width lives only in the type string.
///
/// The column's stored `width` must equal the declared `N`. A mismatch would put
/// a body with the wrong bytes-per-row under a truthful type string, so it is an
/// [`EncodeError::InconsistentBatch`] rather than a corrupt wire column.
fn encode_fixed_string_data(
    buf: &mut Vec<u8>,
    field: &Field,
    col: &FixedBinaryColumn,
    declared_width: usize,
) -> Result<(), EncodeError> {
    if col.width != declared_width {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is declared FixedString({declared_width}) but its buffer stores {}-byte rows",
                field.name, col.width
            ),
        });
    }
    buf.extend_from_slice(&col.data);
    Ok(())
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
            | ChType::String
            | ChType::FixedString(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitmap::Bitmap;
    use crate::column::PrimitiveColumn;
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

    /// Compare two batches column by column for the types this encoder covers
    /// (the numerics, `String`, `FixedString`). Panics on any other variant so a
    /// wrong decode is loud.
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
                (Column::Utf8(x), Column::Utf8(y)) => {
                    assert_eq!(x.offsets, y.offsets, "column {i} offsets differ");
                    assert_eq!(x.data, y.data, "column {i} data differ");
                }
                (Column::FixedBinary(x), Column::FixedBinary(y)) => {
                    assert_eq!(x.width, y.width, "column {i} width differ");
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
        ];
        let columns = vec![
            Column::Int32(PrimitiveColumn::new(vec![])),
            Column::Float64(PrimitiveColumn::new(vec![])),
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

    #[test]
    fn nullable_unsupported_inner_is_unsupported() {
        // `Nullable` is a supported wrapper now, but its inner type must also be
        // encodable. `Date` is decoded yet not encodable, so `Nullable(Date)` is
        // still rejected, and the reported type is the full `Nullable(Date)`.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "nd".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Date)),
            }]),
            vec![Column::Date(PrimitiveColumn::new_nullable(
                vec![19000u16],
                Bitmap::from_ch_null_map(&[0]),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { column, ch_type } => {
                assert_eq!(column, "nd");
                assert_eq!(ch_type, ChType::Nullable(Box::new(ChType::Date)));
            }
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_type_reports_column_and_type() {
        // `Date` is decoded but not yet encodable, so it reports UnsupportedType
        // with the column name and type.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "d".into(),
                ch_type: ChType::Date,
            }]),
            vec![Column::Date(PrimitiveColumn::new(vec![]))],
            0,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { column, ch_type } => {
                assert_eq!(column, "d");
                assert_eq!(ch_type, ChType::Date);
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
