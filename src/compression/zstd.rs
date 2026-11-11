//! ZSTD payload decoding for ClickHouse compression method byte `0x90`.
//!
//! Confirmed at ClickHouse v26.8.1.2041-lts:
//! `CompressionCodecZSTD::doCompressData` in
//! `src/Compression/CompressionCodecZSTD.cpp` writes one standard ZSTD frame
//! per block with a single `ZSTD_compress2` call and no ClickHouse prefix.
//! `CompressionCodecZSTD::doDecompressData` passes the complete payload to
//! `ZSTD_decompress`, and `ICompressionCodec::decompress` requires its returned
//! byte count to equal the header's `uncompressed_size`.
//!
//! Inferred, not confirmed against ClickHouse: acceptance of concatenated or
//! skippable standard ZSTD frames inside one payload. The server checkout lacks
//! its contrib/zstd submodule, so this follows from the `ZSTD_decompress` API.
//! This decoder uses `ZSTD_decompressDCtx`, which has the same frame handling.

use std::cell::RefCell;

use ::zstd::zstd_safe::{DCtx, SafeResult};

use super::CompressionError;

/// RFC 8878 maximum number of output bytes per payload byte for a
/// conforming frame.
///
/// RFC 8878 section 3.1.1.2.4 caps every block at Block_Maximum_Size, which
/// is min(Window_Size, 128 KiB). The densest conforming block is an
/// RLE_Block: a 3-byte block header plus 1 content byte that repeats into up
/// to 128 KiB, so 131072 / 4 = 32768. Compressed_Blocks top out near 16384,
/// and frame headers only lower the ratio. RFC 8878 section 8 warns not to
/// trust Frame_Content_Size for allocation, so this payload-length bound is
/// used instead.
///
/// libzstd 1.5.7 does not enforce Block_Maximum_Size and accepts larger RLE
/// blocks, so this precheck is deliberately stricter than libzstd and the
/// server. No conforming encoder emits such blocks. The precheck only bounds
/// the reservation. The actual decode work is bounded by giving libzstd an
/// output slice of exactly `uncompressed_size` bytes.
const MAX_EXPANSION: usize = 32 * 1024;

thread_local! {
    /// One reusable decompression context per thread, created on first use
    /// and freed when the thread exits. Building a context per frame is
    /// costly next to the small frames TCP sends. Reuse after any error is
    /// safe because libzstd's `ZSTD_decompressDCtx` starts every frame from a
    /// fresh session. zstd-safe does not reset the context itself after a
    /// failed one-shot call.
    static DCTX: RefCell<Option<DCtx<'static>>> = const { RefCell::new(None) };
}

/// Decompress a complete ZSTD payload into a new buffer.
pub(super) fn decompress_block(
    src: &[u8],
    uncompressed_size: usize,
) -> Result<Vec<u8>, CompressionError> {
    let mut out = Vec::new();
    decompress_block_into(src, uncompressed_size, &mut out)?;
    Ok(out)
}

/// Decompress a complete ZSTD payload and append it to `out`.
///
/// A claimed `uncompressed_size` larger than a conforming payload can expand
/// to is rejected as [`CompressionError::InvalidUncompressedSize`] before any
/// reservation. Otherwise `out` grows by exactly `uncompressed_size`
/// zero-filled bytes, and libzstd decodes into that slice and never past it.
/// The memset is bounded by the precheck. The whole payload is processed,
/// including valid concatenated or skippable frames. Invalid trailing bytes
/// fail.
///
/// The result never depends on the spare capacity of `out`. A payload that
/// decodes to more than the claim fails as [`CompressionError::ZstdMalformed`]
/// (libzstd's dstSize_tooSmall), as on the server. One that decodes to fewer
/// bytes fails as [`CompressionError::SizeMismatch`]. On every error, `out`
/// is restored to its original length.
pub(super) fn decompress_block_into(
    src: &[u8],
    uncompressed_size: usize,
    out: &mut Vec<u8>,
) -> Result<(), CompressionError> {
    if uncompressed_size > src.len().saturating_mul(MAX_EXPANSION) {
        return Err(CompressionError::InvalidUncompressedSize {
            // parse_block_header bounds the size to 1 GiB, so this always
            // converts. Saturate instead of panicking if that ever changes.
            uncompressed_size: u32::try_from(uncompressed_size).unwrap_or(u32::MAX),
        });
    }

    let start = out.len();
    out.try_reserve(uncompressed_size)
        .map_err(|_| CompressionError::AllocationFailed {
            requested: uncompressed_size,
        })?;
    // The reservation succeeded, so this cannot reallocate or overflow.
    out.resize(start + uncompressed_size, 0);

    let err = match decompress_into_slice(&mut out[start..], src) {
        Ok(Ok(decoded)) if decoded == uncompressed_size => return Ok(()),
        Ok(Ok(decoded)) => CompressionError::SizeMismatch {
            expected: uncompressed_size,
            actual: decoded,
        },
        // libzstd's memory_allocation code also lands here. zstd-safe only
        // exposes it through the unsafe `ZSTD_getErrorCode` FFI call, and a
        // one-shot decode with an existing context does not allocate.
        Ok(Err(_code)) => CompressionError::ZstdMalformed,
        Err(err) => err,
    };
    out.truncate(start);
    Err(err)
}

