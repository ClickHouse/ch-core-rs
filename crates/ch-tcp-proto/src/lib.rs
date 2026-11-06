#![doc = include_str!("../README.md")]

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;

/// The highest TCP protocol revision this crate declares.
pub const ENGINE_REVISION: u64 = 54_460;

// At v26.8.1.2041-lts `src/Server/TCPHandler.cpp::sendHello` writes more
// Server Hello fields above this revision: password-complexity rules at 54461,
// an 8-byte nonce at 54462, chunked-protocol capabilities at 54470, and a
// parallel-replicas VarUInt between the revision and the timezone at 54471.
// The Server Hello parser implements none of them, so raising the declared
// revision past 54460 must come with those fields.
const _: () = assert!(
    ENGINE_REVISION < 54_461,
    "raising ENGINE_REVISION requires parsing the Server Hello fields gated at 54461 and above"
);

const CLIENT_HELLO_STRING_LIMIT: usize = 64 * 1024;
const SERVER_HELLO_STRING_LIMIT: usize = 4096;
/// Bytes of each Exception text field kept. Longer fields are truncated.
const EXCEPTION_STRING_LIMIT: usize = 1024 * 1024;
/// Largest declared Exception field length accepted. Engine policy: at
/// v26.8.1.2041-lts neither `writeException` nor `readException` caps the
/// length, so without this a corrupt or hostile length such as `u64::MAX`
/// would keep the engine skipping forever without ever completing or
/// failing. 64 MiB is 64 times the retained cap and far above any real
/// message plus stack trace, and it bounds the bytes skipped per field.
const EXCEPTION_DECLARED_LIMIT: usize = 64 * 1024 * 1024;
/// Backstop on retained input. Data blocks are not meant to raise this or
/// `need`: they will be streamed through core's
/// `ch_core_rs::native::stream_decoder::StreamDecoder`, and a
/// `ProtocolError` variant wrapping core's `DecodeError` will be added then.
const INPUT_LIMIT: usize = 4 * 1024 * 1024;
/// Input buffer capacity above which a mostly drained buffer is shrunk.
const INPUT_SHRINK_THRESHOLD: usize = 64 * 1024;

const CLIENT_PACKET_HELLO: u64 = 0;
const CLIENT_PACKET_PING: u64 = 4;
const SERVER_PACKET_HELLO: u64 = 0;
const SERVER_PACKET_EXCEPTION: u64 = 2;
const SERVER_PACKET_PONG: u64 = 4;

// Thresholds from `src/Core/ProtocolDefines.h` at v26.8.1.2041-lts.
const MIN_REVISION_WITH_SERVER_TIMEZONE: u64 = 54_058;
const MIN_REVISION_WITH_SERVER_DISPLAY_NAME: u64 = 54_372;
const MIN_REVISION_WITH_VERSION_PATCH: u64 = 54_401;
/// `DBMS_MIN_PROTOCOL_VERSION_WITH_ADDENDUM`: the client sends the addendum.
const MIN_PROTOCOL_VERSION_WITH_ADDENDUM: u64 = 54_458;
/// `DBMS_MIN_PROTOCOL_VERSION_WITH_QUOTA_KEY`: the addendum carries the quota
/// key. This is not `DBMS_MIN_REVISION_WITH_QUOTA_KEY_IN_CLIENT_INFO` (54060),
/// which gates the quota key inside the Query packet's ClientInfo.
const MIN_PROTOCOL_VERSION_WITH_QUOTA_KEY: u64 = 54_458;

// ---------------------------------------------------------------------------
// Public values
// ---------------------------------------------------------------------------

/// Values sent in the initial Client Hello and its post-Hello addendum.
///
/// Build it with [`ClientHello::new`] and the `with_*` setters. The client
/// version defaults to `0.0`; drivers should report their own version.
/// `Debug` redacts the password and quota key. `Clone` copies the plaintext
/// password like any other field, and neither this type nor [`Connection`]
/// zeroizes secrets on drop: this crate has no dependencies and makes no
/// zeroization promise.
#[derive(Clone)]
pub struct ClientHello {
    client_name: String,
    version_major: u64,
    version_minor: u64,
    database: String,
    user: String,
    password: String,
    quota_key: String,
}

