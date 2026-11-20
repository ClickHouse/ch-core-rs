//! Descriptor JSON + data blob -> core `Column`s -> Native bytes (the INSERT
//! path).
//!
//! The input is the same column-object shape the decoder produces, so a
//! decoded chunk can be passed straight back. Each column's ClickHouse `type`
//! string is authoritative: the core parses it and it drives which buffers are
//! read. `kind` is ignored. A buffer is `{"$b":[offset,byteLength]}`, a range
//! of the data blob passed next to the descriptor, and is copied into the
//! column's `Vec` here.
//!
//! The core's encoder validates every structural invariant (lengths, offsets,
//! dictionary indices, null placement, Variant/Dynamic routing) before writing
//! a byte, so this layer only checks what it needs to build the columns.

use ch_core_rs::batch::ColBatch;
use ch_core_rs::bitmap::Bitmap;
use ch_core_rs::column::{
    AggregateStateColumn, ArrayColumn, BoolColumn, Column, DecimalColumn, DictionaryColumn,
    DynamicChild, DynamicColumn, FixedBinaryColumn, JsonColumn, MapColumn, NothingColumn,
    PrimitiveColumn, QBitColumn, StructuredJson, TupleColumn, Utf8Column, VariantColumn,
};
use ch_core_rs::native::decode::{low_cardinality_dict_value_type, parse_ch_type};
use ch_core_rs::native::encode::{encode_block, EncodeOptions};
use ch_core_rs::schema::{ChType, Field, Schema};

use crate::decode::{physical, Elem};
use crate::json::Value;
use crate::Error;

type R<T> = Result<T, Error>;

fn bad(path: &str, msg: impl AsRef<str>) -> Error {
    Error::invalid(format!("column {path}: {}", msg.as_ref()))
}

/// Encode input: the parsed descriptor and the blob its buffers point into.
struct Input<'a> {
    data: &'a [u8],
}

impl Input<'_> {
    /// The blob range a `{"$b":[offset,byteLength]}` reference names.
    fn range(&self, v: Option<&Value>, key: &str, path: &str) -> R<&[u8]> {
        let r = v
            .and_then(|v| v.get("$b"))
            .and_then(Value::as_array)
            .ok_or_else(|| bad(path, format!("expected '{key}' to be a typed array")))?;
        let (off, len) = match r {
            [off, len, ..] => (off.as_usize(), len.as_usize()),
            _ => (None, None),
        };
        let (Some(off), Some(len)) = (off, len) else {
            return Err(bad(
                path,
                format!("'{key}' has a malformed buffer reference"),
            ));
        };
        off.checked_add(len)
            .and_then(|end| self.data.get(off..end))
            .ok_or_else(|| bad(path, format!("'{key}' points outside the data buffer")))
    }

    /// Copy a required typed-array field into a `Vec<T>`.
    fn buf<T: Elem>(&self, obj: &Value, key: &str, path: &str) -> R<Vec<T>> {
        self.buf_of(obj.get(key), key, path)
    }

    fn buf_of<T: Elem>(&self, v: Option<&Value>, key: &str, path: &str) -> R<Vec<T>> {
        let bytes = self.range(v, key, path)?;
        if bytes.len() % T::SIZE != 0 {
            return Err(bad(
                path,
                format!("'{key}' length is not a multiple of {} bytes", T::SIZE),
            ));
        }
        Ok(bytes.chunks_exact(T::SIZE).map(T::from_le).collect())
    }

    /// Byte length of a typed-array field, for sizing a dictionary.
    fn byte_len(&self, obj: &Value, key: &str) -> Option<usize> {
        self.range(obj.get(key), key, "").ok().map(<[u8]>::len)
    }
}

fn array<'v>(obj: &'v Value, key: &str, path: &str) -> R<&'v [Value]> {
    obj.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| bad(path, format!("expected '{key}' to be an array")))
}

fn str_field<'v>(obj: &'v Value, key: &str) -> Option<&'v str> {
    obj.get(key).and_then(Value::as_str)
}

