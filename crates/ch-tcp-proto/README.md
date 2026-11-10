# ch-tcp-proto

`ch-tcp-proto` is the sans-I/O ClickHouse TCP native protocol engine for
`ch-core-rs`. The engine owns packet framing and revision gates. Drivers own
sockets, TLS, timeouts, cancellation, and connection pooling. The engine
accepts arbitrary byte chunks, queues protocol events, and queues ordered bytes
to send.

The initial scope is deliberately small: handshake, Ping/Pong, and uncompressed
initial SELECT queries at protocol revision 54460. SELECT results emit complete
Native block bytes plus typed Progress, ProfileInfo, ProfileEvents, Log, Totals,
Extremes, Exception, and EndOfStream events. INSERT, cancellation, external
tables, and compressed TCP packets are not implemented yet.

## Driver loop

The whole contract is: feed any amount, drain `poll_event` until it returns
`None`, repeat. `feed` parses every byte it is given before returning, so a
read of any size is fine, from one byte to a whole result, and there is no
resume call. The event queue has no bound of its own: its memory is
proportional to the bytes fed since the last drain, which the driver controls
through its read size. Drain after every feed.

Write queued output before each read, feed whatever the read returned, then
drain every queued event before looking at the `feed` result. Events completed
before an error in the same chunk stay queued, so draining first never loses
them. A Pong is success only once `feed` also succeeded: a malformed packet
after it in the same chunk still fails the connection. Retry reads interrupted
by a signal; `write_all` already does. Flush after each write so a buffered
stream never holds a request while `read` waits for its reply. Build the
`ClientHello` with the driver's own name and version, for example
`ClientHello::new("example-client", "default").with_client_version(1, 0)`.

`Event`, `State`, `FailureKind`, and `ProtocolError` are `#[non_exhaustive]`,
as are the event payload structs, so matches need a wildcard arm and new
variants are not breaking changes.

`tests/handshake_replay.rs` compiles this exact function from
`tests/support/readme_connect.rs` and fails if the two differ.

```rust
use std::io::{self, Read, Write};

use ch_tcp_proto::{ClientHello, Connection, Event, ProtocolError, State};

/// Runs the handshake and one Ping over a blocking byte stream.
fn connect<S: Read + Write>(
    socket: &mut S,
    hello: ClientHello,
) -> Result<Connection, Box<dyn std::error::Error>> {
    let mut connection = Connection::new(hello)?;
    // The Client Hello, which holds the password until taken. Flush so a
    // buffered stream does not hold it while `read` waits for the reply.
    socket.write_all(&connection.take_output())?;
    socket.flush()?;

    let mut buffer = [0_u8; 8192];
    let mut pinged = false;
    loop {
        let read = match socket.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        let fed = if read == 0 {
            connection.finish()
        } else {
            connection.feed(&buffer[..read])
        };
        // Drain every event before acting on the Pong or on `fed`.
        let mut got_pong = false;
        while let Some(event) = connection.poll_event() {
            match event {
                Event::ServerHello(hello) => println!("connected to {}", hello.server_name),
                Event::Pong => got_pong = true,
                // Terminal during the handshake: no event follows it.
                Event::Exception(exception) => {
                    let message = format!("server error {}: {}", exception.code, exception.message);
                    return Err(message.into());
                }
                _ => {}
            }
        }
        // A packet after the Pong in the same chunk may have failed the
        // connection, so check `fed` before reporting success.
        fed?;
        if got_pong {
            return Ok(connection);
        }
        // EOF queues no events, so it is never mistaken for a Pong.
        if read == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
        if connection.state() == State::Ready && !pinged {
            // In `Ready`, `InvalidState` means a server packet is partly
            // received, so retry after the next read. A conforming server
            // sends nothing after its Hello until it has read the addendum,
            // so this only guards against non-conforming peers.
            match connection.ping() {
                Ok(()) => pinged = true,
                Err(ProtocolError::InvalidState { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        // After the Server Hello: the addendum, then the Ping once sent.
        socket.write_all(&connection.take_output())?;
        socket.flush()?;
    }
}
```

