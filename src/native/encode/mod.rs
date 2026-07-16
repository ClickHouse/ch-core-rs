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
//! Scope: this encodes `Nothing`, `Bool`, the fixed-width numeric types (`Int8`..`Int64`,
//! `UInt8`..`UInt64`, `Float32`, `Float64`, `BFloat16`), the temporal types (`Date`,
//! `Date32`, `DateTime`, `DateTime64`, `Time`, `Time64`, and all 11
//! `Interval*` kinds), `UUID`, `IPv4`, `IPv6`, `String`, `FixedString(N)`,
//! `Enum8`/`Enum16`, `Decimal(P, S)`, the
//! wide integers (`Int128`/`UInt128`/`Int256`/`UInt256`), `LowCardinality(T)`
//! for the allowed inner types this crate decodes, `Array(T)` over any
//! encodable element type (including nested arrays), `Tuple(T1, ...)`
//! (named or unnamed, including the zero-element `Tuple()`) over encodable
//! element types, and `Map(K, V)` for a legal key type and any encodable
//! key/value types, `Variant(T1, ...)` when every alternative is encodable,
//! `Dynamic` with block-local typed children and optional SharedVariant,
//! plus the registered exact `AggregateFunction` state codecs:
//! `count`, canonical `nothingUInt64` and `nothingNull`, and base `sum` over one
//! plain or Nullable numeric or Enum argument. The plain types and `Tuple` also
//! compose inside a `Nullable(T)` wrapper (a per-row null map precedes the inner
//! values). Every
//! other column type returns [`EncodeError::UnsupportedType`] until its encoder
//! lands, the same one-type-at-a-time growth the decode path follows.

use crate::batch::{ChunkedBatch, ColBatch};
use crate::column::{
    ArrayColumn, BoolColumn, Column, DecimalColumn, DictionaryColumn, DynamicChild, DynamicColumn,
    FixedBinaryColumn, MapColumn, TupleColumn, Utf8Column, VariantColumn,
};
use crate::native::aggregate_function::aggregate_state_codec;
use crate::schema::{ChType, Field};

use super::protocol::{
    DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION, DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS,
    DBMS_MIN_REVISION_WITH_V2_DYNAMIC_AND_JSON_SERIALIZATION, LC_HAS_ADDITIONAL_KEYS_BIT,
    LC_NEED_UPDATE_DICTIONARY_BIT, LOW_CARDINALITY_KEY_VERSION,
};
use super::type_binary::write_binary_type;
use super::type_parser::{
    is_low_cardinality_inner, is_valid_map_key_type, low_cardinality_dict_value_type,
};
use super::varint::write_varint;

mod validate;
use validate::validate_block;

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
    /// Opt in to Dynamic's FLATTENED serialization (structure word 3) for a
    /// column with no `SharedVariant` child. Servers before ClickHouse 25.6 do
    /// not know structure word 3 and reject it, so this defaults to `false`: a
    /// shared-less column then encodes with the V1/V2 layout, emitting an
    /// implicit empty `SharedVariant` child (its dense body is zero rows, so no
    /// bytes). A column that carries a real `SharedVariant` child always uses
    /// V1/V2 regardless of this flag, since FLATTENED cannot represent it.
    pub flattened_dynamic: bool,
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
    encode_block_into(&mut buf, batch, options, false)?;
    Ok(buf)
}

/// Encode one block using the server's binary data-type descriptors for outer
/// type headers and Dynamic runtime type tables. This mirrors the out-of-band
/// `output_format_native_encode_types_in_binary_format` setting without adding
/// a field to the backward-compatible [`EncodeOptions`] struct.
pub fn encode_block_binary_types(
    batch: &ColBatch,
    options: &EncodeOptions,
) -> Result<Vec<u8>, EncodeError> {
    let mut buf = Vec::new();
    encode_block_into(&mut buf, batch, options, true)?;
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
        validate_block(chunk, options)?;
    }
    let mut buf = Vec::new();
    for chunk in &batch.chunks {
        write_block_into(&mut buf, chunk, options, false)?;
    }
    Ok(buf)
}

