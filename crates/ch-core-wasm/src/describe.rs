//! `chw_describe_type`: the core's type parser, exposed so a JS binding can
//! lay out column buffers for a ClickHouse type without a second parser.

use ch_core_rs::native::decode::{low_cardinality_dict_value_type, parse_ch_type};
use ch_core_rs::schema::ChType;

use crate::decode::{physical, Describer};

/// Write `{ type, kind, nullable, ...metadata, ...children }` for `ty`, where
/// `kind` is the column-object kind the decoder emits for that type. Name-only
/// aliases (geo types, Nested, SimpleAggregateFunction) resolve to their
/// physical layout. Types the core cannot encode report `kind: "Unsupported"`.
pub(crate) fn describe(d: &mut Describer, ty: &ChType) {
    d.w.begin_object();
    describe_fields(d, ty);
    d.w.end_object();
}

/// The fields of [`describe`] without the enclosing braces.
fn describe_fields(d: &mut Describer, ty: &ChType) {
    let w = &mut d.w;
    w.field_str("type", &ty.to_string());
    let phys = physical(ty);
    let nullable = matches!(ty, ChType::Nullable(_))
        || matches!(phys.as_ref(), ChType::LowCardinality(i) if low_cardinality_dict_value_type(i).0);
    w.key("nullable");
    w.bool(nullable);
    let kind = match phys.as_ref() {
        ChType::Nothing => "Nothing",
        ChType::Bool => "Bool",
        ChType::Int8 => "Int8",
        ChType::Int16 => "Int16",
        ChType::Int32 => "Int32",
        ChType::Int64 => "Int64",
        ChType::UInt8 => "UInt8",
        ChType::UInt16 => "UInt16",
        ChType::UInt32 => "UInt32",
        ChType::UInt64 => "UInt64",
        ChType::Float32 => "Float32",
        ChType::Float64 => "Float64",
        ChType::BFloat16 => "BFloat16",
        ChType::QBit {
            element_type,
            dimension,
        } => {
            d.w.field_uint("dimension", *dimension as u64);
            if let Some(e) = parse_ch_type(&element_type.to_string()) {
                d.w.key("element");
                describe(d, &e);
            }
            "QBit"
        }
        ChType::Date => "Date",
        ChType::Date32 => "Date32",
        ChType::DateTime { timezone } => {
            if let Some(tz) = timezone {
                d.w.field_str("timezone", tz);
            }
            "DateTime"
        }
        ChType::DateTime64 {
            precision,
            timezone,
        } => {
            d.w.field_uint("precision", u64::from(*precision));
            if let Some(tz) = timezone {
                d.w.field_str("timezone", tz);
            }
            "DateTime64"
        }
        ChType::Time => "Time",
        ChType::Time64 { precision } => {
            d.w.field_uint("precision", u64::from(*precision));
            "Time64"
        }
        ChType::Interval(_) => "Interval",
        ChType::String => "String",
        ChType::FixedString(n) => {
            d.w.field_uint("width", *n as u64);
            "FixedString"
        }
        ChType::Uuid => "UUID",
        ChType::Ipv4 => "IPv4",
        ChType::Ipv6 => "IPv6",
        ChType::Enum8 { variants } => {
            d.enum_values(variants.iter().map(|(n, v)| (n, i64::from(*v))));
            "Enum8"
        }
        ChType::Enum16 { variants } => {
            d.enum_values(variants.iter().map(|(n, v)| (n, i64::from(*v))));
            "Enum16"
        }
        ChType::Decimal {
            precision,
            scale,
            bits,
        } => {
            d.w.field_uint("precision", u64::from(*precision));
            d.w.field_uint("scale", u64::from(*scale));
            d.w.field_uint("width", u64::from(*bits / 8));
            "Decimal"
        }
        ChType::Int128 => "Int128",
        ChType::UInt128 => "UInt128",
        ChType::Int256 => "Int256",
        ChType::UInt256 => "UInt256",
        ChType::LowCardinality(inner) => {
            d.w.key("dictionary");
            describe(d, low_cardinality_dict_value_type(inner).1);
            "LowCardinality"
        }
        ChType::Array(e) => {
            d.w.key("element");
            describe(d, e);
            "Array"
        }
        ChType::Tuple(elems) => {
            d.w.key("names");
            d.w.begin_array();
            for (n, _) in elems {
                match n {
                    Some(n) => d.w.string(n),
                    None => d.w.null(),
                }
            }
            d.w.end_array();
            d.w.key("fields");
            d.w.begin_array();
            for (_, t) in elems {
                describe(d, t);
            }
            d.w.end_array();
            "Tuple"
        }
        ChType::Map(k, v) => {
            d.w.key("key");
            describe(d, k);
            d.w.key("value");
            describe(d, v);
            "Map"
        }
        ChType::Variant(alts) => {
            d.w.key("variants");
            d.w.begin_array();
            for t in alts {
                describe(d, t);
            }
            d.w.end_array();
            "Variant"
        }
        ChType::Dynamic { max_types } => {
            d.w.field_uint("maxTypes", u64::from(*max_types));
            "Dynamic"
        }
        ChType::Json { typed_paths, .. } => {
            d.w.key("typedPaths");
            d.w.begin_array();
            // Each entry is the path's type description plus its `path`.
            for (p, t) in typed_paths {
                d.w.begin_object();
                d.w.field_str("path", p);
                describe_fields(d, t);
                d.w.end_object();
            }
            d.w.end_array();
            "JSON"
        }
        ChType::AggregateFunction { .. } => "AggregateFunction",
        _ => "Unsupported",
    };
    d.w.field_str("kind", kind);
}
