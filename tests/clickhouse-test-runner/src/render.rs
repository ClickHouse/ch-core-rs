//! Writes the values that ch-core-rs decoded as the text that the server
//! writes for the `TabSeparated` format. The upstream `.reference` files hold
//! the server's text, so a test passes only when the decoder gave every value
//! correctly.
//!
//! The rendering is type-directed: it follows the [`ChType`] of the column,
//! which the decoder parsed from the block header, and reads the [`Column`]
//! buffers that `CODEC_CONTRACT.md` describes. The buffer alone is not
//! sufficient (for example, a `Date` is a `u16` buffer).
//!
//! ClickHouse uses three text forms:
//! - top level (`serializeTextEscaped`): strings are escaped, not quoted, and
//!   NULL is `\N`;
//! - nested in `Array`, `Tuple` and `Map` (`serializeTextQuoted`): strings
//!   are quoted, and NULL is `NULL`;
//! - plain (`serializeText`, the `TSVRaw` formats): strings as they are.
//!
//! `Variant` and `Dynamic` write the text of the value in the same form.
//! `JSON` writes the JSON text of the object, then escapes or quotes it as one
//! string.

use ch_core_rs::bitmap::Bitmap;
use ch_core_rs::column::{Column, DynamicChild, DynamicColumn, JsonBody, JsonColumn};
use ch_core_rs::native::binary_value::{decode_binary_value, read_binary_type_prefix};
use ch_core_rs::schema::ChType;
use chrono::TimeZone;
use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};

/// `write!` into a `Vec<u8>`; formatting into a `String` cannot fail.
macro_rules! put {
    ($out:expr, $($arg:tt)*) => {{
        let mut s = String::new();
        let _ = write!(s, $($arg)*);
        $out.extend_from_slice(s.as_bytes());
    }};
}

/// The text form of a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Top level of the `TabSeparated` formats: escaped strings, `\N`.
    Escaped,
    /// Top level of the `TSVRaw` formats: strings as they are, `\N`.
    Plain,
    /// Inside `Array`, `Tuple` and `Map`: quoted strings, `NULL`.
    Quoted,
}

/// What the renderer needs besides the type and the column.
#[derive(Debug, Clone, Default)]
pub(crate) struct Context {
    /// The time zone of a `DateTime` or `DateTime64` without a time zone in
    /// its type: the time zone of the query (`session_timezone`, else the
    /// server's). The HTTP response gives it in `X-ClickHouse-Timezone`.
    /// `None` is UTC.
    pub(crate) default_timezone: Option<String>,
}

/// Renders the value of row `row` of a top-level column. With `raw` (the
/// `TSVRaw` formats), the value is written as plain text.
pub(crate) fn render_cell(
    out: &mut Vec<u8>,
    ty: &ChType,
    col: &Column,
    row: usize,
    raw: bool,
    ctx: &Context,
) -> Result<(), String> {
    let mode = if raw { Mode::Plain } else { Mode::Escaped };
    render_value(out, ty, col, row, mode, ctx)
}

fn mismatch(ty: &ChType, col: &Column) -> String {
    format!(
        "clickhouse-test-runner: unexpected column {} for the data type {ty}",
        column_kind(col)
    )
}

fn out_of_range(ty: &ChType, row: usize) -> String {
    format!("clickhouse-test-runner: row {row} is out of range for a column of type {ty}")
}

/// The name of the `Column` variant, for error messages.
fn column_kind(col: &Column) -> &'static str {
    match col {
        Column::Nothing(_) => "Nothing",
        Column::Bool(_) => "Bool",
        Column::Int8(_) => "Int8",
        Column::Int16(_) => "Int16",
        Column::Int32(_) => "Int32",
        Column::Int64(_) => "Int64",
        Column::UInt8(_) => "UInt8",
        Column::UInt16(_) => "UInt16",
        Column::UInt32(_) => "UInt32",
        Column::UInt64(_) => "UInt64",
        Column::Float32(_) => "Float32",
        Column::Float64(_) => "Float64",
        Column::BFloat16(_) => "BFloat16",
        Column::QBit(_) => "QBit",
        Column::Date(_) => "Date",
        Column::Date32(_) => "Date32",
        Column::DateTime(_) => "DateTime",
        Column::DateTime64(_) => "DateTime64",
        Column::Time(_) => "Time",
        Column::Time64(_) => "Time64",
        Column::Interval(_) => "Interval",
        Column::Utf8(_) => "Utf8",
        Column::AggregateState(_) => "AggregateState",
        Column::FixedBinary(_) => "FixedBinary",
        Column::Ipv4(_) => "Ipv4",
        Column::Ipv6(_) => "Ipv6",
        Column::Uuid(_) => "Uuid",
        Column::Enum8(_) => "Enum8",
        Column::Enum16(_) => "Enum16",
        Column::Decimal(_) => "Decimal",
        Column::Int128(_) => "Int128",
        Column::UInt128(_) => "UInt128",
        Column::Int256(_) => "Int256",
        Column::UInt256(_) => "UInt256",
        Column::Dictionary(_) => "Dictionary",
        Column::Array(_) => "Array",
        Column::Tuple(_) => "Tuple",
        Column::Map(_) => "Map",
        Column::Variant(_) => "Variant",
        Column::Dynamic(_) => "Dynamic",
        Column::Json(_) => "Json",
    }
}

/// The validity bitmap of a column (`Nullable(T)` keeps its nulls on the
/// column of `T`).
fn validity(col: &Column) -> Option<&Bitmap> {
    match col {
        Column::Nothing(c) => c.validity.as_ref(),
        Column::Bool(c) => c.validity.as_ref(),
        Column::Int8(c) | Column::Enum8(c) => c.validity.as_ref(),
        Column::Int16(c) | Column::Enum16(c) => c.validity.as_ref(),
        Column::Int32(c) | Column::Date32(c) | Column::Time(c) => c.validity.as_ref(),
        Column::Int64(c) | Column::DateTime64(c) | Column::Time64(c) | Column::Interval(c) => {
            c.validity.as_ref()
        }
        Column::UInt8(c) => c.validity.as_ref(),
        Column::UInt16(c) | Column::Date(c) => c.validity.as_ref(),
        Column::UInt32(c) | Column::DateTime(c) | Column::Ipv4(c) => c.validity.as_ref(),
        Column::UInt64(c) => c.validity.as_ref(),
        Column::Float32(c) => c.validity.as_ref(),
        Column::Float64(c) => c.validity.as_ref(),
        Column::BFloat16(c) => c.validity.as_ref(),
        Column::QBit(c) => c.validity.as_ref(),
        Column::Utf8(c) => c.validity.as_ref(),
        Column::FixedBinary(c)
        | Column::Ipv6(c)
        | Column::Uuid(c)
        | Column::Int128(c)
        | Column::UInt128(c)
        | Column::Int256(c)
        | Column::UInt256(c) => c.validity.as_ref(),
        Column::Decimal(c) => c.validity.as_ref(),
        Column::Dictionary(c) => c.validity.as_ref(),
        Column::Tuple(c) => c.validity.as_ref(),
        Column::Json(c) => c.validity.as_ref(),
        Column::AggregateState(_)
        | Column::Array(_)
        | Column::Map(_)
        | Column::Variant(_)
        | Column::Dynamic(_) => None,
    }
}

fn write_null(out: &mut Vec<u8>, mode: Mode) {
    out.extend_from_slice(if mode == Mode::Quoted {
        b"NULL"
    } else {
        b"\\N"
    });
}

/// Gets element `row` of a typed buffer, or an error.
fn at<'a, T>(values: &'a [T], row: usize, ty: &ChType) -> Result<&'a T, String> {
    values.get(row).ok_or_else(|| out_of_range(ty, row))
}

/// Gets the bytes of row `row` of a fixed-width buffer.
fn fixed_at<'a>(data: &'a [u8], width: usize, row: usize, ty: &ChType) -> Result<&'a [u8], String> {
    let start = row
        .checked_mul(width)
        .ok_or_else(|| out_of_range(ty, row))?;
    data.get(start..start + width)
        .ok_or_else(|| out_of_range(ty, row))
}

/// Gets the range of the elements of row `row` from Arrow list offsets.
fn list_range(offsets: &[i64], row: usize, ty: &ChType) -> Result<(usize, usize), String> {
    let (Some(&start), Some(&end)) = (offsets.get(row), offsets.get(row + 1)) else {
        return Err(out_of_range(ty, row));
    };
    match (usize::try_from(start), usize::try_from(end)) {
        (Ok(s), Ok(e)) if s <= e => Ok((s, e)),
        _ => Err(format!(
            "clickhouse-test-runner: bad offsets {start}..{end} in row {row} of a column of type {ty}"
        )),
    }
}

