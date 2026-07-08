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
//! `FixedString(N)`, `Enum8`/`Enum16`, `Decimal(P, S)`, `LowCardinality(T)`
//! for the allowed inner types this crate decodes, `Array(T)` over any
//! encodable element type (including nested arrays), `Tuple(T1, ...)`
//! (named or unnamed, including the zero-element `Tuple()`) over encodable
//! element types, and `Map(K, V)` for a legal key type and any encodable
//! key/value types. The plain types and `Tuple` also compose inside a
//! `Nullable(T)` wrapper (a per-row null map precedes the inner values). Every
//! other column type returns [`EncodeError::UnsupportedType`] until its
//! encoder lands, the same one-type-at-a-time growth the decode path follows.

use crate::batch::{ChunkedBatch, ColBatch};
use crate::column::{
    ArrayColumn, BoolColumn, Column, DecimalColumn, DictionaryColumn, FixedBinaryColumn, MapColumn,
    TupleColumn, Utf8Column,
};
use crate::schema::{ChType, Field};

use super::decode::{
    decimal_bits_from_precision, is_low_cardinality_inner, is_valid_map_key_type, parse_ch_type,
    DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION, DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS,
    LC_HAS_ADDITIONAL_KEYS_BIT, LC_NEED_UPDATE_DICTIONARY_BIT, LOW_CARDINALITY_KEY_VERSION,
    MAX_TYPE_DEPTH,
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
    /// A column this encoder cannot write: an unsupported physical type, a
    /// `Nullable(T)` whose inner type is not yet encodable, or a type the
    /// server itself cannot construct (an illegal `Map` key type; tuple
    /// element names that are mixed named/unnamed, empty, the reserved
    /// lowercase `null`, or duplicated).
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
    // Bound the declared type's nesting depth before anything walks it.
    // Encode input is caller-constructed and never passes through
    // `parse_ch_type`'s depth cap, and `column_variant_matches`, `is_encodable`,
    // `write_state_prefix`, and `ChType`'s `Display` all recurse one stack frame
    // per wrapper level, so a pathologically deep type (say 10^6 nested Arrays)
    // would overflow the stack before any other check fires. The walk below is
    // iterative, and the rejection deliberately avoids `EncodeError::
    // UnsupportedType`: that variant clones the `ChType` and renders it via
    // `Display`, both of which recurse to full depth, so the error itself would
    // overflow. Reusing the decode parser's `MAX_TYPE_DEPTH` keeps the two
    // directions accepting the same depths: a deeper type renders a header the
    // decode side would reject anyway.
    if type_depth(&field.ch_type) > MAX_TYPE_DEPTH {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} type nesting exceeds the maximum depth of {MAX_TYPE_DEPTH} wrapper/container levels",
                field.name
            ),
        });
    }

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
    // bitmap. A `LowCardinality(Nullable(T))` field instead writes NULL as
    // dictionary index 0, with validity on the index array. In either nullable
    // shape, a present bitmap must cover exactly `num_rows`. A non-nullable field
    // writes no nulls, so a null in its validity bitmap would be silently dropped
    // and the row's placeholder value encoded as real data; reject that.
    let nullable_at_this_level = matches!(field.ch_type, ChType::Nullable(_))
        || matches!(&field.ch_type, ChType::LowCardinality(inner) if matches!(inner.as_ref(), ChType::Nullable(_)));
    if nullable_at_this_level {
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

    if let (ChType::LowCardinality(inner), Column::Dictionary(c)) = (&field.ch_type, column) {
        validate_low_cardinality(field, inner, c, num_rows)?;
    }

    if let (ChType::Array(inner), Column::Array(c)) = (value_type, column) {
        validate_array(field, inner, c, num_rows)?;
    }

    if let (ChType::Tuple(elements), Column::Tuple(c)) = (value_type, column) {
        validate_tuple(field, elements, c, num_rows)?;
    }

    if let (ChType::Map(key, value), Column::Map(c)) = (value_type, column) {
        validate_map(field, key, value, c, num_rows)?;
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

/// Count the deepest wrapper/container nesting of `ch_type`, iteratively.
///
/// Used by [`validate_column`] to reject a pathologically deep
/// caller-constructed type before any of the encoder's recursive walks touch
/// it, so this must not recurse itself. `Tuple` is a multi-child container, so
/// the walk is an explicit worklist over `(type, depth)` pairs taking the
/// maximum leaf depth; memory is bounded by the fan-out (one pending entry per
/// unvisited sibling) and time is linear in the number of type nodes.
fn type_depth(ch_type: &ChType) -> usize {
    let mut max_depth = 0usize;
    let mut work: Vec<(&ChType, usize)> = vec![(ch_type, 0)];
    while let Some((current, depth)) = work.pop() {
        max_depth = max_depth.max(depth);
        match current {
            ChType::Nullable(inner) | ChType::LowCardinality(inner) | ChType::Array(inner) => {
                work.push((inner, depth + 1));
            }
            ChType::Tuple(elements) => {
                for (_, element_type) in elements {
                    work.push((element_type, depth + 1));
                }
            }
            ChType::Map(key, value) => {
                work.push((key, depth + 1));
                work.push((value, depth + 1));
            }
            _ => {}
        }
    }
    max_depth
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

/// Validate a `LowCardinality(T)` dictionary column before any bytes are written.
///
/// The Native payload carries a per-block dictionary plus row indexes. For
/// `LowCardinality(Nullable(T))`, ClickHouse reserves dictionary index 0 as the
/// NULL sentinel; the dictionary body itself is serialized as the non-nullable
/// removeNullable inner type with no null map. This encoder preserves the decoded
/// representation: nullable rows must have index 0, and valid rows must not.
fn validate_low_cardinality(
    field: &Field,
    inner: &ChType,
    col: &DictionaryColumn,
    num_rows: usize,
) -> Result<(), EncodeError> {
    let (nullable, dict_value_type) = match inner {
        ChType::Nullable(t) => (true, t.as_ref()),
        other => (false, other),
    };

    if !is_low_cardinality_inner(dict_value_type) || !is_encodable(dict_value_type) {
        return Err(EncodeError::UnsupportedType {
            column: field.name.clone(),
            ch_type: field.ch_type.clone(),
        });
    }

    let num_keys = col.values.len();
    // Since this crate stores indexes as i32, a larger dictionary could not be
    // fully addressed by the public buffer shape.
    if num_keys > i32::MAX as usize {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} LowCardinality dictionary has {num_keys} entries, exceeding the i32 index buffer contract",
                field.name
            ),
        });
    }

    if num_rows == 0 && num_keys > 0 {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} declares 0 LowCardinality rows but carries a non-empty {}-entry dictionary",
                field.name, num_keys
            ),
        });
    }

    let dict_field = Field {
        name: format!("{} dictionary", field.name),
        ch_type: dict_value_type.clone(),
    };
    validate_column(&dict_field, col.values.as_ref(), num_keys)?;

    for (row, &idx) in col.indices.iter().enumerate() {
        if idx < 0 {
            return Err(EncodeError::InconsistentBatch {
                detail: format!(
                    "column {:?} LowCardinality row {row} has negative dictionary index {idx}",
                    field.name
                ),
            });
        }
        let index = idx as usize;
        if index >= num_keys {
            return Err(EncodeError::InconsistentBatch {
                detail: format!(
                    "column {:?} LowCardinality row {row} index {idx} points outside its {num_keys}-entry dictionary",
                    field.name
                ),
            });
        }
        if nullable {
            let is_valid = match &col.validity {
                Some(bitmap) => bitmap.is_valid(row),
                None => true,
            };
            if is_valid && index == 0 {
                return Err(EncodeError::InconsistentBatch {
                    detail: format!(
                        "column {:?} LowCardinality(Nullable) row {row} is valid but uses dictionary index 0, which ClickHouse reserves for NULL",
                        field.name
                    ),
                });
            }
            if !is_valid && index != 0 {
                return Err(EncodeError::InconsistentBatch {
                    detail: format!(
                        "column {:?} LowCardinality(Nullable) row {row} is NULL but uses dictionary index {idx}; NULL rows must use index 0",
                        field.name
                    ),
                });
            }
        }
    }

    Ok(())
}

/// Validate an `Array(T)` column before any bytes are written.
///
/// [`encode_array_data`] writes `offsets[1..]` verbatim as raw `u64` and then
/// the flattened element column, so the Arrow LargeList invariants must hold
/// first: `num_rows + 1` offsets starting at 0, monotonically non-decreasing
/// (which, from the zero start, also proves every offset non-negative, so the
/// `i64` little-endian bytes written are exactly the wire `UInt64`'s), and a
/// final offset equal to the flattened element count (a smaller value would
/// silently drop trailing elements from the wire, a larger one would declare
/// elements the body does not carry, a misframed stream the server rejects with
/// `INCORRECT_DATA`). The element column is then validated recursively as its
/// own column of `offsets[num_rows]` rows, so every element-level guard (a
/// `Nullable` element's validity length, `LowCardinality` invariants, string
/// offsets, fixed-binary widths, a nested `Array`) applies to the flattened
/// buffer too.
fn validate_array(
    field: &Field,
    inner: &ChType,
    col: &ArrayColumn,
    num_rows: usize,
) -> Result<(), EncodeError> {
    let reject = |detail: String| Err(EncodeError::InconsistentBatch { detail });

    // Arrow list layout: a leading 0 plus one end-offset per row. Mostly implied
    // by the `column.len() == num_rows` check earlier in `validate_column`
    // (`ArrayColumn::len()` is `offsets.len().saturating_sub(1)`), but that
    // saturates, so an empty offsets vector still reports 0 rows; assert the
    // exact length so `offsets[0]` and `offsets[num_rows]` below are in range.
    if col.offsets.len() != num_rows + 1 {
        return reject(format!(
            "column {:?} declares {num_rows} rows so it needs {} Array offsets (a leading 0 plus one end-offset per row), but carries {}",
            field.name,
            num_rows + 1,
            col.offsets.len()
        ));
    }
    if col.offsets[0] != 0 {
        return reject(format!(
            "column {:?} has a nonzero first Array offset {}; Arrow list offsets start at 0",
            field.name, col.offsets[0]
        ));
    }
    // Monotonic non-decreasing from the zero start also proves every offset is
    // non-negative, so the `as u64` casts in `encode_array_data` cannot change
    // the value. Equal adjacent offsets (empty rows) are fine, matching the
    // server's own non-decreasing check in `deserializeOffsetsBinaryBulk`.
    for pair in col.offsets.windows(2) {
        if pair[1] < pair[0] {
            return reject(format!(
                "column {:?} has non-monotonic Array offsets ({} then {})",
                field.name, pair[0], pair[1]
            ));
        }
    }
    let total_elements = col.offsets[num_rows];
    let element_rows = col.values.len();
    if i64::try_from(element_rows) != Ok(total_elements) {
        return reject(format!(
            "column {:?} Array offsets end at {total_elements} but the flattened element column holds {element_rows} rows",
            field.name
        ));
    }

    let element_field = Field {
        name: format!("{} element", field.name),
        ch_type: inner.clone(),
    };
    validate_column(&element_field, col.values.as_ref(), element_rows)
}

