//! Replay real server captures without opening sockets.
//!
//! Assertions avoid values that change on recapture, such as the server
//! display name (the capture host name) and the exact stack trace.

use std::io::{self, Read, Write};

use ch_tcp_proto::{
    ClientHello, Connection, Event, FailureKind, ProtocolError, ServerException, ServerHello, State,
};

const CLIENT_HELLO: &[u8] =
    include_bytes!("../../../tests/fixtures/tcp_handshake_client_hello_rev54460.packet");
const SERVER_HELLO: &[u8] =
    include_bytes!("../../../tests/fixtures/tcp_handshake_server_hello_rev54460.packet");
const ADDENDUM: &[u8] =
    include_bytes!("../../../tests/fixtures/tcp_handshake_addendum_rev54460.bytes");
const PING: &[u8] = include_bytes!("../../../tests/fixtures/tcp_ping_client_rev54460.packet");
const PONG: &[u8] = include_bytes!("../../../tests/fixtures/tcp_ping_server_rev54460.packet");
const FAILED_CLIENT_HELLO: &[u8] =
    include_bytes!("../../../tests/fixtures/tcp_handshake_exception_client_rev54460.packet");
const AUTH_EXCEPTION: &[u8] =
    include_bytes!("../../../tests/fixtures/tcp_handshake_exception_server_rev54460.packet");

const MISSING_USER: &str = "ch_core_handshake_fixture_missing_user";
const EXCEPTION_FAILED: State = State::Failed(FailureKind::ServerException);

fn config(user: &str) -> ClientHello {
    ClientHello::new("ch-core-handshake-fixture", user).with_client_version(26, 8)
}

fn client() -> Connection {
    Connection::new(config("default")).unwrap()
}

fn events(connection: &mut Connection) -> Vec<Event> {
    std::iter::from_fn(|| connection.poll_event()).collect()
}

fn assert_captured_hello(hello: &ServerHello) {
    assert_eq!(hello.server_name, "ClickHouse");
    assert_eq!((hello.version_major, hello.version_minor), (26, 8));
    assert_eq!(hello.version_patch, 1);
    assert_eq!(hello.revision, 54_492);
    assert_eq!(hello.effective_revision, 54_460);
    assert_eq!(hello.timezone, "UTC");
    assert!(!hello.display_name.is_empty());
}

fn assert_captured_exception(exception: &ServerException) {
    assert_eq!(exception.code, 516);
    assert_eq!(exception.name, "DB::Exception");
    assert!(exception.message.contains("Authentication failed"));
    assert!(exception.message.contains(MISSING_USER));
    assert!(exception.stack_trace.contains("receiveHello"));
    assert!(!exception.has_nested);
    assert!(!exception.truncated);
}

#[test]
fn real_hello_addendum_and_ping_match_the_capture() {
    let mut connection = client();
    assert_eq!(connection.state(), State::Handshaking);
    assert_eq!(connection.take_output(), CLIENT_HELLO);
    assert!(connection.take_output().is_empty());
    connection.feed(SERVER_HELLO).unwrap();
    let seen = events(&mut connection);
    let [Event::ServerHello(hello)] = seen.as_slice() else {
        panic!("expected captured Server Hello");
    };
    assert_captured_hello(hello);
    assert_eq!(connection.state(), State::Ready);
    assert_eq!(connection.take_output(), ADDENDUM);
    connection.ping().unwrap();
    assert_eq!(connection.state(), State::AwaitingPong);
    assert_eq!(connection.take_output(), PING);
    connection.feed(PONG).unwrap();
    assert_eq!(events(&mut connection), [Event::Pong]);
    assert_eq!(connection.state(), State::Ready);
    assert_eq!(connection.finish(), Ok(()));
    assert_eq!(connection.state(), State::Closed);
}

#[test]
fn every_server_hello_split_preserves_the_packet() {
    for split in 0..=SERVER_HELLO.len() {
        let mut connection = client();
        connection.take_output();
        connection.feed(&SERVER_HELLO[..split]).unwrap();
        connection.feed(&SERVER_HELLO[split..]).unwrap();
        let seen = events(&mut connection);
        let [Event::ServerHello(hello)] = seen.as_slice() else {
            panic!("expected captured Server Hello at split {split}");
        };
        assert_captured_hello(hello);
        assert_eq!(connection.state(), State::Ready, "split {split}");
        assert_eq!(connection.take_output(), ADDENDUM, "split {split}");
    }
}

