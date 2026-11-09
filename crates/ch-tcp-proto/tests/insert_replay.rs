//! Replay independently captured revision-54460 INSERT exchanges without I/O.

use ch_core_rs::batch::ColBatch;
use ch_core_rs::column::{Column, PrimitiveColumn, Utf8Column};
use ch_core_rs::native::decode::DecodeOptions;
use ch_core_rs::native::encode::{encode_block, EncodeOptions};
use ch_core_rs::native::stream_decoder::StreamDecoder;
use ch_core_rs::schema::{ChType, Field, Schema};
use ch_tcp_proto::{ClientHello, Connection, Event, FailureKind, ProtocolError, Query, State};

const HELLO: &[u8] =
    include_bytes!("../../../tests/fixtures/tcp_handshake_server_hello_rev54460.packet");

#[derive(Debug)]
enum Expected {
    Block(u8, usize, usize),
    TableColumns(&'static [u8], &'static [u8]),
    Progress([u64; 6]),
    #[allow(dead_code)] // Present only when the live server emits packet code 6.
    ProfileInfo([u64; 3], bool, u64, bool),
    Exception(i32, &'static str, &'static str, bool),
    EndOfStream,
}

struct CaptureOracle {
    label: &'static str,
    sql: &'static str,
    schema: &'static [Expected],
    midupload: &'static [Expected],
    completion: &'static [Expected],
}
include!("../../../tests/fixtures/tcp_insert_oracle_rev54460.rs");

struct Capture {
    label: &'static str,
    request: &'static [u8],
    schema: &'static [u8],
    midupload: &'static [u8],
    upload: &'static [u8],
    finish: &'static [u8],
    completion: &'static [u8],
}
macro_rules! capture {
    ($label:literal) => {
        Capture {
            label: $label,
            request: include_bytes!(concat!(
                "../../../tests/fixtures/tcp_insert_",
                $label,
                "_request_rev54460.bytes"
            )),
            schema: include_bytes!(concat!(
                "../../../tests/fixtures/tcp_insert_",
                $label,
                "_schema_rev54460.bytes"
            )),
            midupload: include_bytes!(concat!(
                "../../../tests/fixtures/tcp_insert_",
                $label,
                "_midupload_rev54460.bytes"
            )),
            upload: include_bytes!(concat!(
                "../../../tests/fixtures/tcp_insert_",
                $label,
                "_upload_rev54460.bytes"
            )),
            finish: include_bytes!(concat!(
                "../../../tests/fixtures/tcp_insert_",
                $label,
                "_finish_rev54460.bytes"
            )),
            completion: include_bytes!(concat!(
                "../../../tests/fixtures/tcp_insert_",
                $label,
                "_completion_rev54460.bytes"
            )),
        }
    };
}
const CAPTURES: &[Capture] = &[
    capture!("multi"),
    capture!("empty"),
    capture!("zero_rows"),
    capture!("before_schema"),
    capture!("completion_exception"),
    capture!("midupload"),
    capture!("inline_values"),
    capture!("insert_select"),
    capture!("inline_async"),
];

fn ready() -> Connection {
    let hello = ClientHello::new("ch-core-insert-fixture", "default").with_client_version(26, 8);
    let mut connection = Connection::new(hello).unwrap();
    connection.take_output();
    connection.feed(HELLO).unwrap();
    assert!(matches!(
        connection.poll_event(),
        Some(Event::ServerHello(_))
    ));
    assert_eq!(connection.take_output(), [0]);
    connection
}

fn start(capture: &Capture) -> Connection {
    let sql = ORACLES
        .iter()
        .find(|o| o.label == capture.label)
        .unwrap()
        .sql;
    let query = Query::new(sql)
        .with_query_id(format!("ch-core-insert-fixture-{}", capture.label))
        .with_initial_address("127.0.0.1:0")
        .with_initial_query_start_time_microseconds(0)
        .with_os_user("")
        .with_hostname("")
        .with_setting("send_profile_events", "1")
        .with_setting("send_logs_level", "debug");
    let mut connection = ready();
    connection.insert(query).unwrap();
    assert_eq!(connection.state(), State::AwaitingInsertSchema);
    assert_eq!(
        connection.take_output(),
        capture.request,
        "{}",
        capture.label
    );
    connection
}

