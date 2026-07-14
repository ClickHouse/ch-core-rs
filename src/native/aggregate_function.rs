//! Function-specific codecs for ClickHouse `AggregateFunction(...)` states.
//!
//! Native does not length-prefix an aggregate state or its containing column.
//! Each aggregate function serializes its row states back-to-back, so decode
//! and the streaming completeness scan must agree on the exact state boundary.
//! Keep that knowledge in this module and add signatures only after confirming
//! their concrete server serializer.

use std::io;

use crate::column::AggregateStateColumn;
use crate::native::decode::DecodeError;
use crate::native::varint::{skip_varint, ByteReader};
use crate::schema::ChType;

/// A state layout whose row boundary this crate can find safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggregateStateCodec {
    /// `AggregateFunction(count)` and `AggregateFunction(count, T)` (with
    /// `T != Nullable(Nothing)`) serialize one unsigned VarUInt64 count per row.
    Count,
    /// `AggregateFunction(nothingUInt64, Nullable(Nothing))` serializes exactly
    /// one `0x00` byte per row. This is the canonical name the server assigns
    /// when `count` collapses over an only-null argument (see
    /// [`aggregate_state_codec`]); the state is a fixed-width 1-byte placeholder,
    /// and any nonzero byte is `INCORRECT_DATA` on the server.
    NothingUInt64,
    /// `AggregateFunction(sum, T)` for one supported non-nullable numeric or
    /// Enum argument serializes one fixed-width accumulator per row. The width
    /// is selected from `T` by [`sum_state_width`].
    Sum { state_width: usize },
}

impl AggregateStateCodec {
    /// Fewest wire bytes one serialized state can occupy.
    ///
    /// A `Count` VarUInt64 and a `nothingUInt64` placeholder are each at least
    /// one byte; a `Sum` accumulator is exactly `state_width`. Used to cap the
    /// speculative offsets reservation in [`decode_aggregate_states`] at the
    /// rows the remaining input could actually hold.
    pub(crate) fn min_state_bytes(self) -> usize {
        match self {
            AggregateStateCodec::Count | AggregateStateCodec::NothingUInt64 => 1,
            AggregateStateCodec::Sum { state_width } => state_width,
        }
    }
}

/// Whether `arg` is exactly `Nullable(Nothing)`, the only-null argument shape
/// that makes `count` collapse to `nothingUInt64` on the wire.
fn is_nullable_nothing(arg: &ChType) -> bool {
    matches!(arg, ChType::Nullable(inner) if matches!(**inner, ChType::Nothing))
}

/// Fixed serialized accumulator width for exact base `sum` over one
/// non-nullable argument.
///
/// At ClickHouse `v26.6.1.1193-stable`, `AggregateFunctionSumData::write` writes
/// its accumulator with `writeBinaryLittleEndian`, and `NearestFieldTypeImpl`
/// selects these accumulator representations:
///
/// - Bool and UInt8..UInt64 -> UInt64 (8 bytes)
/// - Int8..Int64 and Enum8/Enum16 -> Int64 (8 bytes)
/// - BFloat16/Float32/Float64 -> Float64 (8 bytes)
/// - UInt128/Int128 -> the same 128-bit integer type (16 bytes)
/// - UInt256/Int256 -> the same 256-bit integer type (32 bytes)
/// - Decimal32/64/128 -> Decimal128 (16 bytes)
/// - Decimal256 -> Decimal256 (32 bytes)
///
/// Each is a raw little-endian POD value with no in-body tag or length. The
/// factory's numeric, Decimal, and explicit Enum dispatch arms exclude every
/// other type. In particular, Nullable arguments are deliberately absent here:
/// the aggregate Null adapter adds a flag and a conditional nested state, so it
/// needs a separate codec before it can be framed safely.
///
/// Confirmed in `AggregateFunctionSum.cpp` (`SumSimple`,
/// `createAggregateFunctionSum`), `AggregateFunctions/Helpers.h`
/// (`createWithNumericType`, `createWithDecimalType`), `Core/Field.h`
/// (`NearestFieldTypeImpl`), and `AggregateFunctionSum.h`
/// (`AggregateFunctionSumData::write`/`read`).
fn sum_state_width(arg: &ChType) -> Option<usize> {
    match arg {
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
        | ChType::BFloat16
        | ChType::Enum8 { .. }
        | ChType::Enum16 { .. } => Some(8),
        ChType::Int128 | ChType::UInt128 => Some(16),
        ChType::Int256 | ChType::UInt256 => Some(32),
        ChType::Decimal {
            bits: 32 | 64 | 128,
            ..
        } => Some(16),
        ChType::Decimal { bits: 256, .. } => Some(32),
        _ => None,
    }
}