#[test]
fn captured_authentication_failure_is_terminal_at_every_split() {
    for split in 0..AUTH_EXCEPTION.len() {
        let mut connection = Connection::new(config(MISSING_USER)).unwrap();
        assert_eq!(connection.take_output(), FAILED_CLIENT_HELLO);
        connection.feed(&AUTH_EXCEPTION[..split]).unwrap();
        assert!(connection.poll_event().is_none(), "split {split}");
        connection.feed(&AUTH_EXCEPTION[split..]).unwrap();
        let seen = events(&mut connection);
        let [Event::Exception(exception)] = seen.as_slice() else {
            panic!("expected captured authentication Exception at split {split}");
        };
        assert_captured_exception(exception);
        assert_eq!(connection.state(), EXCEPTION_FAILED, "split {split}");
        assert!(connection.take_output().is_empty(), "split {split}");
        assert!(connection.ping().is_err(), "split {split}");
        assert_eq!(connection.finish(), Ok(()), "split {split}");
    }
}

#[test]
fn captured_packets_decode_one_byte_at_a_time() {
    let mut connection = client();
    connection.take_output();
    for byte in SERVER_HELLO {
        connection.feed(std::slice::from_ref(byte)).unwrap();
    }
    let seen = events(&mut connection);
    let [Event::ServerHello(hello)] = seen.as_slice() else {
        panic!("expected captured Server Hello");
    };
    assert_captured_hello(hello);
    assert_eq!(connection.state(), State::Ready);
    assert_eq!(connection.take_output(), ADDENDUM);

    let mut connection = Connection::new(config(MISSING_USER)).unwrap();
    connection.take_output();
    for byte in AUTH_EXCEPTION {
        connection.feed(std::slice::from_ref(byte)).unwrap();
    }
    let seen = events(&mut connection);
    let [Event::Exception(exception)] = seen.as_slice() else {
        panic!("expected captured authentication Exception");
    };
    assert_captured_exception(exception);
    assert_eq!(connection.state(), EXCEPTION_FAILED);
    assert_eq!(connection.finish(), Ok(()));
}

#[test]
fn every_truncated_captured_packet_fails_on_eof() {
    for packet in [SERVER_HELLO, AUTH_EXCEPTION] {
        for end in 0..packet.len() {
            let mut connection = client();
            connection.take_output();
            connection.feed(&packet[..end]).unwrap();
            assert!(connection.poll_event().is_none(), "prefix length {end}");
            assert_eq!(
                connection.finish(),
                Err(ProtocolError::UnexpectedEof),
                "prefix length {end}"
            );
            assert_eq!(
                connection.state(),
                State::Failed(FailureKind::Protocol),
                "prefix length {end}"
            );
        }
    }
}

/// Everything observable after replaying `segments`, each split into chunks
/// by `chunk_size`, with a Ping queued between segments.
#[derive(Debug, PartialEq)]
struct Replay {
    feed: Result<(), ProtocolError>,
    events: Vec<Event>,
    output: Vec<u8>,
    state_before_finish: State,
    finish: Result<(), ProtocolError>,
    state_after_finish: State,
}

fn replay(user: &str, segments: &[&[u8]], mut chunk_size: impl FnMut() -> usize) -> Replay {
    let mut connection = Connection::new(config(user)).unwrap();
    let mut output = connection.take_output();
    let mut feed = Ok(());
    'segments: for (index, segment) in segments.iter().enumerate() {
        if index > 0 {
            connection.ping().unwrap();
        }
        let mut rest = *segment;
        while !rest.is_empty() {
            let (chunk, tail) = rest.split_at(chunk_size().clamp(1, rest.len()));
            rest = tail;
            if let Err(error) = connection.feed(chunk) {
                feed = Err(error);
                break 'segments;
            }
        }
        output.extend(connection.take_output());
    }
    let state_before_finish = connection.state();
    let finish = connection.finish();
    Replay {
        feed,
        events: events(&mut connection),
        output,
        state_before_finish,
        finish,
        state_after_finish: connection.state(),
    }
}