impl ClientHello {
    /// Creates a Client Hello with an empty database, password, and quota key.
    pub fn new(client_name: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            client_name: client_name.into(),
            version_major: 0,
            version_minor: 0,
            database: String::new(),
            user: user.into(),
            password: String::new(),
            quota_key: String::new(),
        }
    }

    /// Sets the client version reported to the server.
    pub fn with_client_version(mut self, major: u64, minor: u64) -> Self {
        self.version_major = major;
        self.version_minor = minor;
        self
    }

    /// Sets the default database. Empty means the server default.
    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = database.into();
        self
    }

    /// Sets the plaintext password sent in the Client Hello.
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = password.into();
        self
    }

    /// Sets the quota key sent in the addendum.
    pub fn with_quota_key(mut self, quota_key: impl Into<String>) -> Self {
        self.quota_key = quota_key.into();
        self
    }
}

impl fmt::Debug for ClientHello {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientHello")
            .field("client_name", &self.client_name)
            .field("version_major", &self.version_major)
            .field("version_minor", &self.version_minor)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("quota_key", &"<redacted>")
            .finish()
    }
}

/// Values received from the Server Hello.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ServerHello {
    pub server_name: String,
    pub version_major: u64,
    pub version_minor: u64,
    /// The server's declared revision before applying the engine ceiling.
    pub revision: u64,
    /// `min(ENGINE_REVISION, revision)`, used for every subsequent field gate.
    pub effective_revision: u64,
    pub timezone: String,
    pub display_name: String,
    pub version_patch: u64,
}

/// A ClickHouse exception received from the server.
///
/// The text fields are decoded with lossy UTF-8 so that invalid bytes never
/// cost the caller the error code.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ServerException {
    pub code: i32,
    pub name: String,
    pub message: String,
    pub stack_trace: String,
    /// The obsolete trailing nested-exception flag, recorded as received.
    ///
    /// At v26.8.1.2041-lts `writeException` always writes `false`, and
    /// `readException` reads no payload after the flag whatever its value.
    /// This engine likewise parses nothing after it.
    pub has_nested: bool,
    /// At least one text field was longer than the 1 MiB per-field cap and
    /// was cut to its first 1 MiB of wire bytes. The cut may split a UTF-8
    /// sequence, which then decodes as U+FFFD.
    ///
    /// The cap applies to wire bytes, not to the decoded `String`. Lossy
    /// decoding replaces each invalid byte with U+FFFD, which is 3 bytes, so
    /// a field holds at most 3 MiB even when `truncated` is false.
    pub truncated: bool,
}

/// A complete protocol event, read with [`Connection::poll_event`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Event {
    ServerHello(ServerHello),
    Pong,
    /// A server Exception. Terminal in the handshake and pre-query phase,
    /// which is all this crate implements: the connection is
    /// [`State::Failed`] with [`FailureKind::ServerException`] once this is
    /// queued.
    ///
    /// At v26.8.1.2041-lts the `TCPHandler::runImpl` handshake catch sends at
    /// most one Exception and then the connection closes
    /// (`TCPHandler::run`). Query-time exceptions are not terminal at that
    /// tag: `runImpl` sends the Exception and keeps the connection for the
    /// next query, apart from connection-level errors. They will be modeled
    /// when Query support lands, and will not move the connection to
    /// `Failed`.
    Exception(ServerException),
}

/// Why a connection reached [`State::Failed`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FailureKind {
    /// The server sent a complete Exception packet in the handshake or
    /// pre-query phase, after which the server closes the connection. Bytes
    /// after it are not parsed. See [`Event::Exception`] for why query-time
    /// exceptions will not produce this state.
    ServerException,
    /// Malformed, unexpected, oversized, or truncated input.
    Protocol,
}

/// The externally visible connection state.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum State {
    Handshaking,
    Ready,
    AwaitingPong,
    Failed(FailureKind),
    Closed,
}

