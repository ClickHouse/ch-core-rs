//! Parses ClickHouse type strings into [`ChType`] and answers type-shape
//! questions shared by decode and encode.
//!
//! No wire I/O lives here: the parser takes a `&str` and returns an
//! `Option<ChType>`, so it is self-contained.

use crate::native::aggregate_function::aggregate_state_codec;
use crate::native::protocol::MAX_TYPE_DEPTH;
use crate::schema::{ChType, GeoKind, IntervalKind};

// ---------------------------------------------------------------------------
// Type name parsing
// ---------------------------------------------------------------------------

/// Parse a ClickHouse type name string into a [`crate::schema::ChType`].
///
/// Accepts the canonical spellings the server writes in Native block headers,
/// including the `Nullable`, `LowCardinality`, `Array`, `Tuple`, and `Map`
/// container forms. Returns `None` for an unsupported or malformed name. The
/// input is treated as untrusted wire data: parsing is depth-bounded (see
/// `MAX_TYPE_DEPTH`) and never panics. Also used by the encoder to confirm a
/// rendered type string round-trips (a header this parser rejects is one the
/// server rejects too), and by bindings that map type names to columns without
/// decoding a block.
pub fn parse_ch_type(type_name: &str) -> Option<ChType> {
    parse_ch_type_depth(type_name, 0)
}