/// Renders the value of row `row` of `col`, which has the type `ty`.
pub(crate) fn render_value(
    out: &mut Vec<u8>,
    ty: &ChType,
    col: &Column,
    row: usize,
    mode: Mode,
    ctx: &Context,
) -> Result<(), String> {
    use ChType as T;
    // SimpleAggregateFunction, the geo types, Geometry and Nested have the
    // column and the text of their underlying type.
    if let Some(delegate) = ty.physical_delegate() {
        return render_value(out, &delegate, col, row, mode, ctx);
    }
    match ty {
        T::Nullable(inner) => {
            if validity(col).is_some_and(|v| !v.is_valid(row)) {
                write_null(out, mode);
                return Ok(());
            }
            return render_value(out, inner, col, row, mode, ctx);
        }
        T::LowCardinality(inner) => {
            let Column::Dictionary(dict) = col else {
                return Err(mismatch(ty, col));
            };
            if dict.validity.as_ref().is_some_and(|v| !v.is_valid(row)) {
                write_null(out, mode);
                return Ok(());
            }
            let index = *at(&dict.indices, row, ty)?;
            let index = usize::try_from(index).map_err(|_| {
                format!("clickhouse-test-runner: negative dictionary index {index} in a column of type {ty}")
            })?;
            // The dictionary keeps the nulls in `validity` above; its values
            // have the type without Nullable.
            return render_value(out, inner.inner(), &dict.values, index, mode, ctx);
        }
        T::Nothing => {
            // A `Nothing` column has no values; a value here is a NULL.
            write_null(out, mode);
            return Ok(());
        }
        _ => {}
    }
    match (ty, col) {
        (T::Bool, Column::Bool(c)) => {
            if row >= c.len {
                return Err(out_of_range(ty, row));
            }
            out.extend_from_slice(if c.get(row) { b"true" } else { b"false" })
        }
        (T::Int8, Column::Int8(c)) => put!(out, "{}", at(&c.values, row, ty)?),
        (T::Int16, Column::Int16(c)) => put!(out, "{}", at(&c.values, row, ty)?),
        (T::Int32, Column::Int32(c)) => put!(out, "{}", at(&c.values, row, ty)?),
        (T::Int64, Column::Int64(c)) => put!(out, "{}", at(&c.values, row, ty)?),
        (T::UInt8, Column::UInt8(c)) => put!(out, "{}", at(&c.values, row, ty)?),
        (T::UInt16, Column::UInt16(c)) => put!(out, "{}", at(&c.values, row, ty)?),
        (T::UInt32, Column::UInt32(c)) => put!(out, "{}", at(&c.values, row, ty)?),
        (T::UInt64, Column::UInt64(c)) => put!(out, "{}", at(&c.values, row, ty)?),
        (T::Interval(_), Column::Interval(c)) => put!(out, "{}", at(&c.values, row, ty)?),
        (T::Float32, Column::Float32(c)) => {
            out.extend_from_slice(format_float32(*at(&c.values, row, ty)?).as_bytes())
        }
        (T::Float64, Column::Float64(c)) => {
            out.extend_from_slice(format_float64(*at(&c.values, row, ty)?).as_bytes())
        }
        (T::BFloat16, Column::BFloat16(c)) => {
            let f = bfloat16_to_f32(*at(&c.values, row, ty)?);
            out.extend_from_slice(format_float32(f).as_bytes())
        }
        (T::Int128 | T::UInt128 | T::Int256 | T::UInt256, _) => {
            let (signed, c) = match (ty, col) {
                (T::Int128, Column::Int128(c)) | (T::Int256, Column::Int256(c)) => (true, c),
                (T::UInt128, Column::UInt128(c)) | (T::UInt256, Column::UInt256(c)) => (false, c),
                _ => return Err(mismatch(ty, col)),
            };
            let bytes = fixed_at(&c.data, c.width, row, ty)?;
            let (neg, digits) = wide_digits(&widen(bytes, signed)?, signed);
            if neg {
                out.push(b'-');
            }
            out.extend_from_slice(digits.as_bytes())
        }
        (T::Decimal { scale, .. }, Column::Decimal(c)) => {
            let bytes = fixed_at(&c.data, c.width, row, ty)?;
            let (neg, digits) = wide_digits(&widen(bytes, true)?, true);
            out.extend_from_slice(decimal_text(neg, digits, *scale).as_bytes())
        }
        (T::String, Column::Utf8(c)) => {
            if row >= c.len() {
                return Err(out_of_range(ty, row));
            }
            write_string(out, c.value(row), mode)
        }
        (T::FixedString(_), Column::FixedBinary(c)) => {
            write_string(out, fixed_at(&c.data, c.width, row, ty)?, mode)
        }
        (T::Enum8 { variants }, Column::Enum8(c)) => {
            let v = *at(&c.values, row, ty)?;
            let name = variants
                .iter()
                .find(|(_, n)| *n == v)
                .map(|(name, _)| name)
                .ok_or_else(|| {
                    format!("clickhouse-test-runner: no name for the value {v} of {ty}")
                })?;
            write_string(out, name.as_bytes(), mode)
        }
        (T::Enum16 { variants }, Column::Enum16(c)) => {
            let v = *at(&c.values, row, ty)?;
            let name = variants
                .iter()
                .find(|(_, n)| *n == v)
                .map(|(name, _)| name)
                .ok_or_else(|| {
                    format!("clickhouse-test-runner: no name for the value {v} of {ty}")
                })?;
            write_string(out, name.as_bytes(), mode)
        }
        (T::Uuid, Column::Uuid(c)) => {
            let text = format_uuid(fixed_at(&c.data, c.width, row, ty)?)?;
            write_quoted_if(out, &text, mode)
        }
        (T::Ipv4, Column::Ipv4(c)) => {
            let ip = Ipv4Addr::from(*at(&c.values, row, ty)?);
            write_quoted_if(out, &ip.to_string(), mode)
        }
        (T::Ipv6, Column::Ipv6(c)) => {
            let bytes: [u8; 16] = fixed_at(&c.data, c.width, row, ty)?
                .try_into()
                .map_err(|_| mismatch(ty, col))?;
            write_quoted_if(out, &format_ipv6(&Ipv6Addr::from(bytes)), mode)
        }
        (T::Date, Column::Date(c)) => {
            let days = i64::from(*at(&c.values, row, ty)?);
            write_quoted_if(out, &format_date(days), mode)
        }
        (T::Date32, Column::Date32(c)) => {
            let days = i64::from(*at(&c.values, row, ty)?).clamp(DATE32_MIN_DAYS, DATE32_MAX_DAYS);
            write_quoted_if(out, &format_date(days), mode)
        }
        (T::DateTime { timezone }, Column::DateTime(c)) => {
            let secs = i64::from(*at(&c.values, row, ty)?);
            let text = format_datetime(secs, 0, 0, timezone.as_deref(), ctx)?;
            write_quoted_if(out, &text, mode)
        }
        (
            T::DateTime64 {
                precision,
                timezone,
            },
            Column::DateTime64(c),
        ) => {
            let p = u32::from(*precision);
            let scale = 10i64
                .checked_pow(p)
                .ok_or_else(|| format!("clickhouse-test-runner: precision of {ty}"))?;
            let ticks = *at(&c.values, row, ty)?;
            let text = format_datetime(
                ticks.div_euclid(scale),
                ticks.rem_euclid(scale),
                p,
                timezone.as_deref(),
                ctx,
            )?;
            write_quoted_if(out, &text, mode)
        }
        (T::Time, Column::Time(c)) => {
            let secs = i64::from(*at(&c.values, row, ty)?);
            write_quoted_if(out, &format_time(secs, 0), mode)
        }
        (T::Time64 { precision }, Column::Time64(c)) => {
            let ticks = *at(&c.values, row, ty)?;
            write_quoted_if(out, &format_time(ticks, u32::from(*precision)), mode)
        }
        (
            T::QBit {
                element_type,
                dimension,
            },
            Column::QBit(c),
        ) => {
            let element = element_type.ch_type();
            let start = row
                .checked_mul(*dimension)
                .ok_or_else(|| out_of_range(ty, row))?;
            if c.dimension != *dimension || start + dimension > c.values.len() {
                return Err(out_of_range(ty, row));
            }
            out.push(b'[');
            for i in 0..*dimension {
                if i > 0 {
                    out.push(b',');
                }
                render_value(out, &element, &c.values, start + i, Mode::Quoted, ctx)?;
            }
            out.push(b']');
        }
        (T::Array(inner), Column::Array(c)) => {
            let (start, end) = list_range(&c.offsets, row, ty)?;
            out.push(b'[');
            for (n, i) in (start..end).enumerate() {
                if n > 0 {
                    out.push(b',');
                }
                render_value(out, inner, &c.values, i, Mode::Quoted, ctx)?;
            }
            out.push(b']');
        }
        (T::Tuple(fields), Column::Tuple(c)) => {
            if fields.len() != c.fields.len() {
                return Err(mismatch(ty, col));
            }
            if row >= c.len {
                return Err(out_of_range(ty, row));
            }
            out.push(b'(');
            for (i, ((_, t), field)) in fields.iter().zip(&c.fields).enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                render_value(out, t, field, row, Mode::Quoted, ctx)?;
            }
            out.push(b')');
        }
        (T::Map(kt, vt), Column::Map(c)) => {
            let (start, end) = list_range(&c.offsets, row, ty)?;
            let Column::Tuple(entries) = c.entries.as_ref() else {
                return Err(mismatch(ty, col));
            };
            let [keys, values] = entries.fields.as_slice() else {
                return Err(mismatch(ty, col));
            };
            out.push(b'{');
            for (n, i) in (start..end).enumerate() {
                if n > 0 {
                    out.push(b',');
                }
                render_value(out, kt, keys, i, Mode::Quoted, ctx)?;
                out.push(b':');
                render_value(out, vt, values, i, Mode::Quoted, ctx)?;
            }
            out.push(b'}');
        }
        (T::Variant(types), Column::Variant(c)) => {
            let (disc, offset) = c.value_position(row).ok_or_else(|| out_of_range(ty, row))?;
            if disc == u8::MAX {
                write_null(out, mode);
                return Ok(());
            }
            let i = usize::from(disc);
            let (Some(t), Some(child)) = (types.get(i), c.variants.get(i)) else {
                return Err(mismatch(ty, col));
            };
            render_value(out, t, child, offset as usize, mode, ctx)?;
        }
        (T::Dynamic { .. }, Column::Dynamic(c)) => render_dynamic(out, ty, c, row, mode, ctx)?,
        (T::AggregateFunction { .. }, Column::AggregateState(c)) => {
            if row >= c.len() {
                return Err(out_of_range(ty, row));
            }
            write_string(out, c.value(row), mode)
        }
        (T::Json { .. }, Column::Json(c)) => render_json(out, ty, c, row, mode, ctx)?,
        _ => return Err(mismatch(ty, col)),
    }
    Ok(())
}

