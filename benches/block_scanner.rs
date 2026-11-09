//! Incremental Native block scanner benchmark for runtime-shaped columns.
//!
//! Run with `cargo bench --bench block_scanner`. The benchmark constructs and
//! encodes each block before timing. Incremental scans borrow growing prefixes
//! of that one encoded allocation, so feed-size results do not include copying
//! input into a staging buffer.
//!
//! Set `CH_CORE_BENCH_ROWS` to change the default 1,000,000 rows and
//! `CH_CORE_BENCH_MS` to change the approximate measurement time per cell.
//! `CH_CORE_BENCH_PREFIX_LABELS` controls the default 4,096-label Enum16
//! descriptor nested in the prefix-heavy Dynamic case.
//! `CH_CORE_BENCH_FILTER` runs only case names containing that substring.

use std::hint::black_box;
use std::time::{Duration, Instant};

use ch_core_rs::batch::ColBatch;
use ch_core_rs::column::{
    AggregateStateColumn, BoolColumn, Column, DictionaryColumn, DynamicChild, DynamicColumn,
    JsonColumn, PrimitiveColumn, StructuredJson, Utf8Column,
};
use ch_core_rs::native::decode::{decode_all_bytes, parse_ch_type, DecodeOptions};
use ch_core_rs::native::encode::{encode_block, EncodeOptions};
use ch_core_rs::native::stream_decoder::{BlockBoundary, BlockScanner};
use ch_core_rs::schema::{ChType, Field, Schema};

const DEFAULT_ROWS: usize = 1_000_000;
const CHECK_ROWS: usize = 1_024;
const DEFAULT_CELL_MS: u64 = 350;
const FEEDS: [usize; 4] = [4 << 10, 16 << 10, 64 << 10, 1 << 20];

fn utf8_repeated(rows: usize, width: usize) -> Utf8Column {
    let mut offsets = Vec::with_capacity(rows + 1);
    let mut data = Vec::with_capacity(rows * width);
    offsets.push(0);
    for row in 0..rows {
        let word = (row as u64).wrapping_mul(7_919).to_le_bytes();
        let start = data.len();
        while data.len() - start < width {
            let remaining = width - (data.len() - start);
            data.extend_from_slice(&word[..remaining.min(word.len())]);
        }
        offsets.push(data.len() as i32);
    }
    Utf8Column::new(offsets, data)
}

fn empty_utf8() -> Utf8Column {
    Utf8Column::new(vec![0], Vec::new())
}

fn utf8_same(rows: usize, value: &[u8]) -> Utf8Column {
    let mut offsets = Vec::with_capacity(rows + 1);
    let mut data = Vec::with_capacity(rows * value.len());
    offsets.push(0);
    for _ in 0..rows {
        data.extend_from_slice(value);
        offsets.push(data.len() as i32);
    }
    Utf8Column::new(offsets, data)
}

fn batch(name: &str, ch_type: ChType, column: Column, rows: usize) -> ColBatch {
    ColBatch::new(
        Schema::new(vec![Field {
            name: name.into(),
            ch_type,
        }]),
        vec![column],
        rows,
    )
}

fn multi_type_dynamic(rows: usize) -> DynamicColumn {
    const ALTERNATIVES: usize = 6;
    let counts = (0..ALTERNATIVES)
        .map(|alternative| (rows + ALTERNATIVES - 1 - alternative) / ALTERNATIVES)
        .collect::<Vec<_>>();
    let mut shared_blob = vec![0x04];
    shared_blob.extend_from_slice(&13u64.to_le_bytes());

    let mut children = vec![
        (
            "Bool".to_owned(),
            0usize,
            DynamicChild::Typed {
                ch_type: ChType::Bool,
                values: Column::Bool(BoolColumn::from_wire_bytes(&vec![1; counts[0]])),
            },
        ),
        (
            "Float64".to_owned(),
            1,
            DynamicChild::Typed {
                ch_type: ChType::Float64,
                values: Column::Float64(PrimitiveColumn::new(
                    (0..counts[1]).map(|value| value as f64 / 10.0).collect(),
                )),
            },
        ),
        (
            "Int64".to_owned(),
            2,
            DynamicChild::Typed {
                ch_type: ChType::Int64,
                values: Column::Int64(PrimitiveColumn::new(
                    (0..counts[2]).map(|value| value as i64 - 13).collect(),
                )),
            },
        ),
        (
            "SharedVariant".to_owned(),
            3,
            DynamicChild::Shared(utf8_same(counts[3], &shared_blob)),
        ),
        (
            "String".to_owned(),
            4,
            DynamicChild::Typed {
                ch_type: ChType::String,
                values: Column::Utf8(utf8_repeated(counts[4], 12)),
            },
        ),
        (
            "UInt64".to_owned(),
            5,
            DynamicChild::Typed {
                ch_type: ChType::UInt64,
                values: Column::UInt64(PrimitiveColumn::new(
                    (0..counts[5]).map(|value| value as u64 * 79).collect(),
                )),
            },
        ),
    ];
    children.sort_by(|left, right| left.0.cmp(&right.0));
    let mut type_id = [0u32; ALTERNATIVES];
    for (index, (_, logical, _)) in children.iter().enumerate() {
        type_id[*logical] = index as u32;
    }
    let ids = (0..rows)
        .map(|row| type_id[row % ALTERNATIVES])
        .collect::<Vec<_>>();
    DynamicColumn::try_new(
        &ids,
        children.into_iter().map(|(_, _, child)| child).collect(),
    )
    .expect("valid multi-type Dynamic column")
}

