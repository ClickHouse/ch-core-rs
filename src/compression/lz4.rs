//! LZ4 block compression, the standard block format ClickHouse emits.
//!
//! ClickHouse compresses with `LZ4_compress_default` and decompresses with its
//! own tuned `LZ4::decompress` (see `src/Compression/CompressionCodecLZ4.cpp`
//! and `LZ4_decompress_faster.cpp`, tag v26.8.1.2041-lts). Both read and write
//! the standard LZ4 block format, so a correct standard decompressor is
//! byte-for-byte compatible with the server's output.
//!
//! Block format: a sequence of sequences. Each sequence is a one-byte token
//! whose high nibble is the literal length and low nibble is the match length.
//! A nibble value of 15 means "read more length bytes", each adding its value
//! and continuing while the byte is 255. After the literals come a 2-byte
//! little-endian match offset (distance back into already-written output) and a
//! match of `match_length + 4` bytes copied from that offset. An offset smaller
//! than the match length overlaps the bytes being written, which is how LZ4
//! encodes run-length expansion. The final sequence is literals only with no
//! match.
//!
//! The decoder is hardened against malformed input: every length is checked
//! against the source and against `uncompressed_size` before any copy, and it
//! never panics. It is entirely safe Rust. For speed, short literal runs and
//! short matches are copied in fixed 8- and 16-byte chunks that may write a few
//! bytes past the end of the current run, into output a later sequence
//! overwrites. Those bytes must be initialized, so the decoder keeps the output
//! `Vec`'s length as an initialized frontier a little ahead of the write
//! position, zero-filling it in small windows. Long literal runs and long
//! matches are appended at the frontier without any fill. The frontier never
//! passes the block's declared end, so no byte beyond that end is initialized,
//! written, or exposed as output.
//!
//! The decoder has two sequence paths, the hot slice loop
//! (`decode_initialized`) and the cold `Vec`-level step
//! (`decode_one_sequence`). They must stay in sync: both parse lengths and
//! offsets through the shared [`read_literal_len`], [`read_match`], and
//! [`check_offset`] helpers, so the validation rules live in one place, and
//! they differ only in how they copy. The guard against drift is the
//! `mutated_blocks_agree_with_reference_and_liblz4` test, which requires both
//! paths (forced to alternate by tiny fill windows) to agree exactly, errors
//! included, with a byte-at-a-time reference decoder.

use super::CompressionError;

const MIN_MATCH: usize = 4;

// ---------------------------------------------------------------------------
// Decompression
// ---------------------------------------------------------------------------

/// Matches longer than this use exact copies (doubling when the match overlaps
/// itself) instead of a loop of fixed-width chunks.
const LONG_MATCH: usize = 64;

/// Literal runs longer than this use one exact copy instead of a loop of
/// 16-byte chunks.
const LONG_LITERALS: usize = 64;

/// How far ahead of a fixed-width copy the initialized frontier is advanced
/// when it falls behind. Small enough to stay in L1, large enough that the
/// refill branch is rare. Unit tests also decode with a tiny, odd window so
/// that every test block crosses the frontier, and the hand-off between the
/// two decode phases, many times.
const FILL_WINDOW: usize = 4096;

/// Decompress one standard LZ4 block.
///
/// `src` is the compressed payload only (the 9-byte ClickHouse header and the
/// 16-byte checksum have already been stripped by the caller). `uncompressed_size`
/// is the exact decompressed length from the block header. On success the
/// returned `Vec` has exactly `uncompressed_size` bytes.
///
/// The output is allocated with at least `uncompressed_size` capacity. The server
/// reserves an extra `ADDITIONAL_BYTES_AT_END_OF_BUFFER` (64) for its wide
/// SIMD tail overwrite. This decoder falls back to exact copies within the last
/// few bytes of the block instead, so it needs no slack.
pub fn decompress_block(src: &[u8], uncompressed_size: usize) -> Result<Vec<u8>, CompressionError> {
    let mut out = Vec::new();
    decompress_block_into(src, uncompressed_size, &mut out)?;
    Ok(out)
}

/// Decompress one standard LZ4 block and append it to `out`.
///
/// The declared output size is checked against a conservative maximum
/// expansion for the payload before reserving. The reservation is fallible,
/// and any error restores `out` to its original length. Matches can only
/// reference bytes this block produced, never earlier contents of `out`.
pub(super) fn decompress_block_into(
    src: &[u8],
    uncompressed_size: usize,
    out: &mut Vec<u8>,
) -> Result<(), CompressionError> {
    decompress_block_into_windowed::<FILL_WINDOW>(src, uncompressed_size, out)
}

/// [`decompress_block_into`] with the frontier fill window `W` as a parameter,
/// so tests can force frequent phase hand-offs.
#[inline(always)]
fn decompress_block_into_windowed<const W: usize>(
    src: &[u8],
    uncompressed_size: usize,
    out: &mut Vec<u8>,
) -> Result<(), CompressionError> {
    // Each LZ4 extension byte can account for at most 255 output bytes. The
    // fixed 24-byte allowance covers token nibbles and MIN_MATCH. This is a
    // deliberately loose upper bound, but it prevents a tiny payload from
    // claiming an enormous output allocation.
    let max_output = src.len().saturating_mul(255).saturating_add(24);
    if uncompressed_size > max_output {
        return Err(CompressionError::Lz4Malformed);
    }

    let base = out.len();
    out.try_reserve(uncompressed_size)
        .map_err(|_| CompressionError::AllocationFailed {
            requested: uncompressed_size,
        })?;
    // The reservation succeeded, so `base + uncompressed_size` is within the
    // capacity and cannot overflow. Every write below stays at or before
    // `end`, so the decoder never reallocates.
    let end = base + uncompressed_size;

    match decompress_block_body::<W>(src, out, base, end) {
        // On success the write position reached `end`, and the frontier is
        // never past `end`, so `out.len() == end` already.
        Ok(op) if op == end => Ok(()),
        Ok(op) => {
            out.truncate(base);
            Err(CompressionError::SizeMismatch {
                expected: uncompressed_size,
                actual: op - base,
            })
        }
        Err(err) => {
            out.truncate(base);
            Err(err)
        }
    }
}

/// Where the hot loop stopped.
enum Hot {
    /// The source is exhausted; the final block-relative write position.
    Done(usize),
    /// The hot loop stopped inside a sequence whose writes do not fit below
    /// the frontier. Its literal length is already parsed, so the slow step
    /// does not re-read a long run of extension bytes.
    Pending(Pending),
}

/// Where the slow step stopped.
enum Slow {
    /// The source is exhausted; the final write position.
    Done(usize),
    /// More sequences follow, starting at source position `sp`, with the next
    /// write at `op`.
    More { sp: usize, op: usize },
}

/// A sequence the hot loop handed back: its token, the source position just
/// past its literal length, the literal length, and its output start.
struct Pending {
    token: u8,
    sp: usize,
    lit_len: usize,
    op: usize,
}