A SELECT on that connection follows the same loop. `run_select` returns the
concatenated `Event::Data` payloads, which form one Native stream, and a typed
error that tells I/O and protocol failures, after which the connection must be
discarded, from a server Exception, after which it may be reusable.
`tests/query_replay.rs` runs this exact function from
`tests/support/readme_run_select.rs` against the recorded captures with random
read sizes, including one-byte reads and one read of a whole response, and
decodes the result.

```rust
use std::io::{self, Read, Write};

use ch_tcp_proto::{Connection, Event, ProtocolError, Query, ServerException};

/// Why `run_select` failed. After `Server` the connection is reusable if
/// `connection.state()` is `State::Ready`; after `Io` or `Protocol`, discard
/// it with its socket.
#[derive(Debug)]
enum SelectError {
    Io(io::Error),
    Protocol(ProtocolError),
    Server(Box<ServerException>),
}

impl std::fmt::Display for SelectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Protocol(error) => write!(f, "protocol error: {error}"),
            Self::Server(error) => write!(f, "server error {}: {}", error.code, error.message),
        }
    }
}

impl std::error::Error for SelectError {}

impl From<io::Error> for SelectError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ProtocolError> for SelectError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

/// Runs one SELECT on a connection from `connect` and returns the result as
/// one Native byte stream: the Data payloads concatenated in order.
fn run_select<S: Read + Write>(
    socket: &mut S,
    connection: &mut Connection,
    query: Query,
) -> Result<Vec<u8>, SelectError> {
    connection.query(query)?;
    // Flush so a buffered stream does not hold the Query while `read` waits.
    socket.write_all(&connection.take_output())?;
    socket.flush()?;

    let mut native = Vec::new();
    let mut read_rows = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = match socket.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        // EOF before EndOfStream is truncation, reported by `finish`.
        let fed = if read == 0 {
            connection.finish()
        } else {
            connection.feed(&buffer[..read])
        };
        // Drain every event before acting on `fed`. Queued events hold
        // memory proportional to the bytes fed since the last drain.
        let mut done = false;
        let mut failure = None;
        while let Some(event) = connection.poll_event() {
            match event {
                Event::Data(block) => native.extend_from_slice(&block),
                // Progress packets carry increments, not running totals.
                Event::Progress(progress) => read_rows += progress.read_rows,
                Event::ProfileInfo(info) => println!("{} result rows", info.rows),
                // The query failed and the exchange is over.
                Event::Exception(exception) => failure = Some(exception),
                Event::EndOfStream => done = true,
                // Totals, Extremes, Log, and ProfileEvents. HTTP FORMAT
                // Native drops Totals and Extremes, so ignoring them matches
                // HTTP results. `Event` is non-exhaustive, so a wildcard arm
                // is required anyway.
                _ => {}
            }
        }
        // A malformed packet after the Exception in the same chunk failed
        // the connection, so it is not reusable: report that first.
        fed?;
        if let Some(exception) = failure {
            return Err(SelectError::Server(exception));
        }
        if done {
            println!("read {read_rows} rows");
            return Ok(native);
        }
        if read == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
    }
}
```

### Decoding results

When a TCP SELECT has a non-empty result header, it starts with a zero-row Data
block that carries only that schema; the rows follow in later blocks. A query
with an empty header need not send a schema block. HTTP `FORMAT Native` sends
no leading schema-only block. Core's `StreamDecoder` and `decode_next_block`
return a schema-only block as a zero-row `ch_core_rs::batch::ColBatch`, so
bindings must accept zero-row batches and can use one to learn the schema of
an empty result.