/// Validate a `Tuple(T1, ...)` column before any bytes are written.
///
/// [`encode_tuple_data`] writes each element column's full run sequentially
/// through the shared [`encode_column_values`] path, so every element column
/// must be a valid column of exactly `num_rows` rows: each is validated
/// recursively as its own column (the row-count check inside the recursive
/// [`validate_column`] is what enforces the server's equal-element-lengths
/// invariant; a ragged element would put a misframed stream on the wire, which
/// the server rejects with `INCORRECT_DATA`). A field-count mismatch between
/// the declared type and the buffer is caught here explicitly; the earlier
/// [`column_variant_matches`] gate also requires equal counts, but the check is
/// restated so the zip below provably covers every declared element even if
/// check ordering changes. The zero-element `Tuple()` needs no per-element
/// validation; its row count is `TupleColumn::len`, already checked against
/// `num_rows` by the caller.
///
/// Element names must also be a set the server can construct: the decode
/// parser deliberately round-trips any received name shape (a server-authored
/// header is preserved as written), so the type-string round-trip check in
/// [`validate_column`] cannot catch a caller-constructed illegal name; it is
/// rejected here instead (see the name checks below).
fn validate_tuple(
    field: &Field,
    elements: &[(Option<String>, ChType)],
    col: &TupleColumn,
    num_rows: usize,
) -> Result<(), EncodeError> {
    // Mirror the server's tuple-name legality exactly (confirmed at
    // v26.6.1.1193-stable, `src/DataTypes/DataTypeTuple.cpp`): the type factory
    // rejects mixed named/unnamed elements ("Names are specified not for all
    // elements of Tuple type"), and `checkTupleNames` rejects an empty name,
    // the exact-lowercase reserved name "null" (it would collide with the
    // Nullable null-map subcolumn name; "NULL"/"Null" are fine and render
    // backtick-quoted), and duplicate names. A type violating any of these
    // cannot exist on the server, so the rejection is `UnsupportedType`, the
    // same classification as an illegal Map key.
    let named = elements.iter().filter(|(name, _)| name.is_some()).count();
    let mixed_names = named != 0 && named != elements.len();
    let illegal_name = elements
        .iter()
        .any(|(name, _)| matches!(name.as_deref(), Some("") | Some("null")));
    // O(n^2) over the element names; tuples are small and this runs once per
    // column validation, never per row.
    let duplicate_name = elements.iter().enumerate().any(|(i, (name, _))| {
        name.is_some() && elements[..i].iter().any(|(other, _)| other == name)
    });
    if mixed_names || illegal_name || duplicate_name {
        return Err(EncodeError::UnsupportedType {
            column: field.name.clone(),
            ch_type: field.ch_type.clone(),
        });
    }

    if elements.len() != col.fields.len() {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} declares {} Tuple elements but the buffer carries {} field columns",
                field.name,
                elements.len(),
                col.fields.len()
            ),
        });
    }
    for (i, ((name, element_type), element_col)) in elements.iter().zip(&col.fields).enumerate() {
        let element_field = Field {
            name: match name {
                Some(n) => format!("{} element {n:?}", field.name),
                None => format!("{} element {}", field.name, i + 1),
            },
            ch_type: element_type.clone(),
        };
        validate_column(&element_field, element_col, num_rows)?;
    }
    Ok(())
}

/// Validate a `Map(K, V)` column before any bytes are written.
///
/// [`encode_map_data`] writes `offsets[1..]` verbatim as raw `u64` and then the
/// flattened key and value runs, so the same Arrow offset invariants as
/// [`validate_array`] must hold: `num_rows + 1` offsets starting at 0,
/// monotonically non-decreasing, final offset equal to the flattened entries
/// length. The key type must satisfy the server's `DataTypeMap::isValidKeyType`
/// constraint (no `Nullable` or `LowCardinality(Nullable(...))` key), reported
/// as `UnsupportedType` since the type itself cannot exist on the server. The
/// entries buffer must be a two-field `Tuple` column (keys then values) whose
/// fields are validated recursively as their own columns of
/// `offsets[num_rows]` rows, so every key/value-level guard applies to the
/// flattened buffers.
fn validate_map(
    field: &Field,
    key: &ChType,
    value: &ChType,
    col: &MapColumn,
    num_rows: usize,
) -> Result<(), EncodeError> {
    let reject = |detail: String| Err(EncodeError::InconsistentBatch { detail });

    if !is_valid_map_key_type(key) {
        return Err(EncodeError::UnsupportedType {
            column: field.name.clone(),
            ch_type: field.ch_type.clone(),
        });
    }

    // The same Arrow list offset invariants as `validate_array`; `MapColumn`'s
    // offsets are physically the Array offsets of the wire's
    // Array(Tuple(keys, values)).
    if col.offsets.len() != num_rows + 1 {
        return reject(format!(
            "column {:?} declares {num_rows} rows so it needs {} Map offsets (a leading 0 plus one end-offset per row), but carries {}",
            field.name,
            num_rows + 1,
            col.offsets.len()
        ));
    }
    if col.offsets[0] != 0 {
        return reject(format!(
            "column {:?} has a nonzero first Map offset {}; Arrow list offsets start at 0",
            field.name, col.offsets[0]
        ));
    }
    for pair in col.offsets.windows(2) {
        if pair[1] < pair[0] {
            return reject(format!(
                "column {:?} has non-monotonic Map offsets ({} then {})",
                field.name, pair[0], pair[1]
            ));
        }
    }
    let total_entries = col.offsets[num_rows];
    let entry_rows = col.entries.len();
    if i64::try_from(entry_rows) != Ok(total_entries) {
        return reject(format!(
            "column {:?} Map offsets end at {total_entries} but the flattened entries column holds {entry_rows} rows",
            field.name
        ));
    }

    // The entries buffer must be the two-field keys/values tuple; each field is
    // then validated recursively as its own column of `entry_rows` rows.
    let entries = match col.entries.as_ref() {
        Column::Tuple(t) if t.fields.len() == 2 => t,
        Column::Tuple(t) => {
            return reject(format!(
                "column {:?} Map entries must carry exactly 2 field columns (keys, values), but carry {}",
                field.name,
                t.fields.len()
            ));
        }
        // Defensive: `column_variant_matches` already required a two-field
        // Tuple entries buffer, so a non-Tuple buffer cannot reach here.
        _ => return Err(column_error(field, &field.ch_type)),
    };
    // The entries tuple never carries validity: the wire has no null map at
    // the entries level (a map is never nullable there), so `encode_map_data`
    // writes none, and a caller-attached bitmap would be silently dropped with
    // its null entries encoded as real entries. Reject it before any bytes.
    // (The recursive `validate_column` calls below cover the key and value
    // columns but never the entries tuple itself, so this must be explicit.)
    if entries.validity.is_some() {
        return reject(format!(
            "column {:?} Map entries tuple carries a validity bitmap; map entries are never nullable and the bitmap would be silently dropped",
            field.name
        ));
    }
    let key_field = Field {
        name: format!("{} key", field.name),
        ch_type: key.clone(),
    };
    validate_column(&key_field, &entries.fields[0], entry_rows)?;
    let value_field = Field {
        name: format!("{} value", field.name),
        ch_type: value.clone(),
    };
    validate_column(&value_field, &entries.fields[1], entry_rows)
}

