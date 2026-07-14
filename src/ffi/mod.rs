//! Arrow C Data Interface implementation for zero-copy export.
//!
//! Implements ArrowSchema, ArrowArray, and ArrowArrayStream per
//! https://arrow.apache.org/docs/format/CDataInterface.html

use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::sync::Arc;

use crate::batch::ColBatch;
use crate::column::Column;
use crate::native::decode::low_cardinality_dict_value_type;
use crate::schema::{ChType, IntervalKind, Schema};

// ---------------------------------------------------------------------------
// Arrow C Data Interface structs (repr(C) per spec)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct ArrowSchema {
    format: *const c_char,
    name: *const c_char,
    metadata: *const c_char,
    flags: i64,
    n_children: i64,
    children: *mut *mut ArrowSchema,
    dictionary: *mut ArrowSchema,
    release: Option<unsafe extern "C" fn(*mut ArrowSchema)>,
    private_data: *mut c_void,
}

#[repr(C)]
pub struct ArrowArray {
    length: i64,
    null_count: i64,
    offset: i64,
    n_buffers: i64,
    n_children: i64,
    buffers: *mut *const c_void,
    children: *mut *mut ArrowArray,
    dictionary: *mut ArrowArray,
    release: Option<unsafe extern "C" fn(*mut ArrowArray)>,
    private_data: *mut c_void,
}

#[repr(C)]
pub struct ArrowArrayStream {
    get_schema: Option<unsafe extern "C" fn(*mut ArrowArrayStream, *mut ArrowSchema) -> i32>,
    get_next: Option<unsafe extern "C" fn(*mut ArrowArrayStream, *mut ArrowArray) -> i32>,
    get_last_error: Option<unsafe extern "C" fn(*mut ArrowArrayStream) -> *const c_char>,
    release: Option<unsafe extern "C" fn(*mut ArrowArrayStream)>,
    private_data: *mut c_void,
}