fn wide_json(rows: usize, flattened: bool) -> (ChType, JsonColumn) {
    const TYPED_PATHS: usize = 12;
    const DYNAMIC_PATHS: usize = 8;

    let typed_types = (0..TYPED_PATHS)
        .map(|path| (format!("typed_{path:02}"), ChType::UInt8))
        .collect::<Vec<_>>();
    let typed = (0..TYPED_PATHS)
        .map(|path| {
            (
                format!("typed_{path:02}"),
                Column::UInt8(PrimitiveColumn::new(
                    (0..rows).map(|row| row.wrapping_add(path) as u8).collect(),
                )),
            )
        })
        .collect::<Vec<_>>();
    let dynamic = (0..DYNAMIC_PATHS)
        .map(|path| {
            let selected = (rows + DYNAMIC_PATHS - 1 - path) / DYNAMIC_PATHS;
            let ids = (0..rows)
                .map(|row| {
                    if row % DYNAMIC_PATHS == path {
                        if flattened {
                            0
                        } else {
                            1
                        }
                    } else {
                        u32::MAX
                    }
                })
                .collect::<Vec<_>>();
            let mut children = Vec::with_capacity(if flattened { 1 } else { 2 });
            if !flattened {
                children.push(DynamicChild::Shared(empty_utf8()));
            }
            children.push(DynamicChild::Typed {
                ch_type: ChType::String,
                values: Column::Utf8(utf8_repeated(selected, 12)),
            });
            (
                format!("dynamic_{path:02}"),
                DynamicColumn::try_new(&ids, children).expect("valid wide JSON dynamic path"),
            )
        })
        .collect::<Vec<_>>();

    let (shared_offsets, shared_paths, shared_values) = if flattened {
        (vec![0; rows + 1], empty_utf8(), empty_utf8())
    } else {
        let pairs = rows.div_ceil(32);
        let mut offsets = Vec::with_capacity(rows + 1);
        offsets.push(0i64);
        let mut seen = 0i64;
        for row in 0..rows {
            if row % 32 == 0 {
                seen += 1;
            }
            offsets.push(seen);
        }
        let mut shared_blob = vec![0x04];
        shared_blob.extend_from_slice(&79u64.to_le_bytes());
        (
            offsets,
            utf8_same(pairs, b"overflow_path"),
            utf8_same(pairs, &shared_blob),
        )
    };
    let structured = StructuredJson::try_new(
        typed,
        dynamic,
        shared_offsets,
        shared_paths,
        shared_values,
        rows,
    )
    .expect("valid wide JSON column");
    (
        ChType::Json {
            max_dynamic_paths: 16,
            max_dynamic_types: 32,
            typed_paths: typed_types,
            skip_paths: Vec::new(),
            skip_regexps: Vec::new(),
        },
        JsonColumn::structured(structured),
    )
}

