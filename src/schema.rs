use std::borrow::Cow;
use std::sync::LazyLock;

/// Largest QBit dimension constructible by the pinned ClickHouse server.
///
/// QBit stores each plane as `FixedString(ceil(N / 8))`, and FixedString caps
/// its width at `0x00ff_ffff` bytes.
pub const QBIT_MAX_DIMENSION: usize = 0x00ff_ffff * 8;

/// Element type accepted by ClickHouse `QBit(T, N)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QBitElementType {
    BFloat16,
    Float32,
    Float64,
}

impl QBitElementType {
    /// Number of bit planes in the Native representation.
    pub const fn bit_width(self) -> usize {
        match self {
            Self::BFloat16 => 16,
            Self::Float32 => 32,
            Self::Float64 => 64,
        }
    }

    /// Byte width of one materialized vector element.
    pub const fn byte_width(self) -> usize {
        self.bit_width() / 8
    }

    /// The ordinary ClickHouse scalar type represented by one vector element.
    pub const fn ch_type(self) -> ChType {
        match self {
            Self::BFloat16 => ChType::BFloat16,
            Self::Float32 => ChType::Float32,
            Self::Float64 => ChType::Float64,
        }
    }
}

impl std::fmt::Display for QBitElementType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BFloat16 => f.write_str("BFloat16"),
            Self::Float32 => f.write_str("Float32"),
            Self::Float64 => f.write_str("Float64"),
        }
    }
}

/// ClickHouse logical type system.
///
/// Preserves ClickHouse semantics (timezone, precision, enum labels, etc.)
/// rather than mapping to Arrow or Python types at this layer.
#[derive(Debug, Clone, PartialEq)]
pub enum ChType {
    // ClickHouse Nothing has no value buffer, but its Native bulk
    // serialization still carries one ignored placeholder byte per row.
    Nothing,

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
    // BFloat16 stores the top 16 bits of an IEEE-754 Float32. The core keeps
    // each little-endian 2-byte word verbatim rather than adding a host
    // BFloat16 dependency or converting per value; bindings interpret the bits
    // using this logical tag.
    BFloat16,

    // `QBit(T, N)` is a fixed-size vector of N floating-point values. T is
    // restricted by the server to BFloat16, Float32, or Float64 and is kept in
    // the compact enum below so invalid element types are not constructible.
    // Native stores the vector as bit-transposed planes, but the decoded column
    // materializes one row-major child buffer for Arrow FixedSizeList export.
    QBit {
        element_type: QBitElementType,
        dimension: usize,
    },

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
    // Time is signed seconds without a date or timezone. Time64 carries signed
    // fractional-second ticks at the declared precision, also without a date or
    // timezone. Both are plain primitive buffers; the logical distinction lives
    // in these tags.
    Time,
    Time64 {
        precision: u8,
    },
    // ClickHouse's 11 Interval* logical types all store one signed Int64 count
    // of the declared unit. The unit lives only in the logical type tag; the
    // Native body is the same contiguous i64 buffer for every kind.
    Interval(IntervalKind),

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
    // `Variant(T1, ...)`: ordinary Variant alternatives are stored in the
    // server's canonical order, lexicographically by each type's full canonical
    // name. Geometry is the deliberate exception: its custom type uses the fixed
    // discriminator order in [`GEOMETRY_ALTERNATIVES`]. The Native body carries
    // one UInt8 global discriminator per row (`0..=254`), with 255 reserved for
    // Variant's intrinsic NULL, followed by one dense body per alternative. A
    // Variant therefore must contain 1..=255 distinct, normalized alternatives.
    // Direct Nothing alternatives are dropped by the server;
    // direct Nullable, LowCardinality(Nullable), Variant, and Dynamic alternatives
    // are forbidden. Variant itself cannot sit in Nullable or LowCardinality,
    // but it composes inside Array/Tuple and as either Map key or value.
    Variant(Vec<ChType>),

    // `Dynamic` is a self-describing Variant whose concrete alternatives are
    // carried in each non-empty Native column prefix. `max_types` is the number
    // of ordinary typed alternatives the server may keep before routing new
    // runtime types through its binary `SharedVariant` child. The valid range is
    // 0..=254; 32 is the default and renders as the bare `Dynamic` spelling.
    // Unlike Variant, the runtime alternatives are column data rather than
    // logical schema, so they live on `DynamicColumn`, not here.
    Dynamic {
        max_types: u8,
    },

