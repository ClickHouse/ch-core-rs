//! Replay full compressed revision-54460 SELECT exchanges without socket I/O.

use ch_core_rs::column::Column;
use ch_core_rs::native::decode::{decode_all_bytes, DecodeOptions};
use ch_tcp_proto::{ClientHello, Connection, Event, Query, State};

const HELLO: &[u8] =
    include_bytes!("../../../tests/fixtures/tcp_handshake_server_hello_rev54460.packet");
const QUOTA_KEY: &str = "fixture_quota_13";
const INITIAL_QUERY_START_TIME_MICROSECONDS: u64 = 1_791_500_000_000_013;

struct Capture {
    label: &'static str,
    sql: &'static str,
    method: &'static str,
    request: &'static [u8],
    response: &'static [u8],
}

macro_rules! capture {
    ($label:literal, $sql:expr, $method:literal) => {
        Capture {
            label: $label,
            sql: $sql,
            method: $method,
            request: include_bytes!(concat!(
                "../../../tests/fixtures/tcp_compressed_query_",
                $label,
                "_client_rev54460.packet"
            )),
            response: include_bytes!(concat!(
                "../../../tests/fixtures/tcp_compressed_query_",
                $label,
                "_server_rev54460.packets"
            )),
        }
    };
}

const CAPTURES: &[Capture] = &[
    capture!(
        "lz4_multi",
        "SELECT number + 13 AS n, concat('user_', toString(number)) AS s \
         FROM numbers(5) SETTINGS max_block_size=2, max_threads=1",
        "LZ4"
    ),
    capture!(
        "lz4_large",
        "SELECT toUInt64(79) AS n, repeat('xx', 550000) AS s",
        "LZ4"
    ),
    capture!(
        "lz4_totals_extremes",
        "SELECT number % 2 AS k, count() AS n FROM numbers(5) \
         GROUP BY k WITH TOTALS ORDER BY k SETTINGS max_threads=1, extremes=1",
        "LZ4"
    ),
    capture!(
        "lz4_exception",
        "SELECT intDiv(toUInt64(79), number != 1000) AS n FROM numbers(2000) \
         SETTINGS max_block_size=128, max_threads=1",
        "LZ4"
    ),
    capture!("lz4_exception_recovery", "SELECT toUInt64(13) AS n", "LZ4"),
    capture!(
        "lz4_cancel",
        "SELECT number AS n FROM numbers(1000000000) \
         WHERE sleepEachRow(0.00001) = 0 \
         SETTINGS max_block_size=65536, max_threads=1",
        "LZ4"
    ),
    capture!("lz4_cancel_recovery", "SELECT toUInt64(79) AS n", "LZ4"),
    capture!("none", "SELECT toUInt64(13) AS n, 'user_1' AS s", "NONE"),
    capture!(
        "zstd",
        "SELECT toUInt64(79) AS n, repeat('user_2', 79) AS s",
        "ZSTD"
    ),
];

fn capture(label: &str) -> &'static Capture {
    CAPTURES
        .iter()
        .find(|capture| capture.label == label)
        .unwrap_or_else(|| panic!("missing capture {label}"))
}

fn ready() -> Connection {
    let hello = ClientHello::new("ch-core-query-fixture", "default")
        .with_client_version(13, 79)
        .with_quota_key(QUOTA_KEY);
    let mut connection = Connection::new(hello).unwrap().with_compression(true);
    connection.take_output();
    connection.feed(HELLO).unwrap();
    assert!(matches!(
        connection.poll_event(),
        Some(Event::ServerHello(_))
    ));
    connection.take_output();
    connection
}

fn query(capture: &Capture) -> Query {
    Query::new(capture.sql)
        .with_query_id(format!("ch-core-compressed-fixture-{}", capture.label))
        .with_initial_address("192.0.2.13:79")
        .with_initial_query_start_time_microseconds(INITIAL_QUERY_START_TIME_MICROSECONDS)
        .with_os_user("fixture_os_user_13")
        .with_hostname("fixture_host_79")
        .with_setting("send_profile_events", "1")
        .with_setting("send_logs_level", "debug")
        .with_setting("log_queries", "1")
        .with_setting("network_compression_method", capture.method)
}