Either feed the concatenated payloads to `StreamDecoder`, or decode each
payload as it arrives. The per-payload form is preferred: `StreamDecoder`
scans for block boundaries again, while the engine has already found them.
Set `DecodeOptions::protocol_revision` to `Connection::effective_revision()`
in both cases. `tests/query_replay.rs` runs this function on every capture.

```rust
use std::io;

use ch_core_rs::batch::ColBatch;
use ch_core_rs::native::decode::{decode_next_block, DecodeError, DecodeOptions};
use ch_core_rs::native::varint::ByteReader;
use ch_tcp_proto::Connection;

/// Decodes one `Event::Data` payload, which is exactly one Native block, as
/// soon as it arrives. A zero-row block still carries the result schema.
fn decode_payload(payload: &[u8], connection: &Connection) -> Result<ColBatch, DecodeError> {
    let mut options = DecodeOptions::default();
    // Known once the Server Hello is parsed, which `connect` guarantees.
    options.protocol_revision = connection.effective_revision().unwrap_or(0);
    let mut reader = ByteReader::new(payload);
    match decode_next_block(&mut reader, &options)? {
        // The engine never emits an empty payload, and the block fills it.
        Some(batch) if reader.remaining() == 0 => Ok(batch),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "not one Native block").into()),
    }
}
```

### Health check

A pooled connection can be checked with one Ping while it is `Ready` and
drained. `tests/query_replay.rs` runs this function too.

```rust
use std::io::{self, Read, Write};

use ch_tcp_proto::{Connection, Event};

/// Sends one Ping on an idle `Ready` connection and waits for its Pong, for
/// example before reusing a pooled connection.
fn health_check<S: Read + Write>(
    socket: &mut S,
    connection: &mut Connection,
) -> Result<(), Box<dyn std::error::Error>> {
    connection.ping()?;
    socket.write_all(&connection.take_output())?;
    socket.flush()?;
    let mut buffer = [0_u8; 1024];
    loop {
        let read = match socket.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        let fed = if read == 0 {
            connection.finish()
        } else {
            connection.feed(&buffer[..read])
        };
        let mut got_pong = false;
        let mut failure = None;
        while let Some(event) = connection.poll_event() {
            match event {
                Event::Pong => got_pong = true,
                // Terminal outside a query: the server is closing.
                Event::Exception(exception) => failure = Some(exception),
                _ => {}
            }
        }
        fed?;
        if let Some(exception) = failure {
            let message = format!("server error {}: {}", exception.code, exception.message);
            return Err(message.into());
        }
        if got_pong {
            return Ok(());
        }
        if read == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
    }
}
```

## Contract

- `feed` returns only errors. Every completed packet is an `Event` read with
  `poll_event`. A malformed or unexpected packet fails the connection with
  `State::Failed(FailureKind::Protocol)`, and a later `finish` returns that
  same error.
- `feed` consumes its whole input. The only bytes it retains between calls are
  the tail of one incomplete scalar field; Native block bodies go straight to
  their event buffer. On a live connection `feed(&[])` is a no-op.
- `query` and `ping` are rejected with `InvalidState` until every queued event
  is drained, and while any server packet is partly received. So a Query is
  never queued behind an undelivered Exception that ends the connection.
- A server Exception is an event and the last of its exchange: no EndOfStream,
  Log, or ProfileEvents follows it. It is terminal during the handshake and
  pre-query phase: `State::Failed(FailureKind::ServerException)`. At
  `v26.8.1.2041-lts` (`TCPHandler::runImpl`):
  - The handshake catch sends at most one Exception and the connection closes.
  - `ClientInfo` and wire settings are read before the query context exists.
    An Exception there is the first query response and the connection closes.
    Because the packet does not expose the throw site, a first-packet
    Exception for a query that sent settings fails closed. This covers bad
    values for known settings as well as unknown names. A later Exception is
    reusable unless its code is independently terminal. A first-packet SQL
    syntax error remains reusable when no wire settings were sent.
  - The conservative rule also marks a first-packet syntax error terminal when
    that query sent wire settings, even though the server can keep the socket
    open. The packet does not reveal whether the throw happened while reading
    settings or after context creation. When the engine reports `Failed`, the
    driver must close its socket itself rather than wait for server EOF. Where
    a binding can safely construct one, a SQL `SETTINGS` clause avoids setting
    the wire-settings flag and therefore avoids this false positive.
  - Codes 101, 702, 720, and 749 are sent and the connection closes. Code 99
    closes without being sent; it is listed defensively.
  - Once a query context exists, other query errors normally leave the
    connection reusable. The wire does not identify every server throw site,
    so a driver must still honor transport EOF and I/O failures.
