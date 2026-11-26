//! The `http` protocol: a blocking HTTP transport in the shim, as a binding
//! of ch-core-rs has (`INTEGRATING.md`, "Getting Native Bytes").
//!
//! - One `session_id` for all requests of an invocation, so `SET`, `USE` and
//!   temporary tables last for the whole invocation.
//! - Each result comes as `Native` (`default_format=Native`, so that the
//!   query text keeps its own `FORMAT` clause), framed at the revision of
//!   `client_protocol_version`, in the `compress=1` envelope. The shim takes
//!   the frames apart with `ch_core_rs::compression` and decodes the blocks
//!   with `StreamDecoder`.
//! - The server reports an exception before the result with an HTTP error
//!   status, and an exception in the middle of the result with a block at
//!   the end of the body (`X-ClickHouse-Exception-Tag`).

use crate::runner::{Failure, Outcome, server_error_code};
use ch_core_rs::batch::ColBatch;
use ch_core_rs::compression::{ChecksumMode, decompress_frame};
use ch_core_rs::native::decode::{
    DBMS_TCP_PROTOCOL_VERSION, DecodeOptions, block_end, block_end_binary_types,
};
use ch_core_rs::native::stream_decoder::StreamDecoder;
use std::collections::BTreeMap;
use std::io::Read;

/// The revision of the Native blocks that the shim asks for. Revision 0 (no
/// `client_protocol_version`) loses the time zone of a top-level
/// `DateTime('tz')` column: the server writes its type as `DateTime` for an
/// old client. The server caps the value at its own protocol version, which
/// is higher for every server that the harness supports, so the blocks come
/// at exactly this revision.
pub(crate) const PROTOCOL_REVISION: u64 = DBMS_TCP_PROTOCOL_VERSION;

/// Environment variable to try another revision (`0` for bare Native), for
/// triage.
pub(crate) const REVISION_ENV: &str = "CLICKHOUSE_TEST_RUNNER_REVISION";

/// Environment variable: `0` turns compression off, for triage: over http
/// the result comes without the `compress=1` envelope, over tcp the blocks
/// come without compression.
pub(crate) const COMPRESS_ENV: &str = "CLICKHOUSE_TEST_RUNNER_COMPRESS";

const EXCEPTION_MARKER: &[u8] = b"__exception__\r\n";

pub(crate) struct HttpBackend {
    agent: ureq::Agent,
    url: String,
    user: String,
    password: String,
    session_id: String,
    database: String,
    revision: u64,
    compress: bool,
}

impl HttpBackend {
    pub(crate) fn new(
        url: String,
        user: String,
        password: String,
        database: String,
        session_id: String,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let revision = match env(REVISION_ENV) {
            Some(v) => v
                .parse::<u64>()
                .map_err(|e| format!("{REVISION_ENV}={v:?}: {e}"))?,
            None => PROTOCOL_REVISION,
        };
        let compress = env(COMPRESS_ENV).as_deref() != Some("0");
        let agent: ureq::Agent = ureq::Agent::config_builder()
            // The shim reads the status and the body of an error itself.
            .http_status_as_error(false)
            // The server is local; never use a proxy from the environment.
            .proxy(None)
            .build()
            .into();
        Ok(HttpBackend {
            agent,
            url,
            user,
            password,
            session_id,
            database,
            revision,
            compress,
        })
    }

    /// The database that goes with each request. It replaces the current
    /// database of the session, so the runner sets it after `USE`.
    pub(crate) fn set_database(&mut self, database: String) {
        self.database = database;
    }