fn feed_chunks(
    connection: &mut Connection,
    bytes: &[u8],
    chunks: impl IntoIterator<Item = usize>,
) -> Vec<Event> {
    let mut events = Vec::new();
    let mut rest = bytes;
    for size in chunks {
        if rest.is_empty() {
            break;
        }
        let end = size.min(rest.len());
        connection.feed(&rest[..end]).unwrap();
        rest = &rest[end..];
        events.extend(std::iter::from_fn(|| connection.poll_event()));
    }
    connection.feed(rest).unwrap();
    events.extend(std::iter::from_fn(|| connection.poll_event()));
    events
}

fn assert_oracle(bytes: &[u8], events: &[Event], expected: &[Expected]) {
    assert_eq!(events.len(), expected.len(), "{events:?}");
    for (event, expected) in events.iter().zip(expected) {
        match (event, expected) {
            (Event::InsertSchema(actual), Expected::Block(1, offset, length))
            | (Event::Log(actual), Expected::Block(10, offset, length))
            | (Event::ProfileEvents(actual), Expected::Block(14, offset, length)) => {
                assert_eq!(actual, &bytes[*offset..offset + length]);
            }
            (Event::Exception(actual), Expected::Exception(code, name, message, nested)) => {
                assert_eq!(actual.code, *code);
                assert_eq!(actual.has_nested, *nested);
                assert_eq!(actual.name, *name);
                assert_eq!(actual.message, *message);
                assert!(!actual.truncated);
            }
            (
                Event::TableColumns {
                    table_name,
                    description,
                    truncated,
                    ..
                },
                Expected::TableColumns(table, desc),
            ) => {
                assert_eq!(table_name, table);
                assert_eq!(description, desc);
                assert!(!truncated);
            }
            (Event::Progress(actual), Expected::Progress(values)) => {
                assert_eq!(
                    [
                        actual.read_rows,
                        actual.read_bytes,
                        actual.total_rows_to_read,
                        actual.written_rows,
                        actual.written_bytes,
                        actual.elapsed_ns
                    ],
                    *values
                );
            }
            (
                Event::ProfileInfo(actual),
                Expected::ProfileInfo(values, applied, rows, obsolete),
            ) => {
                assert_eq!([actual.rows, actual.blocks, actual.bytes], *values);
                assert_eq!(actual.applied_limit, *applied);
                assert_eq!(actual.rows_before_limit, *rows);
                assert_eq!(actual.unused_obsolete_field, *obsolete);
            }
            (Event::EndOfStream, Expected::EndOfStream) => {}
            _ => panic!("oracle mismatch: {event:?} vs {expected:?}"),
        }
    }
}

fn encoded_blocks(capture: &Capture) -> Vec<Vec<u8>> {
    let chunks: Vec<Vec<(u64, &str)>> = match capture.label {
        "multi" => vec![vec![(13, "user_1"), (79, "user_2")], vec![(17, "")]],
        "zero_rows" => vec![vec![]],
        "completion_exception" => vec![vec![(179, "rejected")]],
        "midupload" => vec![vec![(13, "user_1")], vec![(79, "user_2")]],
        _ => vec![],
    };
    let mut blocks = Vec::new();
    for rows in chunks {
        let mut offsets = vec![0];
        let mut data = Vec::new();
        for (_, value) in &rows {
            data.extend_from_slice(value.as_bytes());
            offsets.push(data.len() as i32);
        }
        let numbers = Column::UInt64(PrimitiveColumn::new(rows.iter().map(|(n, _)| *n).collect()));
        let batch = ColBatch::new(
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
            vec![numbers, Column::Utf8(Utf8Column::new(offsets, data))],
            rows.len(),
        );
        let mut options = EncodeOptions::default();
        options.protocol_revision = 54_460;
        let native = encode_block(&batch, &options).unwrap();
        blocks.push(native);
    }
    blocks
}

