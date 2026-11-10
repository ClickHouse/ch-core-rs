//! Replay full revision-54460 SELECT exchanges from a real server, without I/O.

use ch_core_rs::column::Column;
use ch_core_rs::native::decode::DecodeOptions;
use ch_core_rs::native::stream_decoder::StreamDecoder;
use ch_tcp_proto::{ClientHello, Connection, Event, Query, State};

const HELLO: &[u8] =
    include_bytes!("../../../tests/fixtures/tcp_handshake_server_hello_rev54460.packet");
/// HTTP `param_` text sent by the escaped_parameter capture.
const ESCAPED_PARAMETER_TEXT: &str =
    "quote\\' slash\\\\ nul\\0 newline\\n carriage\\r tab\\t backspace\\b formfeed\\f";
/// The String value the server returns for `ESCAPED_PARAMETER_TEXT`.
const ESCAPED_PARAMETER: &str =
    "quote' slash\\ nul\0 newline\n carriage\r tab\t backspace\u{8} formfeed\u{c}";
const QUOTA_KEY: &str = "fixture_quota_13";
const INITIAL_QUERY_START_TIME_MICROSECONDS: u64 = 1_791_500_000_000_013;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockKind {
    Data,
    Totals,
    Extremes,
    Log,
    ProfileEvents,
}

#[derive(Clone, Copy, Debug)]
enum ExpectedEvent {
    Block {
        kind: BlockKind,
        packet_offset: usize,
        offset: usize,
        length: usize,
    },
    Progress([u64; 6]),
    ProfileInfo {
        values: [u64; 3],
        applied_limit: bool,
        rows_before_limit: u64,
        obsolete: bool,
    },
    Exception {
        code: i32,
        name: &'static str,
        message: &'static str,
        has_nested: bool,
    },
    EndOfStream,
}

struct CaptureOracle {
    label: &'static str,
    events: &'static [ExpectedEvent],
}

include!("../../../tests/fixtures/tcp_query_oracle_rev54460.rs");

struct Capture {
    label: &'static str,
    sql: &'static str,
    request: &'static [u8],
    response: &'static [u8],
    native: &'static [u8],
}

macro_rules! capture {
    ($label:literal, $sql:expr) => {
        Capture {
            label: $label,
            sql: $sql,
            request: include_bytes!(concat!(
                "../../../tests/fixtures/tcp_query_",
                $label,
                "_client_rev54460.packet"
            )),
            response: include_bytes!(concat!(
                "../../../tests/fixtures/tcp_query_",
                $label,
                "_server_rev54460.packets"
            )),
            native: include_bytes!(concat!(
                "../../../tests/fixtures/tcp_query_",
                $label,
                "_rev54460.native"
            )),
        }
    };
}

const CAPTURES: &[Capture] = &[
    capture!("select", "SELECT 1"),
    capture!(
        "multi",
        "SELECT number + 13 AS n, concat('user_', toString(number)) AS s \
         FROM numbers(5) SETTINGS max_block_size=2, max_threads=1"
    ),
    capture!(
        "exception",
        "SELECT intDiv(toUInt64(79), number != 1000) AS n FROM numbers(2000) \
         SETTINGS max_block_size=128, max_threads=1"
    ),
    capture!(
        "totals",
        "SELECT number % 2 AS k, count() AS n FROM numbers(5) \
         GROUP BY k WITH TOTALS ORDER BY k SETTINGS max_threads=1, extremes=1"
    ),
    capture!("parameter", "SELECT {value:String} AS s"),
    capture!("escaped_parameter", "SELECT {value:String} AS s"),
    capture!("recovery", "SELECT 1"),
    capture!(
        "container_parameter",
        "SELECT {items:Array(String)} AS a, {missing:Nullable(String)} AS n"
    ),
];

/// Looks a capture up by label, so adding a capture never shifts another.
fn capture(label: &str) -> &'static Capture {
    CAPTURES
        .iter()
        .find(|capture| capture.label == label)
        .unwrap_or_else(|| panic!("missing capture {label}"))
}

fn oracle(label: &str) -> &'static CaptureOracle {
    ORACLES
        .iter()
        .find(|oracle| oracle.label == label)
        .unwrap_or_else(|| panic!("missing generated oracle for {label}"))
}