    // Name-decoration aliases over existing machinery. Each of these three is a
    // custom `getName()` attached to an underlying type instance whose
    // serialization slot stays null (confirmed at v26.6.1.1193-stable), so the
    // wire bytes, state prefix, and Arrow shape are byte-identical to the
    // underlying type. Decode, encode, scan, and Arrow export all delegate to
    // [`ChType::physical_delegate`]; no new `Column` variant is needed because
    // the decoded buffer IS the underlying type's buffer.

    // `SimpleAggregateFunction(func, T)`: the runtime object is the inner `T`
    // instance with a custom name (`DataTypeCustomSimpleAggregateFunction`), so
    // everything physical delegates to `inner`. `func` stores the rendered
    // function spelling verbatim, INCLUDING any parenthesized literal params
    // (e.g. "groupArrayLastArray(5)"), so `Display` round-trips exactly. The
    // server whitelists a fixed set of functions (any, any_respect_nulls,
    // anyLast, anyLast_respect_nulls, min, max, sum, sumWithOverflow,
    // groupBitAnd, groupBitOr, groupBitXor, sumMap, minMap, maxMap,
    // groupArrayArray, groupArrayLastArray, groupUniqArrayArray,
    // groupUniqArrayArrayMap, sumMappedArrays, minMappedArrays, maxMappedArrays)
    // but the decoder does NOT enforce it: a server-authored header is trusted
    // and the list grows across versions. The parser accepts the spelling at any
    // nesting depth, so `Nullable`, `LowCardinality`, `Tuple`, `Array`, and `Map`
    // over a `SimpleAggregateFunction` all parse and delegate through `inner`
    // (the wrapped forms are observed live headers, e.g. an AggregatingMergeTree
    // `LowCardinality(SimpleAggregateFunction(anyLast, String))` column).
    SimpleAggregateFunction {
        func: String,
        inner: Box<ChType>,
    },

    // `AggregateFunction` is a real opaque aggregation state, not the
    // name-decoration alias above. Native carries no generic row or column
    // lengths: the concrete aggregate function owns the state serializer. The
    // logical type therefore preserves the function spelling (including literal
    // parameters) and argument types. Decode/encode accept only signatures with
    // an explicitly registered state-boundary codec.
    //
    // No state version is stored. The server omits version 0 from canonical type
    // names and emits no other version at the pin, so the parser rejects an
    // explicit leading integer: a spelling like `AggregateFunction(2, sum,
    // UInt64)` treats `2` as an unknown function name and surfaces as
    // `UnsupportedType`. Versioning is reintroduced with the first confirmed
    // versioned codec, and may key off the negotiated protocol revision rather
    // than the type string.
    AggregateFunction {
        function: String,
        arguments: Vec<ChType>,
    },

    // Geo aliases (`DataTypeCustomGeo`): `Point` = `Tuple(Float64, Float64)`
    // (unnamed elements), `Ring`/`LineString` = `Array(Point)`,
    // `Polygon`/`MultiLineString` = `Array(Array(Point))`, `MultiPolygon` =
    // `Array(Array(Array(Point)))`, and `MultiPoint` = `Array(Point)`. The
    // Native header carries the bare alias
    // spelling, never the expanded form, and the mapping is one-directional: a
    // structural `Array(Tuple(Float64, Float64))` header stays spelled that way
    // and decodes as a plain Array/Tuple. `Nullable(Point)` is legal (Tuple is
    // nullable-able); `Nullable` of the six Array-based kinds and
    // `LowCardinality` of all seven are illegal.
    Geo(GeoKind),

    // `Geometry` (`DataTypeCustomGeo`) is a custom fixed name over the
    // canonical `Variant(LineString, MultiLineString, MultiPolygon, Point,
    // Polygon, Ring, MultiPoint)`. It has exactly the underlying Variant's
    // BASIC Native body and Arrow Dense Union buffers, including discriminator
    // 255 for its intrinsic NULL. The distinct logical tag preserves the
    // `Geometry` header for round-trip encode while [`ChType::physical_delegate`]
    // keeps every physical path on the existing Variant implementation.
    // `Nullable` and `LowCardinality` are illegal because the delegate is a
    // Variant.
    Geometry,

