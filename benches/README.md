# Benchmarks

## Native block scanner

Run the incremental scanner benchmark in release mode:

```sh
CH_CORE_BENCH_MS=150 cargo bench --bench block_scanner
```

The default run synthesizes 1,000,000-row columns through the public column
model and encodes each one with `encode_block` before timing. It covers a wide
String, LowCardinality(String), Dynamic(String), structured JSON with a Dynamic
path, fixed-width AggregateFunction(sum), varint AggregateFunction(count),
AggregateFunction(sum, Nullable(Int32)), UInt64, and a prefix-heavy Dynamic
whose runtime type is an Enum16 with 4,096 long labels. No live server or
fixture file is required.

The runtime-shape follow-up cases add `DynamicMultiType`, whose rows rotate
through six block-local alternatives including SharedVariant, and two wide JSON
forms. `JSONWideV1` and `JSONWideFlattened` each carry 12 typed paths and 8
sparse Dynamic(String) paths. The V1 case also carries one shared-data overflow
pair every 32 rows. Filtered runs construct only matching cases. Full runs
encode source batches one at a time, so adding a shape does not retain all of
the source columns simultaneously. Every generated block is materialized by
the decoder during the untimed correctness pass before measurements begin.

Each row reports whole-buffer decode, one-shot boundary scan, and incremental
scan with 4 KiB, 16 KiB, 64 KiB, and 1 MiB feeds. Incremental calls borrow
successively longer prefixes of the same encoded allocation. The benchmark does
not copy the input once per feed. Timing uses the best of five calibrated
batches. If one operation already exceeds the target cell time, it uses the
best of three single-operation samples so a known quadratic baseline remains
practical to measure.

Useful controls:

```sh
# Run one named case.
CH_CORE_BENCH_FILTER=Dynamic CH_CORE_BENCH_MS=500 \
  cargo bench --bench block_scanner

# Change generated shape sizes.
CH_CORE_BENCH_ROWS=250000 CH_CORE_BENCH_PREFIX_LABELS=1024 \
  cargo bench --bench block_scanner
```

When Cargo executes the bench target without `--bench`, such as under
`cargo test --all-targets`, it only performs correctness scans over 1,024 rows
and prints no timing table.

### Scanner checkpoint result

Representative results from 2026-11-09 on an Apple M4 Max with rustc 1.96.0
(`aarch64-apple-darwin`) are below. Times are milliseconds. The baseline is
commit `749782b`. Dynamic, JSON, and DynamicWideEnum are from the 150 ms full
run. The two AggregateFunction rows use the final 500 ms focused reruns.

| Column | Baseline 4 KiB | Final one-shot | Final 4 KiB | Final 16 KiB | Final 64 KiB | Final 1 MiB |
|---|---:|---:|---:|---:|---:|---:|
| Dynamic | 11602.080 | 2.003 | 2.123 | 2.000 | 1.956 | 1.976 |
| JSON | 16441.215 | 2.419 | 2.704 | 2.492 | 2.441 | 2.469 |
| DynamicWideEnum | 1150.001 | 2.037 | 2.107 | 2.105 | 2.111 | 2.063 |
| AggregateCount | 730.507 | 1.623 | 1.632 | 1.630 | 1.629 | 1.674 |
| AggregateNullableSum | 292.296 | 0.388 | 0.456 | 0.437 | 0.434 | 0.430 |

The controls remained feed-size independent. In the full final run, the
31.47 MiB String case ranged from 1.401 to 1.569 ms and the 0.95 MiB
LowCardinality(String) case ranged from 0.508 to 0.512 ms.

The runtime-shape and scanner-memory follow-up used 500 ms focused cells on
the same machine and toolchain. These cases were measured after counted body
runs and boxed prefix tasks removed path-count-proportional scheduler growth.

| Column | MiB | Decode | One-shot | 4 KiB | 16 KiB | 64 KiB | 1 MiB |
|---|---:|---:|---:|---:|---:|---:|---:|
| DynamicMultiType | 8.58 | 2.673 | 1.033 | 1.077 | 1.047 | 1.039 | 1.046 |
| JSONWideV1 | 39.82 | 16.046 | 5.189 | 5.501 | 5.149 | 5.018 | 5.087 |
| JSONWideFlattened | 31.47 | 14.224 | 4.447 | 4.572 | 4.551 | 4.517 | 4.568 |
| String control | 31.47 | 5.886 | 1.402 | 1.592 | 1.434 | 1.420 | 1.432 |

All three new runtime-shaped cases stay close to one-shot cost from 4 KiB
through 1 MiB feeds. An independent allocator probe over 100,000 FLATTENED
paths measured 8.74 MB peak live allocation after the scheduler change versus
53.14 MB before it, while scanning the same 2.2 MB wire block. These memory
figures use decimal MB.
