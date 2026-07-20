//! Precondition validation for the Native encoder: checks a `ColBatch`/`Column`
//! against its declared `ChType` before any bytes are written. The byte writers
//! live in encode/mod.rs.

use crate::batch::ColBatch;
use crate::column::{
    variant_child_counts, AggregateStateColumn, ArrayColumn, Column, DecimalColumn,
    DictionaryColumn, DynamicChild, DynamicColumn, FixedBinaryColumn, JsonBody, JsonColumn,
    MapColumn, QBitColumn, TupleColumn, Utf8Column, VariantColumn, VariantLayout,
    ARROW_UNION_MAX_CHILDREN,
};
use crate::native::aggregate_function::{
    aggregate_state_codec, is_valid_aggregate_state, AggregateStateCodec,
};
use crate::native::protocol::MAX_TYPE_DEPTH;
use crate::native::type_parser::{
    decimal_bits_from_precision, is_simple_aggregate_func_spelling, is_valid_variant_alternative,
    low_cardinality_dict_value_type, parse_ch_type, resolves_to_nothing,
    unsupported_header_type_name,
};
use crate::schema::{ChType, Field, QBitElementType, GEOMETRY_EXPANSION_DEPTH};

use super::{column_error, is_encodable, json_uses_flattened, EncodeError, EncodeOptions};

/// Validate that `batch` can be encoded, without writing anything. Every rejection
/// condition lives here, so a caller can validate a whole [`ChunkedBatch`] up front
/// (see [`encode_chunked`]) and then write every block knowing none will fail
/// partway and leave a partial stream.
///
/// `options` is consulted for the one options-dependent legality rule: a
/// shared-less `Dynamic` column encodes as FLATTENED only when
/// [`EncodeOptions::flattened_dynamic`] is set, and the V1/V2 fallback has
/// count limits FLATTENED does not.
pub(super) fn validate_block(batch: &ColBatch, options: &EncodeOptions) -> Result<(), EncodeError> {
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
        validate_column(field, column, batch.num_rows, options, 0)?;
    }
    Ok(())
}

/// Validate one column against its field and the block row count.
///
/// All per-column rejection logic is here so [`write_block_into`] is structurally
/// infallible once validation passes. Runs at every row count, including zero: a
/// not-yet-encodable type is rejected even in a zero-row block (see the note on
/// [`encode_block`]).
///
/// `depth` is the cumulative nesting depth from the batch column root,
/// incremented on every worklist descent. The `type_depth` cap below bounds
/// one declared type, but a `DynamicColumn`'s children carry their own
/// `ChType`s, so each Dynamic level restarts that per-type budget;
/// [`validate_dynamic`] charges every level against this one cumulative budget
/// so a pathologically nested Dynamic column errors without consuming one Rust
/// stack frame per level. The write phase only runs after this validation passes.
fn validate_column<'a>(
    field: &'a Field,
    column: &'a Column,
    num_rows: usize,
    options: &EncodeOptions,
    depth: usize,
) -> Result<(), EncodeError> {
    // A scalar or leaf column validates without descending, so the worklist
    // stays empty and never allocates. The root starts in `next`; only container
    // validators push onto `work`. `next` holds the root once, is drained on the
    // first iteration, and is never refilled, so the LIFO pop order (which the
    // reversed child pushes rely on) is preserved exactly.
    let mut work: Vec<ValidationTask> = Vec::new();
    let mut next = Some(ValidationTask {
        field: ValidationField::Borrowed(field),
        column,
        num_rows,
        depth,
    });
    while let Some(task) = next.take().or_else(|| work.pop()) {
        validate_column_once(
            task.field.as_ref(),
            task.column,
            task.num_rows,
            options,
            task.depth,
            &mut work,
        )?;
    }
    Ok(())
}

enum ValidationField<'a> {
    Borrowed(&'a Field),
    Owned(Field),
}

impl ValidationField<'_> {
    fn as_ref(&self) -> &Field {
        match self {
            Self::Borrowed(field) => field,
            Self::Owned(field) => field,
        }
    }
}

struct ValidationTask<'a> {
    field: ValidationField<'a>,
    column: &'a Column,
    num_rows: usize,
    depth: usize,
}