    // `Nested(name1 T1, ...)` (`DataTypeNested`): with `flatten_nested = 0` the
    // header carries the literal `Nested(a T, b U)` spelling and the body is
    // byte-identical to `Array(Tuple(named elements))`. The runtime object is a
    // `DataTypeArray` over a `DataTypeTuple` with a custom name; there is no
    // `SerializationNested`. Element names are mandatory and follow the same
    // `checkTupleNames` rules as a named `Tuple`. `Nullable(Nested)` and
    // `LowCardinality(Nested)` are both illegal (it is an Array).
    Nested(Vec<(String, ChType)>),

    // `JSON` (the new `DataTypeObject`, confirmed at v26.6.1.1193-stable in
    // `src/DataTypes/DataTypeObject.{h,cpp}` and `registerDataTypeJSON`). Legacy
    // `Object('json')` is fully unregistered at this tag and is NOT parsed. The
    // canonical `doGetName` form is the bare word `JSON` with parenthesized
    // parameters omitted entirely when empty, otherwise `JSON(...)` joining, in
    // order: `max_dynamic_types=M` (only if != 32), `max_dynamic_paths=N` (only
    // if != 1024), the typed paths sorted lexicographically (each `<path>
    // <TypeName>`), the `SKIP <path>` entries sorted, then the `SKIP REGEXP
    // '<regex>'` entries. Each path is rendered through `backQuoteIfNeed` on the
    // whole path (a dotted path always quotes, and a path literally named `SKIP`
    // case-insensitively always quotes).
    //
    // This is a physical type (`physical_delegate` returns `None`); the runtime
    // typed-path columns, block-local dynamic paths, and shared-data overflow all
    // live on [`crate::column::JsonColumn`], not here. `max_dynamic_paths` default
    // 1024 (legal max 10000), `max_dynamic_types` default 32 (legal max 254),
    // typed paths max 1000. A typed path type may itself be `JSON` (nested).
    Json {
        max_dynamic_paths: u32,
        max_dynamic_types: u8,
        /// Declared typed paths, kept sorted lexicographically by path string.
        typed_paths: Vec<(String, ChType)>,
        /// `SKIP <path>` entries, kept sorted lexicographically.
        skip_paths: Vec<String>,
        /// `SKIP REGEXP '<regex>'` entries, kept sorted.
        skip_regexps: Vec<String>,
    },
}

/// Default `max_dynamic_paths` for `JSON` (`DEFAULT_MAX_DYNAMIC_PATHS`, confirmed
/// at v26.6.1.1193-stable). Omitted from the canonical name at this value.
pub const JSON_DEFAULT_MAX_DYNAMIC_PATHS: u32 = 1024;
/// Largest legal `max_dynamic_paths` for `JSON` (`MAX_DYNAMIC_PATHS_LIMIT`).
pub const JSON_MAX_DYNAMIC_PATHS: u32 = 10000;
/// Default `max_dynamic_types` for `JSON` (`DataTypeDynamic::DEFAULT_MAX_DYNAMIC_TYPES`).
/// Omitted from the canonical name at this value.
pub const JSON_DEFAULT_MAX_DYNAMIC_TYPES: u8 = 32;
/// Largest legal `max_dynamic_types` for `JSON`.
pub const JSON_MAX_DYNAMIC_TYPES: u8 = 254;
/// Largest number of typed paths a `JSON` type may declare (`MAX_TYPED_PATHS`).
pub const JSON_MAX_TYPED_PATHS: usize = 1000;

/// The seven ClickHouse geo alias kinds. Each renders its bare alias name and
/// expands to a fixed `Tuple`/`Array`-of-`Float64` nesting via
/// `GeoKind::underlying_type`; the wire layout and Arrow shape are exactly
/// that of the underlying nesting (confirmed at v26.8.1.2041-lts,
/// `DataTypeCustomGeo.{h,cpp}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GeoKind {
    Point,
    Ring,
    LineString,
    MultiLineString,
    Polygon,
    MultiPolygon,
    MultiPoint,
}

