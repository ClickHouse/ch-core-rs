//! C ABI over `ch-core-rs` for WebAssembly hosts.
//!
//! This crate compiles to one `.wasm` file with a small C ABI and no runtime
//! dependencies. The hand-written JS glue in `js/` drives it from Node.js and
//! browsers, and any other WebAssembly host can call the same exports. The ABI
//! is a public contract, documented in `ABI.md`; bump [`ABI_VERSION`] for any
//! breaking change to it.
//!
//! The ABI stays columnar. It decodes Native bytes into column buffers and
//! encodes column buffers into Native bytes. Turning columns into rows or
//! host values belongs to the binding, not here.
//!
//! Memory model in short:
//!
//! - Input: the host allocates with [`chw_alloc`], writes the bytes, passes
//!   `(ptr, len)`, and frees with [`chw_free`] after the call returns. Calls
//!   only borrow their input.
//! - Output: calls that produce data return a result handle. Its descriptor
//!   (UTF-8 JSON) and every buffer the descriptor references stay valid until
//!   [`chw_result_free`]. Any call into the module may grow linear memory, so
//!   the host must re-read the memory buffer after each call and must copy
//!   data out before freeing the result. The JS glue copies every buffer once
//!   and never hands out views into linear memory.
//! - Errors: a call that fails returns 0 (or a null pointer) and records a
//!   code and message, read with [`chw_last_error_code`] and friends.

use std::alloc::Layout;
use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::Arc;

use ch_core_rs::batch::ColBatch;
use ch_core_rs::column::Column;
use ch_core_rs::native::binary_value::{decode_binary_value, read_binary_type_prefix};
use ch_core_rs::native::decode::{decode_all_bytes, parse_ch_type, DecodeError, DecodeOptions};
use ch_core_rs::native::stream_decoder::StreamDecoder;

mod decode;
mod describe;
mod encode;
mod json;

use decode::Describer;

/// Version of the C ABI described in `ABI.md`. Hosts check it at load time.
pub const ABI_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Error codes returned by [`chw_last_error_code`]. Numeric values are part
/// of the ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    /// No error recorded.
    None = 0,
    /// The input can never succeed, however many bytes arrive: unsupported
    /// type, corrupt framing, invalid layout, a bad handle, or lifecycle misuse
    /// such as pushing after finish.
    InvalidArg = 1,
    /// An I/O-shaped failure, such as a truncated final block in
    /// [`chw_stream_finish`].
    GenericFailure = 2,
    /// [`chw_alloc`] could not allocate.
    OutOfMemory = 3,
}

#[derive(Debug)]
pub(crate) struct Error {
    code: ErrorCode,
    message: String,
}

impl Error {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::InvalidArg,
            message: message.into(),
        }
    }

    pub(crate) fn generic(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::GenericFailure,
            message: message.into(),
        }
    }
}