fn start(connection: &mut Connection, capture: &Capture) {
    connection.query(query(capture)).unwrap();
    assert_eq!(connection.state(), State::Querying);
    let expected = if capture.label == "lz4_cancel" {
        assert_eq!(capture.request.last(), Some(&3), "raw Client::Cancel");
        &capture.request[..capture.request.len() - 1]
    } else {
        capture.request
    };
    assert_eq!(connection.take_output(), expected, "{}", capture.label);
}

fn drain(connection: &mut Connection, events: &mut Vec<Event>) {
    events.extend(std::iter::from_fn(|| connection.poll_event()));
}

fn feed_chunks(
    connection: &mut Connection,
    response: &[u8],
    chunks: impl IntoIterator<Item = usize>,
) -> Vec<Event> {
    let mut events = Vec::new();
    let mut rest = response;
    for size in chunks {
        if rest.is_empty() {
            break;
        }
        let end = size.min(rest.len());
        connection.feed(&rest[..end]).unwrap();
        rest = &rest[end..];
        drain(connection, &mut events);
    }
    connection.feed(rest).unwrap();
    drain(connection, &mut events);
    events
}

fn replay(capture: &Capture, chunks: impl IntoIterator<Item = usize>) -> Vec<Event> {
    let mut connection = ready();
    start(&mut connection, capture);
    let events = feed_chunks(&mut connection, capture.response, chunks);
    assert_eq!(connection.state(), State::Ready, "{}", capture.label);
    assert_eq!(connection.finish(), Ok(()));
    events
}

fn random_chunks(length: usize) -> Vec<usize> {
    let mut state = 0x9e37_79b9_u32;
    let mut remaining = length;
    let mut chunks = Vec::new();
    while remaining != 0 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let size = ((state as usize >> 16) % 257 + 1).min(remaining);
        chunks.push(size);
        remaining -= size;
    }
    chunks
}

#[derive(Clone, Copy)]
enum NativeKind {
    Data,
    Totals,
    Extremes,
}

