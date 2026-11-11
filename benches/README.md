# Benchmarks

## Native block scanner

Run the incremental scanner benchmark in release mode:

```sh
CH_CORE_BENCH_MS=150 cargo bench --bench block_scanner
```

The default run synthesizes 250,000-row and 1,000,000-row columns through the public column
model and encodes each one with `encode_block` before timing. It covers a wide
String, LowCardinality(String) with 1-, 2-, 4-, and 8-byte indexes (the wider
widths are byte-patched after `encode_block`, which selects UInt8 here),
Dynamic(String), structured JSON with a Dynamic path, fixed-width
AggregateFunction(sum), varint AggregateFunction(count),
AggregateFunction(sum, Nullable(Int32)), UInt64, and a prefix-heavy Dynamic
whose runtime type is an Enum16 with 4,096 long labels. No live server or
fixture file is required. The additional `TopLevelEnum` and `LongName` cases
exercise a top-level Enum8 with 256 long labels and a UInt64 column with a
1 MiB name. `CombinedHeader` puts both large name and Enum descriptor on the
same column. `MixedColumns` combines UInt64, Bool, Nullable(Int64), and wide
String columns to exercise checkpoint transitions. `JSONManyPaths` uses one row and one dynamic path for every ten
configured rows, giving 25,000 and 100,000 paths in the default run.

The runtime-shape follow-up cases add `DynamicMultiType`, whose rows rotate
through six block-local alternatives including SharedVariant, and two wide JSON
forms. `JSONWideV1` and `JSONWideFlattened` each carry 12 typed paths and 8
sparse Dynamic(String) paths. The V1 case also carries one shared-data overflow
pair every 32 rows. Filtered runs construct only matching cases. Full runs
encode source batches one at a time, so adding a shape does not retain all of
the source columns simultaneously. Every generated block is materialized by
the decoder during the untimed correctness pass before measurements begin.

Each size runs text headers at revisions 0 and 54460 and binary headers at
revision 54460. Each row reports whole-buffer decode, one-shot boundary scan,
and incremental scan with 64 B, 4 KiB, 16 KiB, 64 KiB, and 1 MiB feeds.
Incremental calls borrow successively longer prefixes of the same encoded allocation. The benchmark does
not copy the input once per feed. Timing uses the best of five calibrated
batches. If one operation already exceeds the target cell time, it uses the
best of three single-operation samples so a known quadratic baseline remains
practical to measure.

A benchmark-only counting allocator reports peak requested live heap bytes
above the pre-scan baseline and successful allocation/reallocation call counts
for 64 B and 1 MiB feeds, and checks that dropping the scanner restores the
baseline. These samples include all scanner metadata
and transient allocations while the block is incomplete. They exclude input
storage, source batches, allocator overhead, and RSS. Allocation accounting
is active during timings too, so compare results made with the same harness.

Useful controls:

```sh
# Run one named case.
CH_CORE_BENCH_FILTER=Dynamic CH_CORE_BENCH_MS=500 \
  cargo bench --bench block_scanner

# Change generated shape sizes.
CH_CORE_BENCH_ROWS=250000 CH_CORE_BENCH_PREFIX_LABELS=1024 \
  cargo bench --bench block_scanner

# Measure TCP INSERT validation and output copying for all LC index widths.
CH_CORE_BENCH_ROWS=1048576 CH_CORE_BENCH_MS=150 \
  cargo bench -p ch-tcp-proto --bench insert_data
```

When Cargo executes the bench target without `--bench`, such as under
`cargo test --all-targets`, it only performs correctness scans over 256 and
1,024 rows in all three modes and prints no timing table.

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
LowCardinality(String) case ranged from 0.508 to 0.512 ms. That case is
today's `LowCardinalityW1`.

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

### PR #10 header and retained-name fixes

The 2026-11-09 fix round also measures large top-level headers, both header
modes at revision 54460, and 64 B feeds. On the same M4 Max and rustc 1.96.0,
release measurements with 1,000,000 rows gave the following times in ms:

| Case | Mode | 64 B | 4 KiB | 1 MiB |
|---|---|---:|---:|---:|
| UInt64, c437bbe baseline | text | 17.112 | 0.263 | 0.001 |
| UInt64, fixed | text | 0.854 | 0.013 | <0.001 |
| CombinedHeader, fixed | text | 0.479 | 0.129 | 0.107 |
| CombinedHeader, fixed | binary | 0.584 | 0.220 | 0.203 |