fn ready() -> Connection {
    let hello = ClientHello::new("ch-core-query-fixture", "default")
        .with_client_version(13, 79)
        .with_quota_key(QUOTA_KEY);
    let mut connection = Connection::new(hello).unwrap();
    connection.take_output();
    connection.feed(HELLO).unwrap();
    assert!(matches!(
        connection.poll_event(),
        Some(Event::ServerHello(_))
    ));
    assert_eq!(
        connection.take_output(),
        [QUOTA_KEY.len() as u8]
            .into_iter()
            .chain(QUOTA_KEY.bytes())
            .collect::<Vec<_>>()
    );
    connection
}

/// The Query every capture sent, before any capture-specific parameters.
fn base_query(capture: &Capture) -> Query {
    Query::new(capture.sql)
        .with_query_id(format!("ch-core-fixture-{}", capture.label))
        .with_initial_address("192.0.2.13:79")
        .with_initial_query_start_time_microseconds(INITIAL_QUERY_START_TIME_MICROSECONDS)
        .with_os_user("fixture_os_user_13")
        .with_hostname("fixture_host_79")
        .with_setting("send_profile_events", "1")
        .with_setting("send_logs_level", "debug")
        .with_setting("log_queries", "1")
}

fn start(connection: &mut Connection, capture: &Capture) {
    let mut query = base_query(capture);
    match capture.label {
        "parameter" => query = query.with_parameter("value", "user_1"),
        "escaped_parameter" => query = query.with_parameter("value", ESCAPED_PARAMETER_TEXT),
        "container_parameter" => {
            query = query
                .with_parameter("items", "['user_1','it\\'s']")
                .with_parameter("missing", "\\N")
        }
        _ => {}
    }
    connection.query(query).unwrap();
    assert_eq!(connection.state(), State::Querying);
    assert_eq!(
        connection.take_output(),
        capture.request,
        "{}",
        capture.label
    );
}

/// The whole driver contract after each `feed`: drain until `None`.
fn drain(connection: &mut Connection, events: &mut Vec<Event>) {
    events.extend(std::iter::from_fn(|| connection.poll_event()));
}

fn replay_response(
    capture: &Capture,
    response: &[u8],
    chunks: impl IntoIterator<Item = usize>,
) -> Vec<Event> {
    let mut connection = ready();
    start(&mut connection, capture);
    let mut events = Vec::new();
    let mut rest = response;
    for size in chunks {
        if rest.is_empty() {
            break;
        }
        let end = size.min(rest.len());
        connection.feed(&rest[..end]).unwrap();
        rest = &rest[end..];
        drain(&mut connection, &mut events);
    }
    connection.feed(rest).unwrap();
    drain(&mut connection, &mut events);
    assert_eq!(connection.state(), State::Ready, "{}", capture.label);
    assert_eq!(connection.finish(), Ok(()));
    events
}

fn replay(capture: &Capture, chunks: impl IntoIterator<Item = usize>) -> Vec<Event> {
    replay_response(capture, capture.response, chunks)
}

fn native_bytes(events: &[Event]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for event in events {
        if let Event::Data(block) = event {
            bytes.extend_from_slice(block);
        }
    }
    bytes
}