fn type_of(obj: &Value, path: &str) -> R<ChType> {
    let s = str_field(obj, "type").ok_or_else(|| bad(path, "missing ClickHouse 'type' string"))?;
    parse_ch_type(s).ok_or_else(|| bad(path, format!("cannot parse type '{s}'")))
}

/// Length implied by an Arrow-style offsets buffer (its last entry).
fn last_offset(offsets: &[i64], path: &str) -> R<usize> {
    let last = *offsets
        .last()
        .ok_or_else(|| bad(path, "offsets must not be empty"))?;
    usize::try_from(last).map_err(|_| bad(path, "negative offset"))
}

/// Rows routed to each child by a discriminator or type-id buffer.
fn counts<T: Copy + Into<u64>>(ids: &[T], null: u64, n: usize) -> Vec<usize> {
    let mut c = vec![0usize; n];
    for &id in ids {
        let id: u64 = id.into();
        if id != null {
            if let Some(slot) = usize::try_from(id).ok().and_then(|i| c.get_mut(i)) {
                *slot += 1;
            }
        }
    }
    c
}

/// Element width of the primitive types `build` reads from a `values` buffer.
fn primitive_width(ty: &ChType) -> Option<usize> {
    Some(match ty {
        ChType::Int8 | ChType::UInt8 | ChType::Enum8 { .. } => 1,
        ChType::Int16 | ChType::UInt16 | ChType::Date | ChType::Enum16 { .. } => 2,
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
        _ => return None,
    })
}

/// Attach a validity bitmap to the column variants that carry one.
fn attach_validity(col: &mut Column, bm: Bitmap, path: &str) -> R<()> {
    let slot = match col {
        Column::Nothing(c) => &mut c.validity,
        Column::Bool(c) => &mut c.validity,
        Column::Int8(c) | Column::Enum8(c) => &mut c.validity,
        Column::Int16(c) | Column::Enum16(c) => &mut c.validity,
        Column::Int32(c) | Column::Date32(c) | Column::Time(c) => &mut c.validity,
        Column::Int64(c) | Column::DateTime64(c) | Column::Time64(c) | Column::Interval(c) => {
            &mut c.validity
        }
        Column::UInt8(c) => &mut c.validity,
        Column::UInt16(c) | Column::Date(c) => &mut c.validity,
        Column::UInt32(c) | Column::DateTime(c) | Column::Ipv4(c) => &mut c.validity,
        Column::UInt64(c) => &mut c.validity,
        Column::Float32(c) => &mut c.validity,
        Column::Float64(c) => &mut c.validity,
        Column::BFloat16(c) => &mut c.validity,
        Column::QBit(c) => &mut c.validity,
        Column::Utf8(c) => &mut c.validity,
        Column::FixedBinary(c)
        | Column::Ipv6(c)
        | Column::Uuid(c)
        | Column::Int128(c)
        | Column::UInt128(c)
        | Column::Int256(c)
        | Column::UInt256(c) => &mut c.validity,
        Column::Decimal(c) => &mut c.validity,
        Column::Dictionary(c) => &mut c.validity,
        Column::Tuple(c) => &mut c.validity,
        Column::Json(c) => &mut c.validity,
        _ => return Err(bad(path, "this type cannot carry a validity bitmap")),
    };
    *slot = Some(bm);
    Ok(())
}