impl From<DecodeError> for Error {
    fn from(e: DecodeError) -> Self {
        let code = match &e {
            DecodeError::Io(io) if io.kind() != std::io::ErrorKind::InvalidData => {
                ErrorCode::GenericFailure
            }
            _ => ErrorCode::InvalidArg,
        };
        Self {
            code,
            message: e.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Module state
// ---------------------------------------------------------------------------

/// A finished result: the descriptor plus everything its buffer references
/// point into. Only `json` is read after construction. The other fields own
/// the referenced buffers and exist to keep them alive until the result is
/// freed.
pub(crate) struct Output {
    json: String,
    #[allow(dead_code)]
    batches: Vec<Arc<ColBatch>>,
    #[allow(dead_code)]
    columns: Vec<Column>,
    bytes: Vec<Vec<u8>>,
}

enum Slot {
    Free,
    /// A stream taken out of its slot for the length of one push or finish.
    Busy,
    Stream(Box<Stream>),
    Output(Box<Output>),
}

struct Entry {
    /// Bumped each time the slot is freed, so an old handle to a reused slot
    /// no longer matches.
    generation: u32,
    slot: Slot,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    free: Vec<usize>,
    last_error: Option<Error>,
}

/// A handle is `generation << INDEX_BITS | (index + 1)`. The low bits are
/// never zero, so `0` stays free to mean failure. The generation makes a
/// freed handle stale even after its slot is reused, until the 12-bit counter
/// wraps after 4096 reuses of that one slot.
const INDEX_BITS: u32 = 20;
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;
const GENERATION_MASK: u32 = u32::MAX >> INDEX_BITS;

thread_local! {
    // wasm32-unknown-unknown is single threaded, so this is one global table.
    // Handles index it, which keeps a stale or forged handle from ever
    // becoming a wild pointer: a bad handle is an `InvalidArg` error.
    static STATE: RefCell<State> = RefCell::default();
}

fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

impl State {
    fn insert(&mut self, slot: Slot) -> Result<u32, Error> {
        let index = match self.free.pop() {
            Some(i) => {
                self.entries[i].slot = slot;
                i
            }
            None if self.entries.len() < INDEX_MASK as usize => {
                self.entries.push(Entry {
                    generation: 0,
                    slot,
                });
                self.entries.len() - 1
            }
            None => return Err(Error::invalid("too many live handles")),
        };
        Ok(self.entries[index].generation << INDEX_BITS | (index as u32 + 1))
    }

    /// The live slot `h` names, or `None` for a zero, stale or forged handle.
    fn slot(&mut self, h: u32) -> Option<&mut Slot> {
        let index = ((h & INDEX_MASK) as usize).checked_sub(1)?;
        let entry = self.entries.get_mut(index)?;
        if entry.generation != h >> INDEX_BITS || matches!(entry.slot, Slot::Free) {
            return None;
        }
        Some(&mut entry.slot)
    }

    fn remove(&mut self, h: u32) -> Option<Slot> {
        let taken = std::mem::replace(self.slot(h)?, Slot::Free);
        let index = (h & INDEX_MASK) as usize - 1;
        let entry = &mut self.entries[index];
        entry.generation = (entry.generation + 1) & GENERATION_MASK;
        self.free.push(index);
        Some(taken)
    }

    fn output(&mut self, h: u32) -> Option<&Output> {
        match self.slot(h)? {
            Slot::Output(o) => Some(o),
            _ => None,
        }
    }
}

/// Record `e` as the last error and return the failure value.
fn fail<T: Default>(e: Error) -> T {
    with_state(|s| s.last_error = Some(e));
    T::default()
}

/// Store a successful output and return its handle, or record the error.
fn finish(r: Result<Output, Error>) -> u32 {
    let stored = r.and_then(|out| with_state(|s| s.insert(Slot::Output(Box::new(out)))));
    match stored {
        Ok(h) => {
            with_state(|s| s.last_error = None);
            h
        }
        Err(e) => fail(e),
    }
}

/// Borrow `len` bytes at `ptr` for the duration of one call.
///
/// # Safety
///
/// `ptr` must be valid for reads of `len` bytes for the returned lifetime,
/// which callers bound to the current ABI call, and must not alias memory the
/// module owns and may change or free during the call. In practice it is a
/// `chw_alloc` block. A zero `len` ignores `ptr`.
unsafe fn input<'a>(ptr: *const u8, len: usize) -> Result<&'a [u8], Error> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(Error::invalid("null pointer with a non-zero length"));
    }
    // Safety: non-null, and the caller guarantees `len` readable bytes that
    // stay untouched for the duration of this call (see `ABI.md`).
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

fn options() -> DecodeOptions {
    // protocol_revision 0: a bare HTTP `FORMAT Native` body with no TCP framing.
    DecodeOptions::default()
}

// ---------------------------------------------------------------------------
// Version, memory, errors
// ---------------------------------------------------------------------------

/// The C ABI version, [`ABI_VERSION`].
#[no_mangle]
pub extern "C" fn chw_abi_version() -> u32 {
    ABI_VERSION
}

/// Alignment of every [`chw_alloc`] block, enough for any column element.
const ALLOC_ALIGN: usize = 8;

/// Allocate `len` bytes, 8-byte aligned, for the host to write input into.
/// Returns null and records `OutOfMemory` on failure. A zero `len` returns a
/// non-null dangling pointer that must not be read or written.
#[no_mangle]
pub extern "C" fn chw_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return NonNull::<u64>::dangling().as_ptr().cast();
    }
    let Ok(layout) = Layout::from_size_align(len, ALLOC_ALIGN) else {
        return fail_null(ErrorCode::OutOfMemory, len);
    };
    // Safety: `layout` has a non-zero size, checked above.
    let ptr = unsafe { std::alloc::alloc(layout) };
    if ptr.is_null() {
        return fail_null(ErrorCode::OutOfMemory, len);
    }
    ptr
}

fn fail_null(code: ErrorCode, len: usize) -> *mut u8 {
    with_state(|s| {
        s.last_error = Some(Error {
            code,
            message: format!("could not allocate {len} bytes"),
        })
    });
    std::ptr::null_mut()
}

