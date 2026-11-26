//! Runs the statements of one invocation on one session, and writes their
//! results.
//!
//! The protocol owns the transport and the session (`http`: one
//! `session_id` for all requests; `tcp`: one connection). The runner owns what `clickhouse-client`
//! does with the statements: test hints, `echo`, the command-line settings,
//! `USE` and `SET`, other dialects, and the output formats. The shim sends
//! nothing that `clickhouse-client` does not send, because some tests read
//! their own queries from `system.query_log`.

use crate::args::{Options, http_port};
use crate::format::{OutputFormat, returns_rows, split_output_format, unquote_identifier};
use crate::http::{COMPRESS_ENV, HttpBackend};
use crate::log::InvocationLog;
use crate::render::{Context, Mode, render_cell, write_string};
use crate::split::{
    Statement, is_word_byte, leading_keyword, leading_word_is_call, skip_quoted,
    skip_whitespace_and_comments, top_level_words,
};
use crate::tcp::{TcpBackend, tcp_address};
use ch_core_rs::batch::ColBatch;
use std::collections::BTreeMap;
use std::hash::{BuildHasher, Hasher};
use std::io::Write;

const SYNTAX_ERROR: i32 = 62;

/// The environment variable that selects the protocol.
pub(crate) const PROTOCOL_ENV: &str = "CLICKHOUSE_TEST_RUNNER_PROTOCOL";

/// A triage knob: with `1`, a statement with an output format that the shim
/// cannot write runs without its FORMAT clause, and its result is written as
/// TabSeparated (`format_fallback=` in the invocation log). The test then
/// fails with a diff, but the rest of it runs, so a later failure of another
/// kind (of the decoder, for example) shows.
pub(crate) const FORMAT_FALLBACK_ENV: &str = "CLICKHOUSE_TEST_RUNNER_FORMAT_FALLBACK";

/// The protocol of an invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Protocol {
    /// HTTP, `FORMAT Native` (`src/http.rs`).
    Http,
    /// The native TCP protocol through `ch-tcp-proto`.
    Tcp,
}

impl Protocol {
    pub(crate) fn from_env(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("") | Some("http") => Ok(Protocol::Http),
            Some("tcp") => Ok(Protocol::Tcp),
            Some(v) => Err(format!("unknown {PROTOCOL_ENV} {v:?}: use http or tcp")),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Protocol::Http => "http",
            Protocol::Tcp => "tcp",
        }
    }
}

/// Why a statement or the invocation failed.
#[derive(Debug)]
pub(crate) enum Failure {
    /// A server exception (`Code: N. DB::Exception: ...`).
    Server { code: i32, message: String },
    /// A result that does not match the test hint.
    Hint { message: String, code: Option<i32> },
    /// Any other error: of the decoder, of the transport, of the shim, or of
    /// the output.
    Other(String),
}

/// The result of one statement: the decoded blocks, the time zone of the
/// query (for a `DateTime` without a time zone in its type), and an
/// exception that came after the blocks.
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    pub(crate) blocks: Vec<ColBatch>,
    /// The `WITH TOTALS` row (`tcp` only: HTTP `Native` has no totals).
    pub(crate) totals: Vec<ColBatch>,
    /// The `extremes = 1` rows, minimum and maximum (`tcp` only).
    pub(crate) extremes: Vec<ColBatch>,
    pub(crate) timezone: Option<String>,
    pub(crate) error: Option<Failure>,
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Failure::Other(format!("write stdout: {e}"))
    }
}

