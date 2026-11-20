//! Native tests of the C ABI. They call the exported functions directly on the
//! host target and act as the host would: read the descriptor, copy every
//! referenced buffer out, then free the result.

use super::*;
use crate::json::{self, Value, Writer};

const ALL_TYPES: &[u8] = include_bytes!("../../../tests/fixtures/all_types_rev0.native");
const MULTI_BLOCK: &[u8] = include_bytes!("../../../tests/fixtures/multi_block_rev0.native");
/// Captured with the server's flattened Dynamic/JSON serialization, which
/// the core decodes but does not write, so it cannot re-encode identically.
const JSON_FLATTENED: &[u8] = include_bytes!("../../../tests/fixtures/json_flattened_rev0.native");

/// Rev-0 fixtures in the server's default serialization.
const REV0_FIXTURES: &[(&str, &[u8])] = &[
    ("all_types", ALL_TYPES),
    ("multi_block", MULTI_BLOCK),
    (
        "json_mixed_rows",
        include_bytes!("../../../tests/fixtures/json_mixed_rows_rev0.native"),
    ),
    (
        "json_multipart",
        include_bytes!("../../../tests/fixtures/json_multipart_rev0.native"),
    ),
    (
        "json_shared",
        include_bytes!("../../../tests/fixtures/json_shared_rev0.native"),
    ),
    (
        "json_string",
        include_bytes!("../../../tests/fixtures/json_string_rev0.native"),
    ),
    (
        "zero_width_dynamic",
        include_bytes!("../../../tests/fixtures/zero_width_dynamic_rev0.native"),
    ),
];

fn last_error() -> (u32, String) {
    let code = chw_last_error_code();
    let len = chw_last_error_len();
    let msg = if len == 0 {
        String::new()
    } else {
        // Safety: the module reports `len` readable bytes at this pointer,
        // valid until the next call that records or clears an error.
        let bytes = unsafe { std::slice::from_raw_parts(chw_last_error_ptr(), len) };
        String::from_utf8(bytes.to_vec()).unwrap()
    };
    (code, msg)
}

/// Read a result's descriptor, resolve every `{"$b":[ptr,len,dtype]}` into
/// an owned copy of the bytes, and free the result.
fn take(result: u32) -> Value {
    assert_ne!(result, 0, "call failed: {:?}", last_error());
    let len = chw_result_json_len(result);
    // Safety: the result is live and reports `len` bytes of descriptor.
    let raw = unsafe { std::slice::from_raw_parts(chw_result_json_ptr(result), len) };
    let mut v = json::parse(raw).unwrap();
    copy_out(&mut v);
    assert_eq!(chw_result_free(result), 1);
    v
}

/// Owned copy of a buffer, standing in for a JS TypedArray.
fn bytes_value(bytes: Vec<u8>) -> Value {
    Value::Object(vec![(
        "$bytes".into(),
        Value::Array(bytes.into_iter().map(|b| Value::Number(b.into())).collect()),
    )])
}

fn copy_out(v: &mut Value) {
    if let Some([ptr, len, _dtype]) = v.get("$b").and_then(Value::as_array) {
        let (ptr, len) = (ptr.as_usize().unwrap(), len.as_usize().unwrap());
        let bytes = if len == 0 {
            Vec::new()
        } else {
            // Safety: the result that owns this buffer is still live; `take`
            // frees it only after every reference has been copied out.
            unsafe { std::slice::from_raw_parts(ptr as *const u8, len) }.to_vec()
        };
        *v = bytes_value(bytes);
        return;
    }
    match v {
        Value::Array(items) => items.iter_mut().for_each(copy_out),
        Value::Object(fields) => fields.iter_mut().for_each(|(_, f)| copy_out(f)),
        _ => {}
    }
}

fn owned_bytes(v: &Value) -> Option<Vec<u8>> {
    let items = v.get("$bytes")?.as_array()?;
    Some(items.iter().map(|b| b.as_usize().unwrap() as u8).collect())
}

