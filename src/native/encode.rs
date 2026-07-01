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
//! Scope: this first slice encodes the fixed-width numeric types (`Int8`..`Int64`,
//! `UInt8`..`UInt64`, `Float32`, `Float64`). Every other column, and any
//! `Nullable` wrapper, returns [`EncodeError::UnsupportedType`] until its encoder
//! lands, the same one-type-at-a-time growth the decode path follows.

use crate::batch::{ChunkedBatch, ColBatch};
use crate::column::Column;
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
    /// A column this encoder cannot yet write (an unsupported physical type, or a
    /// `Nullable` wrapper before nullable-encode support lands). Grows narrower as
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

/// Encode one column's data body (no header) into `buf`.
///
/// Matches on the `(ch_type, column)` pair so the on-wire type string (written
/// from `field.ch_type`) and the body (written from the `Column` buffer) can
/// never disagree. A supported numeric declared under a mismatched buffer variant
/// (e.g. `Int64` paired with a `Column::Int32`) is an [`EncodeError::InconsistentBatch`],
/// not a wrong-width column on the wire. A `Nullable` wrapper or any not-yet-supported
/// type falls through to [`EncodeError::UnsupportedType`].
fn encode_column_data(
    buf: &mut Vec<u8>,
    field: &Field,
    column: &Column,
) -> Result<(), EncodeError> {
    match (&field.ch_type, column) {
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
        _ => return Err(column_error(field)),
    }
    Ok(())
}

/// Classify a column that did not match any supported `(ch_type, column)` pair.
///
/// If the declared type is one this encoder supports, the buffer must have been
/// the wrong variant, so the type string and body would disagree: that is an
/// [`EncodeError::InconsistentBatch`]. Otherwise the type itself is not yet
/// supported (a `Nullable` wrapper, or a type whose encoder has not landed), which
/// is an [`EncodeError::UnsupportedType`].
fn column_error(field: &Field) -> EncodeError {
    if is_supported_numeric(&field.ch_type) {
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

/// The fixed-width numeric types this slice can encode.
fn is_supported_numeric(ch_type: &ChType) -> bool {
    matches!(
        ch_type,
        ChType::Int8
            | ChType::Int16
            | ChType::Int32
            | ChType::Int64
            | ChType::UInt8
            | ChType::UInt16
            | ChType::UInt32
            | ChType::UInt64
            | ChType::Float32
            | ChType::Float64
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::PrimitiveColumn;
    use crate::native::decode::{decode_all_bytes, DecodeOptions, DBMS_TCP_PROTOCOL_VERSION};
    use crate::schema::Schema;

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

    /// Compare two batches column by column for the numeric types this slice
    /// encodes. Panics on any other variant so a wrong decode is loud.
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
                (other_a, other_b) => panic!("column {i}: unexpected {other_a:?} vs {other_b:?}"),
            }
        }
    }

    fn roundtrip_at(revision: u64) {
        let batch = numeric_batch();
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
        .unwrap_or_else(|e| panic!("decode at rev {revision} failed: {e}"));
        assert_eq!(decoded.num_chunks(), 1);
        assert_batches_eq(&batch, &decoded.chunks[0]);
    }

    #[test]
    fn roundtrip_numerics_rev0() {
        roundtrip_at(0);
    }

    #[test]
    fn roundtrip_numerics_tcp_revision() {
        roundtrip_at(DBMS_TCP_PROTOCOL_VERSION);
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
    fn nullable_column_is_unsupported() {
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "ni32".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Int32)),
            }]),
            vec![Column::Int32(PrimitiveColumn::new_nullable(
                vec![13],
                crate::bitmap::Bitmap::from_ch_null_map(&[0]),
            ))],
            1,
        );
        let err = encode_block(&batch, &EncodeOptions::default()).unwrap_err();
        assert!(matches!(err, EncodeError::UnsupportedType { .. }));
    }

    #[test]
    fn unsupported_type_reports_column_and_type() {
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "s".into(),
                ch_type: ChType::String,
            }]),
            vec![Column::Utf8(crate::column::Utf8Column::new(
                vec![0],
                vec![],
            ))],
            0,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { column, ch_type } => {
                assert_eq!(column, "s");
                assert_eq!(ch_type, ChType::String);
            }
            other => panic!("expected UnsupportedType, got {other:?}"),
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