/// Run one-shot decompression into `dst` with this thread's cached context.
///
/// libzstd writes only inside `dst`. Falls back to a one-off context if the
/// thread-local is being torn down, or is already borrowed. Nothing re-enters
/// this function while the borrow is held, so the second case should not
/// happen, but it costs nothing to handle without a panic. A context that
/// cannot be allocated reports
/// [`CompressionError::ZstdContextAllocationFailed`].
fn decompress_into_slice(dst: &mut [u8], src: &[u8]) -> Result<SafeResult, CompressionError> {
    let cached = DCTX.try_with(|cell| {
        let mut slot = cell.try_borrow_mut().ok()?;
        if slot.is_none() {
            *slot = DCtx::try_create();
        }
        Some(slot.as_mut().map(|dctx| dctx.decompress(&mut *dst, src)))
    });
    match cached {
        Ok(Some(Some(result))) => Ok(result),
        Ok(Some(None)) => Err(CompressionError::ZstdContextAllocationFailed),
        Ok(None) | Err(_) => DCtx::try_create()
            .map(|mut dctx| dctx.decompress(dst, src))
            .ok_or(CompressionError::ZstdContextAllocationFailed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built frame holding one RLE block of `block_size` copies of
    /// `byte`, with an 8 MiB window, no content size, and no checksum.
    fn rle_frame(byte: u8, block_size: u32) -> Vec<u8> {
        assert!(block_size < 1 << 21);
        let mut frame = 0xFD2F_B528u32.to_le_bytes().to_vec();
        // Frame_Header_Descriptor: no content size, not single segment, no
        // checksum, no dictionary. Window_Descriptor: exponent 13 -> 2^23.
        frame.extend_from_slice(&[0x00, 13 << 3]);
        // Block_Header: Last_Block = 1, Block_Type = RLE (1), Block_Size.
        let header = 1 | (1 << 1) | (block_size << 3);
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.push(byte);
        frame
    }

    #[test]
    fn cached_context_survives_errors() {
        let raw = b"ZSTD bytes for user_1";
        let payload = ::zstd::bulk::compress(raw, 1).unwrap();

        // A multi-block frame whose last block is cut short fails after
        // earlier blocks were already decoded into the output.
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let big: Vec<u8> = (0..300_000)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                if i % 3 == 0 {
                    x as u8
                } else {
                    (i % 79) as u8
                }
            })
            .collect();
        let big_payload = ::zstd::bulk::compress(&big, 1).unwrap();
        let cut = &big_payload[..big_payload.len() - 3];

        for _ in 0..3 {
            let err = decompress_block(b"not a ZSTD frame", raw.len()).unwrap_err();
            assert!(matches!(err, CompressionError::ZstdMalformed), "{err:?}");
            assert_eq!(decompress_block(&payload, raw.len()).unwrap(), raw);

            let err = decompress_block(cut, big.len()).unwrap_err();
            assert!(matches!(err, CompressionError::ZstdMalformed), "{err:?}");
            assert_eq!(decompress_block(&payload, raw.len()).unwrap(), raw);
            assert_eq!(decompress_block(&big_payload, big.len()).unwrap(), big);
        }
    }

    #[test]
    fn expansion_bound_is_inclusive() {
        // A claim exactly at the bound passes the precheck. This payload
        // decodes to far fewer bytes, so it then fails as a size mismatch.
        let payload = ::zstd::bulk::compress(b"tiny", 1).unwrap();
        let at_bound = payload.len() * MAX_EXPANSION;
        let err = decompress_block(&payload, at_bound).unwrap_err();
        assert!(
            matches!(err, CompressionError::SizeMismatch { expected, actual: 4 } if expected == at_bound),
            "{err:?}"
        );
        let err = decompress_block(&payload, at_bound + 1).unwrap_err();
        assert!(
            matches!(err, CompressionError::InvalidUncompressedSize { uncompressed_size }
                if uncompressed_size as usize == at_bound + 1),
            "{err:?}"
        );
    }

    #[test]
    fn oversized_rle_block_is_bounded_by_the_claim() {
        // 256 KiB in one RLE block breaks Block_Maximum_Size, but libzstd
        // accepts it. The 10-byte frame still fits the payload bound
        // (10 * 32768 = 320 KiB), so an honest claim decodes.
        let block = 256 * 1024;
        let frame = rle_frame(0x4f, block);
        assert_eq!(frame.len(), 10);
        let decoded = decompress_block(&frame, block as usize).unwrap();
        assert_eq!(decoded, vec![0x4fu8; block as usize]);

        // An under-claim fails the same way with or without spare capacity.
        for spare in [0, 4 << 20] {
            let mut out = Vec::with_capacity(spare);
            out.extend_from_slice(b"keep me");
            let err = decompress_block_into(&frame, 13, &mut out).unwrap_err();
            assert!(matches!(err, CompressionError::ZstdMalformed), "{err:?}");
            assert_eq!(out, b"keep me");
        }

        // libzstd writes nothing past the claimed slice.
        let mut buf = vec![0xaau8; block as usize];
        let res = decompress_into_slice(&mut buf[..13], &frame).unwrap();
        assert!(res.is_err());
        assert!(buf[13..].iter().all(|&b| b == 0xaa));
    }
}
