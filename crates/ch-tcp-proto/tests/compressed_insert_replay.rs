//! Replay compressed INSERT captures without sockets, plus an opt-in live check.

use ch_core_rs::batch::ColBatch;
use ch_core_rs::column::{Column, PrimitiveColumn, Utf8Column};
use ch_core_rs::compression::{decompress_native_stream, ChecksumMode};
use ch_core_rs::native::decode::{decode_all_bytes, DecodeOptions};
use ch_core_rs::native::encode::{encode_block, EncodeOptions};
use ch_core_rs::schema::{ChType, Field, Schema};
use ch_tcp_proto::{ClientHello, Connection, Event, FailureKind, Query, State};

const HELLO: &[u8] =
    include_bytes!("../../../tests/fixtures/tcp_handshake_server_hello_rev54460.packet");
const TABLE: &str = "ch_core_tcp_compressed_insert_fixture";

struct Capture {
    label: &'static str,
    request: &'static [u8],
    schema: &'static [u8],
    blocks: &'static [&'static [u8]],
    finish: &'static [u8],
    completion: &'static [u8],
    recovery_request: &'static [u8],
    recovery: &'static [u8],
}

macro_rules! fixture {
    ($label:literal, $stage:literal) => {
        include_bytes!(concat!(
            "../../../tests/fixtures/tcp_compressed_insert_",
            $label,
            "_",
            $stage,
            "_rev54460.bytes"
        ))
    };
}

macro_rules! capture {
    ($label:literal, [$($block:literal),*]) => {
        Capture {
            label: $label,
            request: fixture!($label, "request"),
            schema: fixture!($label, "schema"),
            blocks: &[$(fixture!($label, $block)),*],
            finish: fixture!($label, "finish"),
            completion: fixture!($label, "completion"),
            recovery_request: fixture!($label, "recovery_request"),
            recovery: fixture!($label, "recovery"),
        }
    };
}

const CAPTURES: &[Capture] = &[
    capture!("multi", ["block0", "block1"]),
    Capture {
        label: "rejected",
        request: fixture!("rejected", "request"),
        schema: fixture!("rejected", "schema"),
        blocks: &[fixture!("rejected", "block0")],
        finish: fixture!("rejected", "finish"),
        completion: fixture!("rejected", "completion"),
        recovery_request: &[],
        recovery: &[],
    },
    Capture {
        label: "before_schema",
        request: fixture!("before_schema", "request"),
        schema: &[],
        blocks: &[],
        finish: &[],
        completion: fixture!("before_schema", "completion"),
        recovery_request: fixture!("before_schema", "recovery_request"),
        recovery: fixture!("before_schema", "recovery"),
    },
    Capture {
        label: "plain_before_schema",
        request: fixture!("plain_before_schema", "request"),
        schema: &[],
        blocks: &[],
        finish: &[],
        completion: fixture!("plain_before_schema", "completion"),
        recovery_request: fixture!("plain_before_schema", "recovery_request"),
        recovery: fixture!("plain_before_schema", "recovery"),
    },
];

fn hello() -> ClientHello {
    ClientHello::new("ch-core-query-fixture", "default")
        .with_client_version(13, 79)
        .with_quota_key("fixture_quota_13")
}

fn ready() -> Connection {
    let mut connection = Connection::new(hello()).unwrap().with_compression(true);
    connection.take_output();
    connection.feed(HELLO).unwrap();
    assert!(matches!(
        connection.poll_event(),
        Some(Event::ServerHello(_))
    ));
    assert_eq!(connection.take_output(), b"\x10fixture_quota_13");
    connection
}

fn query(sql: String, label: &str, settings: bool) -> Query {
    let query = Query::new(sql)
        .with_query_id(format!("ch-core-compressed-insert-{label}"))
        .with_initial_address("192.0.2.13:79")
        .with_initial_query_start_time_microseconds(1_791_500_000_000_013)
        .with_os_user("fixture_os_user_13")
        .with_hostname("fixture_host_79");
    if settings {
        query
            .with_setting("send_profile_events", "0")
            .with_setting("send_logs_level", "none")
    } else {
        query
    }
}

fn feed(connection: &mut Connection, bytes: &[u8], chunk: usize) -> Vec<Event> {
    let mut events = Vec::new();
    for bytes in bytes.chunks(chunk.max(1)) {
        connection.feed(bytes).unwrap();
        events.extend(std::iter::from_fn(|| connection.poll_event()));
    }
    events
}

fn native(packet: &[u8]) -> Vec<u8> {
    assert_eq!(&packet[..2], &[2, 0]);
    decompress_native_stream(&packet[2..], ChecksumMode::Verify).unwrap()
}

