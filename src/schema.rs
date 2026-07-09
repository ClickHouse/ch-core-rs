/// ClickHouse logical type system.
///
/// Preserves ClickHouse semantics (timezone, precision, enum labels, etc.)
/// rather than mapping to Arrow or Python types at this layer.
#[derive(Debug, Clone, PartialEq)]
pub enum ChType {
    // Fixed-width numerics
    Bool,
    Int8,
    Int16,
    Int32,
    Int64,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    Float32,
    Float64,

    // Wide integers. Each is a raw contiguous little-endian two's-complement
    // (signed) or unsigned fixed-width integer on the wire, 16 bytes for the
    // 128-bit pair and 32 bytes for the 256-bit pair, the same host-agnostic
    // passthrough as `Decimal` minus the precision/scale metadata. The core has
    // no native `i128`/`i256` and needs none: the bytes are stored verbatim and
    // signedness lives only in the type name (the four variants), which the
    // binding reads to recover the host value. `SerializationNumber<T>` on the
    // wire, byte-identical to a `Decimal128`/`Decimal256` integer body.
    Int128,
    UInt128,
    Int256,
    UInt256,

    // Strings
    String,
    FixedString(usize),

    // Temporal (Phase 2)
    Date,
    Date32,
    DateTime {
        timezone: Option<String>,
    },
    DateTime64 {
        precision: u8,
        timezone: Option<String>,
    },

    // Decimal(P, S). The server always emits the canonical `Decimal(P, S)` form
    // on the wire (never `Decimal32(S)` etc.), so that is the only spelling
    // parsed. The wire payload is a raw little-endian two's-complement
    // fixed-width integer whose byte width is derived from the precision P:
    // P in 1..=9 -> 32 bits (Int32), 10..=18 -> 64, 19..=38 -> 128, 39..=76 ->
    // 256. `bits` is stored so the Column and the Arrow export can size the
    // contiguous buffer without re-deriving it. Precision and scale are type
    // metadata only and never appear in the per-row data.
    Decimal {
        precision: u8,
        scale: u8,
        bits: u16,
    },

    // Special (Phase 3-4)
    Uuid,
    Ipv4,
    Ipv6,
    // Enums carry only the name->value mapping in the logical type; the wire
    // payload is the raw underlying Int8/Int16, so the decoded Column stores
    // just the physical int buffer. The variant order is the server's emitted
    // order (ascending by value), preserved so Display round-trips.
    Enum8 {
        variants: Vec<(String, i8)>,
    },
    Enum16 {
        variants: Vec<(String, i16)>,
    },

    // Wrappers
    Nullable(Box<ChType>),
    LowCardinality(Box<ChType>),

    // Containers (Phase 4)
    // The first nested container: `Array(T)` recurses into an element type `T`,
    // which is itself any supported type, including `Nullable(T)`,
    // `LowCardinality(T)`, or a further `Array`. The array itself is never
    // `Nullable` (the server's `DataTypeArray::canBeInsideNullable()` is false),
    // so element-level nulls live in the element type, not in the array.
    Array(Box<ChType>),
    // `Tuple(T1, ...)` / `Tuple(name1 T1, ...)`: a fixed set of element types,
    // each with an optional explicit name. The server renders names
    // all-or-nothing (`DataTypeTuple::doGetName` emits either every name or
    // none), but the parser stores them per element so a header is preserved
    // exactly as received. `Tuple()` (zero elements) is constructible and
    // emittable. The tuple itself may be wrapped in `Nullable`
    // (`DataTypeTuple::canBeInsideNullable()` is true; the DDL gate
    // `enable_nullable_tuple_type` is a creation-time concern only), but never
    // in `LowCardinality`.
    Tuple(Vec<(Option<String>, ChType)>),
    // `Map(K, V)`: on the Native wire this is exactly the
    // `Array(Tuple(keys, values))` layout (offsets, then the flattened key run,
    // then the flattened value run); the nested "keys"/"values" names never
    // appear in the type string or as wire bytes. The key type must not be
    // `Nullable` or `LowCardinality(Nullable(...))`
    // (`DataTypeMap::isValidKeyType`); the value type is unrestricted. The map
    // itself is never inside `Nullable` or `LowCardinality`.
    Map(Box<ChType>, Box<ChType>),
}

