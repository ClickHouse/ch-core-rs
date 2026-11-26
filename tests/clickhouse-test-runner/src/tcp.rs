//! The `tcp` protocol: the native TCP protocol through `ch-tcp-proto`, the
//! sans-I/O engine of this repository, over one `std::net::TcpStream`.
//!
//! - The connection is the session: `SET`, `USE` and temporary tables last
//!   while the connection lives. The driver loop is the one of
//!   `crates/ch-tcp-proto/README.md`: write the queued output, read, feed,
//!   drain every event, then look at the result of `feed`.
//! - Each `Event::Data`, `Event::Totals` and `Event::Extremes` payload is one
//!   Native block, decoded at `Connection::effective_revision()` with
//!   `decode_next_block`. The leading zero-row block gives the schema of the
//!   result, also of an empty one.
//! - A `DateTime` without a time zone in its type uses the time zone of the
//!   Server Hello.
//! - When the engine fails the connection (`State::Failed`), the shim closes
//!   the socket. The runner then opens a new connection before the next
//!   statement and runs the `SET` statements of the session again, as
//!   `clickhouse-client` keeps its settings and database across a reconnect.

use crate::runner::{Failure, Outcome};
use ch_core_rs::batch::ColBatch;
use ch_core_rs::native::decode::{DecodeOptions, decode_next_block};
use ch_core_rs::native::varint::ByteReader;
use ch_tcp_proto::{ClientHello, Connection, Event, ProtocolError, Query, ServerException, State};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;

/// The client name in the Client Hello (`system.query_log.client_name`).
const CLIENT_NAME: &str = "ch-core-rs clickhouse-test-runner";

/// The client version in the Client Hello: the version of the pinned server,
/// as `clickhouse-client` of that version reports it. With the default 0.0
/// of `ClientHello`, server 26.10 fails a query to two shards
/// (`remote('127.0.0.{1,2}', system.one)`) with `LOGICAL_ERROR` "Sending a
/// distributed query with unknown (zero) client version" (checked live; the
/// same as ClickHouse/ch-go#1231).
const CLIENT_VERSION: (u64, u64) = (26, 10);

/// Bytes read from the socket at a time.
const READ_SIZE: usize = 64 * 1024;

/// An open connection: the socket, the engine, and the time zone of the
/// server.
struct Session {
    stream: TcpStream,
    connection: Connection,
    timezone: String,
}

pub(crate) struct TcpBackend {
    address: String,
    user: String,
    password: String,
    database: String,
    compress: bool,
    session: Option<Session>,
    /// Why the engine failed the last connection, until the runner opens a
    /// new one.
    retired: Option<String>,
}

/// The result of one exchange, before the backend looks at the state of the
/// connection.
enum Exchange {
    /// The exchange ended (EndOfStream, or an Exception); the connection is
    /// usable if the engine says `Ready`.
    Done(Outcome),
    /// The exchange ended, but the shim cannot give the result (for example
    /// a block that the decoder cannot decode).
    Shim(String),
    /// A transport or protocol error: the connection is gone.
    Broken(String),
}

impl TcpBackend {
    /// Opens the connection. `compress` asks the server to compress the
    /// blocks, as `clickhouse-client` does by default.
    pub(crate) fn new(
        address: String,
        user: String,
        password: String,
        database: String,
        compress: bool,
    ) -> Result<Self, Failure> {
        let mut backend = TcpBackend {
            address,
            user,
            password,
            database,
            compress,
            session: None,
            retired: None,
        };
        backend.connect()?;
        Ok(backend)
    }

    /// The database for the next connection. The server keeps the database
    /// of `USE` for the life of the connection.
    pub(crate) fn set_database(&mut self, database: String) {
        self.database = database;
    }

    /// Why the engine failed the connection, if it did since the last
    /// `reconnect`.
    pub(crate) fn retired(&self) -> Option<&str> {
        self.retired.as_deref()
    }

    /// Opens a new connection after the engine failed the last one.
    pub(crate) fn reconnect(&mut self) -> Result<(), Failure> {
        self.retired = None;
        self.connect()
    }

    fn connect(&mut self) -> Result<(), Failure> {
        let hello = ClientHello::new(CLIENT_NAME, self.user.clone())
            .with_client_version(CLIENT_VERSION.0, CLIENT_VERSION.1)
            .with_database(self.database.clone())
            .with_password(self.password.clone());
        let session = Session::open(&self.address, hello, self.compress)?;
        self.session = Some(session);
        Ok(())
    }