/// Encode all chunks with binary data-type descriptors. See
/// [`encode_block_binary_types`].
pub fn encode_chunked_binary_types(
    batch: &ChunkedBatch,
    options: &EncodeOptions,
) -> Result<Vec<u8>, EncodeError> {
    for (i, chunk) in batch.chunks.iter().enumerate() {
        if chunk.schema != batch.schema {
            return Err(EncodeError::InconsistentBatch {
                detail: format!("chunk {i} schema differs from the batch schema"),
            });
        }
        validate_block(chunk, options)?;
    }
    let mut buf = Vec::new();
    for chunk in &batch.chunks {
        write_block_into(&mut buf, chunk, options, true)?;
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
    types_in_binary_format: bool,
) -> Result<(), EncodeError> {
    validate_block(batch, options)?;
    write_block_into(buf, batch, options, types_in_binary_format)
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
    types_in_binary_format: bool,
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
        if types_in_binary_format {
            write_binary_type(buf, &field.ch_type);
        } else {
            write_string(buf, field.ch_type.to_string().as_bytes());
        }
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
            encode_column_data(buf, field, column, options, types_in_binary_format)?;
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
    options: &EncodeOptions,
    types_in_binary_format: bool,
) -> Result<(), EncodeError> {
    write_state_prefix(buf, &field.ch_type, column, options, types_in_binary_format)?;
    encode_column_values(buf, field, &field.ch_type, column, options)?;
    write_state_suffix(buf, &field.ch_type, column)?;
    Ok(())
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
fn write_state_prefix(
    buf: &mut Vec<u8>,
    ch_type: &ChType,
    column: &Column,
    options: &EncodeOptions,
    types_in_binary_format: bool,
) -> Result<(), EncodeError> {
    // A name-decoration alias (SimpleAggregateFunction, geo, Nested) writes the
    // exact state prefix of the type it delegates to, so expand and recurse, the
    // encode-side mirror of `decode::read_state_prefix`.
    if let Some(under) = ch_type.physical_delegate() {
        return write_state_prefix(buf, &under, column, options, types_in_binary_format);
    }
    match ch_type {
        ChType::LowCardinality(_) => {
            buf.extend_from_slice(&LOW_CARDINALITY_KEY_VERSION.to_le_bytes());
        }
        ChType::Array(inner) => {
            if let Column::Array(col) = column {
                write_state_prefix(
                    buf,
                    inner,
                    col.values.as_ref(),
                    options,
                    types_in_binary_format,
                )?;
            }
        }
        // `SerializationTuple::serializeBinaryBulkStatePrefix` writes nothing of
        // its own and delegates to every element in declaration order (confirmed
        // at v26.6.1.1193-stable), so a LowCardinality element's key version is
        // hoisted to the front of the whole Tuple column, before any element
        // bodies, in element order.
        ChType::Tuple(elements) => {
            if let Column::Tuple(col) = column {
                for ((_, element_type), element) in elements.iter().zip(&col.fields) {
                    write_state_prefix(
                        buf,
                        element_type,
                        element,
                        options,
                        types_in_binary_format,
                    )?;
                }
            }
        }
        // `SerializationMap` delegates through its nested Array(Tuple(...)),
        // so the chain is Map -> Array (nothing) -> Tuple -> key's prefix then
        // value's prefix (confirmed at v26.6.1.1193-stable). A
        // Map(LowCardinality(String), V) therefore hoists the LC key version
        // to the very front of the whole column, before the offsets.
        ChType::Map(key, value) => {
            if let Column::Map(col) = column {
                if let Column::Tuple(entries) = col.entries.as_ref() {
                    if let [keys, values] = entries.fields.as_slice() {
                        write_state_prefix(buf, key, keys, options, types_in_binary_format)?;
                        write_state_prefix(buf, value, values, options, types_in_binary_format)?;
                    }
                }
            }
        }
        // Variant's own prefix is one fixed-width LE UInt64 discriminator mode.
        // Direct FORMAT Native always uses BASIC mode 0, then delegates to every
        // alternative's prefix in canonical/global discriminator order.
        ChType::Variant(alternatives) => {
            buf.extend_from_slice(&0u64.to_le_bytes());
            if let Column::Variant(col) = column {
                for (alternative, child) in alternatives.iter().zip(&col.variants) {
                    write_state_prefix(buf, alternative, child, options, types_in_binary_format)?;
                }
            }
        }
        ChType::Dynamic { .. } => {
            if let Column::Dynamic(col) = column {
                write_dynamic_state_prefix(buf, col, options, types_in_binary_format)?;
            }
        }
        // `SerializationNullable::serializeBinaryBulkStatePrefix` delegates to
        // the nested type (confirmed at v26.6.1.1193-stable); only a
        // `Nullable(Tuple(...))` can nest a prefix-bearing type today.
        ChType::Nullable(inner) => {
            write_state_prefix(buf, inner, column, options, types_in_binary_format)?
        }
        _ => {}
    }
    Ok(())
}

/// Write Dynamic's self-describing bulk-state prefix and every discovered
/// child's prefix. A column with a real `SharedVariant` child uses direct V1/V2
/// with the protocol-revision-selected structure word. A shared-less column
/// uses FLATTENED word 3 (table order = physical child order) only when
/// [`EncodeOptions::flattened_dynamic`] is set; otherwise it takes the V1/V2
/// layout with an implicit empty `SharedVariant`, writing the type table and
/// child prefixes in the canonical global order the decoder derives. The child
/// bodies remain bulk columns, so only the small type table is visited here and
/// no per-row work is introduced.
fn write_dynamic_state_prefix(
    buf: &mut Vec<u8>,
    col: &DynamicColumn,
    options: &EncodeOptions,
    types_in_binary_format: bool,
) -> Result<(), EncodeError> {
    match dynamic_wire_shape(col, options) {
        DynamicWireShape::Flattened => {
            buf.extend_from_slice(&3u64.to_le_bytes());
            write_varint(buf, col.children.len() as u64);
            for child in &col.children {
                if let DynamicChild::Typed { ch_type, .. } = child {
                    write_dynamic_type_entry(buf, ch_type, types_in_binary_format);
                }
            }
            for child in &col.children {
                if let DynamicChild::Typed { ch_type, values } = child {
                    write_state_prefix(buf, ch_type, values, options, types_in_binary_format)?;
                }
            }
        }
        DynamicWireShape::V1V2 => {
            let structure = if options.protocol_revision
                < DBMS_MIN_REVISION_WITH_V2_DYNAMIC_AND_JSON_SERIALIZATION
            {
                1u64
            } else {
                2u64
            };
            buf.extend_from_slice(&structure.to_le_bytes());

            // For a real SharedVariant child, `validate_dynamic` proved the
            // children are already in canonical global order, so the wire
            // order is the memory order. For the implicit shared child, the
            // order helper interleaves the virtual "SharedVariant" slot into
            // the sorted typed names, the exact order the decoder rebuilds.
            let order = dynamic_wire_order(col);
            let direct_count = col
                .children
                .iter()
                .filter(|child| matches!(child, DynamicChild::Typed { .. }))
                .count();
            if structure == 1 {
                // V1's first count is a legacy ignored slot. The current server
                // writes the direct-type count into both positions.
                write_varint(buf, direct_count as u64);
            }
            write_varint(buf, direct_count as u64);
            for slot in &order {
                if let Some(DynamicChild::Typed { ch_type, .. }) =
                    slot.map(|index| &col.children[index])
                {
                    write_dynamic_type_entry(buf, ch_type, types_in_binary_format);
                }
            }
            // Direct Native Dynamic always uses Variant BASIC mode.
            buf.extend_from_slice(&0u64.to_le_bytes());

            for slot in &order {
                if let Some(DynamicChild::Typed { ch_type, values }) =
                    slot.map(|index| &col.children[index])
                {
                    write_state_prefix(buf, ch_type, values, options, types_in_binary_format)?;
                }
            }
        }
    }
    Ok(())
}

/// The Dynamic wire representation selected for a column under `options`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DynamicWireShape {
    V1V2,
    Flattened,
}

fn dynamic_wire_shape(col: &DynamicColumn, options: &EncodeOptions) -> DynamicWireShape {
    if col.shared_child_index().is_none() && options.flattened_dynamic {
        DynamicWireShape::Flattened
    } else {
        DynamicWireShape::V1V2
    }
}

/// The V1/V2 global wire order of a column's children: one slot per wire
/// discriminator, holding `Some(memory index)` for a stored child and `None`
/// for the implicit empty `SharedVariant` of a shared-less column.
///
/// The decoder rebuilds the global order by sorting the typed canonical names
/// together with `"SharedVariant"`, so a shared-less column (whose FLATTENED
/// table order is not necessarily sorted) must be permuted here. A column with
/// a real shared child was validated to already be in this order, so its
/// mapping comes out the identity.
fn dynamic_wire_order(col: &DynamicColumn) -> Vec<Option<usize>> {
    let mut order: Vec<(String, Option<usize>)> = col
        .children
        .iter()
        .enumerate()
        .map(|(index, child)| {
            let name = match child {
                DynamicChild::Typed { ch_type, .. } => ch_type.to_string(),
                DynamicChild::Shared(_) => "SharedVariant".to_string(),
            };
            (name, Some(index))
        })
        .collect();
    if col.shared_child_index().is_none() {
        order.push(("SharedVariant".to_string(), None));
    }
    order.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    order.into_iter().map(|(_, slot)| slot).collect()
}

fn write_dynamic_type_entry(buf: &mut Vec<u8>, ch_type: &ChType, types_in_binary_format: bool) {
    if types_in_binary_format {
        write_binary_type(buf, ch_type);
    } else {
        write_string(buf, ch_type.to_string().as_bytes());
    }
}

/// Write the per-column bulk-state suffix after the value body.
///
/// Variant owns no suffix bytes and delegates to every alternative in canonical
/// order. No currently supported type writes suffix bytes, but keeping this
/// traversal symmetric with decode and the server preserves the correct
/// insertion point for a future suffix-bearing type.
fn write_state_suffix(
    buf: &mut Vec<u8>,
    ch_type: &ChType,
    column: &Column,
) -> Result<(), EncodeError> {
    if let Some(under) = ch_type.physical_delegate() {
        return write_state_suffix(buf, &under, column);
    }
    match ch_type {
        ChType::Array(inner) => {
            if let Column::Array(col) = column {
                write_state_suffix(buf, inner, col.values.as_ref())?;
            }
        }
        ChType::Nullable(inner) => write_state_suffix(buf, inner, column)?,
        ChType::Tuple(elements) => {
            if let Column::Tuple(col) = column {
                for ((_, element_type), element) in elements.iter().zip(&col.fields) {
                    write_state_suffix(buf, element_type, element)?;
                }
            }
        }
        ChType::Map(key, value) => {
            if let Column::Map(col) = column {
                if let Column::Tuple(entries) = col.entries.as_ref() {
                    if let [keys, values] = entries.fields.as_slice() {
                        write_state_suffix(buf, key, keys)?;
                        write_state_suffix(buf, value, values)?;
                    }
                }
            }
        }
        ChType::Variant(alternatives) => {
            if let Column::Variant(col) = column {
                for (alternative, child) in alternatives.iter().zip(&col.variants) {
                    write_state_suffix(buf, alternative, child)?;
                }
            }
        }
        ChType::Dynamic { .. } => {
            if let Column::Dynamic(col) = column {
                // Same wire order as the prefix and body traversals.
                for slot in dynamic_wire_order(col) {
                    if let Some(DynamicChild::Typed { ch_type, values }) =
                        slot.map(|index| &col.children[index])
                    {
                        write_state_suffix(buf, ch_type, values)?;
                    }
                }
            }
        }
        _ => {
            // Intentionally a no-op: no type this crate currently supports writes
            // suffix bytes. The arm exists only to keep this traversal symmetric
            // with the server's prefix/body/suffix serialization contract.
            let _ = buf;
        }
    }
    Ok(())
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
    options: &EncodeOptions,
) -> Result<(), EncodeError> {
    // A name-decoration alias (SimpleAggregateFunction, geo, Nested) encodes
    // exactly as the physical type it delegates to, the encode-side mirror of
    // `decode::decode_values`. Expand and recurse before the container dispatch
    // so a geo/Nested alias that expands to an `Array` reaches the Array
    // fast-path.
    if let Some(under) = ch_type.physical_delegate() {
        return encode_column_values(buf, field, &under, column, options);
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
            return encode_array_data(buf, field, inner, c, options);
        }
        return Err(column_error(field, ch_type));
    }
    if let ChType::Map(key, value) = ch_type {
        if let Column::Map(c) = column {
            return encode_map_data(buf, field, ch_type, key, value, c, options);
        }
        return Err(column_error(field, ch_type));
    }
    if let ChType::Variant(alternatives) = ch_type {
        if let Column::Variant(c) = column {
            return encode_variant_data(buf, field, alternatives, c, options);
        }
        return Err(column_error(field, ch_type));
    }
    if let ChType::Dynamic { .. } = ch_type {
        if let Column::Dynamic(c) = column {
            return encode_dynamic_data(buf, field, c, options);
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
            return encode_tuple_data(buf, field, elements, c, options);
        }
        return Err(column_error(field, value_type));
    }
    encode_column_body(buf, field, value_type, column)
}