fn validate_column_once<'a>(
    field: &Field,
    column: &'a Column,
    num_rows: usize,
    options: &EncodeOptions,
    depth: usize,
    work: &mut Vec<ValidationTask<'a>>,
) -> Result<(), EncodeError> {
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

    // AggregateFunction argument types are metadata for the state body, but
    // the server still constructs them while resolving the aggregate
    // signature. Apply the same recursive semantic shape validation as decode
    // before accepting a caller-built header. This also keeps the ordinary
    // nested LowCardinality and Map constraints centralized.
    if unsupported_header_type_name(&field.ch_type).is_some() {
        return Err(EncodeError::UnsupportedType {
            column: field.name.clone(),
            ch_type: field.ch_type.clone(),
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

    // Expand a name-decoration alias (SimpleAggregateFunction, geo, Nested) to
    // the physical type it delegates to, for every STRUCTURAL check below. The
    // header round-trip check further down stays on `field.ch_type` (the alias
    // form), so the emitted header keeps the alias spelling.
    // [`ChType::resolved_physical_delegate_ref`] follows the whole alias chain,
    // not a fixed number of steps, mirroring the "recurse on the delegate"
    // pattern the decoders use, so a nested alias like
    // `Nullable(SimpleAggregateFunction(_, Point))` resolves all the way to the
    // physical `Tuple` rather than stopping one level short.
    let physical = field.ch_type.resolved_physical_delegate_ref();
    let physical_type = physical.as_deref().unwrap_or(&field.ch_type);

    // The concrete value type is the inner of a `Nullable`, else the physical
    // type itself; an alias under `Nullable` (`Nullable(Point)` -> `Tuple`,
    // `Nullable(SAF(_, T))` -> physical `T`) is resolved through the whole chain
    // here too.
    let value_inner = physical_type.inner();
    let value = value_inner.resolved_physical_delegate_ref();
    let value_type = value.as_deref().unwrap_or(value_inner);

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
    let nullable_at_this_level = matches!(
        physical_type,
        ChType::Nullable(_) | ChType::Variant(_) | ChType::Dynamic { .. }
    ) || matches!(physical_type, ChType::LowCardinality(inner) if low_cardinality_dict_value_type(inner).0);
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
        validate_low_cardinality(field, inner, c, num_rows, depth, work)?;
    }

    if let (ChType::Array(inner), Column::Array(c)) = (value_type, column) {
        validate_array(field, inner, c, num_rows, depth, work)?;
    }

    if let (
        ChType::QBit {
            element_type,
            dimension,
        },
        Column::QBit(c),
    ) = (value_type, column)
    {
        validate_qbit(field, *element_type, *dimension, c, num_rows)?;
    }

    if let (ChType::Tuple(elements), Column::Tuple(c)) = (value_type, column) {
        validate_tuple(field, elements, c, num_rows, depth, work)?;
    }

    if let (ChType::Map(key, value), Column::Map(c)) = (value_type, column) {
        validate_map(field, key, value, c, num_rows, depth, work)?;
    }

    if let (ChType::Variant(alternatives), Column::Variant(c)) = (value_type, column) {
        validate_variant(field, alternatives, c, num_rows, depth, work)?;
    }

    if let (ChType::Dynamic { max_types }, Column::Dynamic(c)) = (value_type, column) {
        validate_dynamic(field, *max_types, c, num_rows, options, depth, work)?;
    }

    if let (
        ChType::Json {
            max_dynamic_paths,
            max_dynamic_types,
            typed_paths,
            ..
        },
        Column::Json(c),
    ) = (value_type, column)
    {
        validate_json(
            field,
            *max_dynamic_paths,
            *max_dynamic_types,
            typed_paths,
            c,
            num_rows,
            options,
            depth,
            work,
        )?;
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

    if let (ChType::AggregateFunction { .. }, Column::AggregateState(c)) = (value_type, column) {
        // The blanket `unsupported_header_type_name` check at the top of this
        // function owns codec legality, so a registered codec is guaranteed
        // here. Resolve it once and hand it to the per-row state validation.
        if let Some(codec) = aggregate_state_codec(value_type) {
            validate_aggregate_state_column(field, value_type, codec, c, num_rows)?;
        }
    }

    Ok(())
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
        ChType::Variant(alternatives) => {
            for alternative in alternatives {
                validate_saf_func_spellings(field, alternative)?;
            }
            Ok(())
        }
        ChType::AggregateFunction { arguments, .. } => {
            for argument in arguments {
                validate_saf_func_spellings(field, argument)?;
            }
            Ok(())
        }
        ChType::Json { typed_paths, .. } => {
            for (_, ty) in typed_paths {
                validate_saf_func_spellings(field, ty)?;
            }
            Ok(())
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
pub(super) fn type_depth(ch_type: &ChType) -> usize {
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
            ChType::Variant(alternatives) => {
                for alternative in alternatives {
                    work.push((alternative, depth + 1));
                }
            }
            ChType::AggregateFunction { arguments, .. } => {
                for argument in arguments {
                    work.push((argument, depth + 1));
                }
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
            ChType::Geometry => {
                max_depth = max_depth.max(depth + GEOMETRY_EXPANSION_DEPTH);
            }
            // JSON charges one level for the JSON node itself; each typed-path
            // type is one level deeper, matching the decode parser's `depth + 1`
            // recursion, so a nested-JSON type near the cap is not under-counted.
            ChType::Json { typed_paths, .. } => {
                for (_, ty) in typed_paths {
                    work.push((ty, depth + 1));
                }
            }
            _ => {}
        }
    }
    max_depth
}

/// Validate the row-major Arrow child used to encode one QBit column.
///
/// Native carries no per-row lengths, so the stored dimension must equal the
/// declared dimension and the child must contain exactly `rows * dimension`
/// non-null scalar values of the declared floating-point type. The per-plane
/// FixedString bulk limit is also enforced so bytes produced here are accepted
/// by the pinned server reader.
fn validate_qbit(
    field: &Field,
    element_type: QBitElementType,
    dimension: usize,
    col: &QBitColumn,
    num_rows: usize,
) -> Result<(), EncodeError> {
    if col.dimension != dimension {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is declared QBit({element_type}, {dimension}) but its buffer dimension is {}",
                field.name, col.dimension
            ),
        });
    }
    let expected =
        num_rows
            .checked_mul(dimension)
            .ok_or_else(|| EncodeError::InconsistentBatch {
                detail: format!(
                    "column {:?} QBit flattened value count overflows usize",
                    field.name
                ),
            })?;
    if col.values.len() != expected {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is QBit({element_type}, {dimension}) over {num_rows} rows so its child must contain {expected} values, but it contains {}",
                field.name,
                col.values.len()
            ),
        });
    }
    let expected_child = element_type.ch_type();
    if !column_variant_matches(&expected_child, col.values.as_ref()) {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} is QBit({element_type}, {dimension}) but its child buffer has the wrong scalar variant",
                field.name
            ),
        });
    }
    if col.values.validity().is_some() {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} QBit child values must be non-nullable; nullability applies to whole vectors",
                field.name
            ),
        });
    }

    let bytes_per_plane_row = dimension / 8 + usize::from(dimension % 8 != 0);
    let plane_bytes = num_rows.checked_mul(bytes_per_plane_row).ok_or_else(|| {
        EncodeError::InconsistentBatch {
            detail: format!("column {:?} QBit plane size overflows usize", field.name),
        }
    })?;
    if plane_bytes > 1usize << 30 {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} QBit plane is {plane_bytes} bytes, above ClickHouse's 1 GiB FixedString bulk limit",
                field.name
            ),
        });
    }
    if plane_bytes.checked_mul(element_type.bit_width()).is_none() {
        return Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} QBit total wire size overflows usize",
                field.name
            ),
        });
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

