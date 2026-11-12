use std::borrow::Cow;
use std::io;
use std::sync::Arc;

use crate::batch::{ChunkedBatch, ColBatch};
use crate::bitmap::Bitmap;
use crate::column::{
    variant_child_counts, variant_layout_from_discriminators, AggregateStateColumn, ArrayColumn,
    BoolColumn, Column, DecimalColumn, DictionaryColumn, DynamicChild, DynamicColumn,
    FixedBinaryColumn, JsonColumn, MapColumn, NothingColumn, PrimitiveColumn, QBitColumn,
    StructuredJson, TupleColumn, Utf8Column, VariantColumn, VariantColumnError,
};
use crate::native::aggregate_function::{
    decode_aggregate_states, decode_state_codec, scan_aggregate_states, AggregateStateCodec,
};
use crate::native::qbit::{transpose8, QBitWord};
use crate::native::varint::ByteReader;
use crate::schema::{ChType, Field, QBitElementType, Schema};

pub use crate::native::protocol::{
    DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION,
    DBMS_MIN_REVISION_WITH_STRING_WITH_SIZE_STREAM_SERIALIZATION, DBMS_TCP_PROTOCOL_VERSION,
};
use crate::native::protocol::{
    LC_HAS_ADDITIONAL_KEYS_BIT, LC_NEED_GLOBAL_DICTIONARY_BIT, LOW_CARDINALITY_KEY_VERSION,
    MAX_TYPE_DEPTH,
};
use crate::native::type_binary::{read_binary_type, BinaryTypeError, BinaryTypeScanner};
use crate::native::type_parser::{
    is_low_cardinality_inner, is_valid_variant_alternative, resolves_to_nothing,
    unsupported_header_type_name,
};
pub use crate::native::type_parser::{low_cardinality_dict_value_type, parse_ch_type};

/// Errors that can occur during Native format decoding.
#[derive(Debug)]
#[non_exhaustive]
pub enum DecodeError {
    Io(io::Error),
    /// The caller supplied a protocol revision newer than the highest revision
    /// whose Native framing this decoder fully supports.
    UnsupportedProtocolRevision {
        revision: u64,
        max_supported: u64,
    },
    UnsupportedType {
        column: String,
        type_name: String,
    },
    /// A `BlockInfo` preamble carried a field number this decoder does not know.
    /// The server rejects unknown field numbers the same way.
    InvalidBlockInfo {
        field_num: u64,
    },
    /// A `BlockInfo` preamble repeated a known field. The server writer emits
    /// every field exactly once, so repeats are rejected as non-canonical.
    DuplicateBlockInfo {
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
    /// A `Variant` column carried a discriminator mode, discriminator byte, or
    /// dense child layout that is invalid for direct Native serialization.
    InvalidVariant {
        column: String,
        reason: String,
    },
    /// A Dynamic column carried an invalid structure version, runtime type
    /// table, discriminator/index, or child layout.
    InvalidDynamic {
        column: String,
        reason: String,
    },
    /// A JSON column carried an invalid structure version, dynamic-path list,
    /// path count, or shared-data layout.
    InvalidJson {
        column: String,
        reason: String,
    },
    /// A block's cumulative synthesized-buffer allocation would exceed a
    /// resource ceiling.
    ResourceLimit {
        limit: usize,
        requested: usize,
        what: &'static str,
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
            DecodeError::UnsupportedProtocolRevision {
                revision,
                max_supported,
            } => write!(
                f,
                "Unsupported protocol revision {revision}; maximum supported revision is {max_supported}"
            ),
            DecodeError::UnsupportedType { column, type_name } => {
                write!(
                    f,
                    "Unsupported ClickHouse type '{type_name}' for column '{column}'"
                )
            }
            DecodeError::InvalidBlockInfo { field_num } => {
                write!(f, "Unknown BlockInfo field number {field_num}")
            }
            DecodeError::DuplicateBlockInfo { field_num } => {
                write!(f, "Duplicate BlockInfo field number {field_num}")
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
            DecodeError::InvalidVariant { column, reason } => {
                write!(f, "Invalid Variant layout for column '{column}': {reason}")
            }
            DecodeError::InvalidDynamic { column, reason } => {
                write!(f, "Invalid Dynamic layout for column '{column}': {reason}")
            }
            DecodeError::InvalidJson { column, reason } => {
                write!(f, "Invalid JSON layout for column '{column}': {reason}")
            }
            DecodeError::ResourceLimit {
                limit,
                requested,
                what,
            } => write!(
                f,
                "Resource limit exceeded for {what}: requested {requested} bytes cumulatively within one block, limit is {limit} bytes"
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

#[inline]
fn ensure_supported_protocol_revision(revision: u64) -> Result<(), DecodeError> {
    if revision > DBMS_TCP_PROTOCOL_VERSION {
        return Err(DecodeError::UnsupportedProtocolRevision {
            revision,
            max_supported: DBMS_TCP_PROTOCOL_VERSION,
        });
    }
    Ok(())
}

/// Options for Native format decoding.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DecodeOptions {
    /// Negotiated server protocol revision the Native stream was produced with.
    ///
    /// Native block framing is revision gated, and the revision is negotiated
    /// out of band (in the TCP handshake), so the decoder must be told it:
    ///
    /// - A `BlockInfo` preamble precedes every block when this is > 0.
    /// - A per-column custom-serialization marker byte is present when this is
    ///   \>= [`DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION`].
    ///
    /// For the native TCP protocol, the client must advertise at most
    /// [`DBMS_TCP_PROTOCOL_VERSION`] and pass the resulting effective revision
    /// here. Higher revisions are not supported: ClickHouse revision 54492, for
    /// example, enables a different String body layout. Use 0 for a bare Native
    /// stream with no protocol framing, for example HTTP `FORMAT Native` with no
    /// `client_protocol_version` set.
    pub protocol_revision: u64,
    /// Maximum cumulative bytes that one block may allocate for buffers
    /// synthesized without corresponding input bytes.
    ///
    /// This currently covers the all-zero shared offsets synthesized for a
    /// FLATTENED JSON column, whose FLATTENED body carries no shared-data stream
    /// and so cannot bound that offset allocation from the input. Every
    /// FLATTENED JSON column charges its synthesized shared-offsets run against
    /// this budget, whether or not it declares typed or dynamic paths: a typed
    /// path can itself be a pathless FLATTENED JSON that writes zero body bytes
    /// per row, so a with-paths column is not reliably input-bounded.
    ///
    /// The bound is cumulative across all columns of one block and reset before
    /// the next block. A hostile block cannot exceed this ceiling regardless of
    /// how many columns it declares or how deeply its JSON paths nest, so a
    /// single small header can never amplify into unbounded allocation, while
    /// streams of legitimate blocks never accumulate charges. The default is
    /// 256 MiB. A legitimate block is bounded by `max_block_size` at roughly
    /// 65,000 rows and so charges only about half a megabyte per flattened JSON
    /// column, far below the ceiling; lower the limit only for
    /// memory-constrained bindings.
    pub max_synthetic_allocation_bytes: usize,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            protocol_revision: 0,
            max_synthetic_allocation_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Cumulative per-block allowance for allocations not bounded by input.
#[derive(Debug)]
struct AllocationBudget {
    limit: usize,
    used: usize,
}

impl AllocationBudget {
    fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    fn charge(&mut self, bytes: usize, what: &'static str) -> Result<(), DecodeError> {
        let requested = self
            .used
            .checked_add(bytes)
            .ok_or(DecodeError::ResourceLimit {
                limit: self.limit,
                requested: self.used.saturating_add(bytes),
                what,
            })?;
        if requested > self.limit {
            return Err(DecodeError::ResourceLimit {
                limit: self.limit,
                requested,
                what,
            });
        }
        self.used = requested;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct DecodeSettings {
    protocol_revision: u64,
    types_in_binary_format: bool,
    max_synthetic_allocation_bytes: usize,
}

impl DecodeSettings {
    fn text(options: &DecodeOptions) -> Self {
        Self {
            protocol_revision: options.protocol_revision,
            types_in_binary_format: false,
            max_synthetic_allocation_bytes: options.max_synthetic_allocation_bytes,
        }
    }

    fn binary(options: &DecodeOptions) -> Self {
        Self {
            protocol_revision: options.protocol_revision,
            types_in_binary_format: true,
            max_synthetic_allocation_bytes: options.max_synthetic_allocation_bytes,
        }
    }
}

#[derive(Debug, Clone)]
enum DynamicStateChild {
    Typed(ChType),
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DynamicWireKind {
    Variant,
    Flattened,
}

#[derive(Debug, Clone)]
struct DynamicState {
    kind: DynamicWireKind,
    children: Vec<DynamicStateChild>,
}

/// The wire shape a JSON column's structure prefix selected for this block
/// (`SerializationObject::SerializationVersion`, confirmed at
/// v26.6.1.1193-stable). `Structured` covers both V1 (word 0) and V2 (word 2),
/// which differ only in V1's ignored legacy count slot and carry a shared-data
/// stream. `Flattened` (word 3) carries no shared-data stream. `Text` (STRING,
/// word 1) re-serializes each document to one string per row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonWireKind {
    Structured,
    Flattened,
    Text,
}

/// Per-block JSON structure state read in the prefix and consumed in the body.
///
/// `dynamic_paths` are the block-local runtime path names (sorted, strictly
/// increasing), each of which has its own full `SerializationDynamic` state that
/// lives as a separate [`StatePrefix::Dynamic`] in the shared state vector, in
/// the same sorted order. `Text` blocks carry no dynamic paths.
#[derive(Debug, Clone)]
struct JsonState {
    kind: JsonWireKind,
    dynamic_paths: Vec<String>,
    /// Number of dynamic paths represented by this state. Allocating decode
    /// retains their names, while the scan-only path retains only this count.
    dynamic_path_count: usize,
}

impl JsonState {
    fn dynamic_path_count(&self) -> usize {
        self.dynamic_path_count
    }
}

/// One entry in the per-column preorder state vector shared by the prefix, body,
/// suffix, and skip traversals. `Dynamic` and `Json` are the only self-describing
/// types whose per-block structure must be read once in the prefix and reused in
/// the body, so both push a state here; the body walks the same tree in the same
/// preorder and pops them in order.
#[derive(Debug, Clone)]
enum StatePrefix {
    Dynamic(DynamicState),
    Json(JsonState),
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
    // Each value is at least its one-byte varint length prefix, so the input
    // cannot hold more than `remaining()` values. Cap both reservations there
    // before the read loop: a genuine `num_rows`-row column has >= `num_rows`
    // payload bytes, so the cap equals `num_rows` and never reallocates, while a
    // hostile count near the block size cannot drive a 4x (i32 offsets)
    // over-reservation ahead of the truncation error.
    let value_capacity = reader.capacity_for(num_rows, 1);
    let mut offsets = Vec::with_capacity(value_capacity + 1);
    // Reserve a lower bound of one byte per value so the common short-string
    // case does not start from a zero-capacity buffer and reallocate from
    // scratch on the first few pushes. `extend_from_slice` still grows it for
    // longer strings.
    let mut data = Vec::with_capacity(value_capacity);
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
        offset = next_string_offset(offset, len)?;
        let bytes = reader.read_slice(len)?;
        data.extend_from_slice(bytes);
        offsets.push(offset);
    }

    Ok((offsets, data))
}

/// Advance one String run's running Arrow i32 offset past a value of `len`
/// bytes, rejecting a run whose data would exceed `i32::MAX` bytes. Shared by
/// [`decode_string_data`] and both completeness-scan walkers so all three reject
/// the same runs with the same error, before any payload byte is read.
#[inline]
fn next_string_offset(offset: i32, len: usize) -> io::Result<i32> {
    i32::try_from(len)
        .ok()
        .and_then(|len| offset.checked_add(len))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "String column chunk exceeds 2 GiB (i32 offset overflow)",
            )
        })
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

/// Decode one dense BFloat16 run as exact little-endian two-byte words.
///
/// At v26.6.1.1193-stable, `SerializationNumber<BFloat16>::deserializeBinaryBulk`
/// reads exactly 2 bytes per row. The `[u8; 2]` element type preserves those
/// bytes verbatim on every host and makes the width invariant structural for
/// the Arrow FixedSizeBinary(2) export.
fn decode_bfloat16_data(reader: &mut ByteReader, num_rows: usize) -> io::Result<Vec<[u8; 2]>> {
    let total = num_rows.checked_mul(2).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "BFloat16 column byte length overflows usize",
        )
    })?;
    let src = reader.read_slice(total)?;
    let mut values = Vec::<[u8; 2]>::with_capacity(num_rows);
    // Safety: the allocation has capacity for exactly `num_rows` two-byte
    // arrays, every bit pattern is valid for `[u8; 2]`, and `total` was checked
    // as `num_rows * 2`. The copy initializes all elements before the Vec length
    // is set, and source and destination are distinct allocations.
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), values.as_mut_ptr().cast::<u8>(), total);
        values.set_len(num_rows);
    }
    Ok(values)
}

/// Checked byte counts for one QBit bulk run.
///
/// QBit stores `bit_width` FixedString planes, each with `num_rows` values of
/// `ceil(dimension / 8)` bytes. The materialized Arrow child has
/// `num_rows * dimension` scalar values. Overflow is reported as
/// `UnexpectedEof` so the streaming decoder treats an impossible advertised
/// run like every other truncated fixed-width body.
fn qbit_layout(
    num_rows: usize,
    dimension: usize,
    bit_width: usize,
) -> io::Result<(usize, usize, usize)> {
    let bytes_per_plane_row = dimension / 8 + usize::from(dimension % 8 != 0);
    let plane_stride = num_rows.checked_mul(bytes_per_plane_row).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "QBit plane byte length overflows usize",
        )
    })?;
    // SerializationFixedString rejects one bulk plane above 1 GiB. Match that
    // fatal server limit before waiting for or allocating an impossible body.
    if plane_stride > 1usize << 30 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "QBit plane exceeds ClickHouse's 1 GiB FixedString bulk limit",
        ));
    }
    let wire_len = plane_stride.checked_mul(bit_width).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "QBit column byte length overflows usize",
        )
    })?;
    let value_count = num_rows.checked_mul(dimension).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "QBit materialized value count overflows usize",
        )
    })?;
    Ok((bytes_per_plane_row, wire_len, value_count))
}

/// Transpose one QBit Native body into row-major Arrow child values.
///
/// At v26.6.1.1193-stable,
/// `SerializationQBit::deserializeBinaryBulkWithMultipleStreams` delegates to a
/// Tuple of one `FixedString(ceil(N / 8))` column per scalar bit. Tuple order is
/// most-significant bit first. Within one plane row, logical element `i` is bit
/// `i % 8` of byte `K - 1 - i / 8`. Each eight-element group is one 8-lane x
/// bit-width bit matrix, rebuilt one byte position at a time with the 8x8 SWAR
/// transpose, reading each plane byte once per eight output values and writing
/// the final child buffer in one allocation. Server readers ignore unused
/// padding bits in the first plane byte, so this decoder does too and truncates
/// each final group to the declared dimension.
fn decode_qbit_words<T, W, F>(
    wire: &[u8],
    num_rows: usize,
    dimension: usize,
    bytes_per_plane_row: usize,
    value_count: usize,
    from_word: F,
) -> Vec<T>
where
    W: QBitWord,
    F: Fn(W) -> T,
{
    let bit_width = W::BYTES * 8;
    let plane_stride = num_rows * bytes_per_plane_row;
    let mut values = Vec::with_capacity(value_count);

    for row in 0..num_rows {
        let row_base = row * bytes_per_plane_row;
        for group in 0..bytes_per_plane_row {
            let first_element = group * 8;
            let lanes = (dimension - first_element).min(8);
            let wire_byte = bytes_per_plane_row - 1 - group;
            let mut words = [W::default(); 8];
            for j in 0..W::BYTES {
                let mut x = 0u64;
                // Element bit `8j + b` lives in plane `bit_width - 1 - (8j + b)`.
                for b in 0..8 {
                    let plane = bit_width - 1 - (8 * j + b);
                    x |= u64::from(wire[plane * plane_stride + row_base + wire_byte]) << (8 * b);
                }
                let t = transpose8(x);
                for (lane, word) in words.iter_mut().enumerate() {
                    word.set_byte(j, (t >> (8 * lane)) as u8);
                }
            }
            values.extend(words[..lanes].iter().copied().map(&from_word));
        }
    }

    values
}

fn decode_qbit_data(
    reader: &mut ByteReader,
    num_rows: usize,
    element_type: QBitElementType,
    dimension: usize,
    validity: Option<Bitmap>,
) -> Result<QBitColumn, DecodeError> {
    let bit_width = element_type.bit_width();
    let (bytes_per_plane_row, wire_len, value_count) = qbit_layout(num_rows, dimension, bit_width)?;
    let wire = reader.read_slice(wire_len)?;
    let values = match element_type {
        QBitElementType::BFloat16 => Column::BFloat16(PrimitiveColumn::new(decode_qbit_words(
            wire,
            num_rows,
            dimension,
            bytes_per_plane_row,
            value_count,
            u16::to_le_bytes,
        ))),
        QBitElementType::Float32 => Column::Float32(PrimitiveColumn::new(decode_qbit_words(
            wire,
            num_rows,
            dimension,
            bytes_per_plane_row,
            value_count,
            f32::from_bits,
        ))),
        QBitElementType::Float64 => Column::Float64(PrimitiveColumn::new(decode_qbit_words(
            wire,
            num_rows,
            dimension,
            bytes_per_plane_row,
            value_count,
            f64::from_bits,
        ))),
    };
    Ok(match validity {
        Some(validity) => QBitColumn::new_nullable(values, dimension, validity),
        None => QBitColumn::new(values, dimension),
    })
}

// ---------------------------------------------------------------------------
// Bulk-state prefix
// ---------------------------------------------------------------------------

/// One pending step in the iterative state-prefix traversal.
enum PrefixWork<'a> {
    Borrowed(&'a ChType, usize),
    Owned(ChType, usize),
    Dynamic { max_types: u8, depth: usize },
}

/// Consume a column's per-block `deserializeBinaryBulkStatePrefix` bytes.
///
/// In the Native format the server runs `readData` once per column per block,
/// which calls `deserializeBinaryBulkStatePrefix` immediately before the column
/// payload, and only when the block has rows (`NativeReader::readData`, gated by
/// `if (rows)`). Most supported types read zero prefix bytes.
/// `LowCardinality` reads its 8-byte key version, while `Variant` and `Dynamic`
/// read their discriminator mode or block-local structure and type table.
/// Centralizing these bytes here keeps nested prefix order identical to the
/// server instead of special-casing the per-column loop.
///
/// The traversal uses an explicit worklist. This matters for Dynamic and JSON:
/// their runtime type tables arrive as column data, so each level restarts the
/// ordinary parser's per-type depth budget. Keeping one cumulative `depth` on
/// work items rejects a hostile cross-table chain without consuming one Rust
/// stack frame per level. Borrowed header types stay borrowed; runtime child
/// types are cloned once into owned work items because their originals remain
/// in `states` for the body traversal.
fn read_state_prefix(
    reader: &mut ByteReader,
    ch_type: &ChType,
    column: &str,
    options: &DecodeSettings,
    states: &mut Vec<StatePrefix>,
    depth: usize,
) -> Result<(), DecodeError> {
    // A scalar or leaf column reads its prefix without ever descending, so the
    // worklist stays empty and never allocates. The root starts in `next`; only
    // container arms push onto `work`. `next` holds the root once, is drained on
    // the first iteration, and is never refilled, so LIFO pop order (which the
    // reversed container pushes rely on) is preserved exactly.
    let mut work: Vec<PrefixWork> = Vec::new();
    let mut next = Some(PrefixWork::Borrowed(ch_type, depth));
    while let Some(item) = next.take().or_else(|| work.pop()) {
        match item {
            PrefixWork::Borrowed(current, depth) => {
                // Aliases have the exact state prefix of their physical type.
                // A Nested expansion is owned; fixed aliases and SAF borrow.
                if let Some(under) = current.physical_delegate_ref() {
                    match under {
                        Cow::Borrowed(under) => work.push(PrefixWork::Borrowed(under, depth + 1)),
                        Cow::Owned(under) => work.push(PrefixWork::Owned(under, depth + 1)),
                    }
                    continue;
                }
                match current {
                    ChType::LowCardinality(_) => read_low_cardinality_state(reader, column)?,
                    ChType::Array(inner) | ChType::Nullable(inner) => {
                        work.push(PrefixWork::Borrowed(inner, depth + 1));
                    }
                    ChType::Tuple(elements) => {
                        for (_, element_type) in elements.iter().rev() {
                            work.push(PrefixWork::Borrowed(element_type, depth + 1));
                        }
                    }
                    ChType::Map(key, value) => {
                        work.push(PrefixWork::Borrowed(value, depth + 1));
                        work.push(PrefixWork::Borrowed(key, depth + 1));
                    }
                    ChType::Variant(alternatives) => {
                        read_variant_state(reader, column)?;
                        for alternative in alternatives.iter().rev() {
                            work.push(PrefixWork::Borrowed(alternative, depth + 1));
                        }
                    }
                    ChType::Dynamic { max_types } => read_dynamic_state_prefix(
                        reader, *max_types, column, options, states, depth, &mut work,
                    )?,
                    ChType::Json {
                        max_dynamic_paths,
                        max_dynamic_types,
                        typed_paths,
                        ..
                    } => {
                        let state = read_json_state(reader, *max_dynamic_paths, column, depth)?;
                        let kind = state.kind;
                        let num_dynamic = state.dynamic_path_count();
                        states.push(StatePrefix::Json(state));
                        if kind != JsonWireKind::Text {
                            for _ in 0..num_dynamic {
                                work.push(PrefixWork::Dynamic {
                                    max_types: *max_dynamic_types,
                                    depth: depth + 1,
                                });
                            }
                            for (_, element_type) in typed_paths.iter().rev() {
                                work.push(PrefixWork::Borrowed(element_type, depth + 1));
                            }
                        }
                    }
                    _ => {}
                }
            }
            PrefixWork::Owned(current, depth) => {
                if let Some(under) = current.physical_delegate() {
                    work.push(PrefixWork::Owned(under, depth + 1));
                    continue;
                }
                match current {
                    ChType::LowCardinality(_) => read_low_cardinality_state(reader, column)?,
                    ChType::Array(inner) | ChType::Nullable(inner) => {
                        work.push(PrefixWork::Owned(*inner, depth + 1));
                    }
                    ChType::Tuple(elements) => {
                        for (_, element_type) in elements.into_iter().rev() {
                            work.push(PrefixWork::Owned(element_type, depth + 1));
                        }
                    }
                    ChType::Map(key, value) => {
                        work.push(PrefixWork::Owned(*value, depth + 1));
                        work.push(PrefixWork::Owned(*key, depth + 1));
                    }
                    ChType::Variant(alternatives) => {
                        read_variant_state(reader, column)?;
                        for alternative in alternatives.into_iter().rev() {
                            work.push(PrefixWork::Owned(alternative, depth + 1));
                        }
                    }
                    ChType::Dynamic { max_types } => read_dynamic_state_prefix(
                        reader, max_types, column, options, states, depth, &mut work,
                    )?,
                    ChType::Json {
                        max_dynamic_paths,
                        max_dynamic_types,
                        typed_paths,
                        ..
                    } => {
                        let state = read_json_state(reader, max_dynamic_paths, column, depth)?;
                        let kind = state.kind;
                        let num_dynamic = state.dynamic_path_count();
                        states.push(StatePrefix::Json(state));
                        if kind != JsonWireKind::Text {
                            for _ in 0..num_dynamic {
                                work.push(PrefixWork::Dynamic {
                                    max_types: max_dynamic_types,
                                    depth: depth + 1,
                                });
                            }
                            for (_, element_type) in typed_paths.into_iter().rev() {
                                work.push(PrefixWork::Owned(element_type, depth + 1));
                            }
                        }
                    }
                    _ => {}
                }
            }
            PrefixWork::Dynamic { max_types, depth } => read_dynamic_state_prefix(
                reader, max_types, column, options, states, depth, &mut work,
            )?,
        }
    }
    Ok(())
}

