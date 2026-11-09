#![doc = include_str!("../README.md")]

use std::collections::{HashSet, VecDeque};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use ch_core_rs::native::decode::{DecodeError, DecodeOptions};
use ch_core_rs::native::stream_decoder::{BlockBoundary, BlockScanner};

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
// At v26.8.1.2041-lts `TCPHandler::processInsertQuery` can send Progress
// during an async INSERT upload starting at revision 54484. If the engine
// ceiling reaches that revision, Inserting and FinishingInsert must accept it.
const _: () = assert!(
    ENGINE_REVISION < 54_484,
    "raising ENGINE_REVISION requires accepting Progress during INSERT upload"
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
/// Largest declared TableColumns field length accepted, as a multiple of the
/// connection's `max_block_bytes` (saturating). Engine policy, the same
/// corruption-only bound as `EXCEPTION_DECLARED_LIMIT`: at v26.8.1.2041-lts
/// `TCPHandler::sendTableColumns` writes two unbounded `writeStringBinary`
/// fields, so without this a corrupt length such as `u64::MAX` would discard
/// every later byte until EOF instead of failing. Lengths within the bound
/// keep at most `max_block_bytes` across both fields and skip the rest; a
/// length above it fails the connection with
/// [`ProtocolError::StringTooLong`].
const TABLE_COLUMNS_DECLARED_FACTOR: usize = 64;
/// Backstop on retained input. `feed` parses everything it is given, so the
/// only input it retains is the tail of one incomplete scalar parse step: at
/// most one Exception text field (1 MiB kept plus its length prefix), one
/// Server Hello, or one block table name. Native block bodies are copied into
/// their own buffer and never retained here. The per-field caps keep this
/// from firing; it guards against a future field that forgets its cap.
const INPUT_LIMIT: usize = 4 * 1024 * 1024;
/// Input buffer capacity above which a mostly drained buffer is shrunk.
const INPUT_SHRINK_THRESHOLD: usize = 64 * 1024;
/// Default for [`Connection::with_max_block_bytes`]: 1 GiB.
pub const DEFAULT_MAX_BLOCK_BYTES: usize = 1024 * 1024 * 1024;
/// Smallest block cap [`Connection::with_max_block_bytes`] accepts: 1 KiB.
/// Smaller values are raised to it, so the cap can never reject the TCP
/// end-of-data marker or a small schema block.
pub const MIN_MAX_BLOCK_BYTES: usize = 1024;
const QUERY_OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
/// Longest setting name `Query::with_setting` accepts. Engine policy, not a
/// server limit: the server reads names up to 1 GiB. This independently bounds
/// user-controlled identifiers without imposing the same per-field cap on
/// setting values. Real setting names are far below this.
const SETTING_NAME_LIMIT: usize = 64 * 1024;

const CLIENT_PACKET_HELLO: u64 = 0;
const CLIENT_PACKET_QUERY: u64 = 1;
const CLIENT_PACKET_DATA: u64 = 2;
const CLIENT_PACKET_PING: u64 = 4;
const SERVER_PACKET_HELLO: u64 = 0;
const SERVER_PACKET_DATA: u64 = 1;
const SERVER_PACKET_EXCEPTION: u64 = 2;
const SERVER_PACKET_PROGRESS: u64 = 3;
const SERVER_PACKET_PONG: u64 = 4;
const SERVER_PACKET_END_OF_STREAM: u64 = 5;
const SERVER_PACKET_PROFILE_INFO: u64 = 6;
const SERVER_PACKET_TOTALS: u64 = 7;
const SERVER_PACKET_EXTREMES: u64 = 8;
const SERVER_PACKET_LOG: u64 = 10;
const SERVER_PACKET_TABLE_COLUMNS: u64 = 11;
const SERVER_PACKET_PROFILE_EVENTS: u64 = 14;

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

/// An uncompressed initial query for SELECT or client-fed INSERT.
///
/// Settings use the server's string representation. Parameter values are
/// quoted as ClickHouse string literals on the wire.
#[derive(Clone)]
pub struct Query {
    sql: String,
    query_id: String,
    initial_address: String,
    initial_query_start_time_microseconds: u64,
    os_user: String,
    hostname: String,
    settings: Vec<(String, String)>,
    parameters: Vec<(String, String)>,
}

impl Query {
    /// Creates a query request. Use [`Connection::query`] for a SELECT result
    /// exchange or [`Connection::insert`] for a client-fed INSERT.
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            query_id: String::new(),
            initial_address: "0.0.0.0:0".to_owned(),
            initial_query_start_time_microseconds: 0,
            os_user: String::new(),
            hostname: String::new(),
            settings: Vec::new(),
            parameters: Vec::new(),
        }
    }

    /// Sets the query ID. Empty lets the server choose one.
    pub fn with_query_id(mut self, query_id: impl Into<String>) -> Self {
        self.query_id = query_id.into();
        self
    }

    /// Sets the initial client address serialized in `ClientInfo`.
    ///
    /// The default is `0.0.0.0:0`. For this engine's initial queries, the
    /// server overwrites the field with the connection's peer address before
    /// logging it as `initial_address` in `system.query_log`.
    pub fn with_initial_address(mut self, address: impl Into<String>) -> Self {
        self.initial_address = address.into();
        self
    }

    /// Sets the initial query start time in microseconds since the Unix epoch.
    pub fn with_initial_query_start_time_microseconds(mut self, time: u64) -> Self {
        self.initial_query_start_time_microseconds = time;
        self
    }

    /// Sets the operating-system user reported in `ClientInfo`.
    pub fn with_os_user(mut self, os_user: impl Into<String>) -> Self {
        self.os_user = os_user.into();
        self
    }

    /// Sets the hostname reported in `ClientInfo`.
    pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostname = hostname.into();
        self
    }

    /// Adds a query setting using its ClickHouse string representation.
    ///
    /// Every explicit setting is sent with the `IMPORTANT` flag. An invalid
    /// setting name or value can therefore produce a first-packet Exception
    /// before the server creates a query context, after which it closes the
    /// connection. A binding that forwards a user's settings map should
    /// validate names and values where practical.
    pub fn with_setting(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.settings.push((name.into(), value.into()));
        self
    }

    /// Adds a query parameter for a `{name:Type}` substitution.
    ///
    /// `value` is exactly the text you would send as the HTTP `param_<name>`
    /// value: the server's escaped text form for the declared type, not a SQL
    /// literal and not raw bytes. The engine adds the one quoting layer the
    /// TCP transport needs, so the server sees the same text HTTP delivers.
    ///
    /// | Declared type          | `value` text           | Server value         |
    /// |------------------------|------------------------|----------------------|
    /// | `String`               | `a\tb`                 | `a`, tab, `b`        |
    /// | `String`               | `a\\b`                 | `a\b`                |
    /// | `String`               | `it's` or `it\'s`      | `it's`               |
    /// | `Nullable(String)`     | `\N`                   | NULL                 |
    /// | `Nullable(Int32)`      | `13`                   | 13                   |
    /// | `Array(String)`        | `['user_1','user_2']`  | two elements         |
    /// | `Tuple(String, Int32)` | `('user_1',13)`        | tuple                |
    /// | `Map(String, UInt8)`   | `{'k1':13,'k2':79}`    | two entries          |
    /// | `Date`                 | `2026-11-06`           | date                 |
    ///
    /// The cells above are the text itself, so the Rust literal for the first
    /// row is `"a\\tb"`. A literal tab or newline byte in a `String` value is
    /// rejected by the server with `BAD_QUERY_PARAMETER`, as over HTTP, and
    /// `\N` for a non-`Nullable` `String` is the empty string.
    ///
    /// Server behavior at v26.8.1.2041-lts: `TCPHandler::processQuery` reads
    /// parameters with `readQueryParameters` (`src/Core/Settings.cpp`), which
    /// undoes one `readQuoted` layer per value, the inverse of
    /// `writeQueryParameters`. HTTP `param_` values go unchanged into the
    /// same `Context` parameter map (`DynamicQueryHandler::customizeQueryParam`
    /// in `src/Server/HTTPHandler.cpp`). Both are then parsed by
    /// `ReplaceQueryParameterVisitor::resolveParameterValueAsField` with
    /// `deserializeTextEscaped`, which must consume the whole text.
    ///
    /// Each name may be given once. [`Connection::query`] rejects a repeated
    /// name with [`ProtocolError::DuplicateName`], because TCP would keep the
    /// last value where HTTP keeps the first.
    pub fn with_parameter(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.parameters.push((name.into(), value.into()));
        self
    }
}

impl fmt::Debug for Query {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Query")
            .field("sql", &self.sql)
            .field("query_id", &self.query_id)
            .field("initial_address", &self.initial_address)
            .field(
                "initial_query_start_time_microseconds",
                &self.initial_query_start_time_microseconds,
            )
            .field("os_user", &self.os_user)
            .field("hostname", &self.hostname)
            .field("settings", &self.settings)
            .field("parameters", &"<redacted>")
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
    /// The server timezone, the TCP counterpart of HTTP's
    /// `X-ClickHouse-Timezone` header. Use it for `DateTime` columns that
    /// declare no timezone.
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

/// Query progress since the previous Progress packet.
///
/// Every field is an increment, not a running total: at v26.8.1.2041-lts
/// `TCPHandler::sendProgress` writes
/// `progress.fetchValuesAndResetPiecewiseAtomically()` and the elapsed time
/// since the previous packet. Sum the packets for totals.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Progress {
    /// Rows read since the previous packet.
    pub read_rows: u64,
    /// Uncompressed bytes read since the previous packet.
    pub read_bytes: u64,
    /// Increase in the estimated total rows to read.
    pub total_rows_to_read: u64,
    /// Rows written since the previous packet.
    pub written_rows: u64,
    /// Bytes written since the previous packet.
    pub written_bytes: u64,
    /// Server time elapsed since the previous packet, in nanoseconds.
    pub elapsed_ns: u64,
}