/// A protocol or input error.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProtocolError {
    InvalidState {
        operation: &'static str,
        state: State,
    },
    StringTooLong {
        field: &'static str,
        /// The offending length, or `None` for the client `password` and
        /// `quota_key`, whose length is not disclosed.
        length: Option<usize>,
        limit: usize,
    },
    /// More than the retained-input limit is buffered after parsing.
    ///
    /// A backstop: the per-field caps keep retained input far below the
    /// 4 MiB production limit, so this is not expected to fire.
    InputTooLarge {
        length: usize,
        limit: usize,
    },
    InvalidVarUInt,
    InvalidUtf8 {
        field: &'static str,
    },
    UnexpectedPacket {
        packet: u64,
        state: State,
    },
    UnexpectedEof,
    /// Bytes followed a handshake-phase Exception packet, after which the
    /// server closes the connection. They are counted, not parsed.
    TrailingInput {
        length: usize,
    },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidState { operation, state } => {
                write!(formatter, "cannot {operation} while in state {state:?}")
            }
            Self::StringTooLong {
                field,
                length: Some(length),
                limit,
            } => write!(
                formatter,
                "{field} length {length} exceeds the {limit} byte limit"
            ),
            Self::StringTooLong {
                field,
                length: None,
                limit,
            } => write!(formatter, "{field} exceeds the {limit} byte limit"),
            Self::InputTooLarge { length, limit } => {
                write!(
                    formatter,
                    "input length {length} exceeds the {limit} byte limit"
                )
            }
            Self::InvalidVarUInt => formatter.write_str("invalid VarUInt"),
            Self::InvalidUtf8 { field } => write!(formatter, "{field} is not valid UTF-8"),
            Self::UnexpectedPacket { packet, state } => {
                write!(
                    formatter,
                    "unexpected server packet {packet} in state {state:?}"
                )
            }
            Self::UnexpectedEof => formatter.write_str("unexpected end of protocol input"),
            Self::TrailingInput { length } => {
                write!(formatter, "{length} bytes followed the server Exception")
            }
        }
    }
}

impl Error for ProtocolError {}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Incremental Client Hello, Server Hello, addendum, Ping, and Pong engine.
///
/// Events have exactly one channel: [`Self::feed`] reports only errors, and
/// every completed packet is queued for [`Self::poll_event`]. A `Vec` return
/// cannot carry events and an error at once, so a second drain API would be
/// needed for events completed before a malformed packet. One queue keeps
/// them in order in one place, and `feed` allocates nothing per call. Drain
/// `poll_event` after every `feed` and `finish`, including when they fail.
pub struct Connection {
    state: State,
    /// Bytes of an incomplete parse step that straddles `feed` calls. It
    /// always starts at a step boundary and is shorter than `need`.
    input: Vec<u8>,
    /// Minimum `input.len()` at which reparsing can make progress.
    need: usize,
    /// An Exception packet whose fields are being read.
    pending: Option<ExceptionProgress>,
    /// Bytes received after a terminal Exception, counted and dropped.
    trailing: usize,
    output: Vec<u8>,
    events: VecDeque<Event>,
    quota_key: String,
    /// The first `finish` outcome, returned again by later calls.
    finish_result: Option<Result<(), ProtocolError>>,
    /// Bound on `input.len()` after parsing. A field so tests can lower it.
    input_limit: usize,
    /// Number of `parse_step` calls, so tests can prove a step is not
    /// reparsed.
    #[cfg(test)]
    parse_steps: usize,
}