fn read_low_cardinality_state(reader: &mut ByteReader, column: &str) -> Result<(), DecodeError> {
    let key_version = reader.read_u64_le()?;
    if key_version != LOW_CARDINALITY_KEY_VERSION {
        return Err(DecodeError::InvalidLowCardinality {
            column: column.to_string(),
            reason: "key version is not 1 (SharedDictionariesWithAdditionalKeys)",
        });
    }
    Ok(())
}

fn read_variant_state(reader: &mut ByteReader, column: &str) -> Result<(), DecodeError> {
    let mode = reader.read_u64_le()?;
    if mode != 0 {
        return Err(DecodeError::InvalidVariant {
            column: column.to_string(),
            reason: format!(
                "discriminator mode {mode} is not BASIC mode 0 emitted by FORMAT Native"
            ),
        });
    }
    Ok(())
}

/// Consume a column's `deserializeBinaryBulkStateSuffix` bytes after its body.
///
/// Variant owns no suffix bytes, but delegates to every alternative in canonical
/// order. None of the types currently supported by this crate emits suffix bytes;
/// keeping the recursive traversal explicit mirrors the server and gives the
/// first future suffix-bearing type one correct integration point.
fn read_state_suffix(
    reader: &mut ByteReader,
    ch_type: &ChType,
    column: &str,
    states: &[StatePrefix],
    state_cursor: &mut usize,
) -> Result<(), DecodeError> {
    if let Some(under) = ch_type.physical_delegate_ref() {
        return read_state_suffix(reader, under.as_ref(), column, states, state_cursor);
    }
    match ch_type {
        ChType::Array(inner) | ChType::Nullable(inner) => {
            read_state_suffix(reader, inner, column, states, state_cursor)
        }
        ChType::Tuple(elements) => {
            for (_, element_type) in elements {
                read_state_suffix(reader, element_type, column, states, state_cursor)?;
            }
            Ok(())
        }
        ChType::Map(key, value) => {
            read_state_suffix(reader, key, column, states, state_cursor)?;
            read_state_suffix(reader, value, column, states, state_cursor)
        }
        ChType::Variant(alternatives) => {
            for alternative in alternatives {
                read_state_suffix(reader, alternative, column, states, state_cursor)?;
            }
            Ok(())
        }
        ChType::Dynamic { .. } => {
            let state = next_dynamic_state(states, state_cursor, column)?;
            for child in &state.children {
                if let DynamicStateChild::Typed(ch_type) = child {
                    read_state_suffix(reader, ch_type, column, states, state_cursor)?;
                }
            }
            Ok(())
        }
        // JSON owns no suffix bytes, but its typed-path and dynamic-path children
        // must still be walked in the same preorder as the prefix and body so the
        // state cursor stays aligned (all suffixes are no-ops today).
        ChType::Json { typed_paths, .. } => {
            let state = next_json_state(states, state_cursor, column)?;
            let (kind, num_dynamic) = (state.kind, state.dynamic_path_count());
            if kind == JsonWireKind::Text {
                return Ok(());
            }
            for (_, element_type) in typed_paths {
                read_state_suffix(reader, element_type, column, states, state_cursor)?;
            }
            // Each dynamic path is a full Dynamic; walk its typed children, one
            // per path in the same sorted order the prefix appended them.
            for _ in 0..num_dynamic {
                let state = next_dynamic_state(states, state_cursor, column)?;
                for child in &state.children {
                    if let DynamicStateChild::Typed(ch_type) = child {
                        read_state_suffix(reader, ch_type, column, states, state_cursor)?;
                    }
                }
            }
            Ok(())
        }
        _ => {
            // Intentionally a no-op: no type this crate currently supports emits
            // suffix bytes. The arm exists only to keep this traversal symmetric
            // with the server's prefix/body/suffix serialization contract.
            let _ = (reader, column);
            Ok(())
        }
    }
}

fn read_dynamic_state(
    reader: &mut ByteReader,
    max_types: u8,
    column: &str,
    options: &DecodeSettings,
) -> Result<DynamicState, DecodeError> {
    let version = reader.read_u64_le()?;
    let kind = match version {
        1 => {
            // V1's first VarUInt was historically max_dynamic_types. The
            // current writer puts the direct-type count there and the reader
            // ignores it, so consume it without comparing it.
            reader.read_varint()?;
            DynamicWireKind::Variant
        }
        2 => DynamicWireKind::Variant,
        3 => DynamicWireKind::Flattened,
        4 => {
            return Err(invalid_dynamic(
                column,
                "structure word 4 (V3) is not emitted by NativeWriter",
            ))
        }
        other => {
            return Err(invalid_dynamic(
                column,
                format!("unknown structure word {other}"),
            ))
        }
    };

    let count = varint_usize(reader.read_varint()?, "Dynamic runtime type count")?;
    if kind == DynamicWireKind::Variant && count > 254 {
        return Err(invalid_dynamic(
            column,
            format!("direct runtime type count {count} exceeds 254"),
        ));
    }
    if kind == DynamicWireKind::Variant && count > max_types as usize {
        return Err(invalid_dynamic(
            column,
            format!("direct runtime type count {count} exceeds max_types={max_types}"),
        ));
    }
    if count >= u32::MAX as usize {
        return Err(invalid_dynamic(
            column,
            "runtime type count exceeds the u32 block-local routing model",
        ));
    }

    let mut typed = Vec::with_capacity(reader.capacity_for(count, 1));
    let mut names = std::collections::BTreeSet::new();
    for _ in 0..count {
        let ch_type = read_dynamic_type_entry(reader, options, column)?;
        let canonical = ch_type.to_string();
        if resolves_to_nothing(&ch_type) || !is_valid_variant_alternative(&ch_type) {
            return Err(invalid_dynamic(
                column,
                format!("runtime type {canonical} is not legal in Dynamic"),
            ));
        }
        if let Some(unsupported) = unsupported_header_type_name(&ch_type) {
            return Err(DecodeError::UnsupportedType {
                column: column.to_string(),
                type_name: unsupported,
            });
        }
        if !names.insert(canonical.clone()) {
            return Err(invalid_dynamic(
                column,
                format!("duplicate canonical runtime type {canonical}"),
            ));
        }
        typed.push((canonical, ch_type));
    }

    let children = if kind == DynamicWireKind::Variant {
        let mut global = typed
            .into_iter()
            .map(|(name, ch_type)| (name, DynamicStateChild::Typed(ch_type)))
            .collect::<Vec<_>>();
        global.push(("SharedVariant".to_string(), DynamicStateChild::Shared));
        global.sort_unstable_by(|left, right| left.0.cmp(&right.0));

        let mode = reader.read_u64_le()?;
        if mode != 0 {
            return Err(invalid_dynamic(
                column,
                format!("Variant discriminator mode {mode} is not BASIC mode 0"),
            ));
        }
        global.into_iter().map(|(_, child)| child).collect()
    } else {
        typed
            .into_iter()
            .map(|(_, ch_type)| DynamicStateChild::Typed(ch_type))
            .collect()
    };

    Ok(DynamicState { kind, children })
}

fn read_dynamic_type_entry(
    reader: &mut ByteReader,
    options: &DecodeSettings,
    column: &str,
) -> Result<ChType, DecodeError> {
    if options.types_in_binary_format {
        return read_binary_type(reader).map_err(|error| match error {
            BinaryTypeError::Io(error) => DecodeError::Io(error),
            BinaryTypeError::Invalid(reason) | BinaryTypeError::Unsupported(reason) => {
                invalid_dynamic(column, reason)
            }
        });
    }

    let type_name = reader.read_varint_string()?;
    parse_ch_type(&type_name).ok_or_else(|| DecodeError::UnsupportedType {
        column: column.to_string(),
        type_name,
    })
}

/// Shared reason for a traversal that ran past the retained preorder states.
const MISSING_BODY_STATE: &str = "internal prefix traversal did not retain a body state";

/// Pop the next preorder state, advancing the shared cursor. The prefix
/// traversal retained one entry per self-describing node (`Dynamic` or `JSON`)
/// in the exact order the body, suffix, and skip traversals revisit them.
/// `None` means the traversal ran past the retained states; the caller reports
/// it under its own node kind (Dynamic or JSON).
fn next_state<'a>(states: &'a [StatePrefix], state_cursor: &mut usize) -> Option<&'a StatePrefix> {
    let state = states.get(*state_cursor)?;
    *state_cursor += 1;
    Some(state)
}

/// Pop the next preorder state, requiring it to be a Dynamic structure state.
fn next_dynamic_state<'a>(
    states: &'a [StatePrefix],
    state_cursor: &mut usize,
    column: &str,
) -> Result<&'a DynamicState, DecodeError> {
    match next_state(states, state_cursor)
        .ok_or_else(|| invalid_dynamic(column, MISSING_BODY_STATE))?
    {
        StatePrefix::Dynamic(state) => Ok(state),
        StatePrefix::Json(_) => Err(invalid_dynamic(
            column,
            "expected a Dynamic structure state but found a JSON one",
        )),
    }
}

/// Pop the next preorder state, requiring it to be a JSON structure state.
fn next_json_state<'a>(
    states: &'a [StatePrefix],
    state_cursor: &mut usize,
    column: &str,
) -> Result<&'a JsonState, DecodeError> {
    match next_state(states, state_cursor)
        .ok_or_else(|| invalid_json(column, MISSING_BODY_STATE))?
    {
        StatePrefix::Json(state) => Ok(state),
        StatePrefix::Dynamic(_) => Err(invalid_json(column, "expected a JSON structure state")),
    }
}

/// Read one Dynamic column's structure prefix, retain its state in preorder,
/// and schedule its runtime typed children's prefixes on the shared worklist.
fn read_dynamic_state_prefix<'a>(
    reader: &mut ByteReader,
    max_types: u8,
    column: &str,
    options: &DecodeSettings,
    states: &mut Vec<StatePrefix>,
    depth: usize,
    work: &mut Vec<PrefixWork<'a>>,
) -> Result<(), DecodeError> {
    // Charge the cumulative budget HERE, before parsing this level's runtime
    // type table: Dynamic is one of the two constructs (with JSON) whose nested
    // types arrive as data rather than through the depth-capped header parser,
    // so it is where the per-type cap can be restarted.
    if depth >= MAX_TYPE_DEPTH {
        return Err(invalid_dynamic(
            column,
            format!("Dynamic nesting exceeds the maximum type depth {MAX_TYPE_DEPTH}"),
        ));
    }
    let state = read_dynamic_state(reader, max_types, column, options)?;
    // Retain the parent before scheduling children, yielding the same preorder
    // state vector the body consumes. Runtime child types are cloned once for
    // owned work items; the originals stay in this state for body decoding.
    for child in state.children.iter().rev() {
        if let DynamicStateChild::Typed(ch_type) = child {
            work.push(PrefixWork::Owned(ch_type.clone(), depth + 1));
        }
    }
    states.push(StatePrefix::Dynamic(state));
    Ok(())
}

/// Read one JSON column's own structure word and dynamic-path list. The caller
/// retains the returned state in preorder, then schedules the typed-path and
/// per-dynamic-path Dynamic prefixes on the shared worklist.
///
/// Wire layout (`SerializationObject::serializeBinaryBulkStatePrefix` /
/// `deserializeObjectStructureStatePrefix`, confirmed at v26.6.1.1193-stable):
/// an LE u64 structure version word (V1=0, STRING=1, V2=2, FLATTENED=3, V3=4);
/// V3 is MergeTree-only and never emitted by `NativeWriter`, so it is rejected
/// like the Dynamic V3 precedent. STRING carries nothing else in the prefix. V1
/// carries a VarUInt legacy count (the dynamic-path count, ignored on read,
/// confirmed) then the VarUInt dynamic-path count and the sorted path strings;
/// V2 and FLATTENED carry the count and paths without the legacy slot. Then, for
/// every non-STRING form, the typed-path nested prefixes in sorted path order,
/// then one full `SerializationDynamic` state prefix per dynamic path in sorted
/// order (each a `Dynamic` at `max_dynamic_types`). The shared-data child
/// (`Array(Tuple(String, String))`, V1/V2 only) contributes no prefix bytes.
fn read_json_state(
    reader: &mut ByteReader,
    max_dynamic_paths: u32,
    column: &str,
    depth: usize,
) -> Result<JsonState, DecodeError> {
    if depth >= MAX_TYPE_DEPTH {
        return Err(invalid_json(
            column,
            format!("JSON nesting exceeds the maximum type depth {MAX_TYPE_DEPTH}"),
        ));
    }
    let version = reader.read_u64_le()?;
    let kind = match version {
        0 => JsonWireKind::Structured, // V1
        1 => JsonWireKind::Text,       // STRING
        2 => JsonWireKind::Structured, // V2
        3 => JsonWireKind::Flattened,
        4 => {
            return Err(invalid_json(
                column,
                "structure word 4 (V3) is not emitted by NativeWriter",
            ))
        }
        other => {
            return Err(invalid_json(
                column,
                format!("unknown JSON structure word {other}"),
            ))
        }
    };

    let mut dynamic_paths: Vec<String> = Vec::new();
    if kind != JsonWireKind::Text {
        if version == 0 {
            // V1's leading VarUInt is a legacy back-compat slot whose value is the
            // dynamic-path count; the reader consumes it and ignores it
            // (confirmed at v26.6.1.1193-stable), matching the Dynamic V1
            // precedent.
            reader.read_varint()?;
        }
        let count = varint_usize(reader.read_varint()?, "JSON dynamic path count")?;
        // The path-count bound is V1/V2 only. In those forms the writer routes
        // any path past `max_dynamic_paths` into shared data, so the direct list
        // can never exceed it, and a larger count is malformed. FLATTENED is
        // different: `flattenPaths` writes the union of the dynamic paths AND
        // every distinct shared-data path (sorted), and the server's FLATTENED
        // reader (`SerializationObject::deserializeObjectStructureStatePrefix` ->
        // `unflattenAndInsertPaths` in `SerializationObjectHelpers.cpp`, confirmed
        // at v26.6.1.1193-stable) enforces NO bound on that count at all: it
        // greedily assigns the first sorted paths that fit `max_dynamic_paths`
        // and spills the rest back into shared data. So the FLATTENED count
        // legitimately and routinely exceeds `max_dynamic_paths`; capping it here
        // would reject valid data. It is protected purely by the read-before-
        // allocate discipline below (a hostile count fails on the truncated path
        // reads), the same as any other untrusted count.
        if kind == JsonWireKind::Structured && count > max_dynamic_paths as usize {
            return Err(invalid_json(
                column,
                format!("dynamic path count {count} exceeds max_dynamic_paths={max_dynamic_paths}"),
            ));
        }
        // Every path is at least its one-byte varint length prefix, so bound the
        // reservation at the bytes present (the read-before-allocate discipline).
        dynamic_paths.reserve(reader.capacity_for(count, 1));
        for _ in 0..count {
            let path = reader.read_varint_string()?;
            // The server always writes the dynamic paths sorted and unique;
            // reject anything else as malformed rather than silently accept a
            // duplicate or out-of-order path.
            if let Some(last) = dynamic_paths.last() {
                if path.as_str() <= last.as_str() {
                    return Err(invalid_json(
                        column,
                        "dynamic paths are not strictly increasing (sorted, unique)",
                    ));
                }
            }
            dynamic_paths.push(path);
        }
    }

    Ok(JsonState {
        kind,
        dynamic_path_count: dynamic_paths.len(),
        dynamic_paths,
    })
}

fn invalid_dynamic(column: &str, reason: impl Into<String>) -> DecodeError {
    DecodeError::InvalidDynamic {
        column: column.to_string(),
        reason: reason.into(),
    }
}