fn compare_with_oracle(capture: &Capture, events: &[Event]) -> Result<(), String> {
    let expected = oracle(capture.label).events;
    if events.len() != expected.len() {
        return Err(format!(
            "{} event count: got {}, expected {}",
            capture.label,
            events.len(),
            expected.len()
        ));
    }
    for (index, (event, expected)) in events.iter().zip(expected).enumerate() {
        let mismatch = match (event, expected) {
            (
                Event::Progress(progress),
                ExpectedEvent::Progress(
                    [read_rows, read_bytes, total_rows_to_read, written_rows, written_bytes, elapsed_ns],
                ),
            ) => {
                progress.read_rows != *read_rows
                    || progress.read_bytes != *read_bytes
                    || progress.total_rows_to_read != *total_rows_to_read
                    || progress.written_rows != *written_rows
                    || progress.written_bytes != *written_bytes
                    || progress.elapsed_ns != *elapsed_ns
            }
            (
                Event::ProfileInfo(profile),
                ExpectedEvent::ProfileInfo {
                    values,
                    applied_limit,
                    rows_before_limit,
                    obsolete,
                },
            ) => {
                profile.rows != values[0]
                    || profile.blocks != values[1]
                    || profile.bytes != values[2]
                    || profile.applied_limit != *applied_limit
                    || profile.rows_before_limit != *rows_before_limit
                    || profile.unused_obsolete_field != *obsolete
            }
            (
                Event::Exception(error),
                ExpectedEvent::Exception {
                    code,
                    name,
                    message,
                    has_nested,
                },
            ) => {
                error.code != *code
                    || error.name != *name
                    || error.message != *message
                    || error.has_nested != *has_nested
            }
            (Event::EndOfStream, ExpectedEvent::EndOfStream) => false,
            (
                event,
                ExpectedEvent::Block {
                    kind,
                    offset,
                    length,
                    ..
                },
            ) => {
                let actual = match event {
                    Event::Data(bytes) if *kind == BlockKind::Data => Some(bytes),
                    Event::Totals(bytes) if *kind == BlockKind::Totals => Some(bytes),
                    Event::Extremes(bytes) if *kind == BlockKind::Extremes => Some(bytes),
                    Event::Log(bytes) if *kind == BlockKind::Log => Some(bytes),
                    Event::ProfileEvents(bytes) if *kind == BlockKind::ProfileEvents => Some(bytes),
                    _ => None,
                };
                match actual {
                    Some(bytes) => {
                        bytes.as_slice() != &capture.response[*offset..*offset + *length]
                    }
                    None => true,
                }
            }
            _ => true,
        };
        if mismatch {
            return Err(format!(
                "{} event {index}: got {event:?}, expected {expected:?}",
                capture.label
            ));
        }
    }
    Ok(())
}

#[test]
fn real_select_streams_replay_at_every_split_bytewise_and_randomly() {
    for capture in CAPTURES {
        let expected = replay(capture, []);
        compare_with_oracle(capture, &expected).unwrap();
        assert_eq!(native_bytes(&expected), capture.native, "{}", capture.label);
        for split in 0..=capture.response.len() {
            assert_eq!(
                replay(capture, [split]),
                expected,
                "{} split {split}",
                capture.label
            );
        }
        assert_eq!(
            replay(capture, std::iter::repeat(1).take(capture.response.len())),
            expected,
            "{} bytewise",
            capture.label
        );
        for seed in 0_u64..64 {
            let mut random = seed;
            let chunks = std::iter::from_fn(|| {
                random = random
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                Some(1 + ((random >> 33) as usize % 97))
            })
            .take(capture.response.len());
            assert_eq!(
                replay(capture, chunks),
                expected,
                "{} seed {seed}",
                capture.label
            );
        }
        let mut options = DecodeOptions::default();
        options.protocol_revision = 54_460;
        let mut decoder = StreamDecoder::new(options);
        let blocks = decoder.feed(capture.native).unwrap();
        decoder.finish().unwrap();
        let rows: usize = blocks.iter().map(|block| block.num_rows).sum();
        match capture.label {
            "select" | "parameter" | "escaped_parameter" | "recovery" | "container_parameter" => {
                assert_eq!(rows, 1)
            }
            "multi" => assert_eq!(rows, 5),
            "exception" => assert!(rows > 0 && rows < 2000),
            "totals" => assert_eq!(rows, 2),
            _ => unreachable!(),
        }
        if capture.label == "escaped_parameter" {
            let block = blocks
                .iter()
                .find(|block| block.num_rows == 1)
                .expect("escaped parameter result block");
            let Column::Utf8(column) = &block.columns[0] else {
                panic!("escaped parameter did not decode as String");
            };
            assert_eq!(column.offsets, [0, ESCAPED_PARAMETER.len() as i32]);
            assert_eq!(column.data, ESCAPED_PARAMETER.as_bytes());
        }
        if capture.label == "container_parameter" {
            let block = blocks
                .iter()
                .find(|block| block.num_rows == 1)
                .expect("container parameter result block");
            let Column::Array(items) = &block.columns[0] else {
                panic!("Array(String) parameter did not decode as Array");
            };
            assert_eq!(items.offsets, [0, 2]);
            let Column::Utf8(values) = items.values.as_ref() else {
                panic!("Array(String) elements did not decode as String");
            };
            assert_eq!(values.offsets, [0, 6, 10]);
            assert_eq!(values.data, b"user_1it's");
            // `\N` for Nullable(String) is NULL, not the string "\N".
            assert_eq!(block.columns[1].null_count(), 1);
        }
    }
}