/// Redacts the queued output, which holds the plaintext password until it is
/// taken, and the quota key.
impl fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("state", &self.state)
            .field("buffered_input", &self.input.len())
            .field("need", &self.need)
            .field("exception_pending", &self.pending.is_some())
            .field("trailing", &self.trailing)
            .field(
                "output",
                &format_args!("<{} bytes redacted>", self.output.len()),
            )
            .field("queued_events", &self.events.len())
            .field("quota_key", &"<redacted>")
            .field("finish_result", &self.finish_result)
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Creates an engine and queues the Client Hello.
    ///
    /// ClickHouse `v26.8.1.2041-lts`
    /// `src/Server/TCPHandler.cpp::receiveHello` reads packet code 0, client
    /// name, major, minor, declared revision, database, user, and password.
    /// Strings are an unsigned LEB128 byte length followed by bytes and have a
    /// 64 KiB server-side limit. Packet code 0 is defined in
    /// `src/Core/Protocol.h`.
    ///
    /// The queued Client Hello holds the plaintext password until
    /// [`Self::take_output`] hands it to the driver. The output buffer is
    /// allocated at the Hello's exact size so that it does not reallocate
    /// while it holds the password, but nothing is zeroized: the engine
    /// makes no zeroization promise. Take the output before feeding the
    /// Server Hello, or the queued addendum may grow the buffer and leave a
    /// stale copy in freed memory.
    ///
    /// A too-long `password` or `quota_key` is reported as
    /// [`ProtocolError::StringTooLong`] with `length: None`.
    pub fn new(hello: ClientHello) -> Result<Self, ProtocolError> {
        validate_client_string("client_name", &hello.client_name, false)?;
        validate_client_string("database", &hello.database, false)?;
        validate_client_string("user", &hello.user, false)?;
        validate_client_string("password", &hello.password, true)?;
        validate_client_string("quota_key", &hello.quota_key, true)?;

        // Sized exactly, so the buffer never reallocates, leaving a stale
        // copy of the password in freed memory, while it holds the Hello.
        let hello_len = varuint_len(CLIENT_PACKET_HELLO)
            + string_len(&hello.client_name)
            + varuint_len(hello.version_major)
            + varuint_len(hello.version_minor)
            + varuint_len(ENGINE_REVISION)
            + string_len(&hello.database)
            + string_len(&hello.user)
            + string_len(&hello.password);
        let mut output = Vec::with_capacity(hello_len);
        put_varuint(&mut output, CLIENT_PACKET_HELLO);
        put_string(&mut output, &hello.client_name);
        put_varuint(&mut output, hello.version_major);
        put_varuint(&mut output, hello.version_minor);
        put_varuint(&mut output, ENGINE_REVISION);
        put_string(&mut output, &hello.database);
        put_string(&mut output, &hello.user);
        put_string(&mut output, &hello.password);
        debug_assert_eq!(output.len(), hello_len);

        Ok(Self {
            state: State::Handshaking,
            input: Vec::new(),
            need: 0,
            pending: None,
            trailing: 0,
            output,
            events: VecDeque::new(),
            quota_key: hello.quota_key,
            finish_result: None,
            input_limit: INPUT_LIMIT,
            #[cfg(test)]
            parse_steps: 0,
        })
    }

    /// Returns the current protocol state.
    pub fn state(&self) -> State {
        self.state
    }

    /// Takes all queued output bytes in protocol order.
    ///
    /// After a Server Hello this includes the addendum. A later [`Self::ping`]
    /// appends its packet, so a driver may take both in one ordered write.
    /// Until this is called after [`Self::new`], the engine holds the
    /// plaintext password in the queued Client Hello.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Returns the next completed event, oldest first.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Feeds an arbitrary received byte chunk.
    ///
    /// Completed packets are queued for [`Self::poll_event`], including the
    /// ones completed before an error in the same chunk. A malformed packet
    /// returns the error and moves the engine to
    /// `State::Failed(FailureKind::Protocol)`.
    ///
    /// Parsing stops at a server Exception, which is terminal in the
    /// handshake and pre-query phase this crate implements (see
    /// [`Event::Exception`]). Later bytes, in this chunk or in later calls,
    /// are counted without being parsed and `feed` keeps returning `Ok`, so
    /// the outcome never depends on where the stream was split.
    /// [`Self::finish`] reports them as [`ProtocolError::TrailingInput`].
    ///
    /// Bytes are parsed in place. Only the tail of a packet that straddles a
    /// chunk boundary is copied, and it is reparsed only once enough bytes
    /// arrived to complete the field that was short. The retained tail is
    /// bounded by the per-field caps, and `InputTooLarge` is a backstop.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), ProtocolError> {
        if self.finish_result.is_some() {
            return Err(ProtocolError::InvalidState {
                operation: "feed input after finish",
                state: self.state,
            });
        }
        if matches!(
            self.state,
            State::Failed(FailureKind::Protocol) | State::Closed
        ) {
            return Err(ProtocolError::InvalidState {
                operation: "feed input",
                state: self.state,
            });
        }
        let result = self.feed_inner(bytes);
        if result.is_err() {
            self.fail_protocol();
        }
        result
    }

    /// Marks transport EOF.
    ///
    /// EOF closes a `Ready` engine successfully. It is truncation while
    /// waiting for Server Hello or Pong, or whenever a packet is incomplete.
    /// After a server Exception it succeeds unless bytes followed the
    /// Exception. That Exception is terminal because this crate only
    /// implements the handshake and pre-query phase, where the server closes
    /// the connection after it; query-time exceptions are not terminal (see
    /// [`Event::Exception`]). The first outcome is stored and every later
    /// call returns it again.
    pub fn finish(&mut self) -> Result<(), ProtocolError> {
        if let Some(result) = &self.finish_result {
            return result.clone();
        }
        let result = match self.state {
            State::Failed(FailureKind::ServerException) if self.trailing == 0 => Ok(()),
            State::Failed(FailureKind::ServerException) => Err(ProtocolError::TrailingInput {
                length: self.trailing,
            }),
            State::Failed(FailureKind::Protocol) => Err(ProtocolError::InvalidState {
                operation: "finish after protocol failure",
                state: self.state,
            }),
            State::Closed => Ok(()),
            State::Ready if self.input.is_empty() && self.pending.is_none() => {
                self.state = State::Closed;
                Ok(())
            }
            State::Handshaking | State::Ready | State::AwaitingPong => {
                self.fail_protocol();
                Err(ProtocolError::UnexpectedEof)
            }
        };
        self.finish_result = Some(result.clone());
        result
    }

    /// Queues a Ping packet and waits for Pong.
    ///
    /// ClickHouse `v26.8.1.2041-lts` `src/Core/Protocol.h` defines Ping and
    /// Pong as packet code 4 with no body.
    ///
    /// Rejected with [`ProtocolError::InvalidState`] while any server packet
    /// is partly received, even though [`Self::state`] is still `Ready`. A
    /// partial Exception will end the connection, so the Pong would never
    /// arrive. Any other partial packet in `Ready` is unsolicited: if a Ping
    /// were sent now, a Pong whose packet code straddled the call would be
    /// accepted as the reply, while the same bytes in one chunk are an
    /// [`ProtocolError::UnexpectedPacket`].
    pub fn ping(&mut self) -> Result<(), ProtocolError> {
        if self.pending.is_some() || !self.input.is_empty() {
            return Err(ProtocolError::InvalidState {
                operation: "send Ping while a server packet is partly received",
                state: self.state,
            });
        }
        if self.state != State::Ready {
            return Err(ProtocolError::InvalidState {
                operation: "send Ping",
                state: self.state,
            });
        }
        put_varuint(&mut self.output, CLIENT_PACKET_PING);
        self.state = State::AwaitingPong;
        Ok(())
    }

    fn is_terminal(&self) -> bool {
        matches!(self.state, State::Failed(_) | State::Closed)
    }

    fn fail_protocol(&mut self) {
        self.state = State::Failed(FailureKind::Protocol);
        self.input = Vec::new();
        self.need = 0;
        self.pending = None;
    }

    fn feed_inner(&mut self, mut bytes: &[u8]) -> Result<(), ProtocolError> {
        if self.state == State::Failed(FailureKind::ServerException) {
            self.trailing = self.trailing.saturating_add(bytes.len());
            return Ok(());
        }

        // Complete a step that straddles the previous chunk boundary. Copy
        // only as many bytes as the parser asked for, so that the rest of a
        // large chunk is parsed in place below.
        while !self.input.is_empty() && !bytes.is_empty() {
            let wanted = self
                .need
                .saturating_sub(self.input.len())
                .max(1)
                .min(bytes.len());
            let (head, rest) = bytes.split_at(wanted);
            self.input.extend_from_slice(head);
            bytes = rest;
            if self.input.len() < self.need {
                continue;
            }
            let mut input = std::mem::take(&mut self.input);
            let consumed = self.parse_available(&input)?;
            if self.is_terminal() {
                self.trailing = (input.len() - consumed).saturating_add(bytes.len());
                return Ok(());
            }
            input.drain(..consumed);
            // A completed large field can leave a mostly empty buffer with up
            // to 1 MiB of capacity. Release it rather than holding it for the
            // life of the connection.
            if input.capacity() > INPUT_SHRINK_THRESHOLD && input.len() < INPUT_SHRINK_THRESHOLD / 4
            {
                input.shrink_to(INPUT_SHRINK_THRESHOLD / 4);
            }
            self.input = input;
        }

        if self.input.is_empty() && !bytes.is_empty() {
            let consumed = self.parse_available(bytes)?;
            // `parse_available` never reports more than it was given.
            let rest = &bytes[consumed..];
            if self.is_terminal() {
                self.trailing = rest.len();
                return Ok(());
            }
            self.input.extend_from_slice(rest);
        }

        if self.input.len() > self.input_limit {
            return Err(ProtocolError::InputTooLarge {
                length: self.input.len(),
                limit: self.input_limit,
            });
        }
        Ok(())
    }

    /// Parses complete steps from `bytes` and returns the bytes consumed.
    ///
    /// Stops at a terminal state, leaving the rest unconsumed, or when the
    /// next step needs more input, recording how much in `need`.
    fn parse_available(&mut self, bytes: &[u8]) -> Result<usize, ProtocolError> {
        let mut position = 0;
        while !self.is_terminal() {
            // `position` only advances by what a step consumed from the slice
            // it was given, so it never passes `bytes.len()`.
            match self.parse_step(&bytes[position..]) {
                Ok(consumed) => position += consumed,
                Err(ParseError::NeedMore(need)) => {
                    self.need = need;
                    break;
                }
                Err(ParseError::Protocol(error)) => return Err(error),
            }
        }
        Ok(position)
    }

    /// Parses one step: a whole Hello or Pong, an Exception packet code, or
    /// one Exception field. Returns the bytes consumed, at least one.
    fn parse_step(&mut self, bytes: &[u8]) -> ParseResult<usize> {
        #[cfg(test)]
        {
            self.parse_steps += 1;
        }
        if let Some(progress) = self.pending.as_mut() {
            let consumed = progress.step(bytes)?;
            if let Some(has_nested) = progress.has_nested {
                let exception = ServerException {
                    code: progress.code,
                    name: std::mem::take(&mut progress.name),
                    message: std::mem::take(&mut progress.message),
                    stack_trace: std::mem::take(&mut progress.stack_trace),
                    has_nested,
                    truncated: progress.truncated,
                };
                self.pending = None;
                self.state = State::Failed(FailureKind::ServerException);
                self.events.push_back(Event::Exception(exception));
            }
            return Ok(consumed);
        }

        let mut cursor = Cursor::new(bytes);
        let packet = cursor.varuint()?;
        match (self.state, packet) {
            (State::Handshaking, SERVER_PACKET_HELLO) => {
                let hello = parse_server_hello(&mut cursor)?;
                self.accept_server_hello(hello);
            }
            // An Exception is legal in `Ready` before any query. At
            // v26.8.1.2041-lts `TCPHandler.h` defaults `proto_send_chunked_cl`
            // and `proto_recv_chunked_cl` to "notchunked", and
            // `TCPHandler::receiveAddendum` reads the chunked fields only from
            // clients at revision 54470 or later, so a 54460 client is
            // assumed notchunked. `TCPHandler::runImpl` runs the chunked
            // compatibility check on every connection, whatever the revision,
            // right after the addendum and inside the handshake try-block. A
            // server whose `proto_caps` send or recv is strict "chunked" (not
            // "chunked_optional") throws NETWORK_ERROR "Incompatible
            // protocol", sent as an Exception after a complete Server Hello,
            // and closes the connection. `checkSettingsConstraints` rejecting
            // a non-empty default database fails the same way.
            (State::Handshaking | State::Ready | State::AwaitingPong, SERVER_PACKET_EXCEPTION) => {
                self.pending = Some(ExceptionProgress::default());
            }
            (State::AwaitingPong, SERVER_PACKET_PONG) => {
                self.state = State::Ready;
                self.events.push_back(Event::Pong);
            }
            (state, packet) => {
                return Err(ParseError::Protocol(ProtocolError::UnexpectedPacket {
                    packet,
                    state,
                }))
            }
        }
        Ok(cursor.position())
    }

    fn accept_server_hello(&mut self, hello: ServerHello) {
        if hello.effective_revision >= MIN_PROTOCOL_VERSION_WITH_ADDENDUM {
            // At v26.8.1.2041-lts `src/Client/Connection.cpp::sendAddendum`
            // writes no packet code. At this engine revision only the
            // quota-key string is present.
            if hello.effective_revision >= MIN_PROTOCOL_VERSION_WITH_QUOTA_KEY {
                put_string(&mut self.output, &self.quota_key);
            }
        }
        self.state = State::Ready;
        self.events.push_back(Event::ServerHello(hello));
    }
}