fn invalid_json(column: &str, reason: impl Into<String>) -> DecodeError {
    DecodeError::InvalidJson {
        column: column.to_string(),
        reason: reason.into(),
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

    // `read_slice(total)` above already proved `total = num_rows * index_width`
    // bytes are present, so `num_rows` is bounded by the bytes actually read
    // (read-before-allocate, like `decode_primitive!`); no separate capacity cap
    // is needed here.
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
    options: &DecodeSettings,
    allocation_budget: &mut AllocationBudget,
) -> Result<Column, DecodeError> {
    // Per-column bulk-state prefix. Zero bytes for every type except
    // LowCardinality, which reads its key version here; Array recurses into its
    // element type's prefix (so a leaf LowCardinality key version is consumed
    // here, before the offsets).
    let mut states = Vec::new();
    read_state_prefix(reader, ch_type, column, options, &mut states, 0)?;
    let mut state_cursor = 0usize;
    let decoded = decode_values(
        reader,
        ch_type,
        num_rows,
        column,
        &states,
        &mut state_cursor,
        allocation_budget,
    )?;
    if state_cursor != states.len() {
        // Report under the kind of the first unconsumed state.
        let reason = "body traversal did not consume every prefix state";
        return Err(match states.get(state_cursor) {
            Some(StatePrefix::Json(_)) => invalid_json(column, reason),
            _ => invalid_dynamic(column, reason),
        });
    }
    let mut suffix_index = 0usize;
    read_state_suffix(reader, ch_type, column, &states, &mut suffix_index)?;
    Ok(decoded)
}

/// Decode a column's value payload once its per-column state prefix has been
/// consumed by [`read_state_prefix`].
///
/// Split out from [`decode_column`] so [`decode_array`] can decode its flattened
/// element column WITHOUT re-consuming a state prefix: `SerializationArray` emits
/// the element type's prefix once, at the very front of the Array column (before
/// the offsets), not again per element run.
///
/// At v26.8.1.2041-lts, `MultiPoint` is the custom name over `Array(Point)`
/// registered by `registerDataTypeDomainGeo` in
/// `src/DataTypes/DataTypeCustomGeo.cpp`. Its payload therefore uses
/// `SerializationArray` offsets followed by the flattened
/// `SerializationTuple` Point body, with no MultiPoint-specific framing.
fn decode_values(
    reader: &mut ByteReader,
    ch_type: &ChType,
    num_rows: usize,
    column: &str,
    states: &[StatePrefix],
    state_cursor: &mut usize,
    allocation_budget: &mut AllocationBudget,
) -> Result<Column, DecodeError> {
    // A name-decoration alias (SimpleAggregateFunction, geo, Geometry, Nested)
    // decodes exactly as the physical type it delegates to, producing the
    // underlying Column variant (no new variant). Expand and recurse before the
    // container dispatch below so a geo/Nested alias that expands to an `Array`
    // reaches the Array fast-path, and a SimpleAggregateFunction over any inner
    // delegates to that inner.
    if let Some(under) = ch_type.physical_delegate_ref() {
        return decode_values(
            reader,
            under.as_ref(),
            num_rows,
            column,
            states,
            state_cursor,
            allocation_budget,
        );
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
        // today the zero-count entry points are an Array whose arrays are all
        // empty and a Variant alternative that occurs in zero rows (a zero-row
        // block skips column data entirely and never reaches here), but any
        // future one (Map values, Tuple elements) gets the same absent body and
        // takes this same gate.
        if num_rows == 0 {
            return Ok(empty_column(ch_type));
        }
        return decode_low_cardinality(reader, inner, num_rows, column);
    }

    // Array is offsets plus a flattened element column, decoded as a unit; its
    // element type's prefix was already consumed by the caller's
    // `read_state_prefix`.
    if let ChType::Array(inner) = ch_type {
        return decode_array(
            reader,
            inner,
            num_rows,
            column,
            states,
            state_cursor,
            allocation_budget,
        );
    }

    // Map is the Array(Tuple(keys, values)) wire layout decoded as a unit; like
    // Array it is never nullable at this level, so it dispatches before the
    // Nullable unwrap. The key/value prefixes were consumed by the caller's
    // `read_state_prefix`.
    if let ChType::Map(key, value) = ch_type {
        return decode_map(
            reader,
            key,
            value,
            num_rows,
            column,
            states,
            state_cursor,
            allocation_budget,
        );
    }

    // Variant is one discriminator byte per row followed by dense alternative
    // bodies. It has intrinsic NULL semantics and cannot be wrapped in Nullable,
    // so dispatch it before the ordinary Nullable unwrap.
    if let ChType::Variant(alternatives) = ch_type {
        return decode_variant(
            reader,
            alternatives,
            num_rows,
            column,
            states,
            state_cursor,
            allocation_budget,
        );
    }

    if matches!(ch_type, ChType::Dynamic { .. }) {
        let state = next_dynamic_state(states, state_cursor, column)?;
        return decode_dynamic(
            reader,
            state,
            num_rows,
            column,
            states,
            state_cursor,
            allocation_budget,
        );
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
    let delegate = inner.physical_delegate_ref();
    let inner = delegate.as_deref().unwrap_or(inner);

    // Tuple is a container of element columns decoded as a unit (each element
    // recurses back through this function), dispatched after the Nullable
    // unwrap because `Nullable(Tuple(...))` is legal: its per-row null map
    // precedes the tuple body, the ordinary Nullable framing.
    if let ChType::Tuple(elements) = inner {
        return decode_tuple(
            reader,
            elements,
            num_rows,
            column,
            validity,
            states,
            state_cursor,
            allocation_budget,
        );
    }

    // JSON is a container decoded as a unit, dispatched after the Nullable
    // unwrap because `Nullable(JSON)` is legal (`DataTypeObject::canBeInsideNullable`
    // is true): the per-row null map precedes the full JSON body, the ordinary
    // Nullable framing, so `validity` is threaded into the body builder.
    if let ChType::Json { .. } = inner {
        return decode_json(
            reader,
            inner,
            num_rows,
            column,
            validity,
            states,
            state_cursor,
            allocation_budget,
        );
    }

    decode_column_body(reader, inner, num_rows, validity)
}

/// Decode one `Variant(T1, ...)` body into Arrow Dense Union buffers.
///
/// At v26.6.1.1193-stable, `SerializationVariant` in
/// `src/DataTypes/Serializations/SerializationVariant.cpp` writes BASIC Native
/// bodies as exactly `num_rows` global UInt8 discriminators followed by every
/// alternative's dense body in canonical type-name order. Discriminator 255 is
/// NULL and consumes no child value; every other byte indexes `alternatives`.
/// Child counts and Arrow i32 offsets are derived in one discriminator pass,
/// with no per-row allocation. The mode word and child state prefixes were
/// already consumed by [`read_state_prefix`].
fn decode_variant(
    reader: &mut ByteReader,
    alternatives: &[ChType],
    num_rows: usize,
    column: &str,
    states: &[StatePrefix],
    state_cursor: &mut usize,
    allocation_budget: &mut AllocationBudget,
) -> Result<Column, DecodeError> {
    let discriminators = reader.read_slice(num_rows)?;
    let (layout, counts, null_count) =
        variant_layout_from_discriminators(discriminators, alternatives.len()).map_err(|err| {
            DecodeError::InvalidVariant {
                column: column.to_string(),
                reason: err.to_string(),
            }
        })?;

    let mut variants = Vec::with_capacity(alternatives.len());
    for (alternative, count) in alternatives.iter().zip(counts) {
        variants.push(decode_values(
            reader,
            alternative,
            count,
            column,
            states,
            state_cursor,
            allocation_budget,
        )?);
    }

    Ok(Column::Variant(VariantColumn::from_parts(
        discriminators.to_vec(),
        layout,
        variants,
        null_count,
    )))
}

/// Decode one self-describing `Dynamic` body after its structure/type-table and
/// child state prefixes were retained by [`read_state_prefix`].
///
/// At `v26.6.1.1193-stable`, V1/V2 are BASIC Variant bodies: one UInt8 local
/// discriminator per row, then one dense body per block-local child in global
/// canonical-name order. The implicit SharedVariant child is a String body whose
/// cells are arbitrary binary descriptor+value blobs. FLATTENED word 3 instead
/// writes the smallest fixed-width index for `children.len() + 1` values, where
/// the final index is NULL, then dense typed child bodies in table order.
fn decode_dynamic(
    reader: &mut ByteReader,
    state: &DynamicState,
    num_rows: usize,
    column: &str,
    states: &[StatePrefix],
    state_cursor: &mut usize,
    allocation_budget: &mut AllocationBudget,
) -> Result<Column, DecodeError> {
    let num_children = state.children.len();
    if num_rows > i32::MAX as usize {
        return Err(invalid_dynamic(
            column,
            "row count exceeds Arrow Dense Union's i32 child-offset range",
        ));
    }
    let mut counts = vec![0usize; num_children];
    let mut null_count = 0usize;
    // Read-before-allocate (the `decode_primitive!` hardening): each arm reads
    // its index run first, bounding it against the bytes present, and only then
    // reserves the routing buffers, so a hostile row count cannot drive a giant
    // allocation ahead of the truncation error.
    let mut type_ids = Vec::new();
    let mut offsets = Vec::new();
    match state.kind {
        DynamicWireKind::Variant => {
            let raw = reader.read_slice(num_rows)?;
            type_ids.reserve_exact(num_rows);
            offsets.reserve_exact(num_rows);
            for (row, &discriminator) in raw.iter().enumerate() {
                if discriminator == u8::MAX {
                    type_ids.push(u32::MAX);
                    offsets.push(null_count as i32);
                    null_count += 1;
                } else if (discriminator as usize) < num_children {
                    let child = discriminator as usize;
                    type_ids.push(child as u32);
                    offsets.push(counts[child] as i32);
                    counts[child] += 1;
                } else {
                    return Err(invalid_dynamic(
                        column,
                        format!(
                            "row {row} has discriminator {discriminator}, but only {num_children} children exist"
                        ),
                    ));
                }
            }
        }
        DynamicWireKind::Flattened => {
            let values = num_children
                .checked_add(1)
                .ok_or_else(|| invalid_dynamic(column, "flattened child count overflows usize"))?;
            // The server's getSmallestIndexesType picks width 4 for
            // `values <= u32::MAX + 1`; the `<= u32::MAX` here diverges by one,
            // but the divergence is unreachable: `read_dynamic_state` rejects a
            // runtime type count >= u32::MAX first, so `values <= u32::MAX`
            // always holds and the width-8 arm is dead.
            let width = if values <= u8::MAX as usize + 1 {
                1
            } else if values <= u16::MAX as usize + 1 {
                2
            } else if values <= u32::MAX as usize {
                4
            } else {
                8
            };
            let byte_len = num_rows.checked_mul(width).ok_or_else(|| {
                DecodeError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Dynamic flattened index byte length overflows usize",
                ))
            })?;
            let raw = reader.read_slice(byte_len)?;
            type_ids.reserve_exact(num_rows);
            offsets.reserve_exact(num_rows);
            for (row, bytes) in raw.chunks_exact(width).enumerate() {
                let index = match width {
                    1 => bytes[0] as u64,
                    2 => u16::from_le_bytes([bytes[0], bytes[1]]) as u64,
                    4 => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64,
                    8 => u64::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ]),
                    _ => unreachable!("flattened width selected from 1/2/4/8"),
                };
                if index == num_children as u64 {
                    type_ids.push(u32::MAX);
                    offsets.push(null_count as i32);
                    null_count += 1;
                } else if index < num_children as u64 {
                    let child = index as usize;
                    type_ids.push(child as u32);
                    offsets.push(counts[child] as i32);
                    counts[child] += 1;
                } else {
                    return Err(invalid_dynamic(
                        column,
                        format!(
                            "row {row} has flattened index {index}, greater than NULL index {num_children}"
                        ),
                    ));
                }
            }
        }
    }
    let mut children = Vec::with_capacity(num_children);
    for (state_child, count) in state.children.iter().zip(counts) {
        match state_child {
            DynamicStateChild::Typed(ch_type) => {
                let values = decode_values(
                    reader,
                    ch_type,
                    count,
                    column,
                    states,
                    state_cursor,
                    allocation_budget,
                )?;
                children.push(DynamicChild::Typed {
                    ch_type: ch_type.clone(),
                    values,
                });
            }
            DynamicStateChild::Shared => {
                let (offsets, data) = decode_string_data(reader, count)?;
                children.push(DynamicChild::Shared(Utf8Column::new(offsets, data)));
            }
        }
    }

    Ok(Column::Dynamic(DynamicColumn::from_parts(
        type_ids, offsets, children, null_count,
    )))
}

/// Upper bound on the row count of a pathless FLATTENED `JSON` block, which
/// writes zero body bytes per row and so is not bounded by the input size the
/// way every other run is. Generous against real server blocks (default
/// `max_block_size` is 65409); a larger count in a complete block is malformed
/// input.
const MAX_PATHLESS_JSON_ROWS: usize = 1 << 24;