fn cases(rows: usize, filter: Option<&str>) -> Vec<(&'static str, Vec<u8>)> {
    // A 32-byte value makes the String, Dynamic, and structured JSON cases
    // about 32 MiB at the default row count, matching a wide result column.
    let string = || utf8_repeated(rows, 32);
    let wanted = |name: &str| filter.map_or(true, |needle| name.contains(needle));
    let mut output = Vec::new();
    let mut push = |name: &'static str, batch: ColBatch, options: EncodeOptions| {
        if wanted(name) {
            let bytes = encode_block(&batch, &options).expect("encode benchmark");
            output.push((name, bytes));
        }
    };

    if wanted("String") {
        push(
            "String",
            batch("s", ChType::String, Column::Utf8(string()), rows),
            EncodeOptions::default(),
        );
    }
    if wanted("LowCardinality") {
        let dictionary = Column::Utf8(Utf8Column::new(
            vec![0, 0, 6, 12, 18],
            b"user_1user_2user_3".to_vec(),
        ));
        let indices = (0..rows).map(|row| (row % 3 + 1) as i32).collect();
        push(
            "LowCardinality",
            batch(
                "lc",
                ChType::LowCardinality(Box::new(ChType::String)),
                Column::Dictionary(DictionaryColumn::new(indices, dictionary)),
                rows,
            ),
            EncodeOptions::default(),
        );
    }
    if wanted("Dynamic") {
        let dynamic = DynamicColumn::try_new(
            &vec![1; rows],
            vec![
                DynamicChild::Shared(empty_utf8()),
                DynamicChild::Typed {
                    ch_type: ChType::String,
                    values: Column::Utf8(string()),
                },
            ],
        )
        .expect("valid Dynamic benchmark column");
        push(
            "Dynamic",
            batch(
                "d",
                ChType::Dynamic { max_types: 32 },
                Column::Dynamic(dynamic),
                rows,
            ),
            EncodeOptions::default(),
        );
    }
    if wanted("DynamicMultiType") {
        push(
            "DynamicMultiType",
            batch(
                "multi",
                ChType::Dynamic { max_types: 32 },
                Column::Dynamic(multi_type_dynamic(rows)),
                rows,
            ),
            EncodeOptions::default(),
        );
    }
    if wanted("DynamicWideEnum") {
        let enum_labels = std::env::var("CH_CORE_BENCH_PREFIX_LABELS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4_096usize)
            .max(1)
            .min(i16::MAX as usize + 1);
        let enum_variants = (0..enum_labels)
            .map(|value| {
                (
                    format!(
                        "runtime_label_{value:05}_abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMNOPQRSTUVWXYZ"
                    ),
                    value as i16,
                )
            })
            .collect::<Vec<_>>();
        let enum_values = (0..rows)
            .map(|row| (row % enum_labels) as i16)
            .collect::<Vec<_>>();
        let wide_enum_type = ChType::Enum16 {
            variants: enum_variants,
        };
        let prefix_dynamic = DynamicColumn::try_new(
            &vec![0; rows],
            vec![
                DynamicChild::Typed {
                    ch_type: wide_enum_type,
                    values: Column::Enum16(PrimitiveColumn::new(enum_values)),
                },
                DynamicChild::Shared(empty_utf8()),
            ],
        )
        .expect("valid prefix-heavy Dynamic column");
        push(
            "DynamicWideEnum",
            batch(
                "descriptor",
                ChType::Dynamic { max_types: 32 },
                Column::Dynamic(prefix_dynamic),
                rows,
            ),
            EncodeOptions::default(),
        );
    }
    if wanted("JSON") {
        let json_dynamic = DynamicColumn::try_new(
            &vec![1; rows],
            vec![
                DynamicChild::Shared(empty_utf8()),
                DynamicChild::Typed {
                    ch_type: ChType::String,
                    values: Column::Utf8(string()),
                },
            ],
        )
        .expect("valid JSON dynamic path");
        let json = StructuredJson::try_new(
            Vec::new(),
            vec![("payload".into(), json_dynamic)],
            vec![0; rows + 1],
            empty_utf8(),
            empty_utf8(),
            rows,
        )
        .expect("valid JSON benchmark column");
        push(
            "JSON",
            batch(
                "j",
                parse_ch_type("JSON(max_dynamic_paths=8)").expect("valid JSON type"),
                Column::Json(JsonColumn::structured(json)),
                rows,
            ),
            EncodeOptions::default(),
        );
    }
    for (name, flattened) in [("JSONWideV1", false), ("JSONWideFlattened", true)] {
        if wanted(name) {
            let (ch_type, json) = wide_json(rows, flattened);
            let mut options = EncodeOptions::default();
            options.flattened_dynamic = flattened;
            push(
                name,
                batch("wide", ch_type, Column::Json(json), rows),
                options,
            );
        }
    }
    if wanted("AggregateFunction") {
        let mut data = Vec::with_capacity(rows * 8);
        for row in 0..rows {
            data.extend_from_slice(&(row as i64).wrapping_mul(13).to_le_bytes());
        }
        push(
            "AggregateFunction",
            batch(
                "a",
                ChType::AggregateFunction {
                    function: "sum".into(),
                    arguments: vec![ChType::Int32],
                },
                Column::AggregateState(AggregateStateColumn::new(
                    (0..=rows).map(|row| (row * 8) as i64).collect(),
                    data,
                )),
                rows,
            ),
            EncodeOptions::default(),
        );
    }
    if wanted("AggregateCount") {
        let mut offsets = Vec::with_capacity(rows + 1);
        let mut data = Vec::with_capacity(rows * 3);
        offsets.push(0);
        for row in 0..rows {
            let mut value = (row as u64).wrapping_mul(13);
            while value >= 0x80 {
                data.push(value as u8 | 0x80);
                value >>= 7;
            }
            data.push(value as u8);
            offsets.push(data.len() as i64);
        }
        push(
            "AggregateCount",
            batch(
                "count",
                ChType::AggregateFunction {
                    function: "count".into(),
                    arguments: Vec::new(),
                },
                Column::AggregateState(AggregateStateColumn::new(offsets, data)),
                rows,
            ),
            EncodeOptions::default(),
        );
    }
    if wanted("AggregateNullableSum") {
        let mut offsets = Vec::with_capacity(rows + 1);
        let mut data = Vec::with_capacity(rows * 5);
        offsets.push(0);
        for row in 0..rows {
            if row % 2 == 0 {
                data.push(0);
            } else {
                data.push(1);
                data.extend_from_slice(&(row as i64).wrapping_mul(13).to_le_bytes());
            }
            offsets.push(data.len() as i64);
        }
        push(
            "AggregateNullableSum",
            batch(
                "nullable_sum",
                ChType::AggregateFunction {
                    function: "sum".into(),
                    arguments: vec![ChType::Nullable(Box::new(ChType::Int32))],
                },
                Column::AggregateState(AggregateStateColumn::new(offsets, data)),
                rows,
            ),
            EncodeOptions::default(),
        );
    }
    if wanted("UInt64") {
        push(
            "UInt64",
            batch(
                "u",
                ChType::UInt64,
                Column::UInt64(PrimitiveColumn::new(
                    (0..rows).map(|row| row as u64).collect(),
                )),
                rows,
            ),
            EncodeOptions::default(),
        );
    }
    output
}

