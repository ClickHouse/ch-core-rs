//! ClickHouse native compressed-block framing (the `CompressedReadBuffer`
//! envelope).
//!
//! This is the server's own block compression layer, the one negotiated over
//! the native protocol and selected by `compress=1`. It is distinct from HTTP
//! transport compression (gzip, zstd, br), which stays in the language
//! bindings. When a Native stream is compressed, the bytes on the wire are a
//! sequence of these independent compressed blocks; concatenating the
//! decompressed output of all of them yields the plain Native byte stream the
//! decoder in [`crate::native`] consumes.
//!
//! Boundary behavior depends on the transport. On the native TCP protocol, the
//! server flushes compression after writing each Native block. One Native block
//! may therefore occupy several compressed frames, but a frame never contains
//! bytes from two Native blocks. Protocol engines should call
//! [`FrameInfo::finish_block`] when Native decoding completes a block. With HTTP
//! `compress=1`, the default server path auto-flushes after each output chunk,
//! but frame-to-Native-block alignment is not a format guarantee and readers
//! must treat frames as an arbitrary stream partition.
//!
//! Wire layout of one compressed block (confirmed against server source
//! `src/Compression/{CompressedReadBufferBase.cpp,ICompressionCodec.cpp,
//! CompressionInfo.h}`, tag v26.8.1.2041-lts):
//!
//! ```text
//! [16 bytes] CityHash128 checksum: low64 (LE u64) then high64 (LE u64)
//! [ 1 byte ] method byte
//! [ 4 bytes] compressed_size   (LE u32) includes the 9-byte header, excludes the checksum
//! [ 4 bytes] uncompressed_size (LE u32)
//! [ N bytes] compressed payload, N = compressed_size - 9
//! ```
//!
//! The checksum is `CityHash128` over the 9-byte header plus the payload, that
//! is over `compressed_size` bytes starting at the method byte, never over the
//! checksum bytes themselves. Method bytes: `0x02` NONE (payload is the raw
//! bytes, copied verbatim), `0x82` LZ4 and LZ4HC (identical block format and
//! decompressor), `0x90` ZSTD (not implemented here, see below). Any other
//! method byte is rejected. `compressed_size` above `DBMS_MAX_COMPRESSED_SIZE`
//! (1 GiB) is rejected as corrupt, and `uncompressed_size` is bounded the same
//! way since it is attacker-controlled and sizes the output allocation. This
//! core also rejects zero-size frames because the server writer never emits
//! them, although release server readers accept them.
//!
//! ZSTD (`0x90`) is intentionally not implemented: a correct ZSTD decoder is far
//! larger than LZ4 and would force a dependency, which this crate forbids. It
//! surfaces as [`CompressionError::UnsupportedCodec`] with a clear message so
//! callers can fall back rather than misread bytes.
//!
//! [`decompress_frame`] reads exactly one compressed frame from an input slice.
//! It reports the compressed byte count separately, so a protocol engine can
//! leave the following packet bytes untouched. A [`CompressionError::Truncated`]
//! result is retryable after more transport bytes arrive. Other errors report
//! corrupt or unsupported input. [`decompress_native_stream`] is a convenience
//! for complete buffers containing only back-to-back compressed frames.

use std::borrow::Cow;

mod cityhash;
mod lz4;

use cityhash::city_hash128;

/// Compression method bytes on the wire (`CompressionMethodByte` in
/// `src/Compression/CompressionInfo.h`).
const METHOD_NONE: u8 = 0x02;
const METHOD_LZ4: u8 = 0x82;
const METHOD_ZSTD: u8 = 0x90;

/// 16-byte CityHash128 checksum prefix.
const CHECKSUM_SIZE: usize = 16;
/// 9-byte header: 1 method byte + 4 compressed-size + 4 uncompressed-size.
const HEADER_SIZE: usize = 9;
/// `DBMS_MAX_COMPRESSED_SIZE`, 1 GiB. A larger compressed_size is corrupt.
const MAX_COMPRESSED_SIZE: u32 = 0x4000_0000;
/// `DBMS_MAX_DECOMPRESSED_SIZE`, 1 GiB. The server bounds this separately from
/// `compressed_size`; it also drives this decoder's output allocation.
const MAX_DECOMPRESSED_SIZE: u32 = 0x4000_0000;
/// Largest plaintext accepted by the LZ4 writer.
///
/// At this size, `LZ4_compressBound` plus the 9-byte ClickHouse header is
/// exactly the 1 GiB compressed-size limit. Capping input here guarantees every
/// encoded frame is representable even for incompressible bytes.
pub const MAX_LZ4_FRAME_PLAINTEXT: usize = 1_069_547_496;