    /// Runs one statement and decodes its result. `insert` selects the
    /// INSERT exchange of the engine (`Connection::insert`), which the server
    /// needs for an INSERT that takes its data from the client.
    pub(crate) fn query(
        &mut self,
        sql: &str,
        settings: &BTreeMap<String, String>,
        params: &BTreeMap<String, String>,
        insert: bool,
    ) -> Result<Outcome, Failure> {
        let Some(session) = self.session.as_mut() else {
            return Err(Failure::Other(
                "tcp: no connection (the last one failed)".into(),
            ));
        };
        if crate::input::has_raw_bytes(sql) {
            // `Query::new` takes the SQL as a `String`, and the Query packet
            // has no other way to carry it: a statement with bytes that are
            // not UTF-8 cannot be sent (`src/input.rs`). The connection did
            // not change.
            return Err(Failure::Other(
                "ch-tcp-proto: Query::new takes the SQL as a String (UTF-8), and this statement \
                 has bytes that are not UTF-8"
                    .into(),
            ));
        }
        let mut query = Query::new(sql);
        for (name, value) in settings {
            query = query.with_setting(name, value);
        }
        for (name, value) in params {
            query = query.with_parameter(name, value);
        }
        let exchange = session.exchange(query, insert);
        let state = session.connection.state();
        let result = match exchange {
            Exchange::Done(outcome) => Ok(outcome),
            Exchange::Shim(message) => Err(Failure::Other(message)),
            Exchange::Broken(message) => Err(Failure::Other(message)),
        };
        if state != State::Ready {
            // The engine failed the connection (or the socket is gone). The
            // driver must close the socket itself, and never use the
            // connection again.
            let reason = match &result {
                Ok(Outcome {
                    error: Some(Failure::Server { code, .. }),
                    ..
                }) => format!("{state:?} after server exception {code}"),
                Err(Failure::Other(message)) => format!("{state:?}: {message}"),
                _ => format!("{state:?}"),
            };
            if let Some(session) = self.session.take() {
                let _ = session.stream.shutdown(std::net::Shutdown::Both);
            }
            self.retired = Some(reason);
        }
        result
    }
}

impl Session {
    /// Connects, and runs the handshake and one Ping (the `connect` of
    /// `crates/ch-tcp-proto/README.md`).
    fn open(address: &str, hello: ClientHello, compress: bool) -> Result<Self, Failure> {
        let other =
            |e: &dyn std::fmt::Display| Failure::Other(format!("tcp: connect to {address}: {e}"));
        let mut stream = TcpStream::connect(address).map_err(|e| other(&e))?;
        stream.set_nodelay(true).map_err(|e| other(&e))?;
        let mut connection = Connection::new(hello)
            .map_err(|e| other(&Chain(&e)))?
            .with_compression(compress);
        write_output(&mut stream, &mut connection).map_err(|e| other(&e))?;

        let mut buffer = vec![0_u8; READ_SIZE];
        let mut timezone = String::new();
        let mut pinged = false;
        loop {
            let read = read_some(&mut stream, &mut buffer).map_err(|e| other(&e))?;
            let fed = if read == 0 {
                connection.finish()
            } else {
                connection.feed(&buffer[..read])
            };
            let mut got_pong = false;
            let mut failure = None;
            while let Some(event) = connection.poll_event() {
                match event {
                    Event::ServerHello(server) => timezone = server.timezone.clone(),
                    Event::Pong => got_pong = true,
                    // Terminal in the handshake: a wrong password, for
                    // example.
                    Event::Exception(e) => failure = Some(server_failure(&e)),
                    _ => {}
                }
            }
            if let Some(failure) = failure {
                return Err(failure);
            }
            fed.map_err(|e| other(&Chain(&e)))?;
            if got_pong {
                return Ok(Session {
                    stream,
                    connection,
                    timezone,
                });
            }
            if read == 0 {
                return Err(other(&"the server closed the connection"));
            }
            if connection.state() == State::Ready && !pinged {
                match connection.ping() {
                    Ok(()) => pinged = true,
                    // A server packet is partly received: retry after the
                    // next read.
                    Err(ProtocolError::InvalidState { .. }) => {}
                    Err(e) => return Err(other(&Chain(&e))),
                }
            }
            write_output(&mut stream, &mut connection).map_err(|e| other(&e))?;
        }
    }

