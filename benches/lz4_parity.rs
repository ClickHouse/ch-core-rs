//! LZ4 block parity bench: this crate's encoder and decoder against liblz4.
//!
//! Run with `cargo bench --bench lz4_parity`. Plain `harness = false` timing,
//! best of several batches, on 1 MiB inputs. Every decoder decodes the same
//! liblz4-produced block, so decode numbers compare like for like.
//!
//! The crate keeps its block codec private, so this bench compiles
//! `src/compression/lz4.rs` directly as a module, with a stand-in for the one
//! error type it uses. That measures exactly the shipped block code without
//! the frame header and CityHash checksum around it.

use std::hint::black_box;
use std::time::Instant;

#[allow(dead_code)]
#[path = "../src/compression/lz4.rs"]
mod lz4_block;

/// Stand-in for `ch_core_rs::compression::CompressionError`, covering the
/// variants the block codec constructs.
// Keep these variants in sync with `CompressionError` in src/compression/mod.rs.
#[allow(dead_code)]
#[derive(Debug)]
enum CompressionError {
    Lz4Malformed,
    SizeMismatch { expected: usize, actual: usize },
    AllocationFailed { requested: usize },
}

const INPUT_SIZE: usize = 1 << 20;
const WARMUP: usize = 5;
const BATCHES: usize = 9;
const ITERS: usize = 30;

fn xorshift(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Column-major Native-like data: u64 ids, DateTime, `user_N` strings, f64
/// noise, a LowCardinality u8 key, a null mask, and i32.
fn realistic(target: usize) -> Vec<u8> {
    let rows = target / 36;
    let mut o = Vec::with_capacity(target + 64);
    for i in 0..rows {
        o.extend_from_slice(&(1_000_000u64 + i as u64).to_le_bytes());
    }
    for i in 0..rows {
        o.extend_from_slice(&(1_700_000_000u32 + (i as u32) / 7).to_le_bytes());
    }
    for i in 0..rows {
        let s = format!("user_{}", i * 7919 % 100_000);
        varint(s.len() as u64, &mut o);
        o.extend_from_slice(s.as_bytes());
    }
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..rows {
        let f = (xorshift(&mut x) % 100_000) as f64 / 100.0;
        o.extend_from_slice(&f.to_le_bytes());
    }
    for _ in 0..rows {
        o.push((xorshift(&mut x) % 5) as u8);
    }
    for _ in 0..rows {
        o.push((xorshift(&mut x) % 10 == 0) as u8);
    }
    for i in 0..rows {
        o.extend_from_slice(&((i as i32) * 3 - 500).to_le_bytes());
    }
    o.resize(target, 0);
    o
}

/// Sorted DateTime64 with small jittered deltas, LowCardinality u8 runs, a
/// 12-word string enum, u16 status codes, and small u32 values.
fn sorted_lowcard(target: usize) -> Vec<u8> {
    let rows = target / 24;
    let mut o = Vec::with_capacity(target + 64);
    let mut x: u64 = 42;
    let mut t: u64 = 1_700_000_000_000;
    for _ in 0..rows {
        t += xorshift(&mut x) % 50;
        o.extend_from_slice(&t.to_le_bytes());
    }
    let mut k = 0u8;
    for _ in 0..rows {
        if xorshift(&mut x) % 8 == 0 {
            k = (xorshift(&mut x) % 20) as u8;
        }
        o.push(k);
    }
    let words = [
        "GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH", "TRACE", "CONNECT", "LIST",
        "SYNC", "PURGE",
    ];
    for _ in 0..rows {
        let w = words[(xorshift(&mut x) % 100 % 12) as usize];
        varint(w.len() as u64, &mut o);
        o.extend_from_slice(w.as_bytes());
    }
    let codes = [200u16, 200, 200, 200, 200, 304, 404, 500];
    for _ in 0..rows {
        o.extend_from_slice(&codes[(xorshift(&mut x) % 8) as usize].to_le_bytes());
    }
    for _ in 0..rows {
        o.extend_from_slice(&((xorshift(&mut x) % 3000) as u32).to_le_bytes());
    }
    o.resize(target, 0);
    o
}

/// A real Native fixture tiled to the target size: long, far matches.
fn repeat(target: usize) -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/all_types_rev54460.native"
    );
    let tile = std::fs::read(path).expect("fixture present");
    let mut o = Vec::with_capacity(target + tile.len());
    while o.len() < target {
        o.extend_from_slice(&tile);
    }
    o.truncate(target);
    o
}

