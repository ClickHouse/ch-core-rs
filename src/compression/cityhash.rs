//! CityHash128, the v1.0.2 variant ClickHouse uses for compressed-block
//! checksums.
//!
//! ClickHouse froze CityHash at the pre-v1.0.3 algorithm because the values are
//! persisted on the wire and in data parts and the v1.0.3 release changed the
//! algorithm. This is a direct port of the vendored server source at
//! `contrib/cityhash102/{include/city.h,src/city.cc}` (namespace
//! `CityHash_v1_0_2`), tag v26.8.1.2041-lts. Only the paths the 128-bit
//! checksum actually exercises are ported: `CityHash128`,
//! `CityHash128WithSeed`, `CityMurmur`, `Hash128to64`, and their helpers. The
//! SSE4.2 `CityHashCrc*` paths are not used for this checksum and are omitted.
//!
//! All arithmetic uses `wrapping_*` to reproduce C++ unsigned overflow, and
//! `Rotate` maps to `u64::rotate_right`. `Fetch64`/`Fetch32` are little-endian
//! unaligned loads, matching the server on little-endian targets (its only
//! supported targets for this code).

/// CityHash 128-bit result. `low64` then `high64`, the same field order the
/// server's `uint128` uses and the order the wire stores them in: low64 LE,
/// then high64 LE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct U128 {
    pub(super) low64: u64,
    pub(super) high64: u64,
}

impl U128 {
    #[inline]
    fn new(low64: u64, high64: u64) -> Self {
        Self { low64, high64 }
    }
}

// Some primes between 2^63 and 2^64 for various uses.
const K0: u64 = 0xc3a5c85c97cb3127;
const K1: u64 = 0xb492b66fbe98f273;
const K2: u64 = 0x9ae16a3b2f90404f;
const K3: u64 = 0xc949d7c7509e6557;

/// Little-endian unaligned 64-bit load. `s[off..off + 8]` must be in bounds;
/// every caller in this module satisfies that from the length checks the
/// algorithm's branch structure guarantees.
#[inline]
fn fetch64(s: &[u8], off: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&s[off..off + 8]);
    u64::from_le_bytes(buf)
}

/// Little-endian unaligned 32-bit load, zero-extended to `u64` as the C++
/// `Fetch32` result is when used in 64-bit arithmetic.
#[inline]
fn fetch32(s: &[u8], off: usize) -> u64 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&s[off..off + 4]);
    u32::from_le_bytes(buf) as u64
}

/// Bitwise right rotate. C++ `Rotate` returns `val` unchanged for shift 0, and
/// `rotate_right(0)` does the same, so no special case is needed.
#[inline]
fn rotate(val: u64, shift: u32) -> u64 {
    val.rotate_right(shift)
}

/// C++ `RotateByAtLeast1`: a plain right rotate the algorithm only ever calls
/// with a nonzero shift. Folded onto `rotate` since `rotate_right` is already
/// well defined at every shift.
#[inline]
fn rotate_by_at_least_1(val: u64, shift: u32) -> u64 {
    val.rotate_right(shift)
}

#[inline]
fn shift_mix(val: u64) -> u64 {
    val ^ (val >> 47)
}

/// Hash 128 input bits down to 64 bits. Murmur-inspired mixer from `city.h`.
#[inline]
fn hash_128_to_64(x: U128) -> u64 {
    const K_MUL: u64 = 0x9ddfea08eb382d69;
    let mut a = (x.low64 ^ x.high64).wrapping_mul(K_MUL);
    a ^= a >> 47;
    let mut b = (x.high64 ^ a).wrapping_mul(K_MUL);
    b ^= b >> 47;
    b = b.wrapping_mul(K_MUL);
    b
}

#[inline]
fn hash_len_16(u: u64, v: u64) -> u64 {
    hash_128_to_64(U128::new(u, v))
}

/// `HashLen0to16` from `city.cc`. `s` must have at least `len` bytes and
/// `len <= 16`.
fn hash_len_0_to_16(s: &[u8], len: usize) -> u64 {
    if len > 8 {
        let a = fetch64(s, 0);
        let b = fetch64(s, len - 8);
        // RotateByAtLeast1(b + len, len): len is 9..=16 here, never zero.
        return hash_len_16(
            a,
            rotate_by_at_least_1(b.wrapping_add(len as u64), len as u32),
        ) ^ b;
    }
    if len >= 4 {
        let a = fetch32(s, 0);
        return hash_len_16((len as u64).wrapping_add(a << 3), fetch32(s, len - 4));
    }
    if len > 0 {
        let a = s[0] as u32;
        let b = s[len >> 1] as u32;
        let c = s[len - 1] as u32;
        let y = a.wrapping_add(b << 8);
        let z = (len as u32).wrapping_add(c << 2);
        return shift_mix((y as u64).wrapping_mul(K2) ^ (z as u64).wrapping_mul(K3))
            .wrapping_mul(K2);
    }
    K2
}