fn batch(rows: &[(u64, &str)]) -> ColBatch {
    let mut offsets = vec![0];
    let mut data = Vec::new();
    for (_, value) in rows {
        data.extend_from_slice(value.as_bytes());
        offsets.push(data.len() as i32);
    }
    ColBatch::new(
        Schema::new(vec![
            Field {
                name: "n".into(),
                ch_type: ChType::UInt64,
            },
            Field {
                name: "s".into(),
                ch_type: ChType::String,
            },
        ]),
        vec![
            Column::UInt64(PrimitiveColumn::new(
                rows.iter().map(|(number, _)| *number).collect(),
            )),
            Column::Utf8(Utf8Column::new(offsets, data)),
        ],
        rows.len(),
    )
}

fn encode(batch: &ColBatch) -> Vec<u8> {
    let mut options = EncodeOptions::default();
    options.protocol_revision = 54_460;
    encode_block(batch, &options).unwrap()
}

fn start(capture: &Capture) -> Connection {
    let mut connection = ready().with_compression(capture.label != "plain_before_schema");
    let before_schema = capture.schema.is_empty();
    let sql = if before_schema {
        "INSERT INTO ch_core_tcp_compressed_insert_missing (n, s) VALUES".to_owned()
    } else {
        format!("INSERT INTO {TABLE} (n, s) VALUES")
    };
    connection
        .insert(query(sql, capture.label, !before_schema))
        .unwrap();
    assert_eq!(connection.take_output(), capture.request);
    connection
}

fn upload(connection: &mut Connection, capture: &Capture) {
    let large = "x".repeat(1_100_000);
    for (index, captured) in capture.blocks.iter().enumerate() {
        let rows = match (capture.label, index) {
            ("multi", 0) => vec![(13, "user_1")],
            ("multi", 1) => vec![(79, large.as_str())],
            ("rejected", 0) => vec![(179, "rejected")],
            _ => panic!("unexpected capture block"),
        };
        let encoded = encode(&batch(&rows));
        assert_eq!(encoded, native(captured));
        connection.insert_data(&encoded).unwrap();
        assert_eq!(connection.take_output(), *captured);
    }
    connection.finish_insert().unwrap();
    assert_eq!(connection.take_output(), capture.finish);
}