/// Final query profile counters.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ProfileInfo {
    /// Rows in the result.
    pub rows: u64,
    /// Blocks in the result.
    pub blocks: u64,
    /// Bytes in the result.
    pub bytes: u64,
    /// Whether a row limit was applied.
    pub applied_limit: bool,
    /// Rows before the applied limit.
    pub rows_before_limit: u64,
    /// Obsolete server field, retained to preserve the six-field wire packet.
    pub unused_obsolete_field: bool,
}

/// A complete protocol event, read with [`Connection::poll_event`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Event {
    /// The negotiated server identity and revision. Boxed, like
    /// `Exception`, because each is sent once while `Event` is queued per
    /// packet; see the size note on [`Event`].
    ServerHello(Box<ServerHello>),
    /// Reply to a client Ping.
    Pong,
    /// One complete Native block from a server Data packet. Concatenating
    /// these payloads in event order produces the query's Native stream.
    Data(Vec<u8>),
    /// The zero-row Native header block the server requests for an INSERT.
    ///
    /// This is separate from [`Event::Data`] so a driver cannot accidentally
    /// treat the INSERT sample block as SELECT result data. After this event,
    /// [`Connection::insert_data`] accepts client blocks.
    InsertSchema(Vec<u8>),
    /// Textual INSERT column metadata sent before [`Event::InsertSchema`]
    /// when omitted-field defaults are enabled.
    #[non_exhaustive]
    TableColumns {
        /// External-table name. Empty for the main INSERT table.
        table_name: Vec<u8>,
        /// Raw `ColumnsDescription::toString(false)` bytes from the server.
        description: Vec<u8>,
        /// The two fields together exceeded
        /// [`Connection::with_max_block_bytes`] and were cut to share that
        /// many bytes: `table_name` keeps up to the limit and `description`
        /// keeps the rest of the budget.
        truncated: bool,
    },
    /// Incremental query progress counters.
    Progress(Progress),
    /// Final query profile counters.
    ProfileInfo(ProfileInfo),
    /// A complete Native block from a ProfileEvents packet.
    ProfileEvents(Vec<u8>),
    /// A complete Native block from a server Log packet.
    Log(Vec<u8>),
    /// A complete Native block containing query totals.
    Totals(Vec<u8>),
    /// A complete Native block containing query extremes.
    Extremes(Vec<u8>),
    /// Successful end of the query response.
    EndOfStream,
    /// A server Exception. Terminal in the handshake and pre-query phase and
    /// for the connection-level query cases below, where the connection
    /// becomes [`State::Failed`] with [`FailureKind::ServerException`]. Other
    /// query errors return to [`State::Ready`] after this event is queued.
    /// Every exchange started with [`Connection::insert`] conservatively
    /// fails on Exception and discards queued upload bytes, because the server
    /// may be waiting for the rest of an upload in its skip-data path.
    /// Either way it is the last event of its exchange: no EndOfStream, Log,
    /// or ProfileEvents follows it.
    ///
    /// Server behavior at v26.8.1.2041-lts, `TCPHandler::runImpl`:
    ///
    /// - The handshake catch sends at most one Exception and then the
    ///   connection closes (`TCPHandler::run`).
    /// - `ClientInfo` and wire settings are read before the query context is
    ///   created. If a query that sent settings receives an Exception as its
    ///   first server packet, the engine fails closed because invalid known
    ///   values and unknown names both close the server connection at that
    ///   point. An Exception after any earlier query packet is reusable unless
    ///   its code is independently terminal. Without wire settings, ordinary
    ///   first-packet SQL errors such as code 62 remain reusable.
    /// - This fail-closed rule deliberately classifies a first-packet syntax
    ///   error as terminal when the query also sent wire settings, even though
    ///   the server can keep that socket open. The engine cannot distinguish
    ///   that case from a settings parse failure. A driver must close its
    ///   transport when it observes [`State::Failed`]; it must not wait for the
    ///   server to close first. Safely constructed SQL `SETTINGS` clauses do
    ///   not set the wire-settings flag and avoid this conservative result.
    /// - Codes 101, 702, 720, and 749 (UNEXPECTED_PACKET_FROM_CLIENT,
    ///   CLIENT_INFO_DOES_NOT_MATCH, USER_EXPIRED,
    ///   TCP_CONNECTION_LIMIT_REACHED) are sent and then the connection closes.
    /// - Code 99 (UNKNOWN_PACKET_FROM_CLIENT) closes without sending at that
    ///   tag; it is listed so a relayed one cannot leave the engine `Ready`.
    /// - Once the query context exists, other query errors normally leave the
    ///   connection reusable. The wire does not identify every server throw
    ///   site, so drivers must still honor transport EOF and I/O failures.
    ///
    /// Boxed to keep [`Event`] small.
    Exception(Box<ServerException>),
}

// Every queued packet costs one `Event` in the queue plus its own payload
// allocation. Keep the enum small: Progress, the largest unboxed variant, is
// six `u64`s.
const _: () = assert!(
    std::mem::size_of::<Event>() <= 56,
    "Event grew; box the large variant"
);

/// Why a connection reached [`State::Failed`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FailureKind {
    /// The server sent a complete Exception packet in the handshake or
    /// pre-query phase, a connection-level query Exception, or any INSERT
    /// Exception. The server closes in the first cases. INSERT is treated as
    /// terminal because safe reuse would require finishing or draining its
    /// skip-data exchange. Bytes after the Exception are not parsed. See
    /// [`Event::Exception`] for details.
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
    Querying,
    /// An INSERT Query was sent and the engine is waiting for the server's
    /// sample block.
    AwaitingInsertSchema,
    /// The server's INSERT sample block was received and data may be sent.
    Inserting,
    /// The client end-of-data marker was queued and the engine is waiting for
    /// EndOfStream.
    FinishingInsert,
    AwaitingPong,
    Failed(FailureKind),
    Closed,
}

/// A protocol or input error.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ProtocolError {
    /// The requested operation is illegal in the current connection state.
    InvalidState {
        /// Operation that was rejected.
        operation: &'static str,
        /// State in which it was attempted.
        state: State,
    },
    /// A length-prefixed string exceeded its field-specific limit.
    StringTooLong {
        /// Protocol field name.
        field: &'static str,
        /// The offending length, or `None` for the client `password` and
        /// `quota_key`, whose length is not disclosed.
        length: Option<usize>,
        /// Maximum accepted wire length.
        limit: usize,
    },
    /// More than the retained-input limit is buffered after parsing.
    ///
    /// A backstop: the per-field caps keep retained input far below the
    /// 4 MiB production limit, so this is not expected to fire.
    InputTooLarge {
        /// Bytes that would have been retained.
        length: usize,
        /// Maximum retained input.
        limit: usize,
    },
    /// A VarUInt was longer than ten bytes or overflowed `u64`.
    InvalidVarUInt,
    /// A protocol field required strict UTF-8.
    InvalidUtf8 {
        /// Protocol field name.
        field: &'static str,
    },
    /// A setting or parameter name was empty.
    EmptyName {
        /// Protocol field name.
        field: &'static str,
    },
    /// A query parameter name was given more than once.
    ///
    /// Rejected because the transports disagree: at v26.8.1.2041-lts TCP
    /// keeps the last value (`readQueryParameters`) and HTTP `param_` keeps
    /// the first (`DynamicQueryHandler::customizeQueryParam`).
    DuplicateName {
        /// Protocol field name.
        field: &'static str,
        /// The repeated name.
        name: String,
    },
    /// The packet code is not accepted in the current state.
    UnexpectedPacket {
        /// Server packet code.
        packet: u64,
        /// State in which it arrived.
        state: State,
    },
    /// Transport EOF or a truncated field ended an incomplete exchange.
    UnexpectedEof,
    /// The negotiated revision cannot encode this engine's Query packet.
    UnsupportedQueryRevision {
        /// Negotiated effective revision.
        revision: u64,
    },
    /// The encoded Query packet exceeded the output limit.
    QueryOutputTooLarge {
        /// Encoded or conservatively bounded length.
        length: usize,
        /// Maximum Query packet length.
        limit: usize,
    },
    /// Bytes passed to [`Connection::insert_data`] were rejected: too large,
    /// or not exactly one complete Native block with at least one column.
    /// The connection is unchanged. The reason is the inner error, also
    /// returned by [`Error::source`].
    InvalidInsertBlock(InsertBlockError),
    /// The server's INSERT sample block was not a zero-row schema with at
    /// least one column.
    InvalidInsertSchema,
    /// The setting switches Native type names to binary framing, which this
    /// engine cannot delimit.
    ///
    /// The check is best-effort: it sees only settings passed through
    /// [`Query::with_setting`]. A SQL `SETTINGS` clause, a user profile, or
    /// an earlier `SET` can still enable
    /// `output_format_native_encode_types_in_binary_format`, and the engine
    /// then fails with [`ProtocolError::Decode`] on the first block. Keep the
    /// setting disabled server-side.
    BinaryNativeTypeEncodingUnsupported,
    /// An inbound Native block exceeded the per-packet limit. Always fatal:
    /// the connection fails with [`FailureKind::Protocol`].
    ///
    /// An oversized outbound [`Connection::insert_data`] block is reported
    /// as [`InsertBlockError::TooLarge`] instead, so this variant never
    /// describes a recoverable rejection.
    NativeBlockTooLarge {
        /// Maximum Native block length.
        limit: usize,
    },
    /// Reserving the complete outbound INSERT Data packet failed.
    #[non_exhaustive]
    InsertOutputAllocationFailed {
        /// Bytes the packet would append to the output queue.
        additional: usize,
    },
    /// Core rejected malformed Native block bytes, or a column type it cannot
    /// decode. The [`DecodeError`] is returned by [`Error::source`].
    Decode(Arc<DecodeError>),
    /// Core's scanner reported a block end inside bytes it had already
    /// reported as an incomplete block. An internal invariant; not expected.
    InconsistentBlockBoundary {
        /// Reported block length.
        consumed: usize,
        /// Bytes previously scanned as incomplete.
        buffered: usize,
    },
    /// Bytes followed a terminal Exception packet. They are counted, not
    /// parsed. This applies to handshake, pre-query, and terminal query
    /// Exceptions.
    TrailingInput {
        /// Bytes received after the terminal Exception.
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
            Self::EmptyName { field } => write!(formatter, "{field} must not be empty"),
            Self::DuplicateName { field, name } => {
                write!(formatter, "{field} {name:?} is given more than once")
            }
            Self::UnexpectedPacket { packet, state } => {
                write!(
                    formatter,
                    "unexpected server packet {packet} in state {state:?}"
                )
            }
            Self::UnexpectedEof => formatter.write_str("unexpected end of protocol input"),
            Self::UnsupportedQueryRevision { revision } => write!(
                formatter,
                "Query requires protocol revision {ENGINE_REVISION}, effective revision is {revision}"
            ),
            Self::QueryOutputTooLarge { length, limit } => {
                write!(formatter, "query packet length {length} exceeds the {limit} byte limit")
            }
            // The cause is reported through `source`, not repeated here.
            Self::InvalidInsertBlock(_) => formatter.write_str("invalid INSERT Native block"),
            Self::InvalidInsertSchema => {
                formatter.write_str("invalid INSERT schema block")
            }
            Self::BinaryNativeTypeEncodingUnsupported => formatter.write_str(
                "output_format_native_encode_types_in_binary_format accepts only \"0\"",
            ),
            Self::NativeBlockTooLarge { limit } => {
                write!(formatter, "Native block exceeds the {limit} byte limit")
            }
            Self::InsertOutputAllocationFailed { additional } => write!(
                formatter,
                "cannot reserve {additional} bytes for the INSERT Data packet"
            ),
            // The DecodeError is reported through `source`, not repeated here.
            Self::Decode(_) => formatter.write_str("Native block decode failed"),
            Self::InconsistentBlockBoundary { consumed, buffered } => write!(
                formatter,
                "Native block boundary {consumed} is inside the {buffered} bytes already scanned"
            ),
            Self::TrailingInput { length } => {
                write!(formatter, "{length} bytes followed the server Exception")
            }
        }
    }
}