/// `WeakHashLen32WithSeeds` (the 6-argument form). Returns `(first, second)`.
#[inline]
fn weak_hash_len_32_with_seeds_vals(
    w: u64,
    x: u64,
    y: u64,
    z: u64,
    mut a: u64,
    mut b: u64,
) -> (u64, u64) {
    a = a.wrapping_add(w);
    b = rotate(b.wrapping_add(a).wrapping_add(z), 21);
    let c = a;
    a = a.wrapping_add(x);
    a = a.wrapping_add(y);
    b = b.wrapping_add(rotate(a, 44));
    (a.wrapping_add(z), b.wrapping_add(c))
}

/// `WeakHashLen32WithSeeds` over `s[off..off + 32]`.
#[inline]
fn weak_hash_len_32_with_seeds(s: &[u8], off: usize, a: u64, b: u64) -> (u64, u64) {
    weak_hash_len_32_with_seeds_vals(
        fetch64(s, off),
        fetch64(s, off + 8),
        fetch64(s, off + 16),
        fetch64(s, off + 24),
        a,
        b,
    )
}

/// `CityMurmur` from `city.cc`. A decent 128-bit hash for any length, seeded.
fn city_murmur(s: &[u8], len: usize, seed: U128) -> U128 {
    let mut a = seed.low64;
    let mut b = seed.high64;
    let mut c: u64;
    let mut d: u64;
    if len <= 16 {
        // l <= 0 branch.
        a = shift_mix(a.wrapping_mul(K1)).wrapping_mul(K1);
        c = b.wrapping_mul(K1).wrapping_add(hash_len_0_to_16(s, len));
        let tail = if len >= 8 { fetch64(s, 0) } else { c };
        d = shift_mix(a.wrapping_add(tail));
    } else {
        // l > 0 branch: a do/while that advances 16 bytes per step while l > 0.
        c = hash_len_16(fetch64(s, len - 8).wrapping_add(K1), a);
        d = hash_len_16(
            b.wrapping_add(len as u64),
            c.wrapping_add(fetch64(s, len - 16)),
        );
        a = a.wrapping_add(d);
        let mut off = 0usize;
        let mut l = len as isize - 16;
        loop {
            a ^= shift_mix(fetch64(s, off).wrapping_mul(K1)).wrapping_mul(K1);
            a = a.wrapping_mul(K1);
            b ^= a;
            c ^= shift_mix(fetch64(s, off + 8).wrapping_mul(K1)).wrapping_mul(K1);
            c = c.wrapping_mul(K1);
            d ^= c;
            off += 16;
            l -= 16;
            if l <= 0 {
                break;
            }
        }
    }
    a = hash_len_16(a, c);
    b = hash_len_16(d, b);
    U128::new(a ^ b, hash_len_16(b, a))
}