/// Decode one `JSON` column body after its structure state prefix was retained
/// by [`read_json_state_prefix`].
///
/// Wire layout (`SerializationObject::serializeBinaryBulkWithMultipleStreams`,
/// confirmed at v26.6.1.1193-stable). STRING (word 1): one varint-length string
/// per row, the re-serialized JSON document, and nothing else. V1/V2
/// (`Structured`): the typed-path columns in sorted path order (each the path
/// type's normal bulk body), then one complete Dynamic column body per dynamic
/// path in sorted order (consuming the per-path DynamicState read in the
/// prefix), then the shared data last as a plain `Array(Tuple(String, String))`:
/// num_rows cumulative LE UInt64 end-offsets, then the flattened `paths` String
/// column, then the flattened `values` String column (whose cells are opaque
/// binary descriptor + `serializeBinary` payloads, kept as raw bytes and never
/// materialized, exactly like a `SharedVariant` cell). FLATTENED (word 3): the
/// typed-path columns then one full Dynamic column per flattened path, with NO
/// shared-data stream. `validity` is the `Nullable(JSON)` null map already
/// decoded by the caller.
#[allow(clippy::too_many_arguments)]
fn decode_json(
    reader: &mut ByteReader,
    ch_type: &ChType,
    num_rows: usize,
    column: &str,
    validity: Option<Bitmap>,
    states: &[StatePrefix],
    state_cursor: &mut usize,
    allocation_budget: &mut AllocationBudget,
) -> Result<Column, DecodeError> {
    let ChType::Json { typed_paths, .. } = ch_type else {
        return Err(invalid_json(column, "not a JSON type"));
    };

    if num_rows > i32::MAX as usize {
        return Err(invalid_json(
            column,
            "row count exceeds Arrow's i32 offset range",
        ));
    }

    // Pop this node's structure state; clone the small path list so the borrow
    // of `states` ends before the child decodes below take it again.
    let state = next_json_state(states, state_cursor, column)?;
    let (kind, dynamic_paths) = (state.kind, state.dynamic_paths.clone());

    if kind == JsonWireKind::Text {
        // STRING mode carries only one document string per row; the declared
        // typed paths are not present on the wire in this mode.
        let (offsets, data) = decode_string_data(reader, num_rows)?;
        return Ok(Column::Json(
            JsonColumn::text(Utf8Column::new(offsets, data)).with_validity(validity),
        ));
    }

    // Typed-path columns, in the ChType's sorted path order.
    let mut typed = Vec::with_capacity(typed_paths.len());
    for (path, element_type) in typed_paths {
        let values = decode_values(
            reader,
            element_type,
            num_rows,
            column,
            states,
            state_cursor,
            allocation_budget,
        )?;
        typed.push((path.clone(), values));
    }

    // Dynamic-path columns, in sorted path order, each a full Dynamic body.
    let mut dynamic = Vec::with_capacity(dynamic_paths.len());
    for path in &dynamic_paths {
        let state = match next_state(states, state_cursor)
            .ok_or_else(|| invalid_json(column, MISSING_BODY_STATE))?
        {
            StatePrefix::Dynamic(state) => state,
            StatePrefix::Json(_) => {
                return Err(invalid_json(
                    column,
                    "expected a Dynamic state for a JSON dynamic path",
                ))
            }
        };
        match decode_dynamic(
            reader,
            state,
            num_rows,
            column,
            states,
            state_cursor,
            allocation_budget,
        )? {
            Column::Dynamic(dynamic_col) => dynamic.push((path.clone(), dynamic_col)),
            _ => {
                return Err(invalid_json(
                    column,
                    "JSON dynamic path did not decode to a Dynamic column",
                ))
            }
        }
    }

    // Shared data (V1/V2 only): the plain Array(Tuple(String, String)) layout.
    // FLATTENED carries no shared-data stream, so it decodes to empty shared
    // columns.
    let (shared_offsets, shared_paths, shared_values) = if kind == JsonWireKind::Structured {
        let mut offsets = Vec::new();
        let total_pairs = read_array_offsets(reader, num_rows, column, Some(&mut offsets))?;
        let (path_offsets, path_data) = decode_string_data(reader, total_pairs)?;
        let (value_offsets, value_data) = decode_string_data(reader, total_pairs)?;
        (
            offsets,
            Utf8Column::new(path_offsets, path_data),
            Utf8Column::new(value_offsets, value_data),
        )
    } else {
        // FLATTENED carries no shared-data stream, so the shared columns are
        // empty. The offsets still carry the Arrow leading 0 plus one entry per
        // row (all zero, no pairs), so the column is a valid empty-shared
        // structured column that re-encodes as either FLATTENED or V1/V2.
        //
        // Every FLATTENED column synthesizes this all-zero shared-offsets run
        // without reading any wire bytes for it, so every one charges the
        // block's cumulative budget, with paths or not. A with-paths column is
        // NOT reliably input-bounded: a typed path that is itself a pathless
        // FLATTENED JSON writes zero body bytes per row, so charging only the
        // pathless case would let `JSON(a JSON(a JSON(...)))` retain up to
        // MAX_TYPE_DEPTH uncharged offset runs from a tiny header. Charging here
        // rejects no legitimate traffic: a real block is bounded by
        // `max_block_size` (~65k rows), so each flattened JSON column charges
        // ~0.5 MB against the 256 MiB default and the budget resets next block.
        //
        // The pathless case additionally caps the row count. That cap exists
        // because a pathless body writes zero bytes per row, so nothing on the
        // wire bounds `num_rows` before this synthetic fill, and it keeps the
        // scan and decode paths in agreement (skip_json_data carries the
        // matching check). A with-paths column is bounded by its path bodies, so
        // the cap stays scoped to the pathless case; the bytes charge below is
        // what covers a with-paths column whose paths are themselves pathless.
        if typed_paths.is_empty() && dynamic_paths.is_empty() && num_rows > MAX_PATHLESS_JSON_ROWS {
            return Err(invalid_json(
                column,
                format!(
                    "row count {num_rows} exceeds the pathless FLATTENED block limit {MAX_PATHLESS_JSON_ROWS}"
                ),
            ));
        }
        // Outside the pathless gate `num_rows` is bounded only by the earlier
        // `num_rows <= i32::MAX` check, so `(num_rows + 1) * size_of::<i64>()`
        // fits a 64-bit usize but can overflow a 32-bit one. Compute the charge
        // with checked arithmetic so it is target-independent; a saturated value
        // is rejected by the budget just like any other over-limit request.
        let offset_bytes = num_rows
            .checked_add(1)
            .and_then(|rows| rows.checked_mul(std::mem::size_of::<i64>()))
            .unwrap_or(usize::MAX);
        allocation_budget.charge(offset_bytes, "FLATTENED JSON shared offsets")?;
        (
            vec![0i64; num_rows + 1],
            Utf8Column::new(vec![0], Vec::new()),
            Utf8Column::new(vec![0], Vec::new()),
        )
    };

    Ok(Column::Json(
        JsonColumn::structured(StructuredJson::from_parts(
            typed,
            dynamic,
            shared_offsets,
            shared_paths,
            shared_values,
            num_rows,
        ))
        .with_validity(validity),
    ))
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
#[allow(clippy::too_many_arguments)]
fn decode_tuple(
    reader: &mut ByteReader,
    elements: &[(Option<String>, ChType)],
    num_rows: usize,
    column: &str,
    validity: Option<Bitmap>,
    states: &[StatePrefix],
    state_cursor: &mut usize,
    allocation_budget: &mut AllocationBudget,
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
        let element = decode_values(
            reader,
            element_type,
            num_rows,
            column,
            states,
            state_cursor,
            allocation_budget,
        )?;
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
    states: &[StatePrefix],
    state_cursor: &mut usize,
    allocation_budget: &mut AllocationBudget,
) -> Result<Column, DecodeError> {
    // Offsets: the shared walk reads and validates the run and builds the
    // Arrow-shaped offsets (leading 0, each wire offset widened to i64),
    // bounding both the allocation and the returned element count against the
    // bytes present.
    let mut offsets = Vec::new();
    let total_elements = read_array_offsets(reader, num_rows, column, Some(&mut offsets))?;

    // Element body: the flattened element column. The state prefix was consumed
    // by the caller's `read_state_prefix`, so decode the values only.
    let values = decode_values(
        reader,
        inner,
        total_elements,
        column,
        states,
        state_cursor,
        allocation_budget,
    )?;
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
#[allow(clippy::too_many_arguments)]
fn decode_map(
    reader: &mut ByteReader,
    key: &ChType,
    value: &ChType,
    num_rows: usize,
    column: &str,
    states: &[StatePrefix],
    state_cursor: &mut usize,
    allocation_budget: &mut AllocationBudget,
) -> Result<Column, DecodeError> {
    // Offsets: the shared Array walk (a Map's offsets are byte-identical to an
    // Array's), building the Arrow-shaped run with the leading 0 and bounding
    // the entry count against the bytes present.
    let mut offsets = Vec::new();
    let total_entries = read_array_offsets(reader, num_rows, column, Some(&mut offsets))?;

    // Flattened entries: the keys' full run then the values' full run, the
    // Tuple(K, V) body with prefixes already consumed. Both decodes are driven
    // by the same total, so the two fields cannot come out ragged.
    let keys = decode_values(
        reader,
        key,
        total_entries,
        column,
        states,
        state_cursor,
        allocation_budget,
    )?;
    let values = decode_values(
        reader,
        value,
        total_entries,
        column,
        states,
        state_cursor,
        allocation_budget,
    )?;
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
///
/// `BFloat16` is likewise a contiguous `num_rows * 2` byte run at this tag.
/// `DataTypeNumber<BFloat16>` is registered by `registerDataTypeNumbers` in
/// `src/DataTypes/DataTypesNumber.cpp`; its bulk body uses
/// `SerializationNumber<BFloat16>` in
/// `src/DataTypes/Serializations/SerializationNumber.cpp`. The core preserves
/// the raw little-endian 16-bit words in structurally width-2 `[u8; 2]` values,
/// including NaN payloads, without converting per value.
///
/// `Nothing` is the exceptional zero-width logical type with a nonzero Native
/// body. At v26.6.1.1193-stable,
/// `SerializationNothing::deserializeBinaryBulk` in
/// `src/DataTypes/Serializations/SerializationNothing.cpp` consumes exactly one
/// arbitrary byte per row and performs no value validation. The decoder skips
/// that run and stores only its row count plus any outer Nullable mask.
fn decode_column_body(
    reader: &mut ByteReader,
    inner_type: &ChType,
    num_rows: usize,
    validity: Option<Bitmap>,
) -> Result<Column, DecodeError> {
    let column = match inner_type {
        // At v26.6.1.1193-stable, `SerializationNothing::deserializeBinaryBulk`
        // in `src/DataTypes/Serializations/SerializationNothing.cpp` consumes
        // exactly one byte per row with no value validation. Nothing has no
        // physical value buffer, so retain only the row count and any structural
        // `Nullable(Nothing)` mask decoded by the caller.
        ChType::Nothing => {
            reader.skip(num_rows)?;
            match validity {
                Some(bm) => Column::Nothing(NothingColumn::new_nullable(num_rows, bm)),
                None => Column::Nothing(NothingColumn::new(num_rows)),
            }
        }
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
        ChType::BFloat16 => {
            let values = decode_bfloat16_data(reader, num_rows)?;
            Column::BFloat16(PrimitiveColumn { values, validity })
        }
        ChType::QBit {
            element_type,
            dimension,
        } => Column::QBit(decode_qbit_data(
            reader,
            num_rows,
            *element_type,
            *dimension,
            validity,
        )?),
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
        // `SerializationAggregateFunction` writes concrete function states
        // back-to-back with no generic length framing. The shared registry
        // admits only layouts whose row boundary is confirmed. At
        // v26.6.1.1193-stable, `AggregateFunction(count[, T])` is one VarUInt64
        // per row, canonical `nothingUInt64` and `nothingNull` are one strict
        // 0x00 byte per row, and exact base `sum` over a numeric or Enum is one
        // fixed-width accumulator. A Nullable sum argument adds a leading flag
        // and omits the accumulator when no non-NULL value was seen. Preserve
        // each state's exact serialized bytes in LargeBinary layout; the
        // argument types are metadata and do not recurse here.
        ChType::AggregateFunction { .. } => {
            // `can_be_inside_nullable` excludes `AggregateFunction`, so
            // `parse_ch_type` never yields `Nullable(AggregateFunction(...))` and
            // this arm is only ever reached with `validity == None`. Return an
            // error rather than panic, matching the catch-all arms below: the
            // states decoder ignores `validity`, so a future regression must
            // degrade to a clean decode error, not a silently dropped null map.
            if validity.is_some() {
                return Err(DecodeError::UnsupportedType {
                    column: String::new(),
                    type_name: inner_type.to_string(),
                });
            }
            let codec = decode_state_codec(inner_type)?;
            Column::AggregateState(decode_aggregate_states(reader, codec, num_rows)?)
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
        | ChType::Variant(_)
        | ChType::Dynamic { .. }
        | ChType::Json { .. }
        | ChType::SimpleAggregateFunction { .. }
        | ChType::Geo(_)
        | ChType::Geometry
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
    if let Some(under) = ch_type.physical_delegate_ref() {
        return empty_column(under.as_ref());
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
    let resolved = inner.resolved_physical_delegate_ref();
    let inner = resolved.as_deref().unwrap_or(inner);

    match inner {
        ChType::Nothing => Column::Nothing(match empty_validity {
            Some(bm) => NothingColumn::new_nullable(0, bm),
            None => NothingColumn::new(0),
        }),
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
        ChType::BFloat16 => Column::BFloat16(PrimitiveColumn {
            values: vec![],
            validity: empty_validity,
        }),
        ChType::QBit {
            element_type,
            dimension,
        } => {
            let values = match element_type {
                QBitElementType::BFloat16 => Column::BFloat16(PrimitiveColumn::new(vec![])),
                QBitElementType::Float32 => Column::Float32(PrimitiveColumn::new(vec![])),
                QBitElementType::Float64 => Column::Float64(PrimitiveColumn::new(vec![])),
            };
            Column::QBit(match empty_validity {
                Some(validity) => QBitColumn::new_nullable(values, *dimension, validity),
                None => QBitColumn::new(values, *dimension),
            })
        }
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
        // A zero-row Native block skips aggregate `readData` entirely. The
        // LargeBinary shape still carries Arrow's required leading zero offset.
        ChType::AggregateFunction { .. } => {
            Column::AggregateState(AggregateStateColumn::new(vec![0], vec![]))
        }
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
        // A zero-row Native block skips Variant's mode prefix and every child
        // body. Build the same Arrow union tree a populated decode would, over
        // one empty child column per canonical alternative and an empty Null
        // child. Header validation already proved 1..=255 alternatives, so the
        // layout constructor cannot fail here.
        ChType::Variant(alternatives) => {
            let layout = match variant_layout_from_discriminators(&[], alternatives.len()) {
                Ok((layout, _, _)) => layout,
                Err(_) => unreachable!(
                    "validated Variant header always has between 1 and 255 alternatives"
                ),
            };
            let variants = alternatives.iter().map(empty_column).collect();
            Column::Variant(VariantColumn::from_parts(Vec::new(), layout, variants, 0))
        }
        // NativeWriter gates the entire Dynamic data step on rows > 0, so a
        // zero-row block carries no structure word or runtime type table. The
        // logical Dynamic schema remains in ChType; the physical column has no
        // discovered children and empty routing/null buffers.
        ChType::Dynamic { .. } => {
            Column::Dynamic(DynamicColumn::from_parts(vec![], vec![], vec![], 0))
        }
        // NativeWriter gates the entire JSON data step on rows > 0 too, so a
        // zero-row block carries no structure word or path list. Build the
        // canonical empty structured column: the declared typed paths present
        // with their own empty columns, no dynamic paths, and empty shared data.
        // A `Nullable(JSON)` zero-row column carries the empty top-level validity
        // bitmap like the other nullable empties.
        ChType::Json { typed_paths, .. } => {
            let typed = typed_paths
                .iter()
                .map(|(path, ty)| (path.clone(), empty_column(ty)))
                .collect();
            Column::Json(
                JsonColumn::structured(StructuredJson::from_parts(
                    typed,
                    Vec::new(),
                    vec![0i64],
                    Utf8Column::new(vec![0], Vec::new()),
                    Utf8Column::new(vec![0], Vec::new()),
                    0,
                ))
                .with_validity(empty_validity),
            )
        }
        // The outer `Nullable` was unwrapped above, `parse_ch_type` never
        // produces a `Nullable` directly inside a `Nullable`, and any
        // name-decoration alias was expanded to its physical delegate above, so
        // `inner` is never a `Nullable` or an alias here. Unlike the decode and
        // scan paths this constructor is infallible (it returns a `Column`, not a
        // `Result`), so the invariant is asserted rather than surfaced as an error.
        ChType::Nullable(_)
        | ChType::SimpleAggregateFunction { .. }
        | ChType::Geo(_)
        | ChType::Geometry
        | ChType::Nested(_) => {
            unreachable!("Nullable inner unwrapped and aliases expanded; parse_ch_type rejects nested Nullable")
        }
    }
}

// ---------------------------------------------------------------------------
// Block info preamble
// ---------------------------------------------------------------------------

/// Consume the `BlockInfo` preamble that precedes each block when the producer
/// used a protocol revision > 0 (server `BlockInfo::read` and
/// `BlockInfo::write` in `src/Core/BlockInfo.cpp`, confirmed at
/// v26.8.1.2041-lts).
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
/// A repeated known field is also rejected. The server reader accepts repeats
/// with last-value-wins semantics, but `BlockInfo::write` emits every declared
/// field exactly once in declaration order, so repeats are not server output.
///
/// Returns `Ok(false)` if the stream ends cleanly before any block info byte (a
/// block boundary at end of stream), or `Ok(true)` once a full preamble has been
/// consumed.
fn read_block_info(reader: &mut ByteReader) -> Result<bool, DecodeError> {
    if reader.remaining() == 0 {
        return Ok(false);
    }
    // Once any byte of the first field-number VarUInt is present, EOF means
    // the block is partial rather than a clean boundary. This distinction is
    // essential for streaming: a non-canonical multi-byte field number may
    // straddle feeds.
    let mut field_num = reader.read_varint()?;
    let mut seen = 0u8;

    while field_num != 0 {
        let bit = match field_num {
            1 => 1 << 0,
            2 => 1 << 1,
            3 => 1 << 2,
            other => return Err(DecodeError::InvalidBlockInfo { field_num: other }),
        };
        if seen & bit != 0 {
            return Err(DecodeError::DuplicateBlockInfo { field_num });
        }
        seen |= bit;
        match field_num {
            1 => reader.skip(1)?, // is_overflows
            2 => reader.skip(4)?, // bucket_num (Int32)
            3 => {
                // out_of_order_buckets: a varint count then that many Int32 values.
                let count = varint_usize(reader.read_varint()?, "out_of_order_buckets count")?;
                reader.skip(count.saturating_mul(4))?;
            }
            // The field number was validated above.
            _ => unreachable!("known BlockInfo field matched above"),
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
/// entry point. `StreamDecoder` first runs the same resumable completeness
/// scan that backs [`BlockScanner`](crate::native::stream_decoder::BlockScanner),
/// keeping its `ScanProgress` checkpoint across feeds, so it reaches the
/// allocating decode only once a whole block is buffered and walks a block fed
/// in many pieces once overall rather than once per feed.
///
/// [`max_synthetic_allocation_bytes`](DecodeOptions::max_synthetic_allocation_bytes)
/// bounds synthesized allocations cumulatively across this block's columns.
/// The budget resets for the next block.
pub fn decode_next_block(
    reader: &mut ByteReader,
    options: &DecodeOptions,
) -> Result<Option<ColBatch>, DecodeError> {
    decode_next_block_with_settings(reader, &DecodeSettings::text(options))
}

/// Decode one block whose type headers and Dynamic runtime type tables use the
/// server's binary data-type descriptor grammar. The framing revision remains
/// supplied through [`DecodeOptions`]; only the out-of-band type encoding
/// setting differs from [`decode_next_block`].
///
/// Like [`decode_next_block`],
/// [`max_synthetic_allocation_bytes`](DecodeOptions::max_synthetic_allocation_bytes)
/// bounds synthesized allocations cumulatively across this block's columns.
/// The budget resets for the next block.
pub fn decode_next_block_binary_types(
    reader: &mut ByteReader,
    options: &DecodeOptions,
) -> Result<Option<ColBatch>, DecodeError> {
    decode_next_block_with_settings(reader, &DecodeSettings::binary(options))
}

fn decode_next_block_with_settings(
    reader: &mut ByteReader,
    options: &DecodeSettings,
) -> Result<Option<ColBatch>, DecodeError> {
    ensure_supported_protocol_revision(options.protocol_revision)?;
    decode_next_block_with_settings_unchecked(reader, options)
}

/// Decode the block that a completeness scan has already accepted with the
/// same options. The scan validates the protocol revision, so repeating that
/// comparison here would make streaming pay twice per block.
pub(crate) fn decode_scanned_block(
    reader: &mut ByteReader,
    options: &DecodeOptions,
    binary_types: bool,
) -> Result<Option<ColBatch>, DecodeError> {
    let settings = if binary_types {
        DecodeSettings::binary(options)
    } else {
        DecodeSettings::text(options)
    };
    decode_next_block_with_settings_unchecked(reader, &settings)
}

fn decode_next_block_with_settings_unchecked(
    reader: &mut ByteReader,
    options: &DecodeSettings,
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

    // No protocol framing. Only an empty reader is a clean block boundary.
    // Once the column-count VarUInt starts, EOF means the block is partial.
    if reader.remaining() == 0 {
        return Ok(None);
    }
    let num_cols = varint_usize(reader.read_varint()?, "column count")?;
    let num_rows = varint_usize(reader.read_varint()?, "row count")?;
    Ok(Some(decode_block_body(
        reader, options, num_cols, num_rows,
    )?))
}

/// Read one column header: name, type string, and the optional custom
/// serialization marker. Returns the parsed `ChType` plus the column name.
///
/// The resumable completeness walk mirrors this in [`scan_column_header`],
/// retaining partial strings and binary descriptors across feeds. Both paths
/// finish with [`validate_header_type`] so acceptance cannot drift.
fn read_column_header(
    reader: &mut ByteReader,
    options: &DecodeSettings,
) -> Result<(String, ChType), DecodeError> {
    let col_name = reader.read_varint_string()?;
    let ch_type = if options.types_in_binary_format {
        read_binary_type(reader).map_err(|error| match error {
            BinaryTypeError::Io(error) => DecodeError::Io(error),
            BinaryTypeError::Invalid(reason) | BinaryTypeError::Unsupported(reason) => {
                DecodeError::UnsupportedType {
                    column: col_name.clone(),
                    type_name: reason,
                }
            }
        })?
    } else {
        let type_name = reader.read_varint_string()?;
        parse_ch_type(&type_name).ok_or_else(|| DecodeError::UnsupportedType {
            column: col_name.clone(),
            type_name,
        })?
    };

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

    validate_header_type(&col_name, &ch_type)?;

    Ok((col_name, ch_type))
}

/// Resume one top-level column header without reparsing completed strings.
fn scan_column_header(
    reader: &mut ByteReader,
    options: &DecodeSettings,
    saved: &mut Option<Box<ColumnHeaderScanState>>,
) -> Result<(String, ChType), DecodeError> {
    if let Some(state) = saved.as_mut() {
        reader.skip(state.offset)?;
        let result = scan_column_header_state(reader, options, state);
        if !is_unexpected_eof_result(&result) {
            saved.take();
        }
        return result;
    }

    let mut state = ColumnHeaderScanState {
        name_scan: PartialStringScan::default(),
        name: None,
        text_type_scan: PartialStringScan::default(),
        descriptor_start: None,
        descriptor: None,
        ch_type: None,
        offset: reader.position(),
    };
    let result = scan_column_header_state(reader, options, &mut state);
    if is_unexpected_eof_result(&result) {
        *saved = Some(Box::new(state));
    }
    result
}

fn is_unexpected_eof_result<T>(result: &Result<T, DecodeError>) -> bool {
    matches!(result, Err(DecodeError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof)
}

fn scan_column_header_state(
    reader: &mut ByteReader,
    options: &DecodeSettings,
    state: &mut ColumnHeaderScanState,
) -> Result<(String, ChType), DecodeError> {
    if state.name.is_none() {
        let Some(name) = state.name_scan.scan(reader)? else {
            state.offset = reader.position();
            return Err(need_more_bytes());
        };
        state.name = Some(name);
        state.offset = reader.position();
    }
    if state.ch_type.is_none() {
        let name = state.name.as_deref().unwrap_or("");
        if options.types_in_binary_format {
            let descriptor_start = *state.descriptor_start.get_or_insert(reader.position());
            if state.descriptor.is_none() {
                let mut probe = ByteReader::new(reader.remaining_slice());
                match read_binary_type(&mut probe) {
                    Ok(ch_type) => {
                        reader.skip(probe.position())?;
                        state.ch_type = Some(ch_type);
                    }
                    Err(BinaryTypeError::Io(error))
                        if error.kind() == io::ErrorKind::UnexpectedEof =>
                    {
                        state.descriptor = Some(BinaryTypeScanner::new());
                    }
                    Err(error) => return Err(binary_type_error(error, name)),
                }
            }
            if state.ch_type.is_none() {
                let Some(descriptor_scanner) = state.descriptor.as_mut() else {
                    return Err(DecodeError::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "binary column header has no descriptor scanner",
                    )));
                };
                if let Err(error) = descriptor_scanner.scan(reader) {
                    state.offset = reader.position();
                    return Err(binary_type_error(error, name));
                }
                let descriptor = reader.consumed_slice(descriptor_start)?;
                state.ch_type = Some(parse_complete_binary_type(descriptor, name)?);
            }
        } else {
            let Some(type_name) = state.text_type_scan.scan(reader)? else {
                state.offset = reader.position();
                return Err(need_more_bytes());
            };
            state.ch_type =
                Some(
                    parse_ch_type(&type_name).ok_or_else(|| DecodeError::UnsupportedType {
                        column: name.to_string(),
                        type_name,
                    })?,
                );
        }
        state.offset = reader.position();
    }

    if options.protocol_revision >= DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION {
        let marker = reader.read_u8()?;
        state.offset = reader.position();
        if marker != 0 {
            return Err(DecodeError::UnsupportedSerialization {
                column: state.name.clone().unwrap_or_default(),
                serialization_byte: marker,
            });
        }
    }

    let Some(ch_type) = state.ch_type.take() else {
        return Err(DecodeError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "column header scanner returned before its type completed",
        )));
    };
    let name = state.name.take().ok_or_else(|| {
        DecodeError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "column header scanner returned before its name completed",
        ))
    })?;
    validate_header_type(&name, &ch_type)?;
    Ok((name, ch_type))
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
    if let Some(type_name) = unsupported_header_type_name(ch_type) {
        Err(DecodeError::UnsupportedType {
            column: col_name.to_string(),
            type_name,
        })
    } else {
        Ok(())
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
///
/// This is a one-byte-per-item bound. A reservation whose per-item element is
/// wider than a byte (the `Field`/`Column` records here, the i64 aggregate
/// offsets) narrows it further with [`ByteReader::capacity_for`], so the
/// reservation tracks the items the remaining input could actually produce
/// rather than the raw count.
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
    options: &DecodeSettings,
    num_cols: usize,
    num_rows: usize,
) -> Result<ColBatch, DecodeError> {
    let mut allocation_budget = AllocationBudget::new(options.max_synthetic_allocation_bytes);
    check_header_count(num_cols, "column count", reader)?;

    // `check_header_count` bounds `num_cols` at one byte per column, but each
    // column also stores a `Field` and a `Column` several dozen bytes wide, so a
    // hostile count near `remaining()` could reserve many times the input. A
    // column header is at minimum a name-length varint and a type-length varint,
    // so two bytes; cap the reservation at what the remaining input could
    // actually frame. A real block always carries >= 2 header bytes per column,
    // so this never shrinks a legitimate reservation, it only defuses an
    // inflated count.
    let col_capacity = reader.capacity_for(num_cols, 2);
    let mut fields = Vec::with_capacity(col_capacity);
    let mut columns = Vec::with_capacity(col_capacity);

    for _ in 0..num_cols {
        let (col_name, ch_type) = read_column_header(reader, options)?;

        if num_rows == 0 {
            columns.push(empty_column(&ch_type));
        } else {
            // Type-aware row-count guard, applied per column now that the type is
            // known (the header is read here, so a single pre-loop guard could
            // not see it). Every pre-JSON type writes at least one byte per row
            // (a fixed/variable primitive, a null map, Array/Map offsets, a
            // LowCardinality index, a Variant/Dynamic discriminator, or the
            // Nothing/`Tuple()` placeholder byte), so a claimed row count above
            // the bytes remaining is impossible and rejected early, exactly as
            // the old global guard did. A `JSON` column, or a container that
            // bottoms out in one, can legitimately write ZERO bytes per row (a
            // pathless FLATTENED object under the flattened JSON serialization),
            // so it is bounded by its own read-before-allocate decode instead
            // (`decode_json`), keeping decode symmetric with the scan-only
            // walk, which never applied a row-count guard.
            if has_min_one_byte_per_row(&ch_type) {
                check_header_count(num_rows, "row count", reader)?;
            }
            columns.push(decode_column(
                reader,
                &ch_type,
                num_rows,
                &col_name,
                options,
                &mut allocation_budget,
            )?);
        }

        fields.push(Field {
            name: col_name,
            ch_type,
        });
    }

    let schema = Schema::new(fields);
    Ok(ColBatch::new(schema, columns, num_rows))
}

/// Whether every non-empty block of `ch_type` writes at least one byte per row,
/// which lets [`decode_block_body`] reject an impossible row count early.
///
/// True for every type except one that can bottom out in a `JSON` body with no
/// paths: a pathless FLATTENED `JSON` column writes zero bytes per row (no typed
/// paths, no dynamic paths, and FLATTENED carries no shared-data stream), and a
/// non-empty `Tuple` all of whose elements are such columns inherits that. A
/// `Nullable` wrapper (null map), `Array`/`Map` (offsets), `LowCardinality`
/// (index word plus per-row indexes), `Variant`/`Dynamic` (discriminators), and
/// the empty `Tuple()` (one placeholder byte) all still guarantee >= 1 byte per
/// row, so only a JSON leaf or a Tuple entirely of them returns false. Name
/// decorations resolve through [`ChType::physical_delegate`].
fn has_min_one_byte_per_row(ch_type: &ChType) -> bool {
    if let Some(under) = ch_type.physical_delegate_ref() {
        return has_min_one_byte_per_row(under.as_ref());
    }
    match ch_type {
        ChType::Json { .. } => false,
        ChType::Tuple(elements) => {
            elements.is_empty() || elements.iter().any(|(_, t)| has_min_one_byte_per_row(t))
        }
        _ => true,
    }
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
///   BlockInfo field, varint overflow, invalid UTF-8 in a header), which may be
///   surfaced before the whole block is buffered. Scan and decode
///   agree on acceptance or rejection and complete-block boundaries, not on
///   error wording or timing. Corrupt partial data can fail with invalid data
///   while decode still reports truncation.
///
/// This is the one-shot form: it restarts from the block start on every call.
/// `StreamDecoder` and [`BlockScanner`](crate::native::stream_decoder::BlockScanner)
/// instead run the resumable form of the same scan, which keeps a
/// `ScanProgress` checkpoint (completed columns, plus a task stack inside a
/// resumable column) across "need more bytes", so incremental feeds walk each
/// byte about once. Calling this after every partial read is quadratic in the
/// number of reads. The scan shares `read_block_info` with the real decode,
/// mirrors its column-header validation, and its column walkers consume exactly
/// the wire bytes the per-type decoders consume and reject what they reject.
pub fn block_end(data: &[u8], options: &DecodeOptions) -> Result<Option<usize>, DecodeError> {
    block_end_with_settings(data, &DecodeSettings::text(options))
}

/// Completeness scan for a block using binary data-type descriptors. This is
/// the binary-header counterpart of [`block_end`].
pub fn block_end_binary_types(
    data: &[u8],
    options: &DecodeOptions,
) -> Result<Option<usize>, DecodeError> {
    block_end_with_settings(data, &DecodeSettings::binary(options))
}

fn block_end_with_settings(
    data: &[u8],
    options: &DecodeSettings,
) -> Result<Option<usize>, DecodeError> {
    block_end_resume_with_settings(data, options, &mut ScanProgress::default())
}

/// Verified completeness-scan progress into the current partial block, held by
/// [`StreamDecoder`](crate::native::stream_decoder::StreamDecoder) between
/// feeds so the scan resumes where it stopped instead of re-walking the block
/// from its start on every feed. All offsets are relative to the block start
/// (byte 0 of the `data` slice passed to the scan), which keeps them valid
/// across the stream decoder's buffer compaction: compaction only drops bytes
/// before the block start.
///
/// A checkpoint survives only a "need more bytes" outcome (`Io(UnexpectedEof)`).
/// A complete block, a clean boundary, and a real decode error all clear it, so
/// stale progress can never leak into the next block or past an error.
#[derive(Debug, Default)]
pub(crate) struct ScanProgress {
    /// Whether this block's protocol revision has passed the supported-revision
    /// ceiling. Retained across partial feeds so streaming pays one comparison
    /// per block, then cleared with the rest of the checkpoint.
    revision_validated: bool,
    /// Column count and row count, once the block header (BlockInfo preamble
    /// plus the two count varints) has been fully parsed.
    header: Option<(usize, usize)>,
    /// Fully scanned top-level columns (header, state prefix, body, and state
    /// suffix all verified) and the offset just past the last one, initialized
    /// to the offset just past the block header.
    columns_done: usize,
    columns_end: usize,
    /// Partially scanned top-level column header, including incremental name
    /// and text-type strings or a binary descriptor grammar walk.
    column_header: Option<Box<ColumnHeaderScanState>>,
    /// A fully parsed fixed-width top-level column whose body is incomplete.
    /// Retaining only its byte range avoids both header reparsing and task
    /// storage on the primitive hot path.
    fixed_body: Option<FixedBodyState>,
    /// Mid-column checkpoint inside a column [`is_resumable`] accepts (fixed
    /// width values, String, Nullable, containers, LowCardinality, Variant,
    /// Dynamic, JSON, and AggregateFunction), covering prefixes and bodies.
    body: Option<Box<ResumableBodyState>>,
    synthetic_allocation_used: usize,
    /// Cumulative bytes the scan advanced over, across resumes. Backs the
    /// O(n) linearity guard in the stream decoder tests.
    #[cfg(test)]
    pub(crate) bytes_walked: u64,
}

impl ScanProgress {
    /// Offset the next scan resumes from, which is also the largest saved
    /// offset (checkpoints only move forward within a block).
    fn resume_offset(&self) -> usize {
        match &self.body {
            Some(body) => body.offset,
            None => self.fixed_body.as_ref().map_or_else(
                || {
                    self.column_header
                        .as_ref()
                        .map_or(self.columns_end, |header| header.offset)
                },
                |body| body.start,
            ),
        }
    }

    /// Drop all saved progress so the next scan restarts from the block start.
    fn reset(&mut self) {
        self.revision_validated = false;
        self.header = None;
        self.columns_done = 0;
        self.columns_end = 0;
        self.column_header = None;
        self.fixed_body = None;
        self.body = None;
        self.synthetic_allocation_used = 0;
    }

    #[cfg(test)]
    pub(crate) fn retained_header_string_bytes(&self) -> usize {
        self.column_header.as_ref().map_or(0, |header| {
            header.name.as_ref().map_or(0, |name| name.capacity())
        })
    }

    #[cfg(test)]
    pub(crate) fn header_waits_after_complete_type(&self) -> bool {
        self.column_header
            .as_ref()
            .is_some_and(|header| header.ch_type.is_some() && header.descriptor.is_none())
    }

    #[cfg(test)]
    /// Fixed task storage, used only after verifying task-owned heaps are empty.
    pub(crate) fn retained_task_bytes(&self) -> usize {
        let Some(body) = self.body.as_deref() else {
            return 0;
        };
        let prefix_boxes = body
            .prefix_tasks
            .iter()
            .map(|task| match task {
                PrefixScanTask::Dynamic(task) => std::mem::size_of_val(task.as_ref()),
                PrefixScanTask::Json(task) => {
                    std::mem::size_of_val(task.as_ref())
                        + task
                            .previous_path
                            .as_ref()
                            .map_or(0, |path| path.capacity())
                }
                _ => 0,
            })
            .sum::<usize>();
        std::mem::size_of::<ResumableBodyState>()
            + body.prefix_tasks.capacity() * std::mem::size_of::<PrefixScanTask>()
            + prefix_boxes
            + body.tasks.capacity() * std::mem::size_of::<BodyScanTask>()
    }

    #[cfg(test)]
    pub(crate) fn retained_tasks_have_empty_payloads(&self) -> bool {
        let Some(body) = self.body.as_deref() else {
            return true;
        };
        body.prefix_tasks.iter().all(|task| match task {
            PrefixScanTask::Dynamic(task) => {
                task.typed.capacity() == 0 && task.names.is_empty() && task.binary_scanner.is_none()
            }
            PrefixScanTask::DynamicRun { .. } | PrefixScanTask::Word(_) => true,
            PrefixScanTask::Type { .. } | PrefixScanTask::Json(_) => false,
        }) && body.tasks.iter().all(|task| match task {
            BodyScanTask::DynamicValuesRun { .. } | BodyScanTask::ChargeSynthetic { .. } => true,
            BodyScanTask::DynamicIndexes {
                children, counts, ..
            } => children.capacity() == 0 && counts.capacity() == 0,
            _ => false,
        })
    }
}

#[derive(Debug, Default)]
struct PartialStringScan {
    length: PartialVarint,
    len: Option<usize>,
    payload_start: Option<usize>,
    bytes_read: usize,
}

impl PartialStringScan {
    fn scan(&mut self, reader: &mut ByteReader) -> Result<Option<String>, DecodeError> {
        if self.len.is_none() {
            let Some(len) = read_partial_varint(reader, &mut self.length)? else {
                return Ok(None);
            };
            self.len = Some(varint_usize(len, "String length")?);
        }
        let len = self.len.unwrap_or(0);
        let payload_start = *self.payload_start.get_or_insert(reader.position());
        let remaining = len.saturating_sub(self.bytes_read);
        let take = remaining.min(reader.remaining());
        reader.skip(take)?;
        self.bytes_read += take;
        if self.bytes_read != len {
            return Ok(None);
        }
        std::str::from_utf8(reader.consumed_slice(payload_start)?)
            .map(str::to_owned)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error).into())
    }
}

/// Resumable framing state for one top-level column header.
#[derive(Debug)]
struct ColumnHeaderScanState {
    name_scan: PartialStringScan,
    name: Option<String>,
    text_type_scan: PartialStringScan,
    descriptor_start: Option<usize>,
    descriptor: Option<BinaryTypeScanner>,
    ch_type: Option<ChType>,
    offset: usize,
}

#[derive(Debug, Clone, Copy)]
struct FixedBodyState {
    start: usize,
    end: usize,
}

/// Resumable scan state for one top-level column: its static and runtime-shaped
/// state prefix, retained self-describing states, then its body task stack.
#[derive(Debug)]
struct ResumableBodyState {
    column: String,
    root_type: ChType,
    num_rows: usize,
    settings: DecodeSettings,
    prefix_tasks: Vec<PrefixScanTask>,
    states: Vec<StatePrefix>,
    state_cursor: usize,
    body_seeded: bool,
    allocation_budget: AllocationBudget,
    tasks: Vec<BodyScanTask>,
    /// Offset of the first unverified byte, relative to the block start. The
    /// task on top of the stack resumes reading exactly here.
    offset: usize,
}

impl ResumableBodyState {
    /// Seed the prefix traversal for one column. The body is scheduled only
    /// after every runtime type table and nested prefix is retained.
    fn new(
        column: String,
        ch_type: ChType,
        num_rows: usize,
        offset: usize,
        settings: DecodeSettings,
    ) -> Self {
        Self {
            column,
            root_type: ch_type.clone(),
            num_rows,
            settings,
            prefix_tasks: vec![PrefixScanTask::Type { ch_type, depth: 0 }],
            states: Vec::new(),
            state_cursor: 0,
            body_seeded: false,
            allocation_budget: AllocationBudget::new(settings.max_synthetic_allocation_bytes),
            tasks: Vec::new(),
            offset,
        }
    }
}

#[derive(Debug, Default)]
struct PartialVarint {
    value: u64,
    shift: u32,
}

#[derive(Debug)]
enum PrefixScanTask {
    Type {
        ch_type: ChType,
        depth: usize,
    },
    Word(PrefixWord),
    Dynamic(Box<DynamicPrefixTask>),
    DynamicRun {
        remaining: usize,
        max_types: u8,
        depth: usize,
    },
    Json(Box<JsonPrefixTask>),
}

#[derive(Debug)]
struct DynamicPrefixTask {
    max_types: u8,
    depth: usize,
    version: Option<u64>,
    legacy: Option<PartialVarint>,
    count: Option<usize>,
    count_varint: PartialVarint,
    typed: Vec<(String, ChType)>,
    names: std::collections::BTreeSet<String>,
    entry_length: PartialVarint,
    entry_len: Option<usize>,
    binary_start: Option<usize>,
    binary_scanner: Option<BinaryTypeScanner>,
}

impl DynamicPrefixTask {
    fn new(max_types: u8, depth: usize) -> Self {
        Self {
            max_types,
            depth,
            version: None,
            legacy: None,
            count: None,
            count_varint: PartialVarint::default(),
            typed: Vec::new(),
            names: std::collections::BTreeSet::new(),
            entry_length: PartialVarint::default(),
            entry_len: None,
            binary_start: None,
            binary_scanner: None,
        }
    }
}

fn dynamic_prefix_task(max_types: u8, depth: usize) -> PrefixScanTask {
    PrefixScanTask::Dynamic(Box::new(DynamicPrefixTask::new(max_types, depth)))
}

#[derive(Debug)]
struct JsonPrefixTask {
    max_dynamic_paths: u32,
    max_dynamic_types: u8,
    typed_paths: Vec<(String, ChType)>,
    depth: usize,
    version: Option<u64>,
    kind: Option<JsonWireKind>,
    legacy: Option<PartialVarint>,
    count: Option<usize>,
    count_varint: PartialVarint,
    paths_read: usize,
    previous_path: Option<String>,
    path_length: PartialVarint,
    path_len: Option<usize>,
}

/// One fixed 8-byte bulk-state-prefix word of a resumable column.
#[derive(Debug, Clone, Copy)]
enum PrefixWord {
    /// `LowCardinality` key version, which must be 1
    /// ([`read_low_cardinality_state`]).
    LowCardinalityKeyVersion,
    /// `Variant` discriminator mode, which must be BASIC mode 0
    /// ([`read_variant_state`]).
    VariantMode,
}

/// A bounded piece of the resumable column traversal. Every task that reads a
/// fixed 8-byte word ([`BodyScanTask::reads_word`]) is parked whole until the
/// word is buffered, so a word is never split across resumes.
#[derive(Debug)]
enum BodyScanTask {
    Values {
        ch_type: ChType,
        num_rows: usize,
    },
    DynamicValuesRun {
        remaining: usize,
        num_rows: usize,
    },
    Bytes {
        remaining: usize,
    },
    Strings {
        rows_remaining: usize,
        length_value: u64,
        length_shift: u32,
        length_complete: bool,
        payload_remaining: usize,
        /// Running Arrow i32 offset of this run, checked exactly as one
        /// [`decode_string_data`] call checks it.
        data_offset: i32,
    },
    Offsets {
        rows_remaining: usize,
        previous: u64,
        children: Vec<ChType>,
    },
    ValidateOffsetCount {
        count: u64,
        children: Vec<ChType>,
    },
    /// `LowCardinality` index-serialization word, the first body word
    /// [`decode_low_cardinality`] reads.
    LowCardinality {
        dict_type: ChType,
        num_rows: usize,
    },
    /// `LowCardinality` per-block dictionary size word; the dictionary body
    /// follows as an ordinary value run of `dict_type`.
    LowCardinalityDictionary {
        dict_type: ChType,
        num_rows: usize,
        index_width: usize,
    },
    /// `LowCardinality` index row-count word, after the dictionary body.
    LowCardinalityRows {
        num_rows: usize,
        index_width: usize,
        num_keys: usize,
    },
    /// `LowCardinality` raw index array, validated against the dictionary.
    LowCardinalityIndexes {
        rows_remaining: usize,
        index_width: usize,
        num_keys: usize,
    },
    /// `Variant` one-byte discriminators, counted per alternative; the dense
    /// alternative runs follow once every row is counted.
    VariantDiscriminators {
        alternatives: Vec<ChType>,
        counts: Vec<usize>,
        rows_done: usize,
        rows_remaining: usize,
    },
    DynamicIndexes {
        children: Vec<DynamicStateChild>,
        counts: Vec<usize>,
        rows_remaining: usize,
        width: usize,
        kind: DynamicWireKind,
    },
    AggregateCount {
        rows_remaining: usize,
        shift: u32,
    },
    AggregateZeroBytes {
        rows_remaining: usize,
    },
    AggregateNullableSum {
        rows_remaining: usize,
        state_width: usize,
    },
    ChargeSynthetic {
        bytes: usize,
        what: &'static str,
    },
}

impl BodyScanTask {
    /// Whether the task begins by reading one fixed 8-byte LE word.
    fn reads_word(&self) -> bool {
        matches!(
            self,
            BodyScanTask::LowCardinality { .. }
                | BodyScanTask::LowCardinalityDictionary { .. }
                | BodyScanTask::LowCardinalityRows { .. }
        )
    }
}

fn is_unexpected_eof(error: &DecodeError) -> bool {
    matches!(error, DecodeError::Io(error) if error.kind() == io::ErrorKind::UnexpectedEof)
}

fn need_more_bytes() -> DecodeError {
    // A bare-kind `io::Error` carries no heap payload.
    DecodeError::Io(io::Error::from(io::ErrorKind::UnexpectedEof))
}

/// Resumable [`block_end`]: on "need more bytes" the verified progress is kept
/// in `progress`, and the next call continues from it instead of re-walking
/// the block from its start. Accepts and rejects exactly the same streams as
/// [`block_end`]; `progress` only changes where the walk starts.
pub(crate) fn block_end_resume(
    data: &[u8],
    options: &DecodeOptions,
    progress: &mut ScanProgress,
) -> Result<Option<usize>, DecodeError> {
    block_end_resume_with_settings(data, &DecodeSettings::text(options), progress)
}

/// Binary-descriptor counterpart of [`block_end_resume`].
pub(crate) fn block_end_binary_types_resume(
    data: &[u8],
    options: &DecodeOptions,
    progress: &mut ScanProgress,
) -> Result<Option<usize>, DecodeError> {
    block_end_resume_with_settings(data, &DecodeSettings::binary(options), progress)
}

fn block_end_resume_with_settings(
    data: &[u8],
    settings: &DecodeSettings,
    progress: &mut ScanProgress,
) -> Result<Option<usize>, DecodeError> {
    block_scan_resume_with_settings(data, settings, progress)
        .map(|block| block.map(|block| block.end))
}

/// Shape and boundary found by the scan-only block walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScannedBlock {
    pub(crate) end: usize,
    pub(crate) num_columns: usize,
    pub(crate) num_rows: usize,
}

pub(crate) fn scan_block_resume(
    data: &[u8],
    options: &DecodeOptions,
    progress: &mut ScanProgress,
) -> Result<Option<ScannedBlock>, DecodeError> {
    block_scan_resume_with_settings(data, &DecodeSettings::text(options), progress)
}

pub(crate) fn scan_block_binary_types_resume(
    data: &[u8],
    options: &DecodeOptions,
    progress: &mut ScanProgress,
) -> Result<Option<ScannedBlock>, DecodeError> {
    block_scan_resume_with_settings(data, &DecodeSettings::binary(options), progress)
}

fn block_scan_resume_with_settings(
    data: &[u8],
    settings: &DecodeSettings,
    progress: &mut ScanProgress,
) -> Result<Option<ScannedBlock>, DecodeError> {
    if !progress.revision_validated {
        if let Err(error) = ensure_supported_protocol_revision(settings.protocol_revision) {
            progress.reset();
            return Err(error);
        }
        progress.revision_validated = true;
    }

    // A saved offset past the data cannot describe this buffer; that is a
    // caller wiring bug, so fail loudly in debug builds, then drop the
    // checkpoint and rescan from the block start.
    debug_assert!(
        progress.resume_offset() <= data.len(),
        "scan checkpoint offset {} exceeds data length {}",
        progress.resume_offset(),
        data.len()
    );
    if progress.resume_offset() > data.len() {
        progress.reset();
    }

    #[cfg(test)]
    let resume_from = progress.resume_offset();

    let mut reader = ByteReader::new(data);
    let result = scan_block_body_resume(&mut reader, settings, progress);

    #[cfg(test)]
    {
        progress.bytes_walked += reader.position().saturating_sub(resume_from) as u64;
    }

    // Keep the checkpoint only across "need more bytes".
    match &result {
        Err(DecodeError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {}
        _ => progress.reset(),
    }
    result
}

/// Body of the resumable completeness scan. Jumps to the finest checkpoint in
/// `progress`, then records new progress after the header, after each fully
/// scanned column, and inside resumable columns ([`is_resumable`]).
fn scan_block_body_resume(
    reader: &mut ByteReader,
    settings: &DecodeSettings,
    progress: &mut ScanProgress,
) -> Result<Option<ScannedBlock>, DecodeError> {
    let (num_cols, num_rows) = if let Some((num_cols, num_rows)) = progress.header {
        if let Some(body) = progress.body.as_mut() {
            // The wrapper verified `body.offset` lies within the data.
            reader.skip(body.offset)?;
            scan_resumable_body(reader, body)?;
            progress.synthetic_allocation_used = body.allocation_budget.used;
            progress.body = None;
            progress.columns_done += 1;
        } else if let Some(body) = progress.fixed_body {
            if reader.remaining() < body.end {
                reader.skip(body.start)?;
                return Err(need_more_bytes());
            }
            reader.skip(body.end)?;
            progress.fixed_body = None;
            progress.columns_done += 1;
        } else if progress.column_header.is_none() {
            reader.skip(progress.columns_end)?;
        }
        (num_cols, num_rows)
    } else {
        if settings.protocol_revision > 0 {
            if !read_block_info(reader)? {
                return Ok(None);
            }
        } else if reader.remaining() == 0 {
            return Ok(None);
        }
        let num_cols = varint_usize(reader.read_varint()?, "column count")?;
        let num_rows = varint_usize(reader.read_varint()?, "row count")?;
        progress.header = Some((num_cols, num_rows));
        (num_cols, num_rows)
    };
    if progress.column_header.is_none() {
        progress.columns_end = reader.position();
    }

    while progress.columns_done < num_cols {
        let (name, ch_type) = scan_column_header(reader, settings, &mut progress.column_header)?;
        if num_rows > 0 {
            if let Some(bytes) = fixed_width_top_level_bytes(&ch_type, num_rows, &name)? {
                if reader.remaining() < bytes {
                    let start = reader.position();
                    let end = start.checked_add(bytes).ok_or_else(need_more_bytes)?;
                    progress.fixed_body = Some(FixedBodyState { start, end });
                    return Err(need_more_bytes());
                }
                reader.skip(bytes)?;
            } else if is_resumable(&ch_type) {
                // One pass, complete or partial: the task walker is the only
                // walker for these types, so a complete buffer is not walked
                // twice and a partial one keeps its exact checkpoint.
                let mut body =
                    ResumableBodyState::new(name, ch_type, num_rows, reader.position(), *settings);
                body.allocation_budget.used = progress.synthetic_allocation_used;
                if let Err(error) = scan_resumable_body(reader, &mut body) {
                    progress.synthetic_allocation_used = body.allocation_budget.used;
                    if is_unexpected_eof(&error) {
                        progress.body = Some(Box::new(body));
                    }
                    return Err(error);
                }
                progress.synthetic_allocation_used = body.allocation_budget.used;
            } else {
                skip_column_data(reader, &ch_type, num_rows, &name, settings)?;
            }
        }
        progress.columns_done += 1;
        progress.columns_end = reader.position();
    }

    Ok(Some(ScannedBlock {
        end: reader.position(),
        num_columns: num_cols,
        num_rows,
    }))
}

/// Compute the complete body size for top-level types whose Native payload is
/// one fixed byte run and has no state prefix or suffix.
fn fixed_width_top_level_bytes(
    ch_type: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<Option<usize>, DecodeError> {
    if let Some(delegate) = ch_type.physical_delegate_ref() {
        return fixed_width_top_level_bytes(delegate.as_ref(), num_rows, column);
    }
    if matches!(ch_type, ChType::Tuple(elements) if elements.is_empty()) {
        return Ok(Some(num_rows));
    }
    if is_fixed_width_leaf(ch_type) {
        return fixed_width_body_bytes(ch_type, num_rows, column).map(Some);
    }
    Ok(None)
}

/// Whether a top-level column of `ch_type` can use the resumable task walker.
/// This includes self-describing Dynamic and JSON prefixes and the registered
/// AggregateFunction state codecs. Returning `false` remains safe: the column
/// then goes through [`skip_column_data`], the reference walker.
fn is_resumable(ch_type: &ChType) -> bool {
    if let Some(delegate) = ch_type.physical_delegate_ref() {
        return is_resumable(delegate.as_ref());
    }
    match ch_type {
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
        | ChType::QBit { .. }
        | ChType::Int128
        | ChType::UInt128
        | ChType::Int256
        | ChType::UInt256
        | ChType::String
        | ChType::FixedString(_)
        | ChType::Date
        | ChType::Date32
        | ChType::DateTime { .. }
        | ChType::DateTime64 { .. }
        | ChType::Time
        | ChType::Time64 { .. }
        | ChType::Interval(_)
        | ChType::Decimal { .. }
        | ChType::Uuid
        | ChType::Ipv4
        | ChType::Ipv6
        | ChType::Enum8 { .. }
        | ChType::Enum16 { .. } => true,
        ChType::Nullable(inner) => is_resumable_nullable_inner(inner),
        // The dictionary value type is validated by the task itself, exactly
        // where `decode_low_cardinality` validates it.
        ChType::LowCardinality(_) => true,
        ChType::Array(inner) => is_resumable(inner),
        ChType::Tuple(elements) => elements.iter().all(|(_, element)| is_resumable(element)),
        ChType::Map(key, value) => is_resumable(key) && is_resumable(value),
        ChType::Variant(alternatives) => alternatives.iter().all(is_resumable),
        ChType::Dynamic { .. } | ChType::Json { .. } | ChType::AggregateFunction { .. } => true,
        // Resolved by `physical_delegate_ref` above; unreachable, and `false`
        // is the safe answer if that ever changes.
        ChType::SimpleAggregateFunction { .. }
        | ChType::Geo(_)
        | ChType::Geometry
        | ChType::Nested(_) => false,
    }
}

/// Whether `Nullable(inner)` walks the same bytes in the task walker as in
/// [`skip_values`]. That walker unwraps the null map, expands at most ONE alias
/// level, then accepts only a Tuple or a leaf body; anything else (a nested
/// wrapper or container, a chained alias) is an `UnsupportedType` there, so it
/// must stay on that path here too rather than be silently accepted.
fn is_resumable_nullable_inner(inner: &ChType) -> bool {
    let delegate = inner.physical_delegate_ref();
    let inner = delegate.as_deref().unwrap_or(inner);
    match inner {
        ChType::Tuple(elements) => elements.iter().all(|(_, element)| is_resumable(element)),
        ChType::Json { .. } => true,
        leaf => is_fixed_width_leaf(leaf) || matches!(leaf, ChType::String),
    }
}

fn read_partial_varint(
    reader: &mut ByteReader,
    partial: &mut PartialVarint,
) -> Result<Option<u64>, DecodeError> {
    while reader.remaining() != 0 {
        let byte = reader.read_u8()?;
        partial.value |= u64::from(byte & 0x7f) << partial.shift;
        if byte & 0x80 == 0 {
            return Ok(Some(partial.value));
        }
        partial.shift += 7;
        if partial.shift >= 64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "varint overflow").into());
        }
    }
    Ok(None)
}

fn push_prefix_type(tasks: &mut Vec<PrefixScanTask>, ch_type: ChType, depth: usize) {
    tasks.push(PrefixScanTask::Type { ch_type, depth });
}

fn parse_complete_binary_type(bytes: &[u8], column: &str) -> Result<ChType, DecodeError> {
    let mut descriptor = ByteReader::new(bytes);
    let parsed = read_binary_type(&mut descriptor).map_err(|error| match error {
        BinaryTypeError::Io(error) => DecodeError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("binary type boundary scanner/parser mismatch: {error}"),
        )),
        other => binary_type_error(other, column),
    })?;
    if descriptor.position() != bytes.len() {
        return Err(DecodeError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "binary type boundary scanner/parser mismatch for column '{column}': parser consumed {} of {} bytes",
                descriptor.position(),
                bytes.len()
            ),
        )));
    }
    Ok(parsed)
}