impl ArrowArrayStream {
    /// Invoke the release callback if present, then clear it. Follows the
    /// spec's consumer pattern: the callback runs with `release` still set and
    /// normally clears it itself; clearing afterwards keeps the call
    /// idempotent even for a callback that does not. A stream already released
    /// by a consumer has `release` set to null, making this a no-op.
    ///
    /// # Safety
    ///
    /// `self` must be a stream initialized per the Arrow C Data Interface, for
    /// example by [`export_chunks_to_stream`], or already released or moved
    /// from with `release` cleared to `None`.
    pub unsafe fn release_if_set(&mut self) {
        if let Some(release) = self.release {
            release(self as *mut ArrowArrayStream);
            self.release = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Private data for release callbacks
// ---------------------------------------------------------------------------

struct SchemaPrivateData {
    format: CString,
    name: CString,
    children: Vec<*mut ArrowSchema>,
    /// The dictionary value-type child schema for a `LowCardinality(T)` field,
    /// owned here so it is freed when this field's schema is released. Null for
    /// every non-dictionary field.
    dictionary: *mut ArrowSchema,
}

struct ArrayPrivateData {
    buffers: Vec<*const c_void>,
    children: Vec<*mut ArrowArray>,
    _batch: Arc<ColBatch>,
    /// The dictionary values child array for a `LowCardinality(T)` column, owned
    /// here so it is freed when this column's array is released. Null for every
    /// non-dictionary column.
    dictionary: *mut ArrowArray,
}

struct StreamPrivateData {
    /// Schema for `get_schema`; valid even when there are zero chunks.
    schema: Schema,
    /// Remaining chunks to hand out, one Arrow record batch per `get_next`.
    chunks: std::vec::IntoIter<Arc<ColBatch>>,
    error_msg: CString,
}

// ---------------------------------------------------------------------------
// Release callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn release_schema(schema: *mut ArrowSchema) {
    if schema.is_null() {
        return;
    }
    let s = &mut *schema;
    if s.private_data.is_null() {
        return;
    }
    let pd = Box::from_raw(s.private_data as *mut SchemaPrivateData);
    for child_ptr in &pd.children {
        let child = &mut **child_ptr;
        if let Some(release_fn) = child.release {
            release_fn(*child_ptr);
        }
        let _ = Box::from_raw(*child_ptr);
    }
    // Release and free the dictionary value-type child, if this is a dictionary
    // field. Heap-allocated and owned the same way as the children above.
    if !pd.dictionary.is_null() {
        let dict = &mut *pd.dictionary;
        if let Some(release_fn) = dict.release {
            release_fn(pd.dictionary);
        }
        let _ = Box::from_raw(pd.dictionary);
    }
    drop(pd);
    s.release = None;
    s.private_data = ptr::null_mut();
}

unsafe extern "C" fn release_array(array: *mut ArrowArray) {
    if array.is_null() {
        return;
    }
    let a = &mut *array;
    if a.private_data.is_null() {
        return;
    }
    let pd = Box::from_raw(a.private_data as *mut ArrayPrivateData);
    for child_ptr in &pd.children {
        let child = &mut **child_ptr;
        if let Some(release_fn) = child.release {
            release_fn(*child_ptr);
        }
        let _ = Box::from_raw(*child_ptr);
    }
    // Release and free the dictionary values child, if this is a dictionary
    // column. Heap-allocated and owned the same way as the children above.
    if !pd.dictionary.is_null() {
        let dict = &mut *pd.dictionary;
        if let Some(release_fn) = dict.release {
            release_fn(pd.dictionary);
        }
        let _ = Box::from_raw(pd.dictionary);
    }
    drop(pd);
    a.release = None;
    a.private_data = ptr::null_mut();
}

unsafe extern "C" fn release_stream(stream: *mut ArrowArrayStream) {
    if stream.is_null() {
        return;
    }
    let s = &mut *stream;
    if s.private_data.is_null() {
        return;
    }
    let _ = Box::from_raw(s.private_data as *mut StreamPrivateData);
    s.release = None;
    s.private_data = ptr::null_mut();
}

// ---------------------------------------------------------------------------
// Arrow format strings
// ---------------------------------------------------------------------------

fn arrow_format(ch_type: &ChType) -> String {
    match ch_type {
        // Arrow Null has no data buffers; every row is intrinsically null.
        ChType::Nothing => "n".into(),
        ChType::Bool => "b".into(),
        ChType::Int8 => "c".into(),
        ChType::Int16 => "s".into(),
        ChType::Int32 => "i".into(),
        ChType::Int64 => "l".into(),
        ChType::UInt8 => "C".into(),
        ChType::UInt16 => "S".into(),
        ChType::UInt32 => "I".into(),
        ChType::UInt64 => "L".into(),
        ChType::Float32 => "f".into(),
        ChType::Float64 => "g".into(),
        // Arrow has no native BFloat16 type: `e` is IEEE binary16, whose bit
        // layout differs. Export the raw little-endian word honestly as
        // FixedSizeBinary(2), preserving every bit pattern without a copy.
        ChType::BFloat16 => "w:2".into(),
        // Temporal export is zero-copy: never widen or rescale a buffer. Map to
        // a real Arrow temporal type only on an exact same-width match, else
        // expose the raw integer primitive.
        //
        // Date is UInt16 days; Arrow has no 16-bit date, so expose raw days as
        // uint16. Date32 is i32 days, an exact match for Arrow date32. DateTime
        // is UInt32 seconds; Arrow has no u32-seconds timestamp, so expose raw
        // seconds as uint32. DateTime64(P) ticks are i64; an Arrow timestamp is
        // also i64, an exact match, but only for the units Arrow can express
        // (P in {0,3,6,9} -> s/m/u/n). Other precisions fall back to raw i64.
        ChType::Date => "S".into(),
        ChType::Date32 => "tdD".into(),
        ChType::DateTime { .. } => "I".into(),
        ChType::DateTime64 {
            precision,
            timezone,
        } => {
            let unit = match precision {
                0 => "s",
                3 => "m",
                6 => "u",
                9 => "n",
                _ => return "l".into(),
            };
            let tz = timezone.as_deref().unwrap_or("");
            format!("ts{unit}:{tz}")
        }
        // ClickHouse Time permits signed values outside Arrow Time's [0, 24h)
        // domain, so exporting as an Arrow time type would misstate the logical
        // range. Preserve the raw signed seconds/ticks at their native widths,
        // with no validation, rescaling, or copy.
        ChType::Time => "i".into(),
        ChType::Time64 { .. } => "l".into(),
        // Arrow Duration is physically i64 and exactly matches the four units
        // it supports: seconds, milliseconds, microseconds, and nanoseconds.
        // Minute/Hour/Day/Week are fixed counts too, but Arrow has no Duration
        // units for them. Month/Quarter/Year are calendar-variable, and Arrow's
        // calendar interval layouts are physically different. Preserve both
        // groups as raw i64 rather than rescaling or copying.
        ChType::Interval(kind) => match kind {
            IntervalKind::Second => "tDs".into(),
            IntervalKind::Millisecond => "tDm".into(),
            IntervalKind::Microsecond => "tDu".into(),
            IntervalKind::Nanosecond => "tDn".into(),
            IntervalKind::Year
            | IntervalKind::Quarter
            | IntervalKind::Month
            | IntervalKind::Week
            | IntervalKind::Day
            | IntervalKind::Hour
            | IntervalKind::Minute => "l".into(),
        },
        ChType::String => "u".into(),
        // Serialized aggregate states are opaque binary values whose row
        // boundaries were recovered by a function-specific Native codec.
        // LargeBinary is the honest zero-copy Arrow storage: the state can be
        // variable width and has no UTF-8 semantics or 2 GiB aggregate-data cap.
        ChType::AggregateFunction { .. } => "Z".into(),
        ChType::FixedString(n) => format!("w:{n}"),
        // IPv4 is the standard UInt32 numeric value, exported as Arrow uint32
        // (`I`), zero-copy like DateTime. IPv6 and UUID are raw 16-byte blobs,
        // exported as Arrow fixed-size binary of width 16 (`w:16`). The bytes are
        // verbatim wire bytes; this crate does not claim the `arrow.uuid`
        // extension type. Any host value mapping is a binding concern.
        ChType::Ipv4 => "I".into(),
        ChType::Ipv6 => "w:16".into(),
        ChType::Uuid => "w:16".into(),
        // Enum8/Enum16 export as their underlying signed int (Arrow int8 `c` /
        // int16 `s`), zero-copy. Arrow has no native enum and ClickHouse enum
        // values are arbitrary signed ints (not 0..N-1 dictionary indices), so a
        // dictionary export would require forbidden per-cell remapping. The
        // name->value map is carried in the ChType for bindings.
        ChType::Enum8 { .. } => "c".into(),
        ChType::Enum16 { .. } => "s".into(),
        // Decimal(P, S) exports with the Arrow C Data decimal format string
        // `d:precision,scale,bitwidth` over the contiguous little-endian
        // two's-complement buffer, zero-copy. The Arrow C Data spec defines
        // `d:P,S` as 128-bit and `d:P,S,bits` for an explicit bit width, so the
        // 128-bit case is emitted as bare `d:P,S` and the others carry the width.
        // The buffer is exported verbatim at its native width (4/8/16/32 bytes);
        // we deliberately do NOT widen narrow decimals to 128 bits, which would
        // cost a forbidden per-value copy in the hot path.
        //
        // NOTE: `decimal32`/`decimal64`/`decimal256` are newer in the Arrow C
        // Data Interface than `decimal128`. The `d:P,S,bits` spelling is the
        // documented form, but consumer support for the non-128 widths varies by
        // Arrow implementation/version. Confirm consumer compatibility before
        // relying on the 32/64/256-bit exports downstream.
        ChType::Decimal {
            precision,
            scale,
            bits,
        } => {
            if *bits == 128 {
                format!("d:{precision},{scale}")
            } else {
                format!("d:{precision},{scale},{bits}")
            }
        }
        // Wide integers export as Arrow FixedSizeBinary: `w:16` for the 128-bit
        // pair, `w:32` for the 256-bit pair, zero-copy over the verbatim
        // little-endian bytes. NOT the decimal format `d:P,0,bits`: Arrow caps
        // decimal128 at precision 38 and decimal256 at 76, which cannot represent
        // the full 128/256-bit range (2^127-1 is 39 digits), and decimal is
        // signed so an unsigned high-bit value would read as negative.
        // FixedSizeBinary is opaque bytes and universally supported; the binding
        // recovers the integer (and its signedness) from the ChType/type name,
        // exactly as it separates UUID from IPv6, both `w:16`.
        ChType::Int128 | ChType::UInt128 => "w:16".into(),
        ChType::Int256 | ChType::UInt256 => "w:32".into(),
        ChType::Nullable(inner) => arrow_format(inner),
        // A dictionary array's top-level format is the INDEX type. The value
        // type lives in the schema's `dictionary` child. Index width is
        // normalized to i32 by the decoder, so the index format is always `i`.
        ChType::LowCardinality(_) => "i".into(),
        // Array(T) exports as an Arrow LargeList (`+L`, 64-bit offsets), NOT a
        // List (`+l`, 32-bit). The element type is NOT in this format string; it
        // is fully described by a single `item` child schema (see
        // `write_field_schema`). ClickHouse array offsets are `UInt64` element
        // counts and the decoder stores them as `i64` with no artificial i32 cap,
        // so the `offsets: Vec<i64>` buffer is the exact LargeList offsets buffer
        // and is handed over verbatim, zero-copy.
        //
        // NOTE: LargeList consumer support is slightly less universal than
        // List, but is supported by pyarrow, arrow-rs, polars, and duckdb. A
        // `+l` (32-bit) variant would require copying every offset into a fresh
        // i32 buffer (and capping/erroring on element counts above i32::MAX), a
        // per-value copy in the hot path we deliberately avoid. LargeList is
        // the zero-copy match for the i64 offsets the decoder already produces.
        ChType::Array(_) => "+L".into(),
        // Tuple(T1, ...) exports as an Arrow struct (`+s`). The element types
        // are NOT in this format string; each element is a child schema
        // recursively described by `write_field_schema`, named by the
        // ClickHouse element name (verbatim) for a named tuple and by the
        // 1-based decimal position ("1", "2", ...) for an unnamed one. The
        // zero-element `Tuple()` is `+s` with no children. A
        // `Nullable(Tuple(...))` reaches this arm through the `Nullable`
        // recursion above; struct validity is independent of the children per
        // the C Data spec (consumers AND them), so the nullable flag plus the
        // buffers[0] validity bitmap compose like any other nullable column.
        ChType::Tuple(_) => "+s".into(),
        // Map(K, V) exports as LargeList-of-struct (`+L` over an `entries`
        // struct child with `key`/`value` grandchildren), NOT as the Arrow map
        // type `+m`: the C Data spec mandates i32 offsets for `+m` and defines
        // no large-map format, while ClickHouse map offsets are `UInt64` stored
        // as i64, so `+m` would force a per-offset copy and an i32 cap. The
        // chosen naming makes the export shape-isomorphic to Arrow Map minus
        // the offset width, so bindings can cast cheaply.
        // `ARROW_FLAG_MAP_KEYS_SORTED` is never set (it is `+m`-only).
        ChType::Map(..) => "+L".into(),
        // Name-decoration aliases export with the Arrow shape of the physical
        // type they delegate to (`SimpleAggregateFunction` -> its inner, a geo
        // alias -> its Tuple/Array nesting, `Nested` -> the LargeList over an
        // `Array(Tuple(...))`). The child schemas are emitted by
        // `write_field_schema`, which expands the same delegate.
        ChType::SimpleAggregateFunction { inner, .. } => arrow_format(inner),
        ChType::Geo(kind) => arrow_format(&kind.underlying_type()),
        // `Nested` is always an `Array(Tuple(...))`, so its top format is the
        // LargeList `+L` regardless of the field types (which appear in the
        // `item` struct child), matching the `Array`/`Map` arms above.
        ChType::Nested(_) => "+L".into(),
    }
}

/// Whether a type exports as an Arrow dictionary array (a `LowCardinality(T)`).
/// Such a type needs the schema/array `dictionary` child populated, unlike the
/// flat types whose `dictionary` pointer stays null.
fn is_dictionary(ch_type: &ChType) -> bool {
    matches!(ch_type, ChType::LowCardinality(_))
}

/// The Arrow value type of a `LowCardinality(T)` dictionary, i.e. the type of
/// the entries in the dictionary `values` column. The inner `Nullable` is
/// transparent here: nulls live in the index validity, so the dictionary value
/// type is always the non-nullable inner type. The shared
/// `low_cardinality_dict_value_type` helper sees through any
/// `SimpleAggregateFunction` chain around the removeNullable `Nullable`, so
/// `LowCardinality(SAF(anyLast, Nullable(String)))` exports the `String` value
/// type rather than a stray alias/`Nullable`.
fn dictionary_value_type(ch_type: &ChType) -> &ChType {
    match ch_type {
        ChType::LowCardinality(inner) => low_cardinality_dict_value_type(inner).1,
        other => other,
    }
}

/// Whether a column exports with the Arrow nullable flag set. A bare
/// `Nullable(T)` is nullable, and a `LowCardinality(Nullable(T))` is nullable
/// at the index level (nulls live in the index validity bitmap). A plain
/// `LowCardinality(T)` is not nullable. The `LowCardinality` case resolves the
/// null flag through the shared `low_cardinality_dict_value_type` helper, so a
/// `LowCardinality(SAF(anyLast, Nullable(String)))` is correctly nullable.
fn field_is_nullable(ch_type: &ChType) -> bool {
    match ch_type {
        ChType::Nullable(_) => true,
        ChType::LowCardinality(inner) => low_cardinality_dict_value_type(inner).0,
        _ => false,
    }
}

/// Build a C string from a Rust string, dropping any interior NUL bytes.
///
/// Arrow C Data names and format strings are NUL-terminated C strings, so an
/// interior NUL cannot be represented. A column name and a `DateTime`/
/// `DateTime64` timezone both originate from the untrusted wire stream and are
/// only validated as UTF-8, so either can carry a NUL byte. `CString::new`
/// would return `Err` on such input and a `.unwrap()` would panic. That panic
/// can unwind out of the `extern "C"` stream callbacks (`stream_get_schema`),
/// which is undefined behavior across the FFI boundary. Strip the NUL bytes so
/// export stays panic-free; the resulting name/format is best effort for input
/// that is already malformed.
fn cstring_lossy(s: &str) -> CString {
    if s.as_bytes().contains(&0) {
        let cleaned: Vec<u8> = s.bytes().filter(|&b| b != 0).collect();
        // `cleaned` has no interior NUL, so this cannot fail.
        CString::new(cleaned).unwrap_or_default()
    } else {
        CString::new(s).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Schema export
// ---------------------------------------------------------------------------

unsafe fn write_field_schema(out: *mut ArrowSchema, name: &str, ch_type: &ChType) {
    // A top-level name-decoration alias (SimpleAggregateFunction, geo, Nested)
    // exports exactly as the physical type it delegates to, so expand and recurse
    // once here. A `Nullable(Point)` is NOT caught here (Nullable has no
    // delegate); its inner geo alias is expanded in the children match below,
    // while `arrow_format` and `field_is_nullable` handle the Nullable wrapper.
    if let Some(under) = ch_type.physical_delegate() {
        write_field_schema(out, name, &under);
        return;
    }

    let format = cstring_lossy(&arrow_format(ch_type));
    let name_cstr = cstring_lossy(name);

    // For a dictionary field the value type goes in a `dictionary` child schema;
    // the field's own format string is the index type. The child is allocated
    // and owned by this field's private data so it is freed on release.
    let dictionary = if is_dictionary(ch_type) {
        // Safety: an all-zero `ArrowSchema` is a valid initial value (pointers
        // null, integers zero, `Option<extern fn>` the all-zero `None` niche),
        // and `write_field_schema` overwrites every field before any consumer
        // observes it.
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        write_field_schema(child, "", dictionary_value_type(ch_type));
        child
    } else {
        ptr::null_mut()
    };

    // Container fields describe their element type(s) in child schemas, owned
    // by this field's private data (in `children`, exactly like the top-level
    // record-batch schema owns its column children), so `release_schema`
    // releases and frees them when this field is released. Recursing through
    // `write_field_schema` fully describes each child, including its own
    // nullable flag (`Array(Nullable(T))`), its dictionary child
    // (`Array(LowCardinality(T))`), or a further nested container. The match is
    // on the Nullable-unwrapped value type so a `Nullable(Tuple(...))` still
    // emits its element children (an `Array` is never inside a `Nullable`, so
    // its arm is unaffected by the unwrap).
    let mut children: Vec<*mut ArrowSchema> = Vec::new();
    // Expand a name-decoration alias sitting directly inside a `Nullable`
    // (`Nullable(Point)` -> `Tuple`) so its element children are emitted. A
    // top-level alias was already expanded and recursed above, so this only
    // matters for the one alias legal under `Nullable`, `Point`.
    let inner_delegate = ch_type.inner().physical_delegate();
    let inner_type = inner_delegate.as_ref().unwrap_or_else(|| ch_type.inner());
    match inner_type {
        // An `Array(T)` LargeList field: one conventionally-named `item` child.
        ChType::Array(inner) => {
            // Safety: an all-zero `ArrowSchema` is a valid initial value, the same
            // niche argument as the dictionary child above; `write_field_schema`
            // overwrites every field before any consumer observes it.
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            write_field_schema(child, "item", inner);
            children.push(child);
        }
        // A `Tuple(T1, ...)` struct field: one child per element, in declaration
        // order, named by the ClickHouse element name (verbatim; it flows through
        // `cstring_lossy` inside the recursion, like every wire-origin name) or by
        // the 1-based decimal position for an unnamed element. `Tuple()` emits no
        // children.
        ChType::Tuple(elements) => {
            for (i, (element_name, element_type)) in elements.iter().enumerate() {
                // Safety: an all-zero `ArrowSchema` is a valid initial value, the
                // same niche argument as the dictionary child above;
                // `write_field_schema` overwrites every field before any consumer
                // observes it.
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
                match element_name {
                    Some(n) => write_field_schema(child, n, element_type),
                    None => write_field_schema(child, &(i + 1).to_string(), element_type),
                }
                children.push(child);
            }
        }
        // A `Map(K, V)` LargeList-of-struct field: one child named `entries`,
        // a non-nullable struct whose `key`/`value` grandchildren describe the
        // key and value types. Synthesizing a transient two-element named
        // `ChType::Tuple` and recursing reuses the Tuple arm above verbatim
        // (the struct format, flags 0, and the recursive key/value description
        // including the value's own nullable flag); the two `ChType` clones are
        // per schema export, never per row.
        ChType::Map(key, value) => {
            let entries_type = ChType::Tuple(vec![
                (Some("key".to_string()), key.as_ref().clone()),
                (Some("value".to_string()), value.as_ref().clone()),
            ]);
            // Safety: an all-zero `ArrowSchema` is a valid initial value, the
            // same niche argument as the dictionary child above;
            // `write_field_schema` overwrites every field before any consumer
            // observes it.
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            write_field_schema(child, "entries", &entries_type);
            children.push(child);
        }
        _ => {}
    }
    let n_children = children.len() as i64;

    let pd = Box::new(SchemaPrivateData {
        format,
        name: name_cstr,
        children,
        dictionary,
    });

    let schema = &mut *out;
    schema.format = pd.format.as_ptr();
    schema.name = pd.name.as_ptr();
    schema.metadata = ptr::null();
    schema.flags = if field_is_nullable(ch_type) { 2 } else { 0 };
    schema.n_children = n_children;
    // `pd.children` heap buffer is stable across the `Box::into_raw(pd)` move
    // below (moving the Box moves the struct fields, not the Vec's heap buffer),
    // so this pointer stays valid until release. Null when there are no children.
    schema.children = if pd.children.is_empty() {
        ptr::null_mut()
    } else {
        pd.children.as_ptr() as *mut *mut ArrowSchema
    };
    schema.dictionary = pd.dictionary;
    schema.release = Some(release_schema);
    schema.private_data = Box::into_raw(pd) as *mut c_void;
}

/// # Safety
///
/// `out` must be a valid, writable pointer to an `ArrowSchema`, normally a
/// zeroed struct. On return `out` owns its data and must be freed through its
/// `release` callback per the Arrow C Data Interface.
pub unsafe fn export_schema(schema_in: &Schema, out: *mut ArrowSchema) {
    let n_children = schema_in.num_fields() as i64;

    let mut child_schemas: Vec<*mut ArrowSchema> = Vec::with_capacity(n_children as usize);
    for field in &schema_in.fields {
        // Safety: an all-zero `ArrowSchema` is a valid initial value. Every field
        // is a raw pointer (null is valid), an integer, or `Option<extern fn>`,
        // whose `None` niche is the all-zero bit pattern. `write_field_schema`
        // overwrites every field before the struct is observed by a consumer.
        let child_schema = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        write_field_schema(child_schema, &field.name, &field.ch_type);
        child_schemas.push(child_schema);
    }

    let format = CString::new("+s").unwrap();
    let name = CString::new("").unwrap();

    let pd = Box::new(SchemaPrivateData {
        format,
        name,
        children: child_schemas.clone(),
        dictionary: ptr::null_mut(),
    });

    let schema = &mut *out;
    schema.format = pd.format.as_ptr();
    schema.name = pd.name.as_ptr();
    schema.metadata = ptr::null();
    schema.flags = 0;
    schema.n_children = n_children;
    schema.children = pd.children.as_ptr() as *mut *mut ArrowSchema;
    schema.dictionary = ptr::null_mut();
    schema.release = Some(release_schema);
    schema.private_data = Box::into_raw(pd) as *mut c_void;
}

// ---------------------------------------------------------------------------
// Array export
// ---------------------------------------------------------------------------

unsafe fn export_column_array(batch: &Arc<ColBatch>, col_idx: usize, out: *mut ArrowArray) {
    export_one_column(batch, &batch.columns[col_idx], out);
}

/// Export an arbitrary `Column` into `out`. Recursive: the dictionary `values`
/// column of a `LowCardinality(T)` is exported through the same path into the
/// array's `dictionary` child. `batch` is kept alive in private data so all the
/// borrowed buffers stay valid until release.
unsafe fn export_one_column(batch: &Arc<ColBatch>, col: &Column, out: *mut ArrowArray) {
    let length = col.len() as i64;
    let null_count = col.null_count() as i64;

    let mut buffers: Vec<*const c_void> = Vec::new();
    let mut children: Vec<*mut ArrowArray> = Vec::new();
    let mut dictionary: *mut ArrowArray = ptr::null_mut();

    match col {
        // Arrow Null has zero buffers. A `Nullable(Nothing)` column retains its
        // ClickHouse null map for Native re-encoding, but Arrow ignores it and
        // treats every row as null by definition.
        Column::Nothing(_) => {}
        Column::Bool(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            buffers.push(c.bitmap.as_ptr() as *const c_void);
        }
        Column::Int8(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Int16(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Int32(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Int64(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::UInt8(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::UInt16(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::UInt32(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::UInt64(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Float32(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Float64(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::BFloat16(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Date(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Date32(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::DateTime(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::DateTime64(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Time(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Time64(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Interval(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Utf8(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            push_offsets(&mut buffers, &c.offsets, EMPTY_OFFSETS_I32.as_ptr());
            buffers.push(c.data.as_ptr() as *const c_void);
        }
        // Arrow LargeBinary: validity (always null because ClickHouse forbids
        // Nullable(AggregateFunction)), i64 offsets, then serialized state data.
        // All three buffers borrow the batch-owned AggregateStateColumn and are
        // kept alive by ArrayPrivateData's Arc<ColBatch>.
        Column::AggregateState(c) => {
            buffers.push(ptr::null());
            push_offsets(&mut buffers, &c.offsets, EMPTY_OFFSETS_I64.as_ptr());
            buffers.push(c.data.as_ptr() as *const c_void);
        }
        Column::FixedBinary(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            buffers.push(c.data.as_ptr() as *const c_void);
        }
        // IPv4 is a uint32 primitive (validity, then values). UUID and IPv6 are
        // width-16 fixed binary (validity, then the contiguous 16-byte rows),
        // pushed exactly like the FixedString arm above.
        Column::Ipv4(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        // Enum8/Enum16 export the underlying signed int buffer (validity, then
        // values), exactly like Int8/Int16.
        Column::Enum8(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Enum16(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        // UUID, IPv6, and the wide integers are all width-16/32 fixed-binary
        // buffers exported as 2 buffers (validity, then the contiguous rows),
        // zero-copy. The Arrow format string (`w:16`/`w:32`) is chosen by
        // `arrow_format` from the ChType; the buffers are identical here.
        Column::Ipv6(c)
        | Column::Uuid(c)
        | Column::Int128(c)
        | Column::UInt128(c)
        | Column::Int256(c)
        | Column::UInt256(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            buffers.push(c.data.as_ptr() as *const c_void);
        }
        // Decimal exports 2 buffers (validity, then the contiguous fixed-width
        // little-endian data), exactly like a fixed-size binary buffer of width
        // bits/8. Arrow decimal layout is a single data buffer of the native
        // width, so this is a zero-copy handoff.
        Column::Decimal(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            buffers.push(c.data.as_ptr() as *const c_void);
        }
        Column::Dictionary(c) => {
            // A dictionary array carries the INDEX buffers (validity, then the
            // i32 indices). The dictionary VALUES live in the array's
            // `dictionary` child, exported recursively below.
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            buffers.push(c.indices.as_ptr() as *const c_void);

            // Safety: an all-zero `ArrowArray` is a valid initial value, the
            // same niche argument as the children in `export_batch_array`.
            // `export_one_column` overwrites every field before any consumer
            // observes it.
            let dict_child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_one_column(batch, &c.values, dict_child);
            dictionary = dict_child;
        }
        // Arrow LargeList: 2 buffers in Arrow order, buffer[0] = validity and
        // buffer[1] = the i64 offsets. Arrays are never null at the array level
        // (ClickHouse forbids `Nullable(Array(T))`), so validity is always null.
        // The `offsets` Vec has length `num_rows + 1` with a leading 0 (including
        // the zero-row `[0]` case) and is handed over verbatim, zero-copy. The
        // flattened element column is the single Arrow child, exported
        // recursively so any element type (including `Nullable`,
        // `LowCardinality`, or a further nested `Array`) is fully exported. The
        // child is owned by this array's private data (in `children`), so
        // `release_array` releases and frees it when this array is released. The
        // `_batch` clone keeps the borrowed `offsets` and element buffers alive
        // until release.
        Column::Array(c) => {
            buffers.push(ptr::null());
            push_offsets(&mut buffers, &c.offsets, EMPTY_OFFSETS_I64.as_ptr());

            // Safety: an all-zero `ArrowArray` is a valid initial value, the same
            // niche argument as the `Dictionary` arm's `dict_child` above;
            // `export_one_column` overwrites every field before any consumer
            // observes it.
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_one_column(batch, &c.values, child);
            children.push(child);
        }
        // Arrow struct (`+s`): 1 buffer, the validity slot. Populated only for
        // a `Nullable(Tuple(...))` (struct validity is independent of the
        // children per the C Data spec; consumers AND them); a plain Tuple
        // pushes null with null_count 0, which the spec permits. One child per
        // element column, in declaration order, exported recursively so any
        // element type (`Nullable`, `LowCardinality`, `Array`, a nested
        // `Tuple`) composes; a zero-element `Tuple()` has no children and its
        // length comes from the explicit `TupleColumn::len`. The children are
        // owned by this array's private data (in `children`), so
        // `release_array` releases and frees them, and the `_batch` clone keeps
        // every borrowed element buffer alive until release.
        Column::Tuple(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            for element in &c.fields {
                // Safety: an all-zero `ArrowArray` is a valid initial value, the
                // same niche argument as the `Dictionary` arm's `dict_child`
                // above; `export_one_column` overwrites every field before any
                // consumer observes it.
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                export_one_column(batch, element, child);
                children.push(child);
            }
        }
        // Map(K, V) exports byte-identically to the `Array` arm above: 2
        // buffers (validity, always null here since a map is never nullable at
        // the map level, then the i64 offsets handed over verbatim) and one
        // child, the flattened `entries` tuple column, exported recursively
        // (its own Tuple arm yields the struct node with the key and value
        // children). Ownership follows the same private-data pattern.
        Column::Map(c) => {
            buffers.push(ptr::null());
            push_offsets(&mut buffers, &c.offsets, EMPTY_OFFSETS_I64.as_ptr());

            // Safety: an all-zero `ArrowArray` is a valid initial value, the
            // same niche argument as the `Dictionary` arm's `dict_child` above;
            // `export_one_column` overwrites every field before any consumer
            // observes it.
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_one_column(batch, &c.entries, child);
            children.push(child);
        }
    }

    let n_children = children.len() as i64;

    let pd = Box::new(ArrayPrivateData {
        buffers,
        children,
        _batch: Arc::clone(batch),
        dictionary,
    });

    let array = &mut *out;
    array.length = length;
    array.null_count = null_count;
    array.offset = 0;
    array.n_buffers = pd.buffers.len() as i64;
    array.buffers = if pd.buffers.is_empty() {
        ptr::null_mut()
    } else {
        pd.buffers.as_ptr() as *mut *const c_void
    };
    array.n_children = n_children;
    // `pd.children` heap buffer is stable across the `Box::into_raw(pd)` move
    // below, so this pointer stays valid until release. Null when there are no
    // children (every arm except the container arms `Array`, `Tuple` with
    // elements, and `Map`).
    array.children = if pd.children.is_empty() {
        ptr::null_mut()
    } else {
        pd.children.as_ptr() as *mut *mut ArrowArray
    };
    array.dictionary = pd.dictionary;
    array.release = Some(release_array);
    array.private_data = Box::into_raw(pd) as *mut c_void;
}

fn push_primitive_buffers<T>(
    buffers: &mut Vec<*const c_void>,
    values: &[T],
    validity: &Option<crate::bitmap::Bitmap>,
) {
    match validity {
        Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
        None => buffers.push(ptr::null()),
    }
    buffers.push(values.as_ptr() as *const c_void);
}

/// A single zero offset: the offsets buffer for a length-0 variable-length array
/// whose column carries an empty `offsets` `Vec`.
///
/// The Arrow C Data Interface requires the offsets buffer of a variable-size
/// binary or list array to hold `length + 1` elements starting with 0 (Columnar
/// format spec, "Variable-size Binary Layout"). For a length-0 array that is
/// exactly one element, so the buffer's byte size is nonzero and its pointer MAY
/// NOT be null: the C Data Interface allows a null buffer pointer only when the
/// buffer's byte size would be 0 (see ArrowArray.buffers). A hand-built column
/// can leave `offsets` empty (a zero-capacity `Vec` whose `as_ptr()` is a
/// dangling pointer with no leading 0); the decoder always emits `[0]`, but
/// `Column` fields are public. Exporting these shared 'static single zeros keeps
/// the exported buffer spec-compliant and interoperable with strict consumers
/// (pyarrow, arrow-rs) instead of handing out a dangling pointer. A 'static
/// outlives every consumer and is never freed by a release callback, which frees
/// only the boxed private data and releases child arrays.
static EMPTY_OFFSETS_I32: [i32; 1] = [0];
static EMPTY_OFFSETS_I64: [i64; 1] = [0];

/// Push a variable-length column's offsets buffer, substituting a 'static single
/// zero when `offsets` is empty so the export never hands out the dangling
/// `as_ptr()` of a zero-capacity `Vec` and always satisfies Arrow's `length + 1`
/// offsets contract (see [`EMPTY_OFFSETS_I32`] / [`EMPTY_OFFSETS_I64`]).
fn push_offsets<T>(buffers: &mut Vec<*const c_void>, offsets: &[T], empty: *const T) {
    let ptr = if offsets.is_empty() {
        empty
    } else {
        offsets.as_ptr()
    };
    buffers.push(ptr as *const c_void);
}

/// # Safety
///
/// `out` must be a valid, writable pointer to an `ArrowArray`, normally a
/// zeroed struct. The exported array borrows the batch buffers and keeps the
/// `Arc<ColBatch>` alive until `out` is freed through its `release` callback.
pub unsafe fn export_batch_array(batch: &Arc<ColBatch>, out: *mut ArrowArray) {
    let num_rows = batch.num_rows as i64;
    let n_children = batch.num_columns() as i64;

    let mut child_arrays: Vec<*mut ArrowArray> = Vec::with_capacity(n_children as usize);
    for i in 0..batch.num_columns() {
        // Safety: an all-zero `ArrowArray` is a valid initial value, same as in
        // `export_schema`: pointers null, integers zero, `Option<extern fn>` the
        // all-zero `None` niche. `export_column_array` overwrites every field.
        let child_array = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
        export_column_array(batch, i, child_array);
        child_arrays.push(child_array);
    }

    let pd = Box::new(ArrayPrivateData {
        buffers: vec![ptr::null()],
        children: child_arrays.clone(),
        _batch: Arc::clone(batch),
        dictionary: ptr::null_mut(),
    });

    let array = &mut *out;
    array.length = num_rows;
    array.null_count = 0;
    array.offset = 0;
    array.n_buffers = 1;
    array.buffers = pd.buffers.as_ptr() as *mut *const c_void;
    array.n_children = n_children;
    array.children = pd.children.as_ptr() as *mut *mut ArrowArray;
    array.dictionary = ptr::null_mut();
    array.release = Some(release_array);
    array.private_data = Box::into_raw(pd) as *mut c_void;
}

// ---------------------------------------------------------------------------
// Stream export
// ---------------------------------------------------------------------------

unsafe extern "C" fn stream_get_schema(
    stream: *mut ArrowArrayStream,
    out: *mut ArrowSchema,
) -> i32 {
    let s = &mut *stream;
    let pd = &*(s.private_data as *const StreamPrivateData);
    export_schema(&pd.schema, out);
    0
}

unsafe extern "C" fn stream_get_next(stream: *mut ArrowArrayStream, out: *mut ArrowArray) -> i32 {
    let s = &mut *stream;
    let pd = &mut *(s.private_data as *mut StreamPrivateData);
    match pd.chunks.next() {
        Some(batch) => {
            export_batch_array(&batch, out);
            0
        }
        None => {
            // End of stream: mark the array released-and-empty per the spec.
            let a = &mut *out;
            a.release = None;
            0
        }
    }
}

unsafe extern "C" fn stream_get_last_error(stream: *mut ArrowArrayStream) -> *const c_char {
    let s = &*stream;
    let pd = &*(s.private_data as *const StreamPrivateData);
    pd.error_msg.as_ptr()
}

/// Export a sequence of chunks as a single Arrow C Stream — one record batch
/// per chunk. `schema` is used for `get_schema` and remains valid even when
/// `chunks` is empty (a zero-row result still advertises its columns).
///
/// # Safety
///
/// `out` must be a valid, writable pointer to an `ArrowArrayStream`, normally a
/// zeroed struct. On return `out` owns its data and must be freed through its
/// `release` callback per the Arrow C Data Interface.
pub unsafe fn export_chunks_to_stream(
    schema: Schema,
    chunks: Vec<Arc<ColBatch>>,
    out: *mut ArrowArrayStream,
) {
    let pd = Box::new(StreamPrivateData {
        schema,
        chunks: chunks.into_iter(),
        error_msg: CString::new("").unwrap(),
    });

    let s = &mut *out;
    s.get_schema = Some(stream_get_schema);
    s.get_next = Some(stream_get_next);
    s.get_last_error = Some(stream_get_last_error);
    s.release = Some(release_stream);
    s.private_data = Box::into_raw(pd) as *mut c_void;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
