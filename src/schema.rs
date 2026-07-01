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

    // Extended numerics (Phase 6)
    // Int128, UInt128, Int256, UInt256,

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
    // Array(Box<ChType>),
    // Tuple(Vec<(Option<String>, ChType)>),
    // Map(Box<ChType>, Box<ChType>),
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
        }
    }
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