/// Select the state codec for one declared aggregate type.
///
/// At ClickHouse `v26.6.1.1193-stable`, no aggregate function is treated as
/// opaque bytes: aggregate states have function-specific serialization and no
/// generic length framing, so an unknown function must stay `UnsupportedType`
/// (returning `None` here) or the streaming scan could not locate the next
/// column. Three exact base signatures are registered:
///
/// - `count`, unversioned, with zero or one argument type, writes one VarUInt64
///   per row (`AggregateFunctionCount::serialize`). `count` is strictly unary,
///   so `createAggregateFunctionCount` throws for more than one argument.
/// - `nothingUInt64` with a single `Nullable(Nothing)` argument writes one
///   `0x00` byte per row (`AggregateFunctionNothingImpl::serialize`).
/// - `sum` with exactly one non-nullable numeric or Enum argument writes one
///   fixed-width accumulator per row (`AggregateFunctionSumData::write`); see
///   [`sum_state_width`] for the exact argument-to-width mapping.
///
/// The one exception carved out of `count` is `count, Nullable(Nothing)`.
/// Parsing the type string `AggregateFunction(count, Nullable(Nothing))` resolves
/// through `AggregateFunctionFactory::get` ->
/// `AggregateFunctionCombinatorNull::transformAggregateFunction`: because
/// `AggregateFunctionCount` registers `returns_default_when_only_null = true` and
/// `Nullable(Nothing).onlyNull()` is true, the server substitutes
/// `AggregateFunctionNothingUInt64`, and `DataTypeAggregateFunction::getNameImpl`
/// renders the canonical `AggregateFunction(nothingUInt64, Nullable(Nothing))`.
/// `NativeWriter` writes that canonical name, so the `count, Nullable(Nothing)`
/// spelling never appears on the wire. Admitting the VarUInt `Count` codec under
/// that header would be a wire-format mismatch, since a server that parses the
/// name resolves the one-zero-byte `nothingUInt64` codec, so it is rejected here.
///
/// The `nothingUInt64` gate is deliberately restricted to the confirmed
/// `Nullable(Nothing)` argument shape rather than any argument list. That name is
/// synthesized only by the count collapse at this tag, and that collapse only
/// happens for `Nullable(Nothing)`, so it is the only spelling the server emits.
/// Accepting other argument lists would fabricate headers the server never
/// writes and, on encode, that it could not parse back to `nothingUInt64`. The
/// analogous parameterized `nothing*` forms exist for other functions, but only
/// the confirmed shape is implemented.
///
/// All confirmed against the server source at `v26.6.1.1193-stable`:
/// `AggregateFunctionCount::serialize`,
/// `AggregateFunctionCombinatorNull::transformAggregateFunction` in
/// `AggregateFunctions/Combinators/AggregateFunctionNull.cpp`,
/// `AggregateFunctionNothingImpl::serialize`/`deserialize` in
/// `AggregateFunctions/AggregateFunctionNothing.h`, and
/// `DataTypeAggregateFunction::getNameImpl`, plus the sum references on
/// [`sum_state_width`].
pub(crate) fn aggregate_state_codec(ch_type: &ChType) -> Option<AggregateStateCodec> {
    let ChType::AggregateFunction {
        function,
        arguments,
    } = ch_type
    else {
        return None;
    };
    match function.as_str() {
        "count" if arguments.len() <= 1 && !arguments.iter().any(is_nullable_nothing) => {
            Some(AggregateStateCodec::Count)
        }
        "nothingUInt64" if arguments.len() == 1 && is_nullable_nothing(&arguments[0]) => {
            Some(AggregateStateCodec::NothingUInt64)
        }
        "sum" if arguments.len() == 1 => sum_state_width(&arguments[0])
            .map(|state_width| AggregateStateCodec::Sum { state_width }),
        _ => None,
    }
}