fn scan_once(bytes: &[u8], feed: usize) {
    let mut scanner = BlockScanner::new(DecodeOptions::default());
    let mut end = feed.min(bytes.len());
    loop {
        match scanner.scan(black_box(&bytes[..end])).expect("scan") {
            BlockBoundary::NeedMore => {
                assert!(end < bytes.len(), "complete block reported as incomplete");
                end = (end + feed).min(bytes.len());
            }
            BlockBoundary::Complete { consumed, .. } => {
                assert_eq!(consumed, bytes.len());
                return;
            }
            _ => panic!("unknown BlockBoundary variant"),
        }
    }
}

fn one_shot(bytes: &[u8]) {
    scan_once(bytes, bytes.len());
}

fn decode(bytes: &[u8]) {
    black_box(decode_all_bytes(black_box(bytes), &DecodeOptions::default()).expect("decode"));
}

fn measure(target: Duration, mut operation: impl FnMut()) -> Duration {
    operation();
    let probe = Instant::now();
    operation();
    let elapsed = probe.elapsed().max(Duration::from_nanos(1));
    // Quadratic baseline cases can take seconds per operation. Keep their run
    // count bounded while still taking the best of three independent samples.
    if elapsed >= target {
        let mut best = elapsed;
        for _ in 0..2 {
            let start = Instant::now();
            operation();
            best = best.min(start.elapsed());
        }
        return best;
    }
    let iterations = (target.as_nanos() / elapsed.as_nanos()).clamp(1, 100_000) as usize;

    let mut best = Duration::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..iterations {
            operation();
        }
        best = best.min(start.elapsed() / iterations as u32);
    }
    best
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn main() {
    let timing = std::env::args().any(|arg| arg == "--bench");
    let configured_rows = std::env::var("CH_CORE_BENCH_ROWS")
        .ok()
        .and_then(|value| value.parse().ok());
    let rows = configured_rows.unwrap_or(if timing { DEFAULT_ROWS } else { CHECK_ROWS });
    let target = Duration::from_millis(
        std::env::var("CH_CORE_BENCH_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_CELL_MS),
    );
    let filter = std::env::var("CH_CORE_BENCH_FILTER").ok();
    let cases = cases(rows, filter.as_deref());

    // Keep `cargo test --all-targets` useful without spending benchmark time.
    for (_, bytes) in &cases {
        one_shot(bytes);
        scan_once(bytes, 4 << 10);
        decode(bytes);
    }
    if !timing {
        return;
    }

    println!(
        "rows={rows}, best of 5 calibrated batches (best of 3 when one run exceeds target), times in ms"
    );
    println!(
        "{:<20} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "column", "MiB", "decode", "one-shot", "4 KiB", "16 KiB", "64 KiB", "1 MiB"
    );
    for (name, bytes) in &cases {
        let decode_time = measure(target, || decode(bytes));
        let one_shot_time = measure(target, || one_shot(bytes));
        let feeds = FEEDS.map(|feed| measure(target, || scan_once(bytes, feed)));
        println!(
            "{:<20} {:>9.2} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>9.3}",
            name,
            bytes.len() as f64 / (1 << 20) as f64,
            ms(decode_time),
            ms(one_shot_time),
            ms(feeds[0]),
            ms(feeds[1]),
            ms(feeds[2]),
            ms(feeds[3]),
        );
    }
}