fn decode_events(events: &[Event], kind: NativeKind) -> ch_core_rs::batch::ChunkedBatch {
    let native = events
        .iter()
        .filter_map(|event| match (kind, event) {
            (NativeKind::Data, Event::Data(block))
            | (NativeKind::Totals, Event::Totals(block))
            | (NativeKind::Extremes, Event::Extremes(block)) => Some(block.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    let mut options = DecodeOptions::default();
    options.protocol_revision = 54_460;
    decode_all_bytes(&native, &options).unwrap()
}

#[test]
fn small_compressed_exchanges_replay_at_every_split_bytewise_and_randomly() {
    for capture in CAPTURES
        .iter()
        .filter(|capture| capture.label != "lz4_large")
    {
        let expected = replay(capture, []);
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
        assert_eq!(
            replay(capture, random_chunks(capture.response.len())),
            expected,
            "{} random",
            capture.label
        );
    }
}

#[test]
fn large_multiframe_block_replays_with_linear_bytewise_and_random_feeds() {
    let capture = capture("lz4_large");
    let expected = replay(capture, []);
    assert_eq!(
        replay(capture, std::iter::repeat(1).take(capture.response.len())),
        expected,
        "bytewise"
    );
    assert_eq!(
        replay(capture, random_chunks(capture.response.len())),
        expected,
        "random"
    );

    let decoded = decode_events(&expected, NativeKind::Data);
    assert!(
        expected
            .iter()
            .filter_map(|event| match event {
                Event::Data(block) => Some(block.len()),
                _ => None,
            })
            .sum::<usize>()
            > 1024 * 1024
    );
    assert_eq!(decoded.num_rows(), 1);
    let batch = decoded.chunks[0].as_ref();
    let (Column::UInt64(numbers), Column::Utf8(strings)) = (batch.column(0), batch.column(1))
    else {
        panic!("unexpected large capture columns");
    };
    assert_eq!(numbers.values, [79]);
    assert_eq!(strings.value(0).len(), 1_100_000);
    assert!(strings.value(0).iter().all(|byte| *byte == b'x'));
}

#[test]
fn compressed_capture_covers_framed_and_raw_packet_kinds() {
    let multi = replay(capture("lz4_multi"), []);
    assert_eq!(
        multi
            .iter()
            .filter(|event| matches!(event, Event::Data(_)))
            .count(),
        4
    );
    assert!(multi.iter().any(|event| matches!(event, Event::Log(_))));
    assert!(multi
        .iter()
        .any(|event| matches!(event, Event::ProfileEvents(_))));
    assert!(multi
        .iter()
        .any(|event| matches!(event, Event::EndOfStream)));
    let decoded = decode_events(&multi, NativeKind::Data);
    assert_eq!(decoded.num_rows(), 5);
    let mut expected = 0;
    for batch in &decoded.chunks {
        let (Column::UInt64(numbers), Column::Utf8(strings)) = (batch.column(0), batch.column(1))
        else {
            panic!("unexpected multi capture columns");
        };
        for row in 0..batch.num_rows {
            assert_eq!(numbers.values[row], expected + 13);
            assert_eq!(strings.value(row), format!("user_{expected}").as_bytes());
            expected += 1;
        }
    }
    assert_eq!(expected, 5);

    let totals = replay(capture("lz4_totals_extremes"), []);
    assert!(totals.iter().any(|event| matches!(event, Event::Totals(_))));
    assert!(totals
        .iter()
        .any(|event| matches!(event, Event::Extremes(_))));
    let decoded_totals = decode_events(&totals, NativeKind::Totals);
    let totals_batch = decoded_totals.chunks[0].as_ref();
    let (Column::UInt8(keys), Column::UInt64(counts)) =
        (totals_batch.column(0), totals_batch.column(1))
    else {
        panic!("unexpected totals columns");
    };
    assert_eq!(keys.values, [0]);
    assert_eq!(counts.values, [5]);
    let decoded_extremes = decode_events(&totals, NativeKind::Extremes);
    let extremes_batch = decoded_extremes.chunks[0].as_ref();
    let (Column::UInt8(keys), Column::UInt64(counts)) =
        (extremes_batch.column(0), extremes_batch.column(1))
    else {
        panic!("unexpected extremes columns");
    };
    assert_eq!(keys.values, [0, 1]);
    assert_eq!(counts.values, [2, 3]);

    let none = replay(capture("none"), []);
    let decoded = decode_events(&none, NativeKind::Data);
    let batch = decoded.chunks[0].as_ref();
    let (Column::UInt64(numbers), Column::Utf8(strings)) = (batch.column(0), batch.column(1))
    else {
        panic!("unexpected NONE capture columns");
    };
    assert_eq!(numbers.values, [13]);
    assert_eq!(strings.value(0), b"user_1");

    let zstd = replay(capture("zstd"), []);
    let decoded = decode_events(&zstd, NativeKind::Data);
    let batch = decoded.chunks[0].as_ref();
    let (Column::UInt64(numbers), Column::Utf8(strings)) = (batch.column(0), batch.column(1))
    else {
        panic!("unexpected ZSTD capture columns");
    };
    assert_eq!(numbers.values, [79]);
    assert_eq!(strings.value(0), "user_2".repeat(79).as_bytes());
}

#[test]
fn compressed_exception_and_cancel_leave_connection_reusable() {
    let mut connection = ready();
    let exception = capture("lz4_exception");
    start(&mut connection, exception);
    let events = feed_chunks(&mut connection, exception.response, []);
    assert!(matches!(events.last(), Some(Event::Exception(_))));
    assert!(!events
        .iter()
        .any(|event| matches!(event, Event::EndOfStream)));
    let decoded = decode_events(&events, NativeKind::Data);
    assert!(decoded.num_rows() > 0);
    assert!(decoded.chunks.iter().all(|batch| {
        matches!(
            batch.column(0),
            Column::UInt64(numbers) if numbers.values.iter().all(|value| *value == 79)
        )
    }));
    assert_eq!(connection.state(), State::Ready);

    let recovery = capture("lz4_exception_recovery");
    start(&mut connection, recovery);
    let events = feed_chunks(&mut connection, recovery.response, []);
    assert!(matches!(events.last(), Some(Event::EndOfStream)));
    assert_eq!(connection.state(), State::Ready);

    let mut connection = ready();
    let cancel = capture("lz4_cancel");
    start(&mut connection, cancel);
    let events = feed_chunks(&mut connection, cancel.response, []);
    assert!(matches!(events.last(), Some(Event::EndOfStream)));
    assert_eq!(connection.state(), State::Ready);

    let recovery = capture("lz4_cancel_recovery");
    start(&mut connection, recovery);
    let events = feed_chunks(&mut connection, recovery.response, []);
    assert!(matches!(events.last(), Some(Event::EndOfStream)));
    assert_eq!(connection.state(), State::Ready);
}
