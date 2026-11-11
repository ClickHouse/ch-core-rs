//! Real-server 54460 fixtures. These tests perform no I/O.

use ch_core_rs::batch::ColBatch;
use ch_core_rs::column::{Column, PrimitiveColumn, Utf8Column};
use ch_core_rs::compression::{
    compress_lz4_frame, compress_lz4_frames, decompress_frame, decompress_frame_into,
    decompress_native_stream, write_lz4_frame, ChecksumMode, CompressionError,
};
use ch_core_rs::native::decode::{
    block_end, decode_all_bytes, decode_next_block, DecodeError, DecodeOptions,
};
use ch_core_rs::native::encode::{encode_block, EncodeOptions};
use ch_core_rs::native::varint::ByteReader;

const HTTP_PLAIN: &[u8] = include_bytes!("fixtures/http_multi_rev54460.native");
const HTTP_LZ4: &[u8] = include_bytes!("fixtures/http_multi_rev54460_lz4.native");
const TCP_PLAIN: &[u8] = include_bytes!("fixtures/tcp_multi_rev54460.packets");
const TCP_LZ4: &[u8] = include_bytes!("fixtures/tcp_multi_rev54460_lz4.packets");
const TCP_LARGE: &[u8] = include_bytes!("fixtures/tcp_large_rev54460_lz4.packets");
const TCP_ZSTD: &[u8] = include_bytes!("fixtures/tcp_multi_rev54460_zstd.packets");
const TCP_LARGE_ZSTD: &[u8] = include_bytes!("fixtures/tcp_large_rev54460_zstd.packets");

fn options() -> DecodeOptions {
    let mut options = DecodeOptions::default();
    options.protocol_revision = 54460;
    options
}

/// Read only the Data packets preserved by the capture script. Packet type
/// and block name use the raw stream; only the Native body is compressed.
/// Completion scanning here models an engine accumulating a multi-frame body.
fn packets(bytes: &[u8], compressed: bool) -> (Vec<ColBatch>, Vec<Vec<usize>>) {
    let mut wire = ByteReader::new(bytes);
    let mut batches = Vec::new();
    let mut frame_sizes = Vec::new();
    while wire.remaining() != 0 {
        assert_eq!(wire.read_varint().unwrap(), 1, "Server::Data");
        assert_eq!(wire.read_varint_string().unwrap(), "");
        let mut sizes = Vec::new();
        let batch = if compressed {
            let mut plain = Vec::new();
            loop {
                // Pass the entire remaining packet stream. consumed() must
                // stop before the next frame or the next raw packet header.
                let prior = plain.len();
                let frame =
                    decompress_frame_into(wire.remaining_slice(), ChecksumMode::Verify, &mut plain)
                        .unwrap();
                sizes.push(frame.plaintext_len());
                wire.skip(frame.consumed()).unwrap();
                match block_end(&plain, &options()) {
                    Ok(Some(end)) => {
                        frame.finish_block(end - prior).unwrap();
                        let mut reader = ByteReader::new(&plain);
                        let batch = decode_next_block(&mut reader, &options()).unwrap().unwrap();
                        assert_eq!(reader.remaining(), 0);
                        break batch;
                    }
                    Err(DecodeError::Io(error))
                        if error.kind() == std::io::ErrorKind::UnexpectedEof => {}
                    other => panic!("unexpected block completeness result: {other:?}"),
                }
            }
        } else {
            decode_next_block(&mut wire, &options()).unwrap().unwrap()
        };
        batches.push(batch);
        frame_sizes.push(sizes);
    }
    (batches, frame_sizes)
}

fn assert_small_values(batches: &[ColBatch]) {
    let mut row = 0;
    for batch in batches.iter().filter(|batch| batch.num_rows != 0) {
        assert_eq!(batch.column_names(), ["n", "s"]);
        let (Column::UInt64(numbers), Column::Utf8(strings)) = (batch.column(0), batch.column(1))
        else {
            panic!("unexpected fixture columns");
        };
        for index in 0..batch.num_rows {
            assert_eq!(numbers.values[index], row + 13);
            assert_eq!(strings.value(index), format!("user_{row}").as_bytes());
            row += 1;
        }
    }
    assert_eq!(row, 5);
}

#[test]
fn http_compressed_and_plain_54460_decode_identically() {
    let plain = decompress_native_stream(HTTP_LZ4, ChecksumMode::Verify).unwrap();
    assert_eq!(plain, HTTP_PLAIN);
    let batches = decode_all_bytes(&plain, &options()).unwrap();
    assert_eq!(batches.num_rows(), 5);
    assert_eq!(
        batches
            .chunks
            .iter()
            .map(|batch| batch.num_rows)
            .collect::<Vec<_>>(),
        [2, 2, 1]
    );
    assert_small_values(
        &batches
            .chunks
            .iter()
            .map(|batch| batch.as_ref().clone())
            .collect::<Vec<_>>(),
    );
}