/// Resolve the state codec for a decode-side `AggregateFunction` column, mapping
/// an unregistered signature to the `UnsupportedType` decode error.
///
/// Shared by the materializing decoder (`decode_column_body`) and the streaming
/// scan (`skip_column_body`) so both report the exact same error, with an empty
/// `column` name the caller fills in, for a signature with no registered
/// state-boundary codec.
pub(crate) fn decode_state_codec(ch_type: &ChType) -> Result<AggregateStateCodec, DecodeError> {
    aggregate_state_codec(ch_type).ok_or_else(|| DecodeError::UnsupportedType {
        column: String::new(),
        type_name: ch_type.to_string(),
    })
}

/// Decode `num_rows` serialized states, preserving each row's exact wire bytes.
///
/// `AggregateFunction(count[, T])` uses one unsigned VarUInt64 per state
/// (`AggregateFunctionCount::serialize` / `deserialize` at
/// `v26.6.1.1193-stable`); `AggregateFunction(nothingUInt64, Nullable(Nothing))`
/// uses one `0x00` byte per state; exact base `sum` uses the fixed accumulator
/// width selected by its argument. The walk records row ends while validating
/// each state, then copies the complete contiguous run once. There is one offsets
/// allocation and one data allocation per column, with no per-row allocation.
///
/// The offsets vector holds 8-byte i64 end offsets, so a hostile `num_rows`
/// (which the block header only bounds at one byte per row) could otherwise
/// reserve up to 8x the input before the run is read. It is capped at the rows
/// the remaining input could actually hold, given each state's minimum wire
/// width, mirroring the read-before-allocate cap in `decode_primitive!`.
pub(crate) fn decode_aggregate_states(
    reader: &mut ByteReader<'_>,
    codec: AggregateStateCodec,
    num_rows: usize,
) -> io::Result<AggregateStateColumn> {
    let start = reader.position();
    let capacity = reader.capacity_for(num_rows, codec.min_state_bytes());
    let mut offsets = Vec::with_capacity(capacity.saturating_add(1));
    offsets.push(0);

    // Walk every state boundary over the borrowed run, collecting Arrow offsets,
    // then copy the whole contiguous body once.
    scan_aggregate_states(reader, codec, num_rows, Some(&mut offsets))?;

    let data = reader.consumed_slice(start)?.to_vec();
    Ok(AggregateStateColumn::new(offsets, data))
}