impl PartialEq for ProtocolError {
    fn eq(&self, other: &Self) -> bool {
        use ProtocolError::*;
        match (self, other) {
            (
                InvalidState {
                    operation: left_operation,
                    state: left_state,
                },
                InvalidState {
                    operation: right_operation,
                    state: right_state,
                },
            ) => left_operation == right_operation && left_state == right_state,
            (
                StringTooLong {
                    field: left_field,
                    length: left_length,
                    limit: left_limit,
                },
                StringTooLong {
                    field: right_field,
                    length: right_length,
                    limit: right_limit,
                },
            ) => {
                left_field == right_field
                    && left_length == right_length
                    && left_limit == right_limit
            }
            (
                InputTooLarge {
                    length: left_length,
                    limit: left_limit,
                },
                InputTooLarge {
                    length: right_length,
                    limit: right_limit,
                },
            )
            | (
                QueryOutputTooLarge {
                    length: left_length,
                    limit: left_limit,
                },
                QueryOutputTooLarge {
                    length: right_length,
                    limit: right_limit,
                },
            ) => left_length == right_length && left_limit == right_limit,
            (InvalidVarUInt, InvalidVarUInt) | (UnexpectedEof, UnexpectedEof) => true,
            (InvalidUtf8 { field: left }, InvalidUtf8 { field: right }) => left == right,
            (EmptyName { field: left }, EmptyName { field: right }) => left == right,
            (
                DuplicateName {
                    field: left_field,
                    name: left_name,
                },
                DuplicateName {
                    field: right_field,
                    name: right_name,
                },
            ) => left_field == right_field && left_name == right_name,
            (
                UnexpectedPacket {
                    packet: left_packet,
                    state: left_state,
                },
                UnexpectedPacket {
                    packet: right_packet,
                    state: right_state,
                },
            ) => left_packet == right_packet && left_state == right_state,
            (
                UnsupportedQueryRevision { revision: left },
                UnsupportedQueryRevision { revision: right },
            ) => left == right,
            (InvalidInsertBlock(left), InvalidInsertBlock(right)) => left == right,
            (InvalidInsertSchema, InvalidInsertSchema) => true,
            (BinaryNativeTypeEncodingUnsupported, BinaryNativeTypeEncodingUnsupported) => true,
            (NativeBlockTooLarge { limit: left }, NativeBlockTooLarge { limit: right }) => {
                left == right
            }
            (
                InsertOutputAllocationFailed { additional: left },
                InsertOutputAllocationFailed { additional: right },
            ) => left == right,
            (Decode(left), Decode(right)) => left.to_string() == right.to_string(),
            (
                InconsistentBlockBoundary {
                    consumed: left_consumed,
                    buffered: left_buffered,
                },
                InconsistentBlockBoundary {
                    consumed: right_consumed,
                    buffered: right_buffered,
                },
            ) => left_consumed == right_consumed && left_buffered == right_buffered,
            (TrailingInput { length: left }, TrailingInput { length: right }) => left == right,
            _ => false,
        }
    }
}

impl Eq for ProtocolError {}

impl Error for ProtocolError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error.as_ref()),
            Self::InvalidInsertBlock(error) => Some(error),
            _ => None,
        }
    }
}

impl From<DecodeError> for ProtocolError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(Arc::new(error))
    }
}

/// Why an outbound Native block was rejected before any bytes were queued.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum InsertBlockError {
    /// The Native block scanner rejected malformed bytes.
    Decode(Arc<DecodeError>),
    /// The payload ended before one complete block.
    Truncated,
    /// Bytes followed the first complete block.
    #[non_exhaustive]
    TrailingBytes {
        /// Bytes occupied by the first complete block.
        consumed: usize,
        /// Total payload length.
        len: usize,
    },
    /// A zero-column block is the upload terminator, not INSERT data.
    NoColumns,
    /// The payload exceeded [`Connection::with_max_block_bytes`]. Checked
    /// before the block is scanned.
    #[non_exhaustive]
    TooLarge {
        /// Payload length.
        len: usize,
        /// Maximum outbound Native block length.
        limit: usize,
    },
}

impl fmt::Display for InsertBlockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The DecodeError is reported through `source`, not repeated here.
            Self::Decode(_) => formatter.write_str("Native block scan failed"),
            Self::Truncated => formatter.write_str("payload is truncated"),
            Self::TrailingBytes { consumed, len } => write!(
                formatter,
                "{} bytes follow the first block",
                len.saturating_sub(*consumed)
            ),
            Self::NoColumns => formatter.write_str("data blocks must contain at least one column"),
            Self::TooLarge { len, limit } => write!(
                formatter,
                "block length {len} exceeds the {limit} byte limit"
            ),
        }
    }
}

impl PartialEq for InsertBlockError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Decode(left), Self::Decode(right)) => left.to_string() == right.to_string(),
            (Self::Truncated, Self::Truncated) | (Self::NoColumns, Self::NoColumns) => true,
            (
                Self::TrailingBytes {
                    consumed: left_consumed,
                    len: left_len,
                },
                Self::TrailingBytes {
                    consumed: right_consumed,
                    len: right_len,
                },
            ) => left_consumed == right_consumed && left_len == right_len,
            (
                Self::TooLarge {
                    len: left_len,
                    limit: left_limit,
                },
                Self::TooLarge {
                    len: right_len,
                    limit: right_limit,
                },
            ) => left_len == right_len && left_limit == right_limit,
            _ => false,
        }
    }
}

impl Eq for InsertBlockError {}

impl Error for InsertBlockError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

struct ClientIdentity {
    user: String,
    client_name: String,
    version_major: u64,
    version_minor: u64,
}