#[test]
fn totals_and_extremes_cannot_be_swapped_without_oracle_failure() {
    let capture = capture("totals");
    let mut response = capture.response.to_vec();
    for event in oracle("totals").events {
        if let ExpectedEvent::Block {
            kind,
            packet_offset,
            ..
        } = event
        {
            match kind {
                BlockKind::Totals => response[*packet_offset] = 8,
                BlockKind::Extremes => response[*packet_offset] = 7,
                _ => {}
            }
        }
    }
    let events = replay_response(capture, &response, []);
    assert!(compare_with_oracle(capture, &events).is_err());
}

#[test]
fn capture_exercises_all_query_server_packets_and_error_reuse() {
    let mut connection = ready();
    let mut seen = Vec::new();
    for capture in CAPTURES {
        start(&mut connection, capture);
        for chunk in capture.response.chunks(7) {
            connection.feed(chunk).unwrap();
            drain(&mut connection, &mut seen);
        }
        assert_eq!(connection.state(), State::Ready);
        if capture.label == "exception" {
            assert!(matches!(seen.last(), Some(Event::Exception(error)) if error.code == 153));
            // This response was captured on the very same socket immediately
            // after the exception, with no EndOfStream or intervening Ping.
            let recovery = crate::capture("recovery");
            assert_eq!(recovery.label, "recovery");
            start(&mut connection, recovery);
            connection.feed(recovery.response).unwrap();
            let mut recovered = Vec::new();
            drain(&mut connection, &mut recovered);
            assert_eq!(native_bytes(&recovered), recovery.native);
            assert!(matches!(recovered.last(), Some(Event::EndOfStream)));
            assert_eq!(connection.state(), State::Ready);
        } else {
            assert!(matches!(seen.last(), Some(Event::EndOfStream)));
        }
    }
    assert!(seen.iter().any(|e| matches!(e, Event::Data(_))));
    assert!(seen.iter().any(|e| matches!(e, Event::Progress(_))));
    assert!(seen.iter().any(|e| matches!(e, Event::ProfileInfo(_))));
    assert!(seen.iter().any(|e| matches!(e, Event::ProfileEvents(_))));
    assert!(seen.iter().any(|e| matches!(e, Event::Log(_))));
    assert!(seen.iter().any(|e| matches!(e, Event::Totals(_))));
    assert!(seen.iter().any(|e| matches!(e, Event::Extremes(_))));
}

// ---------------------------------------------------------------------------
// README driver examples
// ---------------------------------------------------------------------------

/// The README `connect` and `run_select` examples, compiled from the same
/// files the README is checked against, so these tests run that exact code.
mod readme {
    use std::collections::VecDeque;
    use std::io::{self, Read, Write};

    use ch_core_rs::batch::ColBatch;
    use ch_core_rs::native::decode::{decode_next_block, DecodeError, DecodeOptions};
    use ch_core_rs::native::stream_decoder::StreamDecoder;
    use ch_core_rs::native::varint::ByteReader;
    use ch_tcp_proto::{
        ClientHello, Connection, Event, FailureKind, ProtocolError, Query, ServerException, State,
    };

    use crate::{
        base_query, capture, oracle, ready, start, BlockKind, Capture, ExpectedEvent, CAPTURES,
        HELLO, QUOTA_KEY,
    };

    include!("support/readme_connect.rs");
    include!("support/readme_run_select.rs");
    include!("support/readme_decode_payload.rs");
    include!("support/readme_health_check.rs");

    const PONG: &[u8] = &[4];

    fn hello() -> ClientHello {
        ClientHello::new("ch-core-query-fixture", "default")
            .with_client_version(13, 79)
            .with_quota_key(QUOTA_KEY)
    }

    /// How the fake server sizes each read.
    #[derive(Clone, Copy, Debug)]
    enum Reads {
        Fixed(usize),
        Random(u64),
    }