/// Offset element type shared by the Arrow variable-length columns: `i32` for
/// `String`, `i64` for `Array`/`Map`/`AggregateFunction` state. Lets
/// [`validate_offsets`] check both widths with one implementation instead of the
/// four hand-rolled copies these validators used to carry.
trait Offset: Copy + Ord + std::fmt::Display {
    /// The Arrow zero start offset for this width.
    const ZERO: Self;
    /// The offset as a `usize`, or `None` if it is negative or wider than the
    /// host `usize`.
    fn to_usize(self) -> Option<usize>;
}

impl Offset for i32 {
    const ZERO: Self = 0;
    fn to_usize(self) -> Option<usize> {
        usize::try_from(self).ok()
    }
}

impl Offset for i64 {
    const ZERO: Self = 0;
    fn to_usize(self) -> Option<usize> {
        usize::try_from(self).ok()
    }
}

/// Validate the Arrow offset invariants shared by every variable-length column
/// (`String`, `Array`, `Map`, and `AggregateFunction` state).
///
/// A well-formed offset array has `num_rows + 1` entries starting at 0, is
/// monotonically non-decreasing (which, from the zero start, also proves every
/// offset is non-negative, so the `as usize`/`as u64` casts the body writers
/// perform cannot wrap), and ends at `content_len`, the length of the buffer the
/// offsets index into (data bytes for `String`/`AggregateFunction`, flattened
/// element/entry count for `Array`/`Map`). A final offset short of `content_len`
/// would silently drop trailing content from the wire; one past it would slice
/// out of bounds, a stream the server rejects with `INCORRECT_DATA`.
///
/// Zero-row policy is uniform and lenient: a zero-row column may carry either an
/// empty offsets vec or the single sentinel `[0]` (what the decoder emits), with
/// empty content. Each caller layers its own type-specific checks on top (the
/// per-row aggregate state validity, the array/map element recursion). `label`
/// names the offset kind and `content_label` its content unit for error
/// messages.
fn validate_offsets<O: Offset>(
    field: &Field,
    label: &str,
    content_label: &str,
    offsets: &[O],
    content_len: usize,
    num_rows: usize,
) -> Result<(), EncodeError> {
    let reject = |detail: String| Err(EncodeError::InconsistentBatch { detail });

    // A zero-row column carries no content and either the single sentinel `[0]`
    // (what the decoder emits) or no offsets at all. Accept both, reject else.
    if num_rows == 0 {
        let well_formed_empty = content_len == 0
            && (offsets.is_empty() || (offsets.len() == 1 && offsets[0] == O::ZERO));
        if well_formed_empty {
            return Ok(());
        }
        return reject(format!(
            "column {:?} declares 0 rows but carries {} {label} offsets and {content_len} {content_label}",
            field.name,
            offsets.len(),
        ));
    }

    // Arrow layout: one offset per row plus a trailing end offset.
    if offsets.len() != num_rows + 1 {
        return reject(format!(
            "column {:?} declares {num_rows} rows so it needs {} {label} offsets (a leading 0 plus one end-offset per row), but carries {}",
            field.name,
            num_rows + 1,
            offsets.len()
        ));
    }
    // Offsets start at 0 (Arrow convention).
    if offsets[0] != O::ZERO {
        return reject(format!(
            "column {:?} has a nonzero first {label} offset {}; Arrow offsets start at 0",
            field.name, offsets[0]
        ));
    }
    // Monotonic non-decreasing; from the zero start this also proves every
    // offset is non-negative, so the body writers' casts cannot wrap.
    for pair in offsets.windows(2) {
        if pair[1] < pair[0] {
            return reject(format!(
                "column {:?} has non-monotonic {label} offsets ({} then {})",
                field.name, pair[0], pair[1]
            ));
        }
    }
    // The final offset must cover the content buffer exactly.
    let end = offsets[num_rows];
    if end.to_usize() != Some(content_len) {
        return reject(format!(
            "column {:?} {label} offsets end at {end} but the column holds {content_len} {content_label}",
            field.name
        ));
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
fn validate_low_cardinality<'a>(
    field: &Field,
    inner: &ChType,
    col: &'a DictionaryColumn,
    num_rows: usize,
    depth: usize,
    work: &mut Vec<ValidationTask<'a>>,
) -> Result<(), EncodeError> {
    // Resolve the inner through the shared helper (full SAF chain + optional
    // removeNullable Nullable + inner SAF chain), so the dictionary body and index
    // nullability are those of the physical inner for both
    // `LowCardinality(SAF(anyLast, Nullable(String)))` and a chained SAF.
    let (nullable, dict_value_type) = low_cardinality_dict_value_type(inner);

    // LowCardinality inner legality (canBeInsideLowCardinality) is owned by the
    // blanket `unsupported_header_type_name` check in `validate_column`; only the
    // encoder-coverage guard (does a body writer for this inner exist yet)
    // remains here.
    if !is_encodable(dict_value_type) {
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
    work.push(ValidationTask {
        field: ValidationField::Owned(dict_field),
        column: col.values.as_ref(),
        num_rows: num_keys,
        depth: depth + 1,
    });

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
fn validate_array<'a>(
    field: &Field,
    inner: &ChType,
    col: &'a ArrayColumn,
    num_rows: usize,
    depth: usize,
    work: &mut Vec<ValidationTask<'a>>,
) -> Result<(), EncodeError> {
    // Arrow LargeList offset invariants (shape, leading 0, monotonic, final
    // offset == flattened element count). Equal adjacent offsets (empty rows)
    // are fine, matching the server's own non-decreasing check in
    // `deserializeOffsetsBinaryBulk`.
    let element_rows = col.values.len();
    validate_offsets(
        field,
        "Array",
        "flattened element rows",
        &col.offsets,
        element_rows,
        num_rows,
    )?;

    // The element column is validated recursively as its own column of
    // `element_rows` rows, so every element-level guard applies to the flattened
    // buffer too.
    let element_field = Field {
        name: format!("{} element", field.name),
        ch_type: inner.clone(),
    };
    work.push(ValidationTask {
        field: ValidationField::Owned(element_field),
        column: col.values.as_ref(),
        num_rows: element_rows,
        depth: depth + 1,
    });
    Ok(())
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
/// Tuple element-name legality (no mixed named/unnamed, empty, reserved
/// lowercase `null`, or duplicate names, per `DataTypeTuple::checkTupleNames`)
/// is owned by the blanket `unsupported_header_type_name` check in
/// [`validate_column`], which reports it as `UnsupportedType`; only buffer shape
/// is checked here.
fn validate_tuple<'a>(
    field: &Field,
    elements: &[(Option<String>, ChType)],
    col: &'a TupleColumn,
    num_rows: usize,
    depth: usize,
    work: &mut Vec<ValidationTask<'a>>,
) -> Result<(), EncodeError> {
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
    for (i, ((name, element_type), element_col)) in
        elements.iter().zip(&col.fields).enumerate().rev()
    {
        let element_field = Field {
            name: match name {
                Some(n) => format!("{} element {n:?}", field.name),
                None => format!("{} element {}", field.name, i + 1),
            },
            ch_type: element_type.clone(),
        };
        work.push(ValidationTask {
            field: ValidationField::Owned(element_field),
            column: element_col,
            num_rows,
            depth: depth + 1,
        });
    }
    Ok(())
}

/// Validate a `Map(K, V)` column before any bytes are written.
///
/// [`encode_map_data`] writes `offsets[1..]` verbatim as raw `u64` and then the
/// flattened key and value runs, so the same Arrow offset invariants as
/// [`validate_array`] must hold: `num_rows + 1` offsets starting at 0,
/// monotonically non-decreasing, final offset equal to the flattened entries
/// length. Map key legality (the server's `DataTypeMap::isValidKeyType`: no
/// `Nullable` or `LowCardinality(Nullable(...))` key) is owned by the blanket
/// `unsupported_header_type_name` check in [`validate_column`], which reports it
/// as `UnsupportedType` since the type itself cannot exist on the server. The
/// entries buffer must be a two-field `Tuple` column (keys then values) whose
/// fields are validated recursively as their own columns of
/// `offsets[num_rows]` rows, so every key/value-level guard applies to the
/// flattened buffers.
fn validate_map<'a>(
    field: &Field,
    key: &ChType,
    value: &ChType,
    col: &'a MapColumn,
    num_rows: usize,
    depth: usize,
    work: &mut Vec<ValidationTask<'a>>,
) -> Result<(), EncodeError> {
    let reject = |detail: String| Err(EncodeError::InconsistentBatch { detail });

    // Map key legality (no `Nullable` or `LowCardinality(Nullable(...))` key) is
    // owned by the blanket `unsupported_header_type_name` check in
    // `validate_column`; only buffer shape is checked here.

    // The same Arrow list offset invariants as `validate_array`; `MapColumn`'s
    // offsets are physically the Array offsets of the wire's
    // Array(Tuple(keys, values)).
    let entry_rows = col.entries.len();
    validate_offsets(
        field,
        "Map",
        "flattened entry rows",
        &col.offsets,
        entry_rows,
        num_rows,
    )?;

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
    let value_field = Field {
        name: format!("{} value", field.name),
        ch_type: value.clone(),
    };
    work.push(ValidationTask {
        field: ValidationField::Owned(value_field),
        column: &entries.fields[1],
        num_rows: entry_rows,
        depth: depth + 1,
    });
    work.push(ValidationTask {
        field: ValidationField::Owned(key_field),
        column: &entries.fields[0],
        num_rows: entry_rows,
        depth: depth + 1,
    });
    Ok(())
}

/// Validate a Variant's discriminator run, Arrow Dense Union tree, and dense
/// child columns.
///
/// `VariantColumn::discriminators` is the single source of routing truth: the
/// exact Native wire run of one global UInt8 discriminator per logical row (255
/// = NULL). `layout` is the deterministic Arrow view the decoder derives from
/// it, so this walks the discriminators once with running per-child counters
/// and requires every stored routing entry to equal the canonical derivation,
/// without materializing a throwaway layout (the old recompute allocated ~5
/// bytes per row per encoded column). The comparison covers everything the
/// whole-layout equality did: an out-of-range discriminator (rejected by the
/// shared [`variant_child_counts`], the same helper the streaming scan uses),
/// the wrong flat-vs-nested shape for the alternative count, non-canonical
/// type ids, offsets that are not occurrence ordinals, and malformed groups.
/// Client-side rejection stays deliberately stricter than the server, whose
/// bulk deserialize path has no discriminator range check at all.
fn validate_variant<'a>(
    field: &Field,
    alternatives: &[ChType],
    col: &'a VariantColumn,
    num_rows: usize,
    depth: usize,
    work: &mut Vec<ValidationTask<'a>>,
) -> Result<(), EncodeError> {
    let reject = |detail: String| Err(EncodeError::InconsistentBatch { detail });

    if alternatives.len() != col.variants.len() {
        return reject(format!(
            "column {:?} declares {} Variant alternatives but the buffer carries {} child columns",
            field.name,
            alternatives.len(),
            col.variants.len()
        ));
    }

    // The discriminator run carries one byte per logical row, so its length is
    // the row count. Check it before the layout walk so a short/long run is
    // reported as a row-count mismatch rather than surfacing later as a layout
    // mismatch.
    if col.discriminators().len() != num_rows {
        return reject(format!(
            "column {:?} declares {num_rows} Variant rows but carries {} discriminators",
            field.name,
            col.discriminators().len()
        ));
    }

    // Every dense offset is an occurrence ordinal bounded by the row count, so
    // the i32 routing entries only exist below i32::MAX rows (the same guard
    // the decode-side layout builder applies).
    if num_rows > i32::MAX as usize {
        return reject(format!(
            "column {:?} Variant row count {num_rows} exceeds Arrow Dense Union's i32 child-offset range",
            field.name
        ));
    }

    // Count each alternative's rows through the helper shared with the
    // streaming skip scan, so an out-of-range discriminator is rejected with
    // the same error on both paths, and the counts the child columns must
    // match come back without any per-row materialization.
    let (counts, null_count) = variant_child_counts(col.discriminators(), alternatives.len())
        .map_err(|err| EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} has invalid Variant discriminators: {err}",
                field.name
            ),
        })?;

    validate_variant_layout(field, alternatives.len(), col)?;

    if col.nulls.len != null_count || col.nulls.validity.is_some() {
        return reject(format!(
            "column {:?} Variant Null child has length {} and validity={}, expected {null_count} rows with no bitmap",
            field.name,
            col.nulls.len,
            col.nulls.validity.is_some()
        ));
    }

    for (alternative, ((ch_type, child), expected)) in alternatives
        .iter()
        .zip(&col.variants)
        .zip(counts)
        .enumerate()
        .rev()
    {
        if child.len() != expected {
            return reject(format!(
                "column {:?} Variant alternative {alternative} has {} values, expected {expected} from the discriminators",
                field.name,
                child.len()
            ));
        }
        let child_field = Field {
            name: format!("{} alternative {alternative}", field.name),
            ch_type: ch_type.clone(),
        };
        work.push(ValidationTask {
            field: ValidationField::Owned(child_field),
            column: child,
            num_rows: expected,
            depth: depth + 1,
        });
    }
    Ok(())
}