/// ClickHouse `v26.8.1.2041-lts`
/// `src/Server/TCPHandler.cpp::sendHello` and
/// `src/Client/Connection.cpp::receiveHello` define these fields and
/// gates. Packet codes and thresholds are in `src/Core/Protocol.h` and
/// `src/Core/ProtocolDefines.h`.
///
/// The whole packet is reparsed if it is incomplete. Its strings are capped
/// at 4096 bytes, so a reparse is cheap, and nothing is allocated until the
/// packet is complete.
fn parse_server_hello(cursor: &mut Cursor<'_>) -> ParseResult<ServerHello> {
    let server_name = cursor.string_bytes("server_name", SERVER_HELLO_STRING_LIMIT)?;
    let version_major = cursor.varuint()?;
    let version_minor = cursor.varuint()?;
    let revision = cursor.varuint()?;

    // INFERRED: the server gates its output on the client-declared revision,
    // while stock clients gate reads on the raw server revision. It follows
    // that a client advertising a lower ceiling must use this effective
    // revision to avoid reading fields it did not request.
    let effective_revision = ENGINE_REVISION.min(revision);
    let timezone = if effective_revision >= MIN_REVISION_WITH_SERVER_TIMEZONE {
        cursor.string_bytes("timezone", SERVER_HELLO_STRING_LIMIT)?
    } else {
        &[]
    };
    let display_name = if effective_revision >= MIN_REVISION_WITH_SERVER_DISPLAY_NAME {
        cursor.string_bytes("display_name", SERVER_HELLO_STRING_LIMIT)?
    } else {
        &[]
    };
    let version_patch = if effective_revision >= MIN_REVISION_WITH_VERSION_PATCH {
        cursor.varuint()?
    } else {
        revision
    };

    Ok(ServerHello {
        server_name: strict_string("server_name", server_name)?,
        version_major,
        version_minor,
        revision,
        effective_revision,
        timezone: strict_string("timezone", timezone)?,
        display_name: strict_string("display_name", display_name)?,
        version_patch,
    })
}