/// Encode one Dynamic body after [`write_dynamic_state_prefix`] has emitted its
/// block-local type table. V1/V2 writes one UInt8 discriminator per row, then
/// dense child bodies in global order; a shared-less column's implicit
/// `SharedVariant` slot maps through the same [`dynamic_wire_order`] the prefix
/// used, and its zero-row dense body writes no bytes. FLATTENED (opt-in) writes
/// its smallest fixed-width index run, then the dense typed bodies. The stored
/// offsets are routing-only and validation has already proved they are
/// occurrence ordinals, so encoding touches only `type_ids` once and performs
/// one bulk write per child.
fn encode_dynamic_data(
    buf: &mut Vec<u8>,
    field: &Field,
    col: &DynamicColumn,
    options: &EncodeOptions,
) -> Result<(), EncodeError> {
    match dynamic_wire_shape(col, options) {
        DynamicWireShape::V1V2 => {
            let order = dynamic_wire_order(col);
            // Memory child index -> wire discriminator. The identity for a
            // column with a real shared child (validated canonical order); a
            // permutation over the implicit SharedVariant slot otherwise.
            let mut discriminator_of = vec![u8::MAX; col.children.len()];
            for (discriminator, slot) in order.iter().enumerate() {
                if let Some(index) = slot {
                    discriminator_of[*index] = u8::try_from(discriminator)
                        .map_err(|_| column_error(field, &field.ch_type))?;
                }
            }
            buf.reserve(col.type_ids.len());
            for &type_id in &col.type_ids {
                if type_id == u32::MAX {
                    buf.push(u8::MAX);
                } else {
                    let discriminator = discriminator_of
                        .get(type_id as usize)
                        .copied()
                        .ok_or_else(|| column_error(field, &field.ch_type))?;
                    buf.push(discriminator);
                }
            }
            for slot in &order {
                match slot.map(|index| &col.children[index]) {
                    Some(DynamicChild::Typed { ch_type, values }) => {
                        encode_column_values(buf, field, ch_type, values, options)?;
                    }
                    Some(DynamicChild::Shared(values)) => encode_string_data(buf, values),
                    // The implicit empty SharedVariant: a zero-row String body
                    // writes no bytes, matching the decoder's zero-count read.
                    None => {}
                }
            }
        }
        DynamicWireShape::Flattened => {
            let null_index = col.children.len() as u64;
            let width = dynamic_flat_index_width(col.children.len());
            buf.reserve(col.type_ids.len().saturating_mul(width));
            for &type_id in &col.type_ids {
                let index = if type_id == u32::MAX {
                    null_index
                } else {
                    u64::from(type_id)
                };
                match width {
                    1 => buf.push(index as u8),
                    2 => buf.extend_from_slice(&(index as u16).to_le_bytes()),
                    4 => buf.extend_from_slice(&(index as u32).to_le_bytes()),
                    8 => buf.extend_from_slice(&index.to_le_bytes()),
                    _ => unreachable!("Dynamic index width is selected from 1/2/4/8"),
                }
            }
            for child in &col.children {
                match child {
                    DynamicChild::Typed { ch_type, values } => {
                        encode_column_values(buf, field, ch_type, values, options)?;
                    }
                    DynamicChild::Shared(values) => encode_string_data(buf, values),
                }
            }
        }
    }
    Ok(())
}

