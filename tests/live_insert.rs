//! Live-server acceptance test for the Native encoder.
//!
//! This proves the encoder produces bytes a real ClickHouse server accepts on
//! `INSERT ... FORMAT Native`: it encodes a batch, POSTs it over HTTP, reads the
//! rows back as `FORMAT Native`, decodes them with this crate, and asserts the
//! values survived the round-trip through the server.
//!
//! It is `#[ignore]` so `cargo test` stays hermetic (CI has no server). Run it
//! against a ClickHouse server matching `.server-ref` with:
//!
//! ```sh
//! cargo test --test live_insert -- --ignored
//! ```
//!
//! It shells out to `curl` (no added Rust dependency, same as
//! `scripts/gen_fixtures.sh`) and reads the same environment convention,
//! defaulting to `localhost:8123`, user `default`, no password:
//!
//! - `CLICKHOUSE_CONNECT_TEST_HOST` (default `localhost`)
//! - `CLICKHOUSE_CONNECT_TEST_PORT` (default `8123`)
//! - `CLICKHOUSE_CONNECT_TEST_USER` (default `default`)
//! - `CLICKHOUSE_CONNECT_TEST_PASSWORD` (default empty)
//! - `CLICKHOUSE_CONNECT_TEST_SCHEME` (default `http`)

use std::env;
use std::io::Write;
use std::process::{Command, Stdio};

use ch_core_rs::batch::ColBatch;
use ch_core_rs::column::{Column, FixedBinaryColumn, PrimitiveColumn, Utf8Column};
use ch_core_rs::native::decode::{decode_all_bytes, DecodeOptions};
use ch_core_rs::native::encode::{encode_block, EncodeOptions};
use ch_core_rs::schema::{ChType, Field, Schema};

const TABLE: &str = "ch_core_rs_encode_test";

/// Build a `Utf8Column` from raw byte values, computing Arrow offsets the same
/// way the decoder does.
fn utf8_column(values: &[&[u8]]) -> Utf8Column {
    let mut offsets = Vec::with_capacity(values.len() + 1);
    let mut data = Vec::new();
    offsets.push(0i32);
    for v in values {
        data.extend_from_slice(v);
        offsets.push(data.len() as i32);
    }
    Utf8Column::new(offsets, data)
}

/// Build a `FixedBinaryColumn` of the given width from equal-width byte values.
fn fixed_binary_column(width: usize, values: &[&[u8]]) -> FixedBinaryColumn {
    let mut data = Vec::with_capacity(width * values.len());
    for v in values {
        assert_eq!(v.len(), width, "fixed-string value must be {width} bytes");
        data.extend_from_slice(v);
    }
    FixedBinaryColumn::new(data, width)
}

/// The batch to insert: every encodable type over four rows (the ten fixed-width
/// numerics plus `String` and `FixedString(4)`). The `i32` column is strictly
/// ascending so `ORDER BY i32` on read-back is deterministic and matches
/// insertion order, which lets the string columns line up row-for-row too.
fn sample_batch() -> ColBatch {
    let fields = vec![
        ("i8", ChType::Int8),
        ("i16", ChType::Int16),
        ("i32", ChType::Int32),
        ("i64", ChType::Int64),
        ("u8", ChType::UInt8),
        ("u16", ChType::UInt16),
        ("u32", ChType::UInt32),
        ("u64", ChType::UInt64),
        ("f32", ChType::Float32),
        ("f64", ChType::Float64),
        ("s", ChType::String),
        ("fs", ChType::FixedString(4)),
    ]
    .into_iter()
    .map(|(name, ch_type)| Field {
        name: name.to_string(),
        ch_type,
    })
    .collect();

    let columns = vec![
        Column::Int8(PrimitiveColumn::new(vec![i8::MIN, -13, 0, i8::MAX])),
        Column::Int16(PrimitiveColumn::new(vec![i16::MIN, -13, 0, i16::MAX])),
        Column::Int32(PrimitiveColumn::new(vec![i32::MIN, -79, 0, i32::MAX])),
        Column::Int64(PrimitiveColumn::new(vec![i64::MIN, -79, 0, i64::MAX])),
        Column::UInt8(PrimitiveColumn::new(vec![0, 13, 79, u8::MAX])),
        Column::UInt16(PrimitiveColumn::new(vec![0, 13, 79, u16::MAX])),
        Column::UInt32(PrimitiveColumn::new(vec![0, 13, 79, u32::MAX])),
        Column::UInt64(PrimitiveColumn::new(vec![0, 13, 79, u64::MAX])),
        Column::Float32(PrimitiveColumn::new(vec![-1.25, 0.0, 3.5, 79.125])),
        Column::Float64(PrimitiveColumn::new(vec![-1.25, 0.0, 3.5, 79.125])),
        Column::Utf8(utf8_column(&[b"user_1", b"", b"n", b"user_2_longer"])),
        Column::FixedBinary(fixed_binary_column(
            4,
            &[b"road", b"1234", b"\x00\x00\x00\x00", b"n\x00\x00\x00"],
        )),
    ];

    ColBatch::new(Schema::new(fields), columns, 4)
}