fn upload(capture: &Capture, connection: &mut Connection) -> Vec<Event> {
    let mut output = Vec::new();
    let mut midupload = Vec::new();
    for (index, native) in encoded_blocks(capture).iter().enumerate() {
        connection.insert_data(native).unwrap();
        output.extend(connection.take_output());
        if index == 0 {
            midupload = feed_chunks(connection, capture.midupload, []);
            let oracle = ORACLES.iter().find(|o| o.label == capture.label).unwrap();
            assert_oracle(capture.midupload, &midupload, oracle.midupload);
        }
    }
    assert_eq!(
        output, capture.upload,
        "{} core encode upload",
        capture.label
    );
    if capture.finish.is_empty() {
        assert_eq!(
            capture.label, "completion_exception",
            "successful upload capture omitted its terminator"
        );
        assert_eq!(connection.state(), State::Inserting);
        return midupload;
    }
    connection.finish_insert().unwrap();
    assert_eq!(connection.state(), State::FinishingInsert);
    assert_eq!(connection.take_output(), capture.finish);
    midupload
}

fn replay(
    capture: &Capture,
    schema_chunks: impl IntoIterator<Item = usize>,
    completion_chunks: impl IntoIterator<Item = usize>,
) -> (Vec<Event>, Vec<Event>, Vec<Event>) {
    let mut connection = start(capture);
    let schema = feed_chunks(&mut connection, capture.schema, schema_chunks);
    let oracle = ORACLES.iter().find(|o| o.label == capture.label).unwrap();
    assert_oracle(capture.schema, &schema, oracle.schema);
    if capture.label == "before_schema" {
        assert_eq!(
            connection.state(),
            State::Failed(FailureKind::ServerException)
        );
        return (schema, Vec::new(), Vec::new());
    }
    if matches!(
        capture.label,
        "inline_values" | "insert_select" | "inline_async"
    ) {
        assert_eq!(connection.state(), State::Ready);
        assert!(schema
            .iter()
            .any(|event| matches!(event, Event::EndOfStream)));
        assert!(capture.upload.is_empty() && capture.finish.is_empty());
        assert_eq!(connection.finish(), Ok(()));
        return (schema, Vec::new(), Vec::new());
    }
    assert_eq!(connection.state(), State::Inserting);
    let header = schema
        .iter()
        .find_map(|e| {
            if let Event::InsertSchema(b) = e {
                Some(b)
            } else {
                None
            }
        })
        .unwrap();
    let mut options = DecodeOptions::default();
    options.protocol_revision = 54_460;
    let mut decoder = StreamDecoder::new(options);
    let mut batches = decoder.feed(header).unwrap();
    let batch = batches.remove(0);
    assert_eq!(batch.num_rows, 0);
    assert_eq!(
        batch.schema.fields,
        vec![
            Field {
                name: "n".into(),
                ch_type: ChType::UInt64,
            },
            Field {
                name: "s".into(),
                ch_type: ChType::String,
            },
        ]
    );
    let midupload = upload(capture, &mut connection);
    let completion = feed_chunks(&mut connection, capture.completion, completion_chunks);
    assert_oracle(capture.completion, &completion, oracle.completion);
    assert_eq!(
        connection.state(),
        if capture.label == "completion_exception" {
            State::Failed(FailureKind::ServerException)
        } else {
            State::Ready
        }
    );
    if capture.label == "completion_exception" && capture.finish.is_empty() {
        assert!(connection.take_output().is_empty());
    }
    assert_eq!(connection.finish(), Ok(()));
    (schema, midupload, completion)
}

#[test]
fn captured_insert_replays_every_split_and_bytewise() {
    for capture in CAPTURES {
        let expected = replay(capture, [], []);
        for split in 0..=capture.schema.len() {
            assert_eq!(
                replay(capture, [split], []),
                expected,
                "{} schema split {split}",
                capture.label
            );
        }
        for split in 0..=capture.completion.len() {
            assert_eq!(
                replay(capture, [], [split]),
                expected,
                "{} completion split {split}",
                capture.label
            );
        }
        assert_eq!(
            replay(
                capture,
                std::iter::repeat(1).take(capture.schema.len()),
                std::iter::repeat(1).take(capture.completion.len())
            ),
            expected
        );
        for seed in 0_u64..32 {
            let chunks = |mut random: u64, len| {
                std::iter::from_fn(move || {
                    random = random
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    Some(1 + ((random >> 33) as usize % 97))
                })
                .take(len)
            };
            assert_eq!(
                replay(
                    capture,
                    chunks(seed, capture.schema.len()),
                    chunks(seed ^ 79, capture.completion.len())
                ),
                expected
            );
        }
    }
}