/// Renders a `Dynamic` value: the text of the value, as `Variant` does
/// (`SerializationDynamic`). A value in the shared variant is a binary type
/// descriptor and one binary value.
fn render_dynamic(
    out: &mut Vec<u8>,
    ty: &ChType,
    c: &DynamicColumn,
    row: usize,
    mode: Mode,
    ctx: &Context,
) -> Result<(), String> {
    let (type_id, offset) = c.value_position(row).ok_or_else(|| out_of_range(ty, row))?;
    if type_id == u32::MAX {
        write_null(out, mode);
        return Ok(());
    }
    let child = c
        .children
        .get(type_id as usize)
        .ok_or_else(|| out_of_range(ty, row))?;
    match child {
        DynamicChild::Typed { ch_type, values } => {
            render_value(out, ch_type, values, offset as usize, mode, ctx)
        }
        DynamicChild::Shared(cells) => {
            let cell = cells.value(offset as usize);
            let (value_type, consumed) = read_binary_type_prefix(cell).map_err(|e| {
                format!("ch-core-rs: read the type of a Dynamic shared-variant value: {e}")
            })?;
            let value = decode_binary_value(&value_type, &cell[consumed..]).map_err(|e| {
                format!(
                    "ch-core-rs: decode a Dynamic shared-variant value of type {value_type}: {e}"
                )
            })?;
            render_value(out, &value_type, &value, 0, mode, ctx)
        }
    }
}

/// Renders a `JSON` value: the JSON text of the object, escaped or quoted as
/// one string (`SerializationObject`).
fn render_json(
    out: &mut Vec<u8>,
    ty: &ChType,
    c: &JsonColumn,
    row: usize,
    mode: Mode,
    ctx: &Context,
) -> Result<(), String> {
    if c.validity.as_ref().is_some_and(|v| !v.is_valid(row)) {
        write_null(out, mode);
        return Ok(());
    }
    let mut text = Vec::new();
    match c.body() {
        JsonBody::Text(docs) => {
            if row >= docs.len() {
                return Err(out_of_range(ty, row));
            }
            text.extend_from_slice(docs.value(row));
        }
        JsonBody::Structured(_) => write_json_object(&mut text, ty, c, row, ctx)?,
    }
    write_string(out, &text, mode);
    Ok(())
}

/// Writes the JSON object of row `row`. The server writes the paths in
/// sorted order, splits them at `.` into nested objects, writes every typed
/// path (NULL as `null`), and leaves out a dynamic or shared path whose value
/// is NULL.
fn write_json_object(
    out: &mut Vec<u8>,
    ty: &ChType,
    c: &JsonColumn,
    row: usize,
    ctx: &Context,
) -> Result<(), String> {
    let (ChType::Json { typed_paths, .. }, JsonBody::Structured(body)) = (ty, c.body()) else {
        return Err(format!(
            "clickhouse-test-runner: unexpected JSON column for the data type {ty}"
        ));
    };
    if row >= body.len {
        return Err(out_of_range(ty, row));
    }
    let mut entries: Vec<(&[u8], Vec<u8>)> = Vec::new();
    if typed_paths.len() != body.typed.len() {
        return Err(format!(
            "ch-core-rs: {} typed paths in the column of type {ty}",
            body.typed.len()
        ));
    }
    for ((path, t), (_, col)) in typed_paths.iter().zip(&body.typed) {
        let mut v = Vec::new();
        write_json_value(&mut v, t, col, row, ctx)?;
        entries.push((path.as_bytes(), v));
    }
    let dynamic_type = ChType::Dynamic { max_types: 0 };
    for (path, col) in &body.dynamic {
        if col
            .value_position(row)
            .is_some_and(|(type_id, _)| type_id == u32::MAX)
        {
            continue;
        }
        let mut v = Vec::new();
        write_json_dynamic(&mut v, &dynamic_type, col, row, ctx)?;
        entries.push((path.as_bytes(), v));
    }
    let (start, end) = list_range(&body.shared_offsets, row, ty)?;
    for i in start..end {
        if i >= body.shared_paths.len() || i >= body.shared_values.len() {
            return Err(out_of_range(ty, row));
        }
        let cell = body.shared_values.value(i);
        let (value_type, consumed) = read_binary_type_prefix(cell)
            .map_err(|e| format!("ch-core-rs: read the type of a JSON shared-data value: {e}"))?;
        if value_type == ChType::Nothing {
            continue;
        }
        let value = decode_binary_value(&value_type, &cell[consumed..]).map_err(|e| {
            format!("ch-core-rs: decode a JSON shared-data value of type {value_type}: {e}")
        })?;
        let mut v = Vec::new();
        write_json_value(&mut v, &value_type, &value, 0, ctx)?;
        entries.push((body.shared_paths.value(i), v));
    }
    entries.sort_by(|a, b| a.0.cmp(b.0));

    // Open objects of the path prefixes; one "first member" flag per level.
    let mut open: Vec<&[u8]> = Vec::new();
    let mut first = vec![true];
    out.push(b'{');
    for (path, value) in &entries {
        let segments: Vec<&[u8]> = path.split(|&c| c == b'.').collect();
        let (leaf, prefix) = segments
            .split_last()
            .ok_or_else(|| "clickhouse-test-runner: empty JSON path".to_owned())?;
        let common = open.iter().zip(prefix).take_while(|(a, b)| a == b).count();
        while open.len() > common {
            open.pop();
            first.pop();
            out.push(b'}');
        }
        for segment in &prefix[common..] {
            json_member(out, &mut first, segment);
            out.push(b'{');
            open.push(segment);
            first.push(true);
        }
        json_member(out, &mut first, leaf);
        out.extend_from_slice(value);
    }
    for _ in &open {
        out.push(b'}');
    }
    out.push(b'}');
    Ok(())
}