/// Reads the error code of a server exception. The client gives the text
/// of the exception (`Code: 60. DB::Exception: ...`), or only `Code: 60`
/// from the `X-ClickHouse-Exception-Code` header when the body is empty.
/// An exception that is not a `DB::Exception` has a prefix before the code
/// (`Poco::Exception. Code: 1000, e.code() = 0, ...`; the forms of
/// `getCurrentExceptionMessage` in ClickHouse's `src/Common/Exception.cpp`).
pub(crate) fn server_error_code(message: &str) -> Option<i32> {
    const PREFIXES: [&str; 3] = [
        "Poco::Exception. ",
        "std::exception. ",
        "Unknown exception. ",
    ];
    let message = message.trim_start();
    let message = PREFIXES
        .iter()
        .find_map(|p| message.strip_prefix(p))
        .unwrap_or(message);
    let rest = message.strip_prefix("Code: ")?;
    let end = rest
        .bytes()
        .position(|c| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Tells if a `SYNTAX_ERROR` comes from a function that parses its argument
/// (`formatQuery`, `formatQuerySingleLine`), not from the statement itself.
/// The statement then parsed, and the server adds the context of the
/// function to the message: `In scope ...` (analyzer) or `While processing
/// ...` (old analyzer). A statement that does not parse has no such context.
fn is_syntax_error_of_function(message: &str) -> bool {
    message.contains(": In scope ") || message.contains(": While processing ")
}

/// The transport of the invocation. One exists per invocation, so the size
/// of the engine's state does not matter: it is boxed for the lint.
enum Backend {
    Http(HttpBackend),
    Tcp(Box<TcpBackend>),
}

pub(crate) struct Runner<'a> {
    log: &'a mut InvocationLog,
    backend: Backend,
    /// The `SET` statements that succeeded (`tcp`). After the engine fails
    /// the connection, they run again on the new connection.
    session_init: Vec<String>,
    params: BTreeMap<String, String>,
    /// The command-line settings that go with each query, as
    /// clickhouse-client sends them. A `SET` of the same setting in the test
    /// removes it from here, because the session then holds the value.
    settings: BTreeMap<String, String>,
    /// The value of the last `SET dialect`.
    dialect: String,
    /// Set by `SET implicit_select = 1`, with which an expression alone
    /// (`1 + 2`) is a query.
    implicit: bool,
    /// The value of the last `SET output_format_native_encode_types_in_binary_format`:
    /// the server then writes the column types of a Native result in the
    /// binary encoding.
    binary_types: bool,
    /// `FORMAT_FALLBACK_ENV` is `1`.
    format_fallback: bool,
}

/// The setting that makes the server write the column types of a Native
/// result in the binary encoding.
const BINARY_TYPES_SETTING: &str = "output_format_native_encode_types_in_binary_format";

impl<'a> Runner<'a> {
    pub(crate) fn new(
        opts: &Options,
        protocol: Protocol,
        log: &'a mut InvocationLog,
    ) -> Result<Self, Failure> {
        let backend = match protocol {
            Protocol::Http => {
                let session_id = new_session_id();
                log.add("session_id", &session_id);
                let scheme = if opts.secure { "https" } else { "http" };
                let port = http_port(opts.port, opts.secure, |name| std::env::var(name).ok());
                let host = if opts.host.contains(':') {
                    format!("[{}]", opts.host)
                } else {
                    opts.host.clone()
                };
                let url = format!("{scheme}://{host}:{port}/");
                log.add("url", &url);
                Backend::Http(
                    HttpBackend::new(
                        url,
                        opts.user.clone(),
                        opts.password.clone(),
                        opts.database.clone(),
                        session_id,
                        |name| std::env::var(name).ok(),
                    )
                    .map_err(Failure::Other)?,
                )
            }
            Protocol::Tcp => {
                if opts.secure {
                    return Err(Failure::Other(
                        "clickhouse-test-runner: --secure is not supported over tcp: the shim \
                         has no TLS"
                            .into(),
                    ));
                }
                let address = tcp_address(&opts.host, opts.port, opts.secure);
                log.add("address", &address);
                // Compressed, as clickhouse-client by default. `0` for triage.
                let compress = std::env::var(COMPRESS_ENV).as_deref() != Ok("0");
                Backend::Tcp(Box::new(TcpBackend::new(
                    address,
                    opts.user.clone(),
                    opts.password.clone(),
                    opts.database.clone(),
                    compress,
                )?))
            }
        };
        Ok(Runner {
            log,
            backend,
            session_init: Vec::new(),
            params: opts.params.clone(),
            settings: opts.settings.clone(),
            dialect: String::new(),
            implicit: false,
            binary_types: false,
            format_fallback: std::env::var(FORMAT_FALLBACK_ENV).as_deref() == Ok("1"),
        })
    }

    /// Runs the statements in order. It stops at the first statement whose
    /// result does not match its test hint, as `clickhouse-client` does.
    pub(crate) fn run(&mut self, stmts: &[Statement], out: &mut dyn Write) -> Result<(), Failure> {
        for st in stmts {
            self.run_one(st, out)?;
        }
        Ok(())
    }

    fn run_one(&mut self, st: &Statement, out: &mut dyn Write) -> Result<(), Failure> {
        if let Some(err) = &st.hint.parse_error {
            self.echo(st, out)?;
            return Err(Failure::Other(err.clone()));
        }
        self.log.add("query", &st.sql);
        let result = self.execute(&st.sql);
        // clickhouse-client parses a statement before it echoes it, so it does
        // not echo a statement with a syntax error.
        let unparsable = matches!(
            &result,
            (_, Some(Failure::Server {
                code: SYNTAX_ERROR,
                message,
            })) if !is_syntax_error_of_function(message)
        );
        if !unparsable {
            self.echo(st, out)?;
        }
        // The rows before an exception are written, as clickhouse-client
        // does.
        let (text, error) = result;
        out.write_all(&text)?;
        out.flush()?;
        let result = match error {
            None => Ok(()),
            Some(e) => Err(e),
        };

        match result {
            Ok(()) if st.hint.expects_error() => Err(Failure::Hint {
                message: format!(
                    "expected {}, but the query succeeded (query: {})",
                    st.hint.label(),
                    st.full
                ),
                code: None,
            }),
            Ok(()) => self.after_success(&st.sql),
            Err(Failure::Server { code, message }) if st.hint.expects_error() => {
                if st.hint.matches(code) {
                    self.log.add("expected_error_matched", &st.hint.label());
                    Ok(())
                } else {
                    Err(Failure::Hint {
                        message: format!(
                            "expected {}, but got {message} (query: {})",
                            st.hint.label(),
                            st.full
                        ),
                        code: Some(code),
                    })
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Writes the statement for the `echo` test hints.
    fn echo(&self, st: &Statement, out: &mut dyn Write) -> std::io::Result<()> {
        if st.hint.echo_this {
            out.write_all(&crate::input::encode(&st.full))?;
            out.write_all(b"\n")?;
        }
        Ok(())
    }

    /// Runs one statement and returns its output, and its error. An
    /// exception after some rows gives the output of the rows too.
    fn execute(&mut self, sql: &str) -> (Vec<u8>, Option<Failure>) {
        let mut out = Vec::new();
        match self.execute_into(sql, &mut out) {
            Ok(()) => (out, None),
            Err(e) => (out, Some(e)),
        }
    }

    fn execute_into(&mut self, sql: &str, out: &mut Vec<u8>) -> Result<(), Failure> {
        let other_dialect = self.other_dialect(sql);
        let sql = if other_dialect {
            // Over HTTP the server parses the request body as one query, and
            // the parsers of other dialects do not accept a trailing
            // semicolon.
            let s = sql.trim_end_matches([' ', '\t', '\r', '\n']);
            s.strip_suffix(';').unwrap_or(s)
        } else {
            sql
        };
        let is_query = returns_rows(sql) || other_dialect || self.implicit_select(sql);
        let (query, format) = if !is_query || other_dialect {
            // The FORMAT clause of an INSERT names the format of its data,
            // and other dialects can use the word differently (the `format`
            // pipe of LogsQL). A result still comes as Native, and is
            // written as TabSeparated (`KILL` returns rows).
            (sql.to_owned(), OutputFormat::tab_separated())
        } else {
            split_output_format(sql)
        };
        if !format.supported && self.format_fallback {
            self.log.add("format_fallback", &format.name);
        }
        let mut format = writable_format(format, self.format_fallback)?;
        let explain_null = format.discard && leading_keyword(sql) == "EXPLAIN";
        if explain_null {
            // With ignore_format_null_for_explain (on by default), the server
            // ignores `FORMAT Null` for EXPLAIN and sends the plan. The server
            // decides, so write the rows that it sends.
            format = OutputFormat::tab_separated();
        }
        // With `FORMAT Null`, send the text as it is: over http the server
        // sends no rows, over tcp it sends the blocks, which are decoded but
        // not written. Otherwise the clause is gone, and the result is Native.
        let query = if format.discard || explain_null {
            sql.to_owned()
        } else {
            query
        };
        // The FORMAT clause and the inline data stay in the text of an
        // INSERT. Over tcp, an INSERT uses the INSERT exchange of the engine.
        let insert = !other_dialect && leading_keyword(&query) == "INSERT";
        self.restore_session()?;
        // Over HTTP the shim decodes the result in the mode of the type
        // headers: the SETTINGS clause of the statement, else the session.
        // Over tcp the engine frames each block with the text mode
        // (`crates/ch-tcp-proto/README.md`: it then fails on the first block).
        let binary_types = parse_settings_clause(&query)
            .get(BINARY_TYPES_SETTING)
            .map_or(self.binary_types, |v| is_true(v));
        let outcome = match &mut self.backend {
            Backend::Http(http) => {
                http.query(&query, &self.settings, &self.params, binary_types)?
            }
            Backend::Tcp(tcp) => tcp.query(&query, &self.settings, &self.params, insert)?,
        };
        write_result(out, &outcome, &format)?;
        match outcome.error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Opens a new connection when the engine failed the last one (`tcp`),
    /// and runs the `SET` statements of the session on it. The database of
    /// the last `USE` goes in the Client Hello. `clickhouse-client` also
    /// keeps its settings and its database across a reconnect; temporary
    /// tables are lost.
    fn restore_session(&mut self) -> Result<(), Failure> {
        let Backend::Tcp(tcp) = &mut self.backend else {
            return Ok(());
        };
        let Some(reason) = tcp.retired() else {
            return Ok(());
        };
        self.log.add("tcp_reconnect", reason);
        tcp.reconnect()?;
        for sql in &self.session_init {
            self.log.add("session_init", sql);
            let outcome = tcp.query(sql, &self.settings, &self.params, false)?;
            if let Some(e) = outcome.error {
                return Err(Failure::Other(format!(
                    "clickhouse-test-runner: restore the session on a new tcp connection with \
                     {sql:?}: {e:?}"
                )));
            }
        }
        Ok(())
    }

    /// Tells if the session uses another query language than SQL
    /// (`SET dialect = 'kusto'`, 'prql', ...). The shim cannot see if a
    /// statement in such a language returns rows, so it reads a result for
    /// each statement.
    fn other_dialect(&self, sql: &str) -> bool {
        !self.dialect.is_empty()
            && !self.dialect.eq_ignore_ascii_case("clickhouse")
            && leading_keyword(sql) != "SET"
    }

    /// Tells if the statement is an expression alone, which
    /// `SET implicit_select = 1` makes a query.
    fn implicit_select(&self, sql: &str) -> bool {
        self.implicit && !is_command(sql)
    }

    /// Updates the session state after a `USE` or `SET` statement.
    fn after_success(&mut self, sql: &str) -> Result<(), Failure> {
        match leading_keyword(sql).as_str() {
            "USE" => {
                let db = parse_use_database(sql, &self.params).ok_or_else(|| {
                    Failure::Other(format!(
                        "clickhouse-test-runner: cannot read the database name of {sql:?}"
                    ))
                })?;
                // http: the client sends its database with each request,
                // and that database replaces the current database of the
                // session. tcp: the database of a new connection.
                self.log.add("database", &db);
                match &mut self.backend {
                    Backend::Http(http) => http.set_database(db),
                    Backend::Tcp(tcp) => tcp.set_database(db),
                }
            }
            "SET" => {
                if matches!(self.backend, Backend::Tcp(_)) {
                    self.session_init.push(sql.to_owned());
                }
                for (name, value) in parse_set_assignments(sql) {
                    self.settings.remove(&name);
                    match name.as_str() {
                        "dialect" => self.dialect = value,
                        "implicit_select" => self.implicit = is_true(&value),
                        BINARY_TYPES_SETTING => self.binary_types = is_true(&value),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// The format in which the shim writes a result: an output format that it
/// cannot write is an error, or with `fallback` (`FORMAT_FALLBACK_ENV`)
/// TabSeparated.
fn writable_format(format: OutputFormat, fallback: bool) -> Result<OutputFormat, Failure> {
    if format.supported {
        return Ok(format);
    }
    if fallback {
        return Ok(OutputFormat::tab_separated());
    }
    Err(Failure::Other(format!(
        "clickhouse-test-runner: output format {} is not supported: the shim reads results as \
         Native with ch-core-rs and writes only the TabSeparated formats and Null",
        format.name
    )))
}

/// Writes the result of a statement as `clickhouse-client` writes it in a
/// TabSeparated format: the header lines with the first block (over tcp the
/// first block has no rows, so an empty result has its header too), the rows,
/// then `WITH TOTALS` and `extremes = 1` rows, each after an empty line
/// (`TabSeparatedRowOutputFormat::writeBeforeTotals`, `writeBeforeExtremes`).
fn write_result(
    out: &mut Vec<u8>,
    outcome: &Outcome,
    format: &OutputFormat,
) -> Result<(), Failure> {
    let ctx = Context {
        default_timezone: outcome.timezone.clone(),
    };
    let mut header = format.names || format.types;
    for block in &outcome.blocks {
        let fields = &block.schema.fields;
        if header {
            header = false;
            if format.names {
                write_header(out, fields.iter().map(|f| f.name.as_bytes()), format.raw);
            }
            if format.types {
                let types: Vec<String> = fields.iter().map(|f| f.ch_type.to_string()).collect();
                write_header(out, types.iter().map(|t| t.as_bytes()), format.raw);
            }
        }
        // `FORMAT Null`: the values are decoded, but not written.
        write_rows(out, block, format, &ctx)?;
    }
    for block in outcome.totals.iter().chain(&outcome.extremes) {
        if !format.discard {
            out.push(b'\n');
        }
        write_rows(out, block, format, &ctx)?;
    }
    Ok(())
}

/// Writes the rows of one block (nothing for `FORMAT Null`).
fn write_rows(
    out: &mut Vec<u8>,
    block: &ColBatch,
    format: &OutputFormat,
    ctx: &Context,
) -> Result<(), Failure> {
    let fields = &block.schema.fields;
    if block.columns.len() != fields.len() {
        return Err(Failure::Other(format!(
            "ch-core-rs: a block has {} columns and {} fields",
            block.columns.len(),
            fields.len()
        )));
    }
    if format.discard {
        return Ok(());
    }
    for row in 0..block.num_rows {
        for (i, (field, column)) in fields.iter().zip(&block.columns).enumerate() {
            if i > 0 {
                out.push(b'\t');
            }
            render_cell(out, &field.ch_type, column, row, format.raw, ctx)
                .map_err(|e| Failure::Other(format!("column {:?}: {e}", field.name)))?;
        }
        out.push(b'\n');
    }
    Ok(())
}

/// Writes a header line (names or types). A Raw format writes it without
/// escapes, as its rows (`TabSeparatedRowOutputFormat::writeLine` in
/// ClickHouse).
fn write_header<'b>(out: &mut Vec<u8>, cells: impl Iterator<Item = &'b [u8]>, raw: bool) {
    for (i, cell) in cells.enumerate() {
        if i > 0 {
            out.push(b'\t');
        }
        write_string(out, cell, if raw { Mode::Plain } else { Mode::Escaped });
    }
    out.push(b'\n');
}

/// Tells if the statement starts with the keyword of a DDL, DML or other
/// command, which `SET implicit_select = 1` does not make a query. A first
/// word that a `(` follows is a function call (`replace('a', 'a', 'b')`), not
/// a keyword.
fn is_command(sql: &str) -> bool {
    !leading_word_is_call(sql)
        && matches!(
            leading_keyword(sql).as_str(),
            "INSERT"
                | "DELETE"
                | "UPDATE"
                | "REPLACE"
                | "CREATE"
                | "DROP"
                | "ALTER"
                | "SET"
                | "USE"
                | "TRUNCATE"
                | "RENAME"
                | "OPTIMIZE"
                | "SYSTEM"
                | "GRANT"
                | "REVOKE"
                | "ATTACH"
                | "DETACH"
                | "KILL"
                | "EXCHANGE"
                | "UNDROP"
                | "BACKUP"
                | "RESTORE"
        )
}

fn new_session_id() -> String {
    // Two hashers with random keys: 128 random bits without a dependency.
    let mut id = String::from("ch-core-rs-test-runner-");
    for _ in 0..2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        );
        h.write_u32(std::process::id());
        id.push_str(&format!("{:016x}", h.finish()));
    }
    id
}

/// Returns the database name of a `USE` statement. A `{name:Identifier}`
/// query parameter is replaced by its value.
pub(crate) fn parse_use_database(sql: &str, params: &BTreeMap<String, String>) -> Option<String> {
    let i = skip_whitespace_and_comments(sql.as_bytes(), 0) + "USE".len();
    let mut rest = sql.get(i..)?.trim();
    rest = rest.strip_suffix(';').unwrap_or(rest).trim();
    if let Some((word, after)) = rest.split_once(' ')
        && word.eq_ignore_ascii_case("DATABASE")
    {
        rest = after.trim();
    }
    match rest.as_bytes().first()? {
        b'`' | b'"' => unquote_identifier(rest),
        b'{' => {
            let (name, _) = rest.trim_matches(['{', '}']).split_once(':')?;
            params.get(name.trim()).cloned()
        }
        _ if rest.bytes().all(is_word_byte) => Some(rest.to_owned()),
        _ => None,
    }
}

/// Returns the settings that a `SET a = 1, b = 'x'` statement assigns, with
/// their values (string values without quotes).
pub(crate) fn parse_set_assignments(sql: &str) -> BTreeMap<String, String> {
    let i = skip_whitespace_and_comments(sql.as_bytes(), 0) + "SET".len();
    parse_assignments(sql.get(i..).unwrap_or(""))
}

/// Returns the settings of the `SETTINGS` clause of the statement itself (the
/// last top-level one; a clause in a subquery is not for the result). The
/// clause ends at a `FORMAT` clause or at the end of the statement.
pub(crate) fn parse_settings_clause(sql: &str) -> BTreeMap<String, String> {
    let offset = |word: &str| word.as_ptr() as usize - sql.as_ptr() as usize;
    let words = top_level_words(sql);
    // `system.settings` is a table, not the clause.
    let clause = words.iter().rposition(|w| {
        w.eq_ignore_ascii_case("SETTINGS") && !sql[..offset(w)].trim_end().ends_with('.')
    });
    let Some(k) = clause else {
        return BTreeMap::new();
    };
    let start = offset(words[k]) + words[k].len();
    let end = words[k + 1..]
        .iter()
        .find(|w| w.eq_ignore_ascii_case("FORMAT"))
        .map_or(sql.len(), |w| offset(w));
    parse_assignments(&sql[start..end])
}

/// The value of a Bool setting: `0` and `false` are off.
fn is_true(value: &str) -> bool {
    value != "0" && !value.eq_ignore_ascii_case("false")
}

/// Splits `a = 1, b = 'x'` at the top-level commas (string values without
/// quotes).
fn parse_assignments(body: &str) -> BTreeMap<String, String> {
    let body = body.trim();
    let body = body.strip_suffix(';').unwrap_or(body).trim_end();
    let mut out = BTreeMap::new();
    let mut add = |part: &str| {
        if let Some((name, value)) = part.split_once('=') {
            let value = value.trim();
            let value = if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
                &value[1..value.len() - 1]
            } else {
                value
            };
            out.insert(name.trim().to_owned(), value.to_owned());
        }
    };
    let b = body.as_bytes();
    let (mut depth, mut start, mut j) = (0i32, 0, 0);
    while j < b.len() {
        match b[j] {
            b'\'' | b'"' | b'`' => {
                j = skip_quoted(b, j);
                continue;
            }
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b',' if depth == 0 => {
                add(&body[start..j]);
                start = j + 1;
            }
            _ => {}
        }
        j += 1;
    }
    add(&body[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_database() {
        let params = BTreeMap::from([("CLICKHOUSE_DATABASE".to_owned(), "test_x".to_owned())]);
        for (input, want) in [
            ("USE db1;", "db1"),
            ("use `my db`", "my db"),
            ("-- c\nUSE DATABASE db2", "db2"),
            ("USE {CLICKHOUSE_DATABASE:Identifier};", "test_x"),
            ("USE \"db3\"", "db3"),
        ] {
            assert_eq!(
                parse_use_database(input, &params).as_deref(),
                Some(want),
                "{input}"
            );
        }
        assert_eq!(parse_use_database("USE db1, db2", &params), None);
    }

    #[test]
    fn syntax_error_of_function() {
        // Messages of server 26.10 (test 04700_backup_settings_default_round_trip).
        assert!(is_syntax_error_of_function(
            "Code: 62. DB::Exception: Syntax error: failed at position 87 (end of query): . \
             Expected one of: token, Comma, end of query: In scope SELECT \
             formatQuerySingleLine('BACKUP TABLE t'). (SYNTAX_ERROR)"
        ));
        assert!(!is_syntax_error_of_function(
            "Code: 62. DB::Exception: Syntax error: failed at position 11 (end of query): . \
             Expected one of: token, DoubleColon, OR, AND. (SYNTAX_ERROR)"
        ));
    }

    #[test]
    fn set_assignments() {
        assert_eq!(
            parse_set_assignments("SET dialect = 'kusto', max_threads=1, x = (1, 2);"),
            BTreeMap::from([
                ("dialect".to_owned(), "kusto".to_owned()),
                ("max_threads".to_owned(), "1".to_owned()),
                ("x".to_owned(), "(1, 2)".to_owned()),
            ])
        );
    }

    #[test]
    fn settings_clauses() {
        let clause = |sql: &str| parse_settings_clause(sql).into_iter().collect::<Vec<_>>();
        let pair = |n: &str, v: &str| (n.to_owned(), v.to_owned());
        assert_eq!(
            clause("SELECT 1 SETTINGS max_threads = 3, dialect = 'kusto';"),
            [pair("dialect", "kusto"), pair("max_threads", "3")]
        );
        // The clause ends at FORMAT, before or after it.
        assert_eq!(
            clause("SELECT 1 SETTINGS max_threads = 3 FORMAT Null"),
            [pair("max_threads", "3")]
        );
        assert_eq!(
            clause("SELECT 1 FORMAT Null SETTINGS max_threads = 3"),
            [pair("max_threads", "3")]
        );
        // The clause of a subquery, a table named `settings`, a string and a
        // comment are not the clause of the statement.
        assert!(clause("SELECT * FROM (SELECT 1 SETTINGS max_threads = 3)").is_empty());
        assert!(clause("SELECT name FROM system.settings WHERE name = 'max_threads'").is_empty());
        assert!(clause("SELECT 'SETTINGS a = 1' -- SETTINGS b = 2").is_empty());
        assert_eq!(
            clause("SELECT 1 FROM (SELECT 2 SETTINGS a = 1) SETTINGS b = 2"),
            [pair("b", "2")]
        );
    }

    #[test]
    fn binary_type_values() {
        assert!(is_true("1") && is_true("true") && is_true("True"));
        assert!(!is_true("0") && !is_true("false") && !is_true("FALSE"));
    }

    #[test]
    fn error_codes() {
        assert_eq!(
            server_error_code("Code: 60. DB::Exception: Unknown table. (UNKNOWN_TABLE)"),
            Some(60)
        );
        assert_eq!(server_error_code("Code: 395"), Some(395));
        assert_eq!(server_error_code("502 Bad Gateway"), None);
        // The forms of the exceptions that are not a DB::Exception.
        assert_eq!(
            server_error_code(
                "Poco::Exception. Code: 1000, e.code() = 0, Bad URI syntax: URI contains invalid \
                 characters (version 26.10.4.207 (official build))"
            ),
            Some(1000)
        );
        assert_eq!(
            server_error_code("std::exception. Code: 1001, type: std::bad_alloc, e.what() = x"),
            Some(1001)
        );
        assert_eq!(
            server_error_code("Unknown exception. Code: 1002, type: X"),
            Some(1002)
        );
        assert_eq!(server_error_code("Poco::Exception. no code"), None);
    }

    #[test]
    fn commands_are_not_implicit_selects() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "DELETE FROM t WHERE x = 1",
            "update t SET x = 2 WHERE x = 1",
            "REPLACE TABLE t AS SELECT 1",
            "CREATE TABLE t (x UInt8) ENGINE = Memory",
            "-- c\nALTER TABLE t DELETE WHERE 1",
            "SET implicit_select = 0",
        ] {
            assert!(is_command(sql), "{sql}");
        }
        for sql in [
            "1 + 1",
            "'a'",
            "SELECT 1",
            "now() > 0",
            "replace('abc', 'b', 'x')",
            "REPLACE ('a', 'a', 'b')",
            "(replace('a', 'a', 'b'))",
        ] {
            assert!(!is_command(sql), "{sql}");
        }
    }

    /// Decodes one Native block of revision 0 with one `UInt8` column `x`.
    fn uint8_block(values: &[u8]) -> ColBatch {
        let mut bytes = vec![1, values.len() as u8, 1, b'x', 5];
        bytes.extend_from_slice(b"UInt8");
        bytes.extend_from_slice(values);
        let mut reader = ch_core_rs::native::varint::ByteReader::new(&bytes);
        ch_core_rs::native::decode::decode_next_block(&mut reader, &Default::default())
            .unwrap()
            .unwrap()
    }

    #[test]
    fn totals_and_extremes_follow_an_empty_line() {
        let outcome = Outcome {
            blocks: vec![uint8_block(&[]), uint8_block(&[3, 5])],
            totals: vec![uint8_block(&[8])],
            extremes: vec![uint8_block(&[3, 5])],
            ..Outcome::default()
        };
        let mut format = OutputFormat::tab_separated();
        format.names = true;
        let mut out = Vec::new();
        write_result(&mut out, &outcome, &format).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "x\n3\n5\n\n8\n\n3\n5\n");

        // FORMAT Null: no rows, no empty lines.
        format.names = false;
        format.discard = true;
        let mut out = Vec::new();
        write_result(&mut out, &outcome, &format).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn format_fallback() {
        let (_, vertical) = split_output_format("SELECT 1 FORMAT Vertical");
        match writable_format(vertical.clone(), false) {
            Err(Failure::Other(m)) => {
                assert!(m.contains("output format Vertical is not supported"), "{m}")
            }
            other => panic!("{other:?}"),
        }
        let fallback = writable_format(vertical, true).unwrap();
        assert_eq!(fallback, OutputFormat::tab_separated());
        // A format that the shim writes is the same with or without the knob.
        let (_, names) = split_output_format("SELECT 1 FORMAT TSVWithNames");
        for knob in [false, true] {
            assert_eq!(writable_format(names.clone(), knob).unwrap(), names);
        }
    }

    #[test]
    fn session_ids_differ() {
        assert_ne!(new_session_id(), new_session_id());
    }
}