fn binary_type_error(error: BinaryTypeError, column: &str) -> DecodeError {
    match error {
        BinaryTypeError::Io(error) => DecodeError::Io(error),
        BinaryTypeError::Invalid(reason) | BinaryTypeError::Unsupported(reason) => {
            DecodeError::UnsupportedType {
                column: column.to_string(),
                type_name: reason,
            }
        }
    }
}

fn scan_prefix_task(
    reader: &mut ByteReader,
    state: &mut ResumableBodyState,
    task: PrefixScanTask,
) -> Result<(), DecodeError> {
    match task {
        PrefixScanTask::Word(word) => {
            if reader.remaining() < 8 {
                state.prefix_tasks.push(PrefixScanTask::Word(word));
                return Err(need_more_bytes());
            }
            let value = reader.read_u64_le()?;
            match word {
                PrefixWord::LowCardinalityKeyVersion if value != LOW_CARDINALITY_KEY_VERSION => {
                    return Err(DecodeError::InvalidLowCardinality {
                        column: state.column.clone(),
                        reason: "key version is not 1 (SharedDictionariesWithAdditionalKeys)",
                    });
                }
                PrefixWord::VariantMode if value != 0 => {
                    return Err(DecodeError::InvalidVariant {
                        column: state.column.clone(),
                        reason: format!(
                            "discriminator mode {value} is not BASIC mode 0 emitted by FORMAT Native"
                        ),
                    });
                }
                _ => {}
            }
        }
        PrefixScanTask::Type { ch_type, depth } => {
            if let Some(delegate) = ch_type.physical_delegate() {
                push_prefix_type(&mut state.prefix_tasks, delegate, depth + 1);
            } else {
                match ch_type {
                    ChType::LowCardinality(_) => state
                        .prefix_tasks
                        .push(PrefixScanTask::Word(PrefixWord::LowCardinalityKeyVersion)),
                    ChType::Variant(alternatives) => {
                        for alternative in alternatives.into_iter().rev() {
                            push_prefix_type(&mut state.prefix_tasks, alternative, depth + 1);
                        }
                        state
                            .prefix_tasks
                            .push(PrefixScanTask::Word(PrefixWord::VariantMode));
                    }
                    ChType::Array(inner) | ChType::Nullable(inner) => {
                        push_prefix_type(&mut state.prefix_tasks, *inner, depth + 1)
                    }
                    ChType::Tuple(elements) => {
                        for (_, element) in elements.into_iter().rev() {
                            push_prefix_type(&mut state.prefix_tasks, element, depth + 1);
                        }
                    }
                    ChType::Map(key, value) => {
                        push_prefix_type(&mut state.prefix_tasks, *value, depth + 1);
                        push_prefix_type(&mut state.prefix_tasks, *key, depth + 1);
                    }
                    ChType::Dynamic { max_types } => state
                        .prefix_tasks
                        .push(dynamic_prefix_task(max_types, depth)),
                    ChType::Json {
                        max_dynamic_paths,
                        max_dynamic_types,
                        typed_paths,
                        ..
                    } => state
                        .prefix_tasks
                        .push(PrefixScanTask::Json(Box::new(JsonPrefixTask {
                            max_dynamic_paths,
                            max_dynamic_types,
                            typed_paths,
                            depth,
                            version: None,
                            kind: None,
                            legacy: None,
                            count: None,
                            count_varint: PartialVarint::default(),
                            paths_read: 0,
                            previous_path: None,
                            path_length: PartialVarint::default(),
                            path_len: None,
                        }))),
                    _ => {}
                }
            }
        }
        PrefixScanTask::Dynamic(mut dynamic) => {
            if dynamic.depth >= MAX_TYPE_DEPTH {
                return Err(invalid_dynamic(
                    &state.column,
                    format!("Dynamic nesting exceeds the maximum type depth {MAX_TYPE_DEPTH}"),
                ));
            }
            if dynamic.version.is_none() {
                if reader.remaining() < 8 {
                    state.prefix_tasks.push(PrefixScanTask::Dynamic(dynamic));
                    return Err(need_more_bytes());
                }
                let version = reader.read_u64_le()?;
                match version {
                    1 => dynamic.legacy = Some(PartialVarint::default()),
                    2 | 3 => {}
                    4 => {
                        return Err(invalid_dynamic(
                            &state.column,
                            "structure word 4 (V3) is not emitted by NativeWriter",
                        ))
                    }
                    other => {
                        return Err(invalid_dynamic(
                            &state.column,
                            format!("unknown structure word {other}"),
                        ))
                    }
                }
                dynamic.version = Some(version);
            }
            if let Some(partial) = dynamic.legacy.as_mut() {
                if read_partial_varint(reader, partial)?.is_none() {
                    state.prefix_tasks.push(PrefixScanTask::Dynamic(dynamic));
                    return Err(need_more_bytes());
                }
                dynamic.legacy = None;
            }
            if dynamic.count.is_none() {
                let Some(count) = read_partial_varint(reader, &mut dynamic.count_varint)? else {
                    state.prefix_tasks.push(PrefixScanTask::Dynamic(dynamic));
                    return Err(need_more_bytes());
                };
                let count = varint_usize(count, "Dynamic runtime type count")?;
                let variant = dynamic.version != Some(3);
                if variant && count > 254 {
                    return Err(invalid_dynamic(
                        &state.column,
                        format!("direct runtime type count {count} exceeds 254"),
                    ));
                }
                if variant && count > dynamic.max_types as usize {
                    return Err(invalid_dynamic(
                        &state.column,
                        format!(
                            "direct runtime type count {count} exceeds max_types={}",
                            dynamic.max_types
                        ),
                    ));
                }
                if count >= u32::MAX as usize {
                    return Err(invalid_dynamic(
                        &state.column,
                        "runtime type count exceeds the u32 block-local routing model",
                    ));
                }
                dynamic.typed.reserve(reader.capacity_for(count, 1));
                dynamic.count = Some(count);
            }
            let count = dynamic.count.unwrap_or(0);
            while dynamic.typed.len() < count {
                let ch_type = if state.settings.types_in_binary_format {
                    let start = *dynamic.binary_start.get_or_insert(reader.position());
                    let scanner = dynamic
                        .binary_scanner
                        .get_or_insert_with(BinaryTypeScanner::new);
                    if let Err(error) = scanner.scan(reader) {
                        match error {
                            BinaryTypeError::Io(error)
                                if error.kind() == io::ErrorKind::UnexpectedEof =>
                            {
                                state.prefix_tasks.push(PrefixScanTask::Dynamic(dynamic));
                                return Err(error.into());
                            }
                            BinaryTypeError::Io(error) => return Err(error.into()),
                            BinaryTypeError::Invalid(reason)
                            | BinaryTypeError::Unsupported(reason) => {
                                return Err(invalid_dynamic(&state.column, reason))
                            }
                        }
                    }
                    let bytes = reader.consumed_slice(start)?;
                    let parsed = parse_complete_binary_type(bytes, &state.column)?;
                    dynamic.binary_start = None;
                    dynamic.binary_scanner = None;
                    parsed
                } else {
                    if dynamic.entry_len.is_none() {
                        let Some(len) = read_partial_varint(reader, &mut dynamic.entry_length)?
                        else {
                            state.prefix_tasks.push(PrefixScanTask::Dynamic(dynamic));
                            return Err(need_more_bytes());
                        };
                        dynamic.entry_len = Some(varint_usize(len, "String length")?);
                    }
                    let len = dynamic.entry_len.unwrap_or(0);
                    if reader.remaining() < len {
                        state.prefix_tasks.push(PrefixScanTask::Dynamic(dynamic));
                        return Err(need_more_bytes());
                    }
                    let bytes = reader.read_slice(len)?;
                    let type_name = std::str::from_utf8(bytes)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    let parsed =
                        parse_ch_type(type_name).ok_or_else(|| DecodeError::UnsupportedType {
                            column: state.column.clone(),
                            type_name: type_name.to_string(),
                        })?;
                    dynamic.entry_len = None;
                    dynamic.entry_length = PartialVarint::default();
                    parsed
                };
                let canonical = ch_type.to_string();
                if resolves_to_nothing(&ch_type) || !is_valid_variant_alternative(&ch_type) {
                    return Err(invalid_dynamic(
                        &state.column,
                        format!("runtime type {canonical} is not legal in Dynamic"),
                    ));
                }
                if let Some(unsupported) = unsupported_header_type_name(&ch_type) {
                    return Err(DecodeError::UnsupportedType {
                        column: state.column.clone(),
                        type_name: unsupported,
                    });
                }
                if !dynamic.names.insert(canonical.clone()) {
                    return Err(invalid_dynamic(
                        &state.column,
                        format!("duplicate canonical runtime type {canonical}"),
                    ));
                }
                dynamic.typed.push((canonical, ch_type));
            }
            let kind = if dynamic.version == Some(3) {
                DynamicWireKind::Flattened
            } else {
                DynamicWireKind::Variant
            };
            let children = if kind == DynamicWireKind::Variant {
                if reader.remaining() < 8 {
                    state.prefix_tasks.push(PrefixScanTask::Dynamic(dynamic));
                    return Err(need_more_bytes());
                }
                let mode = reader.read_u64_le()?;
                if mode != 0 {
                    return Err(invalid_dynamic(
                        &state.column,
                        format!("Variant discriminator mode {mode} is not BASIC mode 0"),
                    ));
                }
                let mut global: Vec<_> = dynamic
                    .typed
                    .into_iter()
                    .map(|(name, ty)| (name, DynamicStateChild::Typed(ty)))
                    .collect();
                global.push(("SharedVariant".to_string(), DynamicStateChild::Shared));
                global.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                global
                    .into_iter()
                    .map(|(_, child)| child)
                    .collect::<Vec<_>>()
            } else {
                dynamic
                    .typed
                    .into_iter()
                    .map(|(_, ty)| DynamicStateChild::Typed(ty))
                    .collect()
            };
            for child in children.iter().rev() {
                if let DynamicStateChild::Typed(ch_type) = child {
                    push_prefix_type(&mut state.prefix_tasks, ch_type.clone(), dynamic.depth + 1);
                }
            }
            state
                .states
                .push(StatePrefix::Dynamic(DynamicState { kind, children }));
        }
        PrefixScanTask::DynamicRun {
            remaining,
            max_types,
            depth,
        } => {
            if remaining != 0 {
                state.prefix_tasks.push(PrefixScanTask::DynamicRun {
                    remaining: remaining - 1,
                    max_types,
                    depth,
                });
                state
                    .prefix_tasks
                    .push(dynamic_prefix_task(max_types, depth));
            }
        }
        PrefixScanTask::Json(mut json) => {
            if json.depth >= MAX_TYPE_DEPTH {
                return Err(invalid_json(
                    &state.column,
                    format!("JSON nesting exceeds the maximum type depth {MAX_TYPE_DEPTH}"),
                ));
            }
            if json.version.is_none() {
                if reader.remaining() < 8 {
                    state.prefix_tasks.push(PrefixScanTask::Json(json));
                    return Err(need_more_bytes());
                }
                let version = reader.read_u64_le()?;
                let kind = match version {
                    0 | 2 => JsonWireKind::Structured,
                    1 => JsonWireKind::Text,
                    3 => JsonWireKind::Flattened,
                    4 => {
                        return Err(invalid_json(
                            &state.column,
                            "structure word 4 (V3) is not emitted by NativeWriter",
                        ))
                    }
                    other => {
                        return Err(invalid_json(
                            &state.column,
                            format!("unknown JSON structure word {other}"),
                        ))
                    }
                };
                json.version = Some(version);
                json.kind = Some(kind);
                if version == 0 {
                    json.legacy = Some(PartialVarint::default());
                }
                if kind == JsonWireKind::Text {
                    state.states.push(StatePrefix::Json(JsonState {
                        kind,
                        dynamic_paths: Vec::new(),
                        dynamic_path_count: 0,
                    }));
                    state.offset = reader.position();
                    return Ok(());
                }
            }
            if let Some(partial) = json.legacy.as_mut() {
                if read_partial_varint(reader, partial)?.is_none() {
                    state.prefix_tasks.push(PrefixScanTask::Json(json));
                    return Err(need_more_bytes());
                }
                json.legacy = None;
            }
            if json.count.is_none() {
                let Some(count) = read_partial_varint(reader, &mut json.count_varint)? else {
                    state.prefix_tasks.push(PrefixScanTask::Json(json));
                    return Err(need_more_bytes());
                };
                let count = varint_usize(count, "JSON dynamic path count")?;
                if json.kind == Some(JsonWireKind::Structured)
                    && count > json.max_dynamic_paths as usize
                {
                    return Err(invalid_json(
                        &state.column,
                        format!(
                            "dynamic path count {count} exceeds max_dynamic_paths={}",
                            json.max_dynamic_paths
                        ),
                    ));
                }
                json.count = Some(count);
            }
            let count = json.count.unwrap_or(0);
            while json.paths_read < count {
                if json.path_len.is_none() {
                    let Some(len) = read_partial_varint(reader, &mut json.path_length)? else {
                        state.prefix_tasks.push(PrefixScanTask::Json(json));
                        return Err(need_more_bytes());
                    };
                    json.path_len = Some(varint_usize(len, "String length")?);
                }
                let len = json.path_len.unwrap_or(0);
                if reader.remaining() < len {
                    state.prefix_tasks.push(PrefixScanTask::Json(json));
                    return Err(need_more_bytes());
                }
                let path = std::str::from_utf8(reader.read_slice(len)?)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                    .to_string();
                if json
                    .previous_path
                    .as_ref()
                    .is_some_and(|last| path <= *last)
                {
                    return Err(invalid_json(
                        &state.column,
                        "dynamic paths are not strictly increasing (sorted, unique)",
                    ));
                }
                json.previous_path = Some(path);
                json.paths_read += 1;
                json.path_len = None;
                json.path_length = PartialVarint::default();
            }
            let kind = json.kind.unwrap_or(JsonWireKind::Text);
            let num_dynamic = json.paths_read;
            state.states.push(StatePrefix::Json(JsonState {
                kind,
                dynamic_paths: Vec::new(),
                dynamic_path_count: num_dynamic,
            }));
            if num_dynamic != 0 {
                state.prefix_tasks.push(PrefixScanTask::DynamicRun {
                    remaining: num_dynamic,
                    max_types: json.max_dynamic_types,
                    depth: json.depth + 1,
                });
            }
            for (_, ch_type) in json.typed_paths.into_iter().rev() {
                push_prefix_type(&mut state.prefix_tasks, ch_type, json.depth + 1);
            }
        }
    }
    state.offset = reader.position();
    Ok(())
}

