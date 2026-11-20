//! Decoded columns -> descriptor JSON with buffer references.
//!
//! Every buffer is written as `{"$b":[ptr,byteLength,"dtype"]}`, where `ptr`
//! points at the decoded `Vec` inside the [`Output`] that owns it. Nothing is
//! copied here. The JS glue copies each referenced range out of linear memory
//! once, then frees the output (see `ABI.md`).
//!
//! The descriptor shape is the column-object shape the JS side returns: every
//! column has `kind` (the physical layout) and, when known, `type` (the full
//! ClickHouse type string). A nullable column carries an Arrow-style packed
//! `validity` bitmap (bit 1 = valid). Nested kinds carry their children as
//! further column objects.

use std::borrow::Cow;
use std::sync::Arc;

use ch_core_rs::batch::ColBatch;
use ch_core_rs::column::{
    Column, DynamicChild, DynamicColumn, JsonBody, Utf8Column, VariantColumn,
};
use ch_core_rs::native::decode::{low_cardinality_dict_value_type, parse_ch_type};
use ch_core_rs::schema::ChType;

use crate::json::Writer;
use crate::{Error, Output};

/// An element type that crosses the ABI as a typed buffer.
pub(crate) trait Elem: Copy {
    /// The `dtype` tag the JS glue maps to a TypedArray constructor.
    const DTYPE: &'static str;
    const SIZE: usize;
    /// Read one little-endian element from exactly `SIZE` bytes.
    fn from_le(bytes: &[u8]) -> Self;
}

macro_rules! elem {
    ($($t:ty => $tag:literal),* $(,)?) => {$(
        impl Elem for $t {
            const DTYPE: &'static str = $tag;
            const SIZE: usize = std::mem::size_of::<$t>();
            #[inline]
            fn from_le(bytes: &[u8]) -> Self {
                let mut raw = [0u8; std::mem::size_of::<$t>()];
                raw.copy_from_slice(bytes);
                <$t>::from_le_bytes(raw)
            }
        }
    )*};
}

elem!(
    i8 => "i8", u8 => "u8", i16 => "i16", u16 => "u16", i32 => "i32", u32 => "u32",
    i64 => "i64", u64 => "u64", f32 => "f32", f64 => "f64",
);

/// Strip name-only decorations (Nullable, SimpleAggregateFunction, geo
/// aliases, Geometry, Nested) down to the type that determines the column's
/// physical layout. Uses the core's single expansion point so this never
/// drifts from the decoder. `LowCardinality` is kept: it maps to its own
/// dictionary column.
pub(crate) fn physical(ty: &ChType) -> Cow<'_, ChType> {
    let mut t: Cow<'_, ChType> = Cow::Borrowed(ty);
    loop {
        let next = match t.as_ref() {
            ChType::Nullable(inner) => Some(inner.as_ref().clone()),
            other => other.physical_delegate(),
        };
        match next {
            Some(n) => t = Cow::Owned(n),
            None => return t,
        }
    }
}

/// Builds one result descriptor. Derived buffers that do not exist in the
/// decoded columns, such as Variant offsets, are kept in `derived` so their
/// pointers stay valid for the life of the [`Output`].
#[derive(Default)]
pub(crate) struct Describer {
    pub(crate) w: Writer,
    derived: Vec<Vec<u8>>,
}

impl Describer {
    /// Wrap the descriptor and everything its pointers refer to.
    pub(crate) fn into_output(self, batches: Vec<Arc<ColBatch>>, columns: Vec<Column>) -> Output {
        Output {
            json: self.w.finish(),
            batches,
            columns,
            bytes: self.derived,
        }
    }

    /// Write a buffer reference to `values` as the value of `key`.
    ///
    /// The caller must move whatever owns `values` into the [`Output`]
    /// built from this describer. Moving a `Vec` does not move its heap
    /// allocation, so the pointer stays valid.
    pub(crate) fn buf<T: Elem>(&mut self, key: &str, values: &[T]) {
        self.raw_buf(
            key,
            values.as_ptr() as usize,
            std::mem::size_of_val(values),
            T::DTYPE,
        );
    }

    fn raw_buf(&mut self, key: &str, ptr: usize, len: usize, dtype: &str) {
        self.w.key(key);
        self.w.begin_object();
        self.w.key("$b");
        self.w.begin_array();
        self.w.uint(ptr as u64);
        self.w.uint(len as u64);
        self.w.string(dtype);
        self.w.end_array();
        self.w.end_object();
    }

    /// Keep a derived buffer alive and reference it.
    fn derived_buf<T: Elem>(&mut self, key: &str, values: Vec<u8>) {
        self.raw_buf(key, values.as_ptr() as usize, values.len(), T::DTYPE);
        self.derived.push(values);
    }