/// Free a block from [`chw_alloc`].
///
/// # Safety
///
/// `ptr` must come from `chw_alloc(len)` with the same `len` and must not have
/// been freed already. Freeing a zero-length block is a no-op.
#[no_mangle]
pub unsafe extern "C" fn chw_free(ptr: *mut u8, len: usize) {
    if len == 0 || ptr.is_null() {
        return;
    }
    // Safety: the caller passes a pointer from `chw_alloc(len)`, which
    // allocated it with exactly this layout, and has not freed it yet.
    unsafe { std::alloc::dealloc(ptr, Layout::from_size_align_unchecked(len, ALLOC_ALIGN)) }
}

/// Code of the last recorded error, `0` if none. A successful call that
/// returns a result handle clears it.
#[no_mangle]
pub extern "C" fn chw_last_error_code() -> u32 {
    with_state(|s| s.last_error.as_ref().map_or(0, |e| e.code as u32))
}

/// Pointer to the last error's UTF-8 message. Valid until the next call that
/// records or clears an error.
#[no_mangle]
pub extern "C" fn chw_last_error_ptr() -> *const u8 {
    with_state(|s| {
        s.last_error
            .as_ref()
            .map_or(std::ptr::null(), |e| e.message.as_ptr())
    })
}

/// Byte length of the last error's message, `0` if none.
#[no_mangle]
pub extern "C" fn chw_last_error_len() -> usize {
    with_state(|s| s.last_error.as_ref().map_or(0, |e| e.message.len()))
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// Pointer to a result's UTF-8 JSON descriptor, or null for a bad handle.
#[no_mangle]
pub extern "C" fn chw_result_json_ptr(result: u32) -> *const u8 {
    with_state(|s| {
        s.output(result)
            .map_or(std::ptr::null(), |o| o.json.as_ptr())
    })
}

/// Byte length of a result's JSON descriptor, `0` for a bad handle.
#[no_mangle]
pub extern "C" fn chw_result_json_len(result: u32) -> usize {
    with_state(|s| s.output(result).map_or(0, |o| o.json.len()))
}

/// Free a result and every buffer its descriptor references. Returns 1, or
/// 0 with `InvalidArg` for a bad or already-freed handle.
#[no_mangle]
pub extern "C" fn chw_result_free(result: u32) -> u32 {
    let freed = with_state(|s| match s.slot(result) {
        Some(Slot::Output(_)) => s.remove(result).is_some(),
        _ => false,
    });
    if freed {
        1
    } else {
        fail(Error::invalid(format!(
            "{result} is not a live result handle"
        )))
    }
}

// ---------------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------------

/// Decode a complete `FORMAT Native` body into a result whose descriptor is
/// `{ rowCount, columnNames, columnTypes, chunks: [{ rowCount, columns }] }`.
/// An empty body is a valid empty result with no schema. Returns 0 on error.
///
/// # Safety
///
/// `ptr` must be valid for reads of `len` bytes for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn chw_decode(ptr: *const u8, len: usize) -> u32 {
    // Safety: forwarded caller contract.
    finish(unsafe { input(ptr, len) }.and_then(decode_body))
}

fn decode_body(buf: &[u8]) -> Result<Output, Error> {
    let mut d = Describer::default();
    if buf.is_empty() {
        // Empty SELECT over HTTP: no bytes, no schema.
        d.begin_result(0, &[], &[], [])?;
        d.w.end_object();
        return Ok(d.into_output(Vec::new(), Vec::new()));
    }
    let batch = decode_all_bytes(buf, &options())?;
    let names: Vec<String> = batch.schema.fields.iter().map(|f| f.name.clone()).collect();
    let types: Vec<String> = batch
        .schema
        .fields
        .iter()
        .map(|f| f.ch_type.to_string())
        .collect();
    d.begin_result(
        batch.num_rows(),
        &names,
        &types,
        batch.chunks.iter().map(Arc::as_ref),
    )?;
    d.w.end_object();
    Ok(d.into_output(batch.chunks, Vec::new()))
}

/// Decode one `SharedVariant` or JSON shared-data cell, a binary type
/// descriptor followed by one `serializeBinary` value, into a result whose
/// descriptor is `{ type, column }` with a one-row column. Returns 0 on error.
///
/// # Safety
///
/// `ptr` must be valid for reads of `len` bytes for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn chw_decode_binary_value(ptr: *const u8, len: usize) -> u32 {
    // Safety: forwarded caller contract.
    finish(unsafe { input(ptr, len) }.and_then(|bytes| {
        let (ty, used) =
            read_binary_type_prefix(bytes).map_err(|e| Error::invalid(e.to_string()))?;
        let col =
            decode_binary_value(&ty, &bytes[used..]).map_err(|e| Error::invalid(e.to_string()))?;
        let mut d = Describer::default();
        d.w.begin_object();
        d.w.field_str("type", &ty.to_string());
        d.w.key("column");
        d.column(&col, Some(&ty), None)?;
        d.w.end_object();
        Ok(d.into_output(Vec::new(), vec![col]))
    }))
}