struct Server {
    base_url: String,
    user: String,
    password: String,
}

impl Server {
    fn from_env() -> Self {
        let host = env::var("CLICKHOUSE_CONNECT_TEST_HOST").unwrap_or_else(|_| "localhost".into());
        let port = env::var("CLICKHOUSE_CONNECT_TEST_PORT").unwrap_or_else(|_| "8123".into());
        let scheme = env::var("CLICKHOUSE_CONNECT_TEST_SCHEME").unwrap_or_else(|_| "http".into());
        Server {
            base_url: format!("{scheme}://{host}:{port}/"),
            user: env::var("CLICKHOUSE_CONNECT_TEST_USER").unwrap_or_else(|_| "default".into()),
            password: env::var("CLICKHOUSE_CONNECT_TEST_PASSWORD").unwrap_or_default(),
        }
    }

    /// Run `curl` against the server. `extra` carries the request-shaping args
    /// (the URL query, `--data-urlencode`, `--data-binary`, ...). `stdin`, if
    /// present, is piped to curl for a `@-` body. Returns (curl_ok, stdout,
    /// stderr).
    fn curl(&self, url: &str, extra: &[&str], stdin: Option<&[u8]>) -> (bool, Vec<u8>, Vec<u8>) {
        let mut cmd = Command::new("curl");
        cmd.arg("-sS")
            .arg("--user")
            .arg(format!("{}:{}", self.user, self.password))
            .arg(url);
        for a in extra {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });

        let mut child = cmd
            .spawn()
            .expect("failed to spawn curl (is it installed and on PATH?)");
        if let Some(bytes) = stdin {
            child
                .stdin
                .take()
                .expect("curl stdin")
                .write_all(bytes)
                .expect("write curl stdin");
        }
        let out = child.wait_with_output().expect("curl did not run");
        (out.status.success(), out.stdout, out.stderr)
    }

    /// Run a statement that should return an empty body (DDL, INSERT). A
    /// non-empty body is a ClickHouse error (HTTP error responses carry the
    /// message in the body, and curl without `--fail` still exits 0).
    fn exec_empty(&self, url: &str, extra: &[&str], stdin: Option<&[u8]>, what: &str) {
        let (ok, stdout, stderr) = self.curl(url, extra, stdin);
        assert!(
            ok,
            "curl failed for {what}: {}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(
            stdout.is_empty(),
            "{what} returned an error: {}",
            String::from_utf8_lossy(&stdout)
        );
    }

    /// Send a DDL/SQL statement. ClickHouse reads a POST body with no `query`
    /// URL param as the whole query, so `--data-binary` sends the SQL verbatim.
    /// This is a POST (unlike `--get`, which curl would turn into a read-only GET
    /// that rejects DDL).
    fn ddl(&self, sql: &str) {
        let url = self.base_url.clone();
        self.exec_empty(&url, &["--data-binary", sql], None, sql);
    }

    /// INSERT the given Native bytes into `TABLE`. The query goes in the URL (so
    /// the request body is exactly the Native stream) and the bytes are piped as
    /// a `--data-binary @-` body.
    fn insert_native(&self, bytes: &[u8]) {
        let url = format!(
            "{}?query=INSERT%20INTO%20{TABLE}%20FORMAT%20Native",
            self.base_url
        );
        self.exec_empty(&url, &["--data-binary", "@-"], Some(bytes), "INSERT");
    }

    /// Run a SELECT and return the raw response body bytes. The query is the raw
    /// POST body, so a `FORMAT Native` response comes back as binary.
    fn select(&self, sql: &str) -> Vec<u8> {
        let url = self.base_url.clone();
        let (ok, stdout, stderr) = self.curl(&url, &["--data-binary", sql], None);
        assert!(
            ok,
            "curl failed for SELECT: {}",
            String::from_utf8_lossy(&stderr)
        );
        // A ClickHouse error comes back as text; a real Native response is
        // binary. Surface the text if it looks like an error.
        if stdout.starts_with(b"Code:") {
            panic!("SELECT errored: {}", String::from_utf8_lossy(&stdout));
        }
        stdout
    }
}