/// Resume a resumable column without materializing row or column buffers.
///
/// Consumes exactly the bytes [`skip_column_data`] (and so `decode_column`)
/// consumes, with the same acceptance or rejection of complete blocks. Error
/// wording and timing can differ for corrupt partial input. It can stop at any
/// byte and continue later from `state.offset`. The task stack is bounded by
/// type metadata, never by row count.
fn scan_resumable_body(
    reader: &mut ByteReader,
    state: &mut ResumableBodyState,
) -> Result<(), DecodeError> {
    while let Some(task) = state.prefix_tasks.pop() {
        if let Err(error) = scan_prefix_task(reader, state, task) {
            if is_unexpected_eof(&error) {
                state.offset = reader.position();
            }
            return Err(error);
        }
    }
    if !state.body_seeded {
        state.tasks.push(BodyScanTask::Values {
            ch_type: state.root_type.clone(),
            num_rows: state.num_rows,
        });
        state.body_seeded = true;
    }
    while let Some(task) = state.tasks.pop() {
        if task.reads_word() && reader.remaining() < 8 {
            state.tasks.push(task);
            return Err(need_more_bytes());
        }
        match task {
            BodyScanTask::Values { ch_type, num_rows } => {
                if let Err(error) = push_value_tasks(
                    &mut state.tasks,
                    &ch_type,
                    num_rows,
                    &state.column,
                    &state.states,
                    &mut state.state_cursor,
                ) {
                    if is_unexpected_eof(&error) {
                        state.tasks.push(BodyScanTask::Values { ch_type, num_rows });
                    }
                    return Err(error);
                }
            }
            BodyScanTask::DynamicValuesRun {
                remaining,
                num_rows,
            } => {
                if remaining != 0 {
                    state.tasks.push(BodyScanTask::DynamicValuesRun {
                        remaining: remaining - 1,
                        num_rows,
                    });
                    state.tasks.push(BodyScanTask::Values {
                        ch_type: ChType::Dynamic { max_types: 0 },
                        num_rows,
                    });
                }
            }
            BodyScanTask::Bytes { mut remaining } => {
                let consumed = remaining.min(reader.remaining());
                reader.skip(consumed)?;
                remaining -= consumed;
                state.offset = reader.position();
                if remaining != 0 {
                    state.tasks.push(BodyScanTask::Bytes { remaining });
                    return Err(need_more_bytes());
                }
            }
            BodyScanTask::Strings {
                mut rows_remaining,
                mut length_value,
                mut length_shift,
                mut length_complete,
                mut payload_remaining,
                mut data_offset,
            } => {
                while rows_remaining != 0 {
                    if payload_remaining != 0 {
                        let consumed = payload_remaining.min(reader.remaining());
                        reader.skip(consumed)?;
                        payload_remaining -= consumed;
                        state.offset = reader.position();
                        if payload_remaining != 0 {
                            state.tasks.push(BodyScanTask::Strings {
                                rows_remaining,
                                length_value,
                                length_shift,
                                length_complete,
                                payload_remaining,
                                data_offset,
                            });
                            return Err(need_more_bytes());
                        }
                        rows_remaining -= 1;
                        continue;
                    }

                    if !length_complete {
                        let byte = match reader.read_u8() {
                            Ok(byte) => byte,
                            Err(error) => {
                                state.tasks.push(BodyScanTask::Strings {
                                    rows_remaining,
                                    length_value,
                                    length_shift,
                                    length_complete,
                                    payload_remaining,
                                    data_offset,
                                });
                                return Err(error.into());
                            }
                        };
                        state.offset = reader.position();
                        length_value |= u64::from(byte & 0x7f) << length_shift;
                        if byte & 0x80 != 0 {
                            length_shift += 7;
                            if length_shift >= 64 {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "varint overflow",
                                )
                                .into());
                            }
                            continue;
                        }
                        length_complete = true;
                    }
                    payload_remaining = match varint_usize(length_value, "String value length") {
                        Ok(length) => length,
                        Err(error) => {
                            state.tasks.push(BodyScanTask::Strings {
                                rows_remaining,
                                length_value,
                                length_shift,
                                length_complete,
                                payload_remaining,
                                data_offset,
                            });
                            return Err(error.into());
                        }
                    };
                    // Fatal `InvalidData`, so nothing is parked.
                    data_offset = next_string_offset(data_offset, payload_remaining)?;
                    length_value = 0;
                    length_shift = 0;
                    length_complete = false;
                    if payload_remaining == 0 {
                        rows_remaining -= 1;
                    }
                }
            }
            BodyScanTask::Offsets {
                mut rows_remaining,
                mut previous,
                children,
            } => {
                while rows_remaining != 0 {
                    let current = match reader.read_u64_le() {
                        Ok(current) => current,
                        Err(error) => {
                            state.tasks.push(BodyScanTask::Offsets {
                                rows_remaining,
                                previous,
                                children,
                            });
                            return Err(error.into());
                        }
                    };
                    state.offset = reader.position();
                    if current < previous {
                        return Err(DecodeError::InvalidArray {
                            column: state.column.clone(),
                            reason: "offsets are not monotonically non-decreasing",
                        });
                    }
                    if i64::try_from(current).is_err() {
                        return Err(DecodeError::InvalidArray {
                            column: state.column.clone(),
                            reason: "offset exceeds i64::MAX",
                        });
                    }
                    previous = current;
                    rows_remaining -= 1;
                }
                state.tasks.push(BodyScanTask::ValidateOffsetCount {
                    count: previous,
                    children,
                });
            }
            BodyScanTask::ValidateOffsetCount { count, children } => {
                let wire_count = count;
                let count = match usize::try_from(wire_count) {
                    Ok(count) => count,
                    Err(_) => {
                        state
                            .tasks
                            .push(BodyScanTask::ValidateOffsetCount { count, children });
                        return Err(DecodeError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "Array element count overflows usize",
                        )));
                    }
                };
                if let Err(error) = check_header_count(count, "Array element count", reader) {
                    state.tasks.push(BodyScanTask::ValidateOffsetCount {
                        count: wire_count,
                        children,
                    });
                    return Err(error);
                }
                for child in children.into_iter().rev() {
                    state.tasks.push(BodyScanTask::Values {
                        ch_type: child,
                        num_rows: count,
                    });
                }
            }
            // The four LowCardinality tasks mirror `decode_low_cardinality`
            // step for step, with the same rejections in the same order.
            BodyScanTask::LowCardinality {
                dict_type,
                num_rows,
            } => {
                let index_word = reader.read_u64_le()?;
                let index_width = low_cardinality_index_width(index_word, &state.column)?;
                state.offset = reader.position();
                state.tasks.push(BodyScanTask::LowCardinalityDictionary {
                    dict_type,
                    num_rows,
                    index_width,
                });
            }
            BodyScanTask::LowCardinalityDictionary {
                dict_type,
                num_rows,
                index_width,
            } => {
                // Validate before committing the word: the size and capacity
                // checks report `UnexpectedEof`, so on those the task is parked
                // unchanged and the word is re-read once more bytes arrive.
                let checked = usize::try_from(reader.read_u64_le()?)
                    .map_err(|_| {
                        DecodeError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "LowCardinality dictionary size overflows usize",
                        ))
                    })
                    .and_then(|num_keys| {
                        check_header_count(num_keys, "LowCardinality dictionary size", reader)
                            .map(|()| num_keys)
                    });
                let num_keys = match checked {
                    Ok(num_keys) => num_keys,
                    Err(error) => {
                        if is_unexpected_eof(&error) {
                            state.tasks.push(BodyScanTask::LowCardinalityDictionary {
                                dict_type,
                                num_rows,
                                index_width,
                            });
                        }
                        return Err(error);
                    }
                };
                if !is_low_cardinality_inner(&dict_type) {
                    return Err(DecodeError::UnsupportedType {
                        column: state.column.clone(),
                        type_name: format!("LowCardinality({dict_type})"),
                    });
                }
                state.offset = reader.position();
                state.tasks.push(BodyScanTask::LowCardinalityRows {
                    num_rows,
                    index_width,
                    num_keys,
                });
                state.tasks.push(BodyScanTask::Values {
                    ch_type: dict_type,
                    num_rows: num_keys,
                });
            }
            BodyScanTask::LowCardinalityRows {
                num_rows,
                index_width,
                num_keys,
            } => {
                let wire_rows = match usize::try_from(reader.read_u64_le()?) {
                    Ok(wire_rows) => wire_rows,
                    Err(_) => {
                        state.tasks.push(BodyScanTask::LowCardinalityRows {
                            num_rows,
                            index_width,
                            num_keys,
                        });
                        return Err(DecodeError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "LowCardinality row count overflows usize",
                        )));
                    }
                };
                if wire_rows != num_rows {
                    return Err(DecodeError::InvalidLowCardinality {
                        column: state.column.clone(),
                        reason: "indexes row count disagrees with the block row count",
                    });
                }
                if num_rows.checked_mul(index_width).is_none() {
                    // Same overflow report `decode_low_cardinality_indices`
                    // gives; the bytes for it can never arrive.
                    state.tasks.push(BodyScanTask::LowCardinalityRows {
                        num_rows,
                        index_width,
                        num_keys,
                    });
                    return Err(DecodeError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "LowCardinality index byte length overflows usize",
                    )));
                }
                state.offset = reader.position();
                state.tasks.push(BodyScanTask::LowCardinalityIndexes {
                    rows_remaining: num_rows,
                    index_width,
                    num_keys,
                });
            }
            BodyScanTask::LowCardinalityIndexes {
                mut rows_remaining,
                index_width,
                num_keys,
            } => {
                // Whole indexes only, so a partial index is re-read next time.
                let take = rows_remaining.min(reader.remaining() / index_width);
                // `take * index_width <= reader.remaining()`, so no overflow.
                let raw = reader.read_slice(take * index_width)?;
                check_low_cardinality_indexes(raw, index_width, num_keys, &state.column)?;
                rows_remaining -= take;
                state.offset = reader.position();
                if rows_remaining != 0 {
                    state.tasks.push(BodyScanTask::LowCardinalityIndexes {
                        rows_remaining,
                        index_width,
                        num_keys,
                    });
                    return Err(need_more_bytes());
                }
            }
            BodyScanTask::VariantDiscriminators {
                alternatives,
                mut counts,
                mut rows_done,
                mut rows_remaining,
            } => {
                let take = rows_remaining.min(reader.remaining());
                let raw = reader.read_slice(take)?;
                for (row, &discriminator) in raw.iter().enumerate() {
                    if discriminator == u8::MAX {
                        continue;
                    }
                    let Some(count) = counts.get_mut(discriminator as usize) else {
                        return Err(DecodeError::InvalidVariant {
                            column: state.column.clone(),
                            reason: VariantColumnError::InvalidDiscriminator {
                                row: rows_done + row,
                                discriminator,
                                alternatives: alternatives.len(),
                            }
                            .to_string(),
                        });
                    };
                    *count += 1;
                }
                rows_done += take;
                rows_remaining -= take;
                state.offset = reader.position();
                if rows_remaining != 0 {
                    state.tasks.push(BodyScanTask::VariantDiscriminators {
                        alternatives,
                        counts,
                        rows_done,
                        rows_remaining,
                    });
                    return Err(need_more_bytes());
                }
                for (alternative, count) in alternatives.into_iter().zip(counts).rev() {
                    state.tasks.push(BodyScanTask::Values {
                        ch_type: alternative,
                        num_rows: count,
                    });
                }
            }
            BodyScanTask::DynamicIndexes {
                children,
                mut counts,
                mut rows_remaining,
                width,
                kind,
            } => {
                let take = rows_remaining.min(reader.remaining() / width);
                let raw = reader.read_slice(take * width)?;
                let null_index = if kind == DynamicWireKind::Variant {
                    u64::from(u8::MAX)
                } else {
                    children.len() as u64
                };
                for bytes in raw.chunks_exact(width) {
                    let index = match width {
                        1 => u64::from(bytes[0]),
                        2 => u64::from(u16::from_le_bytes([bytes[0], bytes[1]])),
                        4 => {
                            u64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                        }
                        8 => u64::from_le_bytes([
                            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                            bytes[7],
                        ]),
                        _ => unreachable!("Dynamic index width is selected from 1/2/4/8"),
                    };
                    if index == null_index {
                        continue;
                    }
                    let Some(count) = usize::try_from(index)
                        .ok()
                        .and_then(|index| counts.get_mut(index))
                    else {
                        let reason = if kind == DynamicWireKind::Variant {
                            format!(
                                "discriminator {index} does not name one of {} children",
                                children.len()
                            )
                        } else {
                            format!(
                                "flattened index {index} is greater than NULL index {}",
                                children.len()
                            )
                        };
                        return Err(invalid_dynamic(&state.column, reason));
                    };
                    *count += 1;
                }
                rows_remaining -= take;
                state.offset = reader.position();
                if rows_remaining != 0 {
                    state.tasks.push(BodyScanTask::DynamicIndexes {
                        children,
                        counts,
                        rows_remaining,
                        width,
                        kind,
                    });
                    return Err(need_more_bytes());
                }
                for (child, count) in children.into_iter().zip(counts).rev() {
                    state.tasks.push(BodyScanTask::Values {
                        ch_type: match child {
                            DynamicStateChild::Typed(ch_type) => ch_type,
                            DynamicStateChild::Shared => ChType::String,
                        },
                        num_rows: count,
                    });
                }
            }
            BodyScanTask::AggregateCount {
                mut rows_remaining,
                mut shift,
            } => {
                let bytes = reader.remaining_slice();
                let mut consumed = 0;
                while rows_remaining != 0 && consumed < bytes.len() {
                    let byte = bytes[consumed];
                    consumed += 1;
                    if byte & 0x80 != 0 {
                        shift += 7;
                        if shift >= 64 {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "varint overflow",
                            )
                            .into());
                        }
                    } else {
                        rows_remaining -= 1;
                        shift = 0;
                    }
                }
                reader.skip(consumed)?;
                state.offset = reader.position();
                if rows_remaining != 0 {
                    state.tasks.push(BodyScanTask::AggregateCount {
                        rows_remaining,
                        shift,
                    });
                    return Err(need_more_bytes());
                }
            }
            BodyScanTask::AggregateZeroBytes { mut rows_remaining } => {
                let take = rows_remaining.min(reader.remaining());
                if reader.read_slice(take)?.iter().any(|&byte| byte != 0) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "registered AggregateFunction(nothing*) state byte must be zero",
                    )
                    .into());
                }
                rows_remaining -= take;
                state.offset = reader.position();
                if rows_remaining != 0 {
                    state
                        .tasks
                        .push(BodyScanTask::AggregateZeroBytes { rows_remaining });
                    return Err(need_more_bytes());
                }
            }
            BodyScanTask::AggregateNullableSum {
                mut rows_remaining,
                state_width,
            } => {
                let bytes = reader.remaining_slice();
                let mut consumed = 0;
                while rows_remaining != 0 {
                    let Some(&flag) = bytes.get(consumed) else {
                        break;
                    };
                    let after_flag = consumed + 1;
                    if flag == 0 {
                        consumed = after_flag;
                        rows_remaining -= 1;
                        continue;
                    }
                    let end = after_flag.checked_add(state_width).ok_or_else(|| {
                        DecodeError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "aggregate nullable sum state byte length overflow",
                        ))
                    })?;
                    if bytes.get(after_flag..end).is_none() {
                        break;
                    }
                    consumed = end;
                    rows_remaining -= 1;
                }
                reader.skip(consumed)?;
                state.offset = reader.position();
                if rows_remaining != 0 {
                    state.tasks.push(BodyScanTask::AggregateNullableSum {
                        rows_remaining,
                        state_width,
                    });
                    return Err(need_more_bytes());
                }
            }
            BodyScanTask::ChargeSynthetic { bytes, what } => {
                state.allocation_budget.charge(bytes, what)?;
            }
        }
    }
    if state.state_cursor != state.states.len() {
        let reason = "body traversal did not consume every prefix state";
        return Err(match state.states.get(state.state_cursor) {
            Some(StatePrefix::Json(_)) => invalid_json(&state.column, reason),
            _ => invalid_dynamic(&state.column, reason),
        });
    }
    Ok(())
}