/// A named, typed column descriptor.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub ch_type: ChType,
}

/// Schema describing the columns in a batch.
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    pub fields: Vec<Field>,
}

impl Schema {
    pub fn new(fields: Vec<Field>) -> Self {
        Self { fields }
    }

    pub fn num_fields(&self) -> usize {
        self.fields.len()
    }
}

/// Render the canonical ClickHouse type name, the same string `parse_ch_type`
/// accepts. `Display` is the contract bindings use to report column types, so
/// any value produced by the parser must round-trip through it. Values that
/// are constructible but not parser-producible (e.g. `FixedString(0)` or a
/// `DateTime64` precision above 9) still render, but `parse_ch_type` rejects
/// them by design.
impl std::fmt::Display for ChType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChType::Bool => write!(f, "Bool"),
            ChType::Int8 => write!(f, "Int8"),
            ChType::Int16 => write!(f, "Int16"),
            ChType::Int32 => write!(f, "Int32"),
            ChType::Int64 => write!(f, "Int64"),
            ChType::UInt8 => write!(f, "UInt8"),
            ChType::UInt16 => write!(f, "UInt16"),
            ChType::UInt32 => write!(f, "UInt32"),
            ChType::UInt64 => write!(f, "UInt64"),
            ChType::Float32 => write!(f, "Float32"),
            ChType::Float64 => write!(f, "Float64"),
            ChType::Int128 => write!(f, "Int128"),
            ChType::UInt128 => write!(f, "UInt128"),
            ChType::Int256 => write!(f, "Int256"),
            ChType::UInt256 => write!(f, "UInt256"),
            ChType::String => write!(f, "String"),
            ChType::FixedString(n) => write!(f, "FixedString({n})"),
            ChType::Uuid => write!(f, "UUID"),
            ChType::Ipv4 => write!(f, "IPv4"),
            ChType::Ipv6 => write!(f, "IPv6"),
            ChType::Date => write!(f, "Date"),
            ChType::Date32 => write!(f, "Date32"),
            ChType::DateTime { timezone: None } => write!(f, "DateTime"),
            ChType::DateTime { timezone: Some(tz) } => write!(f, "DateTime('{tz}')"),
            ChType::DateTime64 {
                precision,
                timezone: None,
            } => write!(f, "DateTime64({precision})"),
            ChType::DateTime64 {
                precision,
                timezone: Some(tz),
            } => write!(f, "DateTime64({precision}, '{tz}')"),
            // Render the canonical `Decimal(P, S)` the server emits, comma-space
            // separated, both fields always present, so it round-trips the wire
            // string. `bits` is derived from P and is not part of the name.
            ChType::Decimal {
                precision, scale, ..
            } => write!(f, "Decimal({precision}, {scale})"),
            ChType::Enum8 { variants } => write_enum(f, "Enum8", variants),
            ChType::Enum16 { variants } => write_enum(f, "Enum16", variants),
            ChType::Nullable(inner) => write!(f, "Nullable({inner})"),
            ChType::LowCardinality(inner) => write!(f, "LowCardinality({inner})"),
            ChType::Array(inner) => write!(f, "Array({inner})"),
            ChType::Tuple(elements) => write_tuple(f, elements),
            // The canonical server form (`DataTypeMap::doGetName`): the two
            // type arguments only, comma-space separated, no element names.
            ChType::Map(key, value) => write!(f, "Map({key}, {value})"),
        }
    }
}

/// Render a `Tuple(...)` type string: the elements joined by `, ` inside one
/// pair of parentheses, each element as `name type` when it carries a name and
/// as the bare type otherwise. `Tuple()` renders with empty parentheses.
///
/// Name rendering matches the server byte for byte:
/// `DataTypeTuple::doGetName` runs each name through `backQuoteIfNeed`
/// ([`back_quote_if_need`]), confirmed at v26.6.1.1193-stable
/// (`src/DataTypes/DataTypeTuple.cpp`, `src/Common/quoteString.cpp`).
fn write_tuple(
    f: &mut std::fmt::Formatter<'_>,
    elements: &[(Option<String>, ChType)],
) -> std::fmt::Result {
    write!(f, "Tuple(")?;
    for (i, (name, ch_type)) in elements.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        if let Some(name) = name {
            back_quote_if_need(f, name)?;
            write!(f, " ")?;
        }
        write!(f, "{ch_type}")?;
    }
    write!(f, ")")
}