/// Gather every value of column `col` across all chunks, in chunk order, as a
/// debug string, so the supported types can be compared uniformly.
fn column_repr(batch: &ch_core_rs::batch::ChunkedBatch, col: usize) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in &batch.chunks {
        match chunk.column(col) {
            Column::Int8(c) => out.extend(c.values.iter().map(|v| v.to_string())),
            Column::Int16(c) => out.extend(c.values.iter().map(|v| v.to_string())),
            Column::Int32(c) => out.extend(c.values.iter().map(|v| v.to_string())),
            Column::Int64(c) => out.extend(c.values.iter().map(|v| v.to_string())),
            Column::UInt8(c) => out.extend(c.values.iter().map(|v| v.to_string())),
            Column::UInt16(c) => out.extend(c.values.iter().map(|v| v.to_string())),
            Column::UInt32(c) => out.extend(c.values.iter().map(|v| v.to_string())),
            Column::UInt64(c) => out.extend(c.values.iter().map(|v| v.to_string())),
            Column::Float32(c) => out.extend(c.values.iter().map(|v| v.to_bits().to_string())),
            Column::Float64(c) => out.extend(c.values.iter().map(|v| v.to_bits().to_string())),
            Column::Utf8(c) => out.extend((0..c.len()).map(|i| format!("{:?}", c.value(i)))),
            Column::FixedBinary(c) => out.extend((0..c.len()).map(|i| format!("{:?}", c.value(i)))),
            other => panic!("unexpected column {col} variant: {other:?}"),
        }
    }
    out
}

#[test]
#[ignore = "requires a live ClickHouse server matching .server-ref; run with --ignored"]
fn insert_roundtrips_through_server() {
    let server = Server::from_env();
    let batch = sample_batch();

    // Clean slate, then a Memory table matching the batch's columns and order.
    server.ddl(&format!("DROP TABLE IF EXISTS {TABLE}"));
    server.ddl(&format!(
        "CREATE TABLE {TABLE} (\
         i8 Int8, i16 Int16, i32 Int32, i64 Int64, \
         u8 UInt8, u16 UInt16, u32 UInt32, u64 UInt64, \
         f32 Float32, f64 Float64, \
         s String, fs FixedString(4)) ENGINE = Memory"
    ));

    // Encode at revision 0: HTTP INSERT parses the body with server_revision 0,
    // so no BlockInfo preamble and no custom-serialization marker.
    let bytes = encode_block(
        &batch,
        &EncodeOptions {
            protocol_revision: 0,
        },
    )
    .expect("encode numeric batch");
    server.insert_native(&bytes);

    // Read back as Native (HTTP output is revision 0 too) and decode with this
    // crate. ORDER BY i32 is deterministic (i32 is strictly ascending), so the
    // decoded rows line up with the inserted rows.
    let native = server.select(&format!(
        "SELECT i8, i16, i32, i64, u8, u16, u32, u64, f32, f64, s, fs \
         FROM {TABLE} ORDER BY i32 FORMAT Native"
    ));
    let decoded = decode_all_bytes(
        &native,
        &DecodeOptions {
            protocol_revision: 0,
        },
    )
    .expect("decode server Native response");

    server.ddl(&format!("DROP TABLE IF EXISTS {TABLE}"));

    assert_eq!(decoded.num_rows(), batch.num_rows, "row count from server");
    assert_eq!(
        decoded.num_columns(),
        batch.num_columns(),
        "column count from server"
    );

    // The server round-tripped every value: compare each column against the
    // batch we sent, re-decoded through the same crate for an apples-to-apples
    // physical comparison.
    let sent = single_block(&batch);
    for col in 0..batch.num_columns() {
        assert_eq!(
            column_repr(&decoded, col),
            column_repr(&sent, col),
            "column {col} ({}) differs after server round-trip",
            batch.schema.fields[col].name
        );
    }
}

/// Wrap a `ColBatch` as a one-chunk `ChunkedBatch` so `column_repr` can read the
/// sent values with the same code path as the decoded response.
fn single_block(batch: &ColBatch) -> ch_core_rs::batch::ChunkedBatch {
    ch_core_rs::batch::ChunkedBatch {
        schema: batch.schema.clone(),
        chunks: vec![std::sync::Arc::new(batch.clone())],
    }
}