/// The live probe may observe the constraint Exception before it sends the
/// terminator. Exercise that stage ordering even when the committed capture
/// happened to receive the same Exception after its terminator.
#[test]
fn completion_exception_before_terminator_replays_without_finish() {
    let captured = CAPTURES
        .iter()
        .find(|capture| capture.label == "completion_exception")
        .unwrap();
    let before_terminator = Capture {
        label: captured.label,
        request: captured.request,
        schema: captured.schema,
        midupload: captured.midupload,
        upload: captured.upload,
        finish: &[],
        completion: captured.completion,
    };
    let (_, _, completion) = replay(&before_terminator, [], []);
    assert!(completion
        .iter()
        .any(|event| matches!(event, Event::Exception(_))));
}

#[test]
fn captured_insert_truncated_schema_and_completion_are_eof_errors() {
    for capture in CAPTURES {
        for end in 0..capture.schema.len() {
            let mut connection = start(capture);
            feed_chunks(&mut connection, &capture.schema[..end], []);
            assert_eq!(
                connection.finish(),
                Err(ProtocolError::UnexpectedEof),
                "{} schema prefix {end}",
                capture.label
            );
        }
        if matches!(
            capture.label,
            "before_schema" | "inline_values" | "insert_select" | "inline_async"
        ) {
            continue;
        }
        for end in 0..capture.completion.len() {
            let mut connection = start(capture);
            feed_chunks(&mut connection, capture.schema, []);
            upload(capture, &mut connection);
            feed_chunks(&mut connection, &capture.completion[..end], []);
            assert_eq!(
                connection.finish(),
                Err(ProtocolError::UnexpectedEof),
                "{} completion prefix {end}",
                capture.label
            );
        }
    }
}

/// These packets were read on a live socket between block 1 and block 2.
/// Sending remains legal with a partial packet and with undrained telemetry.
#[test]
fn live_midupload_telemetry_allows_writes_at_every_split() {
    let capture = CAPTURES.iter().find(|c| c.label == "midupload").unwrap();
    let oracle = ORACLES.iter().find(|o| o.label == "midupload").unwrap();
    assert!(oracle
        .midupload
        .iter()
        .any(|e| matches!(e, Expected::Block(10, _, _))));
    assert!(oracle
        .midupload
        .iter()
        .any(|e| matches!(e, Expected::Block(14, _, _))));
    let blocks = encoded_blocks(capture);
    for split in 0..=capture.midupload.len() {
        for finish_at in [split, split + (capture.midupload.len() - split) / 2] {
            let mut connection = start(capture);
            feed_chunks(&mut connection, capture.schema, []);
            connection.insert_data(&blocks[0]).unwrap();
            let first = connection.take_output();
            assert_eq!(first, capture.upload[..first.len()]);
            // Intentionally leave completed events queued and any current packet
            // incomplete while queuing the second block and terminator.
            connection.feed(&capture.midupload[..split]).unwrap();
            connection
                .insert_data(&blocks[1])
                .unwrap_or_else(|e| panic!("insert_data split {split}: {e}"));
            connection
                .feed(&capture.midupload[split..finish_at])
                .unwrap();
            connection
                .finish_insert()
                .unwrap_or_else(|e| panic!("finish_insert split {split}, finish {finish_at}: {e}"));
            let expected = [
                capture.upload[first.len()..].to_vec(),
                capture.finish.to_vec(),
            ]
            .concat();
            assert_eq!(connection.take_output(), expected, "outbound split {split}");
            connection.feed(&capture.midupload[finish_at..]).unwrap();
            let events: Vec<_> = std::iter::from_fn(|| connection.poll_event()).collect();
            assert_oracle(capture.midupload, &events, oracle.midupload);
            let completion = feed_chunks(&mut connection, capture.completion, []);
            assert_oracle(capture.completion, &completion, oracle.completion);
            assert_eq!(connection.state(), State::Ready);
            assert_eq!(connection.finish(), Ok(()));
        }
    }
}