impl GeoKind {
    /// The bare alias spelling the server emits in a Native header.
    pub(crate) fn name(self) -> &'static str {
        match self {
            GeoKind::Point => "Point",
            GeoKind::Ring => "Ring",
            GeoKind::LineString => "LineString",
            GeoKind::MultiLineString => "MultiLineString",
            GeoKind::Polygon => "Polygon",
            GeoKind::MultiPolygon => "MultiPolygon",
            GeoKind::MultiPoint => "MultiPoint",
        }
    }

    /// The physical nesting depth this geo alias expands to, the
    /// `type_depth`/`parse_ch_type_depth` charge for the alias token. It equals
    /// the depth of [`GeoKind::underlying_type`]: `Point` is a `Tuple` one level
    /// deep (1), each `Array` level adds one, so `Ring`/`LineString` are 2,
    /// `MultiPoint` is 2, `Polygon`/`MultiLineString` are 3, and
    /// `MultiPolygon` is 4. Both the
    /// decoder's parse-time depth cap and the encoder's `type_depth` charge a geo
    /// token this many levels so a geo-tipped type that decodes is always
    /// re-encodable (the two sides agree on the physical depth).
    pub(crate) fn expansion_depth(self) -> usize {
        match self {
            GeoKind::Point => 1,
            GeoKind::Ring | GeoKind::LineString | GeoKind::MultiPoint => 2,
            GeoKind::Polygon | GeoKind::MultiLineString => 3,
            GeoKind::MultiPolygon => 4,
        }
    }

    /// The underlying physical `ChType` this alias decorates. `Point` is an
    /// UNNAMED two-`Float64` tuple; each `Array` level wraps the level below.
    pub(crate) fn underlying_type(self) -> ChType {
        fn point() -> ChType {
            ChType::Tuple(vec![(None, ChType::Float64), (None, ChType::Float64)])
        }
        match self {
            GeoKind::Point => point(),
            GeoKind::Ring | GeoKind::LineString | GeoKind::MultiPoint => {
                ChType::Array(Box::new(point()))
            }
            GeoKind::Polygon | GeoKind::MultiLineString => {
                ChType::Array(Box::new(ChType::Array(Box::new(point()))))
            }
            GeoKind::MultiPolygon => ChType::Array(Box::new(ChType::Array(Box::new(
                ChType::Array(Box::new(point())),
            )))),
        }
    }

    /// Borrow the process-wide physical type tree for this geo alias.
    ///
    /// Bulk dispatch walks geo types several times per column for the state
    /// prefix, body, and suffix. Caching these seven immutable trees keeps those
    /// traversals allocation-free while [`GeoKind::underlying_type`] preserves
    /// the existing owned helper for callers that need one.
    pub(crate) fn underlying_type_ref(self) -> &'static ChType {
        static UNDERLYING: LazyLock<[ChType; 7]> = LazyLock::new(|| {
            [
                GeoKind::Point.underlying_type(),
                GeoKind::Ring.underlying_type(),
                GeoKind::LineString.underlying_type(),
                GeoKind::MultiLineString.underlying_type(),
                GeoKind::Polygon.underlying_type(),
                GeoKind::MultiPolygon.underlying_type(),
                GeoKind::MultiPoint.underlying_type(),
            ]
        });
        let index = match self {
            GeoKind::Point => 0,
            GeoKind::Ring => 1,
            GeoKind::LineString => 2,
            GeoKind::MultiLineString => 3,
            GeoKind::Polygon => 4,
            GeoKind::MultiPolygon => 5,
            GeoKind::MultiPoint => 6,
        };
        &UNDERLYING[index]
    }
}

/// The canonical global-discriminator order of ClickHouse `Geometry`.
///
/// This explicit order is a wire invariant. ClickHouse 26.8 appended
/// `MultiPoint` at discriminator 6 without changing the existing 0 through 5
/// assignments, so it is deliberately not derived by sorting the names.
/// Confirmed at v26.8.1.2041-lts in `DataTypeCustomGeo.cpp`,
/// `registerDataTypeDomainGeo`, and `DataTypeVariant.cpp`.
pub(crate) const GEOMETRY_ALTERNATIVES: [ChType; 7] = [
    ChType::Geo(GeoKind::LineString),
    ChType::Geo(GeoKind::MultiLineString),
    ChType::Geo(GeoKind::MultiPolygon),
    ChType::Geo(GeoKind::Point),
    ChType::Geo(GeoKind::Polygon),
    ChType::Geo(GeoKind::Ring),
    ChType::Geo(GeoKind::MultiPoint),
];

/// Physical nesting charged to a `Geometry` token: one Variant level plus the
/// deepest alternative, the four-level `MultiPolygon` expansion.
pub(crate) const GEOMETRY_EXPANSION_DEPTH: usize = 5;