/// Incompressible bytes.
fn random(target: usize) -> Vec<u8> {
    let mut x: u64 = 0x0123_4567;
    (0..target).map(|_| xorshift(&mut x) as u8).collect()
}

/// Throughput in MB/s (10^6 bytes) of plaintext, best batch.
fn bench(bytes: usize, mut f: impl FnMut()) -> f64 {
    for _ in 0..WARMUP {
        f();
    }
    let mut best = f64::MAX;
    for _ in 0..BATCHES {
        let t = Instant::now();
        for _ in 0..ITERS {
            f();
        }
        best = best.min(t.elapsed().as_secs_f64() / ITERS as f64);
    }
    bytes as f64 / best / 1e6
}

fn main() {
    // `cargo test --benches` / `--all-targets` also runs this binary, without
    // `--bench`. Then only the correctness cross-checks run, not the timing.
    let timing = std::env::args().any(|a| a == "--bench");
    let cases = [
        ("realistic", realistic(INPUT_SIZE)),
        ("sorted_lowcard", sorted_lowcard(INPUT_SIZE)),
        ("repeat", repeat(INPUT_SIZE)),
        ("random", random(INPUT_SIZE)),
    ];

    if timing {
        println!(
            "{:<15} {:>9} {:>9} {:>6} | {:>9} {:>9} {:>6} | {:>7} {:>7}",
            "input",
            "comp ours",
            "liblz4",
            "ratio",
            "dec ours",
            "liblz4",
            "ratio",
            "sz ours",
            "liblz4"
        );
    }
    for (name, plain) in &cases {
        let n = plain.len();
        let bound = lz4::block::compress_bound(n).expect("bound");

        let mut ours_block = Vec::with_capacity(bound);
        lz4_block::compress_block_into(plain, &mut ours_block);
        let c_block = lz4::block::compress(plain, None, false).expect("liblz4 compress");
        // Cross-check both directions before timing anything.
        assert_eq!(
            lz4::block::decompress(&ours_block, Some(n as i32)).expect("liblz4 decodes ours"),
            *plain
        );
        assert_eq!(
            lz4_block::decompress_block(&c_block, n).expect("ours decodes liblz4"),
            *plain
        );
        if !timing {
            continue;
        }

        let mut out = Vec::with_capacity(bound);
        let comp_ours = bench(n, || {
            out.clear();
            lz4_block::compress_block_into(black_box(plain), &mut out);
            black_box(&out);
        });
        let mut cbuf = vec![0u8; bound];
        let comp_c = bench(n, || {
            let k = lz4::block::compress_to_buffer(black_box(plain), None, false, &mut cbuf)
                .expect("liblz4 compress");
            black_box(k);
        });

        // Ours appends into a reused Vec, the `decompress_frame_into` shape.
        let mut dec = Vec::with_capacity(n);
        let dec_ours = bench(n, || {
            dec.clear();
            lz4_block::decompress_block_into(black_box(&c_block), n, &mut dec).expect("decode");
            black_box(&dec);
        });
        let mut dbuf = vec![0u8; n];
        let dec_c = bench(n, || {
            let k =
                lz4::block::decompress_to_buffer(black_box(&c_block), Some(n as i32), &mut dbuf)
                    .expect("liblz4 decode");
            black_box(k);
        });

        println!(
            "{:<15} {:>9.0} {:>9.0} {:>5.0}% | {:>9.0} {:>9.0} {:>5.0}% | {:>7} {:>7}",
            name,
            comp_ours,
            comp_c,
            100.0 * comp_ours / comp_c,
            dec_ours,
            dec_c,
            100.0 * dec_ours / dec_c,
            ours_block.len(),
            c_block.len(),
        );
    }
}