    // -----------------------------------------------------------------------
    // Results
    // -----------------------------------------------------------------------

    /// `{ rowCount, columnNames, columnTypes, chunks: [{ rowCount, columns }] ...`
    /// The caller closes the object, so the stream path can append fields.
    pub(crate) fn begin_result<'a>(
        &mut self,
        row_count: usize,
        names: &[String],
        types: &[String],
        chunks: impl IntoIterator<Item = &'a ColBatch>,
    ) -> Result<(), Error> {
        let w = &mut self.w;
        w.begin_object();
        w.field_uint("rowCount", row_count as u64);
        w.key("columnNames");
        w.begin_array();
        for n in names {
            w.string(n);
        }
        w.end_array();
        w.key("columnTypes");
        w.begin_array();
        for t in types {
            w.string(t);
        }
        w.end_array();
        self.w.key("chunks");
        self.w.begin_array();
        for chunk in chunks {
            self.chunk(chunk)?;
        }
        self.w.end_array();
        Ok(())
    }

    fn chunk(&mut self, batch: &ColBatch) -> Result<(), Error> {
        if batch.schema.fields.len() != batch.num_columns() {
            return Err(Error::generic(format!(
                "decoded block is inconsistent: {} schema fields but {} columns",
                batch.schema.fields.len(),
                batch.num_columns()
            )));
        }
        self.w.begin_object();
        self.w.field_uint("rowCount", batch.num_rows as u64);
        self.w.key("columns");
        self.w.begin_array();
        for (field, col) in batch.schema.fields.iter().zip(&batch.columns) {
            self.column(col, Some(&field.ch_type), Some(&field.name))?;
        }
        self.w.end_array();
        self.w.end_object();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Columns
    // -----------------------------------------------------------------------

    /// Write one column object.
    pub(crate) fn column(
        &mut self,
        col: &Column,
        ty: Option<&ChType>,
        name: Option<&str>,
    ) -> Result<(), Error> {
        self.w.begin_object();
        if let Some(n) = name {
            self.w.field_str("name", n);
        }
        if let Some(t) = ty {
            self.w.field_str("type", &t.to_string());
        }
        let phys = ty.map(physical);
        let phys = phys.as_deref();
        self.type_metadata(phys);

        macro_rules! values {
            ($kind:literal, $c:expr) => {{
                self.w.field_str("kind", $kind);
                self.buf("values", &$c.values);
            }};
        }

        match col {
            Column::Nothing(c) => {
                self.w.field_str("kind", "Nothing");
                self.w.field_uint("length", c.len as u64);
            }
            Column::Bool(c) => {
                self.w.field_str("kind", "Bool");
                self.w.field_uint("length", c.len as u64);
                self.buf("bitmap", &c.bitmap);
            }
            Column::Int8(c) => values!("Int8", c),
            Column::Int16(c) => values!("Int16", c),
            Column::Int32(c) => values!("Int32", c),
            Column::Int64(c) => values!("Int64", c),
            Column::UInt8(c) => values!("UInt8", c),
            Column::UInt16(c) => values!("UInt16", c),
            Column::UInt32(c) => values!("UInt32", c),
            Column::UInt64(c) => values!("UInt64", c),
            Column::Float32(c) => values!("Float32", c),
            Column::Float64(c) => values!("Float64", c),
            Column::BFloat16(c) => {
                // Raw little-endian 16-bit words: the top half of an f32.
                self.w.field_str("kind", "BFloat16");
                self.buf("values", c.values.as_flattened());
            }
            Column::QBit(c) => {
                self.w.field_str("kind", "QBit");
                self.w.field_uint("dimension", c.dimension as u64);
                let elem = match phys {
                    Some(ChType::QBit { element_type, .. }) => {
                        parse_ch_type(&element_type.to_string())
                    }
                    _ => None,
                };
                self.w.key("values");
                self.column(&c.values, elem.as_ref(), None)?;
            }
            Column::Date(c) => values!("Date", c),
            Column::Date32(c) => values!("Date32", c),
            Column::DateTime(c) => values!("DateTime", c),
            Column::DateTime64(c) => values!("DateTime64", c),
            Column::Time(c) => values!("Time", c),
            Column::Time64(c) => values!("Time64", c),
            Column::Interval(c) => values!("Interval", c),
            Column::Utf8(c) => self.utf8_body("String", c),
            Column::AggregateState(c) => {
                self.w.field_str("kind", "AggregateFunction");
                self.buf("offsets", &c.offsets);
                self.buf("data", &c.data);
            }
            Column::FixedBinary(c) => self.fixed("FixedString", &c.data, c.width),
            Column::Ipv4(c) => values!("IPv4", c),
            Column::Ipv6(c) => self.fixed("IPv6", &c.data, c.width),
            Column::Uuid(c) => self.fixed("UUID", &c.data, c.width),
            Column::Enum8(c) => values!("Enum8", c),
            Column::Enum16(c) => values!("Enum16", c),
            Column::Decimal(c) => {
                self.fixed("Decimal", &c.data, c.width);
                self.w.field_uint("precision", u64::from(c.precision));
                self.w.field_uint("scale", u64::from(c.scale));
            }
            Column::Int128(c) => self.fixed("Int128", &c.data, c.width),
            Column::UInt128(c) => self.fixed("UInt128", &c.data, c.width),
            Column::Int256(c) => self.fixed("Int256", &c.data, c.width),
            Column::UInt256(c) => self.fixed("UInt256", &c.data, c.width),
            Column::Dictionary(c) => {
                self.w.field_str("kind", "LowCardinality");
                self.buf("indices", &c.indices);
                let dict_ty = match phys {
                    Some(ChType::LowCardinality(inner)) => {
                        Some(low_cardinality_dict_value_type(inner).1.clone())
                    }
                    _ => None,
                };
                self.w.key("dictionary");
                self.column(&c.values, dict_ty.as_ref(), None)?;
            }
            Column::Array(c) => {
                self.w.field_str("kind", "Array");
                self.buf("offsets", &c.offsets);
                let elem = match phys {
                    Some(ChType::Array(e)) => Some(e.as_ref()),
                    _ => None,
                };
                self.w.key("values");
                self.column(&c.values, elem, None)?;
            }
            Column::Tuple(c) => {
                self.w.field_str("kind", "Tuple");
                self.w.field_uint("length", c.len as u64);
                let elems = match phys {
                    Some(ChType::Tuple(e)) if e.len() == c.fields.len() => Some(e),
                    _ => None,
                };
                self.w.key("names");
                self.w.begin_array();
                for i in 0..c.fields.len() {
                    match elems.and_then(|e| e[i].0.as_deref()) {
                        Some(n) => self.w.string(n),
                        None => self.w.null(),
                    }
                }
                self.w.end_array();
                self.w.key("fields");
                self.w.begin_array();
                for (i, f) in c.fields.iter().enumerate() {
                    self.column(f, elems.map(|e| &e[i].1), None)?;
                }
                self.w.end_array();
            }
            Column::Map(c) => {
                self.w.field_str("kind", "Map");
                self.buf("offsets", &c.offsets);
                let Column::Tuple(entries) = c.entries.as_ref() else {
                    return Err(Error::invalid("Map entries are not a Tuple"));
                };
                if entries.fields.len() != 2 {
                    return Err(Error::invalid("Map entries must have 2 fields"));
                }
                let (kty, vty) = match phys {
                    Some(ChType::Map(k, v)) => (Some(k.as_ref()), Some(v.as_ref())),
                    _ => (None, None),
                };
                self.w.key("keys");
                self.column(&entries.fields[0], kty, None)?;
                self.w.key("values");
                self.column(&entries.fields[1], vty, None)?;
            }
            Column::Variant(c) => self.variant(c, phys)?,
            Column::Dynamic(c) => self.dynamic(c)?,
            Column::Json(c) => {
                self.w.field_str("kind", "JSON");
                match &c.body {
                    JsonBody::Text(s) => {
                        self.w.field_str("format", "text");
                        self.w.key("text");
                        self.utf8_object(s, None);
                    }
                    JsonBody::Structured(s) => {
                        self.w.field_str("format", "structured");
                        self.w.field_uint("length", s.len as u64);
                        let declared = match phys {
                            Some(ChType::Json { typed_paths, .. }) => Some(typed_paths),
                            _ => None,
                        };
                        self.w.key("typedPaths");
                        self.w.begin_array();
                        for (path, col) in &s.typed {
                            let pty = declared
                                .and_then(|d| d.iter().find(|(p, _)| p == path))
                                .map(|(_, t)| t);
                            self.w.begin_object();
                            self.w.field_str("path", path);
                            self.w.key("column");
                            self.column(col, pty, None)?;
                            self.w.end_object();
                        }
                        self.w.end_array();
                        self.w.key("dynamicPaths");
                        self.w.begin_array();
                        for (path, col) in &s.dynamic {
                            self.w.begin_object();
                            self.w.field_str("path", path);
                            self.w.key("column");
                            self.w.begin_object();
                            self.dynamic(col)?;
                            self.w.end_object();
                            self.w.end_object();
                        }
                        self.w.end_array();
                        self.buf("sharedOffsets", &s.shared_offsets);
                        self.w.key("sharedPaths");
                        self.utf8_object(&s.shared_paths, None);
                        self.w.key("sharedValues");
                        self.utf8_object(&s.shared_values, None);
                    }
                }
            }
        }

        if let Some(validity) = col.validity() {
            self.buf("validity", validity.as_bytes());
        }
        self.w.end_object();
        Ok(())
    }

    /// The `kind`, `offsets` and `data` fields of a String-shaped column.
    fn utf8_body(&mut self, kind: &str, c: &Utf8Column) {
        self.w.field_str("kind", kind);
        self.buf("offsets", &c.offsets);
        self.buf("data", &c.data);
    }

    /// A standalone String-shaped column object, used for JSON text, JSON
    /// shared data and the Dynamic `SharedVariant` child.
    fn utf8_object(&mut self, c: &Utf8Column, kind: Option<&str>) {
        self.w.begin_object();
        self.utf8_body(kind.unwrap_or("String"), c);
        if let Some(v) = &c.validity {
            self.buf("validity", v.as_bytes());
        }
        self.w.end_object();
    }

    fn fixed(&mut self, kind: &str, data: &[u8], width: usize) {
        self.w.field_str("kind", kind);
        self.w.field_uint("width", width as u64);
        self.buf("data", data);
    }

    /// Type metadata a binding needs to turn raw wire values into host values.
    fn type_metadata(&mut self, ty: Option<&ChType>) {
        match ty {
            Some(ChType::DateTime { timezone: Some(tz) }) => self.w.field_str("timezone", tz),
            Some(ChType::DateTime64 {
                precision,
                timezone,
            }) => {
                self.w.field_uint("precision", u64::from(*precision));
                if let Some(tz) = timezone {
                    self.w.field_str("timezone", tz);
                }
            }
            Some(ChType::Time64 { precision }) => {
                self.w.field_uint("precision", u64::from(*precision))
            }
            Some(ChType::Enum8 { variants }) => {
                self.enum_values(variants.iter().map(|(n, v)| (n, i64::from(*v))))
            }
            Some(ChType::Enum16 { variants }) => {
                self.enum_values(variants.iter().map(|(n, v)| (n, i64::from(*v))))
            }
            _ => {}
        }
    }

    pub(crate) fn enum_values<'a>(&mut self, variants: impl Iterator<Item = (&'a String, i64)>) {
        self.w.key("enumValues");
        self.w.begin_array();
        for (name, v) in variants {
            self.w.begin_array();
            self.w.string(name);
            self.w.int(v);
            self.w.end_array();
        }
        self.w.end_array();
    }

    /// Variant: per-row global discriminators (255 = NULL), per-row offsets
    /// into the selected alternative, and one compact child column per
    /// alternative. Offsets are derived here from the discriminators so JS
    /// does not need to understand the two-level Arrow union layout used past
    /// 127 alternatives.
    fn variant(&mut self, c: &VariantColumn, ty: Option<&ChType>) -> Result<(), Error> {
        self.w.field_str("kind", "Variant");
        let alts = match ty {
            Some(ChType::Variant(a)) if a.len() == c.variants.len() => Some(a),
            _ => None,
        };
        let mut seen = [0i32; 256];
        let mut offsets = Vec::with_capacity(c.discriminators.len() * 4);
        for &d in &c.discriminators {
            let slot = &mut seen[usize::from(d)];
            offsets.extend_from_slice(&slot.to_le_bytes());
            *slot += 1;
        }
        self.buf("discriminators", &c.discriminators);
        self.derived_buf::<i32>("offsets", offsets);
        self.w.key("variants");
        self.w.begin_array();
        for (i, v) in c.variants.iter().enumerate() {
            self.column(v, alts.map(|a| &a[i]), None)?;
        }
        self.w.end_array();
        Ok(())
    }

    /// Dynamic: per-row `typeIds` (index into `children`, 0xFFFFFFFF = NULL)
    /// and dense `offsets` into that child. Each child is either a typed column
    /// or the `SharedVariant` overflow, whose cells are binary-encoded values
    /// for `chw_decode_binary_value`.
    fn dynamic(&mut self, c: &DynamicColumn) -> Result<(), Error> {
        self.w.field_str("kind", "Dynamic");
        self.buf("typeIds", &c.type_ids);
        self.buf("offsets", &c.offsets);
        self.w.key("children");
        self.w.begin_array();
        for child in &c.children {
            match child {
                DynamicChild::Typed { ch_type, values } => {
                    self.column(values, Some(ch_type), None)?
                }
                DynamicChild::Shared(values) => self.utf8_object(values, Some("SharedVariant")),
            }
        }
        self.w.end_array();
        Ok(())
    }
}