impl Input<'_> {
    /// Build one core `Column` of `len` rows from a column object.
    fn column(&self, obj: Option<&Value>, ty: &ChType, len: usize, path: &str) -> R<Column> {
        let obj = match obj {
            Some(o @ Value::Object(_)) => o,
            _ => return Err(bad(path, "expected a column object")),
        };
        let phys = physical(ty);
        let mut col = self.build(obj, phys.as_ref(), len, path)?;
        if let Some(v) = obj.get("validity") {
            let bytes: Vec<u8> = self.buf_of(Some(v), "validity", path)?;
            if bytes.len() < len.div_ceil(8) {
                return Err(bad(path, "validity bitmap is shorter than the row count"));
            }
            attach_validity(&mut col, Bitmap::from_raw(bytes, len), path)?;
        }
        Ok(col)
    }

    fn utf8(&self, obj: Option<&Value>, path: &str) -> R<Utf8Column> {
        let obj = obj.ok_or_else(|| bad(path, "expected a String column object"))?;
        Ok(Utf8Column::new(
            self.buf(obj, "offsets", path)?,
            self.buf(obj, "data", path)?,
        ))
    }

    fn build(&self, obj: &Value, ty: &ChType, len: usize, path: &str) -> R<Column> {
        macro_rules! prim {
            ($Var:ident) => {
                Column::$Var(PrimitiveColumn::new(self.buf(obj, "values", path)?))
            };
        }
        let fixed = |width: usize| -> R<FixedBinaryColumn> {
            Ok(FixedBinaryColumn::new(self.buf(obj, "data", path)?, width))
        };
        Ok(match ty {
            ChType::Nothing => Column::Nothing(NothingColumn::new(len)),
            ChType::Bool => Column::Bool(BoolColumn {
                bitmap: self.buf(obj, "bitmap", path)?,
                len,
                validity: None,
            }),
            ChType::Int8 => prim!(Int8),
            ChType::Int16 => prim!(Int16),
            ChType::Int32 => prim!(Int32),
            ChType::Int64 => prim!(Int64),
            ChType::UInt8 => prim!(UInt8),
            ChType::UInt16 => prim!(UInt16),
            ChType::UInt32 => prim!(UInt32),
            ChType::UInt64 => prim!(UInt64),
            ChType::Float32 => prim!(Float32),
            ChType::Float64 => prim!(Float64),
            ChType::BFloat16 => {
                let raw: Vec<u8> = self.buf(obj, "values", path)?;
                if raw.len() % 2 != 0 {
                    return Err(bad(path, "BFloat16 values must be 2 bytes per row"));
                }
                Column::BFloat16(PrimitiveColumn::new(
                    raw.chunks_exact(2).map(|c| [c[0], c[1]]).collect(),
                ))
            }
            ChType::QBit {
                element_type,
                dimension,
            } => {
                let elem = parse_ch_type(&element_type.to_string())
                    .ok_or_else(|| bad(path, "bad QBit element type"))?;
                let n = len
                    .checked_mul(*dimension)
                    .ok_or_else(|| bad(path, "QBit size overflows"))?;
                let values = self.column(obj.get("values"), &elem, n, path)?;
                Column::QBit(QBitColumn::new(values, *dimension))
            }
            ChType::Date => prim!(Date),
            ChType::Date32 => prim!(Date32),
            ChType::DateTime { .. } => prim!(DateTime),
            ChType::DateTime64 { .. } => prim!(DateTime64),
            ChType::Time => prim!(Time),
            ChType::Time64 { .. } => prim!(Time64),
            ChType::Interval(_) => prim!(Interval),
            ChType::String => Column::Utf8(self.utf8(Some(obj), path)?),
            ChType::FixedString(n) => Column::FixedBinary(fixed(*n)?),
            ChType::Uuid => Column::Uuid(fixed(16)?),
            ChType::Ipv4 => prim!(Ipv4),
            ChType::Ipv6 => Column::Ipv6(fixed(16)?),
            ChType::Enum8 { .. } => prim!(Enum8),
            ChType::Enum16 { .. } => prim!(Enum16),
            ChType::Decimal {
                precision,
                scale,
                bits,
            } => Column::Decimal(DecimalColumn::new(
                self.buf(obj, "data", path)?,
                *bits as usize / 8,
                *precision,
                *scale,
            )),
            ChType::Int128 => Column::Int128(fixed(16)?),
            ChType::UInt128 => Column::UInt128(fixed(16)?),
            ChType::Int256 => Column::Int256(fixed(32)?),
            ChType::UInt256 => Column::UInt256(fixed(32)?),
            ChType::LowCardinality(inner) => {
                let indices = self.buf(obj, "indices", path)?;
                let dict_ty = low_cardinality_dict_value_type(inner).1.clone();
                let dict = obj.get("dictionary");
                let n = dict
                    .and_then(|d| self.dictionary_len(d, &dict_ty))
                    .ok_or_else(|| bad(path, "cannot determine dictionary size"))?;
                let values = self.column(dict, &dict_ty, n, &format!("{path}.dictionary"))?;
                Column::Dictionary(DictionaryColumn::new(indices, values))
            }
            ChType::Array(elem) => {
                let offsets = self.buf(obj, "offsets", path)?;
                let n = last_offset(&offsets, path)?;
                let values = self.column(obj.get("values"), elem, n, &format!("{path}[]"))?;
                Column::Array(ArrayColumn::new(offsets, values))
            }
            ChType::Tuple(elems) => {
                let fields = array(obj, "fields", path)?;
                if fields.len() != elems.len() {
                    return Err(bad(path, "Tuple field count does not match its type"));
                }
                let cols = elems
                    .iter()
                    .zip(fields)
                    .enumerate()
                    .map(|(i, ((name, t), f))| {
                        let p = match name {
                            Some(n) => format!("{path}.{n}"),
                            None => format!("{path}.{}", i + 1),
                        };
                        self.column(Some(f), t, len, &p)
                    })
                    .collect::<R<Vec<_>>>()?;
                Column::Tuple(TupleColumn::new(cols, len))
            }
            ChType::Map(k, v) => {
                let offsets = self.buf(obj, "offsets", path)?;
                let n = last_offset(&offsets, path)?;
                let keys = self.column(obj.get("keys"), k, n, &format!("{path}.keys"))?;
                let vals = self.column(obj.get("values"), v, n, &format!("{path}.values"))?;
                Column::Map(MapColumn::new(
                    offsets,
                    Column::Tuple(TupleColumn::new(vec![keys, vals], n)),
                ))
            }
            ChType::Variant(alts) => {
                let disc: Vec<u8> = self.buf(obj, "discriminators", path)?;
                let js_alts = array(obj, "variants", path)?;
                if js_alts.len() != alts.len() {
                    return Err(bad(
                        path,
                        "Variant alternative count does not match its type",
                    ));
                }
                let c = counts(&disc, 255, alts.len());
                let variants = alts
                    .iter()
                    .zip(js_alts)
                    .zip(c)
                    .map(|((t, v), n)| self.column(Some(v), t, n, &format!("{path}<{t}>")))
                    .collect::<R<Vec<_>>>()?;
                Column::Variant(
                    VariantColumn::try_new(&disc, variants)
                        .map_err(|e| bad(path, e.to_string()))?,
                )
            }
            ChType::Dynamic { .. } => Column::Dynamic(self.dynamic(obj, path)?),
            ChType::Json { typed_paths, .. } => {
                let body = if str_field(obj, "format") == Some("structured") {
                    JsonColumn::structured(self.structured_json(obj, typed_paths, len, path)?)
                } else {
                    JsonColumn::text(self.utf8(obj.get("text"), path)?)
                };
                Column::Json(body)
            }
            ChType::AggregateFunction { .. } => Column::AggregateState(AggregateStateColumn::new(
                self.buf(obj, "offsets", path)?,
                self.buf(obj, "data", path)?,
            )),
            other => {
                return Err(bad(
                    path,
                    format!("type {other} is not supported for encode"),
                ))
            }
        })
    }

    /// Row count of a dictionary column object, read from its own buffers.
    fn dictionary_len(&self, dict: &Value, ty: &ChType) -> Option<usize> {
        let phys = physical(ty);
        let n = |key: &str, width: usize| self.byte_len(dict, key).map(|b| b / width.max(1));
        match phys.as_ref() {
            ChType::String => n("offsets", 4).map(|o| o.saturating_sub(1)),
            ChType::FixedString(w) => n("data", *w),
            ChType::Uuid | ChType::Ipv6 | ChType::Int128 | ChType::UInt128 => n("data", 16),
            ChType::Int256 | ChType::UInt256 => n("data", 32),
            ChType::Decimal { bits, .. } => n("data", *bits as usize / 8),
            ChType::BFloat16 => n("values", 2),
            other => primitive_width(other).and_then(|w| n("values", w)),
        }
    }

    fn dynamic(&self, obj: &Value, path: &str) -> R<DynamicColumn> {
        let type_ids: Vec<u32> = self.buf(obj, "typeIds", path)?;
        let js_children = array(obj, "children", path)?;
        let c = counts(&type_ids, u64::from(u32::MAX), js_children.len());
        let mut children = Vec::with_capacity(c.len());
        for (child, n) in js_children.iter().zip(c) {
            if str_field(child, "kind") == Some("SharedVariant") {
                children.push(DynamicChild::Shared(self.utf8(Some(child), path)?));
            } else {
                let ty = type_of(child, path)?;
                let p = format!("{path}<{ty}>");
                let values = self.column(Some(child), &ty, n, &p)?;
                children.push(DynamicChild::Typed {
                    ch_type: ty,
                    values,
                });
            }
        }
        DynamicColumn::try_new(&type_ids, children).map_err(|e| bad(path, e.to_string()))
    }

    fn structured_json(
        &self,
        obj: &Value,
        declared: &[(String, ChType)],
        len: usize,
        path: &str,
    ) -> R<StructuredJson> {
        let mut typed = Vec::new();
        for entry in array(obj, "typedPaths", path)? {
            let p = str_field(entry, "path").unwrap_or_default();
            let ty = declared
                .iter()
                .find(|(d, _)| d == p)
                .map(|(_, t)| t)
                .ok_or_else(|| bad(path, format!("'{p}' is not a declared typed path")))?;
            typed.push((
                p.to_owned(),
                self.column(entry.get("column"), ty, len, &format!("{path}.{p}"))?,
            ));
        }
        let mut dynamic_paths = Vec::new();
        for entry in array(obj, "dynamicPaths", path)? {
            let p = str_field(entry, "path").unwrap_or_default();
            let sub = format!("{path}.{p}");
            let col = entry
                .get("column")
                .ok_or_else(|| bad(&sub, "expected a column object"))?;
            dynamic_paths.push((p.to_owned(), self.dynamic(col, &sub)?));
        }
        StructuredJson::try_new(
            typed,
            dynamic_paths,
            self.buf(obj, "sharedOffsets", path)?,
            self.utf8(obj.get("sharedPaths"), path)?,
            self.utf8(obj.get("sharedValues"), path)?,
            len,
        )
        .map_err(|e| bad(path, e.to_string()))
    }
}

