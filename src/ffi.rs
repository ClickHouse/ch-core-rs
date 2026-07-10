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
use crate::schema::{ChType, Schema};

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
        ChType::String => "u".into(),
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
        Column::Date(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Date32(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::DateTime(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::DateTime64(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Time(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Time64(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Utf8(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            buffers.push(c.offsets.as_ptr() as *const c_void);
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
            buffers.push(c.offsets.as_ptr() as *const c_void);

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
            buffers.push(c.offsets.as_ptr() as *const c_void);

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
    array.buffers = pd.buffers.as_ptr() as *mut *const c_void;
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
mod tests {
    use super::*;
    use crate::batch::ColBatch;
    use crate::bitmap::Bitmap;
    use crate::column::{
        ArrayColumn, Column, DictionaryColumn, PrimitiveColumn, TupleColumn, Utf8Column,
    };
    use crate::schema::{ChType, Field, GeoKind, Schema};
    use std::ffi::CStr;

    fn make_test_batch() -> Arc<ColBatch> {
        let schema = Schema::new(vec![
            Field {
                name: "i".into(),
                ch_type: ChType::Int64,
            },
            Field {
                name: "f".into(),
                ch_type: ChType::Float64,
            },
            Field {
                name: "s".into(),
                ch_type: ChType::String,
            },
        ]);
        let columns = vec![
            Column::Int64(PrimitiveColumn::new(vec![10, 20])),
            Column::Float64(PrimitiveColumn::new(vec![1.5, 2.5])),
            Column::Utf8(Utf8Column::new(vec![0, 2, 5], b"abcde".to_vec())),
        ];
        Arc::new(ColBatch::new(schema, columns, 2))
    }

    #[test]
    fn test_export_schema_format_strings() {
        let batch = make_test_batch();
        unsafe {
            let mut schema: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema);

            let fmt = CStr::from_ptr(schema.format).to_str().unwrap();
            assert_eq!(fmt, "+s");
            assert_eq!(schema.n_children, 3);

            let c0 = &**schema.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "l");

            let c1 = &**schema.children.add(1);
            assert_eq!(CStr::from_ptr(c1.format).to_str().unwrap(), "g");

            let c2 = &**schema.children.add(2);
            assert_eq!(CStr::from_ptr(c2.format).to_str().unwrap(), "u");

            (schema.release.unwrap())(&mut schema);
        }
    }

    #[test]
    fn test_export_array_buffers() {
        let batch = make_test_batch();
        unsafe {
            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);

            assert_eq!(array.length, 2);
            assert_eq!(array.n_children, 3);

            let c0 = &**array.children.add(0);
            assert_eq!(c0.length, 2);
            assert_eq!(c0.n_buffers, 2);
            assert!((*c0.buffers.add(0)).is_null()); // non-nullable
            let data_ptr = *c0.buffers.add(1) as *const i64;
            assert_eq!(*data_ptr, 10);
            assert_eq!(*data_ptr.add(1), 20);

            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_export_bool_column() {
        use crate::column::BoolColumn;

        let schema = Schema::new(vec![Field {
            name: "b".into(),
            ch_type: ChType::Bool,
        }]);
        let columns = vec![Column::Bool(BoolColumn::from_wire_bytes(&[1, 0, 1]))];
        let batch = Arc::new(ColBatch::new(schema, columns, 3));

        unsafe {
            let mut schema: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema);

            let c0 = &**schema.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "b");

            (schema.release.unwrap())(&mut schema);

            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);

            let c0 = &**array.children.add(0);
            assert_eq!(c0.length, 3);
            assert_eq!(c0.n_buffers, 2);

            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_export_fixed_binary_column() {
        use crate::column::FixedBinaryColumn;

        let schema = Schema::new(vec![Field {
            name: "fs".into(),
            ch_type: ChType::FixedString(4),
        }]);
        let columns = vec![Column::FixedBinary(FixedBinaryColumn::new(
            b"abcdwxyz".to_vec(),
            4,
        ))];
        let batch = Arc::new(ColBatch::new(schema, columns, 2));

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);

            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "w:4");

            (schema_out.release.unwrap())(&mut schema_out);
        }
    }

    fn make_dictionary_batch(nullable: bool) -> Arc<ColBatch> {
        use crate::bitmap::Bitmap;
        use crate::column::DictionaryColumn;

        let inner = if nullable {
            ChType::Nullable(Box::new(ChType::String))
        } else {
            ChType::String
        };
        let schema = Schema::new(vec![Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(inner)),
        }]);
        // values dictionary: 3 entries, indices over 4 rows.
        let values = Column::Utf8(Utf8Column::new(
            vec![0, 5, 11, 17],
            b"user_user_1user_2".to_vec(),
        ));
        let dict = if nullable {
            // Row 1 is null (validity bit 0); the rest are valid.
            let validity = Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00, 0x00]);
            DictionaryColumn::new_nullable(vec![0, 0, 1, 2], values, validity)
        } else {
            DictionaryColumn::new(vec![0, 1, 2, 0], values)
        };
        let columns = vec![Column::Dictionary(dict)];
        Arc::new(ColBatch::new(schema, columns, 4))
    }

    #[test]
    fn test_export_low_cardinality_schema() {
        // Dictionary schema: the field format is the index type (`i`), the
        // dictionary child carries the value type (`u`), and the nullable flag
        // tracks the inner Nullable.
        let batch = make_dictionary_batch(true);
        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);

            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "i");
            assert_eq!(c0.flags & 2, 2, "nullable flag set for LC(Nullable(...))");
            assert!(!c0.dictionary.is_null(), "dictionary child present");
            let dict = &*c0.dictionary;
            assert_eq!(CStr::from_ptr(dict.format).to_str().unwrap(), "u");

            (schema_out.release.unwrap())(&mut schema_out);
        }

        // A non-nullable LowCardinality has the flag clear.
        let batch = make_dictionary_batch(false);
        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let c0 = &**schema_out.children.add(0);
            assert_eq!(c0.flags & 2, 0, "nullable flag clear for LC(String)");
            (schema_out.release.unwrap())(&mut schema_out);
        }
    }

    #[test]
    fn test_export_low_cardinality_array() {
        // Dictionary array: 2 index buffers (validity + i32 indices), a length
        // equal to the row count, and a dictionary child holding the values.
        let batch = make_dictionary_batch(true);
        unsafe {
            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);

            let c0 = &**array.children.add(0);
            assert_eq!(c0.length, 4);
            assert_eq!(c0.null_count, 1);
            assert_eq!(c0.n_buffers, 2);
            assert!(!(*c0.buffers.add(0)).is_null(), "validity buffer present");
            let idx = *c0.buffers.add(1) as *const i32;
            assert_eq!(*idx, 0);
            assert_eq!(*idx.add(2), 1);

            assert!(!c0.dictionary.is_null(), "dictionary child array present");
            let dict = &*c0.dictionary;
            assert_eq!(dict.length, 3, "dictionary holds 3 entries");
            assert_eq!(dict.n_buffers, 3, "utf8 values: validity, offsets, data");

            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_arrow_format_low_cardinality() {
        assert_eq!(
            arrow_format(&ChType::LowCardinality(Box::new(ChType::String))),
            "i"
        );
        assert_eq!(
            arrow_format(&ChType::LowCardinality(Box::new(ChType::Nullable(
                Box::new(ChType::String)
            )))),
            "i"
        );
    }

    #[test]
    fn test_export_low_cardinality_saf_nullable_string_schema() {
        use crate::column::DictionaryColumn;

        // LowCardinality(SimpleAggregateFunction(anyLast, Nullable(String))) must
        // export exactly like LowCardinality(Nullable(String)): field format `i`,
        // the index-level nullable flag set (nulls live in the index validity),
        // and a non-nullable `u` (String) dictionary child. The SAF chain between
        // the LC and its removeNullable Nullable is resolved through the shared
        // `low_cardinality_dict_value_type` helper.
        let schema = Schema::new(vec![Field {
            name: "lc_nsaf".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::SimpleAggregateFunction {
                func: "anyLast".into(),
                inner: Box::new(ChType::Nullable(Box::new(ChType::String))),
            })),
        }]);
        let values = Column::Utf8(crate::column::Utf8Column::new(vec![0], vec![]));
        let dict = DictionaryColumn::new_nullable(
            vec![],
            values,
            crate::bitmap::Bitmap::from_ch_null_map(&[]),
        );
        let batch = Arc::new(ColBatch::new(schema, vec![Column::Dictionary(dict)], 0));

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "i");
            assert_eq!(
                c0.flags & 2,
                2,
                "nullable flag set for LC(SAF(_, Nullable(String)))"
            );
            assert!(!c0.dictionary.is_null(), "dictionary child present");
            let dict_schema = &*c0.dictionary;
            assert_eq!(CStr::from_ptr(dict_schema.format).to_str().unwrap(), "u");
            assert_eq!(
                dict_schema.flags & 2,
                0,
                "dictionary values are non-nullable; nulls live in the index validity"
            );
            (schema_out.release.unwrap())(&mut schema_out);
        }
    }

    #[test]
    fn test_export_low_cardinality_uint32() {
        use crate::column::DictionaryColumn;

        // A non-String dictionary value type still exports as dictionary(i32, T):
        // the field format is the index type `i`, and the dictionary child format
        // is the value type, here `I` (uint32). The dictionary export path is
        // generic over the value column, so a UInt32 values column flows through
        // unchanged. This mirrors the String case but pins the child format for a
        // primitive inner.
        let schema = Schema::new(vec![Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::UInt32)),
        }]);
        // Dictionary slot 0 is the reserved default; the 3 rows index into 1..=3.
        let values = Column::UInt32(PrimitiveColumn::new(vec![0, 13, 79, 4_294_967_295]));
        let dict = DictionaryColumn::new(vec![1, 2, 3], values);
        let batch = Arc::new(ColBatch::new(schema, vec![Column::Dictionary(dict)], 3));

        unsafe {
            // Schema: field format `i`, flag clear (non-nullable), child `I`.
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "i");
            assert_eq!(c0.flags & 2, 0, "non-nullable LC has the flag clear");
            assert!(!c0.dictionary.is_null(), "dictionary child present");
            let dict_schema = &*c0.dictionary;
            assert_eq!(CStr::from_ptr(dict_schema.format).to_str().unwrap(), "I");
            (schema_out.release.unwrap())(&mut schema_out);

            // Array: 2 index buffers (validity, i32 indices), dictionary child
            // holding 4 uint32 entries with 2 buffers (validity, values).
            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let c0 = &**array.children.add(0);
            assert_eq!(c0.length, 3);
            assert_eq!(c0.n_buffers, 2);
            let idx = *c0.buffers.add(1) as *const i32;
            assert_eq!(*idx, 1);
            assert_eq!(*idx.add(2), 3);
            assert!(!c0.dictionary.is_null(), "dictionary child array present");
            let dict_array = &*c0.dictionary;
            assert_eq!(dict_array.length, 4, "dictionary holds 4 entries");
            assert_eq!(dict_array.n_buffers, 2, "uint32 values: validity, values");
            let vals = *dict_array.buffers.add(1) as *const u32;
            assert_eq!(*vals.add(1), 13);
            assert_eq!(*vals.add(3), 4_294_967_295);
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_arrow_format_uuid_ipv4_ipv6() {
        // IPv4 exports as Arrow uint32 (`I`), zero-copy. UUID and IPv6 export as
        // Arrow fixed-size binary of width 16 (`w:16`); the crate emits plain
        // `w:16`, not the `arrow.uuid` extension.
        assert_eq!(arrow_format(&ChType::Ipv4), "I");
        assert_eq!(arrow_format(&ChType::Uuid), "w:16");
        assert_eq!(arrow_format(&ChType::Ipv6), "w:16");
        // LowCardinality(UUID) exports as dictionary(i32, w:16): the field format
        // is the index type `i` and the dictionary child carries `w:16`.
        assert_eq!(
            arrow_format(&ChType::LowCardinality(Box::new(ChType::Uuid))),
            "i"
        );
    }

    #[test]
    fn test_export_uuid_ipv4_ipv6_schema_and_buffers() {
        use crate::column::FixedBinaryColumn;

        let schema = Schema::new(vec![
            Field {
                name: "ip4".into(),
                ch_type: ChType::Ipv4,
            },
            Field {
                name: "u".into(),
                ch_type: ChType::Uuid,
            },
            Field {
                name: "ip6".into(),
                ch_type: ChType::Ipv6,
            },
        ]);
        let uuid_bytes = vec![0xaau8; 32]; // 2 rows of width 16
        let ip6_bytes = vec![0xbbu8; 32];
        let columns = vec![
            Column::Ipv4(PrimitiveColumn::new(vec![3221226219u32, 0])),
            Column::Uuid(FixedBinaryColumn::new(uuid_bytes, 16)),
            Column::Ipv6(FixedBinaryColumn::new(ip6_bytes, 16)),
        ];
        let batch = Arc::new(ColBatch::new(schema, columns, 2));

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);

            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "I");
            let c1 = &**schema_out.children.add(1);
            assert_eq!(CStr::from_ptr(c1.format).to_str().unwrap(), "w:16");
            let c2 = &**schema_out.children.add(2);
            assert_eq!(CStr::from_ptr(c2.format).to_str().unwrap(), "w:16");
            (schema_out.release.unwrap())(&mut schema_out);

            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);

            // IPv4: 2 buffers (validity, u32 values).
            let a0 = &**array.children.add(0);
            assert_eq!(a0.length, 2);
            assert_eq!(a0.n_buffers, 2);
            let ip4 = *a0.buffers.add(1) as *const u32;
            assert_eq!(*ip4, 3221226219);

            // UUID and IPv6: 2 buffers (validity, data), like FixedString.
            let a1 = &**array.children.add(1);
            assert_eq!(a1.length, 2);
            assert_eq!(a1.n_buffers, 2);
            let a2 = &**array.children.add(2);
            assert_eq!(a2.length, 2);
            assert_eq!(a2.n_buffers, 2);

            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_export_low_cardinality_uuid_child_format() {
        use crate::column::{DictionaryColumn, FixedBinaryColumn};

        // LowCardinality(UUID) exports as dictionary(i32, w:16): field format `i`,
        // dictionary child format `w:16`.
        let schema = Schema::new(vec![Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::Uuid)),
        }]);
        let values = Column::Uuid(FixedBinaryColumn::new(vec![0u8; 48], 16)); // 3 entries
        let dict = DictionaryColumn::new(vec![1, 2, 1], values);
        let batch = Arc::new(ColBatch::new(schema, vec![Column::Dictionary(dict)], 3));

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "i");
            assert!(!c0.dictionary.is_null(), "dictionary child present");
            let dict_schema = &*c0.dictionary;
            assert_eq!(CStr::from_ptr(dict_schema.format).to_str().unwrap(), "w:16");
            (schema_out.release.unwrap())(&mut schema_out);

            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let c0 = &**array.children.add(0);
            assert!(!c0.dictionary.is_null(), "dictionary child array present");
            let dict_array = &*c0.dictionary;
            assert_eq!(dict_array.length, 3, "dictionary holds 3 entries");
            assert_eq!(dict_array.n_buffers, 2, "fixed binary: validity, data");
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_stream_yields_one_batch_then_ends() {
        let batch = make_test_batch();
        let schema = batch.schema.clone();
        unsafe {
            let mut stream: ArrowArrayStream = std::mem::zeroed();
            export_chunks_to_stream(schema, vec![batch], &mut stream);

            let mut schema: ArrowSchema = std::mem::zeroed();
            let rc = (stream.get_schema.unwrap())(&mut stream, &mut schema);
            assert_eq!(rc, 0);
            (schema.release.unwrap())(&mut schema);

            let mut array: ArrowArray = std::mem::zeroed();
            let rc = (stream.get_next.unwrap())(&mut stream, &mut array);
            assert_eq!(rc, 0);
            assert!(array.release.is_some());
            assert_eq!(array.length, 2);
            (array.release.unwrap())(&mut array);

            let mut array2: ArrowArray = std::mem::zeroed();
            let rc = (stream.get_next.unwrap())(&mut stream, &mut array2);
            assert_eq!(rc, 0);
            assert!(array2.release.is_none());

            (stream.release.unwrap())(&mut stream);
        }
    }

    #[test]
    fn test_arrow_format_and_export_enum() {
        // Enum8 exports as Arrow int8 (`c`), Enum16 as int16 (`s`): the
        // underlying signed int buffer, zero-copy, like Int8/Int16. No
        // dictionary child (ClickHouse enum values are arbitrary signed ints,
        // not 0..N-1 indices).
        let e8 = ChType::Enum8 {
            variants: vec![("pending".to_string(), 1), ("closed".to_string(), -1)],
        };
        let e16 = ChType::Enum16 {
            variants: vec![("a".to_string(), 1)],
        };
        assert_eq!(arrow_format(&e8), "c");
        assert_eq!(arrow_format(&e16), "s");

        let schema = Schema::new(vec![
            Field {
                name: "e8".into(),
                ch_type: e8,
            },
            Field {
                name: "e16".into(),
                ch_type: e16,
            },
        ]);
        let columns = vec![
            Column::Enum8(PrimitiveColumn::new(vec![1i8, -1, 1])),
            Column::Enum16(PrimitiveColumn::new(vec![1i16, 1, 1])),
        ];
        let batch = Arc::new(ColBatch::new(schema, columns, 3));

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "c");
            assert!(c0.dictionary.is_null(), "enum has no dictionary child");
            let c1 = &**schema_out.children.add(1);
            assert_eq!(CStr::from_ptr(c1.format).to_str().unwrap(), "s");
            (schema_out.release.unwrap())(&mut schema_out);

            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let a0 = &**array.children.add(0);
            assert_eq!(a0.length, 3);
            assert_eq!(a0.n_buffers, 2);
            assert!((*a0.buffers.add(0)).is_null(), "non-nullable validity null");
            let vals = *a0.buffers.add(1) as *const i8;
            assert_eq!(*vals, 1);
            assert_eq!(*vals.add(1), -1);
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_arrow_format_and_export_decimal() {
        use crate::column::DecimalColumn;

        // Decimal exports with the Arrow decimal format string. The 128-bit case
        // is bare `d:P,S`; the other widths carry the bit width as the third
        // field (`d:P,S,bits`). The buffer is the native-width contiguous
        // little-endian data, zero-copy, never widened to 128.
        assert_eq!(
            arrow_format(&ChType::Decimal {
                precision: 9,
                scale: 4,
                bits: 32,
            }),
            "d:9,4,32"
        );
        assert_eq!(
            arrow_format(&ChType::Decimal {
                precision: 18,
                scale: 9,
                bits: 64,
            }),
            "d:18,9,64"
        );
        assert_eq!(
            arrow_format(&ChType::Decimal {
                precision: 38,
                scale: 10,
                bits: 128,
            }),
            "d:38,10"
        );
        assert_eq!(
            arrow_format(&ChType::Decimal {
                precision: 50,
                scale: 10,
                bits: 256,
            }),
            "d:50,10,256"
        );

        // Export a Decimal128 column (2 rows): schema format `d:20,2`, array with
        // 2 buffers (validity null for non-nullable, then the 32-byte data).
        let schema = Schema::new(vec![Field {
            name: "d".into(),
            ch_type: ChType::Decimal {
                precision: 20,
                scale: 2,
                bits: 128,
            },
        }]);
        let mut data = vec![0u8; 32]; // 2 rows of width 16
        data[0] = 0x01; // row 0 unscaled = 1
        let columns = vec![Column::Decimal(DecimalColumn::new(data, 16, 20, 2))];
        let batch = Arc::new(ColBatch::new(schema, columns, 2));

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "d:20,2");
            assert!(c0.dictionary.is_null(), "decimal has no dictionary child");
            (schema_out.release.unwrap())(&mut schema_out);

            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let a0 = &**array.children.add(0);
            assert_eq!(a0.length, 2);
            assert_eq!(a0.n_buffers, 2);
            assert!((*a0.buffers.add(0)).is_null(), "non-nullable validity null");
            let bytes = *a0.buffers.add(1) as *const u8;
            assert_eq!(*bytes, 0x01, "row 0 first byte is the unscaled 1");
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_arrow_format_temporal() {
        // Zero-copy temporal export: Date32 and the {0,3,6,9}-precision
        // DateTime64 map to real Arrow temporal formats; Date, DateTime, and
        // other-precision DateTime64 expose the raw integer.
        assert_eq!(arrow_format(&ChType::Date), "S");
        assert_eq!(arrow_format(&ChType::Date32), "tdD");
        assert_eq!(arrow_format(&ChType::DateTime { timezone: None }), "I");
        assert_eq!(
            arrow_format(&ChType::DateTime64 {
                precision: 3,
                timezone: Some("UTC".to_string())
            }),
            "tsm:UTC"
        );
        assert_eq!(
            arrow_format(&ChType::DateTime64 {
                precision: 2,
                timezone: None
            }),
            "l"
        );
        // ClickHouse times can be negative or exceed 24 hours, so every
        // precision exports as the raw signed integer rather than Arrow Time.
        assert_eq!(arrow_format(&ChType::Time), "i");
        for precision in 0..=9 {
            assert_eq!(arrow_format(&ChType::Time64 { precision }), "l");
        }
    }

    #[test]
    fn test_export_time_buffers() {
        let schema = Schema::new(vec![
            Field {
                name: "t".into(),
                ch_type: ChType::Time,
            },
            Field {
                name: "t64".into(),
                ch_type: ChType::Time64 { precision: 3 },
            },
        ]);
        let batch = Arc::new(ColBatch::new(
            schema,
            vec![
                Column::Time(PrimitiveColumn::new(vec![-3_599_999, 0, 3_599_999])),
                Column::Time64(PrimitiveColumn::new(vec![-3_599_999_999, 0, 3_599_999_999])),
            ],
            3,
        ));

        // Safety: the zeroed FFI outputs are writable and the batch remains
        // alive until each matching release callback is invoked below.
        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let t_schema = &**schema_out.children.add(0);
            let t64_schema = &**schema_out.children.add(1);
            assert_eq!(CStr::from_ptr(t_schema.format).to_str().unwrap(), "i");
            assert_eq!(CStr::from_ptr(t64_schema.format).to_str().unwrap(), "l");
            (schema_out.release.unwrap())(&mut schema_out);

            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let t = &**array.children.add(0);
            let t64 = &**array.children.add(1);
            assert_eq!(t.length, 3);
            assert_eq!(t.n_buffers, 2);
            assert!((*t.buffers.add(0)).is_null());
            assert_eq!(*(*t.buffers.add(1) as *const i32), -3_599_999);
            assert_eq!(t64.length, 3);
            assert_eq!(t64.n_buffers, 2);
            assert!((*t64.buffers.add(0)).is_null());
            assert_eq!(*(*t64.buffers.add(1) as *const i64), -3_599_999_999);
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_export_nullable_time_buffers_zero_copy() {
        let t_validity = Bitmap::from_ch_null_map(&[0, 1, 0]);
        let t64_validity = Bitmap::from_ch_null_map(&[0, 1, 0]);
        let t_values = vec![-13i32, 0, 79];
        let t64_values = vec![-13_000_000i64, 0, 79_000_000];
        let t_validity_ptr = t_validity.as_bytes().as_ptr() as *const c_void;
        let t64_validity_ptr = t64_validity.as_bytes().as_ptr() as *const c_void;
        let t_values_ptr = t_values.as_ptr() as *const c_void;
        let t64_values_ptr = t64_values.as_ptr() as *const c_void;

        let schema = Schema::new(vec![
            Field {
                name: "nt".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Time)),
            },
            Field {
                name: "nt64".into(),
                ch_type: ChType::Nullable(Box::new(ChType::Time64 { precision: 6 })),
            },
        ]);
        let batch = Arc::new(ColBatch::new(
            schema,
            vec![
                Column::Time(PrimitiveColumn::new_nullable(t_values, t_validity)),
                Column::Time64(PrimitiveColumn::new_nullable(t64_values, t64_validity)),
            ],
            3,
        ));

        // Safety: the zeroed FFI outputs are writable and the batch remains
        // alive until each matching release callback is invoked below.
        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            assert_eq!((**schema_out.children.add(0)).flags, 2);
            assert_eq!((**schema_out.children.add(1)).flags, 2);
            (schema_out.release.unwrap())(&mut schema_out);

            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let t = &**array.children.add(0);
            let t64 = &**array.children.add(1);
            assert_eq!(t.null_count, 1);
            assert_eq!(*t.buffers.add(0), t_validity_ptr);
            assert_eq!(*t.buffers.add(1), t_values_ptr);
            assert_eq!(t64.null_count, 1);
            assert_eq!(*t64.buffers.add(0), t64_validity_ptr);
            assert_eq!(*t64.buffers.add(1), t64_values_ptr);
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_export_schema_tolerates_nul_in_wire_names() {
        // A column name and a timezone both come from the untrusted wire stream
        // and are only UTF-8 validated, so either can carry an interior NUL.
        // Exporting must not panic (a panic could unwind across the extern "C"
        // stream callbacks). The NUL bytes are stripped from the C strings.
        let schema = Schema::new(vec![
            Field {
                name: "a\0b".into(),
                ch_type: ChType::Int32,
            },
            Field {
                name: "ts".into(),
                ch_type: ChType::DateTime64 {
                    precision: 3,
                    timezone: Some("U\0TC".into()),
                },
            },
        ]);
        unsafe {
            let mut out: ArrowSchema = std::mem::zeroed();
            export_schema(&schema, &mut out);

            let c0 = &**out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.name).to_str().unwrap(), "ab");

            let c1 = &**out.children.add(1);
            assert_eq!(CStr::from_ptr(c1.format).to_str().unwrap(), "tsm:UTC");

            (out.release.unwrap())(&mut out);
        }
    }

    #[test]
    fn test_stream_release_if_set_is_idempotent() {
        let batch = make_test_batch();
        let schema = batch.schema.clone();
        unsafe {
            let mut stream: ArrowArrayStream = std::mem::zeroed();
            export_chunks_to_stream(schema, vec![batch], &mut stream);

            stream.release_if_set();
            assert!(stream.release.is_none());
            assert!(stream.private_data.is_null());

            // Second call sees a cleared callback and does nothing.
            stream.release_if_set();
        }
    }

    #[test]
    fn test_stream_yields_multiple_chunks() {
        // Three separate chunks must come out as three record batches.
        let schema = make_test_batch().schema.clone();
        let chunks = vec![make_test_batch(), make_test_batch(), make_test_batch()];
        unsafe {
            let mut stream: ArrowArrayStream = std::mem::zeroed();
            export_chunks_to_stream(schema, chunks, &mut stream);

            let mut count = 0;
            loop {
                let mut array: ArrowArray = std::mem::zeroed();
                let rc = (stream.get_next.unwrap())(&mut stream, &mut array);
                assert_eq!(rc, 0);
                if array.release.is_none() {
                    break;
                }
                assert_eq!(array.length, 2);
                count += 1;
                (array.release.unwrap())(&mut array);
            }
            assert_eq!(count, 3);

            (stream.release.unwrap())(&mut stream);
        }
    }

    #[test]
    fn test_arrow_format_array() {
        // Array(T) is an Arrow LargeList: `+L`. The element type is NOT in this
        // format string, it lives in the `item` child schema.
        assert_eq!(arrow_format(&ChType::Array(Box::new(ChType::Int32))), "+L");
        // Nesting does not change the top-level format string.
        assert_eq!(
            arrow_format(&ChType::Array(Box::new(ChType::Array(Box::new(
                ChType::Int32
            ))))),
            "+L"
        );
    }

    #[test]
    fn test_export_array_int32_schema() {
        // Schema of Array(Int32): field format `+L`, flags clear (array level is
        // never nullable), one child named `item` with the element format `i`.
        let schema = Schema::new(vec![Field {
            name: "arr".into(),
            ch_type: ChType::Array(Box::new(ChType::Int32)),
        }]);

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&schema, &mut schema_out);

            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
            assert_eq!(c0.flags & 2, 0, "array level is not nullable");
            assert_eq!(c0.n_children, 1);
            assert!(c0.dictionary.is_null(), "array field has no dictionary");

            let item = &**c0.children.add(0);
            assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "i");
            assert_eq!(CStr::from_ptr(item.name).to_str().unwrap(), "item");
            assert_eq!(item.flags & 2, 0, "plain Int32 element is not nullable");

            (schema_out.release.unwrap())(&mut schema_out);
        }
    }

    #[test]
    fn test_export_array_int32_buffers() {
        // Array(Int32) over 3 rows including an empty row (row 1). LargeList:
        // 2 buffers (null validity, i64 offsets), 1 child holding the flattened
        // elements.
        // Rows: [[10,20,30], [], [40]] -> offsets [0,3,3,4], values [10,20,30,40].
        let schema = Schema::new(vec![Field {
            name: "arr".into(),
            ch_type: ChType::Array(Box::new(ChType::Int32)),
        }]);
        let values = Column::Int32(PrimitiveColumn::new(vec![10, 20, 30, 40]));
        let col = Column::Array(crate::column::ArrayColumn::new(vec![0, 3, 3, 4], values));
        let batch = Arc::new(ColBatch::new(schema, vec![col], 3));

        unsafe {
            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);

            let c0 = &**array.children.add(0);
            assert_eq!(c0.length, 3, "array length is the row count");
            assert_eq!(c0.null_count, 0);
            assert_eq!(c0.n_buffers, 2, "LargeList: validity + offsets");
            assert!(
                (*c0.buffers.add(0)).is_null(),
                "array level validity is always null"
            );
            let offsets = *c0.buffers.add(1) as *const i64;
            assert_eq!(*offsets, 0, "leading 0");
            assert_eq!(*offsets.add(1), 3);
            assert_eq!(*offsets.add(2), 3, "empty row -> repeated offset");
            assert_eq!(*offsets.add(3), 4, "last offset == total elements");

            assert_eq!(c0.n_children, 1);
            let item = &**c0.children.add(0);
            assert_eq!(item.length, 4, "element count == last offset");
            assert_eq!(item.n_buffers, 2, "int32 element: validity + values");
            let vals = *item.buffers.add(1) as *const i32;
            assert_eq!(*vals, 10);
            assert_eq!(*vals.add(3), 40);

            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_export_array_nullable_int32() {
        use crate::bitmap::Bitmap;

        // Array(Nullable(Int32)): the `item` child schema carries the nullable
        // flag, the element array's validity is non-null with a matching
        // null_count, and the array-level validity stays null.
        // Rows: [[10, null], [30]] -> offsets [0,2,3], values [10, _, 30] with
        // element index 1 null.
        let schema = Schema::new(vec![Field {
            name: "arr".into(),
            ch_type: ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::Int32)))),
        }]);
        // ClickHouse null map: 1 = null. Element index 1 is null.
        let validity = Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]);
        let values = Column::Int32(PrimitiveColumn::new_nullable(vec![10, 0, 30], validity));
        let col = Column::Array(crate::column::ArrayColumn::new(vec![0, 2, 3], values));
        let batch = Arc::new(ColBatch::new(schema, vec![col], 2));

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
            assert_eq!(c0.flags & 2, 0, "array level not nullable");
            let item = &**c0.children.add(0);
            assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "i");
            assert_eq!(item.flags & 2, 2, "nullable element flag set");
            (schema_out.release.unwrap())(&mut schema_out);

            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let c0 = &**array.children.add(0);
            assert_eq!(c0.length, 2);
            assert_eq!(c0.null_count, 0, "array level has no nulls");
            assert!(
                (*c0.buffers.add(0)).is_null(),
                "array level validity stays null"
            );
            let item = &**c0.children.add(0);
            assert_eq!(item.length, 3);
            assert_eq!(item.null_count, 1, "one null element");
            assert!(
                !(*item.buffers.add(0)).is_null(),
                "element validity buffer present"
            );
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_export_array_low_cardinality_string() {
        use crate::column::DictionaryColumn;

        // Array(LowCardinality(String)): the `item` child schema is a dictionary
        // (format `i`) with a non-null dictionary child of format `u`; the
        // element array carries the index buffers plus a dictionary child.
        // Rows: [["user_1"], ["user_2","user_1"]] -> offsets [0,1,3].
        // Dictionary values ["user_1","user_2"], indices [0,1,0].
        let schema = Schema::new(vec![Field {
            name: "arr".into(),
            ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
        }]);
        let dict_values = Column::Utf8(Utf8Column::new(vec![0, 6, 12], b"user_1user_2".to_vec()));
        let dict = DictionaryColumn::new(vec![0, 1, 0], dict_values);
        let col = Column::Array(crate::column::ArrayColumn::new(
            vec![0, 1, 3],
            Column::Dictionary(dict),
        ));
        let batch = Arc::new(ColBatch::new(schema, vec![col], 2));

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
            let item = &**c0.children.add(0);
            assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "i");
            assert!(!item.dictionary.is_null(), "dictionary child present");
            let dict_schema = &*item.dictionary;
            assert_eq!(CStr::from_ptr(dict_schema.format).to_str().unwrap(), "u");
            (schema_out.release.unwrap())(&mut schema_out);

            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let c0 = &**array.children.add(0);
            assert_eq!(c0.length, 2);
            let item = &**c0.children.add(0);
            assert_eq!(item.length, 3, "3 flattened index rows");
            assert_eq!(item.n_buffers, 2, "dictionary: validity + i32 indices");
            let idx = *item.buffers.add(1) as *const i32;
            assert_eq!(*idx, 0);
            assert_eq!(*idx.add(1), 1);
            assert!(!item.dictionary.is_null(), "dictionary child array present");
            let dict_array = &*item.dictionary;
            assert_eq!(dict_array.length, 2, "2 dictionary entries");
            assert_eq!(
                dict_array.n_buffers, 3,
                "utf8 values: validity, offsets, data"
            );
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_export_array_of_array_int32() {
        // Array(Array(Int32)): the `item` child schema is itself `+L` with its own
        // `item` child of format `i`; nested child arrays have the right lengths.
        // Outer rows: [[[1,2],[3]], [[4]]] -> outer offsets [0,2,3].
        // Inner arrays (3 of them): offsets [0,2,3,4], values [1,2,3,4].
        let schema = Schema::new(vec![Field {
            name: "arr".into(),
            ch_type: ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
        }]);
        let inner_values = Column::Int32(PrimitiveColumn::new(vec![1, 2, 3, 4]));
        let inner = Column::Array(crate::column::ArrayColumn::new(
            vec![0, 2, 3, 4],
            inner_values,
        ));
        let outer = Column::Array(crate::column::ArrayColumn::new(vec![0, 2, 3], inner));
        let batch = Arc::new(ColBatch::new(schema, vec![outer], 2));

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
            let item = &**c0.children.add(0);
            assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "+L");
            assert_eq!(item.n_children, 1);
            let leaf = &**item.children.add(0);
            assert_eq!(CStr::from_ptr(leaf.format).to_str().unwrap(), "i");
            assert_eq!(CStr::from_ptr(leaf.name).to_str().unwrap(), "item");
            (schema_out.release.unwrap())(&mut schema_out);

            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let c0 = &**array.children.add(0);
            assert_eq!(c0.length, 2, "2 outer rows");
            let offsets = *c0.buffers.add(1) as *const i64;
            assert_eq!(*offsets, 0);
            assert_eq!(*offsets.add(2), 3, "3 inner arrays total");

            let item = &**c0.children.add(0);
            assert_eq!(item.length, 3, "3 inner arrays == outer last offset");
            let inner_offsets = *item.buffers.add(1) as *const i64;
            assert_eq!(*inner_offsets.add(3), 4, "4 leaf elements total");

            let leaf = &**item.children.add(0);
            assert_eq!(leaf.length, 4, "4 leaf int32 elements");
            let vals = *leaf.buffers.add(1) as *const i32;
            assert_eq!(*vals, 1);
            assert_eq!(*vals.add(3), 4);
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_export_empty_array_column() {
        // A zero-row Array column has offsets == [0]: length 0, offsets buffer
        // still non-null with a single leading 0, and an empty element child.
        let schema = Schema::new(vec![Field {
            name: "arr".into(),
            ch_type: ChType::Array(Box::new(ChType::Int32)),
        }]);
        let values = Column::Int32(PrimitiveColumn::new(vec![]));
        let col = Column::Array(crate::column::ArrayColumn::new(vec![0], values));
        let batch = Arc::new(ColBatch::new(schema, vec![col], 0));

        unsafe {
            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let c0 = &**array.children.add(0);
            assert_eq!(c0.length, 0);
            assert_eq!(c0.n_buffers, 2);
            assert!((*c0.buffers.add(0)).is_null());
            let offsets = *c0.buffers.add(1) as *const i64;
            assert_eq!(*offsets, 0, "the leading 0 for a zero-row column");
            let item = &**c0.children.add(0);
            assert_eq!(item.length, 0, "no elements");
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_release_array_skips_moved_out_child() {
        // Spec "Moving child arrays": a consumer may take ownership of a child
        // by bitwise-copying its struct and marking the SOURCE released
        // (release = None) WITHOUT calling the source's release callback, then
        // must release the parent. The parent's release must skip the moved
        // child while still freeing the producer-owned child shell, and the
        // moved copy must stay independently valid (it holds its own
        // Arc<ColBatch> in private data) until its own, idempotent release.
        let schema = Schema::new(vec![Field {
            name: "arr".into(),
            ch_type: ChType::Array(Box::new(ChType::Int32)),
        }]);
        let values = Column::Int32(PrimitiveColumn::new(vec![10, 20, 30, 40]));
        let col = Column::Array(crate::column::ArrayColumn::new(vec![0, 3, 3, 4], values));
        let batch = Arc::new(ColBatch::new(schema, vec![col], 3));

        unsafe {
            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            // Drop our own Arc so the export's private-data clones are the only
            // owners of the buffers from here on.
            drop(batch);

            let list_ptr = *array.children.add(0);
            let item_ptr = *(*list_ptr).children.add(0);

            // Consumer move: bitwise copy the item child out of the producer's
            // shell, then mark the source released without calling its
            // callback.
            let mut moved: ArrowArray = ptr::read(item_ptr);
            (*item_ptr).release = None;

            // Per spec the parent must be released after moving a child out.
            // Its release must skip the moved-out item (null release), freeing
            // only the producer-owned shell; the moved copy stays valid.
            (array.release.unwrap())(&mut array);
            assert!(array.release.is_none(), "parent marked released");

            // The moved child still owns its buffers through its own private
            // data: length and values remain readable after the parent (and
            // our Arc) are gone.
            assert_eq!(moved.length, 4);
            assert_eq!(moved.n_buffers, 2);
            let vals = *moved.buffers.add(1) as *const i32;
            assert_eq!(*vals, 10);
            assert_eq!(*vals.add(3), 40);

            // Releasing the moved copy frees its private data exactly once and
            // marks it released; a second call through the producer callback is
            // a no-op (private_data was cleared).
            (moved.release.unwrap())(&mut moved);
            assert!(moved.release.is_none(), "moved child marked released");
            release_array(&mut moved);
        }
    }

    #[test]
    fn test_stream_with_array_column() {
        // A stream whose batch carries an Array(Array(Int32)) column: the first
        // exported type with real `children`, so `get_schema` and `get_next`
        // must agree on the child shape all the way down.
        // One row: [[[7], [9, 11]]] -> outer offsets [0, 2], inner offsets
        // [0, 1, 3], leaf values [7, 9, 11].
        let schema = Schema::new(vec![Field {
            name: "arr".into(),
            ch_type: ChType::Array(Box::new(ChType::Array(Box::new(ChType::Int32)))),
        }]);
        let leaf = Column::Int32(PrimitiveColumn::new(vec![7, 9, 11]));
        let inner = Column::Array(crate::column::ArrayColumn::new(vec![0, 1, 3], leaf));
        let outer = Column::Array(crate::column::ArrayColumn::new(vec![0, 2], inner));
        let batch = Arc::new(ColBatch::new(schema.clone(), vec![outer], 1));

        unsafe {
            let mut stream: ArrowArrayStream = std::mem::zeroed();
            export_chunks_to_stream(schema, vec![batch], &mut stream);

            let mut schema_out: ArrowSchema = std::mem::zeroed();
            let rc = (stream.get_schema.unwrap())(&mut stream, &mut schema_out);
            assert_eq!(rc, 0);
            let s0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(s0.format).to_str().unwrap(), "+L");
            assert_eq!(s0.n_children, 1);
            let s_item = &**s0.children.add(0);
            assert_eq!(CStr::from_ptr(s_item.format).to_str().unwrap(), "+L");
            assert_eq!(s_item.n_children, 1);
            let s_leaf = &**s_item.children.add(0);
            assert_eq!(CStr::from_ptr(s_leaf.format).to_str().unwrap(), "i");

            let mut array: ArrowArray = std::mem::zeroed();
            let rc = (stream.get_next.unwrap())(&mut stream, &mut array);
            assert_eq!(rc, 0);
            assert!(array.release.is_some());
            assert_eq!(array.n_children, schema_out.n_children);
            let a0 = &**array.children.add(0);
            assert_eq!(a0.length, 1, "1 outer row");
            assert_eq!(a0.n_children, s0.n_children);
            let a_item = &**a0.children.add(0);
            assert_eq!(a_item.length, 2, "outer last offset");
            assert_eq!(a_item.n_children, s_item.n_children);
            let a_leaf = &**a_item.children.add(0);
            assert_eq!(a_leaf.length, 3, "inner last offset");
            let vals = *a_leaf.buffers.add(1) as *const i32;
            assert_eq!(*vals, 7);
            assert_eq!(*vals.add(2), 11);
            (array.release.unwrap())(&mut array);
            (schema_out.release.unwrap())(&mut schema_out);

            // End of stream after the single chunk.
            let mut array2: ArrowArray = std::mem::zeroed();
            let rc = (stream.get_next.unwrap())(&mut stream, &mut array2);
            assert_eq!(rc, 0);
            assert!(array2.release.is_none());

            (stream.release.unwrap())(&mut stream);
        }
    }

    #[test]
    fn test_export_array_all_rows_empty_low_cardinality() {
        use crate::column::DictionaryColumn;

        // Rows but every array empty: offsets [0, 0, 0] over an empty element
        // column. This is the shape the decoder produces for the documented
        // wire quirk where a rows-but-all-empty `Array(LowCardinality(String))`
        // column carries no element body at all (see CODEC_CONTRACT.md). The
        // offsets buffer must still be non-null with num_rows + 1 entries, and
        // the item child (and its dictionary) must export with length 0.
        let schema = Schema::new(vec![Field {
            name: "arr".into(),
            ch_type: ChType::Array(Box::new(ChType::LowCardinality(Box::new(ChType::String)))),
        }]);
        let dict_values = Column::Utf8(Utf8Column::new(vec![0], Vec::new()));
        let dict = DictionaryColumn::new(Vec::new(), dict_values);
        let col = Column::Array(crate::column::ArrayColumn::new(
            vec![0, 0, 0],
            Column::Dictionary(dict),
        ));
        let batch = Arc::new(ColBatch::new(schema, vec![col], 2));

        unsafe {
            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let c0 = &**array.children.add(0);
            assert_eq!(c0.length, 2, "2 rows, all empty");
            assert_eq!(c0.null_count, 0);
            assert_eq!(c0.n_buffers, 2);
            assert!((*c0.buffers.add(0)).is_null());
            let offsets_ptr = *c0.buffers.add(1);
            assert!(!offsets_ptr.is_null(), "offsets buffer stays non-null");
            let offsets = offsets_ptr as *const i64;
            assert_eq!(*offsets, 0);
            assert_eq!(*offsets.add(1), 0);
            assert_eq!(*offsets.add(2), 0, "num_rows + 1 all-zero offsets");

            assert_eq!(c0.n_children, 1);
            let item = &**c0.children.add(0);
            assert_eq!(item.length, 0, "no flattened elements");
            assert_eq!(item.null_count, 0);
            assert!(!item.dictionary.is_null(), "dictionary child still present");
            let dict_array = &*item.dictionary;
            assert_eq!(dict_array.length, 0, "empty dictionary");
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_arrow_format_tuple() {
        // Tuple exports as an Arrow struct (`+s`); the element types live in
        // the child schemas, never in the format string. Nullable(Tuple)
        // recurses to the same format.
        assert_eq!(
            arrow_format(&ChType::Tuple(vec![
                (None, ChType::Int32),
                (None, ChType::String),
            ])),
            "+s"
        );
        assert_eq!(arrow_format(&ChType::Tuple(vec![])), "+s");
        assert_eq!(
            arrow_format(&ChType::Nullable(Box::new(ChType::Tuple(vec![(
                None,
                ChType::Int32,
            )])))),
            "+s"
        );
    }

    #[test]
    fn test_export_tuple_schema() {
        // Schema of a named Tuple(a Int32, b Nullable(String)) and an unnamed
        // Tuple(Int32, String): format `+s`, one child per element. Named
        // elements keep their ClickHouse names verbatim; unnamed elements are
        // named by 1-based position. A Nullable element carries its own
        // nullable flag on the child.
        let schema = Schema::new(vec![
            Field {
                name: "tn".into(),
                ch_type: ChType::Tuple(vec![
                    (Some("a".to_string()), ChType::Int32),
                    (
                        Some("b".to_string()),
                        ChType::Nullable(Box::new(ChType::String)),
                    ),
                ]),
            },
            Field {
                name: "t".into(),
                ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
            },
            Field {
                name: "t0".into(),
                ch_type: ChType::Tuple(vec![]),
            },
        ]);

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&schema, &mut schema_out);

            let tn = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(tn.format).to_str().unwrap(), "+s");
            assert_eq!(tn.flags & 2, 0, "plain tuple is not nullable");
            assert_eq!(tn.n_children, 2);
            assert!(tn.dictionary.is_null());
            let a = &**tn.children.add(0);
            assert_eq!(CStr::from_ptr(a.name).to_str().unwrap(), "a");
            assert_eq!(CStr::from_ptr(a.format).to_str().unwrap(), "i");
            let b = &**tn.children.add(1);
            assert_eq!(CStr::from_ptr(b.name).to_str().unwrap(), "b");
            assert_eq!(CStr::from_ptr(b.format).to_str().unwrap(), "u");
            assert_eq!(b.flags & 2, 2, "Nullable element child is nullable");

            let t = &**schema_out.children.add(1);
            assert_eq!(t.n_children, 2);
            let e1 = &**t.children.add(0);
            assert_eq!(CStr::from_ptr(e1.name).to_str().unwrap(), "1");
            let e2 = &**t.children.add(1);
            assert_eq!(CStr::from_ptr(e2.name).to_str().unwrap(), "2");

            let t0 = &**schema_out.children.add(2);
            assert_eq!(CStr::from_ptr(t0.format).to_str().unwrap(), "+s");
            assert_eq!(t0.n_children, 0, "Tuple() exports with no children");

            (schema_out.release.unwrap())(&mut schema_out);
        }
    }

    #[test]
    fn test_export_tuple_buffers() {
        use crate::column::TupleColumn;

        // Tuple(Int32, String) over 2 rows: struct node with 1 buffer (null
        // validity slot, null_count 0), 2 children exported recursively; and a
        // zero-element Tuple() whose node still carries its explicit length.
        let schema = Schema::new(vec![
            Field {
                name: "t".into(),
                ch_type: ChType::Tuple(vec![(None, ChType::Int32), (None, ChType::String)]),
            },
            Field {
                name: "t0".into(),
                ch_type: ChType::Tuple(vec![]),
            },
        ]);
        let columns = vec![
            Column::Tuple(TupleColumn::new(
                vec![
                    Column::Int32(PrimitiveColumn::new(vec![13, 79])),
                    Column::Utf8(Utf8Column::new(vec![0, 2, 2], b"hi".to_vec())),
                ],
                2,
            )),
            Column::Tuple(TupleColumn::new(vec![], 2)),
        ];
        let batch = Arc::new(ColBatch::new(schema, columns, 2));

        unsafe {
            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);

            let t = &**array.children.add(0);
            assert_eq!(t.length, 2);
            assert_eq!(t.null_count, 0);
            assert_eq!(t.n_buffers, 1, "struct: validity slot only");
            assert!((*t.buffers.add(0)).is_null(), "plain tuple validity null");
            assert_eq!(t.n_children, 2);
            assert!(t.dictionary.is_null());
            let e1 = &**t.children.add(0);
            assert_eq!(e1.length, 2);
            let vals = *e1.buffers.add(1) as *const i32;
            assert_eq!(*vals, 13);
            assert_eq!(*vals.add(1), 79);
            let e2 = &**t.children.add(1);
            assert_eq!(e2.length, 2);
            assert_eq!(e2.n_buffers, 3, "utf8 element: validity, offsets, data");

            let t0 = &**array.children.add(1);
            assert_eq!(t0.length, 2, "Tuple() length from the explicit len");
            assert_eq!(t0.n_buffers, 1);
            assert_eq!(t0.n_children, 0);

            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_export_nullable_tuple() {
        use crate::bitmap::Bitmap;
        use crate::column::TupleColumn;

        // Nullable(Tuple(Int32)): the struct node carries the nullable flag on
        // the schema and the validity bitmap in buffers[0] with a matching
        // null_count; children stay independent per the C Data spec.
        let schema = Schema::new(vec![Field {
            name: "nt".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Tuple(vec![(None, ChType::Int32)]))),
        }]);
        let columns = vec![Column::Tuple(TupleColumn::new_nullable(
            vec![Column::Int32(PrimitiveColumn::new(vec![13, 0, 79]))],
            3,
            Bitmap::from_ch_null_map(&[0x00, 0x01, 0x00]),
        ))];
        let batch = Arc::new(ColBatch::new(schema, columns, 3));

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+s");
            assert_eq!(c0.flags & 2, 2, "Nullable(Tuple) sets the nullable flag");
            assert_eq!(c0.n_children, 1, "element children still described");
            (schema_out.release.unwrap())(&mut schema_out);

            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let c0 = &**array.children.add(0);
            assert_eq!(c0.length, 3);
            assert_eq!(c0.null_count, 1);
            assert_eq!(c0.n_buffers, 1);
            assert!(
                !(*c0.buffers.add(0)).is_null(),
                "tuple-level validity bitmap present in buffers[0]"
            );
            let child = &**c0.children.add(0);
            assert_eq!(child.length, 3, "children carry placeholders for nulls");
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_arrow_format_map() {
        // Map exports as LargeList-of-struct (`+L`), never `+m` (whose i32
        // offsets would force a copy of the i64 offset buffer).
        assert_eq!(
            arrow_format(&ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Int32),
            )),
            "+L"
        );
    }

    #[test]
    fn test_export_map_schema() {
        // Schema of Map(String, Nullable(Int32)): field format `+L`, flags 0
        // (maps are never nullable at the map level), one child named
        // "entries" (a non-nullable struct), with grandchildren "key" (flags 0)
        // and "value" (nullable flag per the value type). No
        // ARROW_FLAG_MAP_KEYS_SORTED anywhere (it is +m-only).
        let schema = Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ChType::Map(
                Box::new(ChType::String),
                Box::new(ChType::Nullable(Box::new(ChType::Int32))),
            ),
        }]);

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&schema, &mut schema_out);

            let c0 = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(c0.format).to_str().unwrap(), "+L");
            assert_eq!(c0.flags, 0, "map level is not nullable, no sorted-keys");
            assert_eq!(c0.n_children, 1);
            assert!(c0.dictionary.is_null());

            let entries = &**c0.children.add(0);
            assert_eq!(CStr::from_ptr(entries.name).to_str().unwrap(), "entries");
            assert_eq!(CStr::from_ptr(entries.format).to_str().unwrap(), "+s");
            assert_eq!(entries.flags, 0, "entries struct is non-nullable");
            assert_eq!(entries.n_children, 2);

            let key = &**entries.children.add(0);
            assert_eq!(CStr::from_ptr(key.name).to_str().unwrap(), "key");
            assert_eq!(CStr::from_ptr(key.format).to_str().unwrap(), "u");
            assert_eq!(key.flags & 2, 0, "keys are never nullable");
            let value = &**entries.children.add(1);
            assert_eq!(CStr::from_ptr(value.name).to_str().unwrap(), "value");
            assert_eq!(CStr::from_ptr(value.format).to_str().unwrap(), "i");
            assert_eq!(value.flags & 2, 2, "Nullable value child is nullable");

            (schema_out.release.unwrap())(&mut schema_out);
        }
    }

    #[test]
    fn test_export_map_lc_key_schema() {
        // Map(LowCardinality(String), UInt8): the key grandchild is a
        // dictionary field (index format `i`, values in the dictionary child),
        // composing through the entries struct exactly like a top-level LC.
        let schema = Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ChType::Map(
                Box::new(ChType::LowCardinality(Box::new(ChType::String))),
                Box::new(ChType::UInt8),
            ),
        }]);

        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&schema, &mut schema_out);
            let c0 = &**schema_out.children.add(0);
            let entries = &**c0.children.add(0);
            let key = &**entries.children.add(0);
            assert_eq!(CStr::from_ptr(key.format).to_str().unwrap(), "i");
            assert!(!key.dictionary.is_null(), "LC key has a dictionary child");
            let dict = &*key.dictionary;
            assert_eq!(CStr::from_ptr(dict.format).to_str().unwrap(), "u");
            (schema_out.release.unwrap())(&mut schema_out);
        }
    }

    #[test]
    fn test_export_map_buffers() {
        use crate::column::{MapColumn, TupleColumn};

        // Map(String, Int32) over 3 rows including an empty row: the map node
        // is byte-identical in shape to an Array node (2 buffers: null
        // validity, i64 offsets), with the entries struct as the single child
        // and the key/value columns as its children.
        // Rows: {a: 13} / {} / {b: 1, c: 2} -> offsets [0, 1, 1, 3].
        let schema = Schema::new(vec![Field {
            name: "m".into(),
            ch_type: ChType::Map(Box::new(ChType::String), Box::new(ChType::Int32)),
        }]);
        let entries = Column::Tuple(TupleColumn::new(
            vec![
                Column::Utf8(Utf8Column::new(vec![0, 1, 2, 3], b"abc".to_vec())),
                Column::Int32(PrimitiveColumn::new(vec![13, 1, 2])),
            ],
            3,
        ));
        let col = Column::Map(MapColumn::new(vec![0, 1, 1, 3], entries));
        let batch = Arc::new(ColBatch::new(schema, vec![col], 3));

        unsafe {
            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);

            let c0 = &**array.children.add(0);
            assert_eq!(c0.length, 3, "map length is the row count");
            assert_eq!(c0.null_count, 0);
            assert_eq!(c0.n_buffers, 2, "LargeList shape: validity + offsets");
            assert!((*c0.buffers.add(0)).is_null(), "map validity always null");
            let offsets = *c0.buffers.add(1) as *const i64;
            assert_eq!(*offsets, 0, "leading 0");
            assert_eq!(*offsets.add(2), 1, "empty row -> repeated offset");
            assert_eq!(*offsets.add(3), 3, "last offset == total entries");

            assert_eq!(c0.n_children, 1);
            let entries = &**c0.children.add(0);
            assert_eq!(entries.length, 3, "entry count == last offset");
            assert_eq!(entries.null_count, 0);
            assert_eq!(entries.n_buffers, 1, "struct: validity slot only");
            assert!((*entries.buffers.add(0)).is_null());
            assert_eq!(entries.n_children, 2);
            let key = &**entries.children.add(0);
            assert_eq!(key.length, 3);
            assert_eq!(key.n_buffers, 3, "utf8 keys: validity, offsets, data");
            let value = &**entries.children.add(1);
            assert_eq!(value.length, 3);
            let vals = *value.buffers.add(1) as *const i32;
            assert_eq!(*vals, 13);
            assert_eq!(*vals.add(2), 2);

            (array.release.unwrap())(&mut array);
        }
    }

    // -----------------------------------------------------------------------
    // SimpleAggregateFunction / geo aliases / Nested Arrow export
    // -----------------------------------------------------------------------

    #[test]
    fn test_arrow_format_name_decoration_delegates() {
        // Each alias reports the Arrow format of the physical type it delegates to.
        assert_eq!(
            arrow_format(&ChType::SimpleAggregateFunction {
                func: "sum".into(),
                inner: Box::new(ChType::Float64),
            }),
            "g"
        );
        assert_eq!(
            arrow_format(&ChType::SimpleAggregateFunction {
                func: "anyLast".into(),
                inner: Box::new(ChType::LowCardinality(Box::new(ChType::String))),
            }),
            "i" // dictionary index type
        );
        assert_eq!(arrow_format(&ChType::Geo(GeoKind::Point)), "+s");
        assert_eq!(arrow_format(&ChType::Geo(GeoKind::Ring)), "+L");
        assert_eq!(arrow_format(&ChType::Geo(GeoKind::MultiPolygon)), "+L");
        assert_eq!(
            arrow_format(&ChType::Nested(vec![("a".into(), ChType::UInt32)])),
            "+L"
        );
    }

    #[test]
    fn test_export_point_schema_and_array() {
        // Point exports as an Arrow struct of two float64 children (unnamed tuple
        // elements are named by 1-based position).
        let schema = Schema::new(vec![Field {
            name: "p".into(),
            ch_type: ChType::Geo(GeoKind::Point),
        }]);
        let columns = vec![Column::Tuple(TupleColumn::new(
            vec![
                Column::Float64(PrimitiveColumn::new(vec![1.0, 3.0])),
                Column::Float64(PrimitiveColumn::new(vec![2.0, 4.0])),
            ],
            2,
        ))];
        let batch = Arc::new(ColBatch::new(schema, columns, 2));
        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let p = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(p.format).to_str().unwrap(), "+s");
            assert_eq!(p.n_children, 2);
            let x = &**p.children.add(0);
            let y = &**p.children.add(1);
            assert_eq!(CStr::from_ptr(x.format).to_str().unwrap(), "g");
            assert_eq!(CStr::from_ptr(y.format).to_str().unwrap(), "g");
            assert_eq!(CStr::from_ptr(x.name).to_str().unwrap(), "1");
            assert_eq!(CStr::from_ptr(y.name).to_str().unwrap(), "2");
            (schema_out.release.unwrap())(&mut schema_out);

            let mut array: ArrowArray = std::mem::zeroed();
            export_batch_array(&batch, &mut array);
            let p = &**array.children.add(0);
            assert_eq!(p.length, 2);
            assert_eq!(p.n_children, 2);
            (array.release.unwrap())(&mut array);
        }
    }

    #[test]
    fn test_export_nullable_point_schema() {
        // Nullable(Point) sets the struct's nullable flag while still emitting its
        // two element children.
        let schema = Schema::new(vec![Field {
            name: "p".into(),
            ch_type: ChType::Nullable(Box::new(ChType::Geo(GeoKind::Point))),
        }]);
        let columns = vec![Column::Tuple(TupleColumn::new_nullable(
            vec![
                Column::Float64(PrimitiveColumn::new(vec![1.0, 0.0])),
                Column::Float64(PrimitiveColumn::new(vec![2.0, 0.0])),
            ],
            2,
            Bitmap::from_ch_null_map(&[0, 1]),
        ))];
        let batch = Arc::new(ColBatch::new(schema, columns, 2));
        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let p = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(p.format).to_str().unwrap(), "+s");
            assert_eq!(p.flags & 2, 2, "nullable flag set for Nullable(Point)");
            assert_eq!(p.n_children, 2);
            (schema_out.release.unwrap())(&mut schema_out);
        }
    }

    #[test]
    fn test_export_nested_schema() {
        // Nested exports as a LargeList of a struct whose children carry the
        // Nested field names verbatim.
        let schema = Schema::new(vec![Field {
            name: "n".into(),
            ch_type: ChType::Nested(vec![
                ("a".into(), ChType::UInt32),
                ("b".into(), ChType::String),
            ]),
        }]);
        let entries = Column::Tuple(TupleColumn::new(
            vec![
                Column::UInt32(PrimitiveColumn::new(vec![10, 20])),
                Column::Utf8(Utf8Column::new(vec![0, 1, 2], b"xy".to_vec())),
            ],
            2,
        ));
        let columns = vec![Column::Array(ArrayColumn::new(vec![0, 2], entries))];
        let batch = Arc::new(ColBatch::new(schema, columns, 1));
        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let n = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(n.format).to_str().unwrap(), "+L");
            assert_eq!(n.n_children, 1);
            let item = &**n.children.add(0);
            assert_eq!(CStr::from_ptr(item.format).to_str().unwrap(), "+s");
            assert_eq!(CStr::from_ptr(item.name).to_str().unwrap(), "item");
            assert_eq!(item.n_children, 2);
            let a = &**item.children.add(0);
            let b = &**item.children.add(1);
            assert_eq!(CStr::from_ptr(a.name).to_str().unwrap(), "a");
            assert_eq!(CStr::from_ptr(a.format).to_str().unwrap(), "I");
            assert_eq!(CStr::from_ptr(b.name).to_str().unwrap(), "b");
            assert_eq!(CStr::from_ptr(b.format).to_str().unwrap(), "u");
            (schema_out.release.unwrap())(&mut schema_out);
        }
    }

    #[test]
    fn test_export_simple_aggregate_function_over_low_cardinality_schema() {
        // SAF over LowCardinality(Nullable(String)) exports as a dictionary field
        // (index format `i`, a `u` dictionary child, the nullable flag set).
        let schema = Schema::new(vec![Field {
            name: "s".into(),
            ch_type: ChType::SimpleAggregateFunction {
                func: "anyLast".into(),
                inner: Box::new(ChType::LowCardinality(Box::new(ChType::Nullable(
                    Box::new(ChType::String),
                )))),
            },
        }]);
        let columns = vec![Column::Dictionary(DictionaryColumn::new_nullable(
            vec![1, 0],
            Column::Utf8(Utf8Column::new(vec![0, 0, 6], b"user_1".to_vec())),
            Bitmap::from_ch_null_map(&[0, 1]),
        ))];
        let batch = Arc::new(ColBatch::new(schema, columns, 2));
        unsafe {
            let mut schema_out: ArrowSchema = std::mem::zeroed();
            export_schema(&batch.schema, &mut schema_out);
            let s = &**schema_out.children.add(0);
            assert_eq!(CStr::from_ptr(s.format).to_str().unwrap(), "i");
            assert_eq!(
                s.flags & 2,
                2,
                "nullable flag set for SAF over LC(Nullable)"
            );
            assert!(!s.dictionary.is_null(), "dictionary child present");
            let dict = &*s.dictionary;
            assert_eq!(CStr::from_ptr(dict.format).to_str().unwrap(), "u");
            (schema_out.release.unwrap())(&mut schema_out);
        }
    }
}