    /// Runs one statement and decodes its result.
    pub(crate) fn query(
        &mut self,
        sql: &str,
        settings: &BTreeMap<String, String>,
        params: &BTreeMap<String, String>,
        binary_types: bool,
    ) -> Result<Outcome, Failure> {
        let mut req = self
            .agent
            .post(&self.url)
            .header("X-ClickHouse-User", &self.user)
            .query("database", &self.database)
            .query("session_id", &self.session_id)
            .query("default_format", "Native");
        if !self.password.is_empty() {
            req = req.header("X-ClickHouse-Key", &self.password);
        }
        if self.revision > 0 {
            req = req.query("client_protocol_version", self.revision.to_string());
        }
        if self.compress {
            req = req.query("compress", "1");
        }
        for (name, value) in settings {
            req = req.query(name, value);
        }
        for (name, value) in params {
            // The raw value, as clickhouse-client sends it.
            req = req.query(format!("param_{name}"), value);
        }
        // The original bytes of the statement, also those that are not
        // UTF-8 (`src/input.rs`): the HTTP body carries any bytes.
        let mut resp = req
            .send(crate::input::encode(sql).as_ref())
            .map_err(|e| Failure::Other(format!("HTTP request: {e}")))?;
        let status = resp.status().as_u16();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let timezone = header("X-ClickHouse-Timezone");
        let exception_code = header("X-ClickHouse-Exception-Code");
        let exception_tag = header("X-ClickHouse-Exception-Tag");
        let mut body = Vec::new();
        if let Err(e) = resp.body_mut().as_reader().read_to_end(&mut body) {
            // Without `compress=1`, the server closes the connection after
            // the exception block at the end of the body, without the last
            // chunk. The body is then complete.
            let complete = exception_tag
                .as_deref()
                .is_some_and(|tag| split_exception(&body, tag).is_some());
            if !complete {
                return Err(Failure::Other(format!("HTTP response body: {e}")));
            }
        }

        let decompressed = if self.compress {
            decompress_all(&body)
        } else {
            let len = body.len();
            Ok((body.clone(), vec![len]))
        };
        if status != 200 {
            // The error body is in the envelope too. Take it as it is when it
            // does not decompress. When the server buffered the result, the
            // blocks before the exception come first, then its text.
            let (plain, frames) = decompressed.unwrap_or((body, Vec::new()));
            let (data_len, message) = exception_tag
                .as_deref()
                .and_then(|tag| split_exception(&plain, tag))
                .unwrap_or_else(|| split_error_body(&plain, self.revision, binary_types));
            let error = server_failure(&message, exception_code.as_deref(), status);
            // The exception is the error to report: keep the blocks decoded
            // before a decode error.
            let mut blocks = Vec::new();
            let _ = decode(
                &plain[..data_len],
                &frames,
                self.revision,
                binary_types,
                &mut blocks,
            );
            return Ok(Outcome {
                blocks,
                timezone,
                error: Some(error),
                ..Outcome::default()
            });
        }

        let (plain, frames) = decompressed.map_err(Failure::Other)?;
        // An exception in the middle of the result: the rows before it are
        // valid, as clickhouse-client shows them.
        let (data, error) = match exception_tag
            .as_deref()
            .and_then(|tag| split_exception(&plain, tag))
        {
            Some((data_len, message)) => (
                &plain[..data_len],
                Some(server_failure(&message, exception_code.as_deref(), status)),
            ),
            None => (&plain[..], None),
        };
        let mut blocks = Vec::new();
        if let Err(e) = decode(data, &frames, self.revision, binary_types, &mut blocks) {
            // With an exception, the server can stop in the middle of a
            // block: keep the blocks before it, as over tcp. The exception is
            // the error to report.
            if error.is_none() {
                return Err(Failure::Other(e));
            }
        }
        Ok(Outcome {
            blocks,
            timezone,
            error,
            ..Outcome::default()
        })
    }
}

/// A server exception from its text (`Code: N. DB::Exception: ...`).
fn server_failure(message: &[u8], code_header: Option<&str>, status: u16) -> Failure {
    let message = String::from_utf8_lossy(message).trim_end().to_owned();
    let code = server_error_code(&message).or_else(|| code_header.and_then(|c| c.parse().ok()));
    match code {
        Some(code) => Failure::Server { code, message },
        None => Failure::Other(format!("HTTP status {status}: {message}")),
    }
}