/// Writes `"key":` of an object member, after a `,` when it is not the first.
fn json_member(out: &mut Vec<u8>, first: &mut [bool], key: &[u8]) {
    if let Some(f) = first.last_mut() {
        if !*f {
            out.push(b',');
        }
        *f = false;
    }
    write_json_string(out, key);
    out.push(b':');
}

fn write_json_dynamic(
    out: &mut Vec<u8>,
    ty: &ChType,
    c: &DynamicColumn,
    row: usize,
    ctx: &Context,
) -> Result<(), String> {
    let (type_id, offset) = c.value_position(row).ok_or_else(|| out_of_range(ty, row))?;
    if type_id == u32::MAX {
        out.extend_from_slice(b"null");
        return Ok(());
    }
    match c.children.get(type_id as usize) {
        Some(DynamicChild::Typed { ch_type, values }) => {
            write_json_value(out, ch_type, values, offset as usize, ctx)
        }
        Some(DynamicChild::Shared(cells)) => {
            let cell = cells.value(offset as usize);
            let (value_type, consumed) = read_binary_type_prefix(cell).map_err(|e| {
                format!("ch-core-rs: read the type of a Dynamic shared-variant value: {e}")
            })?;
            let value = decode_binary_value(&value_type, &cell[consumed..]).map_err(|e| {
                format!(
                    "ch-core-rs: decode a Dynamic shared-variant value of type {value_type}: {e}"
                )
            })?;
            write_json_value(out, &value_type, &value, 0, ctx)
        }
        None => Err(out_of_range(ty, row)),
    }
}

/// Writes one value as the server writes it in the JSON text of a `JSON`
/// object (`serializeTextJSON`): numbers, `Decimal` and `Bool` bare, every
/// other scalar as a JSON string of its text, `NULL` as `null`, named tuples
/// and maps as objects, and arrays and unnamed tuples as arrays.
fn write_json_value(
    out: &mut Vec<u8>,
    ty: &ChType,
    col: &Column,
    row: usize,
    ctx: &Context,
) -> Result<(), String> {
    use ChType as T;
    if let Some(delegate) = ty.physical_delegate() {
        return write_json_value(out, &delegate, col, row, ctx);
    }
    match (ty, col) {
        (T::Nullable(inner), _) => {
            if validity(col).is_some_and(|v| !v.is_valid(row)) {
                out.extend_from_slice(b"null");
                return Ok(());
            }
            write_json_value(out, inner, col, row, ctx)
        }
        (T::LowCardinality(inner), Column::Dictionary(dict)) => {
            if dict.validity.as_ref().is_some_and(|v| !v.is_valid(row)) {
                out.extend_from_slice(b"null");
                return Ok(());
            }
            let index =
                usize::try_from(*at(&dict.indices, row, ty)?).map_err(|_| out_of_range(ty, row))?;
            write_json_value(out, inner.inner(), &dict.values, index, ctx)
        }
        (T::Nothing, _) => {
            out.extend_from_slice(b"null");
            Ok(())
        }
        (
            T::Bool
            | T::Int8
            | T::Int16
            | T::Int32
            | T::Int64
            | T::UInt8
            | T::UInt16
            | T::UInt32
            | T::UInt64
            | T::Int128
            | T::UInt128
            | T::Int256
            | T::UInt256
            | T::Decimal { .. }
            | T::Interval(_),
            _,
        ) => render_value(out, ty, col, row, Mode::Plain, ctx),
        (T::Float32 | T::Float64 | T::BFloat16, _) => {
            let mut text = Vec::new();
            render_value(&mut text, ty, col, row, Mode::Plain, ctx)?;
            // JSON has no NaN or infinity.
            if matches!(text.as_slice(), b"nan" | b"inf" | b"-inf") {
                out.extend_from_slice(b"null");
            } else {
                out.extend_from_slice(&text);
            }
            Ok(())
        }
        (T::Array(inner), Column::Array(c)) => {
            let (start, end) = list_range(&c.offsets, row, ty)?;
            out.push(b'[');
            for (n, i) in (start..end).enumerate() {
                if n > 0 {
                    out.push(b',');
                }
                write_json_value(out, inner, &c.values, i, ctx)?;
            }
            out.push(b']');
            Ok(())
        }
        (T::QBit { .. }, _) => render_value(out, ty, col, row, Mode::Plain, ctx),
        (T::Tuple(fields), Column::Tuple(c)) => {
            if fields.len() != c.fields.len() || row >= c.len {
                return Err(mismatch(ty, col));
            }
            let named = !fields.is_empty() && fields.iter().all(|(name, _)| name.is_some());
            out.push(if named { b'{' } else { b'[' });
            for (i, ((name, t), field)) in fields.iter().zip(&c.fields).enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                if let Some(name) = name.as_deref().filter(|_| named) {
                    write_json_string(out, name.as_bytes());
                    out.push(b':');
                }
                write_json_value(out, t, field, row, ctx)?;
            }
            out.push(if named { b'}' } else { b']' });
            Ok(())
        }
        (T::Map(kt, vt), Column::Map(c)) => {
            let (start, end) = list_range(&c.offsets, row, ty)?;
            let Column::Tuple(entries) = c.entries.as_ref() else {
                return Err(mismatch(ty, col));
            };
            let [keys, values] = entries.fields.as_slice() else {
                return Err(mismatch(ty, col));
            };
            out.push(b'{');
            for (n, i) in (start..end).enumerate() {
                if n > 0 {
                    out.push(b',');
                }
                let mut key = Vec::new();
                render_value(&mut key, kt, keys, i, Mode::Plain, ctx)?;
                write_json_string(out, &key);
                out.push(b':');
                write_json_value(out, vt, values, i, ctx)?;
            }
            out.push(b'}');
            Ok(())
        }
        (T::Variant(types), Column::Variant(c)) => {
            let (disc, offset) = c.value_position(row).ok_or_else(|| out_of_range(ty, row))?;
            if disc == u8::MAX {
                out.extend_from_slice(b"null");
                return Ok(());
            }
            let i = usize::from(disc);
            match (types.get(i), c.variants.get(i)) {
                (Some(t), Some(child)) => write_json_value(out, t, child, offset as usize, ctx),
                _ => Err(mismatch(ty, col)),
            }
        }
        (T::Dynamic { .. }, Column::Dynamic(c)) => write_json_dynamic(out, ty, c, row, ctx),
        (T::Json { .. }, Column::Json(c)) => {
            if c.validity.as_ref().is_some_and(|v| !v.is_valid(row)) {
                out.extend_from_slice(b"null");
                return Ok(());
            }
            match c.body() {
                JsonBody::Text(docs) => {
                    out.extend_from_slice(docs.value(row));
                    Ok(())
                }
                JsonBody::Structured(_) => write_json_object(out, ty, c, row, ctx),
            }
        }
        _ => {
            // Strings, enums, dates, times, UUIDs and addresses: a JSON
            // string of the text.
            let mut text = Vec::new();
            render_value(&mut text, ty, col, row, Mode::Plain, ctx)?;
            write_json_string(out, &text);
            Ok(())
        }
    }
}

/// Writes a JSON string as ClickHouse's `writeJSONString` does with the
/// default settings: `/` is escaped, control characters are `\uXXXX` (or
/// their short escape), U+2028 and U+2029 are escaped, and other bytes
/// (also bytes that are not UTF-8) are written as they are.
fn write_json_string(out: &mut Vec<u8>, s: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push(b'"');
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'/' => out.extend_from_slice(b"\\/"),
            b'\x08' => out.extend_from_slice(b"\\b"),
            b'\x0c' => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0..=0x1f => {
                out.extend_from_slice(b"\\u00");
                out.push(HEX[usize::from(c >> 4)]);
                out.push(HEX[usize::from(c & 0xf)]);
            }
            0xe2 if s.get(i + 1) == Some(&0x80) && matches!(s.get(i + 2), Some(0xa8 | 0xa9)) => {
                out.extend_from_slice(if s[i + 2] == 0xa8 {
                    b"\\u2028"
                } else {
                    b"\\u2029"
                });
                i += 2;
            }
            _ => out.push(c),
        }
        i += 1;
    }
    out.push(b'"');
}