/// Whether `value_type` (the unwrapped inner value type) and `column` form a
/// supported, matching pair this encoder can write.
///
/// This must list exactly the matching arms of [`encode_column_body`], plus
/// `LowCardinality`'s [`encode_low_cardinality_data`] path and `Array`'s
/// [`encode_array_data`] path; keep it in sync the same way [`is_encodable`] is.
/// [`validate_column`] uses it to reject a wrong-buffer or not-yet-encodable
/// column before any bytes are written, which is what lets [`write_block_into`]
/// treat the body match as structurally infallible.
fn column_variant_matches(value_type: &ChType, column: &Column) -> bool {
    // `Array(T)` matches only if the flattened element column matches the
    // element value type in turn, recursing the same way `decode_array` decodes
    // through `decode_values`. A `Nullable` element unwraps to its inner here
    // (the element column carries the inner variant plus validity), exactly like
    // the top-level unwrap in `validate_column`.
    if let (ChType::Array(inner), Column::Array(c)) = (value_type, column) {
        return column_variant_matches(inner.inner(), c.values.as_ref());
    }
    // `Tuple(T1, ...)` matches only if the buffer carries exactly one field
    // column per declared element and each matches its element value type in
    // turn (a `Nullable` element unwraps to its inner, like the Array arm
    // above; a `LowCardinality` or nested container element recurses through
    // this function's own dispatch). The zero-element `Tuple()` matches a
    // zero-field `TupleColumn`.
    if let (ChType::Tuple(elements), Column::Tuple(c)) = (value_type, column) {
        return elements.len() == c.fields.len()
            && elements
                .iter()
                .zip(&c.fields)
                .all(|((_, t), col)| column_variant_matches(t.inner(), col));
    }
    // `Map(K, V)` matches only if the entries buffer is a two-field Tuple
    // column whose fields match the key and value types in turn (a `Nullable`
    // value unwraps to its inner like everywhere else; a legal key is never
    // `Nullable`, so its `inner()` is a no-op).
    if let (ChType::Map(key, value), Column::Map(c)) = (value_type, column) {
        return match c.entries.as_ref() {
            Column::Tuple(t) => {
                t.fields.len() == 2
                    && column_variant_matches(key.inner(), &t.fields[0])
                    && column_variant_matches(value.inner(), &t.fields[1])
            }
            _ => false,
        };
    }
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
            | (ChType::LowCardinality(_), Column::Dictionary(_))
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
        // A zero-row block carries only the column headers: `NativeWriter::write`
        // gates `writeData` (the state prefix included, so not even a
        // LowCardinality key version) on `rows > 0`, and `NativeReader::read`
        // skips symmetrically (confirmed at v26.6.1.1193-stable).
        if batch.num_rows > 0 {
            encode_column_data(buf, field, column)?;
        }
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

/// Encode one column into `buf` (no header): the per-column bulk-state prefix,
/// then the value payload, the inverse of [`super::decode::decode_column`].
/// Called only for blocks with rows (see [`write_block_into`]), matching the
/// server's `rows > 0` gate around `writeData`.
fn encode_column_data(
    buf: &mut Vec<u8>,
    field: &Field,
    column: &Column,
) -> Result<(), EncodeError> {
    write_state_prefix(buf, &field.ch_type);
    encode_column_values(buf, field, &field.ch_type, column)
}

/// Write the per-column bulk-state prefix, the inverse of
/// [`super::decode::read_state_prefix`] and the encode side of the server's
/// `serializeBinaryBulkStatePrefix` recursion.
///
/// `SerializationArray::serializeBinaryBulkStatePrefix` writes nothing of its
/// own and recurses into the element type (confirmed at v26.6.1.1193-stable,
/// `src/DataTypes/Serializations/SerializationArray.cpp`), so a leaf
/// `LowCardinality`'s 8-byte key version is hoisted to the very front of the
/// whole column's data, before any `Array` offsets, across every nesting level.
/// `SerializationLowCardinality::serializeBinaryBulkStatePrefix` writes that one
/// UInt64 LE key version (`SharedDictionariesWithAdditionalKeys` = 1); every
/// other supported type writes a zero-byte prefix.
fn write_state_prefix(buf: &mut Vec<u8>, ch_type: &ChType) {
    match ch_type {
        ChType::LowCardinality(_) => {
            buf.extend_from_slice(&LOW_CARDINALITY_KEY_VERSION.to_le_bytes());
        }
        ChType::Array(inner) => write_state_prefix(buf, inner),
        // `SerializationTuple::serializeBinaryBulkStatePrefix` writes nothing of
        // its own and delegates to every element in declaration order (confirmed
        // at v26.6.1.1193-stable), so a LowCardinality element's key version is
        // hoisted to the front of the whole Tuple column, before any element
        // bodies, in element order.
        ChType::Tuple(elements) => {
            for (_, element_type) in elements {
                write_state_prefix(buf, element_type);
            }
        }
        // `SerializationMap` delegates through its nested Array(Tuple(...)),
        // so the chain is Map -> Array (nothing) -> Tuple -> key's prefix then
        // value's prefix (confirmed at v26.6.1.1193-stable). A
        // Map(LowCardinality(String), V) therefore hoists the LC key version
        // to the very front of the whole column, before the offsets.
        ChType::Map(key, value) => {
            write_state_prefix(buf, key);
            write_state_prefix(buf, value);
        }
        // `SerializationNullable::serializeBinaryBulkStatePrefix` delegates to
        // the nested type (confirmed at v26.6.1.1193-stable); only a
        // `Nullable(Tuple(...))` can nest a prefix-bearing type today.
        ChType::Nullable(inner) => write_state_prefix(buf, inner),
        _ => {}
    }
}

/// Encode one column's value payload once its state prefix has been written,
/// the inverse of [`super::decode::decode_values`].
///
/// Split from [`encode_column_data`] so [`encode_array_data`] can write its
/// flattened element column WITHOUT re-emitting a state prefix: the server
/// hoists the element prefix to the front of the whole `Array` column and never
/// repeats it per element run. A `Nullable(T)` writes the per-row null map
/// first, then the inner type's body from the same physical column buffer,
/// which carries the inner variant plus the validity bitmap. A plain type goes
/// straight to its body.
fn encode_column_values(
    buf: &mut Vec<u8>,
    field: &Field,
    ch_type: &ChType,
    column: &Column,
) -> Result<(), EncodeError> {
    if let ChType::LowCardinality(inner) = ch_type {
        if let Column::Dictionary(c) = column {
            // A zero-length run writes no LowCardinality body at all: no index
            // word, no dictionary, no row count, no indexes.
            // `SerializationLowCardinality::serializeBinaryBulkWithMultipleStreams`
            // early-returns when limit == 0 (confirmed at v26.6.1.1193-stable).
            // Reachable only nested inside an Array whose arrays are all empty;
            // a zero-row block skips the column data, prefix included, in
            // `write_block_into`.
            if c.is_empty() {
                return Ok(());
            }
            return encode_low_cardinality_data(buf, field, inner, c);
        }
        return Err(column_error(field, ch_type));
    }
    if let ChType::Array(inner) = ch_type {
        if let Column::Array(c) = column {
            return encode_array_data(buf, field, inner, c);
        }
        return Err(column_error(field, ch_type));
    }
    if let ChType::Map(key, value) = ch_type {
        if let Column::Map(c) = column {
            return encode_map_data(buf, field, ch_type, key, value, c);
        }
        return Err(column_error(field, ch_type));
    }
    let value_type = if let ChType::Nullable(inner) = ch_type {
        encode_null_map(buf, column);
        inner.as_ref()
    } else {
        ch_type
    };
    // Tuple after the Nullable unwrap, mirroring the decode side: a
    // `Nullable(Tuple(...))` writes its per-row null map above, then the tuple
    // body (element bodies still carry a placeholder value for null rows).
    if let ChType::Tuple(elements) = value_type {
        if let Column::Tuple(c) = column {
            return encode_tuple_data(buf, field, elements, c);
        }
        return Err(column_error(field, value_type));
    }
    encode_column_body(buf, field, value_type, column)
}

/// Encode one `Map(K, V)` column body: the Array offsets run, then the
/// flattened key run and the flattened value run, the inverse of
/// [`super::decode::decode_map`].
///
/// On the Native wire a Map is always the plain `Array(Tuple(keys, values))`
/// layout (server `SerializationMap`, confirmed at v26.6.1.1193-stable; the
/// bucketed `WITH_BUCKETS` on-disk mode never reaches the Native wire, see the
/// decode-side doc). The offsets are `offsets[1..]` written as raw
/// little-endian `u64` exactly like [`encode_array_data`] (validation proved
/// them non-negative), and the two runs go through the shared
/// [`encode_column_values`] path with no prefix re-emission
/// ([`write_state_prefix`] hoisted the key and value prefixes to the front of
/// the whole column). A zero-length entries run (rows > 0 but every map empty)
/// writes nothing for the runs; in particular a `LowCardinality` key or value
/// takes its `limit == 0` early-return gate.
///
/// `map_type` is the full declared `Map` type, used only for the defensive
/// wrong-buffer error (validation already proved the entries shape).
fn encode_map_data(
    buf: &mut Vec<u8>,
    field: &Field,
    map_type: &ChType,
    key: &ChType,
    value: &ChType,
    col: &MapColumn,
) -> Result<(), EncodeError> {
    // `get(1..)` rather than `[1..]`: validation guarantees the leading 0
    // exists, but stay panic-free if a caller reaches this without validating.
    if let Some(end_offsets) = col.offsets.get(1..) {
        encode_primitive!(buf, end_offsets, i64);
    }
    match col.entries.as_ref() {
        Column::Tuple(entries) if entries.fields.len() == 2 => {
            encode_column_values(buf, field, key, &entries.fields[0])?;
            encode_column_values(buf, field, value, &entries.fields[1])
        }
        // Defensive: `validate_map` rejected any other entries shape before
        // the write phase.
        _ => Err(column_error(field, map_type)),
    }
}

/// Encode one `Tuple(T1, ...)` column body: each element column's FULL run, in
/// declaration order, through the shared [`encode_column_values`] path (no
/// state prefix re-emission; [`write_state_prefix`] already hoisted every
/// element's prefix to the front of the whole column), the inverse of
/// [`super::decode::decode_tuple`].
///
/// Wire layout per block (server `SerializationTuple`, confirmed at
/// v26.6.1.1193-stable in `src/DataTypes/Serializations/SerializationTuple.cpp`):
/// the element bodies one after another, column-of-columns, with no
/// interleaving, no offsets, and no Tuple-level length framing. A `Nullable`,
/// `LowCardinality`, `Array`, or nested `Tuple` element composes through the
/// shared path, including the `limit == 0` early return for a zero-length
/// `LowCardinality` element run.
///
/// The zero-element `Tuple()` writes exactly one literal ASCII '0' byte (0x30)
/// per row and nothing else (confirmed at v26.6.1.1193-stable; the reader side
/// ignores the byte values via `tryIgnore`); a zero-length run (`col.len == 0`,
/// reachable nested inside an all-empty `Array` run) writes nothing.
fn encode_tuple_data(
    buf: &mut Vec<u8>,
    field: &Field,
    elements: &[(Option<String>, ChType)],
    col: &TupleColumn,
) -> Result<(), EncodeError> {
    if elements.is_empty() {
        buf.resize(buf.len() + col.len, b'0');
        return Ok(());
    }
    for ((_, element_type), element_col) in elements.iter().zip(&col.fields) {
        encode_column_values(buf, field, element_type, element_col)?;
    }
    Ok(())
}

/// Encode one `LowCardinality(T)` column body, after its 8-byte key-version
/// state prefix ([`write_state_prefix`] writes that separately so it hoists
/// correctly through an `Array` wrapper).
///
/// Confirmed at `v26.6.1.1193-stable`: after the prefix, Native writes an index
/// word with `HasAdditionalKeysBit` and `NeedUpdateDictionary` set, then the
/// per-block dictionary as the removeNullable inner type's plain body, then the
/// row count and fixed-width raw indexes. A zero-row block skips the column
/// data entirely in [`write_block_into`], matching `NativeWriter::write`'s
/// `rows > 0` gate, and a zero-length nested run is skipped by
/// [`encode_column_values`], matching the server's `limit == 0` early return,
/// so this always writes at least one index.
fn encode_low_cardinality_data(
    buf: &mut Vec<u8>,
    field: &Field,
    inner: &ChType,
    col: &DictionaryColumn,
) -> Result<(), EncodeError> {
    let dict_value_type = match inner {
        ChType::Nullable(t) => t.as_ref(),
        other => other,
    };
    let (index_width, width_tag) = low_cardinality_index_width(col.values.len());

    let index_word = width_tag | LC_HAS_ADDITIONAL_KEYS_BIT | LC_NEED_UPDATE_DICTIONARY_BIT;
    buf.extend_from_slice(&index_word.to_le_bytes());
    buf.extend_from_slice(&(col.values.len() as u64).to_le_bytes());
    encode_column_body(buf, field, dict_value_type, col.values.as_ref())?;
    buf.extend_from_slice(&(col.indices.len() as u64).to_le_bytes());

    match index_width {
        1 => {
            buf.reserve(col.indices.len());
            for &idx in &col.indices {
                buf.push(idx as u8);
            }
        }
        2 => {
            buf.reserve(col.indices.len() * 2);
            for &idx in &col.indices {
                buf.extend_from_slice(&(idx as u16).to_le_bytes());
            }
        }
        4 => {
            buf.reserve(col.indices.len() * 4);
            for &idx in &col.indices {
                buf.extend_from_slice(&(idx as u32).to_le_bytes());
            }
        }
        8 => {
            buf.reserve(col.indices.len() * 8);
            for &idx in &col.indices {
                buf.extend_from_slice(&(idx as u64).to_le_bytes());
            }
        }
        _ => unreachable!("LowCardinality index width is selected from 1/2/4/8"),
    }
    Ok(())
}

/// Pick the Native index width and low-bit type tag from the dictionary size.
///
/// The width tag is self-describing and ClickHouse accepts any width whose index
/// values are in range for the emitted dictionary. This encoder uses UInt8
/// through 255 dictionary entries, UInt16 through 65535, UInt32 through
/// `u32::MAX`, and UInt64 above that. The validation layer rejects dictionary
/// sizes beyond the i32 public index-buffer contract before this is called.
fn low_cardinality_index_width(num_keys: usize) -> (usize, u64) {
    if num_keys <= u8::MAX as usize {
        (1, 0)
    } else if num_keys <= u16::MAX as usize {
        (2, 1)
    } else if num_keys <= u32::MAX as usize {
        (4, 2)
    } else {
        (8, 3)
    }
}

/// Encode one `Array(T)` column body: the offsets run, then the flattened
/// element column, the inverse of [`super::decode::decode_array`].
///
/// Wire layout per block (server `SerializationArray`, confirmed at
/// v26.6.1.1193-stable in `src/DataTypes/Serializations/SerializationArray.cpp`;
/// the element type's state prefix was already hoisted to the front of the
/// whole column by [`write_state_prefix`], so nothing here re-emits it):
///
/// ```text
/// [num_rows * 8]  offsets   // raw LE u64, cumulative ABSOLUTE end-offsets, no
///                           // leading zero and no count; equal adjacent values
///                           // are empty rows
/// [element body]            // the flattened element column of length
///                           // `offsets[num_rows]`, the element type's normal
///                           // bulk body WITHOUT its state prefix (a nested
///                           // Array recurses here; a zero-length
///                           // LowCardinality run writes nothing at all)
/// ```
///
/// `ArrayColumn::offsets` is the Arrow LargeList layout (a leading 0 plus one
/// i64 end-offset per row), so the wire run is exactly `offsets[1..]`.
/// [`validate_array`] proved the offsets start at 0 and are monotonically
/// non-decreasing, so every offset is non-negative and each i64's little-endian
/// bytes are exactly the wire UInt64's; on little-endian targets the whole run
/// is one `extend_from_slice` via `encode_primitive!` (per-element
/// `to_le_bytes` on big-endian hosts), with no per-row allocation. The element
/// body then goes through the shared [`encode_column_values`] path, so a
/// `Nullable`, `LowCardinality`, or nested `Array` element all compose.
fn encode_array_data(
    buf: &mut Vec<u8>,
    field: &Field,
    inner: &ChType,
    col: &ArrayColumn,
) -> Result<(), EncodeError> {
    // `get(1..)` rather than `[1..]`: validation guarantees the leading 0
    // exists, but stay panic-free if a caller reaches this without validating
    // (the same defensive posture as `encode_column_body`'s fall-through arm).
    if let Some(end_offsets) = col.offsets.get(1..) {
        encode_primitive!(buf, end_offsets, i64);
    }
    encode_column_values(buf, field, inner, col.values.as_ref())
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
/// unwrapped value types plus the `LowCardinality` and `Array` wrappers, whose
/// framing is handled by [`encode_low_cardinality_data`] and
/// [`encode_array_data`]. The `Nullable` wrapper composes with any non-wrapper
/// type here via [`encode_null_map`]. Extend it as each new type's arm lands in
/// [`encode_column_body`] or [`encode_column_values`].
fn is_encodable(ch_type: &ChType) -> bool {
    match ch_type {
        ChType::LowCardinality(inner) => {
            let dict_value_type = match inner.as_ref() {
                ChType::Nullable(t) => t.as_ref(),
                other => other,
            };
            is_low_cardinality_inner(dict_value_type) && is_encodable(dict_value_type)
        }
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
        | ChType::Decimal { .. } => true,
        // `Array(T)` only frames offsets around its element body
        // (`encode_array_data`), so it is encodable exactly when its element
        // value type is. A `Nullable` element unwraps like the top level does;
        // `parse_ch_type` never nests `Array` directly inside `Nullable`, so
        // `inner()` cannot hide a second `Array` wrapper.
        ChType::Array(inner) => is_encodable(inner.inner()),
        // `Tuple(T1, ...)` writes its element bodies through the shared path
        // (`encode_tuple_data`), so it is encodable exactly when every element
        // value type is (a `Nullable` element unwraps like the Array arm). The
        // zero-element `Tuple()` is encodable: its body is the one placeholder
        // byte per row.
        ChType::Tuple(elements) => elements.iter().all(|(_, t)| is_encodable(t.inner())),
        // `Map(K, V)` only frames Array offsets around its flattened key and
        // value runs (`encode_map_data`), so it is encodable exactly when the
        // key type is legal (the server's `isValidKeyType`: never `Nullable`
        // or `LowCardinality(Nullable(...))`) and both types are encodable. A
        // legal key is never `Nullable`, so it is checked directly; the value
        // unwraps a `Nullable` like everywhere else.
        ChType::Map(key, value) => {
            is_valid_map_key_type(key) && is_encodable(key) && is_encodable(value.inner())
        }
        ChType::Nullable(_) => false,
    }
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

    /// A plain `LowCardinality(String)` and `LowCardinality(UInt32)` over four
    /// rows. The dictionary includes the server's reserved default slot 0 and
    /// rows reference real values in slots 1.., matching server-produced Native
    /// blocks while still exercising the dictionary/index writer.
    fn low_cardinality_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "lc".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::String)),
            },
            Field {
                name: "lc_u32".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::UInt32)),
            },
        ];
        let columns = vec![
            Column::Dictionary(DictionaryColumn::new(
                vec![1, 2, 1, 2],
                Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
            )),
            Column::Dictionary(DictionaryColumn::new(
                vec![1, 2, 1, 2],
                Column::UInt32(PrimitiveColumn::new(vec![0, 13, 79])),
            )),
        ];
        ColBatch::new(Schema::new(fields), columns, 4)
    }

    /// `LowCardinality(Nullable(String))` and
    /// `LowCardinality(Nullable(UInt32))` over four rows with the valid, null,
    /// valid, null pattern. Index 0 is the ClickHouse NULL sentinel and the
    /// dictionary body is the bare non-nullable inner type.
    fn low_cardinality_nullable_batch() -> ColBatch {
        let validity = || Bitmap::from_ch_null_map(&[0, 1, 0, 1]);
        let fields = vec![
            Field {
                name: "lcn".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(
                    ChType::String,
                )))),
            },
            Field {
                name: "lcn_u32".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(
                    ChType::UInt32,
                )))),
            },
        ];
        let columns = vec![
            Column::Dictionary(DictionaryColumn::new_nullable(
                vec![1, 0, 2, 0],
                Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
                validity(),
            )),
            Column::Dictionary(DictionaryColumn::new_nullable(
                vec![1, 0, 2, 0],
                Column::UInt32(PrimitiveColumn::new(vec![0, 13, 79])),
                validity(),
            )),
        ];
        ColBatch::new(Schema::new(fields), columns, 4)
    }

    /// An `Array(Int32)` column over four rows: `[13, 79]`, `[]` (an empty row,
    /// so an adjacent-equal offset pair), `[21]`, `[34, 55, 89]`.
    fn array_int32_batch() -> ColBatch {
        let fields = vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::Int32)),
        }];
        let columns = vec![Column::Array(ArrayColumn::new(
            vec![0, 2, 2, 3, 6],
            Column::Int32(PrimitiveColumn::new(vec![13, 79, 21, 34, 55, 89])),
        ))];
        ColBatch::new(Schema::new(fields), columns, 4)
    }

    /// An `Array(Nullable(String))` column over three rows: `["user_1", NULL]`,
    /// `[]`, `["user_2"]`. The element null map covers the flattened element
    /// run, so its validity lives on the flattened Utf8 column, not the array.
    fn array_nullable_string_batch() -> ColBatch {
        let mut elements = utf8_column(&[b"user_1", b"", b"user_2"]);
        elements.validity = Some(Bitmap::from_ch_null_map(&[0, 1, 0]));
        let fields = vec![Field {
            name: "ans".into(),
            ch_type: ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::String)))),
        }];
        let columns = vec![Column::Array(ArrayColumn::new(
            vec![0, 2, 2, 3],
            Column::Utf8(elements),
        ))];
        ColBatch::new(Schema::new(fields), columns, 3)
    }

    /// An `Array(LowCardinality(String))` column over three rows:
    /// `[user_1, user_2]`, `[]`, `[user_1]`. The element column is one
    /// dictionary over the flattened run, and the LC key version is hoisted to
    /// the front of the whole column, before the offsets.
    fn array_low_cardinality_batch() -> ColBatch {
        let fields = vec![Field {
            name: "alc".into(),
            ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
        }];
        let columns = vec![Column::Array(ArrayColumn::new(
            vec![0, 2, 2, 3],
            Column::Dictionary(DictionaryColumn::new(
                vec![1, 2, 1],
                Column::Utf8(utf8_column(&[b"", b"user_1", b"user_2"])),
            )),
        ))];
        ColBatch::new(Schema::new(fields), columns, 3)
    }

    /// An `Array(LowCardinality(String))` column with rows > 0 but EVERY array
    /// empty, so the flattened element run has zero length and the LC element
    /// body must be entirely absent: the wire is `[key version][zero offsets]`
    /// and nothing else (the server's `limit == 0` early return).
    fn array_low_cardinality_all_empty_batch() -> ColBatch {
        let fields = vec![Field {
            name: "alc".into(),
            ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
        }];
        let columns = vec![Column::Array(ArrayColumn::new(
            vec![0, 0, 0],
            Column::Dictionary(DictionaryColumn::new(
                vec![],
                Column::Utf8(utf8_column(&[])),
            )),
        ))];
        ColBatch::new(Schema::new(fields), columns, 2)
    }

    /// An `Array(Array(Int32))` column over three rows:
    /// `[[13, 79], [21]]`, `[]`, `[[34, 55, 89]]`. The outer offsets count inner
    /// arrays, the inner offsets count leaf ints, and only one offsets run per
    /// level is written (no prefixes anywhere for an Int32 leaf).
    fn array_of_array_batch() -> ColBatch {
        let fields = vec![Field {
            name: "aa".into(),
            ch_type: ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
        }];
        let columns = vec![Column::Array(ArrayColumn::new(
            vec![0, 2, 2, 3],
            Column::Array(ArrayColumn::new(
                vec![0, 2, 3, 6],
                Column::Int32(PrimitiveColumn::new(vec![13, 79, 21, 34, 55, 89])),
            )),
        ))];
        ColBatch::new(Schema::new(fields), columns, 3)
    }

    /// Tuple(Int32, String) plus a named Tuple(a Int32, b Nullable(String)),
    /// covering an unnamed tuple, element names, and a Nullable element.
    fn tuple_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "t".into(),
                ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
            },
            Field {
                name: "tn".into(),
                ch_type: ChType::Tuple(vec![
                    (Some("a".to_string()), ChType::Int32),
                    (
                        Some("b".to_string()),
                        ChType::Nullable(Box::new(ChType::String)),
                    ),
                ]),
            },
        ];
        let mut b = utf8_column(&[b"user_1", b"", b"user_2"]);
        b.validity = Some(Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]));
        let columns = vec![
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int32(PrimitiveColumn::new(vec![13, 79, -7])),
                    Column::Utf8(utf8_column(&[b"user_1", b"user_2", b""])),
                ],
                3,
            )),
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
                    Column::Utf8(b),
                ],
                3,
            )),
        ];
        ColBatch::new(Schema::new(fields), columns, 3)
    }

    /// Nullable(Tuple(Int32, String)) with the tuple-level null map, plus a
    /// tuple with a LowCardinality element (whose key-version prefix is hoisted
    /// ahead of element 0's body).
    fn nullable_and_lc_tuple_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "nt".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Tuple(vec![
                    (None, ChType::Int32),
                    (None, ChType::String),
                ]))),
            },
            Field {
                name: "tlc".into(),
                ch_type: ChType::Tuple(vec![
                    (Some("k".to_string()), ChType::Int32),
                    (
                        Some("lc".to_string()),
                        ChType::LowCardinality(Box::new(ChType::String)),
                    ),
                ]),
            },
        ];
        let columns = vec![
            Column::Tuple(TupleColumn::new_nullable(
                vec![
                    Column::Int32(PrimitiveColumn::new(vec![13, 0, 79])),
                    Column::Utf8(utf8_column(&[b"user_1", b"", b"user_2"])),
                ],
                3,
                Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]),
            )),
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
                    Column::Dictionary(DictionaryColumn::new(
                        vec![1, 2, 1],
                        Column::Utf8(utf8_column(&[b"", b"red", b"green"])),
                    )),
                ],
                3,
            )),
        ];
        ColBatch::new(Schema::new(fields), columns, 3)
    }

    /// Array(Tuple(Int32, Int32)) and a nested Tuple(p Tuple(Int8, Int8), s
    /// String), covering both container compositions.
    fn array_and_nested_tuple_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "at".into(),
                ch_type: ChType::Array(Box::new(ChType::Tuple(vec![
                    (None, ChType::Int32),
                    (None, ChType::Int32),
                ]))),
            },
            Field {
                name: "tt".into(),
                ch_type: ChType::Tuple(vec![
                    (
                        Some("p".to_string()),
                        ChType::Tuple(vec![(None, ChType::Int8), (None, ChType::Int8)]),
                    ),
                    (Some("s".to_string()), ChType::String),
                ]),
            },
        ];
        let columns = vec![
            // [], [(13, 79)], [(1, 2), (3, 4)] -> offsets [0, 0, 1, 3].
            Column::Array(ArrayColumn::new(
                vec![0, 0, 1, 3],
                Column::Tuple(TupleColumn::new(
                    vec![
                        Column::Int32(PrimitiveColumn::new(vec![13, 1, 3])),
                        Column::Int32(PrimitiveColumn::new(vec![79, 2, 4])),
                    ],
                    3,
                )),
            )),
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Tuple(TupleColumn::new(
                        vec![
                            Column::Int8(PrimitiveColumn::new(vec![1, 3, 5])),
                            Column::Int8(PrimitiveColumn::new(vec![2, 4, 6])),
                        ],
                        3,
                    )),
                    Column::Utf8(utf8_column(&[b"user_1", b"user_2", b"user_3"])),
                ],
                3,
            )),
        ];
        ColBatch::new(Schema::new(fields), columns, 3)
    }

    /// The zero-element Tuple(): one placeholder byte per row on the wire.
    fn empty_tuple_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "k".into(),
                ch_type: ChType::Int32,
            },
            Field {
                name: "t0".into(),
                ch_type: ChType::Tuple(vec![]),
            },
        ];
        let columns = vec![
            Column::Int32(PrimitiveColumn::new(vec![13, 79])),
            Column::Tuple(TupleColumn::new(vec![], 2)),
        ];
        ColBatch::new(Schema::new(fields), columns, 2)
    }

    /// Build a `MapColumn` from Arrow-shaped offsets plus the keys and values
    /// columns.
    fn map_column(offsets: Vec<i64>, keys: Column, values: Column) -> MapColumn {
        let total = keys.len();
        MapColumn::new(
            offsets,
            Column::Tuple(TupleColumn::new(vec![keys, values], total)),
        )
    }

    /// Map(String, Int32) plus Map(Int32, Nullable(String)), covering a plain
    /// map with an empty row and a Nullable value run.
    fn map_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "m".into(),
                ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
            },
            Field {
                name: "mnv".into(),
                ch_type: ChType::Map(
                    Box::new(ChType::Int32),
                    Box::new(ChType::Nullable(Box::new(ChType::String))),
                ),
            },
        ];
        let mut nullable_values = utf8_column(&[b"user_1", b"", b"user_2"]);
        nullable_values.validity = Some(Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]));
        let columns = vec![
            // {} / {a: 13} / {a: 1, b: 2}
            Column::Map(map_column(
                vec![0, 0, 1, 3],
                Column::Utf8(utf8_column(&[b"a", b"a", b"b"])),
                Column::Int32(PrimitiveColumn::new(vec![13, 1, 2])),
            )),
            // {1: user_1} / {2: NULL} / {3: user_2}
            Column::Map(map_column(
                vec![0, 1, 2, 3],
                Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
                Column::Utf8(nullable_values),
            )),
        ];
        ColBatch::new(Schema::new(fields), columns, 3)
    }

    /// Map(LowCardinality(String), UInt8) (the hoisted key prefix) plus
    /// Map(String, Array(Int32)) and a nested Map value.
    fn lc_and_nested_map_batch() -> ColBatch {
        let fields = vec![
            Field {
                name: "mlc".into(),
                ch_type: ChType::Map(
                    Box::new(ChType::LowCardinality(Box::new(ChType::String))),
                    Box::new(ChType::UInt8),
                ),
            },
            Field {
                name: "marr".into(),
                ch_type: ChType::Map(
                    Box::new(ChType::String),
                    Box::new(ChType::Array(Box::new(ChType::Int32))),
                ),
            },
            Field {
                name: "mm".into(),
                ch_type: ChType::Map(
                    Box::new(ChType::String),
                    Box::new(ChType::Map(
                        Box::new(ChType::String),
                        Box::new(ChType::Int32),
                    )),
                ),
            },
        ];
        let columns = vec![
            // {red: 1} / {} / {red: 2, blue: 3}
            Column::Map(map_column(
                vec![0, 1, 1, 3],
                Column::Dictionary(DictionaryColumn::new(
                    vec![1, 1, 2],
                    Column::Utf8(utf8_column(&[b"", b"red", b"blue"])),
                )),
                Column::UInt8(PrimitiveColumn::new(vec![1, 2, 3])),
            )),
            // {a: [13]} / {b: [], c: [1, 2]} / {}
            Column::Map(map_column(
                vec![0, 1, 3, 3],
                Column::Utf8(utf8_column(&[b"a", b"b", b"c"])),
                Column::Array(ArrayColumn::new(
                    vec![0, 1, 1, 3],
                    Column::Int32(PrimitiveColumn::new(vec![13, 1, 2])),
                )),
            )),
            // {a: {x: 1}} / {b: {y: 2, z: 3}} / {}
            Column::Map(map_column(
                vec![0, 1, 2, 2],
                Column::Utf8(utf8_column(&[b"a", b"b"])),
                Column::Map(map_column(
                    vec![0, 1, 3],
                    Column::Utf8(utf8_column(&[b"x", b"y", b"z"])),
                    Column::Int32(PrimitiveColumn::new(vec![1, 2, 3])),
                )),
            )),
        ];
        ColBatch::new(Schema::new(fields), columns, 3)
    }

    /// Array(Map(String, Int32)): maps flattened under array offsets.
    fn array_of_map_batch() -> ColBatch {
        let fields = vec![Field {
            name: "am".into(),
            ch_type: ChType::Array(Box::new(ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Int32),
            ))),
        }];
        // [] / [{a: 1}] / [{b: 2}, {}]
        let columns = vec![Column::Array(ArrayColumn::new(
            vec![0, 0, 1, 3],
            Column::Map(map_column(
                vec![0, 1, 2, 2],
                Column::Utf8(utf8_column(&[b"a", b"b"])),
                Column::Int32(PrimitiveColumn::new(vec![1, 2])),
            )),
        ))];
        ColBatch::new(Schema::new(fields), columns, 3)
    }

    /// Compare two columns for the types this encoder covers. Used recursively
    /// for `LowCardinality` dictionary values.
    fn assert_columns_eq(left: &Column, right: &Column, label: &str) {
        macro_rules! eq {
            ($va:expr, $vb:expr) => {
                assert_eq!($va.values, $vb.values, "{label} values differ")
            };
        }
        match (left, right) {
            (Column::Bool(x), Column::Bool(y)) => {
                assert_eq!(x.len, y.len, "{label} bool len differs");
                for row in 0..x.len {
                    assert_eq!(x.get(row), y.get(row), "{label} bool row {row} differs");
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
                assert_eq!(x.width, y.width, "{label} width differ");
                assert_eq!(x.data, y.data, "{label} data differ");
            }
            (Column::Utf8(x), Column::Utf8(y)) => {
                assert_eq!(x.offsets, y.offsets, "{label} offsets differ");
                assert_eq!(x.data, y.data, "{label} data differ");
            }
            (Column::FixedBinary(x), Column::FixedBinary(y)) => {
                assert_eq!(x.width, y.width, "{label} width differ");
                assert_eq!(x.data, y.data, "{label} data differ");
            }
            (Column::Decimal(x), Column::Decimal(y)) => {
                assert_eq!(x.width, y.width, "{label} width differ");
                assert_eq!(x.precision, y.precision, "{label} precision differs");
                assert_eq!(x.scale, y.scale, "{label} scale differs");
                assert_eq!(x.data, y.data, "{label} data differ");
            }
            (Column::Dictionary(x), Column::Dictionary(y)) => {
                assert_eq!(x.indices, y.indices, "{label} dictionary indices differ");
                let dict_label = format!("{label} dictionary");
                assert_columns_eq(x.values.as_ref(), y.values.as_ref(), &dict_label);
            }
            (Column::Array(x), Column::Array(y)) => {
                assert_eq!(x.offsets, y.offsets, "{label} array offsets differ");
                let elem_label = format!("{label} array elements");
                assert_columns_eq(x.values.as_ref(), y.values.as_ref(), &elem_label);
            }
            (Column::Tuple(x), Column::Tuple(y)) => {
                assert_eq!(x.len, y.len, "{label} tuple len differs");
                assert_eq!(
                    x.fields.len(),
                    y.fields.len(),
                    "{label} tuple field count differs"
                );
                for (i, (a, b)) in x.fields.iter().zip(&y.fields).enumerate() {
                    assert_columns_eq(a, b, &format!("{label} tuple element {i}"));
                }
            }
            (Column::Map(x), Column::Map(y)) => {
                assert_eq!(x.offsets, y.offsets, "{label} map offsets differ");
                let entries_label = format!("{label} map entries");
                assert_columns_eq(x.entries.as_ref(), y.entries.as_ref(), &entries_label);
            }
            (other_a, other_b) => panic!("{label}: unexpected {other_a:?} vs {other_b:?}"),
        }

        // Validity (the null map or dictionary-index validity) must survive the
        // round-trip too. Both sides must agree on presence and on every row's
        // valid/null bit.
        match (left.validity(), right.validity()) {
            (None, None) => {}
            (Some(x), Some(y)) => {
                assert_eq!(x.len(), y.len(), "{label} validity len differs");
                for row in 0..x.len() {
                    assert_eq!(
                        x.is_valid(row),
                        y.is_valid(row),
                        "{label} validity row {row} differs"
                    );
                }
            }
            (x, y) => panic!(
                "{label} validity presence differs: {} vs {}",
                x.is_some(),
                y.is_some()
            ),
        }
    }

    /// Compare two batches column by column. Panics on any unexpected variant so
    /// a wrong decode is loud.
    fn assert_batches_eq(left: &ColBatch, right: &ColBatch) {
        assert_eq!(left.schema, right.schema, "schema mismatch");
        assert_eq!(left.num_rows, right.num_rows, "row count mismatch");
        assert_eq!(
            left.columns.len(),
            right.columns.len(),
            "column count mismatch"
        );
        for (i, (a, b)) in left.columns.iter().zip(&right.columns).enumerate() {
            assert_columns_eq(a, b, &format!("column {i}"));
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
    fn roundtrip_low_cardinality_rev0() {
        roundtrip(&low_cardinality_batch(), 0);
    }

    #[test]
    fn roundtrip_low_cardinality_tcp_revision() {
        roundtrip(&low_cardinality_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_nullable_low_cardinality_rev0() {
        roundtrip(&low_cardinality_nullable_batch(), 0);
    }

    #[test]
    fn roundtrip_nullable_low_cardinality_tcp_revision() {
        roundtrip(&low_cardinality_nullable_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_array_int32_rev0() {
        roundtrip(&array_int32_batch(), 0);
    }

    #[test]
    fn roundtrip_array_int32_tcp_revision() {
        roundtrip(&array_int32_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_array_nullable_string_rev0() {
        roundtrip(&array_nullable_string_batch(), 0);
    }

    #[test]
    fn roundtrip_array_nullable_string_tcp_revision() {
        roundtrip(&array_nullable_string_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_array_low_cardinality_rev0() {
        roundtrip(&array_low_cardinality_batch(), 0);
    }

    #[test]
    fn roundtrip_array_low_cardinality_tcp_revision() {
        roundtrip(&array_low_cardinality_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_array_low_cardinality_all_empty_rev0() {
        roundtrip(&array_low_cardinality_all_empty_batch(), 0);
    }

    #[test]
    fn roundtrip_array_low_cardinality_all_empty_tcp_revision() {
        roundtrip(
            &array_low_cardinality_all_empty_batch(),
            DBMS_TCP_PROTOCOL_VERSION,
        );
    }

    #[test]
    fn roundtrip_array_of_array_rev0() {
        roundtrip(&array_of_array_batch(), 0);
    }

    #[test]
    fn roundtrip_array_of_array_tcp_revision() {
        roundtrip(&array_of_array_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_tuple_rev0() {
        roundtrip(&tuple_batch(), 0);
    }

    #[test]
    fn roundtrip_tuple_tcp_revision() {
        roundtrip(&tuple_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_nullable_and_lc_tuple_rev0() {
        roundtrip(&nullable_and_lc_tuple_batch(), 0);
    }

    #[test]
    fn roundtrip_nullable_and_lc_tuple_tcp_revision() {
        roundtrip(&nullable_and_lc_tuple_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_array_and_nested_tuple_rev0() {
        roundtrip(&array_and_nested_tuple_batch(), 0);
    }

    #[test]
    fn roundtrip_array_and_nested_tuple_tcp_revision() {
        roundtrip(&array_and_nested_tuple_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_map_rev0() {
        roundtrip(&map_batch(), 0);
    }

    #[test]
    fn roundtrip_map_tcp_revision() {
        roundtrip(&map_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_lc_and_nested_map_rev0() {
        roundtrip(&lc_and_nested_map_batch(), 0);
    }

    #[test]
    fn roundtrip_lc_and_nested_map_tcp_revision() {
        roundtrip(&lc_and_nested_map_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_array_of_map_rev0() {
        roundtrip(&array_of_map_batch(), 0);
    }

    #[test]
    fn roundtrip_array_of_map_tcp_revision() {
        roundtrip(&array_of_map_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_map_all_empty_lc_key() {
        // Map(LowCardinality(String), Int32) with rows > 0 but every map empty:
        // the wire must be the hoisted LC key version, the zero offsets, and
        // NOTHING for the key/value runs (limit == 0 gates through the Map
        // path).
        let fields = vec![Field {
            name: "m".into(),
            ch_type: ChType::Map(
                Box::new(ChType::LowCardinality(Box::new(ChType::String))),
                Box::new(ChType::Int32),
            ),
        }];
        let columns = vec![Column::Map(map_column(
            vec![0, 0, 0, 0],
            Column::Dictionary(DictionaryColumn::new(
                vec![],
                Column::Utf8(utf8_column(&[])),
            )),
            Column::Int32(PrimitiveColumn::new(vec![])),
        ))];
        let batch = ColBatch::new(Schema::new(fields), columns, 3);

        // Pin the exact wire body: header, then key version + three zero
        // offsets and nothing else.
        let bytes = encode_block(
            &batch,
            &EncodeOptions {
                protocol_revision: 0,
            },
        )
        .unwrap();
        let mut expected = Vec::new();
        expected.push(0x01); // 1 column
        expected.push(0x03); // 3 rows
        expected.push(0x01); // name len
        expected.extend_from_slice(b"m");
        let type_name = "Map(LowCardinality(String), Int32)";
        expected.push(type_name.len() as u8);
        expected.extend_from_slice(type_name.as_bytes());
        expected.extend_from_slice(&1u64.to_le_bytes()); // hoisted LC key version
        expected.extend_from_slice(&[0u8; 24]); // three zero offsets
        assert_eq!(bytes, expected);

        roundtrip(&batch, 0);
        roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn rev0_frames_map_bytes() {
        // Pin the Map body framing: the Array offsets run (no leading zero),
        // then the flattened key run, then the flattened value run. One
        // Map(String, Int32) column "m" with two rows {hi: 13} and {}.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "m".into(),
                ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
            }]),
            vec![Column::Map(map_column(
                vec![0, 1, 1],
                Column::Utf8(utf8_column(&[b"hi"])),
                Column::Int32(PrimitiveColumn::new(vec![13])),
            ))],
            2,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x02, // num_rows = 2
            0x01, b'm', // name "m"
            0x12, // type name length 18
            b'M', b'a', b'p', b'(', b'S', b't', b'r', b'i', b'n', b'g', b',', b' ', b'I', b'n',
            b't', b'3', b'2', b')', // type "Map(String, Int32)"
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 0: 1
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 1: 1
            0x02, b'h', b'i', // key run: varint len 2 then "hi"
            0x0D, 0x00, 0x00, 0x00, // value run: Int32 13
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn map_illegal_key_type_is_rejected() {
        // A Nullable key violates the server's DataTypeMap::isValidKeyType, so
        // the type itself cannot exist: UnsupportedType, before any bytes.
        let ch_type = ChType::Map(
            Box::new(ChType::Nullable(Box::new(ChType::String))),
            Box::new(ChType::Int32),
        );
        let mut keys = utf8_column(&[b"a"]);
        keys.validity = Some(Bitmap::from_ch_null_map(&[0x00]));
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "m".into(),
                ch_type: ch_type.clone(),
            }]),
            vec![Column::Map(map_column(
                vec![0, 1],
                Column::Utf8(keys),
                Column::Int32(PrimitiveColumn::new(vec![13])),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { column, ch_type: t } => {
                assert_eq!(column, "m");
                assert_eq!(t, ch_type);
            }
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }

    #[test]
    fn map_offsets_entries_mismatch_is_rejected() {
        // Offsets end at 2 but the entries tuple holds 1 row: a misframed
        // stream the server would reject, so InconsistentBatch before any
        // bytes.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "m".into(),
                ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
            }]),
            vec![Column::Map(map_column(
                vec![0, 2],
                Column::Utf8(utf8_column(&[b"a"])),
                Column::Int32(PrimitiveColumn::new(vec![13])),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn map_ragged_entries_are_rejected() {
        // Keys and values of different lengths cannot both be full runs of the
        // entry count: InconsistentBatch, not a panic.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "m".into(),
                ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
            }]),
            vec![Column::Map(map_column(
                vec![0, 2],
                Column::Utf8(utf8_column(&[b"a", b"b"])),
                Column::Int32(PrimitiveColumn::new(vec![13])),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn nullable_map_nesting_is_rejected() {
        // Nullable(Map) is not constructible on the server
        // (canBeInsideNullable false); the type-header round-trip check fails
        // before any bytes are written, like Nullable(LowCardinality).
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "nm".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Map(
                    Box::new(ChType::String),
                    Box::new(ChType::Int32),
                ))),
            }]),
            vec![Column::Map(map_column(
                vec![0, 1],
                Column::Utf8(utf8_column(&[b"a"])),
                Column::Int32(PrimitiveColumn::new(vec![13])),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_empty_tuple_rev0() {
        roundtrip(&empty_tuple_batch(), 0);
    }

    #[test]
    fn roundtrip_empty_tuple_tcp_revision() {
        roundtrip(&empty_tuple_batch(), DBMS_TCP_PROTOCOL_VERSION);
    }

    #[test]
    fn roundtrip_array_of_tuple_all_empty() {
        // Array(Tuple(LowCardinality(String), Int32)) with rows > 0 but every
        // array empty: the wire must be the hoisted LC key version, the zero
        // offsets, and NOTHING for the element bodies (each element gets a
        // limit == 0 run through the Tuple path; the LC early-return gate must
        // fire).
        let fields = vec![Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::Tuple(vec![
                (None, ChType::LowCardinality(Box::new(ChType::String))),
                (None, ChType::Int32),
            ]))),
        }];
        let columns = vec![Column::Array(ArrayColumn::new(
            vec![0, 0, 0, 0],
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Dictionary(DictionaryColumn::new(
                        vec![],
                        Column::Utf8(utf8_column(&[])),
                    )),
                    Column::Int32(PrimitiveColumn::new(vec![])),
                ],
                0,
            )),
        ))];
        let batch = ColBatch::new(Schema::new(fields), columns, 3);

        // Pin the exact wire body: header, then key version + three zero
        // offsets and nothing else.
        let bytes = encode_block(
            &batch,
            &EncodeOptions {
                protocol_revision: 0,
            },
        )
        .unwrap();
        let mut expected = Vec::new();
        expected.push(0x01); // 1 column
        expected.push(0x03); // 3 rows
        expected.push(0x01); // name len
        expected.extend_from_slice(b"a");
        let type_name = "Array(Tuple(LowCardinality(String), Int32))";
        expected.push(type_name.len() as u8);
        expected.extend_from_slice(type_name.as_bytes());
        expected.extend_from_slice(&1u64.to_le_bytes()); // hoisted LC key version
        expected.extend_from_slice(&[0u8; 24]); // three zero offsets
        assert_eq!(bytes, expected);

        roundtrip(&batch, 0);
        roundtrip(&batch, DBMS_TCP_PROTOCOL_VERSION);
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
            Field {
                name: "lc".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::String)),
            },
            Field {
                name: "lcn".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(
                    ChType::String,
                )))),
            },
            Field {
                name: "a".into(),
                ch_type: ChType::Array(Box::new(ChType::Int32)),
            },
            Field {
                name: "alc".into(),
                ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
            },
            Field {
                name: "t".into(),
                ch_type: ChType::Tuple(vec![
                    (Some("a".to_string()), ChType::Int32),
                    (Some("b".to_string()), ChType::String),
                ]),
            },
            Field {
                name: "t0".into(),
                ch_type: ChType::Tuple(vec![]),
            },
            Field {
                name: "m".into(),
                ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
            },
        ];
        let columns = vec![
            Column::Int32(PrimitiveColumn::new(vec![])),
            Column::Float64(PrimitiveColumn::new(vec![])),
            Column::Uuid(FixedBinaryColumn::new(Vec::new(), 16)),
            Column::Ipv4(PrimitiveColumn::new(Vec::new())),
            Column::Ipv6(FixedBinaryColumn::new(Vec::new(), 16)),
            Column::Decimal(DecimalColumn::new(Vec::new(), 4, 9, 4)),
            Column::Dictionary(DictionaryColumn::new(
                vec![],
                Column::Utf8(utf8_column(&[])),
            )),
            Column::Dictionary(DictionaryColumn::new_nullable(
                vec![],
                Column::Utf8(utf8_column(&[])),
                Bitmap::from_ch_null_map(&[]),
            )),
            // A zero-row Array carries only the leading-0 offset and writes no
            // data at all, not even the hoisted LC key version of an LC element.
            Column::Array(ArrayColumn::new(
                vec![0],
                Column::Int32(PrimitiveColumn::new(vec![])),
            )),
            Column::Array(ArrayColumn::new(
                vec![0],
                Column::Dictionary(DictionaryColumn::new(
                    vec![],
                    Column::Utf8(utf8_column(&[])),
                )),
            )),
            // A zero-row Tuple carries only the header: no element bodies, and
            // for Tuple() no placeholder bytes either.
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int32(PrimitiveColumn::new(vec![])),
                    Column::Utf8(utf8_column(&[])),
                ],
                0,
            )),
            Column::Tuple(TupleColumn::new(vec![], 0)),
            // A zero-row Map carries only the leading-0 offset and writes no
            // data at all.
            Column::Map(map_column(
                vec![0],
                Column::Utf8(utf8_column(&[])),
                Column::Int32(PrimitiveColumn::new(vec![])),
            )),
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
    fn encode_chunked_roundtrips_low_cardinality_blocks() {
        // LowCardinality dictionaries are block-local. These two chunks use
        // different dictionaries and must stay separate after decode.
        let field = Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::String)),
        };
        let chunk = |values: &[&[u8]], indices: Vec<i32>| {
            let n = indices.len();
            std::sync::Arc::new(ColBatch::new(
                Schema::new(vec![field.clone()]),
                vec![Column::Dictionary(DictionaryColumn::new(
                    indices,
                    Column::Utf8(utf8_column(values)),
                ))],
                n,
            ))
        };
        let batch = ChunkedBatch {
            schema: Schema::new(vec![field.clone()]),
            chunks: vec![
                chunk(&[b"", b"user_1", b"user_2"], vec![1, 2, 1]),
                chunk(&[b"", b"user_3"], vec![1, 1]),
            ],
        };
        let bytes = encode_chunked(&batch, &EncodeOptions::default()).unwrap();
        let decoded = decode_all_bytes(&bytes, &DecodeOptions::default()).unwrap();
        assert_eq!(decoded.num_chunks(), 2);
        for (sent, got) in batch.chunks.iter().zip(&decoded.chunks) {
            assert_batches_eq(sent, got);
        }
    }

    #[test]
    fn encode_chunked_roundtrips_array_blocks() {
        // Array element data is block-local (offsets restart at 0 per block).
        // Two Array(Int32) chunks with different shapes must stay separate
        // after decode, never concatenated.
        let field = Field {
            name: "a".into(),
            ch_type: ChType::Array(Box::new(ChType::Int32)),
        };
        let chunk = |offsets: Vec<i64>, values: Vec<i32>| {
            let n = offsets.len() - 1;
            std::sync::Arc::new(ColBatch::new(
                Schema::new(vec![field.clone()]),
                vec![Column::Array(ArrayColumn::new(
                    offsets,
                    Column::Int32(PrimitiveColumn::new(values)),
                ))],
                n,
            ))
        };
        let batch = ChunkedBatch {
            schema: Schema::new(vec![field.clone()]),
            chunks: vec![
                chunk(vec![0, 2, 2, 3], vec![13, 79, 21]),
                chunk(vec![0, 2], vec![34, 55]),
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
    fn nullable_low_cardinality_nesting_is_rejected() {
        // `Nullable(LowCardinality(T))` is the illegal nesting direction. The
        // supported shape is `LowCardinality(Nullable(T))`, so this must fail at
        // the type-header round-trip check before any bytes are written.
        let lc_string = ChType::LowCardinality(Box::new(ChType::String));
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "nlc".into(),
                ch_type: ChType::Nullable(Box::new(lc_string)),
            }]),
            vec![Column::Dictionary(DictionaryColumn::new(
                vec![0],
                Column::Utf8(utf8_column(&[b"user_1"])),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn low_cardinality_unsupported_inner_reports_column_and_type() {
        // Decimal is encodable as a plain column, but the server forbids it as a
        // LowCardinality inner (`canBeInsideLowCardinality()` is false), so the
        // wrapper remains unsupported and reports the full declared type.
        let lc_decimal = ChType::LowCardinality(Box::new(ChType::Decimal {
            precision: 9,
            scale: 4,
            bits: 32,
        }));
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "lc".into(),
                ch_type: lc_decimal.clone(),
            }]),
            vec![Column::Dictionary(DictionaryColumn::new(
                vec![0],
                Column::Decimal(DecimalColumn::new(vec![0u8; 4], 4, 9, 4)),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { column, ch_type } => {
                assert_eq!(column, "lc");
                assert_eq!(ch_type, lc_decimal);
            }
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }

    #[test]
    fn rev0_frames_tuple_bytes() {
        // Pin the Tuple body framing: element 0's FULL run then element 1's,
        // column-of-columns, no interleaving, no offsets, no tuple-level
        // framing. One Tuple(Int32, String) column "t" with two rows
        // (13, "hi") and (-1, "").
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "t".into(),
                ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
            }]),
            vec![Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int32(PrimitiveColumn::new(vec![13, -1])),
                    Column::Utf8(utf8_column(&[b"hi", b""])),
                ],
                2,
            ))],
            2,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x02, // num_rows = 2
            0x01, b't', // name "t"
            0x14, // type name length 20
            b'T', b'u', b'p', b'l', b'e', b'(', b'I', b'n', b't', b'3', b'2', b',', b' ', b'S',
            b't', b'r', b'i', b'n', b'g', b')', // type "Tuple(Int32, String)"
            0x0D, 0x00, 0x00, 0x00, // element 0 row 0: Int32 13
            0xFF, 0xFF, 0xFF, 0xFF, // element 0 row 1: Int32 -1
            0x02, b'h', b'i', // element 1 row 0: varint len 2 then "hi"
            0x00, // element 1 row 1: varint len 0
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn rev0_frames_empty_tuple_bytes() {
        // Pin the zero-element Tuple() body: exactly one literal ASCII '0'
        // byte (0x30) per row, nothing else.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "t0".into(),
                ch_type: ChType::Tuple(vec![]),
            }]),
            vec![Column::Tuple(TupleColumn::new(vec![], 3))],
            3,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x03, // num_rows = 3
            0x02, b't', b'0', // name "t0"
            0x07, b'T', b'u', b'p', b'l', b'e', b'(', b')', // type "Tuple()"
            0x30, 0x30, 0x30, // one ASCII '0' per row
        ];
        assert_eq!(bytes, expected);
    }

    /// A one-element named-tuple batch over a matching one-field Int8 column,
    /// for the element-name legality tests.
    fn named_tuple_batch(name: Option<&str>) -> ColBatch {
        ColBatch::new(
            Schema::new(vec![Field {
                name: "t".into(),
                ch_type: ChType::Tuple(vec![(name.map(str::to_string), ChType::Int8)]),
            }]),
            vec![Column::Tuple(TupleColumn::new(
                vec![Column::Int8(PrimitiveColumn::new(vec![13]))],
                1,
            ))],
            1,
        )
    }

    #[test]
    fn tuple_illegal_element_names_are_rejected() {
        // Mirror the server's checkTupleNames: an empty name and the reserved
        // exact-lowercase "null" cannot exist on the server, so they are
        // UnsupportedType. The decode parser round-trips these shapes (a
        // server-authored header is preserved), so the type-string round-trip
        // check cannot catch them; the explicit name check must.
        for bad in [Some(""), Some("null")] {
            match encode_block(&named_tuple_batch(bad), &EncodeOptions::default()).unwrap_err() {
                EncodeError::UnsupportedType { column, .. } => assert_eq!(column, "t"),
                other => panic!("expected UnsupportedType for {bad:?}, got {other:?}"),
            }
        }
        // Any-case variants other than exact-lowercase "null" are legal on the
        // server (checkTupleNames compares exactly) and render backtick-quoted.
        for ok in [Some("NULL"), Some("Null"), Some("a"), None] {
            encode_block(&named_tuple_batch(ok), &EncodeOptions::default())
                .unwrap_or_else(|e| panic!("{ok:?} should encode: {e}"));
        }
    }

    #[test]
    fn tuple_duplicate_element_names_are_rejected() {
        // checkTupleNames rejects duplicates (DUPLICATE_COLUMN). Unnamed
        // elements do not count as duplicates of each other.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "t".into(),
                ch_type: ChType::Tuple(vec![
                    (Some("a".to_string()), ChType::Int8),
                    (Some("a".to_string()), ChType::Int8),
                ]),
            }]),
            vec![Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int8(PrimitiveColumn::new(vec![13])),
                    Column::Int8(PrimitiveColumn::new(vec![79])),
                ],
                1,
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { column, .. } => assert_eq!(column, "t"),
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }

    #[test]
    fn tuple_mixed_named_unnamed_elements_are_rejected() {
        // The server's tuple type factory rejects mixed named/unnamed
        // arguments ("Names are specified not for all elements of Tuple
        // type"), so a mixed ChType is caller-constructed-only and its
        // rendered header cannot be parsed back by the server.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "t".into(),
                ch_type: ChType::Tuple(vec![
                    (Some("a".to_string()), ChType::Int8),
                    (None, ChType::Int8),
                ]),
            }]),
            vec![Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int8(PrimitiveColumn::new(vec![13])),
                    Column::Int8(PrimitiveColumn::new(vec![79])),
                ],
                1,
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { column, .. } => assert_eq!(column, "t"),
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }

    #[test]
    fn nested_tuple_illegal_names_are_rejected() {
        // The name legality check applies through nesting: a duplicate-named
        // tuple as an Array element is rejected by the recursive validation.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "a".into(),
                ch_type: ChType::Array(Box::new(ChType::Tuple(vec![
                    (Some("x".to_string()), ChType::Int8),
                    (Some("x".to_string()), ChType::Int8),
                ]))),
            }]),
            vec![Column::Array(ArrayColumn::new(
                vec![0, 1],
                Column::Tuple(TupleColumn::new(
                    vec![
                        Column::Int8(PrimitiveColumn::new(vec![13])),
                        Column::Int8(PrimitiveColumn::new(vec![79])),
                    ],
                    1,
                )),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { .. } => {}
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
    }

    #[test]
    fn plain_tuple_validity_with_nulls_is_rejected() {
        // A non-Nullable Tuple field whose TupleColumn carries null-marked
        // validity would have the null map silently dropped (no null map is
        // written for a non-nullable column), so the generic nullability check
        // rejects it, the same as every other non-nullable column type.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "t".into(),
                ch_type: ChType::Tuple(vec![(None, ChType::Int8)]),
            }]),
            vec![Column::Tuple(TupleColumn::new_nullable(
                vec![Column::Int8(PrimitiveColumn::new(vec![13, 0]))],
                2,
                Bitmap::from_ch_null_map(&[0x00, 0x01]),
            ))],
            2,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn map_entries_validity_is_rejected() {
        // The Map entries tuple never carries validity on the wire;
        // encode_map_data writes no null map for it, so a caller-attached
        // bitmap (even all-valid) would be silently dropped. Rejected before
        // any bytes.
        let entries = Column::Tuple(TupleColumn::new_nullable(
            vec![
                Column::Utf8(utf8_column(&[b"a"])),
                Column::Int32(PrimitiveColumn::new(vec![13])),
            ],
            1,
            Bitmap::from_ch_null_map(&[0x00]),
        ));
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "m".into(),
                ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
            }]),
            vec![Column::Map(MapColumn::new(vec![0, 1], entries))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { detail } => {
                assert!(detail.contains("entries"), "got detail {detail:?}");
            }
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn tuple_field_count_mismatch_is_rejected() {
        // The declared type has two elements; the buffer carries one field
        // column. InconsistentBatch, before any bytes are written.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "t".into(),
                ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
            }]),
            vec![Column::Tuple(TupleColumn::new(
                vec![Column::Int32(PrimitiveColumn::new(vec![13]))],
                1,
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn tuple_ragged_element_lengths_are_rejected() {
        // Element 0 has two rows, element 1 has one: a ragged tuple would put a
        // misframed stream on the wire (the server's equal-sizes INCORRECT_DATA
        // invariant), so it is InconsistentBatch, not a panic.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "t".into(),
                ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
            }]),
            vec![Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int32(PrimitiveColumn::new(vec![13, 79])),
                    Column::Utf8(utf8_column(&[b"user_1"])),
                ],
                2,
            ))],
            2,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn tuple_mismatched_element_buffer_is_rejected() {
        // A declared Int64 element over an Int32 buffer is a wrong-buffer
        // mismatch, not a wrong-width column on the wire.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "t".into(),
                ch_type: ChType::Tuple(vec![(None, ChType::Int64)]),
            }]),
            vec![Column::Tuple(TupleColumn::new(
                vec![Column::Int32(PrimitiveColumn::new(vec![13]))],
                1,
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn tuple_type_depth_is_capped_via_worklist() {
        // A pathologically deep caller-constructed type must be rejected by the
        // iterative depth walk before any recursive machinery touches it. Tuple
        // is the multi-child container, so this exercises the worklist path
        // with a depth well past MAX_TYPE_DEPTH.
        let mut ch_type = ChType::Int8;
        let mut column = Column::Int8(PrimitiveColumn::new(vec![13]));
        for _ in 0..(MAX_TYPE_DEPTH * 4) {
            ch_type = ChType::Tuple(vec![(None, ch_type)]);
            column = Column::Tuple(TupleColumn::new(vec![column], 1));
        }
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "deep".into(),
                ch_type,
            }]),
            vec![column],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { detail } => {
                assert!(detail.contains("nesting exceeds"), "got detail {detail:?}");
            }
            other => panic!("expected InconsistentBatch, got {other:?}"),
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
    fn rev0_frames_low_cardinality_string_bytes() {
        // Pin the LowCardinality body framing at rev 0. The server-confirmed
        // Native index word sets both HasAdditionalKeysBit and
        // NeedUpdateDictionary, so a UInt8-index block writes 0x600.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "lc".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::String)),
            }]),
            vec![Column::Dictionary(DictionaryColumn::new(
                vec![1],
                Column::Utf8(utf8_column(&[b"", b"user_1"])),
            ))],
            1,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x01, // num_rows = 1
            0x02, b'l', b'c', // name "lc"
            0x16, b'L', b'o', b'w', b'C', b'a', b'r', b'd', b'i', b'n', b'a', b'l', b'i', b't',
            b'y', b'(', b'S', b't', b'r', b'i', b'n', b'g', b')',
            // LowCardinality key version = 1.
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // index_word = 0x600: UInt8 tag, HasAdditionalKeysBit,
            // NeedUpdateDictionary.
            0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, // num_keys = 2
            0x00, // dictionary[0] = ""
            0x06, b'u', b's', b'e', b'r', b'_', b'1', // dictionary[1]
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // row count = 1
            0x01, // row index = 1
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn rev0_frames_low_cardinality_zero_rows_without_payload() {
        // Zero-row Native blocks write only the column header. The server skips
        // writeData entirely, so there is no LowCardinality key-version prefix.
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "lc".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::String)),
            }]),
            vec![Column::Dictionary(DictionaryColumn::new(
                vec![],
                Column::Utf8(utf8_column(&[])),
            ))],
            0,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x00, // num_rows = 0
            0x02, b'l', b'c', // name "lc"
            0x16, b'L', b'o', b'w', b'C', b'a', b'r', b'd', b'i', b'n', b'a', b'l', b'i', b't',
            b'y', b'(', b'S', b't', b'r', b'i', b'n', b'g', b')',
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn rev0_frames_array_int32_bytes() {
        // Pin the Array body framing: one raw LE u64 cumulative end-offset per
        // row with NO leading zero and no count, then the flattened element
        // body. Two rows [13, 79] and [] (the empty row repeats the previous
        // end-offset).
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "a".into(),
                ch_type: ChType::Array(Box::new(ChType::Int32)),
            }]),
            vec![Column::Array(ArrayColumn::new(
                vec![0, 2, 2],
                Column::Int32(PrimitiveColumn::new(vec![13, 79])),
            ))],
            2,
        );
        let bytes = encode_block(&batch, &EncodeOptions::default()).unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x02, // num_rows = 2
            0x01, b'a', // name "a"
            0x0C, b'A', b'r', b'r', b'a', b'y', b'(', b'I', b'n', b't', b'3', b'2',
            b')', // type "Array(Int32)"
            0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 0 = 2
            0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 1 = 2
            0x0D, 0x00, 0x00, 0x00, // Int32 13, little-endian
            0x4F, 0x00, 0x00, 0x00, // Int32 79, little-endian
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn rev0_frames_array_low_cardinality_all_empty_bytes() {
        // Pin the all-empty Array(LowCardinality(String)) shape: the hoisted LC
        // key version FIRST (the element state prefix, before the offsets), then
        // the all-zero offsets, then NOTHING for the LC element run
        // (`SerializationLowCardinality::serializeBinaryBulkWithMultipleStreams`
        // early-returns at limit == 0; confirmed at v26.6.1.1193-stable). An
        // index word, key count, or row count here would make the server
        // misparse the INSERT.
        let bytes = encode_block(
            &array_low_cardinality_all_empty_batch(),
            &EncodeOptions::default(),
        )
        .unwrap();
        let expected = [
            0x01, // num_cols = 1
            0x02, // num_rows = 2
            0x03, b'a', b'l', b'c', // name "alc"
            0x1D, b'A', b'r', b'r', b'a', b'y', b'(', b'L', b'o', b'w', b'C', b'a', b'r', b'd',
            b'i', b'n', b'a', b'l', b'i', b't', b'y', b'(', b'S', b't', b'r', b'i', b'n', b'g',
            b')', b')', // type "Array(LowCardinality(String))"
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // hoisted LC key version = 1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset row 0 = 0
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, // offset row 1 = 0
                  // nothing else: zero-length LC element run writes no body
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn low_cardinality_index_width_selects_self_describing_widths() {
        assert_eq!(low_cardinality_index_width(0), (1, 0));
        assert_eq!(low_cardinality_index_width(255), (1, 0));
        assert_eq!(low_cardinality_index_width(256), (2, 1));
        assert_eq!(low_cardinality_index_width(65_535), (2, 1));
        assert_eq!(low_cardinality_index_width(65_536), (4, 2));
    }

    #[test]
    fn low_cardinality_negative_index_is_rejected() {
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "lc".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::String)),
            }]),
            vec![Column::Dictionary(DictionaryColumn::new(
                vec![-1],
                Column::Utf8(utf8_column(&[b"", b"user_1"])),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn low_cardinality_out_of_range_index_is_rejected() {
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "lc".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::UInt32)),
            }]),
            vec![Column::Dictionary(DictionaryColumn::new(
                vec![2],
                Column::UInt32(PrimitiveColumn::new(vec![0, 13])),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn low_cardinality_nullable_valid_index_zero_is_rejected() {
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "lcn".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(
                    ChType::String,
                )))),
            }]),
            vec![Column::Dictionary(DictionaryColumn::new_nullable(
                vec![0],
                Column::Utf8(utf8_column(&[b"", b"user_1"])),
                Bitmap::from_ch_null_map(&[0]),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn low_cardinality_nullable_null_nonzero_index_is_rejected() {
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "lcn".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::Nullable(Box::new(
                    ChType::UInt32,
                )))),
            }]),
            vec![Column::Dictionary(DictionaryColumn::new_nullable(
                vec![1],
                Column::UInt32(PrimitiveColumn::new(vec![0, 13])),
                Bitmap::from_ch_null_map(&[1]),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn low_cardinality_dictionary_type_mismatch_is_rejected() {
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "lc".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::String)),
            }]),
            vec![Column::Dictionary(DictionaryColumn::new(
                vec![1],
                Column::UInt32(PrimitiveColumn::new(vec![0, 13])),
            ))],
            1,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn low_cardinality_zero_rows_nonempty_dictionary_is_rejected() {
        let batch = ColBatch::new(
            Schema::new(vec![Field {
                name: "lc".into(),
                ch_type: ChType::LowCardinality(Box::new(ChType::String)),
            }]),
            vec![Column::Dictionary(DictionaryColumn::new(
                vec![],
                Column::Utf8(utf8_column(&[b"", b"user_1"])),
            ))],
            0,
        );
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
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

    /// Build a one-column `Array(Int32)` batch directly from raw offsets and
    /// leaf values so a malformed offset array reaches the encoder. `num_rows`
    /// is passed explicitly so only the Array invariants under test, not the
    /// row-count check, are exercised.
    fn array_batch_from_parts(offsets: Vec<i64>, values: Vec<i32>, num_rows: usize) -> ColBatch {
        ColBatch {
            schema: Schema::new(vec![Field {
                name: "a".into(),
                ch_type: ChType::Array(Box::new(ChType::Int32)),
            }]),
            columns: vec![Column::Array(ArrayColumn::new(
                offsets,
                Column::Int32(PrimitiveColumn::new(values)),
            ))],
            num_rows,
        }
    }

    #[test]
    fn array_non_monotonic_offsets_are_rejected() {
        // Decreasing offsets would frame a stream the server rejects with
        // INCORRECT_DATA (and would slice out of bounds on our own decode).
        let batch = array_batch_from_parts(vec![0, 3, 1], vec![13, 79, 21], 2);
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn array_offset_element_count_mismatch_is_rejected() {
        // A final offset that does not equal the flattened element count would
        // either silently drop trailing elements (too small) or declare
        // elements the body does not carry (too large). Both directions.
        for (offsets, values) in [
            (vec![0i64, 2], vec![13, 79, 21]), // ends at 2, holds 3
            (vec![0i64, 3], vec![13, 79]),     // ends at 3, holds 2
        ] {
            let batch = array_batch_from_parts(offsets, values, 1);
            match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
                EncodeError::InconsistentBatch { .. } => {}
                other => panic!("expected InconsistentBatch, got {other:?}"),
            }
        }
    }

    #[test]
    fn array_missing_leading_zero_offset_is_rejected() {
        // Arrow list offsets start at 0; a nonzero first offset means the
        // leading zero is missing and row 0's slice would drop leading elements.
        let batch = array_batch_from_parts(vec![1, 3], vec![13, 79, 21], 1);
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn array_wrong_offsets_length_is_rejected() {
        // An empty offsets vector reports 0 rows through the saturating len()
        // and so passes the row-count check at num_rows = 0, but it is not the
        // well-formed `[0]` shape; the explicit num_rows + 1 length check
        // rejects it before `offsets[0]` is read.
        let batch = array_batch_from_parts(vec![], vec![], 0);
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn array_negative_offset_is_rejected() {
        // A negative offset would wrap through the i64 -> u64 cast into a huge
        // wire offset. The monotonic check from the zero start catches it.
        let batch = array_batch_from_parts(vec![0, -1], vec![], 1);
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn array_element_validation_failure_is_rejected() {
        // Element-level guards must apply to the flattened element column: a
        // String element whose Utf8 offsets point past its data buffer would
        // panic mid-write, so the recursive element validation rejects it
        // through the Array before any bytes are written.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "a".into(),
                ch_type: ChType::Array(Box::new(ChType::String)),
            }]),
            columns: vec![Column::Array(ArrayColumn::new(
                vec![0, 1],
                Column::Utf8(Utf8Column::new(vec![0, 10], b"abc".to_vec())),
            ))],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { .. } => {}
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn over_deep_type_nesting_is_rejected() {
        // Encode input never passes through `parse_ch_type`, so its
        // MAX_TYPE_DEPTH cap does not protect the encoder: a caller-constructed
        // pathologically deep type would recurse one stack frame per wrapper
        // level in `column_variant_matches` / `is_encodable` / `Display` /
        // `write_state_prefix` and overflow the stack. The iterative depth walk
        // in `validate_column` rejects it first, and does so without cloning or
        // rendering the deep type (both recurse to full depth), which is why the
        // rejection is InconsistentBatch rather than UnsupportedType.
        let mut ch_type = ChType::Int32;
        for _ in 0..(MAX_TYPE_DEPTH + 100) {
            ch_type = ChType::Array(Box::new(ch_type));
        }
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "deep".into(),
                ch_type,
            }]),
            columns: vec![Column::Int32(PrimitiveColumn::new(vec![]))],
            num_rows: 0,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::InconsistentBatch { detail } => {
                assert!(detail.contains("nesting"), "unexpected detail: {detail}");
            }
            other => panic!("expected InconsistentBatch, got {other:?}"),
        }
    }

    #[test]
    fn array_forbidden_low_cardinality_element_is_unsupported() {
        // Decimal is encodable as a plain column but forbidden inside
        // LowCardinality (`canBeInsideLowCardinality()` is false), and nesting
        // that LC inside an Array must not launder it: the element validation
        // recurses and reports UnsupportedType before any bytes are written.
        let batch = ColBatch {
            schema: Schema::new(vec![Field {
                name: "a".into(),
                ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(
                    ChType::Decimal {
                        precision: 9,
                        scale: 4,
                        bits: 32,
                    },
                )))),
            }]),
            columns: vec![Column::Array(ArrayColumn::new(
                vec![0, 1],
                Column::Dictionary(DictionaryColumn::new(
                    vec![0],
                    Column::Decimal(DecimalColumn::new(vec![0u8; 4], 4, 9, 4)),
                )),
            ))],
            num_rows: 1,
        };
        match encode_block(&batch, &EncodeOptions::default()).unwrap_err() {
            EncodeError::UnsupportedType { .. } => {}
            other => panic!("expected UnsupportedType, got {other:?}"),
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
