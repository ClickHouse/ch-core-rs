//! INSERT block validation benchmark for LowCardinality index widths.
//!
//! Run with:
//!
//! ```text
//! CH_CORE_BENCH_ROWS=1048576 cargo bench -p ch-tcp-proto --bench insert_data
//! ```
//!
//! Each sample scans one complete Native block, queues its Data packet, and
//! takes the output. The encoded inputs have the same four-entry dictionary and
//! logical index values; only the self-describing index width differs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ch_core_rs::batch::ColBatch;
use ch_core_rs::column::{Column, DictionaryColumn, Utf8Column};
use ch_core_rs::native::encode::{encode_block, EncodeOptions};
use ch_core_rs::schema::{ChType, Field, Schema};
use ch_tcp_proto::{ClientHello, Connection, Query, ENGINE_REVISION};

const DEFAULT_ROWS: usize = 1_048_576;
const DEFAULT_CELL_MS: u64 = 500;

struct CountingAllocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

fn allocated(size: usize) {
    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// Safety: every allocation request is forwarded unchanged to System. The
// counters do not inspect or alter pointers, alignment, sizes, or lifetimes.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Safety: the caller provides a valid allocation layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // Safety: ptr and layout are the unchanged allocation being released.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Safety: forward the caller's valid allocation and new size unchanged.
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            allocated(new_size);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn put_varuint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        output.push(value as u8 | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn put_string(output: &mut Vec<u8>, value: &[u8]) {
    put_varuint(output, value.len() as u64);
    output.extend_from_slice(value);
}

fn server_hello() -> Vec<u8> {
    let mut bytes = Vec::new();
    put_varuint(&mut bytes, 0);
    put_string(&mut bytes, b"ClickHouse");
    put_varuint(&mut bytes, 26);
    put_varuint(&mut bytes, 8);
    put_varuint(&mut bytes, ENGINE_REVISION);
    put_string(&mut bytes, b"UTC");
    put_string(&mut bytes, b"server");
    put_varuint(&mut bytes, 1);
    bytes
}

fn inserting_connection(schema: &[u8]) -> Connection {
    let mut connection =
        Connection::new(ClientHello::new("ch-core-bench", "default")).expect("client hello");
    let _ = connection.take_output();
    connection.feed(&server_hello()).expect("server hello");
    let _ = connection.take_output();
    while connection.poll_event().is_some() {}
    connection
        .insert(Query::new("INSERT INTO bench VALUES"))
        .expect("start insert");
    let _ = connection.take_output();

    let mut packet = Vec::with_capacity(schema.len() + 2);
    packet.push(1); // Server packet Data
    packet.push(0); // Empty temporary-table name
    packet.extend_from_slice(schema);
    connection.feed(&packet).expect("insert schema");
    while connection.poll_event().is_some() {}
    connection
}

fn low_cardinality_schema_bytes() -> Vec<u8> {
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::String)),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            Vec::new(),
            Column::Utf8(Utf8Column::new(vec![0], Vec::new())),
        ))],
        0,
    );
    let mut options = EncodeOptions::default();
    options.protocol_revision = ENGINE_REVISION;
    encode_block(&batch, &options).expect("encode LowCardinality schema")
}