- There is no Cancel packet and no reset. A driver that abandons a query before
  its EndOfStream or Exception, for example on a timeout, must discard the
  `Connection` and its socket rather than reuse them.
- `query` queues a revision-54460 Query packet followed by an empty client Data
  block. It accepts string settings and parameter values. See
  `Query::with_parameter` for how parameter values are encoded. Setting names
  are capped at 64 KiB (`StringTooLong`) as an engine policy that independently
  bounds user-controlled identifiers. Setting values and parameter names and
  values have no per-field limit: the server reads them with a 1 GiB string
  limit, and the engine bounds only the whole packet, at exactly 64 MiB
  (`QueryOutputTooLarge` reports the packet's true length).
- Every `Query::with_setting` is sent with `IMPORTANT`, so invalid names or
  values fail instead of being silently ignored. A binding should validate a
  forwarded settings map where practical.
- `Query` serializes `initial_address` as `0.0.0.0:0` unless
  `Query::with_initial_address` sets it. For an initial query, the server
  overwrites that field with the connection's real peer address before it is
  recorded in `system.query_log`; the setter does not spoof the logged peer.
- At revision 54460 the server never echoes the query ID. A binding that needs
  one, to correlate logs or `system.query_log`, must generate it and pass it
  with `Query::with_query_id`.
- Query support is limited to statements whose complete response uses the
  initial SELECT result exchange. INSERT, input(), external tables,
  parallel-replica exchanges, cancellation, and compressed packets are not
  supported. The engine does not parse SQL to classify statements.
- The engine frames result blocks with core's scanner, so a result column type
  core cannot decode fails the whole connection with `ProtocolError::Decode`.
- `output_format_native_encode_types_in_binary_format` accepts only `"0"` in
  explicit settings. This guard is best-effort: a SQL `SETTINGS` clause, a user
  profile, or an earlier `SET` can still enable it, and the engine then fails
  on the first block with `ProtocolError::Decode`. Keep it disabled
  server-side.
- Each server Data packet emits `Event::Data` with exactly one complete Native
  block. Concatenating Data payloads produces a stream accepted by core's
  `StreamDecoder` at `effective_revision()`. The zero-column, zero-row TCP end
  marker is filtered because appending it would violate the result schema. A
  real zero-row block, including the leading schema block, retains its column
  headers and is emitted.
- `Event::Totals` and `Event::Extremes` can be ignored for parity with HTTP. At
  `v26.8.1.2041-lts` HTTP `FORMAT Native` drops totals and extremes
  (`IOutputFormat::consumeTotals` is a no-op that `NativeOutputFormat` does not
  override). This is inferred from source, not live-probed.
- `Progress` values are increments since the previous Progress packet, not
  running totals (`TCPHandler::sendProgress`).
- `ServerHello::timezone` is the server timezone, the TCP counterpart of HTTP's
  `X-ClickHouse-Timezone` header. Use it for `DateTime` columns that declare
  no timezone.
- Bytes after any terminal Exception are counted, never parsed, and `feed`
  keeps returning `Ok`. `finish` then returns `TrailingInput` if any bytes
  followed, otherwise `Ok`. This includes handshake, pre-query, and terminal
  query Exceptions, and does not depend on how the stream was split into
  chunks.
- `ping` is rejected with `InvalidState` while any server packet is partly
  received, even though `state()` still reports `Ready`. Otherwise a Pong
  whose packet code straddled the `ping` call would be accepted as its reply,
  although the same bytes in one chunk are an `UnexpectedPacket`.
- A too-long client `password` or `quota_key` is reported as `StringTooLong`
  with `length: None`, so the error does not disclose the secret's length.
- `finish` marks transport EOF. It closes a `Ready` connection and reports
  truncation in any other live state or mid-packet. Its first outcome is
  stored and returned again by later calls.
- Exception text is decoded with lossy UTF-8, so a non-UTF-8 message never
  costs the error code. Server Hello strings must be valid UTF-8.

## Limits

Client Hello and addendum strings are limited to 64 KiB each, matching the
server. Server Hello strings are limited to 4096 bytes, matching the stock
client. Exception strings longer than 1 MiB keep their first 1 MiB of wire
bytes, the rest is skipped without buffering, and `ServerException::truncated`
is set. A declared Exception string length above 64 MiB is a protocol error
(`StringTooLong`), so a corrupt length cannot make the engine skip forever.

Lossy UTF-8 decoding turns each invalid byte into U+FFFD, which is 3 bytes, so
each decoded Exception field is at most 3 MiB, 9 MiB for the three text fields.
Bytes retained across `feed` calls are bounded by the per-field caps, at most
about 1 MiB while an Exception field straddles a chunk boundary. The 4 MiB
retained-input limit (`InputTooLarge`) is a backstop that the per-field caps
keep from firing; it does not depend on how much is passed to one `feed`. An
input buffer grown past 64 KiB is shrunk once it drains. The Exception bounds
are engine policy because the server wire reader has no explicit Exception
string cap.

Each queued event costs `size_of::<Event>()`, at most 56 bytes on 64-bit
targets, plus its own payload allocation. `ServerHello` and `ServerException`
are boxed to keep that small.

One Native packet body is capped at `DEFAULT_MAX_BLOCK_BYTES`, 1 GiB, by
default. Set another cap with `Connection::with_max_block_bytes`; values below
`MIN_MAX_BLOCK_BYTES`, 1 KiB, are raised to it. The cap applies to every
block-carrying packet: Data, including the end-of-data marker, Totals,
Extremes, Log (`send_logs_level`), and ProfileEvents. HTTP has no per-block
cap, so a low value can reject results HTTP returns: `SELECT
randomString(1100) FROM numbers(100000)` produces blocks of about 72 MB. Block
memory grows only with bytes actually received, never from a declared length,
and the block buffer's capacity never exceeds the cap. Core's scan-only
boundary parser finds the block's exact end without allocating decoded
columns, and malformed bodies surface as `ProtocolError::Decode`.

Framing retains progress through column bodies and runtime type-table
prefixes, including Dynamic, JSON, AggregateFunction, and nested containers.
Scanning stays linear across transport read sizes.

## Server reference

The wire layout is confirmed against ClickHouse `v26.8.1.2041-lts` in
`TCPHandler::{receiveHello,sendHello,receiveAddendum,receiveQuery,sendData,sendTotals,sendExtremes}`,
`Client::Connection::{sendHello,receiveHello,sendAddendum,sendQuery}`,
`ClientInfo::{write,read}`, `ProgressValues::write`, `ProfileInfo::write`,
`BaseSettingsHelpers::writeFlags`, `Settings::{writeQueryParameters,readQueryParameters}`,
`SettingFieldCustom::toString`,
`ReplaceQueryParameterVisitor::resolveParameterValueAsField`,
`SerializationString::{serializeTextEscaped,deserializeTextEscaped}`,
`Core/Protocol.h`, `Core/ProtocolDefines.h`, and the exception helpers in
`IO/{WriteHelpers,ReadHelpers}`. Applying `min(54460, server revision)` to every
Server Hello field gate is inferred, not confirmed: the server gates on the
client-declared revision while stock clients gate on the server revision.