    /// Sends the query and reads its whole response.
    fn exchange(&mut self, query: Query, insert: bool) -> Exchange {
        let started = if insert {
            self.connection.insert(query)
        } else {
            self.connection.query(query)
        };
        if let Err(e) = started {
            // The connection did not change: the request was refused before
            // anything was queued (for example a packet that is too large).
            return Exchange::Shim(format!("ch-tcp-proto: {}", Chain(&e)));
        }
        if let Err(e) = write_output(&mut self.stream, &mut self.connection) {
            return Exchange::Broken(format!("tcp: write: {e}"));
        }
        let revision = self.connection.effective_revision().unwrap_or(0);
        let mut outcome = Outcome {
            timezone: Some(self.timezone.clone()),
            ..Outcome::default()
        };
        // The first block that the decoder cannot decode. The shim reads on
        // to the end of the exchange, so that the connection stays usable.
        let mut decode_error = None;
        let decode = |payload: &[u8], into: &mut Vec<ColBatch>, error: &mut Option<String>| {
            match decode_payload(payload, revision) {
                Ok(block) => into.push(block),
                Err(e) => {
                    error.get_or_insert(e);
                }
            }
        };
        // The server asked for the data of the INSERT (`Event::InsertSchema`),
        // and the shim ended the upload without blocks. The shim sends the
        // inline data in the query text, and has no input-format parser to
        // make blocks of it.
        let mut client_fed = false;
        let client_fed_error = |detail: Option<String>| {
            let mut message = "clickhouse-test-runner: the server asks the client for the data of \
                               this INSERT (tcp InsertSchema), and the shim has no input-format \
                               parser"
                .to_owned();
            if let Some(detail) = detail {
                message.push_str(&format!(" (then: {detail})"));
            }
            Exchange::Shim(message)
        };
        let mut finished_insert = false;
        let mut buffer = vec![0_u8; READ_SIZE];
        loop {
            let read = match read_some(&mut self.stream, &mut buffer) {
                Ok(read) => read,
                Err(e) => return Exchange::Broken(format!("tcp: read: {e}")),
            };
            // EOF before EndOfStream is truncation, which `finish` reports.
            let fed = if read == 0 {
                self.connection.finish()
            } else {
                self.connection.feed(&buffer[..read])
            };
            let mut done = false;
            let mut exception = None;
            while let Some(event) = self.connection.poll_event() {
                match event {
                    Event::Data(payload) => {
                        decode(&payload, &mut outcome.blocks, &mut decode_error)
                    }
                    Event::Totals(payload) => {
                        decode(&payload, &mut outcome.totals, &mut decode_error)
                    }
                    Event::Extremes(payload) => {
                        decode(&payload, &mut outcome.extremes, &mut decode_error)
                    }
                    Event::InsertSchema(_) => client_fed = true,
                    Event::Exception(e) => exception = Some(server_failure(&e)),
                    Event::EndOfStream => done = true,
                    // Progress, ProfileInfo, ProfileEvents, TableColumns, and
                    // Log: `clickhouse-client` does not write them to stdout.
                    _ => {}
                }
            }
            // A malformed packet after an Exception in the same chunk failed
            // the connection: report that first.
            if let Err(e) = fed {
                if client_fed {
                    // For example `input()`: the server runs the SELECT after
                    // the upload, and the engine does not support that
                    // exchange (`crates/ch-tcp-proto/README.md`).
                    return client_fed_error(Some(format!("ch-tcp-proto: {}", Chain(&e))));
                }
                return Exchange::Broken(format!("ch-tcp-proto: {}", Chain(&e)));
            }
            if let Some(exception) = exception {
                outcome.error = Some(exception);
                return Exchange::Done(outcome);
            }
            if done {
                if client_fed {
                    return client_fed_error(None);
                }
                if let Some(e) = decode_error {
                    return Exchange::Shim(e);
                }
                return Exchange::Done(outcome);
            }
            if read == 0 {
                return Exchange::Broken("tcp: the server closed the connection".into());
            }
            if client_fed && !finished_insert {
                // The shim has no data to send: end the upload, so that the
                // exchange ends and the connection stays usable.
                match self.connection.finish_insert() {
                    Ok(()) => finished_insert = true,
                    // A server packet is partly received: retry after the
                    // next read.
                    Err(ProtocolError::InvalidState { .. }) => {}
                    Err(e) => return Exchange::Broken(format!("ch-tcp-proto: {}", Chain(&e))),
                }
            }
            if let Err(e) = write_output(&mut self.stream, &mut self.connection) {
                return Exchange::Broken(format!("tcp: write: {e}"));
            }
        }
    }
}

/// An error and each error of its `source()` chain, as `error: source:
/// source ...`. The errors of `ch-tcp-proto` give the cause through `source`
/// only (`ProtocolError::Decode` shows "Native block decode failed", and the
/// `DecodeError` of `ch-core-rs` is its source).
struct Chain<'a>(&'a dyn Error);

impl fmt::Display for Chain<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(error) = source {
            write!(formatter, ": {error}")?;
            source = error.source();
        }
        Ok(())
    }
}

/// Writes the queued output and flushes it, so that a request never waits
/// in a buffer while `read` waits for its reply.
fn write_output(stream: &mut TcpStream, connection: &mut Connection) -> io::Result<()> {
    let output = connection.take_output();
    if output.is_empty() {
        return Ok(());
    }
    stream.write_all(&output)?;
    stream.flush()
}