/// Serialize a column tree for `chw_encode`, packing every copied buffer into
/// one data blob at 8-byte aligned offsets, as the JS glue does.
fn write_value(w: &mut Writer, v: &Value, data: &mut Vec<u8>) {
    if let Some(bytes) = owned_bytes(v) {
        let off = data.len().next_multiple_of(8);
        data.resize(off, 0);
        data.extend_from_slice(&bytes);
        w.begin_object();
        w.key("$b");
        w.begin_array();
        w.uint(off as u64);
        w.uint(bytes.len() as u64);
        w.end_array();
        w.end_object();
        return;
    }
    match v {
        Value::Null => w.null(),
        Value::Bool(b) => w.bool(*b),
        Value::Number(n) => w.int(*n as i64),
        Value::String(s) => w.string(s),
        Value::Array(items) => {
            w.begin_array();
            for i in items {
                write_value(w, i, data);
            }
            w.end_array();
        }
        Value::Object(fields) => {
            w.begin_object();
            for (k, f) in fields {
                w.key(k);
                write_value(w, f, data);
            }
            w.end_object();
        }
    }
}

fn call_encode(block: &Value) -> Result<Vec<u8>, (u32, String)> {
    let mut w = Writer::default();
    let mut data = Vec::new();
    write_value(&mut w, block, &mut data);
    let desc = w.finish();
    // Safety: both slices outlive the call.
    let h = unsafe { chw_encode(desc.as_ptr(), desc.len(), data.as_ptr(), data.len()) };
    if h == 0 {
        return Err(last_error());
    }
    Ok(owned_bytes(take(h).get("bytes").unwrap()).unwrap())
}

fn decode(bytes: &[u8]) -> Value {
    // Safety: `bytes` outlives the call.
    take(unsafe { chw_decode(bytes.as_ptr(), bytes.len()) })
}

#[test]
fn abi_version_is_exported() {
    assert_eq!(chw_abi_version(), ABI_VERSION);
}

#[test]
fn every_rev0_fixture_reencodes_byte_identically() {
    for (name, bytes) in REV0_FIXTURES {
        let decoded = decode(bytes);
        let mut again = Vec::new();
        for chunk in decoded.get("chunks").and_then(Value::as_array).unwrap() {
            again.extend(call_encode(chunk).unwrap_or_else(|e| panic!("{name}: {e:?}")));
        }
        assert!(again == *bytes, "{name}: re-encoded bytes differ");
    }
}

#[test]
fn flattened_json_decodes_and_reencodes_or_errors_cleanly() {
    let decoded = decode(JSON_FLATTENED);
    let chunks = decoded.get("chunks").and_then(Value::as_array).unwrap();
    for chunk in chunks {
        let kind = chunk.get("columns").and_then(Value::as_array).unwrap()[0].get("kind");
        assert_eq!(kind.and_then(Value::as_str), Some("JSON"));
        match call_encode(chunk) {
            // Re-encoded in the default shape, which decodes to the same rows.
            Ok(bytes) => assert_eq!(decode(&bytes).get("rowCount"), chunk.get("rowCount")),
            // The flattened shape can carry more dynamic paths than the
            // declared limit allows in the default shape. The encoder rejects
            // that with an error, not a panic.
            Err((code, _)) => assert_eq!(code, ErrorCode::InvalidArg as u32),
        }
    }
}

#[test]
fn decode_reports_schema_and_chunks() {
    let v = decode(MULTI_BLOCK);
    let chunks = v.get("chunks").and_then(Value::as_array).unwrap();
    assert!(chunks.len() > 1, "fixture has several blocks");
    let rows: usize = chunks
        .iter()
        .map(|c| c.get("rowCount").and_then(Value::as_usize).unwrap())
        .sum();
    assert_eq!(v.get("rowCount").and_then(Value::as_usize), Some(rows));
    let names = v.get("columnNames").and_then(Value::as_array).unwrap();
    let types = v.get("columnTypes").and_then(Value::as_array).unwrap();
    assert_eq!(names.len(), types.len());
    let first = &chunks[0].get("columns").and_then(Value::as_array).unwrap()[0];
    assert_eq!(first.get("name"), Some(&names[0]));
    assert_eq!(first.get("type"), Some(&types[0]));
}

#[test]
fn empty_body_is_an_empty_result() {
    let v = decode(&[]);
    assert_eq!(v.get("rowCount").and_then(Value::as_usize), Some(0));
    assert_eq!(v.get("chunks").and_then(Value::as_array), Some(&[][..]));
}

#[test]
fn garbage_input_records_an_error_and_the_module_keeps_working() {
    let garbage = [1u8, 1, 1, b'x', 3, b'F', b'o', b'o'];
    // Safety: `garbage` outlives the call.
    let h = unsafe { chw_decode(garbage.as_ptr(), garbage.len()) };
    assert_eq!(h, 0);
    let (code, msg) = last_error();
    assert_eq!(code, ErrorCode::InvalidArg as u32);
    assert!(!msg.is_empty());
    decode(ALL_TYPES);
    assert_eq!(chw_last_error_code(), 0, "success clears the error");
}