/// Whether `name` renders unquoted in a type string, matching the server's
/// `backQuoteIfNeed` (`src/Common/quoteString.cpp` ->
/// `writeProbablyBackQuotedString` -> `isValidIdentifier`, confirmed at
/// v26.6.1.1193-stable in `src/Common/StringUtils.h` and
/// `src/Common/quoteString.cpp`): a name stays bare only if it is a valid
/// ASCII identifier (`[A-Za-z_][A-Za-z0-9_]*`), not any-case `null` (which
/// `isValidIdentifier` excludes before the keyword list, since a bare NULL
/// would read back as the NULL keyword), and not one of the case-insensitive
/// keywords the server always quotes. Shared with the tuple-element parser in
/// `native::decode` so parse and render agree on which names need backticks.
pub(crate) fn is_bare_identifier(name: &str) -> bool {
    let bytes = name.as_bytes();
    let valid_shape = match bytes.first() {
        Some(b) if b.is_ascii_alphabetic() || *b == b'_' => bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_'),
        _ => false,
    };
    if !valid_shape {
        return false;
    }
    // Any-case "null" is excluded by isValidIdentifier itself; the keyword set
    // below is what writeProbablyBackQuotedString additionally quotes even
    // though the shape is a valid identifier (case-insensitive).
    !name.eq_ignore_ascii_case("null")
        && !name.eq_ignore_ascii_case("distinct")
        && !name.eq_ignore_ascii_case("all")
        && !name.eq_ignore_ascii_case("table")
        && !name.eq_ignore_ascii_case("select")
        && !name.eq_ignore_ascii_case("from")
        && !name.eq_ignore_ascii_case("values")
}

/// Write a tuple element name, backtick-quoting it when [`is_bare_identifier`]
/// says the server would.
fn back_quote_if_need(f: &mut std::fmt::Formatter<'_>, name: &str) -> std::fmt::Result {
    if is_bare_identifier(name) {
        write!(f, "{name}")
    } else {
        write!(f, "`{}`", escape_back_quoted(name))
    }
}

/// Escape a name for emission inside backticks, matching the server's
/// `writeBackQuotedString` -> `writeAnyEscapedString<'`'>` (confirmed at
/// v26.6.1.1193-stable, `src/IO/WriteHelpers.h`): a backtick becomes `` \` ``
/// (the backslash form, never the doubled `` `` `` MySQL form), a backslash
/// becomes `\\`, the C0 control bytes the server names get their two-char
/// letter escapes, and every other byte (including `'`, `,`, and spaces)
/// passes through raw at the byte level. The parser accepts a superset (see
/// `parse_back_quoted_name` in `native::decode`), so `parse(display(x)) == x`
/// holds for any name.
fn escape_back_quoted(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '`' => out.push_str("\\`"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            other => out.push(other),
        }
    }
    out
}

/// Render an `Enum8`/`Enum16` type string: the keyword, then the `'name' = N`
/// pairs joined by `, `, inside one pair of parentheses. The variant order is
/// preserved as stored (the server emits ascending by value). Each name is
/// escaped with [`escape_enum_name`], the exact inverse of the parser's
/// unescape, so `parse(display(x)) == x` holds for any name the parser
/// accepted.
fn write_enum<V: std::fmt::Display>(
    f: &mut std::fmt::Formatter<'_>,
    keyword: &str,
    variants: &[(String, V)],
) -> std::fmt::Result {
    write!(f, "{keyword}(")?;
    for (i, (name, value)) in variants.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "'{}' = {value}", escape_enum_name(name))?;
    }
    write!(f, ")")
}