/// Decode `src` into `out[base..end]`, returning the final write position.
///
/// Positions are absolute indices into `out`. `base` is this block's first
/// output byte, so a match offset can never reach an earlier block's
/// plaintext.
///
/// Invariants, held between sequences: `base <= op <= out.len() <= end <=
/// out.capacity()` and `sp <= src.len()`. `out.len()` is the initialized
/// frontier; bytes in `op..out.len()` are scratch that later sequences
/// overwrite.
///
/// Decoding alternates two phases. [`decode_initialized`] runs the hot loop
/// over the initialized slice and handles every sequence whose writes fit
/// below the frontier. When one does not, it hands that sequence back with its
/// literal header parsed, having written only scratch, and
/// [`decode_one_sequence`] finishes it at the `Vec` level, growing the
/// frontier or appending at it.
fn decompress_block_body<const W: usize>(
    src: &[u8],
    out: &mut Vec<u8>,
    base: usize,
    end: usize,
) -> Result<usize, CompressionError> {
    let mut sp = 0usize;
    let mut op = base;
    loop {
        // The hot loop works in block-relative positions on this block's
        // initialized region, so `offset <= op` is the whole base check.
        let mut pending = match decode_initialized(src, &mut out[base..], sp, op - base)? {
            Hot::Done(rel) => return Ok(base + rel),
            Hot::Pending(pending) => pending,
        };
        pending.op += base;
        match decode_one_sequence::<W>(src, out, base, end, pending)? {
            Slow::Done(op) => return Ok(op),
            Slow::More { sp: s, op: o } => (sp, op) = (s, o),
        }
    }
}

/// Hot loop: decode sequences while every write fits in `dst`, this block's
/// initialized region `out[base..frontier]`. Positions here are relative to
/// `base`, so `dst[0]` is the block's first output byte.
///
/// Bounds argument. `op <= dst.len()` and `sp <= src.len()` hold between
/// sequences. Each literal run and match length is checked against
/// `dst.len() - op` and `src.len() - sp` before it is copied, and a
/// fixed-width write or read is only taken after checking its full width. A
/// sequence that fails a room check returns `Hot::Pending` with its parsed
/// literal header and its starting `op`, so the caller can finish it with a
/// larger frontier; if it truly overruns the declared size the caller reports
/// that. Anything the abandoned attempt wrote is at or past that `op`, which
/// is scratch. Match chunks read `start + d..start + d + w` with
/// `offset >= w` (or, for small offsets, with a stride of at least `w`), so
/// each read ends at or before the chunk's own destination start and sees
/// only final bytes, never scratch. The slice bounds checks the compiler keeps
/// are therefore unreachable; they are a safety net, not a code path.
#[inline(always)]
fn decode_initialized(
    src: &[u8],
    dst: &mut [u8],
    mut sp: usize,
    mut op: usize,
) -> Result<Hot, CompressionError> {
    let sl = src.len();
    let dl = dst.len();

    while sp < sl {
        let token = src[sp];
        sp += 1;
        let lit_nibble = usize::from(token >> 4);
        let match_nibble = usize::from(token & 0x0F);

        // Shortcut for the common short sequence, as in liblz4: neither length
        // is extended (literals <= 14, match <= 18), the source has room for a
        // 16-byte read plus the offset, and the frontier is at least 40 bytes
        // ahead. Because `sl - sp >= 18` exceeds the literal length, this
        // cannot be the final literals-only sequence, so an offset follows.
        if lit_nibble < 15 && match_nibble < 15 && sl - sp >= 18 && dl - op >= 40 {
            // 16 bytes to `op + 16 <= dl - 24`.
            dst[op..op + 16].copy_from_slice(&src[sp..sp + 16]);
            sp += lit_nibble;
            op += lit_nibble;
            // `sp + 2 <= old sp + 16 <= sl`. `op + 26 <= dl` from here, so the
            // match (at most 18 bytes) has 8 bytes of room past its end.
            let offset = usize::from(u16::from_le_bytes([src[sp], src[sp + 1]]));
            sp += 2;
            check_offset(offset, op)?;
            let match_len = match_nibble + MIN_MATCH;
            let start = op - offset;
            if offset >= 8 {
                if offset >= 16 {
                    dst.copy_within(start..start + 16, op);
                } else {
                    dst.copy_within(start..start + 8, op);
                    dst.copy_within(start + 8..start + 16, op + 8);
                }
                if match_len > 16 {
                    // Reads `start + 16..start + 18`, ending by `op + 10`,
                    // which the copies above already filled. Writes end at
                    // `op + 18`.
                    dst.copy_within(start + 16..start + 18, op + 16);
                }
            } else {
                copy_small_offset(dst, offset, op, match_len);
            }
            op += match_len;
            continue;
        }

        // General path: extended lengths and sequences near either end.
        let lit_len = read_literal_len(src, &mut sp, token)?;
        let lit_sp = sp;
        if lit_len > dl - op {
            return Ok(Hot::Pending(Pending {
                token,
                sp: lit_sp,
                lit_len,
                op,
            }));
        }
        if lit_len <= 16 && sl - sp >= 16 && dl - op >= 16 {
            dst[op..op + 16].copy_from_slice(&src[sp..sp + 16]);
        } else if lit_len <= LONG_LITERALS && sl - sp - lit_len >= 16 && dl - op - lit_len >= 16 {
            // Medium runs: 16-byte chunks reading up to `sp + lit_len + 15`
            // and writing up to `op + lit_len + 15`, both checked just above.
            let mut d = 0;
            while d < lit_len {
                dst[op + d..op + d + 16].copy_from_slice(&src[sp + d..sp + d + 16]);
                d += 16;
            }
        } else {
            dst[op..op + lit_len].copy_from_slice(&src[sp..sp + lit_len]);
        }
        // Writes at or past `op` only ever touch scratch, so if this sequence
        // is handed back below, the slow step simply rewrites them.
        let seq_op = op;
        sp += lit_len;
        op += lit_len;
        if sp == sl {
            // Final literals-only sequence.
            return Ok(Hot::Done(op));
        }

        // `op` is block-relative here, so it is the bytes produced so far.
        let (offset, match_len) = read_match(src, &mut sp, token, op)?;
        if match_len > dl - op {
            return Ok(Hot::Pending(Pending {
                token,
                sp: lit_sp,
                lit_len,
                op: seq_op,
            }));
        }

        let start = op - offset;
        // Chunks write up to `op + d + w` with `d < match_len` and `w <= 16`.
        if match_len <= LONG_MATCH && dl - op - match_len >= 16 {
            if offset < 8 {
                copy_small_offset(dst, offset, op, match_len);
            } else if offset >= 16 {
                // Each chunk reads `start + d..start + d + 16`, ending at or
                // before `op + d`.
                let mut d = 0;
                while d < match_len {
                    dst.copy_within(start + d..start + d + 16, op + d);
                    d += 16;
                }
            } else {
                let mut d = 0;
                while d < match_len {
                    dst.copy_within(start + d..start + d + 8, op + d);
                    d += 8;
                }
            }
        } else if offset >= match_len {
            // Long or near-the-frontier match without overlap: one exact copy.
            dst.copy_within(start..start + match_len, op);
        } else {
            copy_overlapping_slice(dst, start, op, match_len);
        }
        op += match_len;
    }

    Ok(Hot::Done(op))
}