/// Errors from decoding the compressed-block envelope.
#[derive(Debug)]
#[non_exhaustive]
pub enum CompressionError {
    /// The buffer ended before a full block (checksum + header + payload) was
    /// available. Carries how many bytes were needed vs present at the point of
    /// failure, for diagnostics.
    Truncated { needed: usize, available: usize },
    /// `compressed_size` from the header is smaller than the 9-byte header or
    /// larger than `DBMS_MAX_COMPRESSED_SIZE`, so the framing is corrupt.
    InvalidCompressedSize { compressed_size: u32 },
    /// `uncompressed_size` from the header is zero or above the decompressed-size
    /// cap. The core deliberately rejects zero-size frames because the server
    /// writer never emits them. Bounding the size keeps a crafted header from
    /// forcing a huge output allocation.
    InvalidUncompressedSize { uncompressed_size: u32 },
    /// The stored checksum does not match `CityHash128` over the header and
    /// payload. Carries both for reporting, matching the server's
    /// "Checksum doesn't match" diagnostic.
    ChecksumMismatch { expected: u128, actual: u128 },
    /// The method byte selects a codec this crate does not decode. ZSTD (0x90)
    /// reports here pending a dependency decision; unknown bytes also report
    /// here rather than being guessed at.
    UnsupportedCodec { method: u8 },
    /// The LZ4 payload is malformed: a length, offset, or copy that runs out of
    /// bounds. The decoder rejects it instead of panicking or reading past the
    /// buffer.
    Lz4Malformed,
    /// Decompression produced a different number of bytes than the header's
    /// `uncompressed_size`, or a NONE block's payload length did not equal it.
    /// The server makes the same check after decompressing.
    SizeMismatch { expected: usize, actual: usize },
    /// Native decoding completed a TCP block before consuming the final
    /// compressed frame's plaintext. TCP compression is flushed at each block
    /// boundary, so leftover plaintext would belong to a misframed block.
    TrailingPlaintext { remaining: usize },
    /// A requested plaintext chunk size cannot be represented by ClickHouse
    /// compression framing.
    InvalidFrameSize { frame_size: usize },
    /// The requested output buffer reservation could not be satisfied.
    AllocationFailed { requested: usize },
}

impl std::fmt::Display for CompressionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompressionError::Truncated { needed, available } => write!(
                f,
                "compressed block truncated: needed {needed} bytes, {available} available"
            ),
            CompressionError::InvalidCompressedSize { compressed_size } => write!(
                f,
                "invalid compressed_size {compressed_size}: below header size or above 1 GiB limit"
            ),
            CompressionError::InvalidUncompressedSize { uncompressed_size } => write!(
                f,
                "invalid uncompressed_size {uncompressed_size}: zero or above the 1 GiB limit"
            ),
            CompressionError::ChecksumMismatch { expected, actual } => write!(
                f,
                "checksum doesn't match: corrupted data. reference low=0x{:016x} high=0x{:016x}, \
                 actual low=0x{:016x} high=0x{:016x}",
                *expected as u64,
                (*expected >> 64) as u64,
                *actual as u64,
                (*actual >> 64) as u64
            ),
            CompressionError::UnsupportedCodec { method } => {
                if *method == METHOD_ZSTD {
                    write!(
                        f,
                        "unsupported codec ZSTD (method byte 0x90): ZSTD decode is pending a \
                         dependency decision and is not implemented in the zero-dependency core"
                    )
                } else {
                    write!(f, "unsupported compression method byte 0x{method:02x}")
                }
            }
            CompressionError::Lz4Malformed => write!(f, "malformed LZ4 block"),
            CompressionError::SizeMismatch { expected, actual } => write!(
                f,
                "decompressed size {actual} differs from header uncompressed_size {expected}"
            ),
            CompressionError::TrailingPlaintext { remaining } => write!(
                f,
                "{remaining} decompressed bytes remain after the Native block boundary"
            ),
            CompressionError::InvalidFrameSize { frame_size } => write!(
                f,
                "invalid compression frame plaintext size {frame_size}: expected \
                 1..={MAX_LZ4_FRAME_PLAINTEXT}"
            ),
            CompressionError::AllocationFailed { requested } => {
                write!(
                    f,
                    "could not reserve {requested} bytes for compression output"
                )
            }
        }
    }
}

impl std::error::Error for CompressionError {}

/// Whether each block's stored checksum is verified before decompression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ChecksumMode {
    /// Verify the CityHash128 of every block, the safe default. A mismatch is a
    /// [`CompressionError::ChecksumMismatch`].
    #[default]
    Verify,
    /// Skip checksum verification. Use only when the transport already
    /// guarantees integrity and the caller wants the cycles back.
    Skip,
}

/// One decoded ClickHouse compression frame.
///
/// LZ4 plaintext is owned because decompression creates new bytes. A NONE
/// frame borrows its payload from the input slice and performs no allocation or
/// copy. [`Self::consumed`] is the exact compressed frame length, including its
/// checksum and header.
#[derive(Debug)]
pub struct DecodedFrame<'a> {
    plaintext: Cow<'a, [u8]>,
    info: FrameInfo,
}

impl<'a> DecodedFrame<'a> {
    /// The decompressed bytes carried by this frame.
    pub fn plaintext(&self) -> &[u8] {
        &self.plaintext
    }

    /// Number of compressed input bytes occupied by this frame.
    pub fn consumed(&self) -> usize {
        self.info.consumed()
    }

    /// Number of plaintext bytes carried by this frame.
    pub fn plaintext_len(&self) -> usize {
        self.info.plaintext_len()
    }

    /// Validate a completed native TCP block against this frame's plaintext.
    ///
    /// The TCP server flushes compression after every Native block, so Native
    /// decoding must consume all plaintext in the frame where that block ends.
    /// `plaintext_consumed` is relative to this frame, not the total bytes
    /// consumed across every frame of the Native block.
    /// Do not apply this check to HTTP `compress=1`, whose frame boundaries are
    /// independent of Native block boundaries.
    pub fn finish_block(&self, plaintext_consumed: usize) -> Result<(), CompressionError> {
        self.info.finish_block(plaintext_consumed)
    }
}

/// Metadata for one frame appended by [`decompress_frame_into`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    consumed: usize,
    plaintext_len: usize,
}

impl FrameInfo {
    /// Number of compressed input bytes occupied by this frame.
    pub fn consumed(&self) -> usize {
        self.consumed
    }