/// Parse a ClickHouse type string with the core's parser. The result's
/// descriptor is `{ type, kind, nullable, ...metadata, ...children }`.
/// Returns 0 on error.
///
/// # Safety
///
/// `ptr` must be valid for reads of `len` bytes for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn chw_describe_type(ptr: *const u8, len: usize) -> u32 {
    // Safety: forwarded caller contract.
    finish(unsafe { input(ptr, len) }.and_then(|bytes| {
        let name =
            std::str::from_utf8(bytes).map_err(|_| Error::invalid("type is not valid UTF-8"))?;
        let ty = parse_ch_type(name)
            .ok_or_else(|| Error::invalid(format!("cannot parse type '{name}'")))?;
        let mut d = Describer::default();
        describe::describe(&mut d, &ty);
        Ok(d.into_output(Vec::new(), Vec::new()))
    }))
}

// ---------------------------------------------------------------------------
// Streaming decode
// ---------------------------------------------------------------------------

/// Incremental decoder state behind a stream handle.
struct Stream {
    decoder: StreamDecoder,
    column_names: Option<Vec<String>>,
    column_types: Option<Vec<String>>,
    /// `u64` so a long export cannot wrap a 32-bit `usize` on wasm32.
    row_count: u64,
    finished: bool,
    errored: bool,
}

impl Stream {
    fn check_usable(&self) -> Result<(), Error> {
        if self.errored {
            return Err(Error::invalid(
                "decoder failed previously; create a new stream",
            ));
        }
        Ok(())
    }

    /// Run one decode step. Any failure in it poisons the stream: the decoder
    /// may have consumed bytes, or handed over blocks that are then lost, so
    /// continuing would silently drop rows. Lifecycle misuse is checked before
    /// this and leaves the stream as it was.
    fn poison_on_error(
        &mut self,
        step: impl FnOnce(&mut Self) -> Result<Output, Error>,
    ) -> Result<Output, Error> {
        let r = step(self);
        if r.is_err() {
            self.errored = true;
        }
        r
    }

    /// Describe newly completed blocks. Zero-row blocks contribute the schema
    /// but are omitted from `chunks`.
    fn emit(&mut self, batches: Vec<ColBatch>) -> Result<Output, Error> {
        if self.column_names.is_none() {
            if let Some(first) = batches.first() {
                let f = &first.schema.fields;
                self.column_names = Some(f.iter().map(|f| f.name.clone()).collect());
                self.column_types = Some(f.iter().map(|f| f.ch_type.to_string()).collect());
            }
        }
        let batches: Vec<Arc<ColBatch>> = batches
            .into_iter()
            .filter(|b| b.num_rows > 0)
            .map(Arc::new)
            .collect();
        let rows: usize = batches.iter().map(|b| b.num_rows).sum();
        let mut d = Describer::default();
        d.begin_result(
            rows,
            self.column_names.as_deref().unwrap_or(&[]),
            self.column_types.as_deref().unwrap_or(&[]),
            batches.iter().map(Arc::as_ref),
        )?;
        d.w.field_uint("bufferedBytes", self.decoder.buffered_bytes() as u64);
        d.w.end_object();
        self.row_count = self.row_count.saturating_add(rows as u64);
        Ok(d.into_output(batches, Vec::new()))
    }
}

/// Create a streaming decoder for an HTTP `FORMAT Native` response body.
/// Free it with [`chw_stream_free`].
#[no_mangle]
pub extern "C" fn chw_stream_new() -> u32 {
    let stream = Stream {
        decoder: StreamDecoder::new(options()),
        column_names: None,
        column_types: None,
        row_count: 0,
        finished: false,
        errored: false,
    };
    match with_state(|s| s.insert(Slot::Stream(Box::new(stream)))) {
        Ok(h) => h,
        Err(e) => fail(e),
    }
}

/// Run `f` on a live stream, then store its output.
fn with_stream(h: u32, f: impl FnOnce(&mut Stream) -> Result<Output, Error>) -> u32 {
    // The stream is taken out of the table while `f` runs so storing the
    // output does not need a second borrow of the table.
    let Some(Slot::Stream(mut stream)) = with_state(|s| match s.slot(h) {
        Some(slot @ Slot::Stream(_)) => Some(std::mem::replace(slot, Slot::Busy)),
        _ => None,
    }) else {
        return fail(Error::invalid(format!("{h} is not a live stream handle")));
    };
    let r = f(&mut stream);
    with_state(|s| {
        if let Some(slot) = s.slot(h) {
            *slot = Slot::Stream(stream);
        }
    });
    finish(r)
}