/// The physical Variant decorated by the `Geometry` custom name.
///
/// The immutable tree is initialized once per process, then every internal
/// prefix/body/suffix, validation, and FFI traversal borrows it. The hot body
/// remains the existing Variant discriminator pass plus one dense decode per
/// selected geo child, with no remapping, value copies, or type-tree allocation.
pub(crate) fn geometry_underlying_type() -> &'static ChType {
    static UNDERLYING: LazyLock<ChType> =
        LazyLock::new(|| ChType::Variant(Vec::from(GEOMETRY_ALTERNATIVES)));
    &UNDERLYING
}

/// The underlying physical `ChType` a `Nested(...)` decorates: an
/// `Array(Tuple(named elements))`. The Tuple carries the Nested field names, so
/// the Arrow struct children are named exactly like the server's flattened
/// `n.a Array(T)` sibling columns without the `n.` prefix.
pub(crate) fn nested_underlying_type(fields: &[(String, ChType)]) -> ChType {
    ChType::Array(Box::new(ChType::Tuple(
        fields
            .iter()
            .map(|(name, ty)| (Some(name.clone()), ty.clone()))
            .collect(),
    )))
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

/// The unit carried by one of ClickHouse's 11 `Interval*` logical types.
///
/// Every kind has the same signed `Int64` Native body. Keeping the unit in a
/// compact enum preserves the exact ClickHouse type while allowing all kinds
/// to share one physical [`crate::column::Column::Interval`] buffer variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalKind {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    Nanosecond,
}

impl IntervalKind {
    /// The canonical, case-sensitive type name emitted in Native headers.
    pub(crate) fn type_name(self) -> &'static str {
        match self {
            IntervalKind::Year => "IntervalYear",
            IntervalKind::Quarter => "IntervalQuarter",
            IntervalKind::Month => "IntervalMonth",
            IntervalKind::Week => "IntervalWeek",
            IntervalKind::Day => "IntervalDay",
            IntervalKind::Hour => "IntervalHour",
            IntervalKind::Minute => "IntervalMinute",
            IntervalKind::Second => "IntervalSecond",
            IntervalKind::Millisecond => "IntervalMillisecond",
            IntervalKind::Microsecond => "IntervalMicrosecond",
            IntervalKind::Nanosecond => "IntervalNanosecond",
        }
    }
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
/// `DateTime64`/`Time64` precision above 9) still render, but `parse_ch_type`
/// rejects them by design.
impl std::fmt::Display for ChType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChType::Nothing => write!(f, "Nothing"),
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
            ChType::BFloat16 => write!(f, "BFloat16"),
            ChType::QBit {
                element_type,
                dimension,
            } => write!(f, "QBit({element_type}, {dimension})"),
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
            ChType::Time => write!(f, "Time"),
            ChType::Time64 { precision } => write!(f, "Time64({precision})"),
            ChType::Interval(kind) => f.write_str(kind.type_name()),
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
            ChType::Variant(alternatives) => {
                write!(f, "Variant(")?;
                for (i, alternative) in alternatives.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{alternative}")?;
                }
                write!(f, ")")
            }
            ChType::Dynamic { max_types: 32 } => write!(f, "Dynamic"),
            ChType::Dynamic { max_types } => write!(f, "Dynamic(max_types={max_types})"),
            // `func` already carries any parenthesized params, so this renders
            // the exact spelling the server emits and round-trips through the
            // parser.
            ChType::SimpleAggregateFunction { func, inner } => {
                write!(f, "SimpleAggregateFunction({func}, {inner})")
            }
            ChType::AggregateFunction {
                function,
                arguments,
            } => {
                write!(f, "AggregateFunction({function}")?;
                for argument in arguments {
                    write!(f, ", {argument}")?;
                }
                write!(f, ")")
            }
            // The bare alias spelling, never the expanded form.
            ChType::Geo(kind) => write!(f, "{}", kind.name()),
            ChType::Geometry => write!(f, "Geometry"),
            ChType::Nested(fields) => write_nested(f, fields),
            ChType::Json {
                max_dynamic_paths,
                max_dynamic_types,
                typed_paths,
                skip_paths,
                skip_regexps,
            } => write_json(
                f,
                *max_dynamic_paths,
                *max_dynamic_types,
                typed_paths,
                skip_paths,
                skip_regexps,
            ),
        }
    }
}