    /// Number of plaintext bytes appended for this frame.
    pub fn plaintext_len(&self) -> usize {
        self.plaintext_len
    }

    /// Validate a completed native TCP block against this frame's plaintext.
    ///
    /// `plaintext_consumed` is relative to this final frame, not the total
    /// plaintext consumed across all frames of the Native block.
    pub fn finish_block(&self, plaintext_consumed: usize) -> Result<(), CompressionError> {
        if plaintext_consumed > self.plaintext_len {
            return Err(CompressionError::SizeMismatch {
                expected: self.plaintext_len,
                actual: plaintext_consumed,
            });
        }
        let remaining = self.plaintext_len - plaintext_consumed;
        if remaining != 0 {
            return Err(CompressionError::TrailingPlaintext { remaining });
        }
        Ok(())
    }
}

/// One parsed compressed block: where it ends in the input and how to produce
/// its decompressed bytes.
struct BlockHeader {
    method: u8,
    /// `compressed_size` from the header: the 9-byte header plus payload, in
    /// bytes, excluding the checksum.
    compressed_size: usize,
    uncompressed_size: usize,
    /// Offset in the input where the 9-byte header (method byte) starts.
    header_start: usize,
    /// Offset in the input where the payload starts (header_start + 9).
    payload_start: usize,
    /// Offset in the input one past the end of this block.
    block_end: usize,
}

/// Parse and validate one compressed block's framing at `data[pos..]`.
///
/// Returns the parsed header on success. Does not verify the checksum or
/// decompress; that is the caller's job so the checksum can be made optional
/// and the payload borrowed without copying.
fn parse_block_header(data: &[u8], pos: usize) -> Result<BlockHeader, CompressionError> {
    let available = data.len() - pos;
    // Need at least the checksum and the 9-byte header to read the sizes.
    if available < CHECKSUM_SIZE + HEADER_SIZE {
        return Err(CompressionError::Truncated {
            needed: CHECKSUM_SIZE + HEADER_SIZE,
            available,
        });
    }

    let header_start = pos + CHECKSUM_SIZE;
    let method = data[header_start];

    // compressed_size and uncompressed_size are 4-byte LE, at header offsets
    // 1 and 5 (server `readCompressedBlockSize` / `readDecompressedBlockSize`).
    let compressed_size = u32::from_le_bytes([
        data[header_start + 1],
        data[header_start + 2],
        data[header_start + 3],
        data[header_start + 4],
    ]);
    let uncompressed_size = u32::from_le_bytes([
        data[header_start + 5],
        data[header_start + 6],
        data[header_start + 7],
        data[header_start + 8],
    ]);

    // compressed_size must include the header and stay under the 1 GiB cap.
    if compressed_size > MAX_COMPRESSED_SIZE || (compressed_size as usize) < HEADER_SIZE {
        return Err(CompressionError::InvalidCompressedSize { compressed_size });
    }
    // uncompressed_size drives the output allocation, so bound it before the
    // caller reserves. The core rejects zero because server writers do not emit
    // it; a value above the cap is refused instead of forcing a huge allocation.
    if uncompressed_size == 0 || uncompressed_size > MAX_DECOMPRESSED_SIZE {
        return Err(CompressionError::InvalidUncompressedSize { uncompressed_size });
    }
    // Codec support is known from the fixed header. Reject it before waiting
    // for an attacker-controlled payload length.
    if method != METHOD_NONE && method != METHOD_LZ4 {
        return Err(CompressionError::UnsupportedCodec { method });
    }
    let compressed_size = compressed_size as usize;

    // The whole block is checksum + compressed_size bytes.
    let block_total = CHECKSUM_SIZE + compressed_size;
    if available < block_total {
        return Err(CompressionError::Truncated {
            needed: block_total,
            available,
        });
    }

    Ok(BlockHeader {
        method,
        compressed_size,
        uncompressed_size: uncompressed_size as usize,
        header_start,
        payload_start: header_start + HEADER_SIZE,
        block_end: pos + block_total,
    })
}

/// Verify the stored CityHash128 checksum of one block against its header and
/// payload, the `compressed_size` bytes starting at the method byte.
fn verify_checksum(data: &[u8], pos: usize, header: &BlockHeader) -> Result<(), CompressionError> {
    // The 16 checksum bytes are present: parse_block_header already required
    // CHECKSUM_SIZE + HEADER_SIZE bytes from `pos`. Read the two LE u64s by
    // explicit indexing, no fallible conversion on the wire-data path.
    let expected_low = u64::from_le_bytes([
        data[pos],
        data[pos + 1],
        data[pos + 2],
        data[pos + 3],
        data[pos + 4],
        data[pos + 5],
        data[pos + 6],
        data[pos + 7],
    ]);
    let expected_high = u64::from_le_bytes([
        data[pos + 8],
        data[pos + 9],
        data[pos + 10],
        data[pos + 11],
        data[pos + 12],
        data[pos + 13],
        data[pos + 14],
        data[pos + 15],
    ]);
    let covered = &data[header.header_start..header.header_start + header.compressed_size];
    let actual = city_hash128(covered);
    if actual.low64 != expected_low || actual.high64 != expected_high {
        return Err(CompressionError::ChecksumMismatch {
            expected: ((expected_high as u128) << 64) | expected_low as u128,
            actual: ((actual.high64 as u128) << 64) | actual.low64 as u128,
        });
    }
    Ok(())
}