#[test]
fn http_one_compression_frame_can_contain_multiple_native_blocks() {
    // Default HTTP output auto-flushes each chunk, so captured frames align
    // with Native blocks. Reframe the real plaintext to verify that the HTTP
    // reader also accepts frames spanning blocks.
    let compressed = compress_lz4_frame(HTTP_PLAIN).unwrap();
    let frame = decompress_frame(&compressed, ChecksumMode::Verify).unwrap();
    assert_eq!(frame.consumed(), compressed.len());
    assert_eq!(frame.plaintext(), HTTP_PLAIN);
    let plain = decompress_native_stream(&compressed, ChecksumMode::Verify).unwrap();
    let actual = decode_all_bytes(&plain, &options()).unwrap();
    let expected = decode_all_bytes(HTTP_PLAIN, &options()).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        actual
            .chunks
            .iter()
            .map(|batch| batch.num_rows)
            .collect::<Vec<_>>(),
        [2, 2, 1]
    );
}

#[test]
fn tcp_compressed_and_plain_packets_decode_identically() {
    let (plain, _) = packets(TCP_PLAIN, false);
    let (compressed, frames) = packets(TCP_LZ4, true);
    assert_eq!(compressed, plain);
    assert_eq!(
        plain.iter().map(|batch| batch.num_rows).collect::<Vec<_>>(),
        [0, 2, 2, 1, 0]
    );
    assert_eq!(plain.first().unwrap().num_columns(), 2);
    assert_eq!(plain.last().unwrap().num_columns(), 0);
    assert_eq!(frames, [vec![30], vec![60], vec![60], vec![45], vec![10]]);
    assert_small_values(&plain);
}

#[test]
fn tcp_zstd_and_plain_packets_decode_identically() {
    let (plain, _) = packets(TCP_PLAIN, false);
    let (compressed, frames) = packets(TCP_ZSTD, true);
    assert_eq!(compressed, plain);
    assert_eq!(frames, [vec![30], vec![60], vec![60], vec![45], vec![10]]);
    assert_small_values(&compressed);
}

#[test]
fn tcp_large_zstd_block_matches_lz4_across_frames() {
    let (expected, expected_frames) = packets(TCP_LARGE, true);
    let (actual, frames) = packets(TCP_LARGE_ZSTD, true);
    assert_eq!(actual, expected);
    assert_eq!(frames, expected_frames);
    assert_eq!(frames[1], [1_048_576, 51_465]);
}

#[test]
fn real_zstd_frame_retries_truncation_and_preserves_packet_suffix() {
    // Each packet begins with the raw Data code and empty block name.
    let first = decompress_frame(&TCP_ZSTD[2..], ChecksumMode::Verify).unwrap();
    let consumed = first.consumed();
    assert_eq!(TCP_ZSTD[18], 0x90);
    for length in 0..consumed {
        assert!(matches!(
            decompress_frame(&TCP_ZSTD[2..2 + length], ChecksumMode::Verify),
            Err(CompressionError::Truncated { .. })
        ));
    }
    assert_eq!(&TCP_ZSTD[2 + consumed..][..2], &[1, 0]);
    assert!(matches!(
        first.finish_block(first.plaintext().len() - 1),
        Err(CompressionError::TrailingPlaintext { remaining: 1 })
    ));
    let mut corrupted = TCP_ZSTD[2..2 + consumed].to_vec();
    corrupted[0] ^= 1;
    assert!(matches!(
        decompress_frame(&corrupted, ChecksumMode::Verify),
        Err(CompressionError::ChecksumMismatch { .. })
    ));
}

#[test]
fn tcp_large_native_block_spans_two_compression_frames() {
    let (batches, frames) = packets(TCP_LARGE, true);
    assert_eq!(batches.len(), 3);
    assert_eq!(frames, [vec![30], vec![1_048_576, 51_465], vec![10]]);
    let batch = &batches[1];
    assert_eq!(batch.num_rows, 1);
    let (Column::UInt64(numbers), Column::Utf8(strings)) = (batch.column(0), batch.column(1))
    else {
        panic!("unexpected large fixture columns");
    };
    assert_eq!(numbers.values, [79]);
    assert_eq!(strings.value(0).len(), 1_100_000);
    assert!(strings.value(0).iter().all(|byte| *byte == b'x'));
}

#[test]
fn real_tcp_frame_retries_every_truncated_prefix_without_consuming_suffix() {
    // The first schema-only packet is two raw bytes followed by one frame.
    let frame_bytes = &TCP_LZ4[2..59];
    for length in 0..frame_bytes.len() {
        assert!(
            matches!(
                decompress_frame(&frame_bytes[..length], ChecksumMode::Verify),
                Err(CompressionError::Truncated { .. })
            ),
            "prefix length {length}"
        );
    }
    let frame = decompress_frame(&TCP_LZ4[2..], ChecksumMode::Verify).unwrap();
    assert_eq!(frame.consumed(), frame_bytes.len());
    assert_eq!(&TCP_LZ4[2 + frame.consumed()..][..2], &[1, 0]);
    assert!(matches!(
        frame.finish_block(frame.plaintext().len() - 1),
        Err(CompressionError::TrailingPlaintext { remaining: 1 })
    ));
}