/// `CityHash128WithSeed` from `city.cc`.
fn city_hash128_with_seed(s: &[u8], len: usize, seed: U128) -> U128 {
    if len < 128 {
        return city_murmur(s, len, seed);
    }

    // len >= 128 is the common case. Keep 56 bytes of state: v, w, x, y, z.
    let mut x = seed.low64;
    let mut y = seed.high64;
    let mut z = (len as u64).wrapping_mul(K1);
    let mut v0 = rotate(y ^ K1, 49)
        .wrapping_mul(K1)
        .wrapping_add(fetch64(s, 0));
    let mut v1 = rotate(v0, 42).wrapping_mul(K1).wrapping_add(fetch64(s, 8));
    let mut w0 = rotate(y.wrapping_add(z), 35)
        .wrapping_mul(K1)
        .wrapping_add(x);
    let mut w1 = rotate(x.wrapping_add(fetch64(s, 88)), 53).wrapping_mul(K1);

    // Same inner loop as CityHash64, manually unrolled into two 64-byte steps.
    let mut off = 0usize;
    let mut remaining = len;
    loop {
        // Step 1.
        x = rotate(
            x.wrapping_add(y)
                .wrapping_add(v0)
                .wrapping_add(fetch64(s, off + 16)),
            37,
        )
        .wrapping_mul(K1);
        y = rotate(y.wrapping_add(v1).wrapping_add(fetch64(s, off + 48)), 42).wrapping_mul(K1);
        x ^= w1;
        y ^= v0;
        z = rotate(z ^ w0, 33);
        let v = weak_hash_len_32_with_seeds(s, off, v1.wrapping_mul(K1), x.wrapping_add(w0));
        v0 = v.0;
        v1 = v.1;
        let w = weak_hash_len_32_with_seeds(s, off + 32, z.wrapping_add(w1), y);
        w0 = w.0;
        w1 = w.1;
        std::mem::swap(&mut z, &mut x);
        off += 64;
        // Step 2.
        x = rotate(
            x.wrapping_add(y)
                .wrapping_add(v0)
                .wrapping_add(fetch64(s, off + 16)),
            37,
        )
        .wrapping_mul(K1);
        y = rotate(y.wrapping_add(v1).wrapping_add(fetch64(s, off + 48)), 42).wrapping_mul(K1);
        x ^= w1;
        y ^= v0;
        z = rotate(z ^ w0, 33);
        let v = weak_hash_len_32_with_seeds(s, off, v1.wrapping_mul(K1), x.wrapping_add(w0));
        v0 = v.0;
        v1 = v.1;
        let w = weak_hash_len_32_with_seeds(s, off + 32, z.wrapping_add(w1), y);
        w0 = w.0;
        w1 = w.1;
        std::mem::swap(&mut z, &mut x);
        off += 64;
        remaining -= 128;
        if remaining < 128 {
            break;
        }
    }
    y = y
        .wrapping_add(rotate(w0, 37).wrapping_mul(K0))
        .wrapping_add(z);
    x = x.wrapping_add(rotate(v0.wrapping_add(z), 49).wrapping_mul(K0));
    // If 0 < remaining < 128, hash up to 4 chunks of 32 bytes from the end. The
    // C++ tail walks `s + len - tail_done` relative to the original `s`; `off`
    // here is the consumed prefix, so `off + remaining` equals the original
    // `len` offset into `s`.
    let tail_base = off + remaining;
    let mut tail_done = 0usize;
    while tail_done < remaining {
        tail_done += 32;
        y = rotate(y.wrapping_sub(x), 42)
            .wrapping_mul(K0)
            .wrapping_add(v1);
        w0 = w0.wrapping_add(fetch64(s, tail_base - tail_done + 16));
        x = rotate(x, 49).wrapping_mul(K0).wrapping_add(w0);
        w0 = w0.wrapping_add(v0);
        let v = weak_hash_len_32_with_seeds(s, tail_base - tail_done, v0, v1);
        v0 = v.0;
        v1 = v.1;
    }
    x = hash_len_16(x, v0);
    y = hash_len_16(y, w0);
    U128::new(
        hash_len_16(x.wrapping_add(v1), w1).wrapping_add(y),
        hash_len_16(x.wrapping_add(w1), y.wrapping_add(v1)),
    )
}