/// Encode one Native block for HTTP `INSERT ... FORMAT Native`.
///
/// `block` is `{ rowCount, columns: [{ name, type, ...buffers }] }`, the shape
/// one decoded chunk has. Returns the block's bytes. Concatenate blocks for a
/// multi-block body.
pub(crate) fn encode_native(block: &Value, data: &[u8]) -> R<Vec<u8>> {
    let input = Input { data };
    let rows = block
        .get("rowCount")
        .and_then(Value::as_usize)
        .ok_or_else(|| Error::invalid("block.rowCount must be a non-negative integer"))?;
    let js_cols = array(block, "columns", "block")?;
    let mut fields = Vec::with_capacity(js_cols.len());
    let mut columns = Vec::with_capacity(js_cols.len());
    for (i, c) in js_cols.iter().enumerate() {
        let name = str_field(c, "name")
            .ok_or_else(|| Error::invalid(format!("column #{i} has no 'name'")))?
            .to_owned();
        let ty = type_of(c, &name)?;
        columns.push(input.column(Some(c), &ty, rows, &name)?);
        fields.push(Field { name, ch_type: ty });
    }
    // Not `ColBatch::new`: it debug-asserts that every column has `rows` rows,
    // and this input is untrusted. `encode_block` validates that and every
    // other invariant and returns an error instead.
    let batch = ColBatch {
        schema: Schema::new(fields),
        columns,
        num_rows: rows,
    };
    encode_block(&batch, &EncodeOptions::default()).map_err(|e| Error::invalid(e.to_string()))
}