    /// An in-memory server. Each segment becomes readable only after the
    /// client has written and flushed the request it answers; reading
    /// earlier panics, because a real server would never answer and the
    /// client would hang. Writes are buffered until `flush`, like a
    /// `BufWriter`. Every third read is `Interrupted`, as a signal would.
    struct ScriptedServer {
        /// `(flushed client bytes before this is readable, server bytes)`.
        segments: VecDeque<(usize, &'static [u8])>,
        reads: Reads,
        random: u64,
        calls: u64,
        written: Vec<u8>,
        unflushed: Vec<u8>,
    }

    impl ScriptedServer {
        fn new(reads: Reads, segments: Vec<(usize, &'static [u8])>) -> Self {
            let random = match reads {
                Reads::Random(seed) => seed,
                Reads::Fixed(_) => 0,
            };
            Self {
                segments: segments.into(),
                reads,
                random,
                calls: 0,
                written: Vec::new(),
                unflushed: Vec::new(),
            }
        }

        fn next_read_size(&mut self) -> usize {
            match self.reads {
                Reads::Fixed(size) => size,
                Reads::Random(_) => {
                    self.random = self
                        .random
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    match (self.random >> 33) % 16 {
                        0 => 1,
                        1 => usize::MAX,
                        _ => 1 + (self.random >> 40) as usize % 97,
                    }
                }
            }
        }
    }

    impl Read for ScriptedServer {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls % 3 == 0 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let size = self.next_read_size();
            let Some((gate, segment)) = self.segments.front_mut() else {
                return Ok(0);
            };
            assert!(
                self.written.len() >= *gate,
                "client read before flushing the request this reply answers \
                 ({} flushed, {} unflushed, {} needed)",
                self.written.len(),
                self.unflushed.len(),
                gate
            );
            let read = segment.len().min(size).min(buffer.len());
            buffer[..read].copy_from_slice(&segment[..read]);
            *segment = &segment[read..];
            if segment.is_empty() {
                self.segments.pop_front();
            }
            Ok(read)
        }
    }