/// Decode exactly one ClickHouse compressed frame from the start of `data`.
///
/// The returned frame reports how many compressed bytes it consumed. Bytes
/// after that boundary are never inspected by decompression, so a native TCP
/// engine can pass a slice that also contains the following packet.
///
/// If the checksum, header, or payload is incomplete, this returns
/// [`CompressionError::Truncated`] with the total bytes needed for this frame
/// and the bytes currently available. Size fields are bounded before checksum
/// calculation or output allocation. A declared zero-length plaintext frame is
/// rejected as corrupt.
pub fn decompress_frame(
    data: &[u8],
    checksum_mode: ChecksumMode,
) -> Result<DecodedFrame<'_>, CompressionError> {
    let header = parse_block_header(data, 0)?;

    if checksum_mode == ChecksumMode::Verify {
        verify_checksum(data, 0, &header)?;
    }

    let payload = &data[header.payload_start..header.block_end];
    let plaintext = match header.method {
        METHOD_NONE => {
            if payload.len() != header.uncompressed_size {
                return Err(CompressionError::SizeMismatch {
                    expected: header.uncompressed_size,
                    actual: payload.len(),
                });
            }
            Cow::Borrowed(payload)
        }
        METHOD_LZ4 => Cow::Owned(lz4::decompress_block(payload, header.uncompressed_size)?),
        other => return Err(CompressionError::UnsupportedCodec { method: other }),
    };

    Ok(DecodedFrame {
        plaintext,
        info: FrameInfo {
            consumed: header.block_end,
            plaintext_len: header.uncompressed_size,
        },
    })
}

/// Decode one ClickHouse compressed frame and append its plaintext to `out`.
///
/// The returned metadata reports this frame's compressed and plaintext lengths.
/// Existing bytes in `out` are preserved. Any error restores its original
/// length, allowing a streaming caller to reuse one allocation safely.
pub fn decompress_frame_into(
    data: &[u8],
    checksum_mode: ChecksumMode,
    out: &mut Vec<u8>,
) -> Result<FrameInfo, CompressionError> {
    let header = parse_block_header(data, 0)?;

    if checksum_mode == ChecksumMode::Verify {
        verify_checksum(data, 0, &header)?;
    }

    let out_start = out.len();
    let payload = &data[header.payload_start..header.block_end];
    let result = match header.method {
        METHOD_NONE => {
            if payload.len() != header.uncompressed_size {
                Err(CompressionError::SizeMismatch {
                    expected: header.uncompressed_size,
                    actual: payload.len(),
                })
            } else {
                out.try_reserve(payload.len())
                    .map_err(|_| CompressionError::AllocationFailed {
                        requested: payload.len(),
                    })
                    .map(|()| out.extend_from_slice(payload))
            }
        }
        METHOD_LZ4 => lz4::decompress_block_into(payload, header.uncompressed_size, out),
        other => Err(CompressionError::UnsupportedCodec { method: other }),
    };
    if let Err(err) = result {
        out.truncate(out_start);
        return Err(err);
    }

    Ok(FrameInfo {
        consumed: header.block_end,
        plaintext_len: out.len() - out_start,
    })
}

/// Compress `plaintext` as one ClickHouse LZ4 frame and return its wire bytes.
///
/// This is a convenience wrapper around [`write_lz4_frame`]. Empty plaintext is
/// rejected because ClickHouse writers do not emit zero-size frames.
pub fn compress_lz4_frame(plaintext: &[u8]) -> Result<Vec<u8>, CompressionError> {
    let mut out = Vec::new();
    write_lz4_frame(plaintext, &mut out)?;
    Ok(out)
}

/// Append one ClickHouse LZ4 frame to `out`.
///
/// The frame is built in one output buffer. Checksum and header space is
/// reserved first, then the LZ4 encoder writes its payload directly after it.
/// Capacity for the standard LZ4 worst case is reserved up front, so payload
/// growth does not reallocate and copy bytes. The checksum is calculated in
/// place, so framing adds no payload copy. Each call allocates only the output
/// capacity, when the caller has not already reserved it. The encoder's hash
/// table lives on the stack.
pub fn write_lz4_frame(plaintext: &[u8], out: &mut Vec<u8>) -> Result<(), CompressionError> {
    if plaintext.is_empty() || plaintext.len() > MAX_LZ4_FRAME_PLAINTEXT {
        return Err(CompressionError::InvalidFrameSize {
            frame_size: plaintext.len(),
        });
    }

    let frame_start = out.len();
    // LZ4_compressBound(input) is input + input / 255 + 16. The plaintext cap
    // above is 1 GiB, so this arithmetic fits every supported usize target.
    let frame_capacity = CHECKSUM_SIZE + HEADER_SIZE + plaintext.len() + plaintext.len() / 255 + 16;
    out.try_reserve(frame_capacity)
        .map_err(|_| CompressionError::AllocationFailed {
            requested: frame_capacity,
        })?;
    out.resize(frame_start + CHECKSUM_SIZE + HEADER_SIZE, 0);
    lz4::compress_block_into(plaintext, out);

    let compressed_size = out.len() - frame_start - CHECKSUM_SIZE;
    if compressed_size > MAX_COMPRESSED_SIZE as usize {
        out.truncate(frame_start);
        return Err(CompressionError::InvalidCompressedSize {
            compressed_size: compressed_size as u32,
        });
    }

    let header_start = frame_start + CHECKSUM_SIZE;
    out[header_start] = METHOD_LZ4;
    out[header_start + 1..header_start + 5]
        .copy_from_slice(&(compressed_size as u32).to_le_bytes());
    out[header_start + 5..header_start + 9]
        .copy_from_slice(&(plaintext.len() as u32).to_le_bytes());

    let checksum = city_hash128(&out[header_start..]);
    out[frame_start..frame_start + 8].copy_from_slice(&checksum.low64.to_le_bytes());
    out[frame_start + 8..header_start].copy_from_slice(&checksum.high64.to_le_bytes());
    Ok(())
}