/// Finish one sequence the hot loop handed back, at the `Vec` level, with
/// `pending.op` now absolute: grow the initialized frontier for fixed-width
/// copies, and append long literal runs and long matches at the frontier
/// without zero filling. Upholds the invariants listed on
/// [`decompress_block_body`].
#[cold]
#[inline(never)]
fn decode_one_sequence<const W: usize>(
    src: &[u8],
    out: &mut Vec<u8>,
    base: usize,
    end: usize,
    pending: Pending,
) -> Result<Slow, CompressionError> {
    let Pending {
        token,
        mut sp,
        lit_len,
        mut op,
    } = pending;
    let sl = src.len();

    // The hot loop parsed this literal length with `read_literal_len`, which
    // already checked it against the source. Only the room check differs:
    // here the bound is the declared end, not the frontier.
    if lit_len > end - op {
        return Err(CompressionError::Lz4Malformed);
    }
    if lit_len <= 16 && sl - sp >= 16 && end - op >= 16 {
        ensure_initialized::<W>(out, op + 16, end);
        out[op..op + 16].copy_from_slice(&src[sp..sp + 16]);
    } else {
        // Long literal runs (incompressible data) or the final bytes of the
        // block: one exact copy, appended at the frontier.
        write_at(out, op, &src[sp..sp + lit_len]);
    }
    sp += lit_len;
    op += lit_len;

    // The last sequence is literals only: if the source is now exhausted, the
    // block ends here. A trailing offset is only present when a match follows.
    if sp == sl {
        return Ok(Slow::Done(op));
    }

    // `op` is absolute here; `op - base` is the bytes this block produced.
    let (offset, match_len) = read_match(src, &mut sp, token, op - base)?;
    if match_len > end - op {
        return Err(CompressionError::Lz4Malformed);
    }

    let start = op - offset;
    if match_len <= LONG_MATCH && offset >= 8 && end - op - match_len >= 16 {
        // A short match: refill a window so the hot loop can resume, then
        // copy exactly (this path is rare, so no chunking).
        ensure_initialized::<W>(out, op + match_len + 16, end);
        copy_overlapping_slice(out, start, op, match_len);
    } else if offset >= match_len {
        copy_within_at(out, start, op, match_len);
    } else {
        copy_overlapping(out, start, op, match_len);
    }
    Ok(Slow::More {
        sp,
        op: op + match_len,
    })
}

/// Grow the initialized frontier to at least `need` (`need <= end`), zero
/// filling up to `W` bytes ahead ([`FILL_WINDOW`] outside tests) but never
/// past `end`.
#[inline(always)]
fn ensure_initialized<const W: usize>(out: &mut Vec<u8>, need: usize, end: usize) {
    if out.len() < need {
        let target = need.max(out.len() + W).min(end);
        // Within the reserved capacity, so this does not reallocate.
        out.resize(target, 0);
    }
}

/// Write `bytes` at `op` (`op <= out.len()`, `op + bytes.len() <= end`),
/// overwriting scratch below the frontier and appending the rest.
#[inline(always)]
fn write_at(out: &mut Vec<u8>, op: usize, bytes: &[u8]) {
    let have = (out.len() - op).min(bytes.len());
    out[op..op + have].copy_from_slice(&bytes[..have]);
    out.extend_from_slice(&bytes[have..]);
}

/// Copy `len` bytes from `start` to `op` where the ranges do not overlap
/// (`start + len <= op <= out.len()`, `op + len <= end`), overwriting scratch
/// below the frontier and appending the rest.
#[inline(always)]
fn copy_within_at(out: &mut Vec<u8>, start: usize, op: usize, len: usize) {
    let have = (out.len() - op).min(len);
    out.copy_within(start..start + have, op);
    // The source ends by `op`, which is at or below the frontier.
    out.extend_from_within(start + have..start + len);
}

/// Copy a match whose source may overlap its destination, writing exactly
/// `match_len` bytes at `op` into an initialized slice
/// (`op + match_len <= dst.len()`).
///
/// The output from `start` is periodic with period `offset`, so each step can
/// copy everything produced so far from `start`, doubling the copy size until
/// the match is complete. Every step's source ends at or before its
/// destination, so each is a non-overlapping copy.
#[inline]
fn copy_overlapping_slice(dst: &mut [u8], start: usize, op: usize, match_len: usize) {
    if op - start == 1 {
        // Offset 1 is a byte run, the most common overlap.
        let byte = dst[start];
        dst[op..op + match_len].fill(byte);
        return;
    }
    let mut done = 0usize;
    while done < match_len {
        let chunk = (match_len - done).min(op + done - start);
        dst.copy_within(start..start + chunk, op + done);
        done += chunk;
    }
}

/// Copy a short match with `offset < 8`, as liblz4 does: expand the first 8
/// bytes one at a time, then continue in 8-byte chunks from a distance that is
/// a multiple of `offset` and at least 8, so each chunk's source ends at or
/// before its destination. Requires `1 <= offset <= op`, `match_len >= 4`, and
/// `op + match_len + 8 <= dst.len()`; writes up to `op + match_len + 7`.
#[inline(always)]
fn copy_small_offset(dst: &mut [u8], offset: usize, op: usize, match_len: usize) {
    /// Smallest multiple of the offset that is at least 8, for offsets 1..=7.
    /// It never exceeds `offset + 8`, so the first chunk's source starts at
    /// or after `op - offset`.
    const STRIDE: [usize; 8] = [0, 8, 8, 9, 8, 10, 12, 14];
    let start = op - offset;
    // Byte at a time, so each read sees the byte just written when the
    // ranges overlap. Writes `op..op + 8`, within `op + match_len + 4`.
    for i in 0..8 {
        dst[op + i] = dst[start + i];
    }
    // The output from `start` is periodic with period `offset`, hence also
    // with period `stride`.
    let stride = STRIDE[offset];
    let mut d = 8;
    while d < match_len {
        dst.copy_within(op + d - stride..op + d - stride + 8, op + d);
        d += 8;
    }
}

/// [`copy_overlapping_slice`] at the `Vec` level: `op <= out.len()` and the
/// match may extend past the frontier, where it is appended.
fn copy_overlapping(out: &mut Vec<u8>, start: usize, op: usize, match_len: usize) {
    if op - start == 1 {
        let byte = out[start];
        let have = (out.len() - op).min(match_len);
        out[op..op + have].fill(byte);
        if have < match_len {
            out.resize(op + match_len, byte);
        }
        return;
    }
    let mut done = 0usize;
    while done < match_len {
        let chunk = (match_len - done).min(op + done - start);
        copy_within_at(out, start, op + done, chunk);
        done += chunk;
    }
}