/// CityHash128 (v1.0.2), the function ClickHouse uses for compressed-block
/// checksums. Returns `{ low64, high64 }`.
pub(super) fn city_hash128(s: &[u8]) -> U128 {
    let len = s.len();
    if len >= 16 {
        city_hash128_with_seed(
            &s[16..],
            len - 16,
            U128::new(fetch64(s, 0) ^ K3, fetch64(s, 8)),
        )
    } else if len >= 8 {
        // CityHash128WithSeed(NULL, 0, ...): the seed is built from s, then a
        // zero-length body runs the CityMurmur len <= 16 path. The empty body
        // slice is never indexed because that path reads no bytes for len 0.
        city_hash128_with_seed(
            &[],
            0,
            U128::new(
                fetch64(s, 0) ^ (len as u64).wrapping_mul(K0),
                fetch64(s, len - 8) ^ K1,
            ),
        )
    } else {
        city_hash128_with_seed(s, len, U128::new(K0, K1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known-answer vectors generated by compiling the exact vendored server
    // source at .server-src/contrib/cityhash102/{include/city.h,src/city.cc}
    // (namespace CityHash_v1_0_2, tag v26.8.1.2041-lts) and printing
    // CityHash128(input).low64 / .high64 for each input. The harness called the
    // vendored CityHash128 directly, so these assert the Rust port reproduces
    // the server's algorithm bit for bit across every length branch.
    struct Vec128 {
        input: Vec<u8>,
        low64: u64,
        high64: u64,
    }

    fn vectors() -> Vec<Vec128> {
        vec![
            // len 0: CityMurmur(s, 0, {K0, K1}) path.
            Vec128 {
                input: b"".to_vec(),
                low64: 0x3df09dfc64c09a2b,
                high64: 0x3cb540c392e51e29,
            },
            // len 1..=3: HashLen0to16 byte path.
            Vec128 {
                input: b"a".to_vec(),
                low64: 0xd27139a1afe01ad0,
                high64: 0xfd7e8ee2e4c86cf6,
            },
            Vec128 {
                input: b"abc".to_vec(),
                low64: 0x900ff195577748fe,
                high64: 0x13a9176355b20d7e,
            },
            // len 4..=7: HashLen0to16 32-bit path.
            Vec128 {
                input: b"1234567".to_vec(),
                low64: 0x8e8777e875b97597,
                high64: 0x8852d1373eb8453b,
            },
            // len 8: CityHash128 len>=8 (<16) seed path into CityMurmur.
            Vec128 {
                input: b"12345678".to_vec(),
                low64: 0x1d7757a6dd6fd9f3,
                high64: 0x7282a0f9c3847a18,
            },
            // len 9..=15: same >=8 seed path, exercises HashLen0to16 len>8.
            Vec128 {
                input: b"123456789".to_vec(),
                low64: 0xc43e148ab1947224,
                high64: 0xc0d05d34783ec23a,
            },
            Vec128 {
                input: b"hello, world".to_vec(),
                low64: 0x07b1912f8e71f73d,
                high64: 0x30e5f73c73dfa645,
            },
            // len 16: CityHash128 len>=16 seed path, body len 0 CityMurmur.
            Vec128 {
                input: b"0123456789abcdef".to_vec(),
                low64: 0xc52ea1adb29e4800,
                high64: 0x7369a2fab076de4c,
            },
            // len 43: CityMurmur len>16 do/while loop.
            Vec128 {
                input: b"The quick brown fox jumps over the lazy dog".to_vec(),
                low64: 0x69102202d326a2fd,
                high64: 0xe4b1346bbee531a1,
            },
            // len 64: CityMurmur len>16 (body 48) longer loop.
            Vec128 {
                input: (0..64u8).map(|i| b'0' + (i % 10)).collect(),
                low64: 0x028eb69e8794a562,
                high64: 0x807776f030d4219f,
            },
            // len 128: the 16-byte CityHash128 seed prefix leaves 112 bytes,
            // which stays on the CityMurmur path.
            Vec128 {
                input: (0..128u8).map(|i| b'A' + (i % 26)).collect(),
                low64: 0x292f5e1651d7427e,
                high64: 0x1251bc99ab918911,
            },
            // len 144: the 16-byte CityHash128 seed prefix leaves exactly 128
            // bytes for CityHash128WithSeed, triggering its main loop.
            Vec128 {
                input: (0..144u16).map(|i| b'A' + (i % 26) as u8).collect(),
                low64: 0x1aabfe9cdaca31ab,
                high64: 0xc49a6d70ac87c24d,
            },
            // len 200: main loop plus the 0 < len < 128 tail chunks.
            Vec128 {
                input: (0..200u16).map(|i| i as u8).collect(),
                low64: 0x275774699e23b6fd,
                high64: 0x5682ef01ebe7289c,
            },
            // len 300: two main-loop rounds plus a nonempty tail.
            Vec128 {
                input: (0..300u16).map(|i| i as u8).collect(),
                low64: 0x8f4ad51c9a584560,
                high64: 0xa8f4769716bb690a,
            },
            // An 18-byte buffer shaped like a real LZ4 compressed-block header
            // (method 0x82, LE sizes, then 9 payload bytes), the kind of input
            // the checksum runs over in practice.
            Vec128 {
                input: vec![
                    0x82, 0x12, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x10, 0x11, 0x12, 0x13,
                    0x14, 0x15, 0x16, 0x17, 0x18,
                ],
                low64: 0x9758b2a0356a3b00,
                high64: 0x3ca431877162c4a2,
            },
        ]
    }

    #[test]
    fn cityhash128_matches_server_v1_0_2_vectors() {
        for v in vectors() {
            let h = city_hash128(&v.input);
            assert_eq!(
                h.low64,
                v.low64,
                "low64 mismatch for len {} input {:?}",
                v.input.len(),
                &v.input
            );
            assert_eq!(
                h.high64,
                v.high64,
                "high64 mismatch for len {} input {:?}",
                v.input.len(),
                &v.input
            );
        }
    }
}