/// Decode and validate a `LowCardinality` index-serialization word, returning
/// the index width. The same rejections, in the same order, as
/// [`decode_low_cardinality`].
fn low_cardinality_index_width(index_word: u64, column: &str) -> Result<usize, DecodeError> {
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
    match index_word & 0xFF {
        0 => Ok(1),
        1 => Ok(2),
        2 => Ok(4),
        3 => Ok(8),
        _ => Err(DecodeError::InvalidLowCardinality {
            column: column.to_string(),
            reason: "index width tag is outside 0..=3",
        }),
    }
}

/// Read one raw little-endian `LowCardinality` index of `index_width` bytes.
/// `chunk` comes from `chunks_exact(index_width)` with a validated width.
#[inline]
fn low_cardinality_raw_index(chunk: &[u8]) -> u64 {
    match *chunk {
        [a] => u64::from(a),
        [a, b] => u64::from(u16::from_le_bytes([a, b])),
        [a, b, c, d] => u64::from(u32::from_le_bytes([a, b, c, d])),
        [a, b, c, d, e, f, g, h] => u64::from_le_bytes([a, b, c, d, e, f, g, h]),
        // The width tag admits only 1/2/4/8; anything else cannot index a
        // dictionary, and reporting it as out of range keeps this panic-free.
        _ => u64::MAX,
    }
}

/// Largest raw index in a run of whole `LowCardinality` indexes, or
/// `u64::MAX` when the run cannot be valid (an 8-byte index with a nonzero
/// high half, or an unsupported width). An empty run returns 0.
#[inline]
fn max_raw_low_cardinality_index(raw: &[u8], index_width: usize) -> u64 {
    // One arm per width so each reduction is inlined with a constant width and
    // vectorizes. Mapping `low_cardinality_raw_index` here would reuse the
    // shared runtime-width fold, which stays scalar.
    match index_width {
        1 => raw.iter().copied().max().map_or(0, u64::from),
        // Measured on aarch64 at width 2: `try_into` lowers to plain u16
        // loads, while indexing the two bytes vectorized as a deinterleave
        // plus reassembly at about half the speed. The `u16::MAX` fallback is
        // unreachable because `chunks_exact(2)` yields only 2-byte chunks.
        2 => raw
            .chunks_exact(2)
            .map(|chunk| chunk.try_into().map_or(u16::MAX, u16::from_le_bytes))
            .fold(0, u16::max)
            .into(),
        4 => raw
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .max()
            .map_or(0, u64::from),
        // NEON has no 64-bit unsigned max. Any nonzero high half exceeds
        // i32::MAX and must fail, so take the max of the low halves and OR the
        // high halves; the caller's re-walk reports the exact error.
        8 => {
            let (mut low, mut high) = (0u32, 0u32);
            for chunk in raw.chunks_exact(8) {
                low = low.max(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                high |= u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            }
            if high == 0 {
                u64::from(low)
            } else {
                u64::MAX
            }
        }
        // `low_cardinality_index_width` admits only 1/2/4/8. This arm only
        // routes an impossible width to the caller's re-walk.
        _ => u64::MAX,
    }
}

/// Validate a run of whole `LowCardinality` indexes the way
/// [`decode_low_cardinality_indices`] does: every index must point inside the
/// block dictionary and a non-sentinel index must fit i32. The common all-valid
/// case is one branch-free max reduction; only a run that fails it is re-walked
/// row by row to report the decode's first error exactly.
fn check_low_cardinality_indexes(
    raw: &[u8],
    index_width: usize,
    num_keys: usize,
    column: &str,
) -> Result<(), DecodeError> {
    let max = max_raw_low_cardinality_index(raw, index_width);
    let num_keys = num_keys as u64;
    if raw.is_empty() || (max < num_keys && max <= i32::MAX as u64) {
        return Ok(());
    }
    for raw_index in raw.chunks_exact(index_width).map(low_cardinality_raw_index) {
        if raw_index >= num_keys {
            return Err(DecodeError::InvalidLowCardinality {
                column: column.to_string(),
                reason: "index value points outside the block dictionary",
            });
        }
        // A NULL sentinel is index 0, always in range, so no exemption is
        // needed here.
        if raw_index > i32::MAX as u64 {
            return Err(DecodeError::InvalidLowCardinality {
                column: column.to_string(),
                reason: "index value exceeds i32::MAX",
            });
        }
    }
    Ok(())
}

/// Expand one logical value run into bounded scan tasks in wire order. The
/// dispatch order mirrors [`skip_values`]: alias, LowCardinality, Array, Map,
/// Variant, then the Nullable unwrap, then Tuple or a leaf body.
fn push_value_tasks(
    tasks: &mut Vec<BodyScanTask>,
    ch_type: &ChType,
    num_rows: usize,
    column: &str,
    states: &[StatePrefix],
    state_cursor: &mut usize,
) -> Result<(), DecodeError> {
    if let Some(delegate) = ch_type.physical_delegate() {
        tasks.push(BodyScanTask::Values {
            ch_type: delegate,
            num_rows,
        });
        return Ok(());
    }
    match ch_type {
        // A zero-length LowCardinality run carries no body bytes at all (the
        // matching gate in `decode_values`).
        ChType::LowCardinality(_) if num_rows == 0 => {}
        ChType::LowCardinality(inner) => {
            // Nullability only changes how decode reads index 0, not the bytes.
            let (_, dict_type) = low_cardinality_dict_value_type(inner);
            tasks.push(BodyScanTask::LowCardinality {
                dict_type: dict_type.clone(),
                num_rows,
            });
        }
        ChType::Variant(alternatives) => {
            // `variant_child_counts` with no rows performs exactly the
            // alternative-count check `decode_variant` applies and returns the
            // zeroed per-alternative counters, then the same row guard.
            // The row guard fires before the discriminators arrive: no
            // completion of these bytes can decode, and waiting would stall.
            let (counts, _) = variant_child_counts(&[], alternatives.len())
                .and_then(|counts| check_variant_row_count(num_rows).map(|()| counts))
                .map_err(|error| DecodeError::InvalidVariant {
                    column: column.to_string(),
                    reason: error.to_string(),
                })?;
            tasks.push(BodyScanTask::VariantDiscriminators {
                alternatives: alternatives.clone(),
                counts,
                rows_done: 0,
                rows_remaining: num_rows,
            });
        }
        ChType::Dynamic { .. } => {
            let state = next_dynamic_state(states, state_cursor, column)?.clone();
            if num_rows > i32::MAX as usize {
                return Err(invalid_dynamic(
                    column,
                    "row count exceeds Arrow Dense Union's i32 child-offset range",
                ));
            }
            let num_children = state.children.len();
            let width = match state.kind {
                DynamicWireKind::Variant => 1,
                DynamicWireKind::Flattened => {
                    let values = num_children.checked_add(1).ok_or_else(|| {
                        invalid_dynamic(column, "flattened child count overflows usize")
                    })?;
                    if values <= u8::MAX as usize + 1 {
                        1
                    } else if values <= u16::MAX as usize + 1 {
                        2
                    } else if values <= u32::MAX as usize {
                        4
                    } else {
                        8
                    }
                }
            };
            if num_rows.checked_mul(width).is_none() {
                return Err(DecodeError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Dynamic flattened index byte length overflows usize",
                )));
            }
            tasks.push(BodyScanTask::DynamicIndexes {
                counts: vec![0; num_children],
                children: state.children,
                rows_remaining: num_rows,
                width,
                kind: state.kind,
            });
        }
        ChType::Json { typed_paths, .. } => {
            let json = next_json_state(states, state_cursor, column)?;
            let kind = json.kind;
            let num_dynamic = json.dynamic_path_count();
            if num_rows > i32::MAX as usize {
                return Err(invalid_json(
                    column,
                    "row count exceeds Arrow's i32 offset range",
                ));
            }
            if kind == JsonWireKind::Text {
                tasks.push(BodyScanTask::Strings {
                    rows_remaining: num_rows,
                    length_value: 0,
                    length_shift: 0,
                    length_complete: false,
                    payload_remaining: 0,
                    data_offset: 0,
                });
            } else {
                if kind == JsonWireKind::Flattened
                    && typed_paths.is_empty()
                    && num_dynamic == 0
                    && num_rows > MAX_PATHLESS_JSON_ROWS
                {
                    return Err(invalid_json(column, format!("row count {num_rows} exceeds the pathless FLATTENED block limit {MAX_PATHLESS_JSON_ROWS}")));
                }
                if kind == JsonWireKind::Flattened {
                    let offset_bytes = num_rows
                        .checked_add(1)
                        .and_then(|rows| rows.checked_mul(std::mem::size_of::<i64>()))
                        .unwrap_or(usize::MAX);
                    tasks.push(BodyScanTask::ChargeSynthetic {
                        bytes: offset_bytes,
                        what: "FLATTENED JSON shared offsets",
                    });
                }
                if kind == JsonWireKind::Structured {
                    tasks.push(BodyScanTask::Offsets {
                        rows_remaining: num_rows,
                        previous: 0,
                        children: vec![ChType::String, ChType::String],
                    });
                }
                if num_dynamic != 0 {
                    tasks.push(BodyScanTask::DynamicValuesRun {
                        remaining: num_dynamic,
                        num_rows,
                    });
                }
                for (_, ch_type) in typed_paths.iter().rev() {
                    tasks.push(BodyScanTask::Values {
                        ch_type: ch_type.clone(),
                        num_rows,
                    });
                }
            }
        }
        ChType::AggregateFunction { .. } => match decode_state_codec(ch_type)? {
            AggregateStateCodec::Count => tasks.push(BodyScanTask::AggregateCount {
                rows_remaining: num_rows,
                shift: 0,
            }),
            AggregateStateCodec::FixedZeroByte => tasks.push(BodyScanTask::AggregateZeroBytes {
                rows_remaining: num_rows,
            }),
            AggregateStateCodec::Sum { state_width } => tasks.push(BodyScanTask::Bytes {
                remaining: num_rows.checked_mul(state_width).ok_or_else(|| {
                    DecodeError::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "aggregate sum state byte length overflow",
                    ))
                })?,
            }),
            AggregateStateCodec::NullableSum { state_width } => {
                tasks.push(BodyScanTask::AggregateNullableSum {
                    rows_remaining: num_rows,
                    state_width,
                })
            }
        },
        ChType::Nullable(inner) => {
            tasks.push(BodyScanTask::Values {
                ch_type: inner.as_ref().clone(),
                num_rows,
            });
            tasks.push(BodyScanTask::Bytes {
                remaining: num_rows,
            });
        }
        ChType::Array(inner) => {
            checked_array_offset_bytes(num_rows)?;
            tasks.push(BodyScanTask::Offsets {
                rows_remaining: num_rows,
                previous: 0,
                children: vec![inner.as_ref().clone()],
            });
        }
        ChType::Map(key, value) => {
            checked_array_offset_bytes(num_rows)?;
            tasks.push(BodyScanTask::Offsets {
                rows_remaining: num_rows,
                previous: 0,
                children: vec![key.as_ref().clone(), value.as_ref().clone()],
            });
        }
        ChType::Tuple(elements) if elements.is_empty() => {
            tasks.push(BodyScanTask::Bytes {
                remaining: num_rows,
            });
        }
        ChType::Tuple(elements) => {
            for (_, element) in elements.iter().rev() {
                tasks.push(BodyScanTask::Values {
                    ch_type: element.clone(),
                    num_rows,
                });
            }
        }
        ChType::String => tasks.push(BodyScanTask::Strings {
            rows_remaining: num_rows,
            length_value: 0,
            length_shift: 0,
            length_complete: false,
            payload_remaining: 0,
            data_offset: 0,
        }),
        fixed => {
            let bytes = fixed_width_body_bytes(fixed, num_rows, column)?;
            tasks.push(BodyScanTask::Bytes { remaining: bytes });
        }
    }
    Ok(())
}