/// Reads once, and retries a read that a signal interrupted.
fn read_some(stream: &mut TcpStream, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match stream.read(buffer) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

/// Decodes one Data, Totals or Extremes payload, which is exactly one Native
/// block.
fn decode_payload(payload: &[u8], revision: u64) -> Result<ColBatch, String> {
    let mut options = DecodeOptions::default();
    options.protocol_revision = revision;
    let mut reader = ByteReader::new(payload);
    match decode_next_block(&mut reader, &options) {
        Ok(Some(block)) if reader.remaining() == 0 => Ok(block),
        Ok(Some(_)) => Err(format!(
            "ch-core-rs: a block of {} bytes has {} bytes after its end",
            payload.len(),
            reader.remaining()
        )),
        Ok(None) => Err("ch-core-rs: an empty Native payload".into()),
        Err(e) => Err(format!("ch-core-rs: decode a Native block: {e}")),
    }
}

/// A server exception in the form of `clickhouse-client` and of the HTTP
/// error text: `Code: N. DB::Exception: <message>`. The message of the
/// packet already starts with the name of the exception.
fn server_failure(e: &ServerException) -> Failure {
    exception_failure(e.code, &e.name, &e.message)
}

fn exception_failure(code: i32, name: &str, message: &str) -> Failure {
    let message = message.trim_end();
    let message = if message.starts_with(&format!("{name}: ")) {
        format!("Code: {code}. {message}")
    } else {
        format!("Code: {code}. {name}: {message}")
    };
    Failure::Server { code, message }
}

/// The address of the native port: `--port`, else 9000 (9440 with
/// `--secure`). A host with `:` is an IPv6 address.
pub(crate) fn tcp_address(host: &str, port: Option<u16>, secure: bool) -> String {
    let port = port.unwrap_or(if secure { 9440 } else { 9000 });
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(tcp_address("localhost", None, false), "localhost:9000");
        assert_eq!(tcp_address("localhost", None, true), "localhost:9440");
        assert_eq!(
            tcp_address("127.0.0.1", Some(19000), false),
            "127.0.0.1:19000"
        );
        assert_eq!(tcp_address("::1", Some(9000), false), "[::1]:9000");
    }

    #[test]
    fn exception_text() {
        let failure = |name: &str, message: &str| match exception_failure(60, name, message) {
            Failure::Server { code, message } => (code, message),
            f => panic!("{f:?}"),
        };
        // The message of the packet starts with the name (server 26.10).
        assert_eq!(
            failure("DB::Exception", "DB::Exception: Unknown table x\n"),
            (60, "Code: 60. DB::Exception: Unknown table x".to_owned())
        );
        assert_eq!(
            failure("DB::Exception", "Unknown table x"),
            (60, "Code: 60. DB::Exception: Unknown table x".to_owned())
        );
    }

    #[test]
    fn error_sources() {
        // `ProtocolError::Decode` names the cause only through `source`.
        let mut reader = ByteReader::new(b"\x01\x01\x01x\x07NoSuchT\x00");
        let decode = decode_next_block(&mut reader, &DecodeOptions::default()).unwrap_err();
        let cause = decode.to_string();
        assert!(cause.contains("NoSuchT"), "{cause}");
        let error = ProtocolError::from(decode);
        assert_eq!(error.to_string(), "Native block decode failed");
        assert_eq!(
            Chain(&error).to_string(),
            format!("Native block decode failed: {cause}")
        );
        // Each level of a longer chain, in order.
        #[derive(Debug)]
        struct Outer(ProtocolError);
        impl fmt::Display for Outer {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("read a result")
            }
        }
        impl Error for Outer {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&self.0)
            }
        }
        assert_eq!(
            Chain(&Outer(error)).to_string(),
            format!("read a result: Native block decode failed: {cause}")
        );
        // An error without a source: its own text only.
        assert_eq!(
            Chain(&ProtocolError::UnexpectedEof).to_string(),
            "unexpected end of protocol input"
        );
    }

    #[test]
    fn payloads() {
        // A Native block at revision 0: 1 column, 1 row, `x UInt8` = 7.
        let block = b"\x01\x01\x01x\x05UInt8\x07";
        let got = decode_payload(block, 0).unwrap();
        assert_eq!((got.num_rows, got.schema.fields.len()), (1, 1));
        // A schema block: no rows, the column header only.
        let got = decode_payload(b"\x01\x00\x01x\x05UInt8", 0).unwrap();
        assert_eq!((got.num_rows, got.schema.fields[0].name.as_str()), (0, "x"));
        // Two blocks in one payload, a cut block, and nothing are errors.
        let two = [block.as_slice(), block].concat();
        assert!(
            decode_payload(&two, 0)
                .unwrap_err()
                .contains("after its end")
        );
        assert!(decode_payload(&block[..block.len() - 1], 0).is_err());
        assert!(decode_payload(b"", 0).is_err());
    }
}