    impl Write for ScriptedServer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.unflushed.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.written.append(&mut self.unflushed);
            Ok(())
        }
    }

    fn client_hello_bytes() -> Vec<u8> {
        Connection::new(hello()).unwrap().take_output()
    }

    /// The exact bytes `query` puts on the wire after the handshake.
    fn request_bytes(query: &Query) -> Vec<u8> {
        let mut connection = ready();
        connection.query(query.clone()).unwrap();
        connection.take_output()
    }

    /// Hello, addendum, and Ping, then one `(request, response)` exchange
    /// per query. Returns the script and every byte the client must send.
    fn script(exchanges: &[(&[u8], &'static [u8])]) -> (Vec<(usize, &'static [u8])>, Vec<u8>) {
        let mut client = client_hello_bytes();
        let mut segments = vec![(client.len(), HELLO)];
        client.push(QUOTA_KEY.len() as u8);
        client.extend_from_slice(QUOTA_KEY.as_bytes());
        client.push(4); // Ping
        segments.push((client.len(), PONG));
        for (request, response) in exchanges {
            client.extend_from_slice(request);
            segments.push((client.len(), *response));
        }
        (segments, client)
    }

    fn read_modes() -> impl Iterator<Item = Reads> {
        [Reads::Fixed(1), Reads::Fixed(usize::MAX)]
            .into_iter()
            .chain((0..24).map(Reads::Random))
    }

    fn decoded_rows(native: &[u8], revision: u64) -> usize {
        let mut options = DecodeOptions::default();
        options.protocol_revision = revision;
        let mut decoder = StreamDecoder::new(options);
        let blocks = decoder.feed(native).unwrap();
        decoder.finish().unwrap();
        blocks.iter().map(|block| block.num_rows).sum()
    }

    /// The README must contain `support/readme_run_select.rs` byte for
    /// byte, so the example and the code these tests run cannot drift.
    #[test]
    fn readme_run_select_example_is_the_tested_function() {
        let readme = include_str!("../README.md");
        let tested = include_str!("support/readme_run_select.rs");
        assert!(
            readme.contains(tested),
            "README.md `run_select` differs from tests/support/readme_run_select.rs"
        );
    }

    #[test]
    fn readme_run_select_replays_captures_at_any_read_size() {
        for (label, rows) in [("select", 1), ("multi", 5), ("totals", 2)] {
            let capture: &Capture = capture(label);
            assert_eq!(request_bytes(&base_query(capture)), capture.request);
            for reads in read_modes() {
                let (segments, client) = script(&[(capture.request, capture.response)]);
                let mut socket = ScriptedServer::new(reads, segments);
                let mut connection = connect(&mut socket, hello()).unwrap();
                let native = run_select(&mut socket, &mut connection, base_query(capture))
                    .unwrap_or_else(|error| panic!("{label} {reads:?}: {error:?}"));
                assert_eq!(native, capture.native, "{label} {reads:?}");
                assert_eq!(connection.state(), State::Ready, "{label} {reads:?}");
                assert_eq!(socket.written, client, "{label} {reads:?}");
                assert!(socket.unflushed.is_empty(), "{label} {reads:?}");
                let revision = connection.effective_revision().unwrap();
                assert_eq!(revision, 54_460);
                assert_eq!(decoded_rows(&native, revision), rows, "{label}");
            }
        }
    }

    /// The captured mid-stream Exception is an error, and the same socket
    /// then runs the recovery query captured right after it.
    #[test]
    fn readme_run_select_reports_an_exception_and_reuses_the_connection() {
        let failed = capture("exception");
        let recovery = capture("recovery");
        for reads in read_modes() {
            let (segments, client) = script(&[
                (failed.request, failed.response),
                (recovery.request, recovery.response),
            ]);
            let mut socket = ScriptedServer::new(reads, segments);
            let mut connection = connect(&mut socket, hello()).unwrap();
            let error = run_select(&mut socket, &mut connection, base_query(failed))
                .expect_err("the captured query fails");
            assert!(
                matches!(&error, SelectError::Server(exception) if exception.code == 153),
                "{reads:?}: {error:?}"
            );
            assert_eq!(connection.state(), State::Ready, "{reads:?}");
            let native = run_select(&mut socket, &mut connection, base_query(recovery))
                .unwrap_or_else(|error| panic!("{reads:?}: {error:?}"));
            assert_eq!(native, recovery.native, "{reads:?}");
            assert_eq!(socket.written, client, "{reads:?}");
            assert_eq!(decoded_rows(&native, 54_460), 1);
        }
    }

    /// Round 2 High 1: more than 1024 small events, then EndOfStream,
    /// complete through the plain read, feed, drain loop at any read size.
    #[test]
    fn readme_run_select_completes_after_more_than_1024_events() {
        let query = Query::new("SELECT 1");
        let request = request_bytes(&query);
        // 1100 Progress packets, the select capture's Data packets
        // exactly as the server framed them, then EndOfStream.
        let select = capture("select");
        let mut response = [3_u8, 1, 2, 3, 4, 5, 6].repeat(1100);
        for event in oracle("select").events {
            if let ExpectedEvent::Block {
                kind: BlockKind::Data,
                packet_offset,
                offset,
                length,
            } = *event
            {
                response.extend_from_slice(&select.response[packet_offset..offset + length]);
            }
        }
        response.push(5);
        let response: &'static [u8] = Box::leak(response.into_boxed_slice());
        for reads in read_modes() {
            let (segments, _) = script(&[(&request, response)]);
            let mut socket = ScriptedServer::new(reads, segments);
            let mut connection = connect(&mut socket, hello()).unwrap();
            let native = run_select(&mut socket, &mut connection, query.clone())
                .unwrap_or_else(|error| panic!("{reads:?}: {error:?}"));
            assert_eq!(native, select.native, "{reads:?}");
            assert_eq!(connection.state(), State::Ready);
        }
    }

    /// EOF in the middle of a result is truncation, not an empty result.
    #[test]
    fn readme_run_select_reports_truncation() {
        let capture = capture("multi");
        let cut = &capture.response[..capture.response.len() / 2];
        let (segments, _) = script(&[(capture.request, cut)]);
        let mut socket = ScriptedServer::new(Reads::Fixed(7), segments);
        let mut connection = connect(&mut socket, hello()).unwrap();
        let error = run_select(&mut socket, &mut connection, base_query(capture))
            .expect_err("a truncated result fails");
        assert!(
            matches!(error, SelectError::Protocol(ProtocolError::UnexpectedEof)),
            "{error:?}"
        );
    }

    /// Round 3 S6: a reusable Exception followed in the same chunk by a
    /// malformed packet is a protocol failure, not a reusable server error.
    #[test]
    fn readme_run_select_prefers_a_protocol_failure_after_an_exception() {
        let failed = capture("exception");
        let response: &'static [u8] =
            Box::leak([failed.response, &[79]].concat().into_boxed_slice());
        for (reads, same_chunk) in [(Reads::Fixed(usize::MAX), true), (Reads::Fixed(1), false)] {
            let (segments, _) = script(&[(failed.request, response)]);
            let mut socket = ScriptedServer::new(reads, segments);
            let mut connection = connect(&mut socket, hello()).unwrap();
            let error = run_select(&mut socket, &mut connection, base_query(failed))
                .expect_err("the captured query fails");
            if same_chunk {
                assert!(
                    matches!(
                        error,
                        SelectError::Protocol(ProtocolError::UnexpectedPacket { packet: 79, .. })
                    ),
                    "{error:?}"
                );
                assert_eq!(connection.state(), State::Failed(FailureKind::Protocol));
            } else {
                // Byte-sized reads deliver the Exception before byte 79 is
                // read, so at that point the connection is reusable.
                assert!(matches!(error, SelectError::Server(_)), "{error:?}");
                assert_eq!(connection.state(), State::Ready);
            }
        }
    }

    /// Every captured result has a non-empty header and starts with a zero-row
    /// schema Data block, unlike HTTP. The per-payload decoder yields it as a
    /// zero-row batch, and row totals match the whole-stream decode.
    #[test]
    fn readme_decode_payload_decodes_each_data_event() {
        for capture in CAPTURES {
            let mut connection = ready();
            start(&mut connection, capture);
            connection.feed(capture.response).unwrap();
            let mut batches = Vec::new();
            while let Some(event) = connection.poll_event() {
                if let Event::Data(payload) = event {
                    batches.push(decode_payload(&payload, &connection).unwrap());
                }
            }
            assert_eq!(batches[0].num_rows, 0, "{}", capture.label);
            assert!(!batches[0].columns.is_empty(), "{}", capture.label);
            let rows: usize = batches.iter().map(|batch| batch.num_rows).sum();
            let expected = decoded_rows(capture.native, 54_460);
            assert_eq!(rows, expected, "{}", capture.label);
        }
        let connection = ready();
        assert!(decode_payload(&[], &connection).is_err());
        let select = capture("select");
        let doubled = [select.native, select.native].concat();
        assert!(decode_payload(&doubled, &connection).is_err());
    }

    #[test]
    fn readme_health_check_pings_an_idle_connection() {
        for reads in read_modes() {
            let (mut segments, mut client) = script(&[]);
            client.push(4);
            segments.push((client.len(), PONG));
            let mut socket = ScriptedServer::new(reads, segments);
            let mut connection = connect(&mut socket, hello()).unwrap();
            health_check(&mut socket, &mut connection)
                .unwrap_or_else(|error| panic!("{reads:?}: {error}"));
            assert_eq!(connection.state(), State::Ready);
            assert_eq!(socket.written, client, "{reads:?}");
            // The server closed instead of answering the next Ping.
            let error = health_check(&mut socket, &mut connection).expect_err("EOF");
            assert_eq!(
                error.downcast_ref::<ProtocolError>(),
                Some(&ProtocolError::UnexpectedEof)
            );
        }
    }

    /// Each README snippet is the file its tests run, byte for byte.
    #[test]
    fn readme_snippets_are_the_tested_functions() {
        let readme = include_str!("../README.md");
        for (name, tested) in [
            (
                "decode_payload",
                include_str!("support/readme_decode_payload.rs"),
            ),
            (
                "health_check",
                include_str!("support/readme_health_check.rs"),
            ),
        ] {
            assert!(
                readme.contains(tested),
                "README.md `{name}` differs from tests/support"
            );
        }
    }
}