`CombinedHeader` combines a 1 MiB name with a 256-label Enum8 descriptor.
Completed names, descriptors, and fixed-width body endpoints survive feeds.
The plain UInt64 scan performs two allocations in either header mode regardless
of feed size, and does not allocate per body feed.
Incomplete headers retain the same boxed state across resumes.

The follow-up header review removed partial-string copies and uses the ordinary
binary type parser directly when a descriptor is already complete. A 1 MiB
column name now peaks at 1.05 MB of requested scanner heap in both header modes
at 64 B feeds, with three allocations including the completed name. Partial
names and type strings retain offsets into the caller's buffer instead of
owning another copy. A focused 5 ms rerun on the same machine measured the
million-row UInt64 case at 0.852 ms text and 0.838 ms binary with 64 B feeds.

For `JSONManyPaths` with 100,000 paths and 2.29 MiB of wire data, peak requested
scanner heap at 64 B feeds fell from 9.59 MB to 5.24 MB in text mode at revision
54460. Binary mode fell from 9.30 MB to 5.24 MB. The scanner retains the path
count and only the current name needed for order validation. These are heap
measurements, not RSS. The comparison uses the same counting-allocator harness
on c437bbe and the fix tree; timing cells used 5 ms for UInt64/JSON and 10 ms for
CombinedHeader, so small timing differences should not be over-interpreted.

### LowCardinality index validation

Results from 2026-11-11 compare baseline commit `7504c58` with the width-specialized
validator. The machine was an Apple M4 Max running macOS 26.7.2, with rustc
1.96.0 on `aarch64-apple-darwin`. Cargo used its default bench profile with no
LTO. Each input had 1,048,576 rows, the same four-entry String dictionary, and
the same logical indexes. Only the self-describing index width changed. Times
are milliseconds for text headers at revision 54460. Baseline and candidate
ran back-to-back from separate source and target directories. The scanner used
`CH_CORE_BENCH_FILTER=LowCardinality CH_CORE_BENCH_ROWS=1048576
CH_CORE_BENCH_MS=50`; the INSERT benchmark used `CH_CORE_BENCH_ROWS=1048576
CH_CORE_BENCH_MS=150`. Each benchmark reports the best of five calibrated
batches, as described above. Each cell below is the best of three such
scanner runs, or two INSERT runs, per tree. Every timed operation re-reads the
same buffer, so inputs are cache-resident and the speedups are upper bounds.

| Width | MiB | One-shot before | One-shot after | 64 B before | 64 B after | 4 KiB before | 4 KiB after | 1 MiB before | 1 MiB after |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1.00 | 0.476 | 0.010 | 0.660 | 0.269 | 0.504 | 0.016 | 0.477 | 0.010 |
| 2 | 2.00 | 0.481 | 0.020 | 0.923 | 0.479 | 0.487 | 0.030 | 0.480 | 0.021 |
| 4 | 4.00 | 0.484 | 0.041 | 1.319 | 0.969 | 0.501 | 0.066 | 0.488 | 0.041 |
| 8 | 8.00 | 0.485 | 0.084 | 2.144 | 1.922 | 0.514 | 0.118 | 0.478 | 0.084 |

One-shot speedups are about 48x, 24x, 12x, and 5.8x for widths 1, 2, 4, and 8.
At 64 B feeds per-call scanner overhead dominates, and the gains are 2.5x,
1.9x, 1.4x, and 1.1x.

`Connection::insert_data` scans the same block and copies it into a Data packet.
The focused benchmark includes `take_output`, so every operation starts with an
empty output buffer and includes its one block-sized allocation.

| Width | Before | After | Speedup | Peak requested bytes | Allocations |
|---:|---:|---:|---:|---:|---:|
| 1 | 0.525 | 0.023 | 22.8x | 1,048,671 | 8 |
| 2 | 0.543 | 0.046 | 11.8x | 2,097,247 | 8 |
| 4 | 0.559 | 0.091 | 6.1x | 4,194,399 | 8 |
| 8 | 0.616 | 0.185 | 3.3x | 8,388,703 | 8 |

Peak requested heap and allocation counts were identical before and after for
both scanner feeds and `insert_data`. The change adds no allocations or copies.
The index validator performs one max reduction over each available index run
and one validity branch per run on valid data. The row-by-row error walk remains
cold. Full allocating decode was unchanged and stayed near 0.8, 1.0, 1.0, and
1.4 ms for widths 1, 2, 4, and 8.