#[test]
fn stream_matches_buffered_decode_at_every_split() {
    let whole = decode(MULTI_BLOCK);
    for split in [1, 7, MULTI_BLOCK.len() / 2] {
        let s = chw_stream_new();
        let mut chunks = Vec::new();
        for piece in MULTI_BLOCK.chunks(split) {
            // Safety: `piece` outlives the call.
            let out = take(unsafe { chw_stream_push(s, piece.as_ptr(), piece.len()) });
            chunks.extend(
                out.get("chunks")
                    .and_then(Value::as_array)
                    .unwrap()
                    .to_vec(),
            );
        }
        let out = take(chw_stream_finish(s));
        chunks.extend(
            out.get("chunks")
                .and_then(Value::as_array)
                .unwrap()
                .to_vec(),
        );
        assert_eq!(
            chunks,
            whole.get("chunks").and_then(Value::as_array).unwrap()
        );
        assert_eq!(
            chw_stream_row_count(s),
            whole.get("rowCount").and_then(Value::as_usize).unwrap() as u64
        );
        assert_eq!(chw_stream_free(s), 1);
    }
}

#[test]
fn stream_lifecycle_errors() {
    // One column `x UInt8`, one row with value 7.
    let block = [1u8, 1, 1, b'x', 5, b'U', b'I', b'n', b't', b'8', 7];
    let s = chw_stream_new();
    // Safety: `block` outlives the call.
    take(unsafe { chw_stream_push(s, block.as_ptr(), block.len()) });
    take(chw_stream_finish(s));
    // Safety: `block` outlives the call.
    assert_eq!(
        unsafe { chw_stream_push(s, block.as_ptr(), block.len()) },
        0
    );
    assert!(last_error().1.contains("after finish"));
    assert_eq!(chw_stream_finish(s), 0);
    assert!(last_error().1.contains("more than once"));
    assert_eq!(chw_stream_free(s), 1);
    assert_eq!(chw_stream_free(s), 0, "double free is an error, not UB");
    assert_eq!(chw_stream_finish(s), 0);
}

#[test]
fn truncated_stream_fails_in_finish_and_poisons() {
    let s = chw_stream_new();
    let part = &MULTI_BLOCK[..MULTI_BLOCK.len() - 3];
    // Safety: `part` outlives the call.
    take(unsafe { chw_stream_push(s, part.as_ptr(), part.len()) });
    assert_eq!(chw_stream_finish(s), 0);
    assert_eq!(last_error().0, ErrorCode::GenericFailure as u32);
    // Safety: `part` outlives the call.
    assert_eq!(unsafe { chw_stream_push(s, part.as_ptr(), part.len()) }, 0);
    assert!(last_error().1.contains("create a new stream"));
    chw_stream_free(s);
}

#[test]
fn handles_are_checked() {
    assert_eq!(chw_result_free(0), 0);
    assert_eq!(chw_result_free(u32::MAX), 0);
    assert_eq!(chw_result_json_len(u32::MAX), 0);
    let s = chw_stream_new();
    assert_eq!(chw_result_free(s), 0, "a stream is not a result");
    assert_eq!(chw_stream_free(s), 1);
    // Safety: `MULTI_BLOCK` outlives the call.
    let r = unsafe { chw_decode(MULTI_BLOCK.as_ptr(), MULTI_BLOCK.len()) };
    assert_eq!(chw_stream_free(r), 0, "a result is not a stream");
    assert_eq!(chw_result_free(r), 1);
}

#[test]
fn alloc_and_free_round_trip() {
    let p = chw_alloc(13);
    assert!(!p.is_null());
    assert_eq!(p as usize % ALLOC_ALIGN, 0);
    // Safety: `p` is a fresh 13-byte block from `chw_alloc(13)`.
    unsafe {
        p.write_bytes(79, 13);
        chw_free(p, 13);
    }
    let empty = chw_alloc(0);
    assert!(!empty.is_null());
    // Safety: freeing a zero-length block is a documented no-op.
    unsafe { chw_free(empty, 0) };
}