fn dynamic_flat_index_width(num_children: usize) -> usize {
    if num_children <= u8::MAX as usize {
        1
    } else if num_children <= u16::MAX as usize {
        2
    } else if num_children <= u32::MAX as usize {
        4
    } else {
        8
    }
}

/// Encode one BASIC `Variant(T1, ...)` body.
///
/// The mode word and every alternative's state prefix were written by
/// [`write_state_prefix`]. The body is one global UInt8 discriminator per row,
/// followed by each alternative's dense child body in canonical order. NULL is
/// discriminator 255 and has no child body. Arrow routing offsets are not on the
/// wire.
///
/// [`VariantColumn::discriminators`] is exactly that wire run (one byte per
/// logical row, 255 = NULL), so the discriminator body is a single
/// `extend_from_slice` with no per-row reconstruction from the Arrow routing
/// buffers. `validate_variant` has already proved those routing buffers are the
/// canonical layout derived from these discriminators and that each child length
/// matches its discriminator count, so this then performs one bulk child write
/// per alternative in canonical order, with no allocation.
fn encode_variant_data(
    buf: &mut Vec<u8>,
    field: &Field,
    alternatives: &[ChType],
    col: &VariantColumn,
    options: &EncodeOptions,
) -> Result<(), EncodeError> {
    buf.extend_from_slice(col.discriminators());
    for (alternative, child) in alternatives.iter().zip(&col.variants) {
        encode_column_values(buf, field, alternative, child, options)?;
    }
    Ok(())
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
    options: &EncodeOptions,
) -> Result<(), EncodeError> {
    // `get(1..)` rather than `[1..]`: validation guarantees the leading 0
    // exists, but stay panic-free if a caller reaches this without validating.
    if let Some(end_offsets) = col.offsets.get(1..) {
        encode_primitive!(buf, end_offsets, i64);
    }
    match col.entries.as_ref() {
        Column::Tuple(entries) if entries.fields.len() == 2 => {
            encode_column_values(buf, field, key, &entries.fields[0], options)?;
            encode_column_values(buf, field, value, &entries.fields[1], options)
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
    options: &EncodeOptions,
) -> Result<(), EncodeError> {
    if elements.is_empty() {
        buf.resize(buf.len() + col.len, b'0');
        return Ok(());
    }
    for ((_, element_type), element_col) in elements.iter().zip(&col.fields) {
        encode_column_values(buf, field, element_type, element_col, options)?;
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
    options: &EncodeOptions,
) -> Result<(), EncodeError> {
    // `get(1..)` rather than `[1..]`: validation guarantees the leading 0
    // exists, but stay panic-free if a caller reaches this without validating
    // (the same defensive posture as `encode_column_body`'s fall-through arm).
    if let Some(end_offsets) = col.offsets.get(1..) {
        encode_primitive!(buf, end_offsets, i64);
    }
    encode_column_values(buf, field, inner, col.values.as_ref(), options)
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
        // At v26.6.1.1193-stable,
        // `SerializationNothing::serializeBinaryBulk` in
        // `src/DataTypes/Serializations/SerializationNothing.cpp` writes one
        // ASCII '0' placeholder byte per row. The decoder accepts arbitrary
        // placeholder values, but encode uses the server's canonical byte.
        (ChType::Nothing, Column::Nothing(c)) => {
            buf.resize(buf.len() + c.len, b'0');
        }
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
        // BFloat16 has no stable Rust primitive and Arrow has no native
        // BFloat16 type. Each `[u8; 2]` holds one exact little-endian wire word,
        // so write the contiguous array buffer without conversion.
        (ChType::BFloat16, Column::BFloat16(c)) => encode_bfloat16_data(buf, c),
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
        // SerializationInterval writes the same contiguous signed Int64 body
        // for every IntervalKind; the exact unit is carried by the type string.
        (ChType::Interval(_), Column::Interval(c)) => {
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
        // AggregateStateColumn stores the exact serialized row states in one
        // contiguous buffer. Validation has already checked every offset and
        // state with the selected function-specific codec, so this is one copy.
        (ChType::AggregateFunction { .. }, Column::AggregateState(c)) => {
            buf.extend_from_slice(&c.data)
        }
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

/// Encode exact BFloat16 words from a structurally width-2 row buffer.
fn encode_bfloat16_data(buf: &mut Vec<u8>, col: &crate::column::PrimitiveColumn<[u8; 2]>) {
    buf.extend_from_slice(col.values.as_flattened());
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
///
/// This predicate answers "does a body writer exist for this type", a question
/// distinct from "is this type-string legal" (owned by
/// [`unsupported_header_type_name`]). It is a self-contained truth about encoder
/// coverage that [`column_error`] must be able to call in isolation, so its
/// `LowCardinality`-inner and `Map`-key arms re-spell those legality rules
/// rather than delegating to that walker: routing through it would couple this
/// predicate to validation ordering and conflate encodable with legal.
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
        ChType::Nothing
        | ChType::Bool
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
        | ChType::BFloat16
        | ChType::Date
        | ChType::Date32
        | ChType::DateTime { .. }
        | ChType::DateTime64 { .. }
        | ChType::Time
        | ChType::Time64 { .. }
        | ChType::Interval(_)
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
        // Variant writes one discriminator run around dense child bodies, so it
        // is encodable exactly when every canonical alternative is encodable.
        ChType::Variant(alternatives) => alternatives.iter().all(is_encodable),
        // Dynamic's direct child types are carried by each block rather than by
        // the logical schema. Their recursive encodability is checked against
        // the concrete DynamicColumn during validation.
        ChType::Dynamic { .. } => true,
        ChType::AggregateFunction { .. } => aggregate_state_codec(ch_type).is_some(),
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