#[test]
fn writer_round_trips_captured_tcp_blocks_without_crossing_packet_boundaries() {
    let (expected, _) = packets(TCP_LARGE, true);
    let mut encoded = Vec::new();
    let mut options = EncodeOptions::default();
    options.protocol_revision = 54460;
    for batch in &expected {
        encoded.extend_from_slice(&[1, 0]);
        let native = encode_block(batch, &options).unwrap();
        encoded.extend_from_slice(&compress_lz4_frames(&native, 1_048_576).unwrap());
    }
    let (actual, frames) = packets(&encoded, true);
    assert_eq!(actual, expected);
    assert_eq!(frames[1], [1_048_576, 51_465]);
}

/// Run with `cargo test --test tcp_compression -- --ignored`. The server must
/// match .server-ref. Uses curl and the CLICKHOUSE_CONNECT_TEST_* convention,
/// defaults to 127.0.0.1:8123, and bypasses proxies.
#[test]
#[ignore = "requires a live ClickHouse server matching .server-ref"]
fn live_server_accepts_compressed_native_insert() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn request(query: &str, body: Option<&[u8]>) -> Result<Vec<u8>, String> {
        let host =
            std::env::var("CLICKHOUSE_CONNECT_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into());
        let port = std::env::var("CLICKHOUSE_CONNECT_TEST_PORT").unwrap_or_else(|_| "8123".into());
        let user =
            std::env::var("CLICKHOUSE_CONNECT_TEST_USER").unwrap_or_else(|_| "default".into());
        let password = std::env::var("CLICKHOUSE_CONNECT_TEST_PASSWORD").unwrap_or_default();
        let encoded_query: String = query.bytes().map(|byte| format!("%{byte:02X}")).collect();
        let mut url = format!("http://{host}:{port}/?query={encoded_query}");
        if body.is_some() {
            url.push_str("&decompress=1");
        }
        let mut command = Command::new("curl");
        command
            .args([
                "--noproxy",
                "*",
                "--max-time",
                "30",
                "-sS",
                "--fail-with-body",
                "-X",
                "POST",
                "--user",
            ])
            .arg(format!("{user}:{password}"))
            .arg(url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if body.is_some() {
            command.args(["--data-binary", "@-"]).stdin(Stdio::piped());
        } else {
            command.args(["--data-binary", ""]);
        }
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        if let Some(bytes) = body {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(bytes)
                .map_err(|error| error.to_string())?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "HTTP request failed: {} {}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            ));
        }
        Ok(output.stdout)
    }

    assert_eq!(request("SELECT version()", None).unwrap(), b"26.8.1.2041\n");
    let (source, _) = packets(TCP_LARGE, true);
    // Independent server decoding exercises incompressible literal runs and
    // matches at offset 49,152, in addition to the large offset-1 run.
    let mut noisy = Vec::with_capacity(98_304);
    let mut state = 0x13_79_51_03_u64;
    for _ in 0..49_152 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        noisy.push((state >> 32) as u8);
    }
    noisy.extend_from_within(..);
    let noisy_batch = ColBatch::new(
        source[1].schema.clone(),
        vec![
            Column::UInt64(PrimitiveColumn::new(vec![13])),
            Column::Utf8(Utf8Column::new(vec![0, noisy.len() as i32], noisy.clone())),
        ],
        1,
    );
    // HTTP INSERT Native input always uses revision 0, independently of the
    // revision advertised for a SELECT response.
    let mut compressed = Vec::new();
    let mut native_size = 0;
    for batch in [&source[1], &noisy_batch] {
        let native = encode_block(batch, &EncodeOptions::default()).unwrap();
        native_size += native.len();
        for chunk in native.chunks(1_048_576) {
            write_lz4_frame(chunk, &mut compressed).unwrap();
        }
    }
    assert!(compressed.len() < native_size / 10);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let table = format!("ch_core_compressed_insert_{}_{nonce}", std::process::id());
    request(
        &format!("CREATE TABLE {table} (n UInt64, s String) ENGINE=Memory"),
        None,
    )
    .unwrap();
    let result = (|| {
        request(
            &format!("INSERT INTO {table} FORMAT Native"),
            Some(&compressed),
        )?;
        request(
            &format!("SELECT n, s FROM {table} ORDER BY n FORMAT Native"),
            None,
        )
    })();
    let cleanup = request(&format!("DROP TABLE {table}"), None);
    let output = result.unwrap();
    cleanup.unwrap();
    let actual = decode_all_bytes(&output, &DecodeOptions::default()).unwrap();
    assert_eq!(actual.num_rows(), 2);
    let mut rows = actual.chunks.iter().flat_map(|batch| {
        let (Column::UInt64(numbers), Column::Utf8(strings)) = (batch.column(0), batch.column(1))
        else {
            panic!("unexpected insert readback columns");
        };
        (0..batch.num_rows).map(move |index| (numbers.values[index], strings.value(index)))
    });
    assert_eq!(rows.next().unwrap(), (13, noisy.as_slice()));
    let (number, string) = rows.next().unwrap();
    assert_eq!(number, 79);
    assert_eq!(string.len(), 1_100_000);
    assert!(string.iter().all(|byte| *byte == b'x'));
    assert!(rows.next().is_none());
}