/// Writes `s` in the text form `mode`: with the escapes of ClickHouse's
/// `writeAnyEscapedString` (with the single quote as the quote character),
/// also quoted when nested; or as it is.
pub(crate) fn write_string(out: &mut Vec<u8>, s: &[u8], mode: Mode) {
    if mode == Mode::Plain {
        out.extend_from_slice(s);
        return;
    }
    if mode == Mode::Quoted {
        out.push(b'\'');
    }
    for &c in s {
        match c {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\'' => out.extend_from_slice(b"\\'"),
            b'\x08' => out.extend_from_slice(b"\\b"),
            b'\x0c' => out.extend_from_slice(b"\\f"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0 => out.extend_from_slice(b"\\0"),
            _ => out.push(c),
        }
    }
    if mode == Mode::Quoted {
        out.push(b'\'');
    }
}

/// Writes text that has no characters to escape (dates, UUIDs, addresses),
/// quoted when nested.
fn write_quoted_if(out: &mut Vec<u8>, s: &str, mode: Mode) {
    if mode == Mode::Quoted {
        out.push(b'\'');
    }
    out.extend_from_slice(s.as_bytes());
    if mode == Mode::Quoted {
        out.push(b'\'');
    }
}

/// A BFloat16 is the high half of a Float32 (little-endian on the wire).
fn bfloat16_to_f32(bytes: [u8; 2]) -> f32 {
    f32::from_bits(u32::from(u16::from_le_bytes(bytes)) << 16)
}

/// Writes a float as ClickHouse does: the shortest text that reads back as
/// the same value, in positional notation when the decimal exponent is in
/// [-6, 21), else in scientific notation without a `+` sign or exponent
/// padding (`1e21`, `1.5e-7`). This is the layout of JavaScript's
/// `Number.prototype.toString`.
pub(crate) fn format_float64(f: f64) -> String {
    // The server writes `nan` also for a NaN with the sign bit set (the
    // result of `0/0`, for example).
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.into();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0" } else { "0" }.into();
    }
    // `{:e}` writes the shortest digits that read back as the same value.
    layout_float(&format!("{f:e}"))
}

pub(crate) fn format_float32(f: f32) -> String {
    if f.is_nan() || f.is_infinite() || f == 0.0 {
        return format_float64(f64::from(f));
    }
    layout_float(&format!("{f:e}"))
}

/// Lays out `d.ddddde-X` (the output of `{:e}`) as JavaScript does.
fn layout_float(sci: &str) -> String {
    let (neg, s) = match sci.strip_prefix('-') {
        Some(s) => (true, s),
        None => (false, sci),
    };
    let (mantissa, exp) = s.split_once('e').unwrap_or((s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|&c| c != '.').collect();
    // The value is 0.digits x 10^n.
    let n = exp + 1;
    let k = digits.len() as i32;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let mut t = digits[..1].to_owned();
        if k > 1 {
            t.push('.');
            t.push_str(&digits[1..]);
        }
        format!("{t}e{}", n - 1)
    };
    if neg { format!("-{body}") } else { body }
}

/// Writes a decimal from its sign, the digits of its absolute unscaled value
/// and its scale, without trailing zeros (`1.5`, not `1.50`).
pub(crate) fn decimal_text(neg: bool, digits: String, scale: u8) -> String {
    let scale = usize::from(scale);
    let digits = if digits.len() <= scale {
        format!("{}{digits}", "0".repeat(scale + 1 - digits.len()))
    } else {
        digits
    };
    let (int, frac) = digits.split_at(digits.len() - scale);
    let frac = frac.trim_end_matches('0');
    let is_zero = int.bytes().all(|c| c == b'0') && frac.is_empty();
    let mut out = String::new();
    if neg && !is_zero {
        out.push('-');
    }
    out.push_str(int);
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out
}

/// Extends a little-endian integer of 4, 8, 16 or 32 bytes to 32 bytes.
fn widen(bytes: &[u8], signed: bool) -> Result<[u8; 32], String> {
    if !matches!(bytes.len(), 4 | 8 | 16 | 32) {
        return Err(format!(
            "clickhouse-test-runner: an integer of {} bytes",
            bytes.len()
        ));
    }
    let fill = if signed && bytes.last().is_some_and(|b| b & 0x80 != 0) {
        0xff
    } else {
        0
    };
    let mut wide = [fill; 32];
    wide[..bytes.len()].copy_from_slice(bytes);
    Ok(wide)
}

/// Returns the sign and the decimal digits of the absolute value of a 256-bit
/// little-endian integer.
fn wide_digits(bytes: &[u8; 32], signed: bool) -> (bool, String) {
    let mut limbs = [0u64; 4];
    for (limb, chunk) in limbs.iter_mut().zip(bytes.chunks_exact(8)) {
        let mut b = [0u8; 8];
        b.copy_from_slice(chunk);
        *limb = u64::from_le_bytes(b);
    }
    let neg = signed && limbs[3] >> 63 == 1;
    if neg {
        // Two's complement: invert and add one.
        let mut carry = true;
        for limb in &mut limbs {
            *limb = !*limb;
            if carry {
                let (v, c) = limb.overflowing_add(1);
                *limb = v;
                carry = c;
            }
        }
    }
    const CHUNK: u64 = 10_000_000_000_000_000_000; // 10^19
    let mut parts = Vec::new();
    while limbs.iter().any(|&l| l != 0) {
        let mut rem: u128 = 0;
        for limb in limbs.iter_mut().rev() {
            let cur = (rem << 64) | u128::from(*limb);
            *limb = (cur / u128::from(CHUNK)) as u64;
            rem = cur % u128::from(CHUNK);
        }
        parts.push(rem as u64);
    }
    let mut s = String::new();
    match parts.split_last() {
        None => s.push('0'),
        Some((first, rest)) => {
            let _ = write!(s, "{first}");
            for p in rest.iter().rev() {
                let _ = write!(s, "{p:019}");
            }
        }
    }
    (neg, s)
}