// ---------------------------------------------------------------------------
// Exception packet
// ---------------------------------------------------------------------------

/// ClickHouse `v26.8.1.2041-lts` `src/IO/WriteHelpers.cpp::writeException`
/// writes a fixed Int32 LE code, name, message, and stack trace as VarUInt
/// length-prefixed strings, and a one-byte nested flag that is always false.
/// `src/IO/ReadHelpers.cpp::readException` reads nothing after the flag.
///
/// Fields are committed one at a time, so a large Exception is never
/// reparsed from its start. A field longer than the cap keeps its first
/// `EXCEPTION_STRING_LIMIT` bytes and the rest is skipped as it arrives,
/// without buffering, so the code survives an oversized message. A declared
/// length above `EXCEPTION_DECLARED_LIMIT` is a protocol error, so the skip
/// always ends.
#[derive(Default)]
struct ExceptionProgress {
    next: ExceptionField,
    code: i32,
    name: String,
    message: String,
    stack_trace: String,
    truncated: bool,
    /// Bytes of an oversized field still to discard.
    skip: u64,
    /// Set by the final step. The packet is complete once this is `Some`.
    has_nested: Option<bool>,
}

#[derive(Clone, Copy, Default)]
enum ExceptionField {
    #[default]
    Code,
    Name,
    Message,
    StackTrace,
    HasNested,
}