/// Takes the `compress=1` envelope apart: it returns the plaintext and the
/// plaintext length of each frame.
fn decompress_all(mut body: &[u8]) -> Result<(Vec<u8>, Vec<usize>), String> {
    let mut plain = Vec::new();
    let mut frames = Vec::new();
    while !body.is_empty() {
        let frame = decompress_frame(body, ChecksumMode::Verify)
            .map_err(|e| format!("ch-core-rs: decompress a compress=1 frame: {e}"))?;
        plain.extend_from_slice(frame.plaintext());
        frames.push(frame.plaintext_len());
        body = &body[frame.consumed()..];
    }
    Ok((plain, frames))
}

/// Finds the exception block at the end of a body:
/// `\r\n__exception__\r\n<tag>\r\n<message>\n<size> <tag>\r\n__exception__\r\n`.
/// Returns the length of the data before it, and the message.
fn split_exception(body: &[u8], tag: &str) -> Option<(usize, Vec<u8>)> {
    let rest = body.strip_suffix(EXCEPTION_MARKER)?;
    let open = [
        b"\r\n".as_slice(),
        EXCEPTION_MARKER,
        tag.as_bytes(),
        b"\r\n",
    ]
    .concat();
    let start = rest
        .windows(open.len())
        .rposition(|w| w == open.as_slice())?;
    let inner = &rest[start + open.len()..];
    let suffix = [b" ".as_slice(), tag.as_bytes(), b"\r\n"].concat();
    let inner = inner.strip_suffix(suffix.as_slice())?;
    // `<message>\n<size>`: the size is the length of the message.
    let nl = inner.iter().rposition(|&c| c == b'\n')?;
    let message = inner[..nl].to_vec();
    Some((start, message))
}

/// Splits an error body without an exception block: the complete Native
/// blocks first (the server buffered them), then the text of the exception.
/// Returns the length of the blocks, and the text.
fn split_error_body(body: &[u8], revision: u64, binary_types: bool) -> (usize, Vec<u8>) {
    let mut options = DecodeOptions::default();
    options.protocol_revision = revision;
    let mut pos = 0;
    while pos < body.len() && !starts_with_exception(&body[pos..]) {
        let end = if binary_types {
            block_end_binary_types(&body[pos..], &options)
        } else {
            block_end(&body[pos..], &options)
        };
        match end {
            Ok(Some(n)) if n > 0 => pos += n,
            _ => return (0, body.to_vec()),
        }
    }
    (pos, body[pos..].to_vec())
}

/// Tells if the bytes start with the text of a server exception.
fn starts_with_exception(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(64)]);
    server_error_code(&head).is_some()
}

