//! Arrow C Data Interface implementation for zero-copy export.
//!
//! Implements ArrowSchema, ArrowArray, and ArrowArrayStream per
//! https://arrow.apache.org/docs/format/CDataInterface.html

use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::sync::Arc;

use crate::batch::ColBatch;
use crate::column::Column;
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
    _child_data: Vec<SchemaPrivateData>,
    /// The dictionary value-type child schema for a `LowCardinality(T)` field,
    /// owned here so it is freed when this field's schema is released. Null for
    /// every non-dictionary field.
    dictionary: *mut ArrowSchema,
}

struct ArrayPrivateData {
    buffers: Vec<*const c_void>,
    children: Vec<*mut ArrowArray>,
    _batch: Arc<ColBatch>,
    _child_data: Vec<ArrayPrivateData>,
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
        ChType::Nullable(inner) => arrow_format(inner),
        // A dictionary array's top-level format is the INDEX type. The value
        // type lives in the schema's `dictionary` child. Index width is
        // normalized to i32 by the decoder, so the index format is always `i`.
        ChType::LowCardinality(_) => "i".into(),
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
/// type is always the non-nullable inner type.
fn dictionary_value_type(ch_type: &ChType) -> &ChType {
    match ch_type {
        ChType::LowCardinality(inner) => match inner.as_ref() {
            ChType::Nullable(t) => t,
            other => other,
        },
        other => other,
    }
}

/// Whether a column exports with the Arrow nullable flag set. A bare
/// `Nullable(T)` is nullable, and a `LowCardinality(Nullable(T))` is nullable
/// at the index level (nulls live in the index validity bitmap). A plain
/// `LowCardinality(T)` is not nullable.
fn field_is_nullable(ch_type: &ChType) -> bool {
    match ch_type {
        ChType::Nullable(_) => true,
        ChType::LowCardinality(inner) => matches!(inner.as_ref(), ChType::Nullable(_)),
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

    let pd = Box::new(SchemaPrivateData {
        format,
        name: name_cstr,
        children: Vec::new(),
        _child_data: Vec::new(),
        dictionary,
    });

    let schema = &mut *out;
    schema.format = pd.format.as_ptr();
    schema.name = pd.name.as_ptr();
    schema.metadata = ptr::null();
    schema.flags = if field_is_nullable(ch_type) { 2 } else { 0 };
    schema.n_children = 0;
    schema.children = ptr::null_mut();
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
        _child_data: Vec::new(),
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
        Column::Ipv6(c) | Column::Uuid(c) => {
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
    }

    let pd = Box::new(ArrayPrivateData {
        buffers,
        children: Vec::new(),
        _batch: Arc::clone(batch),
        _child_data: Vec::new(),
        dictionary,
    });

    let array = &mut *out;
    array.length = length;
    array.null_count = null_count;
    array.offset = 0;
    array.n_buffers = pd.buffers.len() as i64;
    array.buffers = pd.buffers.as_ptr() as *mut *const c_void;
    array.n_children = 0;
    array.children = ptr::null_mut();
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
        _child_data: Vec::new(),
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
    use crate::column::{Column, PrimitiveColumn, Utf8Column};
    use crate::schema::{ChType, Field, Schema};
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
}