/// Every captured server stream gives the same outcome for many seeded
/// pseudo-random chunkings as it does fed whole.
#[test]
fn seeded_random_chunking_is_split_independent_for_every_fixture_stream() {
    let exception_then_hello = [AUTH_EXCEPTION, SERVER_HELLO].concat();
    let streams: [(&str, Vec<&[u8]>); 4] = [
        ("default", vec![SERVER_HELLO]),
        ("default", vec![SERVER_HELLO, PONG]),
        (MISSING_USER, vec![AUTH_EXCEPTION]),
        // Captured bytes after the Exception are counted as trailing input.
        (MISSING_USER, vec![&exception_then_hello]),
    ];
    for (user, segments) in &streams {
        let whole = replay(user, segments, || usize::MAX);
        assert_eq!(whole.feed, Ok(()));
        assert!(!whole.events.is_empty());
        for seed in 0_u64..256 {
            // Knuth's MMIX LCG. Chunk sizes range up to 1, 8, or 64 bytes.
            let mut state = seed;
            let max = [1, 8, 64][(seed % 3) as usize];
            let chunk_size = || {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                1 + (state >> 33) as usize % max
            };
            assert_eq!(
                replay(user, segments, chunk_size),
                whole,
                "user {user} seed {seed}"
            );
        }
    }
}

/// An in-memory stream that replays server bytes a few at a time and records
/// what the client wrote. Like a real server, each server segment becomes
/// readable only after the client wrote the bytes it answers. Every other
/// read fails with `Interrupted` first, as a signal would.
///
/// With `buffered`, writes stay in `unflushed` until `flush`, like a
/// `BufWriter`, and only flushed bytes count toward a segment's gate.
struct ReplaySocket {
    /// `(client bytes written before this is readable, server bytes)`.
    segments: Vec<(usize, Vec<u8>)>,
    read_size: usize,
    /// Bytes the server has received.
    written: Vec<u8>,
    interrupt: bool,
    buffered: bool,
    unflushed: Vec<u8>,
}

impl Read for ReplaySocket {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.interrupt = !self.interrupt;
        if self.interrupt {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let Some((gate, segment)) = self.segments.first_mut() else {
            return Ok(0);
        };
        if self.written.len() < *gate {
            // A real server would never answer, so the client would hang.
            assert!(
                self.written.len() + self.unflushed.len() < *gate,
                "client read while its request was buffered but not flushed"
            );
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "client read before writing the request",
            ));
        }
        let read = segment.len().min(self.read_size).min(buffer.len());
        buffer[..read].copy_from_slice(&segment[..read]);
        segment.drain(..read);
        if segment.is_empty() {
            self.segments.remove(0);
        }
        Ok(read)
    }
}

impl Write for ReplaySocket {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.buffered {
            self.unflushed.extend_from_slice(bytes);
        } else {
            self.written.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.written.append(&mut self.unflushed);
        Ok(())
    }
}

// The README `connect` example, compiled here from the same file the README
// is checked against, so the replay tests run that exact code.
include!("support/readme_connect.rs");

/// The README must contain `support/readme_connect.rs` byte for byte, so the
/// example and the code the replay tests run cannot drift apart.
#[test]
fn readme_connect_example_is_the_tested_function() {
    let readme = include_str!("../README.md");
    let tested = include_str!("support/readme_connect.rs");
    assert!(
        readme.contains(tested),
        "README.md `connect` differs from tests/support/readme_connect.rs"
    );
}

fn replay_socket(read_size: usize, segments: Vec<(usize, Vec<u8>)>) -> ReplaySocket {
    ReplaySocket {
        segments,
        read_size,
        written: Vec::new(),
        interrupt: false,
        buffered: false,
        unflushed: Vec::new(),
    }
}

fn capture_segments() -> Vec<(usize, Vec<u8>)> {
    vec![
        (CLIENT_HELLO.len(), SERVER_HELLO.to_vec()),
        (
            CLIENT_HELLO.len() + ADDENDUM.len() + PING.len(),
            PONG.to_vec(),
        ),
    ]
}

const READ_SIZES: [usize; 4] = [1, 3, 7, SERVER_HELLO.len()];

/// (a) The README loop replays the capture with short and interrupted reads.
#[test]
fn readme_connect_replays_the_capture_with_partial_reads() {
    for read_size in READ_SIZES {
        let mut socket = replay_socket(read_size, capture_segments());
        let connection = connect(&mut socket, config("default"))
            .unwrap_or_else(|error| panic!("read size {read_size}: {error}"));
        assert_eq!(connection.state(), State::Ready, "read size {read_size}");
        assert_eq!(
            socket.written,
            [CLIENT_HELLO, ADDENDUM, PING].concat(),
            "read size {read_size}"
        );
    }
}