/// Decodes the Native stream into `blocks`. It feeds the decoder one frame of
/// plaintext at a time, so that blocks that cross frames go through the
/// streaming path. On an error, `blocks` keeps the blocks decoded before it.
/// `binary_types`: the server writes the column types in the binary encoding
/// (`output_format_native_encode_types_in_binary_format`), which needs the
/// decoder of that mode (`INTEGRATING.md`).
fn decode(
    data: &[u8],
    frames: &[usize],
    revision: u64,
    binary_types: bool,
    blocks: &mut Vec<ColBatch>,
) -> Result<(), String> {
    let mut options = DecodeOptions::default();
    options.protocol_revision = revision;
    let mut decoder = if binary_types {
        StreamDecoder::new_binary_types(options)
    } else {
        StreamDecoder::new(options)
    };
    let mut rest = data;
    for &len in frames {
        if rest.is_empty() {
            break;
        }
        let (chunk, tail) = rest.split_at(len.min(rest.len()));
        rest = tail;
        blocks.extend(
            decoder
                .feed(chunk)
                .map_err(|e| format!("ch-core-rs: decode the Native result: {e}"))?,
        );
    }
    if !rest.is_empty() {
        blocks.extend(
            decoder
                .feed(rest)
                .map_err(|e| format!("ch-core-rs: decode the Native result: {e}"))?,
        );
    }
    blocks.extend(
        decoder
            .finish()
            .map_err(|e| format!("ch-core-rs: decode the Native result: {e}"))?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ch_core_rs::column::Column;

    #[test]
    fn exception_block() {
        let tag = "osgeturekgudzeky";
        let message = "Code: 395. DB::Exception: Value passed to 'throwIf' function is non-zero";
        let body = format!(
            "\x01\x02data\r\n__exception__\r\n{tag}\r\n{message}\n{} {tag}\r\n__exception__\r\n",
            message.len()
        );
        let (len, got) = split_exception(body.as_bytes(), tag).unwrap();
        assert_eq!(len, 6);
        assert_eq!(got, message.as_bytes());
        // Another tag, or no closing marker: not an exception block.
        assert_eq!(split_exception(body.as_bytes(), "other"), None);
        assert_eq!(
            split_exception(&body.as_bytes()[..body.len() - 2], tag),
            None
        );
        // A message with new lines.
        let message = "Code: 1. DB::Exception: a\nb";
        let body = format!(
            "\r\n__exception__\r\n{tag}\r\n{message}\n{} {tag}\r\n__exception__\r\n",
            message.len()
        );
        assert_eq!(
            split_exception(body.as_bytes(), tag),
            Some((0, message.as_bytes().to_vec()))
        );
    }

    #[test]
    fn server_failures() {
        match server_failure(b"Code: 60. DB::Exception: x. (UNKNOWN_TABLE)\n", None, 404) {
            Failure::Server { code, message } => {
                assert_eq!(code, 60);
                assert_eq!(message, "Code: 60. DB::Exception: x. (UNKNOWN_TABLE)");
            }
            f => panic!("{f:?}"),
        }
        assert!(matches!(
            server_failure(b"", Some("395"), 500),
            Failure::Server { code: 395, .. }
        ));
        assert!(matches!(
            server_failure(b"Bad Gateway", None, 502),
            Failure::Other(_)
        ));
    }

    #[test]
    fn error_body_after_blocks() {
        // A Native block at revision 0: 1 column, 1 row, `x UInt8` = 7.
        let block = b"\x01\x01\x01x\x05UInt8\x07";
        let message = b"Code: 395. DB::Exception: Value passed to 'throwIf' function is non-zero";
        let body = [block.as_slice(), block, message].concat();
        assert_eq!(
            split_error_body(&body, 0, false),
            (2 * block.len(), message.to_vec())
        );
        let mut blocks = Vec::new();
        decode(&body[..2 * block.len()], &[], 0, false, &mut blocks).unwrap();
        assert_eq!(blocks.len(), 2);
        // Only the text.
        assert_eq!(split_error_body(message, 0, false), (0, message.to_vec()));
        // Not blocks and not an exception: all of it is the text.
        assert_eq!(
            split_error_body(b"\xff\xff", 0, false),
            (0, b"\xff\xff".to_vec())
        );
    }

    #[test]
    fn empty_body_is_no_blocks() {
        let mut blocks = Vec::new();
        decode(&[], &[], PROTOCOL_REVISION, false, &mut blocks).unwrap();
        assert!(blocks.is_empty());
        assert_eq!(decompress_all(&[]).unwrap(), (Vec::new(), Vec::new()));
    }

    #[test]
    fn truncated_block_keeps_the_blocks_before_it() {
        // Native blocks at revision 0: 1 column, 1 row, `x UInt8` = 7, 13, 79.
        let block = |value: u8| [b"\x01\x01\x01x\x05UInt8".as_slice(), &[value]].concat();
        let data = [block(7), block(13), block(79)].concat();
        // The last block without its value byte, in one piece, and one
        // frame per block.
        let cut = &data[..data.len() - 1];
        let len = block(7).len();
        for frames in [Vec::new(), vec![len, len, len - 1]] {
            let mut blocks = Vec::new();
            let error = decode(cut, &frames, 0, false, &mut blocks).unwrap_err();
            assert!(error.contains("truncated"), "{error}");
            let values: Vec<u8> = blocks
                .iter()
                .flat_map(|b| match &b.columns[0] {
                    Column::UInt8(c) => c.values.clone(),
                    c => panic!("{c:?}"),
                })
                .collect();
            assert_eq!(values, [7, 13], "frames {frames:?}");
        }
    }
}