// ---- Shared sequence parsing. Both decode paths validate through these, so
// ---- the rules cannot drift between them.

/// Parse a sequence's literal length (token high nibble plus any extension)
/// with `sp` just past the token, and check that the literals fit in the
/// source. Leaves `sp` at the first literal byte. Whether the literals fit in
/// the output is the caller's check, because the two paths bound it
/// differently (frontier versus declared end).
#[inline(always)]
fn read_literal_len(src: &[u8], sp: &mut usize, token: u8) -> Result<usize, CompressionError> {
    let mut lit_len = usize::from(token >> 4);
    if lit_len == 15 {
        lit_len = lit_len
            .checked_add(read_length_extension(src, sp)?)
            .ok_or(CompressionError::Lz4Malformed)?;
    }
    // `read_length_extension` never advances `sp` past `src.len()`.
    if lit_len > src.len() - *sp {
        return Err(CompressionError::Lz4Malformed);
    }
    Ok(lit_len)
}

/// Parse a match with `sp` just past its literals: the 2-byte offset,
/// validated against `produced` (bytes this block has written so far), and
/// the match length (token low nibble plus any extension, plus `MIN_MATCH`).
/// Returns `(offset, match_len)`, with `sp` past the match header. Whether the
/// match fits in the output is the caller's check.
#[inline(always)]
fn read_match(
    src: &[u8],
    sp: &mut usize,
    token: u8,
    produced: usize,
) -> Result<(usize, usize), CompressionError> {
    if src.len() - *sp < 2 {
        return Err(CompressionError::Lz4Malformed);
    }
    let offset = usize::from(u16::from_le_bytes([src[*sp], src[*sp + 1]]));
    *sp += 2;
    check_offset(offset, produced)?;
    let mut match_len = usize::from(token & 0x0F);
    if match_len == 15 {
        match_len = match_len
            .checked_add(read_length_extension(src, sp)?)
            .ok_or(CompressionError::Lz4Malformed)?;
    }
    match_len = match_len
        .checked_add(MIN_MATCH)
        .ok_or(CompressionError::Lz4Malformed)?;
    Ok((offset, match_len))
}

/// Reject offset 0 (invalid in the LZ4 block format) and any offset reaching
/// before this block's first output byte, which would read bytes the block
/// did not produce (an earlier frame's plaintext, or nothing at all).
#[inline(always)]
fn check_offset(offset: usize, produced: usize) -> Result<(), CompressionError> {
    if offset == 0 || offset > produced {
        return Err(CompressionError::Lz4Malformed);
    }
    Ok(())
}

/// Read an LZ4 length extension: zero or more bytes, each adding its value to
/// the running length, continuing while the byte is 255. Advances `sp`, never
/// past `src.len()`.
#[inline]
fn read_length_extension(src: &[u8], sp: &mut usize) -> Result<usize, CompressionError> {
    let mut extra = 0usize;
    loop {
        let b = *src.get(*sp).ok_or(CompressionError::Lz4Malformed)?;
        *sp += 1;
        extra = extra
            .checked_add(b as usize)
            .ok_or(CompressionError::Lz4Malformed)?;
        if b != 255 {
            return Ok(extra);
        }
    }
}

// ---------------------------------------------------------------------------
// Compression
// ---------------------------------------------------------------------------

/// Compress one standard LZ4 block and append it to `out`.
///
/// This follows `LZ4_compress_default`'s strategy: a 4,096-entry `u32` hash
/// table of 5-byte hashes, skip acceleration that widens the search step after
/// repeated misses, backward match extension, and forward match extension 8
/// bytes at a time, widening to 16 once a match proves long. The output keeps
/// the end-of-block rules every LZ4 decoder, including ClickHouse's, relies
/// on: the last sequence is literals only, the last 5 bytes are literals, the
/// last match starts at least 12 bytes before the end, offsets are in
/// `1..=65535`, and inputs shorter than 13 bytes are emitted as literals only. It allocates nothing beyond what it writes to
/// `out`; the hash table lives on the stack.
pub fn compress_block_into(src: &[u8], out: &mut Vec<u8>) {
    const HASH_LOG: u32 = 12;
    const MAX_DISTANCE: usize = u16::MAX as usize;
    /// A match must start at least this many bytes before the end (MFLIMIT).
    const MATCH_FIND_LIMIT: usize = 12;
    /// The last bytes are always literals.
    const LAST_LITERALS: usize = 5;
    /// Shorter inputs are emitted as literals only (LZ4_minLength).
    const MIN_INPUT: usize = MATCH_FIND_LIMIT + 1;
    /// After `1 << SKIP_TRIGGER` consecutive misses the search step grows.
    const SKIP_TRIGGER: u32 = 6;

    #[inline(always)]
    fn hash5(sequence: u64) -> usize {
        // LZ4_hash5 for little-endian reads: hash the low 5 bytes.
        ((sequence << 24).wrapping_mul(889_523_592_379) >> (64 - HASH_LOG)) as usize
    }

    let len = src.len();
    let mut anchor = 0usize;

    if len >= MIN_INPUT {
        // Zero-initialized like liblz4: a stale or initial entry is only a
        // candidate, and is accepted only after the distance and content
        // checks below, so position 0 needs no sentinel.
        let mut table = [0u32; 1 << HASH_LOG];
        let match_limit = len - LAST_LITERALS;
        let mut pos = 0usize;
        let mut search = 1usize << SKIP_TRIGGER;

        // `pos + MATCH_FIND_LIMIT <= len` keeps every 8-byte read at `pos` and
        // at any earlier candidate in bounds.
        while pos + MATCH_FIND_LIMIT <= len {
            let sequence = read_u64(src, pos);
            let hash = hash5(sequence);
            // Entries are earlier positions truncated to u32, so `candidate`
            // never exceeds `pos`; the subtraction cannot underflow.
            let mut candidate = table[hash] as usize;
            table[hash] = pos as u32;
            let distance = pos - candidate;
            if distance == 0
                || distance > MAX_DISTANCE
                || read_u64(src, candidate) as u32 != sequence as u32
            {
                pos += search >> SKIP_TRIGGER;
                search += 1;
                continue;
            }
            search = 1 << SKIP_TRIGGER;

            // Extend the match backwards over pending literals.
            while pos > anchor && candidate > 0 && src[pos - 1] == src[candidate - 1] {
                pos -= 1;
                candidate -= 1;
            }

            let literal_len = pos - anchor;
            let token_pos = out.len();
            out.push(0);
            if literal_len >= 15 {
                write_length(literal_len - 15, out);
            }
            out.extend_from_slice(&src[anchor..pos]);
            out.extend_from_slice(&(distance as u16).to_le_bytes());

            // Extend forwards, stopping at `match_limit` so the last 5 bytes
            // stay literal. The verified 4 bytes end at most at
            // `len - MATCH_FIND_LIMIT + 4 < match_limit`.
            let match_start = pos;
            pos += MIN_MATCH;
            candidate += MIN_MATCH;
            // Most matches in column data are short, so the first steps
            // compare 8 bytes; once a match has run past 16 bytes it is
            // likely long, and 16-byte steps pay off. Every read ends by
            // `match_limit`, because `candidate < pos`.
            'extend: loop {
                if pos + 8 > match_limit {
                    while pos < match_limit && src[pos] == src[candidate] {
                        pos += 1;
                        candidate += 1;
                    }
                    break;
                }
                let diff = read_u64(src, pos) ^ read_u64(src, candidate);
                if diff != 0 {
                    pos += (diff.trailing_zeros() / 8) as usize;
                    break;
                }
                pos += 8;
                candidate += 8;
                if pos - match_start >= 16 {
                    while pos + 16 <= match_limit {
                        let diff = read_u128(src, pos) ^ read_u128(src, candidate);
                        if diff != 0 {
                            pos += (diff.trailing_zeros() / 8) as usize;
                            break 'extend;
                        }
                        pos += 16;
                        candidate += 16;
                    }
                }
            }

            let encoded_match_len = pos - match_start - MIN_MATCH;
            if encoded_match_len >= 15 {
                write_length(encoded_match_len - 15, out);
            }
            out[token_pos] = ((literal_len.min(15) as u8) << 4) | encoded_match_len.min(15) as u8;
            anchor = pos;

            // Seed the table two bytes before the match end, as liblz4 does.
            let next = pos - 2;
            if next + 8 <= len {
                table[hash5(read_u64(src, next))] = next as u32;
            }
        }
    }

    let literal_len = len - anchor;
    out.push((literal_len.min(15) as u8) << 4);
    if literal_len >= 15 {
        write_length(literal_len - 15, out);
    }
    out.extend_from_slice(&src[anchor..]);
}