/// Require `col.layout` to be exactly the canonical Arrow routing the
/// discriminators imply, comparing entry by entry against running counters
/// instead of materializing a second layout.
///
/// The caller has already proved every discriminator in range and the row
/// count within i32, so the counter arithmetic here cannot overflow and the
/// per-row expectations are total.
fn validate_variant_layout(
    field: &Field,
    num_variants: usize,
    col: &VariantColumn,
) -> Result<(), EncodeError> {
    let reject = |detail: String| Err(EncodeError::InconsistentBatch { detail });
    let mismatch = |row: usize| {
        Err(EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} Variant routing buffers diverge from the canonical Arrow layout of its discriminators at row {row}",
                field.name
            ),
        })
    };
    let discriminators = col.discriminators();
    let num_rows = discriminators.len();
    let mut child_rows = vec![0usize; num_variants];
    let mut null_rows = 0usize;

    match &col.layout {
        VariantLayout::Flat { type_ids, offsets } => {
            if num_variants >= ARROW_UNION_MAX_CHILDREN {
                return reject(format!(
                    "column {:?} Variant with {num_variants} alternatives must use the nested union layout, but carries a flat one",
                    field.name
                ));
            }
            if type_ids.len() != num_rows || offsets.len() != num_rows {
                return reject(format!(
                    "column {:?} Variant carries {} type ids and {} offsets for {num_rows} rows",
                    field.name,
                    type_ids.len(),
                    offsets.len()
                ));
            }
            for (row, &discriminator) in discriminators.iter().enumerate() {
                let (expected_id, expected_offset) = if discriminator == u8::MAX {
                    let offset = null_rows;
                    null_rows += 1;
                    (num_variants as i8, offset)
                } else {
                    let alternative = discriminator as usize;
                    let offset = child_rows[alternative];
                    child_rows[alternative] += 1;
                    (discriminator as i8, offset)
                };
                if type_ids[row] != expected_id || offsets[row] != expected_offset as i32 {
                    return mismatch(row);
                }
            }
        }
        VariantLayout::Nested {
            type_ids,
            offsets,
            groups,
        } => {
            if num_variants < ARROW_UNION_MAX_CHILDREN {
                return reject(format!(
                    "column {:?} Variant with {num_variants} alternatives must use the flat union layout, but carries a nested one",
                    field.name
                ));
            }
            let num_groups = num_variants.div_ceil(ARROW_UNION_MAX_CHILDREN);
            if groups.len() != num_groups {
                return reject(format!(
                    "column {:?} Variant carries {} union groups, expected {num_groups}",
                    field.name,
                    groups.len()
                ));
            }
            for (group_index, group) in groups.iter().enumerate() {
                if group.first_variant != group_index * ARROW_UNION_MAX_CHILDREN {
                    return reject(format!(
                        "column {:?} Variant union group {group_index} starts at alternative {}, expected {}",
                        field.name,
                        group.first_variant,
                        group_index * ARROW_UNION_MAX_CHILDREN
                    ));
                }
            }
            if type_ids.len() != num_rows || offsets.len() != num_rows {
                return reject(format!(
                    "column {:?} Variant carries {} type ids and {} offsets for {num_rows} rows",
                    field.name,
                    type_ids.len(),
                    offsets.len()
                ));
            }
            let mut group_rows = vec![0usize; num_groups];
            for (row, &discriminator) in discriminators.iter().enumerate() {
                if discriminator == u8::MAX {
                    if type_ids[row] != num_groups as i8 || offsets[row] != null_rows as i32 {
                        return mismatch(row);
                    }
                    null_rows += 1;
                    continue;
                }
                let alternative = discriminator as usize;
                let group_index = alternative / ARROW_UNION_MAX_CHILDREN;
                let local_id = alternative % ARROW_UNION_MAX_CHILDREN;
                let group_row = group_rows[group_index];
                let group = &groups[group_index];
                if type_ids[row] != group_index as i8
                    || offsets[row] != group_row as i32
                    || group.type_ids.get(group_row) != Some(&(local_id as i8))
                    || group.offsets.get(group_row) != Some(&(child_rows[alternative] as i32))
                {
                    return mismatch(row);
                }
                group_rows[group_index] += 1;
                child_rows[alternative] += 1;
            }
            // Every group buffer must hold exactly the rows the discriminators
            // routed to it; a longer buffer would smuggle unreferenced entries
            // past the per-row comparison above.
            for (group_index, (group, expected)) in groups.iter().zip(group_rows).enumerate() {
                if group.type_ids.len() != expected || group.offsets.len() != expected {
                    return reject(format!(
                        "column {:?} Variant union group {group_index} carries {} type ids and {} offsets, expected {expected}",
                        field.name,
                        group.type_ids.len(),
                        group.offsets.len()
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Validate Dynamic's block-local routing and child columns.
///
/// A V1/V2-shaped column is identified by its single `SharedVariant` child. In
/// that shape every child must be in ClickHouse's canonical global Variant
/// order and the number of direct typed children must fit the declared
/// `max_types`. A FLATTENED column has typed children only and preserves its
/// transmitted order; it may exceed `max_types` because the server expands
/// overflow values out of SharedVariant for this representation. A shared-less
/// column only encodes as FLATTENED when [`EncodeOptions::flattened_dynamic`]
/// is set; otherwise it takes the V1/V2 fallback with an implicit empty
/// SharedVariant, so the V1/V2 count limits apply to it too. Both shapes use
/// occurrence-ordinal dense offsets, permitting the body writer to emit one
/// routing run followed by one bulk body per child with no materialization.
fn validate_dynamic<'a>(
    field: &Field,
    max_types: u8,
    col: &'a DynamicColumn,
    num_rows: usize,
    options: &EncodeOptions,
    depth: usize,
    work: &mut Vec<ValidationTask<'a>>,
) -> Result<(), EncodeError> {
    let reject = |detail: String| Err(EncodeError::InconsistentBatch { detail });

    // Charge the cumulative nesting budget before walking this level's
    // children. The per-column `type_depth` cap bounds one declared type, but
    // every Dynamic level carries fresh child `ChType`s that restart it, so an
    // unbounded chain of nested DynamicColumns would otherwise overflow the
    // stack here (and in the write phase, which recurses over the same tree).
    if depth >= MAX_TYPE_DEPTH {
        return reject(format!(
            "column {:?} Dynamic nesting exceeds the maximum type depth {MAX_TYPE_DEPTH}",
            field.name
        ));
    }
    // Each child type is caller-constructed and about to be rendered
    // (`to_string`) and semantically walked below, both of which recurse to
    // its full depth, so bound it first with the same iterative walk
    // `validate_column` applies to the declared type.
    for child in &col.children {
        if let DynamicChild::Typed { ch_type, .. } = child {
            if type_depth(ch_type) > MAX_TYPE_DEPTH {
                return reject(format!(
                    "column {:?} Dynamic child type nesting exceeds the maximum depth of {MAX_TYPE_DEPTH} wrapper/container levels",
                    field.name
                ));
            }
        }
    }

    if col.type_ids.len() != num_rows || col.offsets.len() != num_rows {
        return reject(format!(
            "column {:?} declares {num_rows} Dynamic rows but carries {} type ids and {} offsets",
            field.name,
            col.type_ids.len(),
            col.offsets.len()
        ));
    }
    // `>=`, matching decode's `read_dynamic_state`: the u32 routing model
    // reserves u32::MAX as the NULL sentinel, so u32::MAX children could not
    // all be addressed.
    if col.children.len() >= u32::MAX as usize {
        return reject(format!(
            "column {:?} Dynamic has {} children, exceeding its u32 type-id routing model (u32::MAX is the NULL sentinel)",
            field.name,
            col.children.len()
        ));
    }

    let shared_count = col
        .children
        .iter()
        .filter(|child| matches!(child, DynamicChild::Shared(_)))
        .count();
    if shared_count > 1 {
        return reject(format!(
            "column {:?} Dynamic carries {shared_count} SharedVariant children; at most one is legal",
            field.name
        ));
    }
    let direct_count = col.children.len() - shared_count;
    // The V1/V2 wire shape applies to a column with a real SharedVariant child
    // and, when FLATTENED is not opted into, to a shared-less column too (its
    // implicit empty SharedVariant still occupies a discriminator slot).
    let v1_v2_wire = shared_count == 1 || !options.flattened_dynamic;
    if v1_v2_wire {
        if direct_count > max_types as usize {
            return reject(format!(
                "column {:?} Dynamic carries {direct_count} direct types, exceeding max_types={max_types}",
                field.name
            ));
        }
        if direct_count + 1 > u8::MAX as usize {
            return reject(format!(
                "column {:?} direct Dynamic carries {} children including SharedVariant, exceeding the UInt8 discriminator range",
                field.name,
                direct_count + 1
            ));
        }
    }
    if shared_count == 1 {
        let mut previous: Option<String> = None;
        for child in &col.children {
            let name = match child {
                DynamicChild::Typed { ch_type, .. } => ch_type.to_string(),
                DynamicChild::Shared(_) => "SharedVariant".to_string(),
            };
            if previous.as_ref().is_some_and(|prior| prior >= &name) {
                return reject(format!(
                    "column {:?} direct Dynamic children are not unique and canonically ordered at {name:?}",
                    field.name
                ));
            }
            previous = Some(name);
        }
    }

    let mut typed_names = std::collections::HashSet::with_capacity(direct_count);
    for child in &col.children {
        if let DynamicChild::Typed { ch_type, .. } = child {
            let name = ch_type.to_string();
            if !typed_names.insert(name.clone()) {
                return reject(format!(
                    "column {:?} Dynamic carries duplicate direct type {name}",
                    field.name
                ));
            }
            if resolves_to_nothing(ch_type) || !is_valid_variant_alternative(ch_type) {
                return Err(EncodeError::UnsupportedType {
                    column: field.name.clone(),
                    ch_type: field.ch_type.clone(),
                });
            }
        }
    }

    let mut counts = vec![0usize; col.children.len()];
    let mut null_count = 0usize;
    for (row, (&type_id, &offset)) in col.type_ids.iter().zip(&col.offsets).enumerate() {
        let offset = usize::try_from(offset).map_err(|_| EncodeError::InconsistentBatch {
            detail: format!(
                "column {:?} Dynamic row {row} has negative child offset {offset}",
                field.name
            ),
        })?;
        if type_id == u32::MAX {
            if offset != null_count {
                return reject(format!(
                    "column {:?} Dynamic NULL row {row} has offset {offset}, expected {null_count}",
                    field.name
                ));
            }
            null_count += 1;
            continue;
        }
        let child = type_id as usize;
        let Some(expected) = counts.get_mut(child) else {
            return reject(format!(
                "column {:?} Dynamic row {row} has type id {type_id}, but only {} children exist",
                field.name,
                col.children.len()
            ));
        };
        if offset != *expected {
            return reject(format!(
                "column {:?} Dynamic row {row} points to child {child} offset {offset}, expected {}",
                field.name, *expected
            ));
        }
        *expected += 1;
    }

    if col.nulls.len != null_count || col.nulls.validity.is_some() {
        return reject(format!(
            "column {:?} Dynamic Null child has length {} and validity={}, expected {null_count} rows with no bitmap",
            field.name,
            col.nulls.len,
            col.nulls.validity.is_some()
        ));
    }

    // Collect the Typed children and push them reversed so the worklist pops
    // them in forward declaration order, matching validate_tuple/validate_variant.
    // Only the push order is reversed: unlike variant's pure length guard, the
    // Shared arm here runs a substantive inline Utf8 validation, so reversing the
    // whole loop would change which sibling error surfaces first. Keeping the
    // inline length/validity/Utf8 checks in forward order preserves that precedence.
    let mut typed_children = Vec::new();
    for (child_index, (child, expected)) in col.children.iter().zip(counts).enumerate() {
        if child.len() != expected {
            return reject(format!(
                "column {:?} Dynamic child {child_index} has {} values, expected {expected} from the routing buffers",
                field.name,
                child.len()
            ));
        }
        match child {
            DynamicChild::Typed { ch_type, values } => {
                let child_field = Field {
                    name: format!("{} Dynamic child {ch_type}", field.name),
                    ch_type: ch_type.clone(),
                };
                typed_children.push(ValidationTask {
                    field: ValidationField::Owned(child_field),
                    column: values,
                    num_rows: expected,
                    depth: depth + 1,
                });
            }
            DynamicChild::Shared(values) => {
                if values.validity.is_some() {
                    return reject(format!(
                        "column {:?} Dynamic SharedVariant child carries a validity bitmap; NULL is represented by the outer discriminator",
                        field.name
                    ));
                }
                validate_utf8_column(field, values, expected)?;
            }
        }
    }
    for task in typed_children.into_iter().rev() {
        work.push(task);
    }
    Ok(())
}

/// Validate a `JSON` column's typed paths, block-local dynamic paths, and
/// shared-data layout before any bytes are written, mirroring the rigor of
/// [`validate_dynamic`].
///
/// A `Text`-body column just validates its one-string-per-row `Utf8Column`.
/// A structured body must carry exactly the declared typed paths (names and
/// order), each a valid child column of `num_rows`; strictly sorted, unique
/// dynamic path names within `max_dynamic_paths`, each a valid `Dynamic` column
/// at `max_dynamic_types`; and a shared-data offset run that is a well-formed
/// Arrow list over the matching flattened `paths`/`values` string columns.
///
/// `depth` is charged before recursing, so a pathologically nested JSON (its
/// typed-path or dynamic-path children carry fresh types that restart the
/// per-type cap) errors rather than overflowing the stack.
#[allow(clippy::too_many_arguments)]
fn validate_json<'a>(
    field: &Field,
    max_dynamic_paths: u32,
    max_dynamic_types: u8,
    typed_paths: &[(String, ChType)],
    col: &'a JsonColumn,
    num_rows: usize,
    options: &EncodeOptions,
    depth: usize,
    work: &mut Vec<ValidationTask<'a>>,
) -> Result<(), EncodeError> {
    let reject = |detail: String| Err(EncodeError::InconsistentBatch { detail });

    if depth >= MAX_TYPE_DEPTH {
        return reject(format!(
            "column {:?} JSON nesting exceeds the maximum type depth {MAX_TYPE_DEPTH}",
            field.name
        ));
    }

    let structured = match &col.body {
        JsonBody::Text(values) => {
            if values.null_count() > 0 {
                return reject(format!(
                    "column {:?} JSON text body carries nulls; the STRING wire form has no null map (use Nullable(JSON))",
                    field.name
                ));
            }
            return validate_utf8_column(field, values, num_rows);
        }
        JsonBody::Structured(structured) => structured,
    };

    // Typed paths must be exactly the declared paths, in the same sorted order.
    if structured.typed.len() != typed_paths.len() {
        return reject(format!(
            "column {:?} JSON declares {} typed paths but the buffer carries {}",
            field.name,
            typed_paths.len(),
            structured.typed.len()
        ));
    }
    // Push reversed so typed paths pop in forward declaration order, matching
    // validate_tuple/validate_variant. The inline path-name guard mirrors
    // variant's length guard, so reversing it only reorders which of multiple
    // simultaneous mismatches surfaces first, not whether the column is rejected.
    for ((declared_path, declared_type), (buffer_path, buffer_column)) in
        typed_paths.iter().zip(&structured.typed).rev()
    {
        if declared_path != buffer_path {
            return reject(format!(
                "column {:?} JSON typed path {buffer_path:?} does not match the declared path {declared_path:?}",
                field.name
            ));
        }
        let typed_field = Field {
            name: format!("{} JSON path {declared_path}", field.name),
            ch_type: declared_type.clone(),
        };
        work.push(ValidationTask {
            field: ValidationField::Owned(typed_field),
            column: buffer_column,
            num_rows,
            depth: depth + 1,
        });
    }

    // Dynamic paths: strictly sorted and unique, each a valid Dynamic column at
    // max_dynamic_types. The `max_dynamic_paths` bound applies only to the V1/V2
    // wire shape: there the writer routes any overflow path into shared data, so
    // the direct list cannot exceed the bound. A column that encodes FLATTENED
    // (empty shared data + the opt-in) carries the union of dynamic and
    // shared-data paths and legitimately exceeds it, matching the server's
    // unbounded FLATTENED reader (`unflattenAndInsertPaths`,
    // `SerializationObjectHelpers.cpp`, v26.6.1.1193-stable). Gating on the same
    // shape decision the writer makes keeps a FLATTENED-decoded column with more
    // paths than `max_dynamic_paths` re-encodable while still rejecting a V1/V2
    // column the server could not have produced.
    if !json_uses_flattened(structured, options)
        && structured.dynamic.len() > max_dynamic_paths as usize
    {
        return reject(format!(
            "column {:?} JSON carries {} dynamic paths, exceeding max_dynamic_paths={max_dynamic_paths} for the V1/V2 wire shape",
            field.name,
            structured.dynamic.len()
        ));
    }
    let mut previous: Option<&str> = None;
    for (path, dynamic_col) in &structured.dynamic {
        if previous.is_some_and(|prior| prior >= path.as_str()) {
            return reject(format!(
                "column {:?} JSON dynamic paths are not strictly sorted and unique at {path:?}",
                field.name
            ));
        }
        previous = Some(path);
        let dynamic_field = Field {
            name: format!("{} JSON dynamic path {path}", field.name),
            ch_type: ChType::Dynamic {
                max_types: max_dynamic_types,
            },
        };
        validate_dynamic(
            &dynamic_field,
            max_dynamic_types,
            dynamic_col,
            num_rows,
            options,
            depth + 1,
            work,
        )?;
    }

    // Shared data: the paths/values pair counts must agree, the offset run must
    // be a well-formed Arrow list over that pair count, and each string column's
    // own offsets must be valid.
    if structured.shared_paths.len() != structured.shared_values.len() {
        return reject(format!(
            "column {:?} JSON shared data has {} paths but {} values",
            field.name,
            structured.shared_paths.len(),
            structured.shared_values.len()
        ));
    }
    validate_offsets(
        field,
        "JSON shared",
        "flattened shared pairs",
        &structured.shared_offsets,
        structured.shared_paths.len(),
        num_rows,
    )?;
    if structured.shared_paths.null_count() > 0 {
        return reject(format!(
            "column {:?} JSON shared paths carry nulls; shared data has no wire null map",
            field.name
        ));
    }
    if structured.shared_values.null_count() > 0 {
        return reject(format!(
            "column {:?} JSON shared values carry nulls; shared data has no wire null map",
            field.name
        ));
    }
    let shared_pairs = structured.shared_paths.len();
    validate_utf8_column(field, &structured.shared_paths, shared_pairs)?;
    validate_utf8_column(field, &structured.shared_values, shared_pairs)?;
    Ok(())
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
    if let Some(under) = value_type.physical_delegate_ref() {
        return column_variant_matches(under.as_ref(), column);
    }
    if let (ChType::AggregateFunction { .. }, Column::AggregateState(_)) = (value_type, column) {
        // Codec legality is owned by the blanket `unsupported_header_type_name`
        // check in `validate_column`, which runs before this; only the buffer
        // variant remains to match here.
        return true;
    }
    if let (ChType::QBit { .. }, Column::QBit(_)) = (value_type, column) {
        // Dimension and scalar-child shape are checked by validate_qbit so a
        // malformed public QBitColumn reports InconsistentBatch, not an
        // unsupported type/variant mismatch.
        return true;
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
    if let (ChType::Variant(alternatives), Column::Variant(c)) = (value_type, column) {
        return alternatives.len() == c.variants.len()
            && alternatives
                .iter()
                .zip(&c.variants)
                .all(|(t, child)| column_variant_matches(t.inner(), child));
    }
    if let (ChType::Dynamic { .. }, Column::Dynamic(_)) = (value_type, column) {
        return true;
    }
    // JSON's typed-path, dynamic-path, and shared-data children are validated
    // against the concrete JsonColumn in `validate_json`, so only the buffer
    // variant is matched here, like Dynamic.
    if let (ChType::Json { .. }, Column::Json(_)) = (value_type, column) {
        return true;
    }
    matches!(
        (value_type, column),
        (ChType::Nothing, Column::Nothing(_))
            | (ChType::Bool, Column::Bool(_))
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
            | (ChType::BFloat16, Column::BFloat16(_))
            | (ChType::Date, Column::Date(_))
            | (ChType::Date32, Column::Date32(_))
            | (ChType::DateTime { .. }, Column::DateTime(_))
            | (ChType::DateTime64 { .. }, Column::DateTime64(_))
            | (ChType::Time, Column::Time(_))
            | (ChType::Time64 { .. }, Column::Time64(_))
            | (ChType::Interval(_), Column::Interval(_))
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

/// Validate an Arrow LargeBinary aggregate-state buffer and every row's
/// function-specific state boundary before writing any bytes.
///
/// `codec` is resolved once by [`validate_column`] (codec legality is owned by
/// its blanket `unsupported_header_type_name` check), so this only checks the
/// LargeBinary offsets and each row's state.
fn validate_aggregate_state_column(
    field: &Field,
    ch_type: &ChType,
    codec: AggregateStateCodec,
    col: &AggregateStateColumn,
    num_rows: usize,
) -> Result<(), EncodeError> {
    // Arrow LargeBinary offset invariants (shape, leading 0, monotonic, final
    // offset == data length).
    validate_offsets(
        field,
        "AggregateFunction state",
        "state data bytes",
        &col.offsets,
        col.data.len(),
        num_rows,
    )?;

    // Each row's slice must be exactly one valid serialized state. `validate_offsets`
    // proved every offset non-negative, monotonic, and within the data buffer,
    // so the casts and slice below cannot wrap or go out of bounds.
    for (row, pair) in col.offsets.windows(2).enumerate() {
        let state = &col.data[pair[0] as usize..pair[1] as usize];
        if !is_valid_aggregate_state(state, codec) {
            return Err(EncodeError::InconsistentBatch {
                detail: format!(
                    "column {:?} row {row} is not exactly one valid serialized {ch_type} state",
                    field.name
                ),
            });
        }
    }
    Ok(())
}

/// Validate a `Utf8Column`'s Arrow offsets before its body is written.
///
/// [`encode_string_data`] slices `data[offsets[i]..offsets[i+1]]` per row, so a
/// caller-constructed column with non-monotonic offsets, an offset past
/// `data.len()`, or a negative offset (which `as usize` wraps to a huge value)
/// would panic mid-write, and offsets that do not cover `data` exactly would
/// silently drop leading or trailing bytes from the wire. `Column` fields are
/// public and bindings build these by hand for the insert path, so the shared
/// [`validate_offsets`] guard rejects all of these as
/// [`EncodeError::InconsistentBatch`] rather than trust the buffer. `String`
/// carries no per-type extras beyond the offset invariants, so this is a thin
/// wrapper over that guard. This is O(num_rows) once per column, off the
/// per-byte write path.
fn validate_utf8_column(
    field: &Field,
    col: &Utf8Column,
    num_rows: usize,
) -> Result<(), EncodeError> {
    validate_offsets(
        field,
        "String",
        "data bytes",
        &col.offsets,
        col.data.len(),
        num_rows,
    )
}