/// Append bytes to a stream. The result describes every block that is now
/// complete, plus `bufferedBytes`. After any error the stream is poisoned and
/// every later push or finish fails. Returns 0 on error.
///
/// # Safety
///
/// `ptr` must be valid for reads of `len` bytes for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn chw_stream_push(stream: u32, ptr: *const u8, len: usize) -> u32 {
    // Safety: forwarded caller contract.
    let bytes = match unsafe { input(ptr, len) } {
        Ok(b) => b,
        Err(e) => return fail(e),
    };
    with_stream(stream, |s| {
        s.check_usable()?;
        if s.finished {
            return Err(Error::invalid("cannot push after finish"));
        }
        s.poison_on_error(|s| {
            let batches = s.decoder.feed(bytes)?;
            s.emit(batches)
        })
    })
}

/// Drain the stream's tail. Fails on a truncated final block. Can be called
/// once. Returns 0 on error.
#[no_mangle]
pub extern "C" fn chw_stream_finish(stream: u32) -> u32 {
    with_stream(stream, |s| {
        s.check_usable()?;
        if s.finished {
            return Err(Error::invalid("finish called more than once"));
        }
        s.finished = true;
        // A zero-byte body is a legitimate empty result: ClickHouse's HTTP
        // interface writes no header block for a SELECT that returns no rows,
        // so the schema stays unknown.
        s.poison_on_error(|s| {
            let batches = s.decoder.finish()?;
            s.emit(batches)
        })
    })
}

/// Read a field of a live stream. A bad handle returns 0 and records
/// `InvalidArg`. Success clears the last error, so a 0 result can be told
/// apart from a failure by [`chw_last_error_code`].
fn stream_field<T: Default>(h: u32, f: impl FnOnce(&Stream) -> T) -> T {
    let value = with_state(|s| {
        let value = match s.slot(h) {
            Some(Slot::Stream(st)) => Some(f(st)),
            _ => None,
        };
        if value.is_some() {
            s.last_error = None;
        }
        value
    });
    value.unwrap_or_else(|| fail(Error::invalid(format!("{h} is not a live stream handle"))))
}

/// Rows emitted by the stream so far, as a `u64`, which can pass `u32::MAX`
/// on a long export.
#[no_mangle]
pub extern "C" fn chw_stream_row_count(stream: u32) -> u64 {
    stream_field(stream, |st| st.row_count)
}

/// Bytes the stream holds for an incomplete block.
#[no_mangle]
pub extern "C" fn chw_stream_buffered_bytes(stream: u32) -> usize {
    stream_field(stream, |st| st.decoder.buffered_bytes())
}

/// Free a stream. Returns 1, or 0 with `InvalidArg` for a bad handle.
#[no_mangle]
pub extern "C" fn chw_stream_free(stream: u32) -> u32 {
    let freed = with_state(|s| match s.slot(stream) {
        Some(Slot::Stream(_)) => s.remove(stream).is_some(),
        _ => false,
    });
    if freed {
        1
    } else {
        fail(Error::invalid(format!(
            "{stream} is not a live stream handle"
        )))
    }
}

// ---------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------

/// Encode one Native block for `INSERT ... FORMAT Native`.
///
/// `desc` is the UTF-8 JSON block descriptor `{ rowCount, columns }`, in the
/// shape of one decoded chunk, whose buffers are `{"$b":[offset,byteLength]}`
/// ranges of `data`. The result's descriptor is `{ bytes }`, a reference to
/// the encoded block. Returns 0 on error.
///
/// # Safety
///
/// `desc_ptr` and `data_ptr` must be valid for reads of `desc_len` and
/// `data_len` bytes for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn chw_encode(
    desc_ptr: *const u8,
    desc_len: usize,
    data_ptr: *const u8,
    data_len: usize,
) -> u32 {
    // Safety: forwarded caller contract.
    let inputs =
        unsafe { input(desc_ptr, desc_len).and_then(|d| Ok((d, input(data_ptr, data_len)?))) };
    finish(inputs.and_then(|(desc, data)| {
        let block = json::parse(desc).map_err(Error::invalid)?;
        let bytes = encode::encode_native(&block, data)?;
        let mut d = Describer::default();
        d.w.begin_object();
        d.buf("bytes", &bytes);
        d.w.end_object();
        let mut out = d.into_output(Vec::new(), Vec::new());
        out.bytes.push(bytes);
        Ok(out)
    }))
}

#[cfg(test)]
mod tests;