/// Incremental handshake, Ping/Pong, and uncompressed Query engine.
///
/// Events have exactly one channel: [`Self::feed`] reports only errors, and
/// every completed packet is queued for [`Self::poll_event`]. A `Vec` return
/// cannot carry events and an error at once, so a second drain API would be
/// needed for events completed before a malformed packet. One queue keeps
/// them in order.
///
/// The driver contract is: feed any amount, drain `poll_event` until it
/// returns `None`, repeat. Drain after every `feed` and `finish`, including
/// when they fail. `feed` always parses all of its input, so there is no
/// resume call and no hidden input waiting for queue space.
///
/// The queue is unbounded by design. Its memory is proportional to the bytes
/// fed since the last drain, which the driver controls through its read
/// size; draining after every feed keeps it to about one read. Each queued
/// event costs `size_of::<Event>()`, at most 56 bytes on 64-bit targets,
/// plus its own payload allocation; the smallest packet, a one-byte Pong or
/// EndOfStream, is the worst ratio. A single Native block is bounded
/// separately by [`Self::with_max_block_bytes`].
pub struct Connection {
    state: State,
    /// The tail of one incomplete parse step. Always starts at a parse-step
    /// boundary, and is empty or shorter than `need` between calls.
    input: Vec<u8>,
    /// Minimum `input.len()` at which reparsing can make progress.
    need: usize,
    /// A packet body being read after its packet code.
    pending: Option<PendingPacket>,
    /// Bytes received after a terminal Exception, counted and dropped.
    trailing: usize,
    output: Vec<u8>,
    events: VecDeque<Event>,
    /// Queued events that must be drained before the next INSERT write.
    barrier_events: usize,
    max_block_bytes: usize,
    quota_key: String,
    identity: ClientIdentity,
    effective_revision: Option<u64>,
    /// Whether the active query sent any settings with IMPORTANT.
    active_query_sent_settings: bool,
    /// Whether any server packet code has been read for the active query.
    active_query_has_server_packet: bool,
    /// The error that moved the engine to `Failed(Protocol)`, returned again
    /// by `finish`.
    protocol_error: Option<ProtocolError>,
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
            .field("max_block_bytes", &self.max_block_bytes)
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
            barrier_events: 0,
            max_block_bytes: DEFAULT_MAX_BLOCK_BYTES,
            quota_key: hello.quota_key,
            identity: ClientIdentity {
                user: hello.user,
                client_name: hello.client_name,
                version_major: hello.version_major,
                version_minor: hello.version_minor,
            },
            effective_revision: None,
            active_query_sent_settings: false,
            active_query_has_server_packet: false,
            protocol_error: None,
            finish_result: None,
            input_limit: INPUT_LIMIT,
            #[cfg(test)]
            parse_steps: 0,
        })
    }

    /// Sets the largest Native block body accepted from one server packet, in
    /// bytes. The default is [`DEFAULT_MAX_BLOCK_BYTES`], 1 GiB. A value below
    /// [`MIN_MAX_BLOCK_BYTES`], 1 KiB, is raised to it.
    ///
    /// The limit applies to every inbound block-carrying packet from the next
    /// block that starts, and to each later [`Self::insert_data`] call: Data,
    /// including the zero-column end-of-data marker, Totals, Extremes, Log,
    /// and ProfileEvents. Log blocks from
    /// `send_logs_level` and ProfileEvents blocks count against the same cap
    /// as result blocks. A larger block fails the connection with
    /// [`ProtocolError::NativeBlockTooLarge`] once that many bytes have
    /// arrived without completing it. HTTP has no equivalent per-block cap, so
    /// a low value can reject results that HTTP returns:
    /// `SELECT randomString(1100) FROM numbers(100000)` produces blocks of
    /// about 72 MB with default server settings.
    /// An oversized outbound INSERT block is rejected with
    /// [`InsertBlockError::TooLarge`] before output or state changes; it does
    /// not fail the connection.
    ///
    /// Block memory is only ever allocated from bytes actually received,
    /// never from a declared length, and the block buffer's capacity never
    /// exceeds this limit. The same value is the retention budget shared by
    /// both fields of a later [`Event::TableColumns`]: bytes past it are
    /// skipped and the event has `truncated: true`, so metadata size alone
    /// does not fail the INSERT. A declared TableColumns field length above
    /// 64 times this limit (saturating) is treated as corruption and fails
    /// the connection with [`ProtocolError::StringTooLong`]. Both are
    /// snapshotted when the TableColumns packet begins.
    pub fn with_max_block_bytes(mut self, limit: usize) -> Self {
        self.max_block_bytes = limit.max(MIN_MAX_BLOCK_BYTES);
        self
    }

    /// Returns the current protocol state.
    pub fn state(&self) -> State {
        self.state
    }

    /// Returns `min(ENGINE_REVISION, server revision)` once the Server Hello
    /// is parsed, otherwise `None`.
    ///
    /// Pass this as `DecodeOptions::protocol_revision` when decoding the
    /// payloads of [`Event::Data`] and the other block events, and as
    /// `EncodeOptions::protocol_revision` when encoding INSERT blocks.
    pub fn effective_revision(&self) -> Option<u64> {
        self.effective_revision
    }

    /// Takes all queued output bytes in protocol order.
    ///
    /// After a Server Hello this includes the addendum. A later [`Self::ping`]
    /// appends its packet, so a driver may take both in one ordered write.
    /// Until this is called after [`Self::new`], the engine holds the
    /// plaintext password in the queued Client Hello.
    ///
    /// A protocol failure, from [`Self::feed`] or an EOF reported by
    /// [`Self::finish`], discards any output not yet taken, in every state
    /// including the handshake: none of it is safe to write after the
    /// server stream went wrong. Take output before feeding if it must be
    /// written regardless.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Returns the next completed event, oldest first.
    ///
    /// Call until it returns `None` after every [`Self::feed`] and
    /// [`Self::finish`]. Queued events hold memory proportional to the bytes
    /// fed since the last drain.
    pub fn poll_event(&mut self) -> Option<Event> {
        let event = self.events.pop_front()?;
        if !matches!(event, Event::Log(_) | Event::ProfileEvents(_)) {
            debug_assert!(self.barrier_events > 0);
            self.barrier_events = self.barrier_events.saturating_sub(1);
        }
        Some(event)
    }

    /// Queues an uncompressed query and its empty external-data block.
    ///
    /// Use this for exchanges that need no later client Data, including SELECT,
    /// inline `INSERT ... VALUES`, and `INSERT ... SELECT`. Use
    /// [`Self::insert`] when the client will upload encoded Native blocks.
    /// `input()`, external tables, parallel-replica exchanges, cancellation,
    /// and compressed packets are unsupported.
    ///
    /// Rejected with [`ProtocolError::InvalidState`] unless the connection is
    /// `Ready`, every event is drained, and no server packet is partly
    /// received. Draining first guarantees the caller has seen everything the
    /// previous exchange produced, including an Exception that ended it.
    ///
    /// There is no Cancel packet or reset. A driver that abandons a query
    /// before its EndOfStream or Exception must discard the `Connection` and
    /// its socket. At revision 54460 the server never echoes the query ID, so
    /// a binding that needs one to correlate logs or `system.query_log` must
    /// generate it and pass it with [`Query::with_query_id`].
    ///
    /// The engine frames each result block with core's scanner, so a result
    /// column whose type core cannot decode fails the whole connection with
    /// [`ProtocolError::Decode`]: the block's end cannot be found, so nothing
    /// after it can be parsed. Check the result types a binding allows.
    ///
    /// Setting names are capped at 64 KiB ([`ProtocolError::StringTooLong`])
    /// as an engine policy that independently bounds user-controlled
    /// identifiers. Setting values and parameter names and values have no
    /// per-field limit beyond the server's 1 GiB string limit. The whole
    /// packet must fit in 64 MiB ([`ProtocolError::QueryOutputTooLarge`],
    /// which reports its exact length).
    pub fn query(&mut self, query: Query) -> Result<(), ProtocolError> {
        self.start_query(
            query,
            State::Querying,
            "send Query",
            "send Query while a server packet is partly received",
            "send Query before draining completed events",
        )
    }

    /// Starts an uncompressed INSERT that may request client Native blocks.
    ///
    /// The queued bytes are the Query packet followed by the empty Data block
    /// that terminates external tables. Drain the resulting
    /// [`Event::InsertSchema`] before calling [`Self::insert_data`] or
    /// [`Self::finish_insert`]. An inline VALUES, INSERT SELECT, or async
    /// INSERT can instead complete with Progress, ProfileEvents, Log, and
    /// EndOfStream without a schema. In that case no second client Data marker
    /// is sent.
    ///
    /// The same Query validation and 64 MiB request limit as [`Self::query`]
    /// apply. A server Exception may end any INSERT phase. If one arrives,
    /// queued upload bytes are discarded so they cannot leak into a later
    /// exchange.
    pub fn insert(&mut self, query: Query) -> Result<(), ProtocolError> {
        self.start_query(
            query,
            State::AwaitingInsertSchema,
            "start INSERT",
            "start INSERT while a server packet is partly received",
            "start INSERT before draining completed events",
        )
    }

    fn start_query(
        &mut self,
        query: Query,
        next_state: State,
        operation: &'static str,
        partial_operation: &'static str,
        undrained_operation: &'static str,
    ) -> Result<(), ProtocolError> {
        if self.pending.is_some() || !self.input.is_empty() {
            return Err(ProtocolError::InvalidState {
                operation: partial_operation,
                state: self.state,
            });
        }
        if self.state != State::Ready {
            return Err(ProtocolError::InvalidState {
                operation,
                state: self.state,
            });
        }
        if !self.events.is_empty() {
            return Err(ProtocolError::InvalidState {
                operation: undrained_operation,
                state: self.state,
            });
        }
        let revision = self.effective_revision.unwrap_or(0);
        if revision < ENGINE_REVISION {
            return Err(ProtocolError::UnsupportedQueryRevision { revision });
        }
        for (field, value) in [
            ("query_id", query.query_id.as_str()),
            ("initial_address", query.initial_address.as_str()),
            ("os_user", query.os_user.as_str()),
            ("hostname", query.hostname.as_str()),
        ] {
            validate_client_string(field, value, false)?;
        }
        for (name, value) in &query.settings {
            if name.is_empty() {
                return Err(ProtocolError::EmptyName {
                    field: "setting_name",
                });
            }
            if name.len() > SETTING_NAME_LIMIT {
                return Err(ProtocolError::StringTooLong {
                    field: "setting_name",
                    length: Some(name.len()),
                    limit: SETTING_NAME_LIMIT,
                });
            }
            if name == "output_format_native_encode_types_in_binary_format" && value != "0" {
                return Err(ProtocolError::BinaryNativeTypeEncodingUnsupported);
            }
        }
        let mut parameter_names = HashSet::with_capacity(query.parameters.len());
        for (name, _) in &query.parameters {
            if name.is_empty() {
                return Err(ProtocolError::EmptyName {
                    field: "parameter_name",
                });
            }
            if !parameter_names.insert(name.as_str()) {
                return Err(ProtocolError::DuplicateName {
                    field: "parameter_name",
                    name: name.clone(),
                });
            }
        }
        let packet_size = query_packet_len(&query, &self.identity, &self.quota_key).ok_or(
            ProtocolError::QueryOutputTooLarge {
                length: usize::MAX,
                limit: QUERY_OUTPUT_LIMIT,
            },
        )?;
        if packet_size > QUERY_OUTPUT_LIMIT {
            return Err(ProtocolError::QueryOutputTooLarge {
                length: packet_size,
                limit: QUERY_OUTPUT_LIMIT,
            });
        }

        // Written straight into the output queue. Every fallible step ran in
        // `query_packet_len` above, so the only error below is the overflow
        // it already ruled out; on any error the queue is restored.
        let start = self.output.len();
        self.output.reserve_exact(packet_size);
        let packet = &mut self.output;
        put_varuint(packet, CLIENT_PACKET_QUERY);
        put_string(packet, &query.query_id);
        packet.push(1); // ClientInfo::QueryKind::INITIAL_QUERY
        put_string(packet, &self.identity.user);
        put_string(packet, &query.query_id);
        put_string(packet, &query.initial_address);
        packet.extend_from_slice(&query.initial_query_start_time_microseconds.to_le_bytes());
        packet.push(1); // ClientInfo::Interface::TCP
        put_string(packet, &query.os_user);
        put_string(packet, &query.hostname);
        put_string(packet, &self.identity.client_name);
        put_varuint(packet, self.identity.version_major);
        put_varuint(packet, self.identity.version_minor);
        put_varuint(packet, ENGINE_REVISION);
        put_string(packet, &self.quota_key);
        put_varuint(packet, 0); // distributed_depth
        put_varuint(packet, 0); // version_patch, absent from Client Hello
        packet.push(0); // no OpenTelemetry trace context
        put_varuint(packet, 0); // collaborate_with_initiator
        put_varuint(packet, 0); // obsolete_count_participating_replicas
        put_varuint(packet, 0); // number_of_current_replica
        for (name, value) in &query.settings {
            put_string(packet, name);
            put_varuint(packet, 1); // SettingFlag::IMPORTANT
            put_string(packet, value);
        }
        put_string(packet, "");
        put_string(packet, ""); // empty interserver secret hash
        put_varuint(packet, 2); // QueryProcessingStage::Complete
        put_varuint(packet, 0); // uncompressed
        put_string(packet, &query.sql);
        for (name, value) in &query.parameters {
            put_string(packet, name);
            put_varuint(packet, 2); // SettingFlag::CUSTOM
            if let Err(error) = put_quoted_string(packet, value) {
                packet.truncate(start);
                return Err(error);
            }
        }
        put_string(packet, "");
        put_empty_data_packet(packet);
        debug_assert_eq!(packet.len() - start, packet_size);
        self.active_query_sent_settings = !query.settings.is_empty();
        self.active_query_has_server_packet = false;
        self.state = next_state;
        Ok(())
    }

    /// Queues one client Data packet for an INSERT.
    ///
    /// `native_block` must be exactly one complete Native block encoded for
    /// this connection's effective revision, with at least one column. The
    /// block is scanned for its boundary and framing but is not decoded or
    /// copied until validation succeeds. A complete or partial
    /// Log/ProfileEvents packet does not block the write; its event may
    /// remain queued. The InsertSchema event itself must have been consumed,
    /// and a partial Exception or unknown packet remains a barrier.
    ///
    /// # Errors
    ///
    /// Every error is atomic: output and state are unchanged and the
    /// connection stays usable.
    ///
    /// - [`ProtocolError::InvalidState`] if the state is not
    ///   [`State::Inserting`], or a non-telemetry server packet is queued or
    ///   partly received (the write barrier).
    /// - [`ProtocolError::InvalidInsertBlock`] if the payload is oversized,
    ///   malformed, truncated, concatenated, or has no columns; the
    ///   [`InsertBlockError`] says which.
    /// - [`ProtocolError::InsertOutputAllocationFailed`] if the Data packet
    ///   cannot be reserved in the output queue.
    pub fn insert_data(&mut self, native_block: &[u8]) -> Result<(), ProtocolError> {
        if self.state != State::Inserting {
            return Err(ProtocolError::InvalidState {
                operation: "send INSERT data",
                state: self.state,
            });
        }
        if self.insert_write_is_blocked() {
            return Err(ProtocolError::InvalidState {
                operation:
                    "send INSERT data while a non-telemetry server packet is queued or partly received",
                state: self.state,
            });
        }
        if native_block.len() > self.max_block_bytes {
            return Err(ProtocolError::InvalidInsertBlock(
                InsertBlockError::TooLarge {
                    len: native_block.len(),
                    limit: self.max_block_bytes,
                },
            ));
        }

        let revision = self.effective_revision.unwrap_or(ENGINE_REVISION);
        let boundary = native_scanner(revision)
            .scan(native_block)
            .map_err(|error| {
                ProtocolError::InvalidInsertBlock(InsertBlockError::Decode(Arc::new(error)))
            })?;
        let (consumed, num_columns) = match boundary {
            BlockBoundary::NeedMore => {
                return Err(ProtocolError::InvalidInsertBlock(
                    InsertBlockError::Truncated,
                ))
            }
            BlockBoundary::Complete {
                consumed,
                num_columns,
                ..
            } => (consumed, num_columns),
            _ => {
                return Err(ProtocolError::InvalidInsertBlock(
                    InsertBlockError::Truncated,
                ))
            }
        };
        if consumed != native_block.len() {
            return Err(ProtocolError::InvalidInsertBlock(
                InsertBlockError::TrailingBytes {
                    consumed,
                    len: native_block.len(),
                },
            ));
        }
        // The server treats any zero-column block as end-of-data, regardless
        // of its declared row count. Only finish_insert may send one.
        if num_columns == 0 {
            return Err(ProtocolError::InvalidInsertBlock(
                InsertBlockError::NoColumns,
            ));
        }

        let additional = native_block.len().checked_add(2).ok_or(
            ProtocolError::InsertOutputAllocationFailed {
                additional: usize::MAX,
            },
        )?;
        // Reserve the whole packet before writing any of it, so a failure
        // leaves `output` untouched and the header and body are written
        // without a reallocation between them. Exact sizing for an empty
        // queue avoids slack in the common take-after-every-write driver;
        // amortized growth otherwise keeps repeated untaken writes linear.
        let reserved = if self.output.is_empty() {
            self.output.try_reserve_exact(additional)
        } else {
            self.output.try_reserve(additional)
        };
        reserved.map_err(|_| ProtocolError::InsertOutputAllocationFailed { additional })?;
        put_varuint(&mut self.output, CLIENT_PACKET_DATA);
        put_string(&mut self.output, "");
        self.output.extend_from_slice(native_block);
        Ok(())
    }

    /// Queues the zero-column Data block that ends an INSERT upload.
    ///
    /// The state changes immediately, so the marker can be queued only once
    /// and no later data block can follow it. Success is reported only by a
    /// server [`Event::EndOfStream`].
    pub fn finish_insert(&mut self) -> Result<(), ProtocolError> {
        if self.state != State::Inserting {
            return Err(ProtocolError::InvalidState {
                operation: "finish INSERT",
                state: self.state,
            });
        }
        if self.insert_write_is_blocked() {
            return Err(ProtocolError::InvalidState {
                operation:
                    "finish INSERT while a non-telemetry server packet is queued or partly received",
                state: self.state,
            });
        }
        put_empty_data_packet(&mut self.output);
        self.state = State::FinishingInsert;
        Ok(())
    }

    /// Feeds an arbitrary received byte chunk.
    ///
    /// Completed packets are queued for [`Self::poll_event`], including the
    /// ones completed before an error in the same chunk. A malformed packet
    /// returns the error and moves the engine to
    /// `State::Failed(FailureKind::Protocol)`. The failure also discards any
    /// queued output not yet returned by [`Self::take_output`], such as
    /// INSERT Data packets, so no client bytes follow malformed server input.
    ///
    /// Parsing stops at any terminal server Exception (see
    /// [`Event::Exception`]). Later bytes, in this chunk or in later calls,
    /// are counted without being parsed and `feed` keeps returning `Ok`, so
    /// the outcome never depends on where the stream was split.
    /// [`Self::finish`] reports them as [`ProtocolError::TrailingInput`].
    ///
    /// Every byte is parsed before `feed` returns, whatever the chunk size:
    /// one call may carry thousands of packets and multi-megabyte blocks.
    /// Scalar packet fields are parsed in place; only the tail of one
    /// incomplete field is retained until enough bytes arrive. Native bodies
    /// are copied into their event buffers without decoding columns. On a
    /// live connection an empty slice is a no-op.
    ///
    /// Event memory is proportional to the bytes fed since the last drain,
    /// so drain [`Self::poll_event`] after every feed.
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
        if let Err(error) = &result {
            self.fail_protocol();
            self.protocol_error = Some(error.clone());
        }
        result
    }

    /// Marks transport EOF.
    ///
    /// EOF closes a `Ready` engine successfully. It is truncation during
    /// handshake, SELECT, INSERT, or Ping, or whenever a packet is incomplete.
    /// After a terminal server Exception it succeeds unless bytes followed
    /// the Exception. Every INSERT Exception is treated as terminal even when
    /// the server may instead be waiting in its skip-data path. Reusable
    /// query-time exceptions leave the engine Ready instead (see
    /// [`Event::Exception`]). After a protocol failure it returns the error
    /// that failed the connection. EOF that fails the connection discards
    /// untaken output, as in [`Self::feed`]. Events queued before EOF stay
    /// queued; drain them afterwards as usual. The first EOF outcome is
    /// stored and every later call returns it again.
    pub fn finish(&mut self) -> Result<(), ProtocolError> {
        if let Some(result) = &self.finish_result {
            return result.clone();
        }
        let result = match self.state {
            State::Failed(FailureKind::ServerException) if self.trailing == 0 => Ok(()),
            State::Failed(FailureKind::ServerException) => Err(ProtocolError::TrailingInput {
                length: self.trailing,
            }),
            // The error that failed the connection, so EOF reports the cause.
            State::Failed(FailureKind::Protocol) => {
                Err(self
                    .protocol_error
                    .clone()
                    .unwrap_or(ProtocolError::InvalidState {
                        operation: "finish after protocol failure",
                        state: self.state,
                    }))
            }
            State::Closed => Ok(()),
            State::Ready if self.input.is_empty() && self.pending.is_none() => {
                self.state = State::Closed;
                Ok(())
            }
            State::Handshaking
            | State::Ready
            | State::Querying
            | State::AwaitingInsertSchema
            | State::Inserting
            | State::FinishingInsert
            | State::AwaitingPong => {
                self.fail_protocol();
                self.protocol_error = Some(ProtocolError::UnexpectedEof);
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
    /// [`ProtocolError::UnexpectedPacket`]. Like [`Self::query`], it is also
    /// rejected until every queued event is drained.
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
        if !self.events.is_empty() {
            return Err(ProtocolError::InvalidState {
                operation: "send Ping before draining completed events",
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
        // No bytes queued before a fatal protocol error are safe to write.
        // In particular, an INSERT upload must not continue after malformed
        // or unexpected server input.
        self.output.clear();
        // Queued events stay queued, so an input error never discards a
        // packet completed earlier in the same stream.
    }

    /// Whether an INSERT write could overtake a server packet that may end or
    /// redirect the exchange. Complete or partial Log/ProfileEvents packets
    /// are harmless full-duplex telemetry. A partial packet code, Exception,
    /// schema, or any other event remains a barrier.
    fn insert_write_is_blocked(&self) -> bool {
        let pending_is_telemetry = matches!(
            self.pending,
            Some(PendingPacket::Block(BlockProgress {
                kind: BlockKind::Log | BlockKind::ProfileEvents,
                ..
            }))
        );
        let partial_is_blocked = if self.pending.is_some() {
            !pending_is_telemetry
        } else {
            !self.input.is_empty()
        };
        partial_is_blocked || self.barrier_events > 0
    }

    fn queue_event(&mut self, event: Event, next_state: Option<State>) {
        if !matches!(event, Event::Log(_) | Event::ProfileEvents(_)) {
            self.barrier_events += 1;
        }
        self.events.push_back(event);
        if let Some(state) = next_state {
            self.state = state;
        }
    }

    fn feed_inner(&mut self, mut bytes: &[u8]) -> Result<(), ProtocolError> {
        if self.state == State::Failed(FailureKind::ServerException) {
            self.trailing = self.trailing.saturating_add(bytes.len());
            return Ok(());
        }

        // Complete a step that straddles the previous chunk boundary. Copy
        // only as many bytes as the parser asked for, so that the rest of a
        // large chunk is parsed in place below. Between calls `input` is
        // empty or shorter than `need`, so each parse here makes progress.
        while !self.input.is_empty() && !bytes.is_empty() {
            let wanted = self
                .need
                .saturating_sub(self.input.len())
                .max(1)
                .min(bytes.len());
            let (head, rest) = bytes.split_at(wanted);
            self.retain_input(head)?;
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
            self.retain_input(rest)?;
        }

        Ok(())
    }

    fn retain_input(&mut self, bytes: &[u8]) -> Result<(), ProtocolError> {
        let length = self.input.len().saturating_add(bytes.len());
        if length > self.input_limit {
            return Err(ProtocolError::InputTooLarge {
                length,
                limit: self.input_limit,
            });
        }
        self.input.extend_from_slice(bytes);
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

    /// Parses one packet code or one incremental packet-body step.
    fn parse_step(&mut self, bytes: &[u8]) -> ParseResult<usize> {
        #[cfg(test)]
        {
            self.parse_steps += 1;
        }
        if let Some(mut pending) = self.pending.take() {
            match &mut pending {
                PendingPacket::Exception(progress) => {
                    let consumed = match progress.step(bytes) {
                        Ok(consumed) => consumed,
                        Err(error) => {
                            self.pending = Some(pending);
                            return Err(error);
                        }
                    };
                    if let Some(has_nested) = progress.has_nested {
                        let exception = ServerException {
                            code: progress.code,
                            name: std::mem::take(&mut progress.name),
                            message: std::mem::take(&mut progress.message),
                            stack_trace: std::mem::take(&mut progress.stack_trace),
                            has_nested,
                            truncated: progress.truncated,
                        };
                        let insert_exception = matches!(
                            self.state,
                            State::AwaitingInsertSchema | State::Inserting | State::FinishingInsert
                        );
                        let query_exception = self.state == State::Querying;
                        let terminal = !query_exception
                            || is_terminal_query_exception(
                                &exception,
                                self.active_query_sent_settings,
                                progress.first_query_packet,
                            );
                        if insert_exception {
                            // Upload bytes not yet taken by the driver must
                            // never be sent after the server has rejected the
                            // INSERT, or as part of a later exchange.
                            self.output.clear();
                        }
                        self.active_query_sent_settings = false;
                        self.active_query_has_server_packet = false;
                        let next_state = if terminal {
                            State::Failed(FailureKind::ServerException)
                        } else {
                            State::Ready
                        };
                        self.queue_event(Event::Exception(Box::new(exception)), Some(next_state));
                    } else {
                        self.pending = Some(pending);
                    }
                    return Ok(consumed);
                }
                PendingPacket::Block(progress) => {
                    let (consumed, completed) = match progress.step(bytes) {
                        Ok(result) => result,
                        Err(error) => {
                            self.pending = Some(pending);
                            return Err(error);
                        }
                    };
                    if let Some(completed) = completed {
                        if completed.kind == BlockKind::InsertSchema {
                            if completed.num_columns == 0 || completed.num_rows != 0 {
                                return Err(ParseError::Protocol(
                                    ProtocolError::InvalidInsertSchema,
                                ));
                            }
                            self.queue_event(
                                Event::InsertSchema(completed.native),
                                Some(State::Inserting),
                            );
                        } else if !(completed.kind == BlockKind::Data
                            && completed.is_empty_sentinel)
                        {
                            self.queue_event(completed.kind.event(completed.native), None);
                        }
                    } else {
                        self.pending = Some(pending);
                    }
                    return Ok(consumed);
                }
                PendingPacket::TableColumns(progress) => {
                    let (consumed, completed) = match progress.step(bytes) {
                        Ok(result) => result,
                        Err(error) => {
                            self.pending = Some(pending);
                            return Err(error);
                        }
                    };
                    if let Some(event) = completed {
                        self.queue_event(event, None);
                    } else {
                        self.pending = Some(pending);
                    }
                    return Ok(consumed);
                }
            }
        }

        let mut cursor = Cursor::new(bytes);
        let packet = cursor.varuint()?;
        let first_query_packet = if self.state == State::Querying {
            let first = !self.active_query_has_server_packet;
            self.active_query_has_server_packet = true;
            first
        } else {
            false
        };
        match (self.state, packet) {
            (State::Handshaking, SERVER_PACKET_HELLO) => {
                let hello = parse_server_hello(&mut cursor)?;
                self.accept_server_hello(hello)
                    .map_err(ParseError::Protocol)?;
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
            (
                State::Handshaking
                | State::Ready
                | State::Querying
                | State::AwaitingInsertSchema
                | State::Inserting
                | State::FinishingInsert
                | State::AwaitingPong,
                SERVER_PACKET_EXCEPTION,
            ) => {
                self.pending = Some(PendingPacket::Exception(ExceptionProgress::new(
                    first_query_packet,
                )));
            }
            (State::AwaitingPong, SERVER_PACKET_PONG) => {
                self.queue_event(Event::Pong, Some(State::Ready));
            }
            (State::Querying, SERVER_PACKET_DATA) => {
                self.start_block(BlockKind::Data);
            }
            (State::Querying, SERVER_PACKET_TOTALS) => {
                self.start_block(BlockKind::Totals);
            }
            (State::Querying, SERVER_PACKET_EXTREMES) => {
                self.start_block(BlockKind::Extremes);
            }
            (State::Querying, SERVER_PACKET_LOG) => {
                self.start_block(BlockKind::Log);
            }
            (State::Querying, SERVER_PACKET_PROFILE_EVENTS) => {
                self.start_block(BlockKind::ProfileEvents);
            }
            (State::Querying, SERVER_PACKET_PROGRESS) => {
                let progress = parse_progress(&mut cursor)?;
                self.queue_event(Event::Progress(progress), None);
            }
            (State::Querying, SERVER_PACKET_PROFILE_INFO) => {
                let profile = parse_profile_info(&mut cursor)?;
                self.queue_event(Event::ProfileInfo(profile), None);
            }
            (State::Querying, SERVER_PACKET_END_OF_STREAM) => {
                self.active_query_sent_settings = false;
                self.active_query_has_server_packet = false;
                self.queue_event(Event::EndOfStream, Some(State::Ready));
            }
            (State::AwaitingInsertSchema, SERVER_PACKET_TABLE_COLUMNS) => {
                self.pending = Some(PendingPacket::TableColumns(TableColumnsProgress::new(
                    self.max_block_bytes,
                )));
            }
            (State::AwaitingInsertSchema, SERVER_PACKET_DATA) => {
                self.start_block(BlockKind::InsertSchema);
            }
            (State::AwaitingInsertSchema, SERVER_PACKET_PROGRESS) => {
                let progress = parse_progress(&mut cursor)?;
                self.queue_event(Event::Progress(progress), None);
            }
            (
                State::AwaitingInsertSchema | State::Inserting | State::FinishingInsert,
                SERVER_PACKET_LOG,
            ) => {
                self.start_block(BlockKind::Log);
            }
            (
                State::AwaitingInsertSchema | State::Inserting | State::FinishingInsert,
                SERVER_PACKET_PROFILE_EVENTS,
            ) => {
                self.start_block(BlockKind::ProfileEvents);
            }
            (State::FinishingInsert, SERVER_PACKET_END_OF_STREAM) => {
                self.active_query_sent_settings = false;
                self.active_query_has_server_packet = false;
                self.queue_event(Event::EndOfStream, Some(State::Ready));
            }
            (State::AwaitingInsertSchema, SERVER_PACKET_END_OF_STREAM) => {
                self.active_query_sent_settings = false;
                self.active_query_has_server_packet = false;
                self.queue_event(Event::EndOfStream, Some(State::Ready));
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

    fn start_block(&mut self, kind: BlockKind) {
        let revision = self.effective_revision.unwrap_or(ENGINE_REVISION);
        self.pending = Some(PendingPacket::Block(BlockProgress::new(
            kind,
            revision,
            self.max_block_bytes,
        )));
    }

    fn accept_server_hello(&mut self, hello: ServerHello) -> Result<(), ProtocolError> {
        if hello.effective_revision >= MIN_PROTOCOL_VERSION_WITH_ADDENDUM {
            // At v26.8.1.2041-lts `src/Client/Connection.cpp::sendAddendum`
            // writes no packet code. At this engine revision only the
            // quota-key string is present.
            if hello.effective_revision >= MIN_PROTOCOL_VERSION_WITH_QUOTA_KEY {
                put_string(&mut self.output, &self.quota_key);
            }
        }
        self.effective_revision = Some(hello.effective_revision);
        self.state = State::Ready;
        self.queue_event(Event::ServerHello(Box::new(hello)), None);
        Ok(())
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
// Query result packets
// ---------------------------------------------------------------------------

enum PendingPacket {
    Exception(ExceptionProgress),
    Block(BlockProgress),
    TableColumns(TableColumnsProgress),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BlockKind {
    Data,
    InsertSchema,
    ProfileEvents,
    Log,
    Totals,
    Extremes,
}

impl BlockKind {
    fn event(self, bytes: Vec<u8>) -> Event {
        match self {
            Self::Data => Event::Data(bytes),
            Self::InsertSchema => Event::InsertSchema(bytes),
            Self::ProfileEvents => Event::ProfileEvents(bytes),
            Self::Log => Event::Log(bytes),
            Self::Totals => Event::Totals(bytes),
            Self::Extremes => Event::Extremes(bytes),
        }
    }
}

struct BlockProgress {
    kind: BlockKind,
    table_name_done: bool,
    scanner: BlockScanner,
    native: Vec<u8>,
    limit: usize,
}

struct CompletedBlock {
    kind: BlockKind,
    native: Vec<u8>,
    num_columns: usize,
    num_rows: usize,
    is_empty_sentinel: bool,
}

impl BlockProgress {
    fn new(kind: BlockKind, revision: u64, limit: usize) -> Self {
        Self {
            kind,
            table_name_done: false,
            scanner: native_scanner(revision),
            native: Vec::new(),
            limit,
        }
    }

    fn step(&mut self, bytes: &[u8]) -> ParseResult<(usize, Option<CompletedBlock>)> {
        if !self.table_name_done {
            let mut cursor = Cursor::new(bytes);
            let _table_name = cursor.string_bytes("block_table_name", SERVER_HELLO_STRING_LIMIT)?;
            self.table_name_done = true;
            return Ok((cursor.position(), None));
        }
        if bytes.is_empty() {
            return Err(ParseError::NeedMore(1));
        }
        let remaining = self.limit.saturating_sub(self.native.len());
        if remaining == 0 {
            return Err(ParseError::Protocol(ProtocolError::NativeBlockTooLarge {
                limit: self.limit,
            }));
        }
        let offered = bytes.len().min(remaining);
        let previous = self.native.len();
        if previous == 0 {
            let boundary = self
                .scanner
                .scan(&bytes[..offered])
                .map_err(ProtocolError::from)
                .map_err(ParseError::Protocol)?;
            if let BlockBoundary::Complete {
                consumed,
                num_columns,
                num_rows,
                ..
            } = boundary
            {
                self.native.extend_from_slice(&bytes[..consumed]);
                return Ok((
                    consumed,
                    Some(CompletedBlock {
                        kind: self.kind,
                        native: std::mem::take(&mut self.native),
                        num_columns,
                        num_rows,
                        is_empty_sentinel: num_rows == 0 && num_columns == 0,
                    }),
                ));
            }
            extend_bounded(&mut self.native, &bytes[..offered], self.limit);
            if self.native.len() == self.limit {
                return Err(ParseError::Protocol(ProtocolError::NativeBlockTooLarge {
                    limit: self.limit,
                }));
            }
            return Ok((offered, None));
        }

        extend_bounded(&mut self.native, &bytes[..offered], self.limit);
        let boundary = self
            .scanner
            .scan(&self.native)
            .map_err(ProtocolError::from)
            .map_err(ParseError::Protocol)?;
        if let BlockBoundary::Complete {
            consumed,
            num_columns,
            num_rows,
            ..
        } = boundary
        {
            // A prefix of `previous` bytes already scanned as incomplete, so
            // the block must end in the bytes added now. Anything else is a
            // scanner inconsistency, reported rather than miscounted.
            debug_assert!(consumed > previous);
            if consumed <= previous {
                return Err(ParseError::Protocol(
                    ProtocolError::InconsistentBlockBoundary {
                        consumed,
                        buffered: previous,
                    },
                ));
            }
            self.native.truncate(consumed);
            let consumed_now = consumed - previous;
            return Ok((
                consumed_now,
                Some(CompletedBlock {
                    kind: self.kind,
                    native: std::mem::take(&mut self.native),
                    num_columns,
                    num_rows,
                    is_empty_sentinel: num_rows == 0 && num_columns == 0,
                }),
            ));
        }
        if self.native.len() == self.limit {
            return Err(ParseError::Protocol(ProtocolError::NativeBlockTooLarge {
                limit: self.limit,
            }));
        }
        Ok((offered, None))
    }
}

// ---------------------------------------------------------------------------
// TableColumns packet
// ---------------------------------------------------------------------------

/// Incrementally reads the two raw strings in a TableColumns packet.
///
/// The two fields share one retention budget of `limit` bytes: the table
/// name keeps up to `limit`, and the description keeps up to whatever the
/// retained table name left. Any remainder is consumed and discarded as it
/// arrives, so a very wide table cannot trip the general retained-input
/// backstop or fail an INSERT only because its metadata is large. Capacity
/// grows geometrically from bytes actually received and never from an
/// untrusted declared length. The two buffers' capacities together stay
/// within `limit`, up to any slack the allocator leaves after the finished
/// name's `shrink_to_fit` (std does not guarantee `capacity == len`).
///
/// A declared field length above `declared_limit`
/// (`TABLE_COLUMNS_DECLARED_FACTOR` times `limit`) is a protocol error, so a
/// corrupt length fails as soon as its VarUInt is complete instead of
/// skipping input until EOF. This mirrors `EXCEPTION_DECLARED_LIMIT`.
struct TableColumnsProgress {
    next: TableColumnsField,
    remaining: Option<u64>,
    table_name: Vec<u8>,
    description: Vec<u8>,
    truncated: bool,
    /// Retention budget shared by both fields.
    limit: usize,
    /// Largest declared field length accepted.
    declared_limit: usize,
}

#[derive(Clone, Copy)]
enum TableColumnsField {
    TableName,
    Description,
}

impl TableColumnsProgress {
    /// `limit` is the connection's block cap, snapshotted at packet start.
    fn new(limit: usize) -> Self {
        Self {
            next: TableColumnsField::TableName,
            remaining: None,
            table_name: Vec::new(),
            description: Vec::new(),
            truncated: false,
            limit,
            declared_limit: limit.saturating_mul(TABLE_COLUMNS_DECLARED_FACTOR),
        }
    }

    fn step(&mut self, bytes: &[u8]) -> ParseResult<(usize, Option<Event>)> {
        if let Some(remaining) = self.remaining {
            // Zero-length fields and the final byte of non-empty fields call
            // `finish_field` in the step that consumes their prefix or body.
            debug_assert!(remaining > 0);
            if bytes.is_empty() {
                return Err(ParseError::NeedMore(1));
            }
            let consumed =
                usize::try_from(remaining).map_or(bytes.len(), |len| len.min(bytes.len()));
            let (value, field_limit) = match self.next {
                TableColumnsField::TableName => (&mut self.table_name, self.limit),
                // `table_name.len() <= limit`, so the budget cannot underflow;
                // saturate anyway rather than rely on it.
                TableColumnsField::Description => (
                    &mut self.description,
                    self.limit.saturating_sub(self.table_name.len()),
                ),
            };
            let retained = consumed.min(field_limit.saturating_sub(value.len()));
            extend_bounded(value, &bytes[..retained], field_limit);
            self.truncated |= retained < consumed;
            self.remaining = Some(remaining - consumed as u64);
            let completed = (self.remaining == Some(0))
                .then(|| self.finish_field())
                .flatten();
            return Ok((consumed, completed));
        }

        let mut cursor = Cursor::new(bytes);
        let length = cursor.varuint()?;
        // Checked before any byte of the field is taken, so an oversized
        // declaration fails as soon as its VarUInt is complete.
        if length > self.declared_limit as u64 {
            return Err(ParseError::Protocol(ProtocolError::StringTooLong {
                field: match self.next {
                    TableColumnsField::TableName => "table_columns_table_name",
                    TableColumnsField::Description => "table_columns_description",
                },
                length: Some(usize::try_from(length).unwrap_or(usize::MAX)),
                limit: self.declared_limit,
            }));
        }
        self.remaining = Some(length);
        let completed = (length == 0).then(|| self.finish_field()).flatten();
        Ok((cursor.position(), completed))
    }

    fn finish_field(&mut self) -> Option<Event> {
        self.remaining = None;
        match self.next {
            TableColumnsField::TableName => {
                // Drop growth slack so both buffers together stay within the
                // shared budget. The main table's name is empty, so this
                // is normally a no-op.
                self.table_name.shrink_to_fit();
                self.next = TableColumnsField::Description;
                None
            }
            TableColumnsField::Description => Some(Event::TableColumns {
                table_name: std::mem::take(&mut self.table_name),
                description: std::mem::take(&mut self.description),
                truncated: self.truncated,
            }),
        }
    }
}

/// Appends received block bytes, growing capacity geometrically but never
/// past `limit`. The caller guarantees `buffer.len() + bytes.len() <= limit`.
///
/// Capacity follows bytes actually received, never a declared length, and
/// stays within the configured block cap even for a block near the cap,
/// where plain doubling could reserve up to twice the limit.
fn extend_bounded(buffer: &mut Vec<u8>, bytes: &[u8], limit: usize) {
    let needed = buffer.len().saturating_add(bytes.len());
    debug_assert!(needed <= limit);
    if needed > buffer.capacity() {
        let target = buffer.capacity().saturating_mul(2).max(needed).min(limit);
        // `needed > capacity >= len`, so the subtraction cannot underflow.
        // `max(needed)` keeps the reservation sufficient even if a caller
        // ever broke the `limit` bound.
        buffer.reserve_exact(target.max(needed) - buffer.len());
    }
    buffer.extend_from_slice(bytes);
}

fn native_scanner(revision: u64) -> BlockScanner {
    let mut options = DecodeOptions::default();
    options.protocol_revision = revision;
    BlockScanner::new(options)
}

fn parse_progress(cursor: &mut Cursor<'_>) -> ParseResult<Progress> {
    Ok(Progress {
        read_rows: cursor.varuint()?,
        read_bytes: cursor.varuint()?,
        total_rows_to_read: cursor.varuint()?,
        written_rows: cursor.varuint()?,
        written_bytes: cursor.varuint()?,
        elapsed_ns: cursor.varuint()?,
    })
}

fn parse_profile_info(cursor: &mut Cursor<'_>) -> ParseResult<ProfileInfo> {
    Ok(ProfileInfo {
        rows: cursor.varuint()?,
        blocks: cursor.varuint()?,
        bytes: cursor.varuint()?,
        applied_limit: cursor.byte()? != 0,
        rows_before_limit: cursor.varuint()?,
        unused_obsolete_field: cursor.byte()? != 0,
    })
}

/// Whether a query-time Exception means the server is closing the
/// connection. See [`Event::Exception`] for the server rules.
fn is_terminal_query_exception(
    exception: &ServerException,
    sent_settings: bool,
    first_query_packet: bool,
) -> bool {
    // Confirmed at v26.8.1.2041-lts in `TCPHandler::runImpl`: 101, 720, and
    // 749 return from the connection loop after sending the Exception
    // (TCPHandler.cpp:1166). 99 cancels the output and returns before
    // sending anything (TCPHandler.cpp:1069), so it is listed defensively.
    if matches!(exception.code, 99 | 101 | 702 | 720 | 749) {
        return true;
    }
    // `TCPHandler::processQuery` reads ClientInfo and Settings before it
    // creates the query context. Any failure there is sent as the query's
    // first server packet and then the connection closes. The wire does not
    // identify the exact throw site, so a first-packet Exception after the
    // client sent settings fails closed. Once any earlier packet arrived, the
    // context exists and ordinary query errors leave the connection reusable.
    sent_settings && first_query_packet
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
struct ExceptionProgress {
    /// Whether this Exception packet was the first server packet of its query.
    first_query_packet: bool,
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
    fn new(first_query_packet: bool) -> Self {
        Self {
            first_query_packet,
            next: ExceptionField::Code,
            code: 0,
            name: String::new(),
            message: String::new(),
            stack_trace: String::new(),
            truncated: false,
            skip: 0,
            has_nested: None,
        }
    }

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

/// Writes a client Data packet carrying `Block::empty()` at revision 54460.
/// Confirmed at `v26.8.1.2041-lts`, `src/Server/TCPHandler.cpp`,
/// `TCPHandler::processData` treats any zero-column block as end-of-upload.
fn put_empty_data_packet(output: &mut Vec<u8>) {
    put_varuint(output, CLIENT_PACKET_DATA);
    put_string(output, "");
    // BlockInfo field 1 = overflow false, field 2 = bucket -1, terminator 0,
    // then zero columns and zero rows.
    output.extend_from_slice(&[1, 0, 2]);
    output.extend_from_slice(&(-1_i32).to_le_bytes());
    output.extend_from_slice(&[0, 0, 0]);
}

fn escaped_content_len(value: &[u8]) -> Option<usize> {
    value.iter().try_fold(0_usize, |length, byte| {
        let added = match byte {
            b'\'' | b'\\' | b'\0' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
            _ => 1,
        };
        length.checked_add(added)
    })
}

/// Encoded size of `value` as a single-quoted, backslash-escaped literal.
fn quoted_literal_len(value: &[u8]) -> Option<usize> {
    escaped_content_len(value)?.checked_add(2)
}

/// Writes a query parameter value as one length-prefixed String Field dump.
///
/// This is exactly one quoting layer, the inverse of the server's
/// `readQuoted` in `readQueryParameters` (`src/Core/Settings.cpp`, at
/// v26.8.1.2041-lts). The unquoted result is the parameter's text, which the
/// server then parses as escaped text for the declared type. See
/// [`Query::with_parameter`].
fn put_quoted_string(output: &mut Vec<u8>, value: &str) -> Result<(), ProtocolError> {
    let length =
        quoted_literal_len(value.as_bytes()).ok_or(ProtocolError::QueryOutputTooLarge {
            length: usize::MAX,
            limit: QUERY_OUTPUT_LIMIT,
        })?;
    put_varuint(output, length as u64);
    put_quoted_literal(output, value.as_bytes());
    Ok(())
}

fn put_quoted_literal(output: &mut Vec<u8>, value: &[u8]) {
    output.push(b'\'');
    put_escaped_content(output, value);
    output.push(b'\'');
}

fn put_escaped_content(output: &mut Vec<u8>, value: &[u8]) {
    for &byte in value {
        match byte {
            b'\'' => output.extend_from_slice(b"\\'"),
            b'\\' => output.extend_from_slice(b"\\\\"),
            b'\0' => output.extend_from_slice(b"\\0"),
            b'\n' => output.extend_from_slice(b"\\n"),
            b'\r' => output.extend_from_slice(b"\\r"),
            b'\t' => output.extend_from_slice(b"\\t"),
            0x08 => output.extend_from_slice(b"\\b"),
            0x0c => output.extend_from_slice(b"\\f"),
            _ => output.push(byte),
        }
    }
}

/// Exact encoded length of the Query packet and its empty Data block, in the
/// order `Connection::query` writes them. `None` on `usize` overflow.
fn query_packet_len(query: &Query, identity: &ClientIdentity, quota_key: &str) -> Option<usize> {
    // Fixed-width fields: packet code, query kind, start time, interface,
    // the OpenTelemetry flag, the five zero VarUInts after the quota key, the
    // two empty strings, stage and compression after the settings, the
    // empty parameter terminator, Data packet code, empty table name, and
    // the 10-byte empty block. Every one of these encodes in the bytes
    // counted here.
    let fixed = varuint_len(CLIENT_PACKET_QUERY)
        + 1 // ClientInfo::QueryKind::INITIAL_QUERY
        + 8 // initial_query_start_time_microseconds
        + 1 // ClientInfo::Interface::TCP
        + 5 // distributed_depth .. number_of_current_replica, all zero
        + 1 // OpenTelemetry flag
        + string_len("") // settings terminator
        + string_len("") // interserver secret hash
        + varuint_len(2) // QueryProcessingStage::Complete
        + varuint_len(0) // uncompressed
        + string_len("") // parameters terminator
        + varuint_len(CLIENT_PACKET_DATA)
        + string_len("") // external table name
        + 10; // BlockInfo, zero columns, zero rows
    let mut length = fixed
        .checked_add(varuint_len(identity.version_major))?
        .checked_add(varuint_len(identity.version_minor))?
        .checked_add(varuint_len(ENGINE_REVISION))?;
    for value in [
        query.query_id.as_str(),
        identity.user.as_str(),
        query.query_id.as_str(),
        query.initial_address.as_str(),
        query.os_user.as_str(),
        query.hostname.as_str(),
        identity.client_name.as_str(),
        quota_key,
        query.sql.as_str(),
    ] {
        length = length.checked_add(string_len(value))?;
    }
    for (name, value) in &query.settings {
        length = length
            .checked_add(string_len(name))?
            .checked_add(varuint_len(1))? // SettingFlag::IMPORTANT
            .checked_add(string_len(value))?;
    }
    for (name, value) in &query.parameters {
        // The same single quoting layer `put_quoted_string` writes.
        let quoted = quoted_literal_len(value.as_bytes())?;
        length = length
            .checked_add(string_len(name))?
            .checked_add(varuint_len(2))? // SettingFlag::CUSTOM
            .checked_add(varuint_len(quoted as u64))?
            .checked_add(quoted)?;
    }
    Some(length)
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