/// Compress a plaintext stream into back-to-back ClickHouse LZ4 frames.
///
/// `max_frame_plaintext` controls the maximum uncompressed bytes per frame and
/// must be in `1..=MAX_LZ4_FRAME_PLAINTEXT`. This lets a caller choose a bounded
/// working set for large inserts. Native TCP callers should invoke this once per
/// Native block so no frame crosses a block boundary.
pub fn compress_lz4_frames(
    plaintext: &[u8],
    max_frame_plaintext: usize,
) -> Result<Vec<u8>, CompressionError> {
    if max_frame_plaintext == 0 || max_frame_plaintext > MAX_LZ4_FRAME_PLAINTEXT {
        return Err(CompressionError::InvalidFrameSize {
            frame_size: max_frame_plaintext,
        });
    }
    if plaintext.is_empty() {
        return Err(CompressionError::InvalidFrameSize { frame_size: 0 });
    }

    let mut out = Vec::new();
    for chunk in plaintext.chunks(max_frame_plaintext) {
        write_lz4_frame(chunk, &mut out)?;
    }
    Ok(out)
}

/// Decompress a complete buffer of one or more ClickHouse compressed blocks into
/// the plain Native byte stream.
///
/// `data` is the whole compressed stream, a back-to-back sequence of compressed
/// blocks with no padding between them. The returned `Vec<u8>` is the
/// concatenation of every block's decompressed output, which is exactly the
/// plain Native bytes [`crate::native::decode::decode_all_bytes`] expects.
///
/// With [`ChecksumMode::Verify`] (the default) each block's CityHash128 is
/// checked before it is decompressed and a mismatch fails the whole call. A
/// truncated final block (fewer bytes than its header promises) is a
/// [`CompressionError::Truncated`] error, never a silent short read.
pub fn decompress_native_stream(
    data: &[u8],
    checksum_mode: ChecksumMode,
) -> Result<Vec<u8>, CompressionError> {
    let mut out: Vec<u8> = Vec::new();
    let mut pos = 0usize;

    while pos < data.len() {
        let frame = decompress_frame_into(&data[pos..], checksum_mode, &mut out)?;
        pos += frame.consumed();
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a framed compressed block: prepend the LE checksum over the 9-byte
    /// header plus payload, then the method byte, compressed_size,
    /// uncompressed_size, and payload. `compressed_size` is `9 + payload.len()`.
    fn frame_block(method: u8, uncompressed_size: u32, payload: &[u8]) -> Vec<u8> {
        let compressed_size = (HEADER_SIZE + payload.len()) as u32;
        let mut header = Vec::with_capacity(HEADER_SIZE + payload.len());
        header.push(method);
        header.extend_from_slice(&compressed_size.to_le_bytes());
        header.extend_from_slice(&uncompressed_size.to_le_bytes());
        header.extend_from_slice(payload);

        let checksum = city_hash128(&header);
        let mut block = Vec::with_capacity(CHECKSUM_SIZE + header.len());
        block.extend_from_slice(&checksum.low64.to_le_bytes());
        block.extend_from_slice(&checksum.high64.to_le_bytes());
        block.extend_from_slice(&header);
        block
    }

    #[test]
    fn none_passthrough_roundtrips() {
        let raw = b"native block bytes here, user_1 and user_2";
        let block = frame_block(METHOD_NONE, raw.len() as u32, raw);
        let out = decompress_native_stream(&block, ChecksumMode::Verify).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn lz4_block_roundtrips() {
        // The offset-1 run vector: 40 bytes of 0x79.
        let payload = [
            0x1f, 0x79, 0x01, 0x00, 0x0f, 0x50, 0x79, 0x79, 0x79, 0x79, 0x79,
        ];
        let block = frame_block(METHOD_LZ4, 40, &payload);
        let out = decompress_native_stream(&block, ChecksumMode::Verify).unwrap();
        assert_eq!(out, vec![0x79u8; 40]);
    }

    #[test]
    fn multiple_blocks_concatenate() {
        // Two blocks, one NONE and one LZ4, decompress and concatenate in order.
        let raw1 = b"first-block-payload";
        let block1 = frame_block(METHOD_NONE, raw1.len() as u32, raw1);

        let lz4_payload = [0x50, 0x13, 0x79, 0x01, 0x02, 0x03]; // 5 literals
        let block2 = frame_block(METHOD_LZ4, 5, &lz4_payload);

        let mut stream = block1.clone();
        stream.extend_from_slice(&block2);

        let out = decompress_native_stream(&stream, ChecksumMode::Verify).unwrap();
        let mut expected = raw1.to_vec();
        expected.extend_from_slice(&[0x13, 0x79, 0x01, 0x02, 0x03]);
        assert_eq!(out, expected);
    }

    #[test]
    fn empty_compressed_stream_is_empty() {
        assert_eq!(
            decompress_native_stream(&[], ChecksumMode::Verify).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn frame_into_appends_reuses_and_leaves_suffix() {
        let first_raw = b"first Native frame";
        let second_raw = b"user_13-user_13-user_13-user_13-";
        let first = frame_block(METHOD_NONE, first_raw.len() as u32, first_raw);
        let second = compress_lz4_frame(second_raw).unwrap();
        let suffix = b"\x05following-packet";
        let mut input = first.clone();
        input.extend_from_slice(&second);
        input.extend_from_slice(suffix);

        let mut out = b"prefix:".to_vec();
        out.reserve(first_raw.len() + second_raw.len());
        let capacity = out.capacity();
        let first_info = decompress_frame_into(&input, ChecksumMode::Verify, &mut out).unwrap();
        assert_eq!(first_info.consumed(), first.len());
        assert_eq!(first_info.plaintext_len(), first_raw.len());
        let second_info = decompress_frame_into(
            &input[first_info.consumed()..],
            ChecksumMode::Verify,
            &mut out,
        )
        .unwrap();
        assert_eq!(second_info.consumed(), second.len());
        assert_eq!(second_info.plaintext_len(), second_raw.len());
        assert_eq!(out.capacity(), capacity);
        assert_eq!(out, [b"prefix:".as_slice(), first_raw, second_raw].concat());
        let consumed = first_info.consumed() + second_info.consumed();
        assert_eq!(&input[consumed..], suffix);
    }

    #[test]
    fn frame_into_rolls_back_on_errors() {
        let raw = b"user_13-user_13-user_13-user_13-";
        let mut checksum_error = compress_lz4_frame(raw).unwrap();
        checksum_error[0] ^= 0x01;

        let mut malformed = frame_block(METHOD_LZ4, 5, &[0x10, 0x79, 0x00, 0x00]);
        let checksum = city_hash128(&malformed[CHECKSUM_SIZE..]);
        malformed[..8].copy_from_slice(&checksum.low64.to_le_bytes());
        malformed[8..CHECKSUM_SIZE].copy_from_slice(&checksum.high64.to_le_bytes());

        // The offset-1 match has no frame-local output. It must not refer to the
        // caller's existing prefix.
        let previous_frame_offset = frame_block(METHOD_LZ4, 4, &[0x00, 0x01, 0x00]);
        // A one-byte payload cannot expand to 1 GiB. Reject it before reserving.
        let excessive_expansion = frame_block(METHOD_LZ4, MAX_DECOMPRESSED_SIZE, &[0x00]);

        for frame in [
            &checksum_error,
            &malformed,
            &previous_frame_offset,
            &excessive_expansion,
        ] {
            let mut out = b"keep me".to_vec();
            let err = decompress_frame_into(frame, ChecksumMode::Verify, &mut out).unwrap_err();
            assert!(matches!(
                err,
                CompressionError::ChecksumMismatch { .. } | CompressionError::Lz4Malformed
            ));
            assert_eq!(out, b"keep me");
        }
    }

    #[test]
    fn one_frame_leaves_following_packet_bytes_untouched() {
        let raw = b"one Native block";
        let frame_bytes = frame_block(METHOD_NONE, raw.len() as u32, raw);
        let suffix = b"\x05following-packet";
        let mut input = frame_bytes.clone();
        input.extend_from_slice(suffix);

        let frame = decompress_frame(&input, ChecksumMode::Verify).unwrap();
        assert_eq!(frame.plaintext(), raw);
        assert_eq!(frame.consumed(), frame_bytes.len());
        assert_eq!(&input[frame.consumed()..], suffix);
    }

    #[test]
    fn incomplete_frame_reports_total_bytes_needed() {
        let raw = b"Native bytes split across transport reads";
        let frame = frame_block(METHOD_NONE, raw.len() as u32, raw);

        let err = decompress_frame(&frame[..10], ChecksumMode::Verify).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::Truncated {
                needed: 25,
                available: 10
            }
        ));

        let available = frame.len() - 3;
        let err = decompress_frame(&frame[..available], ChecksumMode::Verify).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::Truncated { needed, available: got }
                if needed == frame.len() && got == available
        ));
    }

    #[test]
    fn tcp_native_block_may_use_multiple_frames() {
        let first = b"first half of one Native block";
        let second = b", then its second half";
        let first_frame = frame_block(METHOD_NONE, first.len() as u32, first);
        let second_frame = frame_block(METHOD_NONE, second.len() as u32, second);
        let packet_suffix = b"\x05next-packet";
        let mut input = first_frame.clone();
        input.extend_from_slice(&second_frame);
        input.extend_from_slice(packet_suffix);

        let decoded_first = decompress_frame(&input, ChecksumMode::Verify).unwrap();
        let decoded_second =
            decompress_frame(&input[decoded_first.consumed()..], ChecksumMode::Verify).unwrap();
        let total_plaintext_consumed =
            decoded_first.plaintext().len() + decoded_second.plaintext().len();
        let consumed_before_second = decoded_first.plaintext().len();
        decoded_second
            .finish_block(total_plaintext_consumed - consumed_before_second)
            .unwrap();

        let mut plain = decoded_first.plaintext().to_vec();
        plain.extend_from_slice(decoded_second.plaintext());
        let compressed_consumed = decoded_first.consumed() + decoded_second.consumed();
        assert_eq!(plain, [first.as_slice(), second.as_slice()].concat());
        assert_eq!(&input[compressed_consumed..], packet_suffix);
    }

    #[test]
    fn tcp_block_boundary_rejects_leftover_plaintext() {
        let raw = b"Native block plus bytes that cannot belong to the next block";
        let frame_bytes = frame_block(METHOD_NONE, raw.len() as u32, raw);
        let frame = decompress_frame(&frame_bytes, ChecksumMode::Verify).unwrap();
        let err = frame.finish_block(raw.len() - 7).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::TrailingPlaintext { remaining: 7 }
        ));
    }

    #[test]
    fn finish_block_rejects_overconsumption() {
        let raw = b"one Native block";
        let frame_bytes = frame_block(METHOD_NONE, raw.len() as u32, raw);
        let frame = decompress_frame(&frame_bytes, ChecksumMode::Verify).unwrap();
        let err = frame.finish_block(raw.len() + 1).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::SizeMismatch {
                expected,
                actual
            } if expected == raw.len() && actual == raw.len() + 1
        ));
    }

    #[test]
    fn lz4_writer_roundtrips_one_frame() {
        let raw = b"user_13-user_13-user_13-user_13-";
        let encoded = compress_lz4_frame(raw).unwrap();
        assert_eq!(encoded[CHECKSUM_SIZE], METHOD_LZ4);

        let decoded = decompress_frame(&encoded, ChecksumMode::Verify).unwrap();
        assert_eq!(decoded.plaintext(), raw);
        assert_eq!(decoded.consumed(), encoded.len());
    }

    #[test]
    fn lz4_writer_appends_without_changing_prefix() {
        let prefix = b"packet-code-and-block-name";
        let mut encoded = prefix.to_vec();
        write_lz4_frame(b"one Native block body", &mut encoded).unwrap();
        assert_eq!(&encoded[..prefix.len()], prefix);

        let decoded = decompress_frame(&encoded[prefix.len()..], ChecksumMode::Verify).unwrap();
        assert_eq!(decoded.plaintext(), b"one Native block body");
    }

    #[test]
    fn lz4_stream_writer_chunks_large_native_block() {
        let raw = vec![0x79; 79];
        let encoded = compress_lz4_frames(&raw, 31).unwrap();
        let mut pos = 0;
        let mut decoded = Vec::new();
        let mut frame_count = 0;
        while pos < encoded.len() {
            let frame = decompress_frame(&encoded[pos..], ChecksumMode::Verify).unwrap();
            decoded.extend_from_slice(frame.plaintext());
            pos += frame.consumed();
            frame_count += 1;
        }

        assert_eq!(frame_count, 3);
        assert_eq!(decoded, raw);
    }

    #[test]
    fn lz4_writer_rejects_zero_plaintext_and_zero_chunk_limit() {
        assert!(matches!(
            compress_lz4_frame(&[]).unwrap_err(),
            CompressionError::InvalidFrameSize { frame_size: 0 }
        ));
        assert!(matches!(
            compress_lz4_frames(b"Native bytes", 0).unwrap_err(),
            CompressionError::InvalidFrameSize { frame_size: 0 }
        ));
    }

    #[test]
    fn lz4_writer_preserves_output_on_error() {
        let mut out = b"existing packet bytes".to_vec();
        let original = out.clone();
        let err = write_lz4_frame(&[], &mut out).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::InvalidFrameSize { frame_size: 0 }
        ));
        assert_eq!(out, original);
    }

    #[test]
    fn lz4_writer_bound_fits_compressed_limit() {
        let bound = HEADER_SIZE + MAX_LZ4_FRAME_PLAINTEXT + MAX_LZ4_FRAME_PLAINTEXT / 255 + 16;
        assert_eq!(bound, MAX_COMPRESSED_SIZE as usize);

        let err = compress_lz4_frames(b"Native bytes", MAX_LZ4_FRAME_PLAINTEXT + 1).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::InvalidFrameSize { frame_size }
                if frame_size == MAX_LZ4_FRAME_PLAINTEXT + 1
        ));
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let raw = b"some native bytes";
        let mut block = frame_block(METHOD_NONE, raw.len() as u32, raw);
        // Corrupt one payload byte after the checksum and header.
        let last = block.len() - 1;
        block[last] ^= 0xFF;
        let err = decompress_native_stream(&block, ChecksumMode::Verify).unwrap_err();
        assert!(matches!(err, CompressionError::ChecksumMismatch { .. }));
    }

    #[test]
    fn lz4_checksum_mismatch_is_rejected_before_decode() {
        let raw = b"user_13-user_13-user_13-user_13-";
        let mut block = compress_lz4_frame(raw).unwrap();
        block[0] ^= 0x01;
        let err = decompress_frame(&block, ChecksumMode::Verify).unwrap_err();
        assert!(matches!(err, CompressionError::ChecksumMismatch { .. }));
    }

    #[test]
    fn checksum_skip_ignores_corruption() {
        // With Skip, a corrupted payload is not flagged; the (now wrong) bytes
        // pass through. This documents the opt-out path.
        let raw = b"some native bytes";
        let mut block = frame_block(METHOD_NONE, raw.len() as u32, raw);
        let last = block.len() - 1;
        block[last] ^= 0xFF;
        let out = decompress_native_stream(&block, ChecksumMode::Skip).unwrap();
        assert_eq!(out.len(), raw.len());
        assert_ne!(out.as_slice(), raw.as_slice());
    }

    #[test]
    fn unknown_method_byte_is_rejected() {
        let raw = b"payload";
        // Frame with an unknown method byte; recompute checksum so framing is
        // otherwise valid and the codec dispatch is what fails.
        let block = frame_block(0x7f, raw.len() as u32, raw);
        let err = decompress_native_stream(&block, ChecksumMode::Verify).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::UnsupportedCodec { method: 0x7f }
        ));
    }

    #[test]
    fn unsupported_method_is_rejected_before_payload_arrives() {
        for method in [0x7f, METHOD_ZSTD] {
            for mode in [ChecksumMode::Verify, ChecksumMode::Skip] {
                let mut block = vec![0u8; CHECKSUM_SIZE + HEADER_SIZE];
                let header_start = CHECKSUM_SIZE;
                block[header_start] = method;
                block[header_start + 1..header_start + 5]
                    .copy_from_slice(&MAX_COMPRESSED_SIZE.to_le_bytes());
                block[header_start + 5..header_start + 9].copy_from_slice(&13u32.to_le_bytes());

                let err = decompress_frame(&block, mode).unwrap_err();
                assert!(matches!(
                    err,
                    CompressionError::UnsupportedCodec { method: actual }
                        if actual == method
                ));
            }
        }

        let mut invalid_size = vec![0u8; CHECKSUM_SIZE + HEADER_SIZE];
        invalid_size[CHECKSUM_SIZE] = 0x7f;
        invalid_size[CHECKSUM_SIZE + 1..CHECKSUM_SIZE + 5].copy_from_slice(&8u32.to_le_bytes());
        invalid_size[CHECKSUM_SIZE + 5..CHECKSUM_SIZE + 9].copy_from_slice(&13u32.to_le_bytes());
        let err = decompress_frame(&invalid_size, ChecksumMode::Skip).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::InvalidCompressedSize { compressed_size: 8 }
        ));
    }

    #[test]
    fn zstd_reports_pending_dependency() {
        let raw = b"payload";
        let block = frame_block(METHOD_ZSTD, raw.len() as u32, raw);
        let err = decompress_native_stream(&block, ChecksumMode::Verify).unwrap_err();
        match err {
            CompressionError::UnsupportedCodec { method } => {
                assert_eq!(method, METHOD_ZSTD);
                // The message names ZSTD and the pending dependency decision.
                let msg = err.to_string();
                assert!(msg.contains("ZSTD"), "message was: {msg}");
                assert!(msg.contains("dependency"), "message was: {msg}");
            }
            other => panic!("expected UnsupportedCodec, got {other:?}"),
        }
    }

    #[test]
    fn compressed_size_over_one_gib_is_rejected() {
        // Hand-build a header with compressed_size = 0x40000001 (just over 1 GiB)
        // and enough bytes to pass the initial 25-byte availability check. The
        // size check fires before any large allocation.
        let mut block = vec![0u8; CHECKSUM_SIZE + HEADER_SIZE];
        let header_start = CHECKSUM_SIZE;
        block[header_start] = METHOD_LZ4;
        block[header_start + 1..header_start + 5].copy_from_slice(&0x4000_0001u32.to_le_bytes());
        block[header_start + 5..header_start + 9].copy_from_slice(&16u32.to_le_bytes());
        let err = decompress_native_stream(&block, ChecksumMode::Skip).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::InvalidCompressedSize {
                compressed_size: 0x4000_0001
            }
        ));
    }

    #[test]
    fn compressed_size_below_header_is_rejected() {
        // compressed_size must include the 9-byte header. A value below 9 is
        // corrupt framing.
        let mut block = vec![0u8; CHECKSUM_SIZE + HEADER_SIZE];
        let header_start = CHECKSUM_SIZE;
        block[header_start] = METHOD_LZ4;
        block[header_start + 1..header_start + 5].copy_from_slice(&8u32.to_le_bytes());
        block[header_start + 5..header_start + 9].copy_from_slice(&4u32.to_le_bytes());
        let err = decompress_native_stream(&block, ChecksumMode::Skip).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::InvalidCompressedSize { compressed_size: 8 }
        ));
    }

    #[test]
    fn truncated_block_is_error_not_panic() {
        let raw = b"native block bytes";
        let block = frame_block(METHOD_NONE, raw.len() as u32, raw);
        // Drop the last few bytes so the payload is short of compressed_size.
        let truncated = &block[..block.len() - 3];
        let err = decompress_native_stream(truncated, ChecksumMode::Verify).unwrap_err();
        assert!(matches!(err, CompressionError::Truncated { .. }));
    }

    #[test]
    fn truncated_header_is_error_not_panic() {
        // Fewer than checksum + header bytes available.
        let data = [0u8; 10];
        let err = decompress_native_stream(&data, ChecksumMode::Verify).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::Truncated {
                needed: 25,
                available: 10
            }
        ));
    }

    #[test]
    fn uncompressed_size_over_cap_is_rejected() {
        // A crafted header declaring a huge uncompressed_size is refused before
        // any allocation, even though the framing and checksum are otherwise
        // valid. This is the unbounded-allocation guard.
        let block = frame_block(METHOD_NONE, MAX_DECOMPRESSED_SIZE + 1, b"tiny");
        let err = decompress_native_stream(&block, ChecksumMode::Verify).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::InvalidUncompressedSize { uncompressed_size }
                if uncompressed_size == MAX_DECOMPRESSED_SIZE + 1
        ));
    }

    #[test]
    fn zero_uncompressed_size_is_rejected() {
        // Release server readers accept this, but their writer never emits it.
        // The core deliberately keeps the reader and writer domains aligned.
        let block = frame_block(METHOD_NONE, 0, b"");
        let err = decompress_native_stream(&block, ChecksumMode::Verify).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::InvalidUncompressedSize {
                uncompressed_size: 0
            }
        ));
    }

    #[test]
    fn none_payload_length_must_equal_uncompressed_size() {
        // A NONE block whose payload is longer than uncompressed_size is corrupt:
        // the server requires exact equality rather than copying a prefix.
        let block = frame_block(METHOD_NONE, 5, b"ten-bytes!");
        let err = decompress_native_stream(&block, ChecksumMode::Verify).unwrap_err();
        assert!(matches!(
            err,
            CompressionError::SizeMismatch {
                expected: 5,
                actual: 10
            }
        ));
    }
}
