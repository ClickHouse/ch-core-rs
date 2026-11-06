# ch-tcp-proto

`ch-tcp-proto` is the sans-I/O ClickHouse TCP native protocol engine for
`ch-core-rs`. The engine owns packet framing and revision gates. Drivers own
sockets, TLS, timeouts, and connection pooling. The engine accepts arbitrary
byte chunks, queues protocol events, and queues ordered bytes to send.

The initial scope is deliberately small: Client Hello, Server Hello, the client
addendum, Ping, Pong, and handshake-phase Exception packets at protocol
revision 54460. Query and Data packets are not implemented yet. Data blocks are
intended to be decoded through core's `StreamDecoder` rather than by growing
this engine's input buffer, and a `ProtocolError` variant wrapping core's
`DecodeError` will be added with them (`ProtocolError` is `non_exhaustive`).

## Driver loop

Write queued output before each read, feed whatever the read returned, then
drain every queued event before looking at the `feed` result. Events completed
before an error in the same chunk stay queued, so draining first never loses
them. Reads may return any number of bytes; the engine reassembles packets
across chunk boundaries. A Pong is success only once `feed` also succeeded: a
malformed packet after it in the same chunk still fails the connection. Retry
reads interrupted by a signal; `write_all` already does. Build the
`ClientHello` with the driver's own name and version, for example
`ClientHello::new("example-client", "default").with_client_version(1, 0)`.

`tests/handshake_replay.rs` compiles this exact function from
`tests/support/readme_connect.rs` and fails if the two differ.

```rust
use std::io::{self, Read, Write};

use ch_tcp_proto::{ClientHello, Connection, Event, State};

/// Runs the handshake and one Ping over a blocking byte stream.
fn connect<S: Read + Write>(
    socket: &mut S,
    hello: ClientHello,
) -> Result<Connection, Box<dyn std::error::Error>> {
    let mut connection = Connection::new(hello)?;
    // The Client Hello, which holds the password until taken.
    socket.write_all(&connection.take_output())?;

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
            connection.ping()?;
            pinged = true;
        }
        // After the Server Hello: the addendum, then the Ping.
        socket.write_all(&connection.take_output())?;
    }
}
```

## Contract

- `feed` returns only errors. Every completed packet is an `Event` read with
  `poll_event`. A malformed or unexpected packet fails the connection with
  `State::Failed(FailureKind::Protocol)`.
- A server Exception is an event. It is terminal during the handshake and
  pre-query phase, which is all this crate implements:
  `State::Failed(FailureKind::ServerException)`. At `v26.8.1.2041-lts` the
  server sends at most one Exception from its handshake and then closes the
  connection (`TCPHandler::runImpl` handshake catch, `TCPHandler::run`).
  Query-time exceptions are not terminal at that tag: `runImpl` sends the
  Exception and keeps the connection for the next query, apart from
  connection-level errors. They will be modeled when Query support lands.
- Bytes after a handshake-phase Exception are counted, never parsed, and `feed`
  keeps returning `Ok`. `finish` then returns `TrailingInput` if any bytes
  followed, otherwise `Ok`. The result does not depend on how the stream was
  split into chunks.
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
keep from firing. An input buffer grown past 64 KiB is shrunk once it drains.
The Exception bounds are engine policy because the server wire reader has no
explicit Exception string cap.

## Server reference

The wire layout is confirmed against ClickHouse `v26.8.1.2041-lts` in
`TCPHandler::{receiveHello,sendHello,receiveAddendum,sendException}`,
`Client::Connection::{sendHello,receiveHello,sendAddendum,receiveException}`,
`Core/Protocol.h`, `Core/ProtocolDefines.h`, and the exception helpers in
`IO/{WriteHelpers,ReadHelpers}`. Applying `min(54460, server revision)` to
every Server Hello field gate is inferred, not confirmed: the server gates on
the client-declared revision while stock clients gate on the server revision.
