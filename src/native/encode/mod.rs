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
//! `Date32`, `DateTime`, `DateTime64`, `Time`, `Time64`), `UUID`, `IPv4`,
//! `IPv6`, `String`, `FixedString(N)`, `Enum8`/`Enum16`, `Decimal(P, S)`, the
//! wide integers (`Int128`/`UInt128`/`Int256`/`UInt256`), `LowCardinality(T)`
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

use super::protocol::{
    DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION, DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS,
    LC_HAS_ADDITIONAL_KEYS_BIT, LC_NEED_UPDATE_DICTIONARY_BIT, LOW_CARDINALITY_KEY_VERSION,
    MAX_TYPE_DEPTH,
};
use super::type_parser::{
    decimal_bits_from_precision, is_low_cardinality_inner, is_simple_aggregate_func_spelling,
    is_valid_map_key_type, low_cardinality_dict_value_type, parse_ch_type,
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

    // Every `SimpleAggregateFunction` in the declared type must carry a
    // syntactically valid function spelling before its header is rendered. A
    // caller-constructed `func` is untrusted, and it is Displayed verbatim into
    // the type-string channel, so a value like `"sum, UInt64), evil"` would inject
    // extra type tokens into the header (the same bug class the Tuple element-name
    // validation guards). Run once here, after the depth cap so the recursive walk
    // is bounded, and before any bytes are written.
    validate_saf_func_spellings(field, &field.ch_type)?;

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

    // Expand a name-decoration alias (SimpleAggregateFunction, geo, Nested) to
    // the physical type it delegates to, for every STRUCTURAL check below. The
    // header round-trip check further down stays on `field.ch_type` (the alias
    // form), so the emitted header keeps the alias spelling. [`resolve_delegate`]
    // follows the whole alias chain, not a fixed number of steps, mirroring the
    // "recurse on the delegate" pattern the decoders use, so a nested alias like
    // `Nullable(SimpleAggregateFunction(_, Point))` resolves all the way to the
    // physical `Tuple` rather than stopping one level short.
    let physical = resolve_delegate(&field.ch_type);
    let physical_type = physical.as_ref().unwrap_or(&field.ch_type);

    // The concrete value type is the inner of a `Nullable`, else the physical
    // type itself; an alias under `Nullable` (`Nullable(Point)` -> `Tuple`,
    // `Nullable(SAF(_, T))` -> physical `T`) is resolved through the whole chain
    // here too.
    let value_inner = physical_type.inner();
    let value = resolve_delegate(value_inner);
    let value_type = value.as_ref().unwrap_or(value_inner);

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
    // the server reject (`FixedString(0)`, a `DateTime64`/`Time64` precision
    // above 9, a timezone whose bytes break the type grammar); catching it here
    // fails at the source rather than letting `decode(encode(x))` fail downstream.
    // Reached only for an encodable, buffer-matched type, so any failure is a bad
    // parameter on a supported type, which `InconsistentBatch` describes correctly.
    // This validates the type inside a `Nullable` wrapper too, since
    // `Display`/`parse` are total on the wrapper.
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
    // Nullability is read off the PHYSICAL type: a `SimpleAggregateFunction`
    // over `LowCardinality(Nullable(T))` is nullable at the index level even
    // though `field.ch_type` is the alias, so consult the delegate. For a
    // `LowCardinality`, the null flag comes from the shared
    // `low_cardinality_dict_value_type` helper, which sees through a SAF chain
    // around the removeNullable `Nullable`, so
    // `LowCardinality(SAF(anyLast, Nullable(String)))` is correctly nullable and a
    // null row is not rejected as an `InconsistentBatch`.
    let nullable_at_this_level = matches!(physical_type, ChType::Nullable(_))
        || matches!(physical_type, ChType::LowCardinality(inner) if low_cardinality_dict_value_type(inner).0);
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

    if let (ChType::LowCardinality(inner), Column::Dictionary(c)) = (physical_type, column) {
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
        // Wide integers are verbatim fixed-width bodies (16 bytes/row for the
        // 128-bit pair, 32 for the 256-bit pair) with the same misframe guard as
        // UUID/IPv6: reject a stored width that disagrees with the type and any
        // body length != width * num_rows before writing.
        (ChType::Int128, Column::Int128(c)) | (ChType::UInt128, Column::UInt128(c)) => {
            validate_fixed_binary(field, value_type, c, 16, num_rows)?;
        }
        (ChType::Int256, Column::Int256(c)) | (ChType::UInt256, Column::UInt256(c)) => {
            validate_fixed_binary(field, value_type, c, 32, num_rows)?;
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

/// Follow a name-decoration alias chain to its underlying physical type,
/// returning `None` when `ch_type` is already physical (not an alias).
///
/// `SimpleAggregateFunction`, the geo aliases, and `Nested` each delegate to a
/// physical type via [`ChType::physical_delegate`]; this resolves the whole
/// chain (e.g. `SimpleAggregateFunction(_, Point)` -> `Geo(Point)` -> `Tuple`)
/// rather than a fixed number of steps, so [`validate_column`] never stops one
/// delegate short. The clone is bounded by the parsed/validated type depth and
/// runs once per column validation, never per row.
fn resolve_delegate(ch_type: &ChType) -> Option<ChType> {
    let mut under = ch_type.physical_delegate()?;
    while let Some(next) = under.physical_delegate() {
        under = next;
    }
    Some(under)
}

/// Reject a `SimpleAggregateFunction` whose function-name spelling is not a bare
/// identifier optionally followed by a single balanced parenthesized parameter
/// suffix (`sum`, `anyLast`, `groupArrayLastArray(5)`), walking every wrapper and
/// container so a nested SAF is checked too.
///
/// Two deliberate decisions, matching the crate's trusted-encode-input
/// precedent:
///
/// - The `func` spelling IS validated (via the shared
///   [`is_simple_aggregate_func_spelling`], the exact predicate the decode parser
///   uses), because it is Displayed verbatim into the header's type-string
///   channel. Without this, a caller-constructed `func` like
///   `"sum, UInt64), evil"` would inject extra type tokens, the same injection
///   class the Tuple element-name validation prevents.
/// - The server's function whitelist is NOT enforced. The list grows across
///   versions, the server rejects an unknown function loudly on INSERT, and this
///   mirrors the same choice made for `DateTime64` precision and `Enum` values:
///   the crate validates wire framing, not semantic legality the server owns.
///
/// Bounded by the [`MAX_TYPE_DEPTH`] check that runs before it in
/// [`validate_column`], so the recursion cannot run away on a hostile type.
fn validate_saf_func_spellings(field: &Field, ch_type: &ChType) -> Result<(), EncodeError> {
    match ch_type {
        ChType::SimpleAggregateFunction { func, inner } => {
            if !is_simple_aggregate_func_spelling(func) {
                return Err(EncodeError::UnsupportedType {
                    column: field.name.clone(),
                    ch_type: field.ch_type.clone(),
                });
            }
            validate_saf_func_spellings(field, inner)
        }
        ChType::Nullable(inner) | ChType::LowCardinality(inner) | ChType::Array(inner) => {
            validate_saf_func_spellings(field, inner)
        }
        ChType::Tuple(elements) => {
            for (_, element_type) in elements {
                validate_saf_func_spellings(field, element_type)?;
            }
            Ok(())
        }
        ChType::Nested(fields) => {
            for (_, field_type) in fields {
                validate_saf_func_spellings(field, field_type)?;
            }
            Ok(())
        }
        ChType::Map(key, value) => {
            validate_saf_func_spellings(field, key)?;
            validate_saf_func_spellings(field, value)
        }
        _ => Ok(()),
    }
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
            // Name-decoration aliases contribute the depth of the physical type
            // they delegate to, so a geo/Nested alias near the cap is not
            // under-counted (a MultiPolygon expands to four Array/Tuple levels).
            // `SimpleAggregateFunction` charges one level (parse it at `depth + 1`
            // for its inner), matching the decode parser: SAF expands via one
            // extra decode recursion frame, and charging it the same on both sides
            // keeps decode-accept and encode-accept in exact agreement while
            // bounding a hostile chain of nested SAFs. `Nested` expands to
            // `Array(Tuple(fields))`, two wrapper levels above each field. Geo
            // expands to a fixed constant nesting ([`GeoKind::expansion_depth`],
            // the depth of `underlying_type`), so its token is charged that many
            // levels directly, the same constant the decode parser charges.
            ChType::SimpleAggregateFunction { inner, .. } => {
                work.push((inner, depth + 1));
            }
            ChType::Nested(fields) => {
                for (_, field_type) in fields {
                    work.push((field_type, depth + 2));
                }
            }
            ChType::Geo(kind) => {
                max_depth = max_depth.max(depth + kind.expansion_depth());
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
    // Resolve the inner through the shared helper (full SAF chain + optional
    // removeNullable Nullable + inner SAF chain), so the dictionary body and index
    // nullability are those of the physical inner for both
    // `LowCardinality(SAF(anyLast, Nullable(String)))` and a chained SAF.
    let (nullable, dict_value_type) = low_cardinality_dict_value_type(inner);

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
    // A name-decoration alias (SimpleAggregateFunction, geo, Nested) matches the
    // column its physical delegate would, since decode produces the delegate's
    // Column variant (no new variant). Expand and recurse before the pair checks
    // below.
    if let Some(under) = value_type.physical_delegate() {
        return column_variant_matches(&under, column);
    }
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
            | (ChType::Time, Column::Time(_))
            | (ChType::Time64 { .. }, Column::Time64(_))
            | (ChType::Uuid, Column::Uuid(_))
            | (ChType::Ipv4, Column::Ipv4(_))
            | (ChType::Ipv6, Column::Ipv6(_))
            | (ChType::String, Column::Utf8(_))
            | (ChType::FixedString(_), Column::FixedBinary(_))
            | (ChType::Enum8 { .. }, Column::Enum8(_))
            | (ChType::Enum16 { .. }, Column::Enum16(_))
            | (ChType::Decimal { .. }, Column::Decimal(_))
            | (ChType::Int128, Column::Int128(_))
            | (ChType::UInt128, Column::UInt128(_))
            | (ChType::Int256, Column::Int256(_))
            | (ChType::UInt256, Column::UInt256(_))
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
    // A name-decoration alias (SimpleAggregateFunction, geo, Nested) writes the
    // exact state prefix of the type it delegates to, so expand and recurse, the
    // encode-side mirror of `decode::read_state_prefix`.
    if let Some(under) = ch_type.physical_delegate() {
        write_state_prefix(buf, &under);
        return;
    }
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
    // A name-decoration alias (SimpleAggregateFunction, geo, Nested) encodes
    // exactly as the physical type it delegates to, the encode-side mirror of
    // `decode::decode_values`. Expand and recurse before the container dispatch
    // so a geo/Nested alias that expands to an `Array` reaches the Array
    // fast-path.
    if let Some(under) = ch_type.physical_delegate() {
        return encode_column_values(buf, field, &under, column);
    }
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
    // Expand a geo alias legal directly inside `Nullable` (only `Nullable(Point)`
    // -> `Tuple`), so the Tuple arm below writes its body after the null map.
    let delegate = value_type.physical_delegate();
    let value_type = delegate.as_ref().unwrap_or(value_type);
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
    // Resolve the inner through the shared helper (the encode mirror of
    // [`super::decode::decode_low_cardinality`]), so the dictionary body is
    // written as its fully-stripped physical value type. Without the full SAF
    // strip a chained SAF would leave an alias here and die in
    // `encode_column_body`'s default arm as an `InconsistentBatch`.
    // `validate_low_cardinality` already confirmed the inner is legal.
    let (_, dict_value_type) = low_cardinality_dict_value_type(inner);
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
        (ChType::Time, Column::Time(c)) => encode_primitive!(buf, &c.values, i32),
        (ChType::Time64 { .. }, Column::Time64(c)) => {
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
        // Wide-int bodies are the contiguous little-endian fixed-width bytes
        // written verbatim from the width-16/32 fixed-binary buffer, the inverse
        // of the decoder's passthrough arms and byte-identical to a
        // Decimal128/256 body. No reordering, no host byteswap; signedness is in
        // the type string only. `validate_column` already confirmed the width and
        // that `data.len() == width * num_rows`, so this is a single copy.
        (ChType::Int128, Column::Int128(c))
        | (ChType::UInt128, Column::UInt128(c))
        | (ChType::Int256, Column::Int256(c))
        | (ChType::UInt256, Column::UInt256(c)) => encode_fixed_binary_data(buf, c),
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
            // Resolve through the shared helper (full SAF chain + optional
            // Nullable + inner SAF chain) so a
            // `LowCardinality(SAF(anyLast, Nullable(String)))` is not
            // misclassified as unencodable: its physical dictionary value type is
            // what must be an allowed and encodable LC inner.
            let (_, dict_value_type) = low_cardinality_dict_value_type(inner);
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
        | ChType::Time
        | ChType::Time64 { .. }
        | ChType::Uuid
        | ChType::Ipv4
        | ChType::Ipv6
        | ChType::String
        | ChType::FixedString(_)
        | ChType::Enum8 { .. }
        | ChType::Enum16 { .. }
        | ChType::Decimal { .. }
        | ChType::Int128
        | ChType::UInt128
        | ChType::Int256
        | ChType::UInt256 => true,
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
        // Name-decoration aliases are encodable exactly when their physical
        // delegate is: `SimpleAggregateFunction` over its inner, a geo alias over
        // its Tuple/Array-of-Float64 nesting (always encodable), and `Nested`
        // over its `Array(Tuple(fields))` (encodable when every field type is, a
        // Nullable field unwrapping like the Tuple arm). The SAF inner unwraps a
        // `Nullable` via `.inner()` exactly like the Array/Tuple/Nested arms, so
        // `SimpleAggregateFunction(anyLast, Nullable(String))` is not misclassified
        // as unencodable (a bare `is_encodable(Nullable(_))` is always false).
        ChType::SimpleAggregateFunction { inner, .. } => is_encodable(inner.inner()),
        ChType::Geo(kind) => is_encodable(&kind.underlying_type()),
        ChType::Nested(fields) => fields.iter().all(|(_, t)| is_encodable(t.inner())),
        ChType::Nullable(_) => false,
    }
}

#[cfg(test)]
mod tests;