/// Parse a ClickHouse type name at nesting depth `depth`, rejecting anything past
/// [`MAX_TYPE_DEPTH`] so an unbounded hostile type string cannot overflow the
/// stack. Each recursing arm (`Nullable`, `LowCardinality`, `Array`) calls this
/// with `depth + 1`; the non-recursive leaf arms are depth independent.
fn parse_ch_type_depth(type_name: &str, depth: usize) -> Option<ChType> {
    // Bound recursion before doing any work at this level. A rejected over-deep
    // type surfaces as `UnsupportedType`, exactly like any other unparseable
    // header.
    if depth > MAX_TYPE_DEPTH {
        return None;
    }

    // Nullable wrapper. ClickHouse forbids a `Nullable`, a `LowCardinality`, or an
    // `Array` directly inside a `Nullable`: the only legal nesting with
    // LowCardinality is `LowCardinality(Nullable(T))`, never the reverse,
    // `Nullable(Nullable(T))` does not exist at all, and
    // `DataTypeArray::canBeInsideNullable()` is false so `Nullable(Array(T))` is
    // not constructible. An honest server never emits any of these, but the type
    // string is untrusted wire input, so reject them here. `decode_column` and
    // `skip_column_data` unwrap exactly one `Nullable` and handle `LowCardinality`
    // and `Array` only at the top level; accepting a nested wrapper would let those
    // inner wrappers reach an `unreachable!` on malformed bytes (AGENTS.md invariant
    // 2: no panics on malformed input). `Nullable(Map(K, V))` is likewise not
    // constructible (`DataTypeMap::canBeInsideNullable()` is false), so `Map` is
    // rejected here too. `Nullable(Tuple(...))` IS legal
    // (`DataTypeTuple::canBeInsideNullable()` is true at v26.6.1.1193-stable; the
    // `enable_nullable_tuple_type` DDL gate is a creation-time concern with no wire
    // effect), so `Tuple` deliberately passes this guard.
    if let Some(inner) = type_name.strip_prefix("Nullable(") {
        if let Some(inner) = inner.strip_suffix(')') {
            let inner_type = parse_ch_type_depth(inner, depth + 1)?;
            // `Nested` is an `Array` and the array-based geo kinds
            // (`Ring`/`LineString`/`Polygon`/`MultiLineString`/`MultiPolygon`)
            // expand to `Array`, so `Nullable` over any of them is as illegal as
            // `Nullable(Array(T))` (`DataTypeArray::canBeInsideNullable()` is
            // false). `Nullable(Point)` IS legal (Point is a `Tuple`, and
            // `DataTypeTuple::canBeInsideNullable()` is true). A
            // `SimpleAggregateFunction` inner delegates to its physical type, so
            // `Nullable(SAF(T))` is legal iff `Nullable(T)` is:
            // [`can_be_inside_nullable`] resolves the alias before deciding.
            if !can_be_inside_nullable(&inner_type) {
                return None;
            }
            return Some(ChType::Nullable(Box::new(inner_type)));
        }
    }

    // LowCardinality wrapper. The inner type is parsed recursively, so
    // LowCardinality(Nullable(String)) yields LowCardinality(Nullable(String)).
    if let Some(inner) = type_name.strip_prefix("LowCardinality(") {
        if let Some(inner) = inner.strip_suffix(')') {
            return parse_ch_type_depth(inner, depth + 1)
                .map(|t| ChType::LowCardinality(Box::new(t)));
        }
    }

    // Array wrapper. The element type is parsed recursively, so
    // `Array(LowCardinality(String))` yields `Array(LowCardinality(String))`. The
    // element may itself be `Nullable`, `LowCardinality`, or a further `Array`;
    // there is no inner-type restriction. The array is never wrapped in an outer
    // `Nullable` (rejected by the guard in the `Nullable(` arm above).
    if let Some(inner) = type_name.strip_prefix("Array(") {
        if let Some(inner) = inner.strip_suffix(')') {
            let inner_type = parse_ch_type_depth(inner, depth + 1)?;
            return Some(ChType::Array(Box::new(inner_type)));
        }
    }

    // Tuple(...). Unnamed (`Tuple(Int32, String)`), named
    // (`Tuple(a Int32, b String)`), and backtick-quoted-name
    // (`Tuple(`a b` Int32)`) forms, per `DataTypeTuple::doGetName` (confirmed at
    // v26.6.1.1193-stable). The inner list cannot be split naively on `,`: an
    // element type may carry commas inside its own parentheses
    // (`Decimal(9, 4)`, a nested `Tuple`), inside an Enum's single-quoted names,
    // or inside a backtick-quoted element name, so the splitter is
    // paren/quote-aware. Each element recurses at depth + 1, so the
    // MAX_TYPE_DEPTH cap bounds a hostile deeply-nested header exactly like the
    // single-child wrappers. `Tuple()` (zero elements) is accepted: it is
    // constructible and emittable, with its own one-byte-per-row wire layout.
    if let Some(inner) = type_name.strip_prefix("Tuple(") {
        if let Some(inner) = inner.strip_suffix(')') {
            return parse_tuple_elements(inner, depth).map(ChType::Tuple);
        }
    }

    // Nested(name1 T1, ...). With `flatten_nested = 0` the server writes this
    // literal spelling and the body is byte-identical to
    // `Array(Tuple(named elements))` (confirmed at v26.6.1.1193-stable,
    // `DataTypeNested.cpp`). Element names are MANDATORY (the server rejects an
    // unnamed element such as `Nested(UInt32)` at parse time), so a field
    // without a name makes the whole header unsupported. Nested-in-container
    // (`Array(Nested(...))`) is grammar-permitted but only INFERRED legal, never
    // test-confirmed; decode is lenient here and accepts it, delegating to the
    // underlying `Array(Tuple(...))` layout (see the type doc). Each element
    // recurses at depth + 1 so the MAX_TYPE_DEPTH cap bounds a hostile
    // deeply-nested header exactly like the Tuple arm.
    if let Some(inner) = type_name.strip_prefix("Nested(") {
        if let Some(inner) = inner.strip_suffix(')') {
            // A `Nested(...)` expands to `Array(Tuple(fields))`, two physical
            // levels above each field, so charge the fields `depth + 2` to match
            // the encoder's `type_depth`. `parse_nested_elements` -> its
            // `parse_tuple_element` adds one more level, so pass `depth + 1` here
            // for the fields to land at `depth + 2` and stay inside the same
            // `MAX_TYPE_DEPTH` bound the physical decode recursion respects.
            return parse_nested_elements(inner, depth + 1).map(ChType::Nested);
        }
    }

    // SimpleAggregateFunction(func[, T]). Pure name decoration over the inner
    // type T (confirmed at v26.6.1.1193-stable,
    // `DataTypeCustomSimpleAggregateFunction.cpp`): the wire bytes are exactly
    // T's. Grammar is `func[(litParams)], T`, where the function-name token may
    // itself contain parentheses (its literal params), so the split is on the
    // FIRST top-level comma after balancing parens, done by the same
    // paren/quote-aware `split_top_level_commas` the Tuple/Map arms use. The
    // function name is validated only for identifier-with-optional-params shape;
    // its params are NOT semantically checked, and the server's function
    // whitelist is deliberately NOT enforced (a server-authored header is
    // trusted and the list grows across versions).
    //
    // Parsed at ANY nesting depth: `SimpleAggregateFunction` is legal inside
    // wrappers and containers, and the server emits the SAF spelling verbatim
    // inside them. Confirmed live at v26.6.1.1193-stable: a Native header for
    // `Tuple(v SimpleAggregateFunction(sum, UInt64))`, `Array(SAF(...))`,
    // `Nullable(SAF(...))`, `LowCardinality(SAF(anyLast, String))`, and
    // `Map(String, SAF(...))` all carry the SAF spelling (hexdump-verified), and
    // the server test `04329_tuple_element_aggregation_reject_nullable_tuple`
    // corroborates it. Wrapper legality delegates to the inner type through
    // `physical_delegate`: `Nullable(SAF(T))` is legal iff `Nullable(T)` is (see
    // `can_be_inside_nullable`), and `LowCardinality`/`Map`-key validity resolve
    // through the delegate the same way. Each SAF level charges one depth (parse
    // inner at `depth + 1`), matching the encoder's `type_depth`, so a hostile
    // chain of SAFs is still bounded by `MAX_TYPE_DEPTH`. Multi-type-arg forms
    // (`SimpleAggregateFunction(f, T1, T2)`) parse server-side but only T1 is
    // physically load-bearing and they are unobserved in practice, so they are
    // rejected.
    if let Some(inner) = type_name.strip_prefix("SimpleAggregateFunction(") {
        if let Some(inner) = inner.strip_suffix(')') {
            let parts = split_top_level_commas(inner.trim_matches(' '))?;
            // Exactly a function name plus one type argument.
            if parts.len() != 2 {
                return None;
            }
            let func = parts[0].trim_matches(' ');
            if !is_simple_aggregate_func_spelling(func) {
                return None;
            }
            let inner_type = parse_ch_type_depth(parts[1].trim_matches(' '), depth + 1)?;
            return Some(ChType::SimpleAggregateFunction {
                func: func.to_string(),
                inner: Box::new(inner_type),
            });
        }
    }

    // AggregateFunction(func[, T]). Unlike SimpleAggregateFunction, this is a
    // real opaque state whose concrete aggregate function owns its row
    // serialization. Native adds no generic length prefix, so parsing is
    // deliberately gated by `aggregate_state_codec`: an unknown function must
    // remain UnsupportedType even for zero rows, otherwise the streaming scan
    // could not locate the next column. At v26.6.1.1193-stable the first
    // supported codec is exact `count`, with zero or one argument type. Each
    // argument is type metadata but still recurses at depth + 1 so hostile
    // nested headers remain bounded by MAX_TYPE_DEPTH.
    //
    // No state version is parsed. The server omits version 0 from canonical
    // names and emits no other version at the pin, so the first token is always
    // the function name. A versioned spelling like `AggregateFunction(2, sum,
    // UInt64)` treats `2` as an unknown function name and parse-rejects cleanly
    // (`UnsupportedType`); the explicit `AggregateFunction(0, count)` spelling,
    // which the server never emits, is likewise rejected rather than accepted.
    // Versioning is reintroduced with the first confirmed versioned codec.
    if let Some(inner) = type_name.strip_prefix("AggregateFunction(") {
        if let Some(inner) = inner.strip_suffix(')') {
            let parts = split_top_level_commas(inner.trim_matches(' '))?;
            let function = parts.first()?.trim_matches(' ');
            if function.is_empty() {
                return None;
            }

            let mut arguments = Vec::with_capacity(parts.len() - 1);
            for argument in &parts[1..] {
                arguments.push(parse_ch_type_depth(argument.trim_matches(' '), depth + 1)?);
            }

            let ch_type = ChType::AggregateFunction {
                function: function.to_string(),
                arguments,
            };
            return (unsupported_header_type_name(&ch_type).is_none()).then_some(ch_type);
        }
    }

    // Map(K, V). Always exactly the two type arguments (`DataTypeMap::doGetName`,
    // confirmed at v26.6.1.1193-stable); the nested tuple's "keys"/"values" names
    // never appear in the type string. Split on the top-level comma with the same
    // paren/quote-aware splitter the Tuple arm uses, since either argument can
    // carry commas (`Decimal(9, 4)`, a nested `Map`/`Tuple`, an Enum). Key-type
    // legality (no `Nullable` or `LowCardinality(Nullable(...))` keys) is
    // enforced by `validate_header_type` at header-read time, not here, so the
    // encoder's round-trip check and the decoder agree on what parses.
    if let Some(inner) = type_name.strip_prefix("Map(") {
        if let Some(inner) = inner.strip_suffix(')') {
            let parts = split_top_level_commas(inner.trim_matches(' '))?;
            if parts.len() != 2 {
                return None;
            }
            let key = parse_ch_type_depth(parts[0].trim_matches(' '), depth + 1)?;
            let value = parse_ch_type_depth(parts[1].trim_matches(' '), depth + 1)?;
            return Some(ChType::Map(Box::new(key), Box::new(value)));
        }
    }

    // FixedString(N). N must be positive: the server rejects FixedString(0) at
    // table-creation time, and a zero width cannot be represented in the
    // contiguous `width * num_rows` buffer (row count would be unrecoverable),
    // so treat it as unsupported rather than decode an inconsistent column.
    if let Some(n_str) = type_name.strip_prefix("FixedString(") {
        if let Some(n_str) = n_str.strip_suffix(')') {
            if let Ok(n) = n_str.trim().parse::<usize>() {
                if n > 0 {
                    return Some(ChType::FixedString(n));
                }
            }
        }
    }

    // Time64(P). The server's canonical Native header always includes the
    // precision and never carries a timezone. Although the type factory accepts
    // a bare Time64 as an input shorthand for precision 3, it normalizes that
    // input to Time64(3), so the wire parser accepts only the emitted form.
    // P is one decimal digit because the supported range is exactly 0..=9.
    if let Some(inner) = type_name.strip_prefix("Time64(") {
        if let Some(inner) = inner.strip_suffix(')') {
            let bytes = inner.as_bytes();
            if bytes.len() == 1 && bytes[0].is_ascii_digit() {
                return Some(ChType::Time64 {
                    precision: bytes[0] - b'0',
                });
            }
            return None;
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

    // Enum8(...) / Enum16(...). The server always emits the concrete keyword
    // with explicit values, never a bare `Enum(...)`, so only these two
    // spellings are parsed. The inner `'name' = value, ...` list is walked by a
    // quote-aware parser (`parse_enum_variants`) because a name can contain `,`
    // and `=` unescaped. An out-of-range value or any syntax error returns None
    // (UnsupportedType) rather than panicking on this untrusted string.
    if let Some(inner) = type_name.strip_prefix("Enum8(") {
        if let Some(inner) = inner.strip_suffix(')') {
            return parse_enum_variants::<i8>(inner).map(|variants| ChType::Enum8 { variants });
        }
    }
    if let Some(inner) = type_name.strip_prefix("Enum16(") {
        if let Some(inner) = inner.strip_suffix(')') {
            return parse_enum_variants::<i16>(inner).map(|variants| ChType::Enum16 { variants });
        }
    }

    // Decimal(P, S). The server always emits the canonical `Decimal(P, S)` form
    // on the wire via `DataTypeDecimal::doGetName` (comma-space, both fields
    // present), never `Decimal32(S)`/`Decimal64(S)`/etc., so only this spelling
    // is parsed (mirroring the decision to accept only `Enum8(`/`Enum16(`). The
    // byte width is derived from P, not carried in the per-row data. Any
    // out-of-range P/S or non-numeric field surfaces as None (UnsupportedType);
    // the type string is untrusted, so this never panics.
    if let Some(inner) = type_name.strip_prefix("Decimal(") {
        if let Some(inner) = inner.strip_suffix(')') {
            if let Some((p_str, s_str)) = inner.split_once(',') {
                let precision = p_str.trim().parse::<u8>().ok()?;
                let scale = s_str.trim().parse::<u8>().ok()?;
                // Constraint (server `DataTypeDecimal`): 1 <= P <= 76 and
                // 0 <= S <= P. The byte width follows from P.
                let bits = decimal_bits_from_precision(precision)?;
                if scale > precision {
                    return None;
                }
                return Some(ChType::Decimal {
                    precision,
                    scale,
                    bits,
                });
            }
            // Missing comma: not the canonical `Decimal(P, S)` the server emits.
            return None;
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
        // `DataTypeNothing` is registered under this exact case-sensitive
        // canonical name, with no alias.
        "Nothing" => Some(ChType::Nothing),
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
        // Registered under this exact case-sensitive name by
        // `registerDataTypeNumbers`; the server exposes no alias.
        "BFloat16" => Some(ChType::BFloat16),
        // Wide integers. The server emits exactly these case-sensitive spellings
        // (no parameters, no aliases) via `DataTypeNumber<T>::doGetName`.
        "Int128" => Some(ChType::Int128),
        "UInt128" => Some(ChType::UInt128),
        "Int256" => Some(ChType::Int256),
        "UInt256" => Some(ChType::UInt256),
        "Date" => Some(ChType::Date),
        "Date32" => Some(ChType::Date32),
        "DateTime" => Some(ChType::DateTime { timezone: None }),
        "Time" => Some(ChType::Time),
        // DataTypeInterval::doGetName emits these 11 exact case-sensitive
        // spellings, one per IntervalKind. There are no parameters or aliases.
        "IntervalYear" => Some(ChType::Interval(IntervalKind::Year)),
        "IntervalQuarter" => Some(ChType::Interval(IntervalKind::Quarter)),
        "IntervalMonth" => Some(ChType::Interval(IntervalKind::Month)),
        "IntervalWeek" => Some(ChType::Interval(IntervalKind::Week)),
        "IntervalDay" => Some(ChType::Interval(IntervalKind::Day)),
        "IntervalHour" => Some(ChType::Interval(IntervalKind::Hour)),
        "IntervalMinute" => Some(ChType::Interval(IntervalKind::Minute)),
        "IntervalSecond" => Some(ChType::Interval(IntervalKind::Second)),
        "IntervalMillisecond" => Some(ChType::Interval(IntervalKind::Millisecond)),
        "IntervalMicrosecond" => Some(ChType::Interval(IntervalKind::Microsecond)),
        "IntervalNanosecond" => Some(ChType::Interval(IntervalKind::Nanosecond)),
        "String" => Some(ChType::String),
        "UUID" => Some(ChType::Uuid),
        "IPv4" => Some(ChType::Ipv4),
        "IPv6" => Some(ChType::Ipv6),
        // Geo aliases. The server registers these case-sensitive with no
        // aliases and emits the bare spelling in the header (never the expanded
        // `Array(Tuple(...))` form); a wrong-case `point` is not a geo type and
        // falls through to `None` (`DataTypeCustomGeo`, confirmed at
        // v26.6.1.1193-stable). A geo token is a leaf here but expands to a fixed
        // `Tuple`/`Array`-of-`Float64` nesting, so `geo_within_depth` charges its
        // expansion depth against `MAX_TYPE_DEPTH`, keeping the decode cap aligned
        // with the encoder's `type_depth` (a geo-tipped header that decodes is
        // always re-encodable).
        "Point" => geo_within_depth(GeoKind::Point, depth),
        "Ring" => geo_within_depth(GeoKind::Ring, depth),
        "LineString" => geo_within_depth(GeoKind::LineString, depth),
        "MultiLineString" => geo_within_depth(GeoKind::MultiLineString, depth),
        "Polygon" => geo_within_depth(GeoKind::Polygon, depth),
        "MultiPolygon" => geo_within_depth(GeoKind::MultiPolygon, depth),
        _ => None,
    }
}

/// Accept a geo alias only if its physical expansion, added to the current
/// nesting `depth`, stays within [`MAX_TYPE_DEPTH`].
///
/// A geo token is a leaf in the parser, but it expands to a fixed
/// `Tuple`/`Array`-of-`Float64` nesting ([`GeoKind::expansion_depth`]) that the
/// physical decode recurses through and that the encoder's `type_depth` counts.
/// Charging that expansion here keeps decode-accept and encode-accept in exact
/// agreement at the cap: a geo-tipped chain the decoder accepts is always
/// re-encodable, and one it rejects the encoder rejects too.
fn geo_within_depth(kind: GeoKind, depth: usize) -> Option<ChType> {
    if depth + kind.expansion_depth() > MAX_TYPE_DEPTH {
        return None;
    }
    Some(ChType::Geo(kind))
}

/// Whether `inner` may sit directly inside `Nullable`, the server's
/// `IDataType::canBeInsideNullable()` (confirmed at v26.6.1.1193-stable): a
/// `Nullable`, `LowCardinality`, `Array`, or `Map` cannot, while a `Tuple` and
/// the scalars can.
///
/// Name-decoration aliases resolve through [`ChType::physical_delegate`] so the
/// physical type that actually governs is checked: `Nullable(SimpleAggregateFunction(T))`
/// is legal iff `Nullable(T)` is, `Nullable(Point)` is legal (Point is a Tuple),
/// and `Nullable(Ring)`/`Nullable(Nested(...))` are not (both expand to an
/// Array). The recursion is bounded by the parsed type depth, so it cannot run
/// away on untrusted input.
fn can_be_inside_nullable(inner: &ChType) -> bool {
    if let Some(under) = inner.physical_delegate() {
        return can_be_inside_nullable(&under);
    }
    !matches!(
        inner,
        ChType::Nullable(_)
            | ChType::LowCardinality(_)
            | ChType::Array(_)
            | ChType::Map(..)
            | ChType::AggregateFunction { .. }
    )
}

/// Return the first server-invalid or decoder-unsupported type shape nested in
/// `ch_type`, rendered the same way a Native header spells it.
///
/// Parsing and semantic type construction are deliberately separate in this
/// crate: for example, `LowCardinality(Decimal(9, 4))` and a `Map` with a
/// nullable key are grammatically well-formed but server-invalid. Aggregate
/// function arguments are metadata rather than nested column bodies, but the
/// server still constructs each argument type while resolving the function, so
/// this single walk covers them too.
///
/// This is the one owner of type-string legality for both directions: decode's
/// `validate_header_type` and encode's `validate_column` both run it, so nested
/// semantic restrictions (`LowCardinality` inners, `Map` keys, `Tuple` element
/// names, registered aggregate codecs) are enforced in exactly one place.
/// `Tuple` element-name legality is checked uniformly, not just inside aggregate
/// arguments: the server cannot construct a tuple with mixed, empty, reserved
/// `null`, or duplicate names, so such a header is unsupported wherever it
/// appears. Unnamed tuples remain valid.
pub(crate) fn unsupported_header_type_name(ch_type: &ChType) -> Option<String> {
    if let Some(under) = ch_type.physical_delegate() {
        return unsupported_header_type_name(&under);
    }

    match ch_type {
        ChType::LowCardinality(inner) => {
            let (_, dict_value_type) = low_cardinality_dict_value_type(inner);
            (!is_low_cardinality_inner(dict_value_type))
                .then(|| format!("LowCardinality({dict_value_type})"))
        }
        // Wrapper legality is enforced by the parser's Nullable arm. Recurse
        // only for nested semantic restrictions so existing encode error
        // classification for a caller-built illegal outer wrapper stays with
        // the type-string round-trip check.
        ChType::Nullable(inner) => unsupported_header_type_name(inner),
        ChType::Array(inner) => unsupported_header_type_name(inner),
        ChType::Tuple(elements) => {
            if !is_valid_tuple_element_names(elements) {
                Some(ch_type.to_string())
            } else {
                elements
                    .iter()
                    .find_map(|(_, element_type)| unsupported_header_type_name(element_type))
            }
        }
        ChType::Map(key, value) => {
            if !is_valid_map_key_type(key) {
                Some(ch_type.to_string())
            } else {
                unsupported_header_type_name(key).or_else(|| unsupported_header_type_name(value))
            }
        }
        ChType::AggregateFunction { arguments, .. } => {
            if aggregate_state_codec(ch_type).is_none() {
                Some(ch_type.to_string())
            } else {
                arguments.iter().find_map(unsupported_header_type_name)
            }
        }
        _ => None,
    }
}

/// Whether Tuple element names satisfy the server's construction rules.
///
/// An unnamed tuple is legal, as is a fully named tuple. Mixed named/unnamed
/// elements, an empty or exact-lowercase `null` name, and duplicates are
/// rejected by `DataTypeTuple::checkTupleNames` at v26.6.1.1193-stable.
pub(crate) fn is_valid_tuple_element_names(elements: &[(Option<String>, ChType)]) -> bool {
    let named = elements.iter().filter(|(name, _)| name.is_some()).count();
    if named != 0 && named != elements.len() {
        return false;
    }
    if elements
        .iter()
        .any(|(name, _)| matches!(name.as_deref(), Some("") | Some("null")))
    {
        return false;
    }

    // O(n^2) over names. Tuples are small, and this runs once per declared
    // type, never per row.
    !elements.iter().enumerate().any(|(i, (name, _))| {
        name.is_some() && elements[..i].iter().any(|(other, _)| other == name)
    })
}

/// Parse the element list of a `Nested(...)` type string into `(name, type)`
/// pairs, preserving declaration order.
///
/// Names are mandatory, so an unnamed element (`Nested(UInt32)`) or an empty
/// list (`Nested()`) returns `None` (-> `UnsupportedType`); the server rejects
/// both at parse time. The list is split on top-level commas with the same
/// paren/quote-aware splitter the Tuple arm uses, and each element is parsed by
/// [`parse_tuple_element`] at `depth + 1`. The parser itself accepts an empty
/// name, the reserved lowercase `null`, or duplicate names as written (they
/// round-trip through `Display`), but `unsupported_header_type_name` enforces
/// `checkTupleNames` uniformly, so both decode header validation and encode
/// reject those shapes (via the Tuple delegation).
fn parse_nested_elements(inner: &str, depth: usize) -> Option<Vec<(String, ChType)>> {
    let trimmed = inner.trim_matches(' ');
    if trimmed.is_empty() {
        return None;
    }
    let parts = split_top_level_commas(trimmed)?;
    let mut fields = Vec::with_capacity(parts.len());
    for part in parts {
        let (name, ch_type) = parse_tuple_element(part.trim_matches(' '), depth)?;
        // Nested elements must be named; an unnamed one is a parse error.
        let name = name?;
        fields.push((name, ch_type));
    }
    Some(fields)
}

/// Whether `func` is a valid `SimpleAggregateFunction` function-name spelling:
/// an ASCII identifier (`[A-Za-z_][A-Za-z0-9_]*`) optionally followed by a
/// balanced parenthesized parameter list running to the end of the token
/// (e.g. `sum`, `anyLast`, `groupArrayLastArray(5)`).
///
/// The parameter contents are NOT semantically validated (the server renders
/// them via `FieldVisitorToString`, mostly numbers, and this crate does not
/// re-derive that grammar); only identifier shape and paren balance are checked
/// so a malformed header surfaces as `UnsupportedType` rather than a wrong
/// decode. The whitelist of allowed function names is deliberately not enforced.
///
/// Shared with `native::encode`, which runs the same check on a
/// caller-constructed `SimpleAggregateFunction` before rendering its header, so
/// a malformed `func` cannot inject extra type-string tokens on the encode side.
pub(crate) fn is_simple_aggregate_func_spelling(func: &str) -> bool {
    let bytes = func.as_bytes();
    // Identifier prefix.
    match bytes.first() {
        Some(b) if b.is_ascii_alphabetic() || *b == b'_' => {}
        _ => return false,
    }
    let mut i = 1usize;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    // Bare identifier: no parameter list.
    if i == bytes.len() {
        return true;
    }
    // Otherwise the remainder must be a balanced `(...)` running to the end. The
    // index math is in-bounds: `i < bytes.len()` here (the bare-identifier return
    // above handled `i == bytes.len()`), so `bytes.len() - 1` is a valid index.
    if bytes[i] != b'(' || bytes[bytes.len() - 1] != b')' {
        return false;
    }
    // `depth` counts open parens with an unsigned counter. It cannot overflow: at
    // most one increment happens per byte, so it stays bounded by the token
    // length, a valid `usize`. Decrementing only after a nonzero check makes the
    // walk obviously total, so a stray closing paren returns false rather than
    // wrapping below zero.
    let mut depth = 0usize;
    for &b in &bytes[i..] {
        match b {
            b'(' => depth += 1,
            b')' => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    depth == 0
}

/// Derive a `Decimal`'s on-wire byte width (as a bit count) from its precision.
///
/// ClickHouse backs a `Decimal(P, S)` with a fixed-width signed integer chosen
/// by P (server `DataTypesDecimal` / `createDecimal`, confirmed at
/// v26.6.1.1193-stable):
///
/// - P in  1..=9  -> Int32  (32 bits, 4 bytes/row)
/// - P in 10..=18 -> Int64  (64 bits, 8 bytes/row)
/// - P in 19..=38 -> Int128 (128 bits, 16 bytes/row)
/// - P in 39..=76 -> Int256 (256 bits, 32 bytes/row)
///
/// P must be in 1..=76; a precision of 0 or above 76 has no backing integer and
/// returns `None` (the caller maps it to `UnsupportedType`).
pub(crate) fn decimal_bits_from_precision(precision: u8) -> Option<u16> {
    match precision {
        1..=9 => Some(32),
        10..=18 => Some(64),
        19..=38 => Some(128),
        39..=76 => Some(256),
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

/// Parse the inner `'name' = value, ...` list of an `Enum8`/`Enum16` type
/// string into `(name, value)` pairs, preserving the server's emitted order
/// (ascending by value).
///
/// The list cannot be split on `,` or `=`: a quoted name passes those bytes
/// through unescaped (server `writeQuotedString`, confirmed at
/// v26.6.1.1193-stable). So this walks the string byte by byte: skip spaces,
/// require `'`, read the name until the closing unescaped `'` unescaping the
/// server's set (`\\`, `\'`, `\b`, `\f`, `\n`, `\r`, `\t`, `\0`), skip spaces,
/// require `=`, parse a signed integer that must fit `T` (`i8` for `Enum8`,
/// `i16` for `Enum16`), then skip spaces and require `,` or end of input.
///
/// Returns `None` (-> UnsupportedType) on any malformed escape, out-of-range
/// value, or syntax error. The type string is untrusted wire input, so this
/// never panics. An empty list (`Enum8()`) returns an empty `Vec`; the server
/// does not emit it, but it is harmless and not a decode error. Duplicate names or
/// values are accepted as written rather than rejected: the variants are metadata
/// carried on the `ChType` only (the wire payload is the raw underlying int), so a
/// duplicate cannot corrupt a decoded column, and a server that emits one is
/// preserved for faithful round-tripping through `Display`.
fn parse_enum_variants<T: TryFrom<i64>>(inner: &str) -> Option<Vec<(String, T)>> {
    let bytes = inner.as_bytes();
    let mut pos = 0usize;
    let mut variants: Vec<(String, T)> = Vec::new();

    skip_ascii_spaces(bytes, &mut pos);
    if pos >= bytes.len() {
        // Empty inner list: `Enum8()`. No variants.
        return Some(variants);
    }

    loop {
        skip_ascii_spaces(bytes, &mut pos);

        // Name: a single-quoted, escaped string.
        if bytes.get(pos) != Some(&b'\'') {
            return None;
        }
        pos += 1;
        let name = parse_enum_name(bytes, &mut pos)?;

        // ` = ` separator (spaces optional, the server writes exactly one each
        // side; accept any run of spaces to stay lenient on the untrusted input).
        skip_ascii_spaces(bytes, &mut pos);
        if bytes.get(pos) != Some(&b'=') {
            return None;
        }
        pos += 1;
        skip_ascii_spaces(bytes, &mut pos);

        // Signed integer value. Parse in i64 first, then narrow to T so an
        // out-of-range value for the concrete enum width is rejected.
        let value = parse_enum_value(bytes, &mut pos)?;
        let value = T::try_from(value).ok()?;
        variants.push((name, value));

        // Separator or end of input.
        skip_ascii_spaces(bytes, &mut pos);
        match bytes.get(pos) {
            None => return Some(variants),
            Some(&b',') => pos += 1,
            Some(_) => return None,
        }
    }
}

/// Advance `pos` past any run of ASCII space (0x20) bytes.
fn skip_ascii_spaces(bytes: &[u8], pos: &mut usize) {
    while bytes.get(*pos) == Some(&b' ') {
        *pos += 1;
    }
}

/// Read an enum variant name from `bytes` starting just after the opening `'`,
/// advancing `pos` past the closing `'`. Applies the server's unescape set; any
/// unknown escape or a missing closing quote returns `None`.
fn parse_enum_name(bytes: &[u8], pos: &mut usize) -> Option<String> {
    let mut name = Vec::new();
    loop {
        let b = *bytes.get(*pos)?;
        *pos += 1;
        match b {
            b'\'' => {
                // Closing quote. The name bytes are UTF-8 (the whole type string
                // came from a `String` validated as UTF-8 in the header), so
                // this conversion succeeds for any well-formed input.
                return String::from_utf8(name).ok();
            }
            b'\\' => {
                let esc = *bytes.get(*pos)?;
                *pos += 1;
                let decoded = match esc {
                    b'\\' => b'\\',
                    b'\'' => b'\'',
                    b'b' => 0x08,
                    b'f' => 0x0C,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'0' => 0x00,
                    _ => return None, // unknown escape
                };
                name.push(decoded);
            }
            other => name.push(other),
        }
    }
}

/// Parse a signed decimal integer (optional leading `-`) from `bytes`,
/// advancing `pos` past the digits. Returns `None` on no digits or overflow.
fn parse_enum_value(bytes: &[u8], pos: &mut usize) -> Option<i64> {
    let start = *pos;
    if bytes.get(*pos) == Some(&b'-') {
        *pos += 1;
    }
    let digits_start = *pos;
    while matches!(bytes.get(*pos), Some(b'0'..=b'9')) {
        *pos += 1;
    }
    if *pos == digits_start {
        return None; // no digits
    }
    // The slice is `[-]?[0-9]+`, all ASCII, so it is valid UTF-8 and parses as
    // i64 unless it overflows, which `parse` reports as an error.
    std::str::from_utf8(&bytes[start..*pos]).ok()?.parse().ok()
}

/// Parse the inner element list of a `Tuple(...)` type string into
/// `(optional name, element type)` pairs, preserving declaration order.
///
/// An empty (or all-spaces) list is `Tuple()`. Otherwise the list is split on
/// top-level commas ([`split_top_level_commas`]) and each element is parsed by
/// [`parse_tuple_element`] at `depth + 1`. Any malformed element (unterminated
/// quoting, a name without a type, an unknown element type) returns `None`
/// (-> `UnsupportedType`); the type string is untrusted wire input, so this
/// never panics. The server rejects empty names, the literal name `null`, and
/// duplicate names at creation time, so an honest header never carries them;
/// this parser accepts them as written rather than second-guessing (they are
/// metadata only and round-trip through `Display`).
fn parse_tuple_elements(inner: &str, depth: usize) -> Option<Vec<(Option<String>, ChType)>> {
    let trimmed = inner.trim_matches(' ');
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    let parts = split_top_level_commas(trimmed)?;
    let mut elements = Vec::with_capacity(parts.len());
    for part in parts {
        elements.push(parse_tuple_element(part.trim_matches(' '), depth)?);
    }
    Some(elements)
}

/// Split a tuple element list on the commas at nesting depth 0, skipping over
/// parenthesized groups, single-quoted strings (an Enum element's variant
/// names, a DateTime timezone), and backtick-quoted element names. Inside
/// either quote form a backslash consumes the next byte (the server's lexer
/// never lets `\` close a quote), and inside backticks a doubled backtick is
/// the escaped-backtick form, not a close-then-reopen. Returns `None` on
/// unbalanced parentheses or an unterminated quote, which only a malformed
/// header can produce.
fn split_top_level_commas(s: &str) -> Option<Vec<&str>> {
    let bytes = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                i += 1;
            }
            b',' if depth == 0 => {
                parts.push(&s[start..i]);
                i += 1;
                start = i;
            }
            quote @ (b'\'' | b'`') => {
                i += 1;
                loop {
                    let b = *bytes.get(i)?; // unterminated quote -> malformed
                    i += 1;
                    if b == b'\\' {
                        // A backslash escape consumes the next byte, so an
                        // escaped quote cannot close the string. A trailing
                        // lone backslash is unterminated input.
                        if i >= bytes.len() {
                            return None;
                        }
                        i += 1;
                    } else if b == quote {
                        if quote == b'`' && bytes.get(i) == Some(&b'`') {
                            i += 1; // doubled backtick: escaped, stay inside
                        } else {
                            break;
                        }
                    }
                }
            }
            _ => i += 1,
        }
    }
    // All split points are ASCII delimiters, so every slice boundary is a char
    // boundary.
    parts.push(&s[start..]);
    Some(parts)
}

/// Parse one tuple element, `part` already trimmed: either a bare type
/// (`Int32`), a bare-identifier name then the type (`a Int32`), or a
/// backtick-quoted name then the type (`` `a b` Int32 ``).
///
/// The bare named form is unambiguous because no ClickHouse type name contains
/// a space outside parentheses or quotes: if the text before the first space is
/// a valid bare identifier (the same [`crate::schema::is_bare_identifier`]
/// predicate `Display` quotes by, so parse and render agree), the element is
/// named; otherwise the whole text is parsed as an unnamed type (e.g.
/// `Decimal(9, 4)`, whose space sits inside its parentheses).
fn parse_tuple_element(part: &str, depth: usize) -> Option<(Option<String>, ChType)> {
    let bytes = part.as_bytes();
    if bytes.first() == Some(&b'`') {
        let mut pos = 1usize;
        let name = parse_back_quoted_name(bytes, &mut pos)?;
        // `pos` sits just past the closing backtick, an ASCII boundary. The
        // rest, after the separating spaces, must be a parseable element type.
        let rest = part[pos..].trim_matches(' ');
        if rest.is_empty() {
            return None; // a name with no type
        }
        let ch_type = parse_ch_type_depth(rest, depth + 1)?;
        return Some((Some(name), ch_type));
    }
    if let Some((first, rest)) = part.split_once(' ') {
        if crate::schema::is_bare_identifier(first) {
            let rest = rest.trim_matches(' ');
            if rest.is_empty() {
                return None; // a name with no type
            }
            let ch_type = parse_ch_type_depth(rest, depth + 1)?;
            return Some((Some(first.to_string()), ch_type));
        }
    }
    let ch_type = parse_ch_type_depth(part, depth + 1)?;
    Some((None, ch_type))
}

/// Read a backtick-quoted tuple element name from `bytes` starting just after
/// the opening backtick, advancing `pos` past the closing backtick.
///
/// The server WRITES only `writeBackQuotedString`'s escapes (`\``, `\\`, and
/// the C0 letter escapes; see `escape_back_quoted` in `crate::schema`), but its
/// own READERS are more permissive, and this parser mirrors the reader side
/// (confirmed at v26.6.1.1193-stable: `Lexer.cpp` `quotedString<'`'>`,
/// `ReadHelpers.cpp` `readBackQuotedStringWithSQLStyle`, `ReadHelpers.h`
/// `parseEscapeSequence`): a doubled backtick is one literal backtick, and a
/// backslash escape accepts `\xAA` (the hex byte), `\N` (empty, nothing
/// appended), the control escapes `\a \b \e \f \n \r \t \v \0`, and ANY other
/// `\c` as the literal `c` with the backslash dropped. A lone backslash always
/// consumes the following byte, so an escaped backtick never closes the name.
/// Unterminated or truncated input returns `None` (-> `UnsupportedType`),
/// never a panic. The decoded bytes must be valid UTF-8 (a `\xAA` escape can
/// break that), or the name is rejected.
fn parse_back_quoted_name(bytes: &[u8], pos: &mut usize) -> Option<String> {
    let mut name = Vec::new();
    loop {
        let b = *bytes.get(*pos)?;
        *pos += 1;
        match b {
            b'`' => {
                if bytes.get(*pos) == Some(&b'`') {
                    // Doubled backtick: the SQL-style escaped form.
                    *pos += 1;
                    name.push(b'`');
                } else {
                    return String::from_utf8(name).ok();
                }
            }
            b'\\' => {
                let esc = *bytes.get(*pos)?;
                *pos += 1;
                match esc {
                    b'x' => {
                        // \xAA: exactly two hex digits for one raw byte.
                        let hi = hex_digit(*bytes.get(*pos)?)?;
                        let lo = hex_digit(*bytes.get(*pos + 1)?)?;
                        *pos += 2;
                        name.push((hi << 4) | lo);
                    }
                    b'N' => {} // \N is the empty escape; nothing appended
                    b'a' => name.push(0x07),
                    b'b' => name.push(0x08),
                    b'e' => name.push(0x1B),
                    b'f' => name.push(0x0C),
                    b'n' => name.push(b'\n'),
                    b'r' => name.push(b'\r'),
                    b't' => name.push(b'\t'),
                    b'v' => name.push(0x0B),
                    b'0' => name.push(0x00),
                    // Any other escaped byte (including ` and \) is itself.
                    other => name.push(other),
                }
            }
            other => name.push(other),
        }
    }
}

/// The value of one ASCII hex digit, or `None` for a non-hex byte.
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Whether `dict_value_type` (the removeNullable inner of a `LowCardinality`) is
/// an inner type this crate decodes and ClickHouse permits inside
/// `LowCardinality`.
///
/// ClickHouse gates LowCardinality inners on
/// `IDataType::canBeInsideLowCardinality()`, checked in the
/// `DataTypeLowCardinality` constructor after `removeNullable` (confirmed at
/// v26.6.1.1193-stable). That predicate is true for `String`, `FixedString`, the
/// fixed-width numerics, and the number-backed temporals `Date`/`Date32`/
/// `DateTime`/`Time`, and every `Interval*` (`Bool` is a `UInt8`-backed number
/// and also qualifies). It is
/// false for `DateTime64`, `Time64`, and every `Decimal`, which are
/// `DataTypeDecimalBase` subclasses, so those are rejected here even though the
/// crate decodes them as ordinary columns. `UUID`/`IPv4`/`IPv6` are permitted by
/// the server and decoded by this crate, so they are in the allowlist: the
/// dictionary body is the inner type's plain bulk form (4 raw bytes per entry for
/// `IPv4`, 16 raw bytes per entry for `UUID`/`IPv6`), decoded through the shared
/// per-type body decoder.
///
/// The fixed-width numeric, temporal, and Interval inners require the server's
/// `allow_suspicious_low_cardinality_types=1` for persisted schema declarations
/// and explicit `CAST` targets. That is a server-side type-use guard only and
/// has no effect on the wire bytes or on decoding a column the server already
/// produced. `UUID` (like `String` and `FixedString`) is allowed
/// unconditionally; `IPv4`/`IPv6` need the suspicious setting at those same
/// boundaries, again with no wire effect.
pub(crate) fn is_low_cardinality_inner(dict_value_type: &ChType) -> bool {
    // A name-decoration alias is legal inside `LowCardinality` exactly when the
    // physical type it delegates to is, so `is_low_cardinality_inner(SAF(T))` ==
    // `is_low_cardinality_inner(T)`. Confirmed live at v26.6.1.1193-stable:
    // `LowCardinality(SimpleAggregateFunction(anyLast, String))` is a legal
    // header. A geo/`Nested` alias resolves to a `Tuple`/`Array`, which is not in
    // the allowlist, so those stay rejected.
    if let Some(under) = dict_value_type.physical_delegate() {
        return is_low_cardinality_inner(&under);
    }
    matches!(
        dict_value_type,
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
            // The wide integers are `DataTypeNumberBase` subclasses whose
            // `canBeInsideLowCardinality()` is final-true, so
            // `LowCardinality(Int128)` etc. are legal on the wire (the server
            // ships tests `02125_low_cardinality_int256` and
            // `02459_low_cardinality_uint128_aggregator`). Unlike `Decimal`/
            // `Enum` (forbidden inners), they belong in this allowlist. Their
            // dictionary body is the plain fixed-width run (16/32 bytes per
            // entry), decoded through the shared per-type body decoder. The
            // `allow_suspicious_low_cardinality_types` setting gates CREATE only,
            // not the wire.
            | ChType::Int128
            | ChType::UInt128
            | ChType::Int256
            | ChType::UInt256
            | ChType::String
            | ChType::FixedString(_)
            | ChType::Date
            | ChType::Date32
            | ChType::DateTime { .. }
            | ChType::Time
            | ChType::Interval(_)
            | ChType::Uuid
            | ChType::Ipv4
            | ChType::Ipv6
    )
}

/// Resolve a `LowCardinality` inner type to `(nullable, dict_value_type)`, the
/// single source of truth every `LowCardinality` site consults so they cannot
/// disagree on nullability or the dictionary value type.
///
/// `inner` is the type spelled inside `LowCardinality(...)`. On the wire it may
/// be wrapped in any number of `SimpleAggregateFunction` name decorations
/// (the only alias legal inside `LowCardinality`) around at most one `Nullable`,
/// with further `SimpleAggregateFunction` decorations under that `Nullable`.
/// ClickHouse always nests `Nullable` inside `LowCardinality`, never the reverse,
/// so at most one `Nullable` is reachable. This strips the full outer SAF chain,
/// unwraps an optional `Nullable`, then strips any further SAF chain beneath it,
/// returning whether the dictionary is nullable together with the physical value
/// type its per-block dictionary body is serialized as.
///
/// Confirmed live at v26.6.1.1193-stable:
/// `LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String)))` is a real
/// server header (hexdump-verified), and a chained SAF such as
/// `SimpleAggregateFunction(anyLast, SimpleAggregateFunction(sum, UInt64))` is
/// constructible, so a single-level see-through is not enough. `SimpleAggregateFunction`
/// is a pure name decoration whose inner is a `Box<ChType>` we can borrow, so this
/// returns a borrow rather than an owned type; the geo/`Nested` aliases are never
/// legal here and resolve (through `is_low_cardinality_inner`) to a rejected
/// `Tuple`/`Array` anyway.
///
/// Public so a binding crate resolves a `LowCardinality` inner the same way,
/// rather than duplicating the SAF/`Nullable` stripping and drifting from it.
pub fn low_cardinality_dict_value_type(inner: &ChType) -> (bool, &ChType) {
    fn strip_saf(mut t: &ChType) -> &ChType {
        while let ChType::SimpleAggregateFunction { inner, .. } = t {
            t = inner.as_ref();
        }
        t
    }
    match strip_saf(inner) {
        ChType::Nullable(t) => (true, strip_saf(t.as_ref())),
        other => (false, other),
    }
}

/// Whether `key` is a legal `Map(K, V)` key type, the server's
/// `DataTypeMap::isValidKeyType` (`!isNullableOrLowCardinalityNullable`,
/// confirmed at v26.6.1.1193-stable): `Nullable(K)` and
/// `LowCardinality(Nullable(K))` keys are forbidden; a plain
/// `LowCardinality(K)` key is legal. `pub(crate)` so the encoder's validation
/// enforces the same constraint on caller-constructed types.
///
/// A name-decoration alias key resolves through [`ChType::physical_delegate`],
/// so a `SimpleAggregateFunction(sum, UInt64)` key is legal (it delegates to a
/// plain `UInt64`) while a `SimpleAggregateFunction(anyLast, Nullable(String))`
/// key is not (it delegates to `Nullable(String)`).
pub(crate) fn is_valid_map_key_type(key: &ChType) -> bool {
    if let Some(under) = key.physical_delegate() {
        return is_valid_map_key_type(&under);
    }
    match key {
        ChType::Nullable(_) => false,
        ChType::LowCardinality(inner) => !matches!(inner.as_ref(), ChType::Nullable(_)),
        _ => true,
    }
}