/// Render the canonical `JSON` type string (`DataTypeObject::doGetName`,
/// confirmed at v26.6.1.1193-stable). The bare word `JSON` when no parameter is
/// non-default and no paths are declared, otherwise `JSON(...)` joining these in
/// order: `max_dynamic_types=M` (only if != 32), `max_dynamic_paths=N` (only if
/// != 1024), typed paths sorted (`<path> <TypeName>`), `SKIP <path>` entries
/// sorted, then `SKIP REGEXP '<regex>'` entries. Each path is quoted through
/// [`write_json_path`], the regex through [`escape_enum_name`] (the server's
/// single-quoted string escaping).
fn write_json(
    f: &mut std::fmt::Formatter<'_>,
    max_dynamic_paths: u32,
    max_dynamic_types: u8,
    typed_paths: &[(String, ChType)],
    skip_paths: &[String],
    skip_regexps: &[String],
) -> std::fmt::Result {
    let has_params = max_dynamic_types != JSON_DEFAULT_MAX_DYNAMIC_TYPES
        || max_dynamic_paths != JSON_DEFAULT_MAX_DYNAMIC_PATHS
        || !typed_paths.is_empty()
        || !skip_paths.is_empty()
        || !skip_regexps.is_empty();
    if !has_params {
        return write!(f, "JSON");
    }
    write!(f, "JSON(")?;
    let mut first = true;
    let mut sep = |f: &mut std::fmt::Formatter<'_>| -> std::fmt::Result {
        if first {
            first = false;
            Ok(())
        } else {
            write!(f, ", ")
        }
    };
    if max_dynamic_types != JSON_DEFAULT_MAX_DYNAMIC_TYPES {
        sep(f)?;
        write!(f, "max_dynamic_types={max_dynamic_types}")?;
    }
    if max_dynamic_paths != JSON_DEFAULT_MAX_DYNAMIC_PATHS {
        sep(f)?;
        write!(f, "max_dynamic_paths={max_dynamic_paths}")?;
    }
    for (path, ch_type) in typed_paths {
        sep(f)?;
        write_json_path(f, path)?;
        write!(f, " {ch_type}")?;
    }
    for path in skip_paths {
        sep(f)?;
        write!(f, "SKIP ")?;
        write_json_path(f, path)?;
    }
    for regex in skip_regexps {
        sep(f)?;
        write!(f, "SKIP REGEXP '{}'", escape_enum_name(regex))?;
    }
    write!(f, ")")
}

/// Write one `JSON` path, backtick-quoting it when the server's `backQuoteIfNeed`
/// would (see [`is_bare_identifier`]) or when the whole path is the JSON keyword
/// `SKIP` (case-insensitive), which the server always quotes as a typed/skip path
/// name (confirmed at v26.6.1.1193-stable).
fn write_json_path(f: &mut std::fmt::Formatter<'_>, path: &str) -> std::fmt::Result {
    if is_bare_identifier(path) && !path.eq_ignore_ascii_case("skip") {
        write!(f, "{path}")
    } else {
        write!(f, "`{}`", escape_back_quoted(path))
    }
}