impl ExceptionProgress {
    fn step(&mut self, bytes: &[u8]) -> ParseResult<usize> {
        if self.skip > 0 {
            if bytes.is_empty() {
                return Err(ParseError::NeedMore(1));
            }
            let skipped =
                usize::try_from(self.skip).map_or(bytes.len(), |skip| skip.min(bytes.len()));
            self.skip -= skipped as u64;
            return Ok(skipped);
        }

        let mut cursor = Cursor::new(bytes);
        match self.next {
            ExceptionField::Code => {
                self.code = cursor.i32_le()?;
                self.next = ExceptionField::Name;
            }
            ExceptionField::Name => {
                self.name = self.capped_string(&mut cursor, "exception_name")?;
                self.next = ExceptionField::Message;
            }
            ExceptionField::Message => {
                self.message = self.capped_string(&mut cursor, "exception_message")?;
                self.next = ExceptionField::StackTrace;
            }
            ExceptionField::StackTrace => {
                self.stack_trace = self.capped_string(&mut cursor, "exception_stack_trace")?;
                self.next = ExceptionField::HasNested;
            }
            ExceptionField::HasNested => {
                self.has_nested = Some(cursor.byte()? != 0);
            }
        }
        Ok(cursor.position())
    }

    fn capped_string(
        &mut self,
        cursor: &mut Cursor<'_>,
        field: &'static str,
    ) -> ParseResult<String> {
        let length = cursor.varuint()?;
        // Checked before any byte of the field is taken, so an oversized
        // declaration fails as soon as its VarUInt is complete.
        if length > EXCEPTION_DECLARED_LIMIT as u64 {
            return Err(ParseError::Protocol(ProtocolError::StringTooLong {
                field,
                length: Some(usize::try_from(length).unwrap_or(usize::MAX)),
                limit: EXCEPTION_DECLARED_LIMIT,
            }));
        }
        let kept = usize::try_from(length).map_or(EXCEPTION_STRING_LIMIT, |length| {
            length.min(EXCEPTION_STRING_LIMIT)
        });
        let bytes = cursor.take(kept)?;
        self.skip = length - kept as u64;
        self.truncated |= self.skip > 0;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// Checks a Client Hello or addendum string against the server limit. The
/// length of a `secret` is left out of the error.
fn validate_client_string(
    field: &'static str,
    value: &str,
    secret: bool,
) -> Result<(), ProtocolError> {
    if value.len() > CLIENT_HELLO_STRING_LIMIT {
        return Err(ProtocolError::StringTooLong {
            field,
            length: (!secret).then_some(value.len()),
            limit: CLIENT_HELLO_STRING_LIMIT,
        });
    }
    Ok(())
}

/// Encoded size of `value` as an unsigned LEB128 VarUInt.
fn varuint_len(value: u64) -> usize {
    // One byte per started group of 7 significant bits, at least one.
    let bits = (u64::BITS - value.leading_zeros()).max(1);
    bits.div_ceil(7) as usize
}

/// Encoded size of a VarUInt length-prefixed string.
fn string_len(value: &str) -> usize {
    varuint_len(value.len() as u64) + value.len()
}

fn put_varuint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn put_string(output: &mut Vec<u8>, value: &str) {
    put_varuint(output, value.len() as u64);
    output.extend_from_slice(value.as_bytes());
}

fn strict_string(field: &'static str, bytes: &[u8]) -> Result<String, ParseError> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| ParseError::Protocol(ProtocolError::InvalidUtf8 { field }))
}

type ParseResult<T> = Result<T, ParseError>;

enum ParseError {
    /// The step needs a slice of at least this many bytes, counted from the
    /// start of the step. Always greater than the length it was given.
    NeedMore(usize),
    Protocol(ProtocolError),
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn position(&self) -> usize {
        self.position
    }

    fn byte(&mut self) -> ParseResult<u8> {
        let byte = *self
            .bytes
            .get(self.position)
            .ok_or(ParseError::NeedMore(self.position + 1))?;
        self.position += 1;
        Ok(byte)
    }

    /// Reads an unsigned LEB128 value of at most 10 bytes.
    ///
    /// This is deliberately stricter than the server's `readVarUInt`: an
    /// encoding longer than 10 bytes, or a 10th byte above `0x01` that would
    /// overflow `u64`, is rejected as [`ProtocolError::InvalidVarUInt`]
    /// rather than truncated.
    fn varuint(&mut self) -> ParseResult<u64> {
        let mut value = 0_u64;
        for shift in (0..=63).step_by(7) {
            let byte = self.byte()?;
            if shift == 63 && byte > 1 {
                return Err(ParseError::Protocol(ProtocolError::InvalidVarUInt));
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(ParseError::Protocol(ProtocolError::InvalidVarUInt))
    }

    fn i32_le(&mut self) -> ParseResult<i32> {
        let bytes = self.take(4)?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn string_bytes(&mut self, field: &'static str, limit: usize) -> ParseResult<&'a [u8]> {
        let length = self.varuint()?;
        let length = usize::try_from(length).unwrap_or(usize::MAX);
        if length > limit {
            return Err(ParseError::Protocol(ProtocolError::StringTooLong {
                field,
                length: Some(length),
                limit,
            }));
        }
        self.take(length)
    }

    /// Takes `length` bytes.
    ///
    /// Every caller passes a small, capped `length`: at most 4096 for a
    /// Server Hello string (`SERVER_HELLO_STRING_LIMIT`), at most 1 MiB for
    /// the kept bytes of an Exception field (`EXCEPTION_STRING_LIMIT`), and 4
    /// for the Int32 Exception code. With `position <= bytes.len()` the end
    /// offset therefore cannot overflow. A caller passing an uncapped
    /// wire length would break that assumption, and also the retained-input
    /// bound that relies on the caps. If the end did overflow, the saturated end is reported as `NeedMore`: the
    /// engine then buffers input against its own adjustable `input_limit`,
    /// failing with `InputTooLarge` or at EOF, rather than this cursor
    /// reporting a limit it does not know.
    fn take(&mut self, length: usize) -> ParseResult<&'a [u8]> {
        let end = self.position.saturating_add(length);
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(ParseError::NeedMore(end))?;
        self.position = end;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests;