/// Little-endian 8-byte read at `pos`. Callers guarantee `pos + 8 <= src.len()`.
#[inline(always)]
fn read_u64(src: &[u8], pos: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&src[pos..pos + 8]);
    u64::from_le_bytes(bytes)
}

/// Little-endian 16-byte read at `pos`. Callers guarantee
/// `pos + 16 <= src.len()`.
#[inline(always)]
fn read_u128(src: &[u8], pos: usize) -> u128 {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&src[pos..pos + 16]);
    u128::from_le_bytes(bytes)
}

fn write_length(mut len: usize, out: &mut Vec<u8>) {
    while len >= 255 {
        out.push(255);
        len -= 255;
    }
    out.push(len as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known-answer LZ4 block vectors generated by compiling against the system
    // liblz4 and calling LZ4_compress_default, the exact routine ClickHouse uses
    // in CompressionCodecLZ4::doCompressData. Each `comp` byte string round
    // trips through liblz4's LZ4_decompress_safe in the generator, so the
    // expected output is the original input.

    #[test]
    fn literals_only() {
        // input = [0x13, 0x79, 0x01, 0x02, 0x03] (5 bytes, incompressible).
        // token 0x50 = 5 literals, 0 match, then the 5 literal bytes.
        let comp = [0x50, 0x13, 0x79, 0x01, 0x02, 0x03];
        let out = decompress_block(&comp, 5).unwrap();
        assert_eq!(out, [0x13, 0x79, 0x01, 0x02, 0x03]);
    }

    #[test]
    fn offset_one_run_length_expansion() {
        // 40 bytes of 0x79 compress to a 1-literal seed plus an offset-1 match,
        // the run-length-expansion case. comp from LZ4_compress_default.
        let comp = [
            0x1f, 0x79, 0x01, 0x00, 0x0f, 0x50, 0x79, 0x79, 0x79, 0x79, 0x79,
        ];
        let out = decompress_block(&comp, 40).unwrap();
        assert_eq!(out, vec![0x79u8; 40]);
    }

    #[test]
    fn normal_offset_match() {
        // A repeated 8-byte pattern "user_13-" x4 (32 bytes). The match has an
        // offset of 8, larger than 1, exercising the non-overlap copy and the
        // length extension path.
        let input: Vec<u8> = b"user_13-user_13-user_13-user_13-".to_vec();
        let comp = [
            0x8f, 0x75, 0x73, 0x65, 0x72, 0x5f, 0x31, 0x33, 0x2d, 0x08, 0x00, 0x00, 0x50, 0x72,
            0x5f, 0x31, 0x33, 0x2d,
        ];
        let out = decompress_block(&comp, input.len()).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn empty_block() {
        // Zero-length input compresses to a single empty-literals token 0x00.
        let comp = [0x00u8];
        let out = decompress_block(&comp, 0).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn truncated_literals_is_error() {
        // token says 5 literals but only 2 source bytes follow: malformed, not a
        // panic.
        let comp = [0x50, 0x13, 0x79];
        let err = decompress_block(&comp, 5).unwrap_err();
        assert!(matches!(err, CompressionError::Lz4Malformed));
    }

    #[test]
    fn truncated_offset_is_error() {
        // 1 literal then a single offset byte (offset needs 2): malformed.
        let comp = [0x10, 0x79, 0x01];
        let err = decompress_block(&comp, 40).unwrap_err();
        assert!(matches!(err, CompressionError::Lz4Malformed));
    }

    #[test]
    fn offset_past_output_is_error() {
        // 1 literal then an offset of 5 with only 1 byte written: reading back 5
        // would go before the output start. Must be a clean error.
        let comp = [0x1f, 0x79, 0x05, 0x00, 0x00];
        let err = decompress_block(&comp, 40).unwrap_err();
        assert!(matches!(err, CompressionError::Lz4Malformed));
    }

    #[test]
    fn offset_zero_is_error() {
        let comp = [0x10, 0x79, 0x00, 0x00];
        let err = decompress_block(&comp, 5).unwrap_err();
        assert!(matches!(err, CompressionError::Lz4Malformed));
    }

    #[test]
    fn tiny_payload_cannot_claim_huge_output() {
        let mut out = b"prefix".to_vec();
        let err = decompress_block_into(&[0x00], 1 << 30, &mut out).unwrap_err();
        assert!(matches!(err, CompressionError::Lz4Malformed));
        assert_eq!(out, b"prefix");
    }

    #[test]
    fn match_overruns_uncompressed_size_is_error() {
        // A valid-looking offset-1 match that would write more than the declared
        // uncompressed_size must be rejected rather than over-allocate or write
        // past the bound.
        let comp = [
            0x1f, 0x79, 0x01, 0x00, 0x0f, 0x50, 0x79, 0x79, 0x79, 0x79, 0x79,
        ];
        // Declare a smaller size than the 40 bytes the block expands to.
        let err = decompress_block(&comp, 8).unwrap_err();
        assert!(matches!(err, CompressionError::Lz4Malformed));
    }

    #[test]
    fn garbage_length_extension_is_error() {
        // token with literal nibble 15 then a run of 0xFF extension bytes that
        // never terminates before the source ends: malformed, no infinite loop.
        let comp = [0xf0, 0xff, 0xff, 0xff];
        let err = decompress_block(&comp, 1024).unwrap_err();
        assert!(matches!(err, CompressionError::Lz4Malformed));
    }

    #[test]
    fn final_size_short_is_error() {
        // A block whose bytes decode cleanly but produce fewer bytes than
        // declared is a size mismatch, the server's "size after decompression
        // differs" check.
        let comp = [0x50, 0x13, 0x79, 0x01, 0x02, 0x03];
        let err = decompress_block(&comp, 6).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::SizeMismatch {
                expected: 6,
                actual: 5
            }
        ));
    }

    #[test]
    fn compressor_roundtrips_literals_and_matches() {
        for input in [
            b"short".as_slice(),
            b"user_13-user_13-user_13-user_13-".as_slice(),
            &[0x79; 79],
            b"five trailing bytes stay literal abcde".as_slice(),
        ] {
            let mut compressed = Vec::new();
            compress_block_into(input, &mut compressed);
            let decoded = decompress_block(&compressed, input.len()).unwrap();
            assert_eq!(decoded, input);
        }
    }

    #[test]
    fn compressor_reduces_repeated_input() {
        let input = vec![0x79; 4096];
        let mut compressed = Vec::new();
        compress_block_into(&input, &mut compressed);
        assert!(compressed.len() < input.len() / 10);
        assert_eq!(decompress_block(&compressed, input.len()).unwrap(), input);
    }

    fn validate_strict_end_of_block(src: &[u8], expected_plaintext_len: usize) {
        fn extension(src: &[u8], sp: &mut usize) -> usize {
            let mut len = 0usize;
            loop {
                let byte = src[*sp];
                *sp += 1;
                len += byte as usize;
                if byte != 255 {
                    return len;
                }
            }
        }

        let mut sp = 0usize;
        let mut produced = 0usize;
        let mut last_match_start = None;
        let mut final_literals = 0usize;
        let mut saw_final_literal_sequence = false;

        while sp < src.len() {
            let token = src[sp];
            sp += 1;
            let mut literal_len = (token >> 4) as usize;
            if literal_len == 15 {
                literal_len += extension(src, &mut sp);
            }
            assert!(sp + literal_len <= src.len());
            sp += literal_len;
            produced += literal_len;
            if sp == src.len() {
                final_literals = literal_len;
                saw_final_literal_sequence = true;
                break;
            }

            assert!(sp + 2 <= src.len());
            let offset = usize::from(src[sp]) | (usize::from(src[sp + 1]) << 8);
            sp += 2;
            assert!(offset != 0 && offset <= produced);
            last_match_start = Some(produced);
            let mut match_len = (token & 0x0f) as usize;
            if match_len == 15 {
                match_len += extension(src, &mut sp);
            }
            produced += match_len + MIN_MATCH;
        }

        assert!(saw_final_literal_sequence);
        assert_eq!(sp, src.len());
        assert_eq!(produced, expected_plaintext_len);
        if expected_plaintext_len >= 5 {
            assert!(final_literals >= 5);
        }
        if let Some(match_start) = last_match_start {
            assert!(expected_plaintext_len - match_start >= 12);
        }
    }

    #[test]
    fn compressor_emits_strict_end_of_block_shape() {
        fn check(input: &[u8]) {
            let mut compressed = Vec::new();
            compress_block_into(input, &mut compressed);
            validate_strict_end_of_block(&compressed, input.len());
            assert_eq!(decompress_block(&compressed, input.len()).unwrap(), input);
        }

        let mut state = 0x79_u64;
        for len in 0..=1024 {
            let mut input = vec![0u8; len];
            for byte in &mut input {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                *byte = (state >> 32) as u8;
            }
            if len % 3 == 0 {
                input.fill((len % 251) as u8);
            }
            check(&input);
        }

        for len in [4095, 4096, 65_535, 65_536, 65_537, 128 * 1024] {
            let run = vec![0x79; len];
            check(&run);

            let cycle: Vec<u8> = (0..len).map(|index| (index % 251) as u8).collect();
            check(&cycle);

            let mut noise = vec![0u8; len];
            for byte in &mut noise {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                *byte = (state >> 32) as u8;
            }
            if len >= 512 {
                let repeated = noise[79..335].to_vec();
                noise[len / 2..len / 2 + repeated.len()].copy_from_slice(&repeated);
            }
            check(&noise);
        }
    }

    // -----------------------------------------------------------------------
    // Differential tests against liblz4 (dev-dependency only)
    // -----------------------------------------------------------------------

    /// The byte-at-a-time decoder this module shipped before the fast paths,
    /// kept as an executable specification. It appends to a fresh `Vec`, so it
    /// is trivially in bounds, and the fast decoder must agree with it exactly,
    /// errors included, on every input. Sets `zero_offset` when it rejects the
    /// block for a zero match offset.
    fn reference_decompress(
        src: &[u8],
        n: usize,
        zero_offset: &mut bool,
    ) -> Result<Vec<u8>, CompressionError> {
        if n > src.len().saturating_mul(255).saturating_add(24) {
            return Err(CompressionError::Lz4Malformed);
        }
        let mut out = Vec::new();
        let mut sp = 0usize;
        while sp < src.len() {
            let token = src[sp];
            sp += 1;
            let mut literal_len = (token >> 4) as usize;
            if literal_len == 15 {
                literal_len = literal_len
                    .checked_add(read_length_extension(src, &mut sp)?)
                    .ok_or(CompressionError::Lz4Malformed)?;
            }
            let end = sp
                .checked_add(literal_len)
                .ok_or(CompressionError::Lz4Malformed)?;
            let lits = src.get(sp..end).ok_or(CompressionError::Lz4Malformed)?;
            if out.len() + literal_len > n {
                return Err(CompressionError::Lz4Malformed);
            }
            out.extend_from_slice(lits);
            sp = end;
            if sp == src.len() {
                break;
            }
            let off_lo = *src.get(sp).ok_or(CompressionError::Lz4Malformed)?;
            let off_hi = *src.get(sp + 1).ok_or(CompressionError::Lz4Malformed)?;
            sp += 2;
            let offset = (off_lo as usize) | ((off_hi as usize) << 8);
            *zero_offset = offset == 0;
            if offset == 0 || offset > out.len() {
                return Err(CompressionError::Lz4Malformed);
            }
            let mut match_len = (token & 0x0F) as usize;
            if match_len == 15 {
                match_len = match_len
                    .checked_add(read_length_extension(src, &mut sp)?)
                    .ok_or(CompressionError::Lz4Malformed)?;
            }
            match_len = match_len
                .checked_add(MIN_MATCH)
                .ok_or(CompressionError::Lz4Malformed)?;
            if out.len() + match_len > n {
                return Err(CompressionError::Lz4Malformed);
            }
            let start = out.len() - offset;
            for from in start..start + match_len {
                let b = out[from];
                out.push(b);
            }
        }
        if out.len() != n {
            return Err(CompressionError::SizeMismatch {
                expected: n,
                actual: out.len(),
            });
        }
        Ok(out)
    }

    /// Deterministic byte source (64-bit LCG, high bits).
    struct Lcg(u64);

    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 16
        }

        fn byte(&mut self) -> u8 {
            (self.next_u64() >> 24) as u8
        }

        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| self.byte()).collect()
        }
    }

    /// `len` bytes repeating a random pattern of `period` bytes.
    fn periodic(len: usize, period: usize, rng: &mut Lcg) -> Vec<u8> {
        let pattern = rng.bytes(period);
        (0..len).map(|i| pattern[i % period]).collect()
    }

    /// Column-major, Native-like plaintext: sequential u64 ids, slowly rising
    /// DateTime, varint-prefixed `user_N` strings, noisy f64, a LowCardinality
    /// u8 key with runs, a sparse null mask, and i32 values.
    fn column_like(len: usize, rng: &mut Lcg) -> Vec<u8> {
        let rows = len / 36 + 1;
        let mut o = Vec::with_capacity(len + 64);
        for i in 0..rows {
            o.extend_from_slice(&(1_000_000u64 + i as u64).to_le_bytes());
        }
        for i in 0..rows {
            o.extend_from_slice(&(1_700_000_000u32 + (i as u32) / 7).to_le_bytes());
        }
        for i in 0..rows {
            let s = format!("user_{}", i * 7919 % 100_000);
            o.push(s.len() as u8);
            o.extend_from_slice(s.as_bytes());
        }
        for _ in 0..rows {
            let f = (rng.next_u64() % 100_000) as f64 / 100.0;
            o.extend_from_slice(&f.to_le_bytes());
        }
        let mut key = 0u8;
        for _ in 0..rows {
            if rng.next_u64() % 8 == 0 {
                key = (rng.next_u64() % 20) as u8;
            }
            o.push(key);
        }
        for _ in 0..rows {
            o.push((rng.next_u64() % 10 == 0) as u8);
        }
        for i in 0..rows {
            o.extend_from_slice(&((i as i32) * 3 - 500).to_le_bytes());
        }
        o.truncate(len);
        o
    }

    /// Every corpus shape at length `len`: all-zero, random, column-like, a
    /// zero run with a random tail, and periodic with periods 1..=20.
    fn corpora(len: usize, rng: &mut Lcg) -> Vec<Vec<u8>> {
        let mut out = vec![vec![0u8; len], rng.bytes(len), column_like(len, rng)];
        let mut half = vec![0u8; len];
        let tail = rng.bytes(len - len / 2);
        half[len / 2..].copy_from_slice(&tail);
        out.push(half);
        for period in 1..=20 {
            out.push(periodic(len, period, rng));
        }
        out
    }

    fn liblz4_compress(input: &[u8]) -> Vec<u8> {
        ::lz4::block::compress(input, None, false).expect("liblz4 compresses")
    }

    /// `LZ4_decompress_safe` with exactly `n` bytes of capacity. `None` when
    /// liblz4 rejects the block or decodes a different length.
    fn liblz4_decompress(block: &[u8], n: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; n];
        match ::lz4::block::decompress_to_buffer(block, Some(n as i32), &mut buf) {
            Ok(k) if k == n => Some(buf),
            _ => None,
        }
    }

    /// Full round trip in both directions plus the strict end-of-block shape
    /// and the into-buffer contract (prefix kept, exact size, no realloc).
    fn check_differential(input: &[u8]) {
        let n = input.len();

        let mut ours = Vec::new();
        compress_block_into(input, &mut ours);
        validate_strict_end_of_block(&ours, n);
        assert_eq!(
            liblz4_decompress(&ours, n).as_deref(),
            Some(input),
            "liblz4 must decode our block (len {n})"
        );
        assert_eq!(decompress_block(&ours, n).unwrap(), input);

        let theirs = liblz4_compress(input);
        for window in WINDOWS {
            let prefix = b"earlier frame plaintext";
            let mut out = prefix.to_vec();
            out.reserve_exact(n);
            let capacity = out.capacity();
            decode_into(&theirs, n, &mut out, window).unwrap();
            assert_eq!(&out[..prefix.len()], prefix);
            assert_eq!(&out[prefix.len()..], input, "decode liblz4 block (len {n})");
            assert_eq!(out.capacity(), capacity, "decode must not reallocate");
        }
    }

    /// Fill windows every decode test runs with: production; an odd 37 that
    /// forces a phase hand-off every few sequences; and 1, below the 16-byte
    /// over-copy, so nearly every fixed-width copy must first grow the
    /// frontier.
    const WINDOWS: [usize; 3] = [FILL_WINDOW, 37, 1];

    /// The decoder instantiated with fill window `window` (one of `WINDOWS`).
    fn decode_into(
        src: &[u8],
        n: usize,
        out: &mut Vec<u8>,
        window: usize,
    ) -> Result<(), CompressionError> {
        match window {
            FILL_WINDOW => decompress_block_into(src, n, out),
            37 => decompress_block_into_windowed::<37>(src, n, out),
            1 => decompress_block_into_windowed::<1>(src, n, out),
            other => panic!("no decoder instantiated for fill window {other}"),
        }
    }

    #[test]
    fn differential_small_lengths() {
        let mut rng = Lcg(0x5eed);
        for len in 0..=300 {
            for input in corpora(len, &mut rng) {
                check_differential(&input);
            }
        }
    }

    #[test]
    fn differential_boundary_lengths() {
        let mut rng = Lcg(0xb0_0da1);
        for len in [4095, 4096, 65_535, 65_536, 65_537] {
            for input in corpora(len, &mut rng) {
                check_differential(&input);
            }
        }
    }

    #[test]
    fn differential_large_inputs() {
        let mut rng = Lcg(0x1_0000);
        let len = 1 << 20;
        for input in [
            vec![0u8; len],
            rng.bytes(len),
            column_like(len, &mut rng),
            periodic(len, 3, &mut rng),
            periodic(len, 13, &mut rng),
        ] {
            check_differential(&input);
        }
        // Periods at and past the maximum match distance: 65,535 repeats are
        // reachable, 65,536 are not.
        for period in [65_535, 65_536] {
            check_differential(&periodic(3 * 65_536 + 77, period, &mut rng));
        }
    }

    /// Compare the fast decoder with the reference decoder (exactly, errors
    /// included) and with liblz4, and check the into-buffer rollback on error.
    ///
    /// liblz4's `LZ4_decompress_safe` is stricter than this crate about the
    /// end-of-block shape, but it accepts a zero match offset (it copies the
    /// destination onto itself), which the LZ4 block format calls invalid and
    /// this crate rejects. That is the only allowed disagreement.
    fn check_mutant(block: &[u8], n: usize) {
        let mut zero_offset = false;
        let expected = reference_decompress(block, n, &mut zero_offset);
        for window in WINDOWS {
            let prefix = b"prior";
            let mut out = prefix.to_vec();
            let actual = decode_into(block, n, &mut out, window);
            match (&expected, &actual) {
                (Ok(want), Ok(())) => assert_eq!(&out[prefix.len()..], want.as_slice()),
                (Err(want), Err(got)) => {
                    assert_eq!(format!("{want:?}"), format!("{got:?}"));
                    assert_eq!(out, prefix, "error must restore the caller's Vec");
                }
                _ => {
                    panic!("fast {actual:?} vs reference {expected:?} (n {n}, block {block:02x?})")
                }
            }
        }
        if let Some(theirs) = liblz4_decompress(block, n) {
            match &expected {
                Ok(want) => assert_eq!(want, &theirs, "liblz4 decoded differently"),
                Err(err) => assert!(
                    zero_offset,
                    "liblz4 accepted a block we reject with {err:?} (n {n}, block {block:02x?})"
                ),
            }
        }
    }

    #[test]
    fn mutated_blocks_agree_with_reference_and_liblz4() {
        let mut rng = Lcg(0xbad_b10c);
        let mut inputs = vec![
            column_like(3000, &mut rng),
            periodic(700, 3, &mut rng),
            periodic(900, 11, &mut rng),
            periodic(900, 23, &mut rng),
            vec![0u8; 1000],
            rng.bytes(200),
        ];
        // Long matches and long literal runs in one block.
        let mut mixed = rng.bytes(300);
        mixed.extend(std::iter::repeat(0x79).take(500));
        mixed.extend(periodic(600, 5, &mut rng));
        inputs.push(mixed);

        for input in &inputs {
            let n = input.len();
            let mut ours = Vec::new();
            compress_block_into(input, &mut ours);
            for block in [ours, liblz4_compress(input)] {
                for size in [n, n.saturating_sub(1), n + 1, n + 17, 0] {
                    check_mutant(&block, size);
                }
                for cut in 0..block.len() {
                    check_mutant(&block[..cut], n);
                }
                for pos in 0..block.len() {
                    for value in [0x00, 0xff, rng.byte(), block[pos] ^ 0x10] {
                        let mut mutant = block.clone();
                        mutant[pos] = value;
                        check_mutant(&mutant, n);
                    }
                }
                for _ in 0..200 {
                    let mut mutant = block.clone();
                    for _ in 0..4 {
                        let pos = (rng.next_u64() as usize) % mutant.len();
                        mutant[pos] = rng.byte();
                    }
                    check_mutant(&mutant, n);
                }
            }
        }

        // Pure garbage of assorted lengths and declared sizes.
        for len in 0..200 {
            let garbage = rng.bytes(len);
            for size in [0, 1, 16, 40, 100, 255, 1000] {
                check_mutant(&garbage, size);
            }
        }
    }

    #[test]
    fn small_offset_matches_all_lengths() {
        // Hand-built blocks: a seed of `offset` literals, then one match of
        // every length 4..=80 at that offset, then 12 trailing literals.
        // Covers the shortcut, the chunked paths, and the doubling paths for
        // each offset 1..=20, with the match close to the end of the block,
        // where the fixed-width paths must fall back to exact copies.
        for offset in 1..=20usize {
            for match_len in MIN_MATCH..=80 {
                let seed: Vec<u8> = (0..offset as u8).map(|b| b.wrapping_mul(37) + 1).collect();
                let mut block = Vec::new();
                let extra = match_len - MIN_MATCH;
                block.push(((offset.min(15) as u8) << 4) | extra.min(15) as u8);
                if offset >= 15 {
                    write_length(offset - 15, &mut block);
                }
                block.extend_from_slice(&seed);
                block.extend_from_slice(&(offset as u16).to_le_bytes());
                if extra >= 15 {
                    write_length(extra - 15, &mut block);
                }
                // 12 trailing literals keep liblz4's end-of-block rules.
                block.push(0xc0);
                block.extend_from_slice(b"twelve tail!");

                let mut want = seed.clone();
                for i in 0..match_len {
                    want.push(want[i]);
                }
                want.extend_from_slice(b"twelve tail!");
                for window in WINDOWS {
                    let mut out = Vec::new();
                    decode_into(&block, want.len(), &mut out, window).unwrap();
                    assert_eq!(out, want);
                }
                assert_eq!(liblz4_decompress(&block, want.len()).unwrap(), want);
            }
        }
    }

    #[test]
    fn decode_into_spare_capacity_stops_at_declared_end() {
        // A caller Vec with far more capacity than base + n: the frontier must
        // still stop at base + n, never using the spare capacity as scratch
        // that would leak into `len`.
        let mut rng = Lcg(0xcafe);
        for input in [column_like(5000, &mut rng), periodic(3000, 3, &mut rng)] {
            let n = input.len();
            let block = liblz4_compress(&input);
            for window in WINDOWS {
                let prefix = b"earlier frame plaintext";
                let mut out = prefix.to_vec();
                out.reserve(n + 100_000);
                decode_into(&block, n, &mut out, window).unwrap();
                assert_eq!(out.len(), prefix.len() + n);
                assert_eq!(&out[..prefix.len()], prefix);
                assert_eq!(&out[prefix.len()..], input.as_slice());
            }
        }
    }

    #[test]
    fn match_reaching_one_byte_into_prefix_is_rejected() {
        // One literal, then a match whose offset reaches exactly one byte
        // before this block's base, into an earlier frame's plaintext already
        // in the caller's Vec. Then five final literals.
        fn block(offset: u16) -> Vec<u8> {
            let mut b = vec![0x10, b'a'];
            b.extend_from_slice(&offset.to_le_bytes());
            b.push(0x50);
            b.extend_from_slice(b"tail!");
            b
        }
        let n = 1 + MIN_MATCH + 5;
        for window in WINDOWS {
            let prefix = b"earlier frame plaintext";
            let mut out = prefix.to_vec();
            let err = decode_into(&block(2), n, &mut out, window).unwrap_err();
            assert!(matches!(err, CompressionError::Lz4Malformed));
            assert_eq!(out, prefix, "error must restore the caller's Vec");

            // One byte less reach is a valid run of the block's own byte.
            let mut out = prefix.to_vec();
            decode_into(&block(1), n, &mut out, window).unwrap();
            assert_eq!(&out[prefix.len()..], b"aaaaatail!");
        }
    }
}