/// Render a `Nested(name1 T1, ...)` type string: the fields joined by `, `
/// inside one pair of parentheses, each as `name type`. Names are mandatory and
/// quoted by the same `backQuoteIfNeed` rules as named `Tuple` elements
/// (confirmed at v26.6.1.1193-stable, `DataTypeNested.cpp` renders each name
/// with `backQuoteIfNeed` exactly like `DataTypeTuple`).
fn write_nested(f: &mut std::fmt::Formatter<'_>, fields: &[(String, ChType)]) -> std::fmt::Result {
    write!(f, "Nested(")?;
    for (i, (name, ch_type)) in fields.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        back_quote_if_need(f, name)?;
        write!(f, " {ch_type}")?;
    }
    write!(f, ")")
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

    /// The underlying physical type a name-decoration alias delegates to, or
    /// `None` for a type that is already physical.
    ///
    /// `SimpleAggregateFunction`, the geo aliases, `Geometry`, and `Nested` all
    /// attach only a custom name to an underlying type instance whose
    /// serialization slot is null (confirmed at v26.6.1.1193-stable), so their
    /// wire bytes, state prefix, and Arrow shape are byte-identical to the type
    /// returned here. The decode, encode, scan, and Arrow-export paths call the
    /// crate-private borrowed form at the top of their per-type dispatch and
    /// recurse on the delegate, so a single expansion point keeps all four
    /// directions consistent. This public API returns an owned `ChType` for
    /// bindings; the clone is bounded by the parsed type depth and never runs
    /// per row.
    ///
    /// Public so a binding crate can reuse the same single expansion point when
    /// mapping decoded columns to host values or building columns for encode,
    /// rather than duplicating the geo/Nested/SAF layout and drifting from it.
    pub fn physical_delegate(&self) -> Option<ChType> {
        self.physical_delegate_ref().map(Cow::into_owned)
    }

    /// Borrow a cached delegate when its shape is fixed, allocating only for a
    /// `Nested` expansion whose fields are carried by this particular value.
    ///
    /// This is the internal hot-dispatch form. In particular, all seven geo trees
    /// and the Geometry Variant tree are initialized once and then borrowed
    /// across state-prefix, body, suffix, validation, and Arrow traversals.
    pub(crate) fn physical_delegate_ref(&self) -> Option<Cow<'_, ChType>> {
        match self {
            ChType::SimpleAggregateFunction { inner, .. } => Some(Cow::Borrowed(inner)),
            ChType::Geo(kind) => Some(Cow::Borrowed(kind.underlying_type_ref())),
            ChType::Geometry => Some(Cow::Borrowed(geometry_underlying_type())),
            ChType::Nested(fields) => Some(Cow::Owned(nested_underlying_type(fields))),
            _ => None,
        }
    }

    /// Resolve the complete name-decoration chain to its physical type.
    ///
    /// Borrowed delegates recurse without cloning. An owned delegate currently
    /// comes only from `Nested` and is already physical, but the loop also
    /// handles a future owned alias-of-alias by taking an owned copy of its next
    /// delegate before replacing the tree that borrowed it.
    pub(crate) fn resolved_physical_delegate_ref(&self) -> Option<Cow<'_, ChType>> {
        match self.physical_delegate_ref()? {
            Cow::Borrowed(under) => under
                .resolved_physical_delegate_ref()
                .or(Some(Cow::Borrowed(under))),
            Cow::Owned(mut under) => {
                while let Some(next) = under.physical_delegate_ref() {
                    under = next.into_owned();
                }
                Some(Cow::Owned(under))
            }
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
    fn fixed_geo_delegates_are_cached_and_borrowed() {
        for ch_type in [
            ChType::Geo(GeoKind::Point),
            ChType::Geo(GeoKind::MultiPolygon),
            ChType::Geo(GeoKind::MultiPoint),
            ChType::Geometry,
        ] {
            let first = ch_type
                .physical_delegate_ref()
                .expect("fixed geo alias has a delegate");
            let second = ch_type
                .physical_delegate_ref()
                .expect("fixed geo alias has a delegate");
            assert!(matches!(first, Cow::Borrowed(_)));
            assert!(std::ptr::eq(first.as_ref(), second.as_ref()));
        }

        let chained = ChType::SimpleAggregateFunction {
            func: "anyLast".into(),
            inner: Box::new(ChType::Geo(GeoKind::Point)),
        };
        let resolved = chained
            .resolved_physical_delegate_ref()
            .expect("chained alias has a delegate");
        assert!(matches!(resolved, Cow::Borrowed(_)));
        assert!(std::ptr::eq(
            resolved.as_ref(),
            GeoKind::Point.underlying_type_ref()
        ));
    }

    #[test]
    fn geometry_alternatives_contain_each_geo_kind_once() {
        let expected = [
            GeoKind::Point,
            GeoKind::Ring,
            GeoKind::LineString,
            GeoKind::MultiLineString,
            GeoKind::Polygon,
            GeoKind::MultiPolygon,
            GeoKind::MultiPoint,
        ];
        assert_eq!(GEOMETRY_ALTERNATIVES.len(), expected.len());
        for kind in expected {
            let occurrences = GEOMETRY_ALTERNATIVES
                .iter()
                .filter(|alternative| matches!(alternative, ChType::Geo(candidate) if *candidate == kind))
                .count();
            assert_eq!(
                occurrences, 1,
                "Geometry contains {kind:?} {occurrences} times"
            );
        }
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