/// (b) A malformed packet in the same chunk as the Pong fails `connect`
/// rather than returning a failed connection as success.
#[test]
fn readme_connect_fails_on_a_bad_packet_after_the_pong() {
    for read_size in READ_SIZES {
        let mut socket = replay_socket(
            read_size,
            vec![
                (CLIENT_HELLO.len(), SERVER_HELLO.to_vec()),
                (
                    CLIENT_HELLO.len() + ADDENDUM.len() + PING.len(),
                    [PONG, &[79]].concat(),
                ),
            ],
        );
        let result = connect(&mut socket, config("default"));
        if read_size == 1 {
            // The Pong arrives alone and is a genuine success: byte 79 has
            // not been read yet.
            assert_eq!(result.unwrap().state(), State::Ready);
            continue;
        }
        let error = result.expect_err("Pong then packet 79 must fail");
        assert_eq!(
            error.downcast_ref::<ProtocolError>(),
            Some(&ProtocolError::UnexpectedPacket {
                packet: 79,
                state: State::Ready,
            }),
            "read size {read_size}"
        );
    }
}

/// (c) The captured authentication failure is an error carrying its code.
#[test]
fn readme_connect_reports_the_captured_authentication_failure() {
    for read_size in READ_SIZES {
        let mut socket = replay_socket(
            read_size,
            vec![(FAILED_CLIENT_HELLO.len(), AUTH_EXCEPTION.to_vec())],
        );
        let error =
            connect(&mut socket, config(MISSING_USER)).expect_err("authentication must fail");
        let message = error.to_string();
        assert!(message.contains("516"), "read size {read_size}: {message}");
        assert!(message.contains("Authentication failed"), "{message}");
        assert_eq!(socket.written, FAILED_CLIENT_HELLO, "read size {read_size}");
    }
}

/// Copilot on PR #9: over a buffered stream, a request left unflushed while
/// `read` waits for the reply would hang the connection. The socket panics
/// if `connect` reads past a request it buffered but did not flush.
#[test]
fn readme_connect_flushes_every_request_on_a_buffered_stream() {
    for read_size in READ_SIZES {
        let mut socket = replay_socket(read_size, capture_segments());
        socket.buffered = true;
        let connection = connect(&mut socket, config("default"))
            .unwrap_or_else(|error| panic!("read size {read_size}: {error}"));
        assert_eq!(connection.state(), State::Ready, "read size {read_size}");
        assert!(socket.unflushed.is_empty(), "read size {read_size}");
        assert_eq!(
            socket.written,
            [CLIENT_HELLO, ADDENDUM, PING].concat(),
            "read size {read_size}"
        );
    }
}

/// Copilot on PR #9: a non-conforming peer sends Server Hello and the start
/// of an Exception together, before the addendum. A conforming server cannot:
/// at v26.8.1.2041-lts `TCPHandler::runImpl` calls `sendHello` and then
/// blocks in `receiveAddendum`. `connect` must defer the Ping rather than
/// fail, still send the addendum, and report the server's error.
#[test]
fn readme_connect_defers_the_ping_behind_a_partial_exception() {
    const HEAD: usize = 5;
    let first = [SERVER_HELLO, &AUTH_EXCEPTION[..HEAD]].concat();
    let mut deferred = 0;
    for read_size in [1, 3, 7, SERVER_HELLO.len(), first.len(), 8192] {
        let mut socket = replay_socket(
            read_size,
            vec![
                (CLIENT_HELLO.len(), first.clone()),
                (
                    CLIENT_HELLO.len() + ADDENDUM.len(),
                    AUTH_EXCEPTION[HEAD..].to_vec(),
                ),
            ],
        );
        let error = connect(&mut socket, config("default"))
            .expect_err("the server Exception must be reported");
        let message = error.to_string();
        assert!(message.contains("516"), "read size {read_size}: {message}");

        // The Ping goes out only if the read that completed the Hello held
        // no Exception bytes, which happens when it ends on the Hello.
        let pinged = SERVER_HELLO.len() % read_size == 0;
        deferred += usize::from(!pinged);
        let expected = if pinged {
            [CLIENT_HELLO, ADDENDUM, PING].concat()
        } else {
            [CLIENT_HELLO, ADDENDUM].concat()
        };
        assert_eq!(socket.written, expected, "read size {read_size}");
    }
    assert!(deferred > 0, "no read size exercised the deferred Ping");
}