/// A UUID is two little-endian `UInt64` halves on the wire, the high half
/// first (`SerializationUUID`).
fn format_uuid(bytes: &[u8]) -> Result<String, String> {
    let (Some(hi), Some(lo)) = (bytes.get(0..8), bytes.get(8..16)) else {
        return Err(format!(
            "clickhouse-test-runner: a UUID of {} bytes",
            bytes.len()
        ));
    };
    let half = |b: &[u8]| {
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        u64::from_le_bytes(a)
    };
    let hex = format!("{:016x}{:016x}", half(hi), half(lo));
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// Converts days since 1970-01-01 to a civil date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 0000-01-01 and 9999-12-31 in days since 1970-01-01. The server writes a
/// `Date32` value outside this range as the nearest end of it
/// (`DATE_LUT_MIN_EXTEND_DAY_NUM`, `DATE_LUT_MAX_EXTEND_DAY_NUM` in
/// `DateLUTImpl.h`); the value itself is not changed, and the Native data has
/// it as it is (for example `fromDaysSinceYearZero32(2147483648)`).
const DATE32_MIN_DAYS: i64 = -719_528;
const DATE32_MAX_DAYS: i64 = 2_932_896;

fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Writes `secs` (seconds since the epoch) in the time zone `tz` (or the
/// default time zone), with `precision` digits of `frac`.
fn format_datetime(
    secs: i64,
    frac: i64,
    precision: u32,
    tz: Option<&str>,
    ctx: &Context,
) -> Result<String, String> {
    let zone = match tz.or(ctx.default_timezone.as_deref()) {
        Some(name) => Some(parse_timezone(name)?),
        None => None,
    };
    // The server writes a value outside 0000-01-01 00:00:00 ..
    // 9999-12-31 23:59:59 (in the time zone of the type) as the nearest end
    // of that range, with the fraction of the value.
    const MIN_SECS: i64 = -62_167_219_200;
    const MAX_SECS: i64 = 253_402_300_799;
    const MARGIN: i64 = 2 * 86_400;
    let clamped = secs.clamp(MIN_SECS - MARGIN, MAX_SECS + MARGIN);
    let naive = match zone {
        None => chrono::DateTime::from_timestamp(clamped, 0).map(|t| t.naive_utc()),
        Some(Zone::Named(zone)) => named_offset(zone, clamped)
            .and_then(|offset| chrono::DateTime::from_timestamp(clamped + i64::from(offset), 0))
            .map(|t| t.naive_utc()),
        Some(Zone::Fixed(offset)) => offset
            .timestamp_opt(clamped, 0)
            .single()
            .map(|t| t.naive_local()),
    }
    .ok_or_else(|| format!("clickhouse-test-runner: timestamp {secs} is out of range"))?;
    let range = |s: i64| chrono::DateTime::from_timestamp(s, 0).map(|t| t.naive_utc());
    let (Some(min), Some(max)) = (range(MIN_SECS), range(MAX_SECS)) else {
        unreachable!("the range ends are valid timestamps");
    };
    let naive = naive.clamp(min, max);
    let mut s = naive.format("%Y-%m-%d %H:%M:%S").to_string();
    if precision > 0 {
        let _ = write!(s, ".{frac:0width$}", width = precision as usize);
    }
    Ok(s)
}

/// 2100-01-01 00:00:00 UTC. chrono-tz has the transitions of a time zone up
/// to the end of 2099 and uses the last offset after that. The server
/// (`cctz`) applies the rule of the zone (the POSIX TZ string of the tzfile)
/// to every later year, for example the daylight saving time of
/// `Europe/Amsterdam` in July 2105.
const END_OF_TRANSITIONS: i64 = 4_102_444_800;

/// Returns the UTC offset in seconds of `zone` at `secs`. From 2100, it is
/// the offset at the same time of a year in 2072..=2099 with the same
/// calendar (leap year, and weekday of January 1): the rule of the zone gives
/// the same transitions in both years.
fn named_offset(zone: chrono_tz::Tz, secs: i64) -> Option<i32> {
    use chrono::{Datelike, Offset};
    let mut probe = secs;
    if secs >= END_OF_TRANSITIONS {
        let is_leap = |y: i32| y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let jan1 = |y: i32| chrono::NaiveDate::from_ymd_opt(y, 1, 1);
        let year = chrono::DateTime::from_timestamp(secs, 0)?.year();
        let target = jan1(year)?;
        let same = (2072..=2099)
            .filter_map(jan1)
            .find(|d| is_leap(d.year()) == is_leap(year) && d.weekday() == target.weekday())?;
        probe -= (target - same).num_seconds();
    }
    let utc = chrono::DateTime::from_timestamp(probe, 0)?.naive_utc();
    Some(zone.offset_from_utc_datetime(&utc).fix().local_minus_utc())
}

/// The time zone of a `DateTime` or `DateTime64` type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Zone {
    /// A name of the time-zone database.
    Named(chrono_tz::Tz),
    /// A fixed offset from UTC: `Fixed/UTC+-HH:MM:SS`.
    Fixed(chrono::FixedOffset),
}

/// The prefix of the names of fixed-offset time zones.
const FIXED_ZONE_PREFIX: &str = "Fixed/UTC";

/// Parses the time-zone name of a `DateTime` or `DateTime64` type.
///
/// The server (`cctz`, `FixedOffsetFromName`) also accepts names of the
/// form `Fixed/UTC+HH:MM:SS` or `Fixed/UTC-HH:MM:SS`, which are not in the
/// time-zone database. `-` is west of UTC. The server writes these names,
/// for example, for Arrow and ORC timestamps with a fixed offset.
fn parse_timezone(name: &str) -> Result<Zone, String> {
    if let Some(offset) = name.strip_prefix(FIXED_ZONE_PREFIX) {
        return parse_fixed_offset(offset)
            .map(Zone::Fixed)
            .ok_or_else(|| format!("clickhouse-test-runner: unknown time zone {name:?}"));
    }
    name.parse::<chrono_tz::Tz>()
        .map(Zone::Named)
        .map_err(|e| format!("clickhouse-test-runner: unknown time zone {name:?}: {e}"))
}

/// Parses `+HH:MM:SS` or `-HH:MM:SS` (exactly two digits each).
fn parse_fixed_offset(s: &str) -> Option<chrono::FixedOffset> {
    let b = s.as_bytes();
    if b.len() != 9 || b[3] != b':' || b[6] != b':' {
        return None;
    }
    let two_digits = |i: usize| -> Option<i32> {
        let (h, l) = (b[i], b[i + 1]);
        (h.is_ascii_digit() && l.is_ascii_digit())
            .then(|| i32::from(h - b'0') * 10 + i32::from(l - b'0'))
    };
    let secs = (two_digits(1)? * 60 + two_digits(4)?) * 60 + two_digits(7)?;
    match b[0] {
        b'+' => chrono::FixedOffset::east_opt(secs),
        b'-' => chrono::FixedOffset::west_opt(secs),
        _ => None,
    }
}

const MAX_TIME_SECONDS: i64 = 999 * 3600 + 59 * 60 + 59;

/// Writes a `Time` / `Time64` value: `[-]HH:MM:SS[.fff]`. The hours can be
/// more than 24. The server writes a value above 999:59:59 as 999:59:59, with
/// a zero fraction; the stored value is not changed.
pub(crate) fn format_time(ticks: i64, precision: u32) -> String {
    let scale = 10i128.pow(precision);
    let ticks = i128::from(ticks);
    let mut s = String::new();
    if ticks < 0 {
        s.push('-');
    }
    let abs = ticks.abs();
    let (mut total, mut frac) = ((abs / scale) as i64, (abs % scale) as i64);
    if total > MAX_TIME_SECONDS {
        total = MAX_TIME_SECONDS;
        frac = 0;
    }
    let _ = write!(
        s,
        "{:02}:{:02}:{:02}",
        total / 3600,
        total / 60 % 60,
        total % 60
    );
    if precision > 0 {
        let _ = write!(s, ".{frac:0width$}", width = precision as usize);
    }
    s
}

/// Follows ClickHouse's `formatIPv6`: the longest run of two or more zero
/// words becomes `::`, and an IPv4-mapped (`::ffff:a.b.c.d`) or
/// IPv4-compatible (`::a.b.c.d`) address ends in dotted form.
pub(crate) fn format_ipv6(ip: &Ipv6Addr) -> String {
    let words = ip.segments();
    let octets = ip.octets();
    let (mut best_base, mut best_len) = (None, 0);
    let mut i = 0;
    while i < 8 {
        if words[i] != 0 {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < 8 && words[j] == 0 {
            j += 1;
        }
        if j - i > best_len {
            best_base = Some(i);
            best_len = j - i;
        }
        i = j;
    }
    if best_len < 2 {
        best_base = None;
    }
    let ipv4_compatible = best_base == Some(0) && best_len == 6;
    let ipv4_mapped = best_base == Some(0) && best_len == 5 && words[5] == 0xffff;

    let mut s = String::new();
    for (i, word) in words.iter().enumerate() {
        if let Some(base) = best_base
            && i >= base
            && i < base + best_len
        {
            if i == base {
                s.push(':');
            }
            continue;
        }
        if i != 0 {
            s.push(':');
        }
        if i == 6 && (ipv4_compatible || ipv4_mapped) {
            let _ = write!(
                s,
                "{}.{}.{}.{}",
                octets[12], octets[13], octets[14], octets[15]
            );
            return s;
        }
        let _ = write!(s, "{word:x}");
    }
    if let Some(base) = best_base
        && base + best_len == 8
    {
        s.push(':');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use ch_core_rs::column::{
        ArrayColumn, DecimalColumn, DictionaryColumn, FixedBinaryColumn, MapColumn, NothingColumn,
        PrimitiveColumn, TupleColumn, Utf8Column,
    };
    use ch_core_rs::schema::IntervalKind;

    /// Renders row `row` at the top level.
    fn render(ty: &ChType, col: &Column, row: usize) -> String {
        let mut out = Vec::new();
        render_cell(&mut out, ty, col, row, false, &Context::default()).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn strings(values: &[&[u8]]) -> Utf8Column {
        let mut offsets = vec![0i32];
        let mut data = Vec::new();
        for v in values {
            data.extend_from_slice(v);
            offsets.push(data.len() as i32);
        }
        Utf8Column::new(offsets, data)
    }

    fn nulls(map: &[u8]) -> Bitmap {
        Bitmap::from_ch_null_map(map)
    }

    fn nullable(t: ChType) -> ChType {
        ChType::Nullable(Box::new(t))
    }

    #[test]
    fn floats() {
        // Variables, so that the compiler does not fold 0.1 + 0.2 exactly.
        let (a, b) = (std::hint::black_box(0.1f64), std::hint::black_box(0.2f64));
        // The expected text is what ClickHouse 26.10 writes for FORMAT TSV.
        let cases: &[(f64, &str)] = &[
            (1e15, "1000000000000000"),
            (1e20, "100000000000000000000"),
            (1e21, "1e21"),
            (1.2345e21, "1.2345e21"),
            (1.2345e20, "123450000000000000000"),
            (0.0001, "0.0001"),
            (1e-6, "0.000001"),
            (1.5e-6, "0.0000015"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (1.5e300, "1.5e300"),
            (a + b, "0.30000000000000004"),
            (5e-324, "5e-324"),
            (-12345678.9, "-12345678.9"),
            (100.0, "100"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (-0.0, "-0"),
            (0.0, "0"),
        ];
        for (f, want) in cases {
            assert_eq!(format_float64(*f), *want, "{f:e}");
        }
        assert_eq!(format_float64(f64::NAN), "nan");
        // The server writes `nan` also for a NaN with the sign bit set.
        assert_eq!(format_float64(-f64::NAN), "nan");
        assert_eq!(format_float32(-f32::NAN), "nan");
        assert_eq!(format_float32(0.1), "0.1");
        assert_eq!(format_float32(3.402_823_5e38), "3.4028235e38");
        assert_eq!(format_float32(16_777_216.0), "16777216");
        // BFloat16 1.5 is 0x3FC0.
        assert_eq!(bfloat16_to_f32([0xc0, 0x3f]), 1.5);
    }

    #[test]
    fn ipv6() {
        for (input, want) in [
            ("::ffff:1.2.3.4", "::ffff:1.2.3.4"),
            ("::1.2.3.4", "::1.2.3.4"),
            ("::", "::"),
            ("::1", "::1"),
            ("2001:db8::1", "2001:db8::1"),
            ("1::", "1::"),
            ("2001:db8:0:1::1", "2001:db8:0:1::1"),
            ("1:0:0:2:0:0:0:3", "1:0:0:2::3"),
            ("1:2:3:4:5:6:7:8", "1:2:3:4:5:6:7:8"),
        ] {
            assert_eq!(format_ipv6(&input.parse().unwrap()), want, "{input}");
        }
    }

    #[test]
    fn decimals() {
        assert_eq!(decimal_text(false, "150".into(), 2), "1.5");
        assert_eq!(decimal_text(false, "0".into(), 2), "0");
        assert_eq!(decimal_text(true, "1".into(), 5), "-0.00001");
        assert_eq!(decimal_text(false, "100000".into(), 3), "100");
        assert_eq!(decimal_text(true, "0".into(), 3), "0");
        let ty = ChType::Decimal {
            precision: 9,
            scale: 2,
            bits: 32,
        };
        let data = [
            150i32.to_le_bytes(),
            (-7i32).to_le_bytes(),
            0i32.to_le_bytes(),
        ]
        .concat();
        let col = Column::Decimal(DecimalColumn::new(data, 4, 9, 2));
        assert_eq!(render(&ty, &col, 0), "1.5");
        assert_eq!(render(&ty, &col, 1), "-0.07");
        assert_eq!(render(&ty, &col, 2), "0");
    }

    #[test]
    fn wide_integers() {
        let to_string = |bytes: &[u8], signed| {
            let (neg, digits) = wide_digits(&widen(bytes, signed).unwrap(), signed);
            format!("{}{digits}", if neg { "-" } else { "" })
        };
        assert_eq!(to_string(&[0xff; 32], true), "-1");
        assert_eq!(
            to_string(&[0xff; 32], false),
            "115792089237316195423570985008687907853269984665640564039457584007913129639935"
        );
        let mut min = [0u8; 32];
        min[31] = 0x80;
        assert_eq!(
            to_string(&min, true),
            "-57896044618658097711785492504343953926634992332820282019728792003956564819968"
        );
        // 128-bit values widen with their sign.
        assert_eq!(to_string(&(-5i128).to_le_bytes(), true), "-5");
        assert_eq!(
            to_string(&u128::MAX.to_le_bytes(), false),
            "340282366920938463463374607431768211455"
        );
        let ten19 = 10_000_000_000_000_000_000u128.to_le_bytes();
        assert_eq!(to_string(&ten19, false), "10000000000000000000");
        let col = Column::Int128(FixedBinaryColumn::new((-5i128).to_le_bytes().to_vec(), 16));
        assert_eq!(render(&ChType::Int128, &col, 0), "-5");
    }

    #[test]
    fn strings_and_nulls() {
        let col = Column::Utf8(strings(&[b"a\tb\\c\nd'e\x00\x08\x0c\x01"]));
        assert_eq!(
            render(&ChType::String, &col, 0),
            "a\\tb\\\\c\\nd\\'e\\0\\b\\f\x01"
        );
        let mut raw = Vec::new();
        render_cell(
            &mut raw,
            &ChType::String,
            &col,
            0,
            true,
            &Context::default(),
        )
        .unwrap();
        assert_eq!(raw, b"a\tb\\c\nd'e\x00\x08\x0c\x01");

        let mut c = strings(&[b"x", b""]);
        c.validity = Some(nulls(&[0, 1]));
        let col = Column::Utf8(c);
        let ty = nullable(ChType::String);
        assert_eq!(render(&ty, &col, 0), "x");
        assert_eq!(render(&ty, &col, 1), "\\N");

        // A null-map byte other than 1 is NULL too.
        let col = Column::Int8(PrimitiveColumn::new_nullable(
            vec![1, 0, 0],
            nulls(&[0, 2, 3]),
        ));
        let ty = nullable(ChType::Int8);
        assert_eq!(
            (0..3).map(|r| render(&ty, &col, r)).collect::<Vec<_>>(),
            ["1", "\\N", "\\N"]
        );

        let col = Column::Nothing(NothingColumn::new(1));
        assert_eq!(render(&nullable(ChType::Nothing), &col, 0), "\\N");
    }

    #[test]
    fn containers() {
        // Array(Nullable(String)): ['a\'b',NULL], []
        let mut values = strings(&[b"a'b", b""]);
        values.validity = Some(nulls(&[0, 1]));
        let col = Column::Array(ArrayColumn::new(vec![0, 2, 2], Column::Utf8(values)));
        let ty = ChType::Array(Box::new(nullable(ChType::String)));
        assert_eq!(render(&ty, &col, 0), "['a\\'b',NULL]");
        assert_eq!(render(&ty, &col, 1), "[]");

        // Tuple(a UInt8, b String, Nullable(UInt8))
        let col = Column::Tuple(TupleColumn::new(
            vec![
                Column::UInt8(PrimitiveColumn::new(vec![1])),
                Column::Utf8(strings(&[b"x"])),
                Column::UInt8(PrimitiveColumn::new_nullable(vec![0], nulls(&[1]))),
            ],
            1,
        ));
        let ty = ChType::Tuple(vec![
            (Some("a".into()), ChType::UInt8),
            (Some("b".into()), ChType::String),
            (None, nullable(ChType::UInt8)),
        ]);
        assert_eq!(render(&ty, &col, 0), "(1,'x',NULL)");

        // Map(String, UInt8) keeps the order of the server.
        let entries = Column::Tuple(TupleColumn::new(
            vec![
                Column::Utf8(strings(&[b"k", b"a"])),
                Column::UInt8(PrimitiveColumn::new(vec![1, 2])),
            ],
            2,
        ));
        let col = Column::Map(MapColumn::new(vec![0, 2], entries));
        let ty = ChType::Map(Box::new(ChType::String), Box::new(ChType::UInt8));
        assert_eq!(render(&ty, &col, 0), "{'k':1,'a':2}");

        // LowCardinality(Nullable(String)): the dictionary has the nulls.
        let col = Column::Dictionary(DictionaryColumn::new_nullable(
            vec![1, 0, 1],
            Column::Utf8(strings(&[b"", b"v"])),
            nulls(&[0, 1, 0]),
        ));
        let ty = ChType::LowCardinality(Box::new(nullable(ChType::String)));
        assert_eq!(
            (0..3).map(|r| render(&ty, &col, r)).collect::<Vec<_>>(),
            ["v", "\\N", "v"]
        );
    }

    #[test]
    fn scalars() {
        let one = |ty: &ChType, col: Column| render(ty, &col, 0);
        assert_eq!(
            one(
                &ChType::Uuid,
                Column::Uuid(FixedBinaryColumn::new(
                    [
                        0x61f0c4045cb311e7u64.to_le_bytes(),
                        0x907ba6006ad3dba0u64.to_le_bytes()
                    ]
                    .concat(),
                    16
                ))
            ),
            "61f0c404-5cb3-11e7-907b-a6006ad3dba0"
        );
        assert_eq!(
            one(
                &ChType::Ipv4,
                Column::Ipv4(PrimitiveColumn::new(vec![0x01020304]))
            ),
            "1.2.3.4"
        );
        let mapped: Ipv6Addr = "::ffff:1.2.3.4".parse().unwrap();
        let col = Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::Ipv6(FixedBinaryColumn::new(mapped.octets().to_vec(), 16)),
        ));
        assert_eq!(
            render(&ChType::Array(Box::new(ChType::Ipv6)), &col, 0),
            "['::ffff:1.2.3.4']"
        );
        assert_eq!(
            one(
                &ChType::Date,
                Column::Date(PrimitiveColumn::new(vec![18263]))
            ),
            "2020-01-02"
        );
        let col = Column::Array(ArrayColumn::new(
            vec![0, 1],
            Column::Date32(PrimitiveColumn::new(vec![-25567])),
        ));
        assert_eq!(
            render(&ChType::Array(Box::new(ChType::Date32)), &col, 0),
            "['1900-01-01']"
        );
        // Outside 0000-01-01 .. 9999-12-31: the nearest end of the range.
        let col = Column::Date32(PrimitiveColumn::new(vec![
            -719_528,
            -719_529,
            i32::MIN,
            2_932_896,
            2_932_897,
            2_146_764_120,
            i32::MAX,
        ]));
        let got: Vec<String> = (0..7).map(|r| render(&ChType::Date32, &col, r)).collect();
        assert_eq!(
            got,
            [
                "0000-01-01",
                "0000-01-01",
                "0000-01-01",
                "9999-12-31",
                "9999-12-31",
                "9999-12-31",
                "9999-12-31"
            ]
        );
        let ty = ChType::DateTime64 {
            precision: 3,
            timezone: None,
        };
        assert_eq!(
            one(
                &ty,
                Column::DateTime64(PrimitiveColumn::new(vec![1_577_836_800_120]))
            ),
            "2020-01-01 00:00:00.120"
        );
        let ty = ChType::DateTime {
            timezone: Some("Asia/Tokyo".into()),
        };
        assert_eq!(
            one(
                &ty,
                Column::DateTime(PrimitiveColumn::new(vec![1_577_836_800]))
            ),
            "2020-01-01 09:00:00"
        );
        // A DateTime without a time zone uses the time zone of the query.
        let mut out = Vec::new();
        let tokyo = Context {
            default_timezone: Some("Asia/Tokyo".into()),
        };
        let col = Column::DateTime(PrimitiveColumn::new(vec![0]));
        render_cell(
            &mut out,
            &ChType::DateTime { timezone: None },
            &col,
            0,
            false,
            &tokyo,
        )
        .unwrap();
        assert_eq!(out, b"1970-01-01 09:00:00");
        assert_eq!(
            one(
                &ChType::Time,
                Column::Time(PrimitiveColumn::new(vec![-3723]))
            ),
            "-01:02:03"
        );
        let t64 = |p: u8, v: i64| {
            one(
                &ChType::Time64 { precision: p },
                Column::Time64(PrimitiveColumn::new(vec![v])),
            )
        };
        assert_eq!(t64(3, 360_000_500), "100:00:00.500");
        // The server writes a value above 999:59:59 as 999:59:59.
        assert_eq!(t64(2, 777_601_200), "999:59:59.00");
        assert_eq!(
            one(
                &ChType::Time,
                Column::Time(PrimitiveColumn::new(vec![-7_200_000]))
            ),
            "-999:59:59"
        );
        assert_eq!(
            one(
                &ChType::Bool,
                Column::Bool(ch_core_rs::column::BoolColumn::from_wire_bytes(&[1]))
            ),
            "true"
        );
        let col = Column::Array(ArrayColumn::new(
            vec![0, 2],
            Column::Float64(PrimitiveColumn::new(vec![f64::NAN, f64::INFINITY])),
        ));
        assert_eq!(
            render(&ChType::Array(Box::new(ChType::Float64)), &col, 0),
            "[nan,inf]"
        );
        assert_eq!(
            one(
                &ChType::Interval(IntervalKind::Day),
                Column::Interval(PrimitiveColumn::new(vec![3]))
            ),
            "3"
        );
        let col = Column::Dictionary(DictionaryColumn::new(
            vec![0],
            Column::Utf8(strings(&[b"a"])),
        ));
        assert_eq!(
            render(&ChType::LowCardinality(Box::new(ChType::String)), &col, 0),
            "a"
        );
        let ty = ChType::SimpleAggregateFunction {
            func: "sum".into(),
            inner: Box::new(ChType::UInt64),
        };
        assert_eq!(one(&ty, Column::UInt64(PrimitiveColumn::new(vec![7]))), "7");
        let ty = ChType::Enum8 {
            variants: vec![("a'b".into(), 1), ("c".into(), -128)],
        };
        let col = Column::Array(ArrayColumn::new(
            vec![0, 2],
            Column::Enum8(PrimitiveColumn::new(vec![1, -128])),
        ));
        assert_eq!(
            render(&ChType::Array(Box::new(ty)), &col, 0),
            "['a\\'b','c']"
        );
    }

    #[test]
    fn named_time_zones_after_2099() {
        // The values and the text of the server (26.10), `toUnixTimestamp` and
        // `toString`: the zone rules apply after 2099 too.
        let ctx = Context::default();
        for (zone, v, want) in [
            ("Europe/Amsterdam", 4_088_887_680, "2099-07-28 04:08:00"),
            ("Europe/Amsterdam", 4_120_423_680, "2100-07-28 04:08:00"),
            ("Europe/Amsterdam", 4_139_089_680, "2101-03-01 04:08:00"),
            ("Europe/Amsterdam", 4_278_190_080, "2105-07-28 04:08:00"),
            ("Europe/Amsterdam", 4_294_967_295, "2106-02-07 07:28:15"),
            ("America/New_York", 4_278_233_280, "2105-07-28 10:08:00"),
            ("America/New_York", 4_255_047_000, "2104-11-02 01:30:00"),
            ("Europe/Amsterdam", 7_276_097_280, "2200-07-28 04:08:00"),
            ("Europe/Amsterdam", 10_407_918_600, "2299-10-25 02:30:00"),
        ] {
            assert_eq!(
                format_datetime(v, 0, 0, Some(zone), &ctx).unwrap(),
                want,
                "{zone} {v}"
            );
        }
    }

    #[test]
    fn fixed_offset_time_zones() {
        let ctx = Context::default();
        for (zone, want) in [
            ("Fixed/UTC+00:00:00", "1970-01-01 00:00:00"),
            ("Fixed/UTC+05:30:00", "1970-01-01 05:30:00"),
            ("Fixed/UTC-08:00:00", "1969-12-31 16:00:00"),
            ("Fixed/UTC+14:00:00", "1970-01-01 14:00:00"),
            ("Fixed/UTC-14:00:00", "1969-12-31 10:00:00"),
        ] {
            assert_eq!(
                format_datetime(0, 0, 0, Some(zone), &ctx).unwrap(),
                want,
                "{zone}"
            );
        }
        assert_eq!(
            format_datetime(0, 5, 1, Some("Fixed/UTC-05:30:00"), &ctx).unwrap(),
            "1969-12-31 18:30:00.5"
        );
        // Same offset as Asia/Kathmandu.
        assert_eq!(
            format_datetime(1_700_000_000, 0, 0, Some("Fixed/UTC+05:45:00"), &ctx),
            format_datetime(1_700_000_000, 0, 0, Some("Asia/Kathmandu"), &ctx),
        );
        for name in [
            "Fixed/UTC+1:00:00",
            "Fixed/UTC+01:00",
            "Fixed/UTC*01:00:00",
            "Fixed/UTC+0a:00:00",
            "Fixed/UTC+05:30:000",
            "Fixed/Europe/Amsterdam",
            "No/Such_Zone",
        ] {
            assert!(parse_timezone(name).is_err(), "{name}");
        }
        assert_eq!(
            parse_timezone("Europe/Amsterdam"),
            Ok(Zone::Named(chrono_tz::Europe::Amsterdam))
        );
    }

    #[test]
    fn mismatch_is_an_error() {
        let col = Column::Utf8(strings(&[b"x"]));
        let mut out = Vec::new();
        let err = render_cell(
            &mut out,
            &ChType::UInt8,
            &col,
            0,
            false,
            &Context::default(),
        )
        .unwrap_err();
        assert!(
            err.contains("unexpected column Utf8 for the data type UInt8"),
            "{err}"
        );
        let err = render_cell(
            &mut out,
            &ChType::String,
            &col,
            1,
            false,
            &Context::default(),
        )
        .unwrap_err();
        assert!(err.contains("out of range"), "{err}");
        // An enum value without a name.
        let ty = ChType::Enum8 {
            variants: vec![("a".into(), 1)],
        };
        let col = Column::Enum8(PrimitiveColumn::new(vec![5]));
        assert!(render_cell(&mut out, &ty, &col, 0, false, &Context::default()).is_err());
    }
}