// Keep the byte patching below in sync with the copy in `benches/block_scanner.rs`.
fn low_cardinality_bytes(rows: usize, index_width: usize) -> Vec<u8> {
    let dictionary = Column::Utf8(Utf8Column::new(
        vec![0, 0, 6, 12, 18],
        b"user_1user_2user_3".to_vec(),
    ));
    let indices = (0..rows)
        .map(|row| (row % 3 + 1) as i32)
        .collect::<Vec<_>>();
    let batch = ColBatch::new(
        Schema::new(vec![Field {
            name: "lc".into(),
            ch_type: ChType::LowCardinality(Box::new(ChType::String)),
        }]),
        vec![Column::Dictionary(DictionaryColumn::new(
            indices, dictionary,
        ))],
        rows,
    );
    let mut options = EncodeOptions::default();
    options.protocol_revision = ENGINE_REVISION;
    let mut bytes = encode_block(&batch, &options).expect("encode LowCardinality benchmark");

    const DICTIONARY_WIRE_BYTES: usize = 22;
    const INDEX_WORD_BYTES: usize = 8;
    const KEY_COUNT_BYTES: usize = 8;
    const ROW_COUNT_BYTES: usize = 8;
    // `HasAdditionalKeysBit` in the index word.
    const HAS_ADDITIONAL_KEYS_BIT: u64 = 1 << 9;
    let old_indexes = bytes.len() - rows;
    let index_word_offset =
        old_indexes - ROW_COUNT_BYTES - DICTIONARY_WIRE_BYTES - KEY_COUNT_BYTES - INDEX_WORD_BYTES;
    let read_u64 = |offset: usize| {
        u64::from_le_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .expect("complete LowCardinality u64 field"),
        )
    };
    let old_word = read_u64(index_word_offset);
    assert_eq!(old_word & 0xff, 0, "encoder did not select UInt8 indexes");
    assert_ne!(
        old_word & HAS_ADDITIONAL_KEYS_BIT,
        0,
        "encoder did not set HasAdditionalKeysBit"
    );
    assert_eq!(
        read_u64(index_word_offset + INDEX_WORD_BYTES),
        4,
        "unexpected LowCardinality dictionary key count"
    );
    assert_eq!(
        read_u64(old_indexes - ROW_COUNT_BYTES),
        rows as u64,
        "unexpected LowCardinality index row count"
    );
    let width_tag = match index_width {
        1 => 0,
        2 => 1,
        4 => 2,
        8 => 3,
        _ => panic!("LowCardinality benchmark width must be 1, 2, 4, or 8"),
    };
    bytes[index_word_offset..index_word_offset + INDEX_WORD_BYTES]
        .copy_from_slice(&(old_word | width_tag).to_le_bytes());
    if index_width != 1 {
        let mut widened = Vec::with_capacity(rows * index_width);
        for &index in &bytes[old_indexes..] {
            widened.extend_from_slice(&(index as u64).to_le_bytes()[..index_width]);
        }
        bytes.truncate(old_indexes);
        bytes.extend_from_slice(&widened);
    }
    bytes
}

fn insert_once(connection: &mut Connection, block: &[u8]) {
    connection
        .insert_data(black_box(block))
        .expect("insert data");
    black_box(connection.take_output());
}

fn allocation_sample(connection: &mut Connection, block: &[u8]) -> (usize, usize) {
    let baseline = LIVE.load(Ordering::Relaxed);
    PEAK.store(baseline, Ordering::Relaxed);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    insert_once(connection, block);
    assert_eq!(
        LIVE.load(Ordering::Relaxed),
        baseline,
        "insert sample leaked allocation"
    );
    (
        PEAK.load(Ordering::Relaxed).saturating_sub(baseline),
        ALLOCATIONS.load(Ordering::Relaxed),
    )
}

fn measure(target: Duration, connection: &mut Connection, block: &[u8]) -> Duration {
    insert_once(connection, block);
    let probe = Instant::now();
    insert_once(connection, block);
    let elapsed = probe.elapsed().max(Duration::from_nanos(1));
    let iterations = (target.as_nanos() / elapsed.as_nanos()).clamp(1, 100_000) as usize;
    let mut best = Duration::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..iterations {
            insert_once(connection, block);
        }
        best = best.min(start.elapsed() / iterations as u32);
    }
    best
}

fn main() {
    let timing = std::env::args().any(|arg| arg == "--bench");
    let rows = std::env::var("CH_CORE_BENCH_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(if timing { DEFAULT_ROWS } else { 1_024 });
    assert!(rows > 0, "CH_CORE_BENCH_ROWS must be positive");
    let target = Duration::from_millis(
        std::env::var("CH_CORE_BENCH_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_CELL_MS),
    );
    if timing {
        println!("rows={rows} time=ms peak=requested heap bytes (input excluded)");
        println!("width MiB insert_data+take_output peak allocations");
    }
    let schema = low_cardinality_schema_bytes();
    for index_width in [1, 2, 4, 8] {
        let block = low_cardinality_bytes(rows, index_width);
        let mut connection = inserting_connection(&schema);
        let (peak, allocations) = allocation_sample(&mut connection, &block);
        if timing {
            let elapsed = measure(target, &mut connection, &block);
            println!(
                "{index_width:<5} {:.2} {:.3} {peak} {allocations}",
                block.len() as f64 / (1 << 20) as f64,
                elapsed.as_secs_f64() * 1_000.0,
            );
        }
    }
}