#[test]
fn describe_type_resolves_aliases_and_metadata() {
    let name = "Array(Nullable(DateTime64(3, 'UTC')))";
    // Safety: `name` outlives the call.
    let v = take(unsafe { chw_describe_type(name.as_ptr(), name.len()) });
    assert_eq!(v.get("kind").and_then(Value::as_str), Some("Array"));
    let elem = v.get("element").unwrap();
    assert_eq!(elem.get("kind").and_then(Value::as_str), Some("DateTime64"));
    assert_eq!(elem.get("nullable"), Some(&Value::Bool(true)));
    assert_eq!(elem.get("precision").and_then(Value::as_usize), Some(3));
    assert_eq!(elem.get("timezone").and_then(Value::as_str), Some("UTC"));

    let bad = "NotAType(";
    // Safety: `bad` outlives the call.
    assert_eq!(unsafe { chw_describe_type(bad.as_ptr(), bad.len()) }, 0);
    assert_eq!(last_error().0, ErrorCode::InvalidArg as u32);
}

#[test]
fn encode_rejects_bad_descriptors_without_panicking() {
    let cases: &[(&str, &[u8])] = &[
        ("not json", b""),
        ("{", b""),
        (r#"{"rowCount":-1,"columns":[]}"#, b""),
        (r#"{"rowCount":1,"columns":[{"type":"UInt8"}]}"#, b""),
        (
            r#"{"rowCount":1,"columns":[{"name":"x","type":"UInt8","values":{"$b":[0,8]}}]}"#,
            &[7],
        ),
        (
            r#"{"rowCount":1,"columns":[{"name":"x","type":"UInt16","values":{"$b":[0,3]}}]}"#,
            &[1, 2, 3],
        ),
        (
            r#"{"rowCount":1,"columns":[{"name":"x","type":"UInt8","values":{"$b":[18446744073709551615,1]}}]}"#,
            &[7],
        ),
        (
            r#"{"rowCount":2,"columns":[{"name":"x","type":"UInt8","values":{"$b":[0,1]}}]}"#,
            &[7],
        ),
        (
            r#"{"rowCount":1,"columns":[{"name":"x","type":"Array(UInt8)","offsets":{"$b":[0,8]},"values":{"$b":[0,0]}}]}"#,
            &[0xff; 8],
        ),
    ];
    for (desc, data) in cases {
        // Safety: both slices outlive the call.
        let h = unsafe { chw_encode(desc.as_ptr(), desc.len(), data.as_ptr(), data.len()) };
        assert_eq!(h, 0, "{desc}");
        assert_eq!(last_error().0, ErrorCode::InvalidArg as u32, "{desc}");
    }
}

#[test]
fn encode_one_column_block() {
    let desc = r#"{"rowCount":1,"columns":[{"name":"x","type":"UInt8","values":{"$b":[0,1]}}]}"#;
    // Safety: both slices outlive the call.
    let h = unsafe { chw_encode(desc.as_ptr(), desc.len(), [7u8].as_ptr(), 1) };
    let bytes = owned_bytes(take(h).get("bytes").unwrap()).unwrap();
    assert_eq!(bytes, [1, 1, 1, b'x', 5, b'U', b'I', b'n', b't', b'8', 7]);
}

#[test]
fn a_freed_handle_stays_stale_after_its_slot_is_reused() {
    // Safety: `MULTI_BLOCK` outlives both calls.
    let first = unsafe { chw_decode(MULTI_BLOCK.as_ptr(), MULTI_BLOCK.len()) };
    assert_eq!(chw_result_free(first), 1);
    // Safety: as above.
    let second = unsafe { chw_decode(MULTI_BLOCK.as_ptr(), MULTI_BLOCK.len()) };
    assert_eq!(
        first & INDEX_MASK,
        second & INDEX_MASK,
        "the slot was reused"
    );
    assert_ne!(first, second);
    assert_eq!(
        chw_result_free(first),
        0,
        "the old handle does not free the new result"
    );
    assert_eq!(chw_result_json_len(first), 0);
    assert!(chw_result_json_len(second) > 0);
    take(second);
}

#[test]
fn stream_lookups_report_bad_handles() {
    assert_eq!(chw_stream_row_count(0), 0);
    assert_eq!(chw_last_error_code(), ErrorCode::InvalidArg as u32);
    let s = chw_stream_new();
    assert_eq!(chw_stream_buffered_bytes(s), 0);
    assert_eq!(
        chw_last_error_code(),
        0,
        "a successful lookup clears the error"
    );
    chw_stream_free(s);
}
