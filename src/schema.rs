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

    // Decimal (Phase 2)
    // Decimal { precision: u8, scale: u8, bits: u16 },

    // Special (Phase 3-4)
    Uuid,
    Ipv4,
    Ipv6,
    // Enum8 { variants: Vec<(String, i8)> },
    // Enum16 { variants: Vec<(String, i16)> },

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
            ChType::Nullable(inner) => write!(f, "Nullable({inner})"),
            ChType::LowCardinality(inner) => write!(f, "LowCardinality({inner})"),
        }
    }
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
}
