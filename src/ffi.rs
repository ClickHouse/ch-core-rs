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

// ---------------------------------------------------------------------------
// Private data for release callbacks
// ---------------------------------------------------------------------------

struct SchemaPrivateData {
    format: CString,
    name: CString,
    children: Vec<*mut ArrowSchema>,
    _child_data: Vec<SchemaPrivateData>,
}

struct ArrayPrivateData {
    buffers: Vec<*const c_void>,
    children: Vec<*mut ArrowArray>,
    _batch: Arc<ColBatch>,
    _child_data: Vec<ArrayPrivateData>,
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
        ChType::Nullable(inner) => arrow_format(inner),
    }
}

fn is_nullable(ch_type: &ChType) -> bool {
    matches!(ch_type, ChType::Nullable(_))
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

    let pd = Box::new(SchemaPrivateData {
        format,
        name: name_cstr,
        children: Vec::new(),
        _child_data: Vec::new(),
    });

    let schema = &mut *out;
    schema.format = pd.format.as_ptr();
    schema.name = pd.name.as_ptr();
    schema.metadata = ptr::null();
    schema.flags = if is_nullable(ch_type) { 2 } else { 0 };
    schema.n_children = 0;
    schema.children = ptr::null_mut();
    schema.dictionary = ptr::null_mut();
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
    let col = &batch.columns[col_idx];
    let num_rows = batch.num_rows as i64;
    let null_count = col.null_count() as i64;

    let mut buffers: Vec<*const c_void> = Vec::new();

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
    }

    let pd = Box::new(ArrayPrivateData {
        buffers,
        children: Vec::new(),
        _batch: Arc::clone(batch),
        _child_data: Vec::new(),
    });

    let array = &mut *out;
    array.length = num_rows;
    array.null_count = null_count;
    array.offset = 0;
    array.n_buffers = pd.buffers.len() as i64;
    array.buffers = pd.buffers.as_ptr() as *mut *const c_void;
    array.n_children = 0;
    array.children = ptr::null_mut();
    array.dictionary = ptr::null_mut();
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