fn assert_schema(events: &[Event]) {
    assert!(events.iter().any(|event| matches!(
        event,
        Event::TableColumns {
            truncated: false,
            ..
        }
    )));
    let native = events
        .iter()
        .find_map(|event| match event {
            Event::InsertSchema(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .unwrap();
    assert_eq!(native, encode(&batch(&[])));
}

fn recovery(connection: &mut Connection, capture: &Capture, chunk: usize) -> Vec<Event> {
    let sql = if capture.schema.is_empty() {
        "SELECT toUInt64(13) AS n, 'user_1' AS s".to_owned()
    } else {
        format!("SELECT n, s FROM {TABLE} ORDER BY n")
    };
    connection
        .query(query(sql, &format!("{}_recovery", capture.label), true))
        .unwrap();
    assert_eq!(connection.take_output(), capture.recovery_request);
    let events = feed(connection, capture.recovery, chunk);
    assert!(matches!(events.last(), Some(Event::EndOfStream)));
    assert_eq!(connection.state(), State::Ready);
    let native: Vec<u8> = events
        .iter()
        .filter_map(|event| match event {
            Event::Data(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    let mut options = DecodeOptions::default();
    options.protocol_revision = 54_460;
    let decoded = decode_all_bytes(&native, &options).unwrap();
    if capture.label == "multi" {
        let large = "x".repeat(1_100_000);
        assert_eq!(decoded.num_rows(), 2);
        assert_eq!(decoded.chunks.len(), 1);
        assert_eq!(
            *decoded.chunks[0],
            batch(&[(13, "user_1"), (79, large.as_str())])
        );
    } else {
        assert_eq!(decoded.num_rows(), 1);
        assert_eq!(*decoded.chunks[0], batch(&[(13, "user_1")]));
    }
    events
}

#[test]
fn captured_compressed_insert_and_exception_recovery_replay_fragmented() {
    for capture in CAPTURES {
        let mut expected = None;
        for chunk in [usize::MAX, 1, 2, 7, 31, 127] {
            let mut connection = start(capture);
            let schema = feed(&mut connection, capture.schema, chunk);
            if !capture.schema.is_empty() {
                assert_schema(&schema);
                upload(&mut connection, capture);
            }
            let completion = feed(&mut connection, capture.completion, chunk);
            if capture.label == "rejected" {
                let Some(Event::Exception(exception)) = completion.last() else {
                    panic!("missing rejection");
                };
                assert_eq!(exception.name, "DB::Exception");
                assert!(exception.message.contains("Constraint"));
                assert!(!completion
                    .iter()
                    .any(|event| matches!(event, Event::EndOfStream)));
                assert_eq!(
                    connection.state(),
                    State::Failed(FailureKind::ServerException)
                );
            } else if capture.schema.is_empty() {
                assert!(matches!(
                    completion.last(),
                    Some(Event::Exception(exception)) if exception.code == 60
                ));
                assert_eq!(connection.state(), State::Ready);
            } else {
                assert!(matches!(completion.last(), Some(Event::EndOfStream)));
                assert_eq!(connection.state(), State::Ready);
            }
            let recovered = if capture.recovery.is_empty() {
                Vec::new()
            } else {
                recovery(&mut connection, capture, chunk)
            };
            let actual = (schema, completion, recovered);
            if let Some(expected) = &expected {
                assert_eq!(&actual, expected, "{} chunk {chunk}", capture.label);
            } else {
                expected = Some(actual);
            }
        }
    }
}

#[test]
fn captured_schema_and_completion_replay_at_every_split() {
    for capture in CAPTURES {
        for stage in ["schema", "completion"] {
            if stage == "schema" && capture.schema.is_empty() {
                continue;
            }
            let bytes = if stage == "schema" {
                capture.schema
            } else {
                capture.completion
            };
            for split in 0..=bytes.len() {
                let mut connection = start(capture);
                if stage == "completion" && !capture.schema.is_empty() {
                    assert_schema(&feed(&mut connection, capture.schema, usize::MAX));
                    upload(&mut connection, capture);
                }
                let mut events = feed(&mut connection, &bytes[..split], usize::MAX);
                events.extend(feed(&mut connection, &bytes[split..], usize::MAX));
                if stage == "schema" {
                    assert_schema(&events);
                    assert_eq!(connection.state(), State::Inserting);
                } else {
                    if capture.label == "rejected" {
                        assert_eq!(
                            connection.state(),
                            State::Failed(FailureKind::ServerException)
                        );
                    } else {
                        assert_eq!(connection.state(), State::Ready);
                        recovery(&mut connection, capture, usize::MAX);
                    }
                }
            }
        }
    }
}

#[test]
fn upload_phase_exception_retires_compressed_connection() {
    let capture = &CAPTURES[1];
    let mut connection = start(capture);
    assert_schema(&feed(&mut connection, capture.schema, 1));
    connection
        .insert_data(&encode(&batch(&[(179, "rejected")])))
        .unwrap();
    let events = feed(&mut connection, capture.completion, 1);
    assert!(matches!(events.last(), Some(Event::Exception(_))));
    assert_eq!(
        connection.state(),
        State::Failed(FailureKind::ServerException)
    );
    assert!(connection.take_output().is_empty());
}

#[test]
#[ignore = "requires ClickHouse v26.8.1.2041-lts on 127.0.0.1:9000"]
fn live_compressed_insert_select_round_trip() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    fn receive(
        socket: &mut TcpStream,
        connection: &mut Connection,
        done: impl Fn(&Event) -> bool,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let count = socket.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "unexpected server EOF");
            connection.feed(&buffer[..count]).unwrap();
            let mut complete = false;
            while let Some(event) = connection.poll_event() {
                assert!(!matches!(event, Event::Exception(_)), "{event:?}");
                complete |= done(&event);
                events.push(event);
            }
            if complete {
                return events;
            }
        }
    }

    fn run(socket: &mut TcpStream, connection: &mut Connection, sql: String) -> Vec<Event> {
        connection.query(Query::new(sql)).unwrap();
        socket.write_all(&connection.take_output()).unwrap();
        receive(socket, connection, |event| {
            matches!(event, Event::EndOfStream)
        })
    }

    let mut socket = TcpStream::connect("127.0.0.1:9000").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let mut connection = Connection::new(hello()).unwrap().with_compression(true);
    socket.write_all(&connection.take_output()).unwrap();
    receive(&mut socket, &mut connection, |event| {
        matches!(event, Event::ServerHello(_))
    });
    socket.write_all(&connection.take_output()).unwrap();
    let table = format!("ch_core_tcp_compressed_live_{}", std::process::id());
    run(
        &mut socket,
        &mut connection,
        format!("CREATE TABLE {table} (n UInt64, s String) ENGINE=Memory"),
    );
    let expected = batch(&[(13, "user_1\0"), (79, "multibyte_\u{1f980}")]);
    connection
        .insert(Query::new(format!("INSERT INTO {table} VALUES")))
        .unwrap();
    socket.write_all(&connection.take_output()).unwrap();
    receive(&mut socket, &mut connection, |event| {
        matches!(event, Event::InsertSchema(_))
    });
    connection.insert_data(&encode(&expected)).unwrap();
    connection.finish_insert().unwrap();
    socket.write_all(&connection.take_output()).unwrap();
    receive(&mut socket, &mut connection, |event| {
        matches!(event, Event::EndOfStream)
    });
    let events = run(
        &mut socket,
        &mut connection,
        format!("SELECT n, s FROM {table} ORDER BY n"),
    );
    run(&mut socket, &mut connection, format!("DROP TABLE {table}"));
    let bytes: Vec<u8> = events
        .iter()
        .filter_map(|event| match event {
            Event::Data(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    let mut options = DecodeOptions::default();
    options.protocol_revision = 54_460;
    let actual = decode_all_bytes(&bytes, &options).unwrap();
    assert_eq!(actual.chunks.len(), 1);
    assert_eq!(*actual.chunks[0], expected);
}