/// Walk `num_rows` serialized state boundaries with the selected codec, advancing
/// the reader once past the whole run. Shared by allocating decode (which passes
/// `Some` and receives Arrow-shaped i64 end offsets relative to the run start)
/// and `block_end` scanning (which passes `None` and only advances), so the
/// streaming decoder can never disagree with materialization on the boundary.
///
/// The run is borrowed once and walked with a LOCAL cursor index, so the per-row
/// varint scan stays in registers: [`skip_varint`] reads the continuation bytes
/// without accumulating the count the boundary walk discards, and there is no
/// per-byte `ByteReader` field access. The reader is advanced a single time at
/// the end. `Some`/`None` is matched once, outside the loop, so the offset
/// collection adds no per-row branch to the scan path.
pub(crate) fn scan_aggregate_states(
    reader: &mut ByteReader<'_>,
    codec: AggregateStateCodec,
    num_rows: usize,
    collect: Option<&mut Vec<i64>>,
) -> io::Result<()> {
    let bytes = reader.remaining_slice();
    let mut pos = 0usize;

    match codec {
        AggregateStateCodec::Count => match collect {
            Some(offsets) => {
                for _ in 0..num_rows {
                    pos = skip_varint(bytes, pos)?;
                    // `pos` is relative to the run start, so it is the row's Arrow
                    // end offset directly. It only ever grows, so it cannot
                    // underflow; the range check catches the (unreachable in
                    // practice) case of a run past the i64 LargeBinary offset
                    // width, and the map_err runs only on that error path.
                    offsets.push(i64::try_from(pos).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "aggregate state data exceeds Arrow LargeBinary offset range",
                        )
                    })?);
                }
            }
            None => {
                for _ in 0..num_rows {
                    pos = skip_varint(bytes, pos)?;
                }
            }
        },
        // `nothingUInt64` is a fixed-width 1-byte state: one `0x00` per row
        // (`AggregateFunctionNothingImpl::serialize` writes one '\0', and
        // `deserialize` throws INCORRECT_DATA if the byte is nonzero, at
        // `v26.6.1.1193-stable`). The boundary walk is trivial: bounds-check the
        // whole `num_rows`-byte run once, reject any nonzero byte as InvalidData
        // (mirroring the server), then record offsets i -> i.
        AggregateStateCodec::NothingUInt64 => {
            let states = bytes.get(..num_rows).ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "failed to fill whole buffer")
            })?;
            if states.iter().any(|&b| b != 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "AggregateFunction(nothingUInt64) state byte must be zero",
                ));
            }
            pos = num_rows;
            if let Some(offsets) = collect {
                // The largest offset pushed is `num_rows`, so one range check
                // covers every pushed offset; each `i <= num_rows` then casts to
                // i64 without a per-row conversion or a lossy narrowing.
                i64::try_from(num_rows).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "aggregate state data exceeds Arrow LargeBinary offset range",
                    )
                })?;
                for i in 1..=num_rows {
                    offsets.push(i as i64);
                }
            }
        }
        // Exact base `sum` is a fixed-width POD accumulator with no semantic
        // deserialize validation. Bounds-check the complete run once, then form
        // Arrow offsets arithmetically. `checked_mul` handles hostile row counts
        // without wrapping; a short available run remains UnexpectedEof so the
        // streaming decoder can request more bytes.
        AggregateStateCodec::Sum { state_width } => {
            let total = num_rows.checked_mul(state_width).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "aggregate sum state byte length overflow",
                )
            })?;
            bytes.get(..total).ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "failed to fill whole buffer")
            })?;
            pos = total;
            if let Some(offsets) = collect {
                i64::try_from(total).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "aggregate state data exceeds Arrow LargeBinary offset range",
                    )
                })?;
                // `total == num_rows * state_width` was checked above, so each
                // intermediate product is in range. The one i64 check on total
                // proves every smaller end offset can be cast without narrowing.
                offsets.extend((1..=num_rows).map(|i| (i * state_width) as i64));
            }
        }
    }

    // Advance the shared cursor past the whole run in one step. Every byte in
    // `0..pos` was already proven present by `skip_varint`, so this cannot fail;
    // it is one bounds check per column, not per row.
    reader.skip(pos)
}

/// Validate that one caller-provided row slice contains exactly one state for
/// `codec`, with no trailing bytes that would misframe the next row or column.
pub(crate) fn is_valid_aggregate_state(bytes: &[u8], codec: AggregateStateCodec) -> bool {
    match codec {
        // Exactly one VarUInt64 that consumes the whole slice: a trailing byte or
        // a malformed varint (truncated or overlong) is invalid.
        AggregateStateCodec::Count => {
            matches!(skip_varint(bytes, 0), Ok(end) if end == bytes.len())
        }
        // Exactly one `0x00` byte: the server's `nothingUInt64` placeholder. A
        // nonzero byte, an empty slice, or trailing bytes are all invalid.
        AggregateStateCodec::NothingUInt64 => bytes == [0x00],
        // The server reads one raw accumulator and performs no value-level
        // validation. Exact width is the entire row-boundary contract.
        AggregateStateCodec::Sum { state_width } => bytes.len() == state_width,
    }
}
