//! Fixture tooling only for scripts/gen_tcp_compressed_insert_fixtures.py.

use std::error::Error;
use std::fs;
use std::path::Path;

use ch_core_rs::batch::ColBatch;
use ch_core_rs::column::{Column, PrimitiveColumn, Utf8Column};
use ch_core_rs::compression::write_lz4_frame;
use ch_core_rs::native::encode::{encode_block, EncodeOptions};
use ch_core_rs::schema::{ChType, Field, Schema};
use ch_tcp_proto::{ClientHello, Connection, Event, Query, ENGINE_REVISION};

const HELLO: &[u8] =
    include_bytes!("../../../tests/fixtures/tcp_handshake_server_hello_rev54460.packet");
const TABLE: &str = "ch_core_tcp_compressed_insert_fixture";

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

fn batch(number: u64, value: &str, empty: bool) -> ColBatch {
    let numbers = if empty { Vec::new() } else { vec![number] };
    let strings = if empty {
        Utf8Column::new(vec![0], Vec::new())
    } else {
        Utf8Column::new(vec![0, value.len() as i32], value.as_bytes().to_vec())
    };
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
            Column::UInt64(PrimitiveColumn::new(numbers)),
            Column::Utf8(strings),
        ],
        usize::from(!empty),
    )
}

fn ready(output: &Path, compressed: bool) -> Result<Connection, Box<dyn Error>> {
    let hello = ClientHello::new("ch-core-query-fixture", "default")
        .with_client_version(13, 79)
        .with_quota_key("fixture_quota_13");
    let mut connection = Connection::new(hello)?.with_compression(compressed);
    fs::write(
        output.join("tcp_compressed_insert_client_hello.bytes"),
        connection.take_output(),
    )?;
    connection.feed(HELLO)?;
    assert!(matches!(
        connection.poll_event(),
        Some(Event::ServerHello(_))
    ));
    fs::write(
        output.join("tcp_compressed_insert_addendum.bytes"),
        connection.take_output(),
    )?;
    Ok(connection)
}

fn save(
    output: &Path,
    label: &str,
    stage: &str,
    connection: &mut Connection,
) -> Result<(), Box<dyn Error>> {
    let name = format!("tcp_compressed_insert_{label}_{stage}_rev54460.bytes");
    fs::write(output.join(name), connection.take_output())?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let output = std::env::args_os()
        .nth(1)
        .ok_or("missing output directory")?;
    let output = Path::new(&output);
    fs::create_dir_all(output)?;
    let mut options = EncodeOptions::default();
    options.protocol_revision = ENGINE_REVISION;
    for label in ["multi", "rejected", "before_schema", "plain_before_schema"] {
        let compressed = label != "plain_before_schema";
        let before_schema = matches!(label, "before_schema" | "plain_before_schema");
        let sql = if before_schema {
            "INSERT INTO ch_core_tcp_compressed_insert_missing (n, s) VALUES".to_owned()
        } else {
            format!("INSERT INTO {TABLE} (n, s) VALUES")
        };
        let mut connection = ready(output, compressed)?;
        connection.insert(query(sql, label, !before_schema))?;
        save(output, label, "request", &mut connection)?;
        if !before_schema {
            let native = encode_block(&batch(0, "", true), &options)?;
            let mut schema = vec![1, 0];
            write_lz4_frame(&native, &mut schema)?;
            connection.feed(&schema)?;
            assert!(matches!(
                connection.poll_event(),
                Some(Event::InsertSchema(_))
            ));
            let large = "x".repeat(1_100_000);
            let rows = if label == "multi" {
                vec![(13, "user_1"), (79, large.as_str())]
            } else {
                vec![(179, "rejected")]
            };
            for (index, (number, value)) in rows.into_iter().enumerate() {
                connection.insert_data(&encode_block(&batch(number, value, false), &options)?)?;
                save(output, label, &format!("block{index}"), &mut connection)?;
            }
            connection.finish_insert()?;
            save(output, label, "finish", &mut connection)?;
        }
        if label != "rejected" {
            let mut recovery = ready(output, compressed)?;
            let sql = if before_schema {
                "SELECT toUInt64(13) AS n, 'user_1' AS s".to_owned()
            } else {
                format!("SELECT n, s FROM {TABLE} ORDER BY n")
            };
            recovery.query(query(sql, &format!("{label}_recovery"), true))?;
            save(output, label, "recovery_request", &mut recovery)?;
        }
    }
    Ok(())
}