fn checked_array_offset_bytes(num_rows: usize) -> Result<usize, DecodeError> {
    num_rows.checked_mul(8).ok_or_else(|| {
        DecodeError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Array offset byte length overflows usize",
        ))
    })
}

/// Whether `ch_type` is a leaf whose body is a computable run of bytes
/// ([`fixed_width_body_bytes`] accepts it). Exhaustive so a new `ChType`
/// variant must be classified here.
fn is_fixed_width_leaf(ch_type: &ChType) -> bool {
    match ch_type {
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
        | ChType::QBit { .. }
        | ChType::Int128
        | ChType::UInt128
        | ChType::Int256
        | ChType::UInt256
        | ChType::FixedString(_)
        | ChType::Date
        | ChType::Date32
        | ChType::DateTime { .. }
        | ChType::DateTime64 { .. }
        | ChType::Time
        | ChType::Time64 { .. }
        | ChType::Interval(_)
        | ChType::Decimal { .. }
        | ChType::Uuid
        | ChType::Ipv4
        | ChType::Ipv6
        | ChType::Enum8 { .. }
        | ChType::Enum16 { .. } => true,
        ChType::String
        | ChType::Nullable(_)
        | ChType::LowCardinality(_)
        | ChType::Array(_)
        | ChType::Tuple(_)
        | ChType::Map(..)
        | ChType::Variant(_)
        | ChType::Dynamic { .. }
        | ChType::SimpleAggregateFunction { .. }
        | ChType::AggregateFunction { .. }
        | ChType::Geo(_)
        | ChType::Geometry
        | ChType::Nested(_)
        | ChType::Json { .. } => false,
    }
}

/// Byte length of a fixed-width value run, the same length
/// [`skip_column_body`] skips (with the overflow reported as `UnexpectedEof`).
fn fixed_width_body_bytes(
    ch_type: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<usize, DecodeError> {
    let width = match ch_type {
        ChType::Nothing | ChType::Bool | ChType::Int8 | ChType::UInt8 | ChType::Enum8 { .. } => 1,
        ChType::Int16
        | ChType::UInt16
        | ChType::BFloat16
        | ChType::Date
        | ChType::Enum16 { .. } => 2,
        ChType::Int32
        | ChType::UInt32
        | ChType::Float32
        | ChType::Date32
        | ChType::DateTime { .. }
        | ChType::Time
        | ChType::Ipv4 => 4,
        ChType::Int64
        | ChType::UInt64
        | ChType::Float64
        | ChType::DateTime64 { .. }
        | ChType::Time64 { .. }
        | ChType::Interval(_) => 8,
        ChType::FixedString(width) => *width,
        ChType::Uuid | ChType::Ipv6 | ChType::Int128 | ChType::UInt128 => 16,
        ChType::Int256 | ChType::UInt256 => 32,
        ChType::Decimal { bits, .. } => usize::from(*bits / 8),
        ChType::QBit {
            element_type,
            dimension,
        } => {
            let (_, wire_len, _) = qbit_layout(num_rows, *dimension, element_type.bit_width())?;
            return Ok(wire_len);
        }
        // Not fixed-width leaves; `push_value_tasks` dispatches every
        // resumable one before reaching here, so this is defense in depth.
        ChType::String
        | ChType::Nullable(_)
        | ChType::LowCardinality(_)
        | ChType::Array(_)
        | ChType::Tuple(_)
        | ChType::Map(..)
        | ChType::Variant(_)
        | ChType::Dynamic { .. }
        | ChType::SimpleAggregateFunction { .. }
        | ChType::AggregateFunction { .. }
        | ChType::Geo(_)
        | ChType::Geometry
        | ChType::Nested(_)
        | ChType::Json { .. } => {
            return Err(DecodeError::UnsupportedType {
                column: column.to_string(),
                type_name: ch_type.to_string(),
            })
        }
    };
    num_rows.checked_mul(width).ok_or_else(|| {
        DecodeError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "fixed-width byte length overflows usize",
        ))
    })
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
    options: &DecodeSettings,
) -> Result<(), DecodeError> {
    // Per-column bulk-state prefix, the same step `decode_column` runs. Zero
    // bytes for every type except LowCardinality; Array, Tuple, and Nullable
    // recurse into their element/inner prefixes.
    let mut states = Vec::new();
    read_state_prefix(reader, ch_type, column, options, &mut states, 0)?;
    let mut state_cursor = 0usize;
    skip_values(
        reader,
        ch_type,
        num_rows,
        column,
        &states,
        &mut state_cursor,
    )?;
    if state_cursor != states.len() {
        let reason = "body traversal did not consume every prefix state";
        return Err(match states.get(state_cursor) {
            Some(StatePrefix::Json(_)) => invalid_json(column, reason),
            _ => invalid_dynamic(column, reason),
        });
    }
    let mut suffix_index = 0usize;
    read_state_suffix(reader, ch_type, column, &states, &mut suffix_index)
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
    states: &[StatePrefix],
    state_cursor: &mut usize,
) -> Result<(), DecodeError> {
    // Expand a name-decoration alias to its physical delegate, the scan-side
    // mirror of `decode_values`, so a geo/Nested alias reaches the Array
    // fast-path and a SimpleAggregateFunction walks its inner.
    if let Some(under) = ch_type.physical_delegate_ref() {
        return skip_values(
            reader,
            under.as_ref(),
            num_rows,
            column,
            states,
            state_cursor,
        );
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
        return skip_array_data(reader, inner, num_rows, column, states, state_cursor);
    }

    // Map before the Nullable unwrap, mirroring `decode_values`: a map is
    // never nullable at this level.
    if let ChType::Map(key, value) = ch_type {
        return skip_map_data(reader, key, value, num_rows, column, states, state_cursor);
    }

    if let ChType::Variant(alternatives) = ch_type {
        return skip_variant_data(reader, alternatives, num_rows, column, states, state_cursor);
    }

    if matches!(ch_type, ChType::Dynamic { .. }) {
        let state = next_dynamic_state(states, state_cursor, column)?;
        return skip_dynamic_data(reader, state, num_rows, column, states, state_cursor);
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
    let delegate = inner.physical_delegate_ref();
    let inner = delegate.as_deref().unwrap_or(inner);

    // Tuple after the Nullable unwrap, mirroring `decode_values`: a
    // `Nullable(Tuple(...))` walks its per-row null map above, then the tuple
    // body.
    if let ChType::Tuple(elements) = inner {
        return skip_tuple_data(reader, elements, num_rows, column, states, state_cursor);
    }

    // JSON after the Nullable unwrap, mirroring `decode_values`: a
    // `Nullable(JSON)` walks its per-row null map above, then the JSON body.
    if let ChType::Json { .. } = inner {
        return skip_json_data(reader, inner, num_rows, column, states, state_cursor);
    }

    skip_column_body(reader, inner, num_rows)
}

/// The `i32::MAX` row guard [`variant_layout_from_discriminators`] applies to a
/// Variant run (Arrow Dense Union child offsets are i32).
fn check_variant_row_count(num_rows: usize) -> Result<(), VariantColumnError> {
    if num_rows > i32::MAX as usize {
        return Err(VariantColumnError::ChildOffsetOverflow {
            row: i32::MAX as usize,
        });
    }
    Ok(())
}

/// Walk one BASIC Variant body without allocating its Arrow routing buffers.
fn skip_variant_data(
    reader: &mut ByteReader,
    alternatives: &[ChType],
    num_rows: usize,
    column: &str,
    states: &[StatePrefix],
    state_cursor: &mut usize,
) -> Result<(), DecodeError> {
    let discriminators = reader.read_slice(num_rows)?;
    let (counts, _null_count) = variant_child_counts(discriminators, alternatives.len())
        .and_then(|counts| check_variant_row_count(num_rows).map(|()| counts))
        .map_err(|err| DecodeError::InvalidVariant {
            column: column.to_string(),
            reason: err.to_string(),
        })?;
    for (alternative, count) in alternatives.iter().zip(counts) {
        skip_values(reader, alternative, count, column, states, state_cursor)?;
    }
    Ok(())
}

/// Walk one Dynamic body without allocating routing or child buffers. The only
/// allocation is one child-count vector, bounded by the already-read type table.
fn skip_dynamic_data(
    reader: &mut ByteReader,
    state: &DynamicState,
    num_rows: usize,
    column: &str,
    states: &[StatePrefix],
    state_cursor: &mut usize,
) -> Result<(), DecodeError> {
    let num_children = state.children.len();
    // Same guard, at the same point, as `decode_dynamic`.
    if num_rows > i32::MAX as usize {
        return Err(invalid_dynamic(
            column,
            "row count exceeds Arrow Dense Union's i32 child-offset range",
        ));
    }
    let mut counts = vec![0usize; num_children];
    match state.kind {
        DynamicWireKind::Variant => {
            let discriminators = reader.read_slice(num_rows)?;
            for &discriminator in discriminators {
                if discriminator == u8::MAX {
                    continue;
                }
                let Some(count) = counts.get_mut(discriminator as usize) else {
                    return Err(invalid_dynamic(
                        column,
                        format!(
                            "discriminator {discriminator} does not name one of {num_children} children"
                        ),
                    ));
                };
                *count += 1;
            }
        }
        DynamicWireKind::Flattened => {
            let values = num_children
                .checked_add(1)
                .ok_or_else(|| invalid_dynamic(column, "flattened child count overflows usize"))?;
            // Same width selection as `decode_dynamic`: `<= u32::MAX` diverges
            // by one from the server's `<= u32::MAX + 1`, unreachably, because
            // `read_dynamic_state` rejects a type count >= u32::MAX first.
            let width = if values <= u8::MAX as usize + 1 {
                1
            } else if values <= u16::MAX as usize + 1 {
                2
            } else if values <= u32::MAX as usize {
                4
            } else {
                8
            };
            let byte_len = num_rows.checked_mul(width).ok_or_else(|| {
                DecodeError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Dynamic flattened index byte length overflows usize",
                ))
            })?;
            for bytes in reader.read_slice(byte_len)?.chunks_exact(width) {
                let index = match width {
                    1 => bytes[0] as u64,
                    2 => u16::from_le_bytes([bytes[0], bytes[1]]) as u64,
                    4 => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64,
                    8 => u64::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ]),
                    _ => unreachable!("flattened width selected from 1/2/4/8"),
                };
                if index == num_children as u64 {
                    continue;
                }
                let Some(count) = usize::try_from(index)
                    .ok()
                    .and_then(|index| counts.get_mut(index))
                else {
                    return Err(invalid_dynamic(
                        column,
                        format!(
                            "flattened index {index} is greater than NULL index {num_children}"
                        ),
                    ));
                };
                *count += 1;
            }
        }
    }

    for (child, count) in state.children.iter().zip(counts) {
        match child {
            DynamicStateChild::Typed(ch_type) => {
                skip_values(reader, ch_type, count, column, states, state_cursor)?
            }
            // Decoded by `decode_string_data`, so walked as one String run.
            DynamicStateChild::Shared => skip_column_body(reader, &ChType::String, count)?,
        }
    }
    Ok(())
}

/// Walk one `JSON` column body without materializing it, the scan-side mirror of
/// [`decode_json`]. Consumes exactly what the decode reads: the STRING form's
/// per-row varint strings, or the structured/flattened form's typed-path bodies,
/// per-dynamic-path Dynamic bodies, and (V1/V2 only) the shared-data offsets plus
/// its two flattened String columns.
fn skip_json_data(
    reader: &mut ByteReader,
    ch_type: &ChType,
    num_rows: usize,
    column: &str,
    states: &[StatePrefix],
    state_cursor: &mut usize,
) -> Result<(), DecodeError> {
    let ChType::Json { typed_paths, .. } = ch_type else {
        return Err(invalid_json(column, "not a JSON type"));
    };
    let state = next_json_state(states, state_cursor, column)?;
    let (kind, num_dynamic) = (state.kind, state.dynamic_path_count());

    if kind == JsonWireKind::Text {
        return skip_column_body(reader, &ChType::String, num_rows);
    }

    if kind == JsonWireKind::Flattened
        && typed_paths.is_empty()
        && num_dynamic == 0
        && num_rows > MAX_PATHLESS_JSON_ROWS
    {
        return Err(invalid_json(
            column,
            format!(
                "row count {num_rows} exceeds the pathless FLATTENED block limit {MAX_PATHLESS_JSON_ROWS}"
            ),
        ));
    }

    for (_, element_type) in typed_paths {
        skip_values(reader, element_type, num_rows, column, states, state_cursor)?;
    }
    for _ in 0..num_dynamic {
        let state = match next_state(states, state_cursor)
            .ok_or_else(|| invalid_json(column, MISSING_BODY_STATE))?
        {
            StatePrefix::Dynamic(state) => state,
            StatePrefix::Json(_) => {
                return Err(invalid_json(
                    column,
                    "expected a Dynamic state for a JSON dynamic path",
                ))
            }
        };
        skip_dynamic_data(reader, state, num_rows, column, states, state_cursor)?;
    }
    if kind == JsonWireKind::Structured {
        // Shared data: the Array(Tuple(String, String)) offsets, then the two
        // flattened String columns, walked exactly as `decode_json` reads them.
        let total_pairs = read_array_offsets(reader, num_rows, column, None)?;
        skip_column_body(reader, &ChType::String, total_pairs)?;
        skip_column_body(reader, &ChType::String, total_pairs)?;
    }
    Ok(())
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
    states: &[StatePrefix],
    state_cursor: &mut usize,
) -> Result<(), DecodeError> {
    if elements.is_empty() {
        reader.skip(num_rows)?;
        return Ok(());
    }
    for (_, element_type) in elements {
        skip_values(reader, element_type, num_rows, column, states, state_cursor)?;
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
    states: &[StatePrefix],
    state_cursor: &mut usize,
) -> Result<(), DecodeError> {
    let total_entries = read_array_offsets(reader, num_rows, column, None)?;
    skip_values(reader, key, total_entries, column, states, state_cursor)?;
    skip_values(reader, value, total_entries, column, states, state_cursor)
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
    states: &[StatePrefix],
    state_cursor: &mut usize,
) -> Result<(), DecodeError> {
    let total_elements = read_array_offsets(reader, num_rows, column, None)?;
    skip_values(reader, inner, total_elements, column, states, state_cursor)
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
        // `SerializationNothing::deserializeBinaryBulk` consumes one ignored
        // byte per row, matching the allocating decoder above.
        ChType::Nothing => reader.skip(num_rows)?,
        // Enum8 is 1 byte/row (like Int8); Enum16 is 2 bytes/row (like Int16).
        ChType::Bool | ChType::Int8 | ChType::UInt8 | ChType::Enum8 { .. } => {
            reader.skip(num_rows)?
        }
        ChType::Int16
        | ChType::UInt16
        | ChType::BFloat16
        | ChType::Date
        | ChType::Enum16 { .. } => reader.skip(num_rows.saturating_mul(2))?,
        ChType::QBit {
            element_type,
            dimension,
        } => {
            let (_, wire_len, _) = qbit_layout(num_rows, *dimension, element_type.bit_width())?;
            reader.skip(wire_len)?;
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
            let mut offset = 0i32;
            for _ in 0..num_rows {
                let len = varint_usize(reader.read_varint()?, "String value length")?;
                offset = next_string_offset(offset, len)?;
                reader.skip(len)?;
            }
        }
        // Aggregate states have no outer length. Walk the run through the same
        // boundary-only codec materializing decode uses (via
        // `scan_aggregate_states` with no offset collection), so `block_end`
        // stops at exactly the same next-column boundary decode would.
        ChType::AggregateFunction { .. } => {
            let codec = decode_state_codec(inner_type)?;
            scan_aggregate_states(reader, codec, num_rows, None)?;
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
        | ChType::Variant(_)
        | ChType::Dynamic { .. }
        | ChType::Json { .. }
        | ChType::SimpleAggregateFunction { .. }
        | ChType::Geo(_)
        | ChType::Geometry
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
/// reads and rejecting what it rejects: the index type word, the per-block
/// dictionary, the row count, and the raw index array (range-checked against the
/// dictionary). Shares its validation helpers with the resumable
/// `BodyScanTask::LowCardinality*` tasks so the walkers cannot drift. Reached
/// only from the non-resumable walkers (a LowCardinality inside Dynamic or JSON).
fn skip_low_cardinality_data(
    reader: &mut ByteReader,
    inner: &ChType,
    num_rows: usize,
    column: &str,
) -> Result<(), DecodeError> {
    // Resolve the inner through the shared helper, the scan-side mirror of
    // [`decode_low_cardinality`], so the dictionary body walk matches the decode
    // for `LowCardinality(SAF(anyLast, Nullable(String)))` and chained SAF alike.
    let (_, dict_value_type) = low_cardinality_dict_value_type(inner);

    // Index type word, with the decode-side flag and width rejections: a hostile
    // flags word would otherwise make the scan walk framing the decode refuses.
    let index_width = low_cardinality_index_width(reader.read_u64_le()?, column)?;

    // Per-block dictionary: a count then that many inner values. Reject an inner
    // type outside the LowCardinality allowlist exactly as the decode path does,
    // so the scan and the decode agree on which columns are accepted.
    let num_keys = usize::try_from(reader.read_u64_le()?).map_err(|_| {
        DecodeError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "LowCardinality dictionary size overflows usize",
        ))
    })?;
    check_header_count(num_keys, "LowCardinality dictionary size", reader)?;
    if !is_low_cardinality_inner(dict_value_type) {
        return Err(DecodeError::UnsupportedType {
            column: column.to_string(),
            type_name: format!("LowCardinality({dict_value_type})"),
        });
    }
    skip_column_body(reader, dict_value_type, num_keys)?;

    // Row count word (must restate the run length), then the raw index array.
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
    let total = num_rows.checked_mul(index_width).ok_or_else(|| {
        DecodeError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "LowCardinality index byte length overflows usize",
        ))
    })?;
    let raw = reader.read_slice(total)?;
    check_low_cardinality_indexes(raw, index_width, num_keys, column)
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
    decode_all_bytes_with_settings(data, &DecodeSettings::text(options))
}

/// Decode a complete Native stream whose type headers and Dynamic runtime type
/// tables use binary data-type descriptors. This keeps [`DecodeOptions`]
/// backward compatible while exposing the server's out-of-band
/// `output_format_native_encode_types_in_binary_format` setting explicitly.
pub fn decode_all_bytes_binary_types(
    data: &[u8],
    options: &DecodeOptions,
) -> Result<ChunkedBatch, DecodeError> {
    decode_all_bytes_with_settings(data, &DecodeSettings::binary(options))
}

fn decode_all_bytes_with_settings(
    data: &[u8],
    settings: &DecodeSettings,
) -> Result<ChunkedBatch, DecodeError> {
    let mut reader = ByteReader::new(data);
    let mut schema: Option<Schema> = None;
    let mut chunks: Vec<Arc<ColBatch>> = Vec::new();
    let mut block_index: usize = 0;

    while let Some(batch) = decode_next_block_with_settings(&mut reader, settings)? {
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