/// Escape an enum variant name for emission inside single quotes, the inverse of
/// the parser's unescape in `parse_ch_type`. This matches the server's
/// `writeQuotedString` with `escape_quote_with_quote=false` and
/// `escape_backslash_with_backslash=true`: a backslash and a single quote are
/// backslash-escaped, the C0 control bytes the server names get their letter
/// escapes, and every other byte passes through unchanged (notably `,` and `=`,
/// which is why the parser cannot split on them).
fn escape_enum_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            other => out.push(other),
        }
    }
    out
}

impl ChType {
    /// Whether this type is nullable (wrapped in Nullable).
    pub fn is_nullable(&self) -> bool {
        matches!(self, ChType::Nullable(_))
    }

    /// The inner type if Nullable, otherwise self.
    pub fn inner(&self) -> &ChType {
        match self {
            ChType::Nullable(inner) => inner,
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_construction() {
        let schema = Schema::new(vec![
            Field {
                name: "id".into(),
                ch_type: ChType::Int64,
            },
            Field {
                name: "name".into(),
                ch_type: ChType::Nullable(Box::new(ChType::String)),
            },
        ]);
        assert_eq!(schema.num_fields(), 2);
        assert_eq!(schema.fields[0].name, "id");
        assert!(!schema.fields[0].ch_type.is_nullable());
        assert!(schema.fields[1].ch_type.is_nullable());
    }

    #[test]
    fn test_tuple_display_matches_server_rendering() {
        // Unnamed elements render as the bare types.
        assert_eq!(
            ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]).to_string(),
            "Tuple(Int32, String)"
        );
        // Zero elements render with empty parentheses.
        assert_eq!(ChType::Tuple(vec![]).to_string(), "Tuple()");
        // Bare-identifier names stay unquoted.
        assert_eq!(
            ChType::Tuple(vec![
                (Some("a".to_string()), ChType::Int32),
                (Some("user_2".to_string()), ChType::String),
            ])
            .to_string(),
            "Tuple(a Int32, user_2 String)"
        );
        // A name with a space, a comma, a backtick, or a backslash is
        // backtick-quoted; the backtick escapes as \` (writeBackQuotedString,
        // never the doubled `` form) and the backslash as \\.
        assert_eq!(
            ChType::Tuple(vec![
                (Some("a b".to_string()), ChType::Int8),
                (Some("c,d".to_string()), ChType::Int8),
                (Some("e`f".to_string()), ChType::Int8),
                (Some("g\\h".to_string()), ChType::Int8),
            ])
            .to_string(),
            "Tuple(`a b` Int8, `c,d` Int8, `e\\`f` Int8, `g\\\\h` Int8)"
        );
        // The case-insensitive keywords backQuoteIfNeed always quotes.
        assert_eq!(
            ChType::Tuple(vec![
                (Some("select".to_string()), ChType::Int8),
                (Some("From".to_string()), ChType::Int8),
                (Some("selected".to_string()), ChType::Int8),
            ])
            .to_string(),
            "Tuple(`select` Int8, `From` Int8, selected Int8)"
        );
        // Any-case "null" is excluded by isValidIdentifier itself (a bare NULL
        // would read back as the NULL keyword), so it is always quoted; a name
        // merely containing it is not.
        assert_eq!(
            ChType::Tuple(vec![
                (Some("NULL".to_string()), ChType::Int8),
                (Some("Null".to_string()), ChType::Int8),
                (Some("nullable".to_string()), ChType::Int8),
            ])
            .to_string(),
            "Tuple(`NULL` Int8, `Null` Int8, nullable Int8)"
        );
        // Control bytes get their two-char letter escapes.
        assert_eq!(
            ChType::Tuple(vec![(Some("a\tb\n".to_string()), ChType::Int8)]).to_string(),
            "Tuple(`a\\tb\\n` Int8)"
        );
    }

    #[test]
    fn test_decimal_display_is_canonical() {
        // Display emits the canonical `Decimal(P, S)` the server writes on the
        // wire (comma-space, both fields present), not the bit width, so it
        // round-trips the type string. bits is metadata only.
        assert_eq!(
            ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            }
            .to_string(),
            "Decimal(9, 4)"
        );
        assert_eq!(
            ChType::Decimal {
                precision: 50,
                scale: 0,
                bits: 256,
            }
            .to_string(),
            "Decimal(50, 0)"
        );
    }
}
