use std::io;

/// A cursor over an in-memory byte slice for decoding Native format bytes.
///
/// The decoder's entire input is already an in-memory `&[u8]`, so decoding over
/// a slice cursor instead of a generic `io::Read` removes a virtual call and a
/// bounds check per byte and lets the variable-length string path borrow its
/// payload as a sub-slice instead of copying through a temporary buffer.
///
/// Running off the end of the available bytes is reported as
/// [`io::ErrorKind::UnexpectedEof`], byte-for-byte compatible with the previous
/// `io::Cursor` based path. The streaming decoder relies on that error kind to
/// tell "need more bytes" apart from a real decode error, so every read that can
/// hit the end surfaces `UnexpectedEof` and nothing else.
pub struct ByteReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

#[inline]
fn unexpected_eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "failed to fill whole buffer")
}

impl<'a> ByteReader<'a> {
    #[inline]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Number of bytes consumed so far. The block decoder uses this to advance
    /// the streaming decoder's position after a block decodes successfully.
    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes not yet consumed.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    /// Cap a speculative `Vec::with_capacity(count)` at the number of items the
    /// not-yet-read input could actually produce.
    ///
    /// A block header's `num_rows`/`num_cols` is only bounded by
    /// `check_header_count` at one byte per item, so a per-item output buffer
    /// wider than a byte (an 8-byte aggregate offset, a `Field`/`Column`
    /// record) could otherwise reserve several times the input size for a
    /// hostile count. Every item of a run occupies at least `min_item_bytes` on
    /// the wire, so the remaining input holds at most
    /// `remaining() / min_item_bytes` of them; reserving beyond that is pure
    /// speculation. This mirrors the read-before-allocate cap in the
    /// `decode_primitive!` macro, which reserves exactly the bytes it already
    /// read.
    ///
    /// The result is never larger than `count`, so a legitimate run (whose
    /// bytes are all present) reserves its full size and never reallocates; the
    /// cap only bites a truncated or inflated count, whose decode fails anyway.
    /// `min_item_bytes` is clamped to at least 1 so a zero can never divide.
    #[inline]
    pub(crate) fn capacity_for(&self, count: usize, min_item_bytes: usize) -> usize {
        count.min(self.remaining() / min_item_bytes.max(1))
    }

    /// Read a single byte.
    #[inline]
    pub fn read_u8(&mut self) -> io::Result<u8> {
        let b = *self.bytes.get(self.pos).ok_or_else(unexpected_eof)?;
        self.pos += 1;
        Ok(b)
    }

    /// Borrow the next `len` bytes as a sub-slice, advancing past them.
    ///
    /// The returned slice borrows the reader's input, so the string path can
    /// hand it straight to a single `extend_from_slice` with no intermediate
    /// per-value allocation.
    #[inline]
    pub fn read_slice(&mut self, len: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(len).ok_or_else(unexpected_eof)?;
        let slice = self.bytes.get(self.pos..end).ok_or_else(unexpected_eof)?;
        self.pos = end;
        Ok(slice)
    }

    /// Borrow all not-yet-consumed bytes, carrying the input lifetime `'a`.
    ///
    /// Lets a variable-width decoder walk row boundaries over one borrowed slice
    /// with a LOCAL cursor index, instead of driving `self.pos` through a method
    /// call per byte. The returned slice does not borrow `self`, so the caller
    /// can advance the cursor with [`ByteReader::skip`] afterwards. `pos` never
    /// exceeds `bytes.len()` (every advance is bounds-checked), so this is always
    /// a valid sub-slice; the `unwrap_or` is a non-panicking guard only.
    #[inline]
    pub fn remaining_slice(&self) -> &'a [u8] {
        self.bytes.get(self.pos..).unwrap_or(&[])
    }

    /// Borrow bytes already consumed from `start` through the current cursor.
    ///
    /// Used by variable-width decoders that must walk row boundaries first and
    /// then copy the complete contiguous body once. An invalid future cursor or
    /// start is reported as `InvalidData`, never as a slice panic.
    #[inline]
    pub fn consumed_slice(&self, start: usize) -> io::Result<&'a [u8]> {
        self.bytes.get(start..self.pos).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid consumed byte range")
        })
    }

    /// Skip `len` bytes without reading them. Used by the completeness scan to
    /// walk past fixed-width column data without allocating.
    #[inline]
    pub fn skip(&mut self, len: usize) -> io::Result<()> {
        let end = self.pos.checked_add(len).ok_or_else(unexpected_eof)?;
        if end > self.bytes.len() {
            return Err(unexpected_eof());
        }
        self.pos = end;
        Ok(())
    }

    /// Read a fixed 8-byte little-endian `u64`.
    ///
    /// `LowCardinality` framing words (the key version, the index type word, the
    /// dictionary size, and the per-block row count) are written with the
    /// server's fixed-width `writeBinaryLittleEndian`, not as varints.
    #[inline]
    pub fn read_u64_le(&mut self) -> io::Result<u64> {
        let bytes = self.read_slice(8)?;
        // `read_slice(8)` returns exactly 8 bytes, so the conversion cannot fail.
        Ok(u64::from_le_bytes(
            bytes.try_into().map_err(|_| unexpected_eof())?,
        ))
    }

    /// Read a LEB128-encoded unsigned integer.
    ///
    /// A shift of 64 bits or more (a 10th continuation byte) is rejected as
    /// `InvalidData`, matching the previous reader.
    #[inline]
    pub fn read_varint(&mut self) -> io::Result<u64> {
        let mut result: u64 = 0;
        let mut shift: u32 = 0;

        loop {
            let byte = self.read_u8()?;
            result |= ((byte & 0x7F) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "varint overflow",
                ));
            }
        }
    }

    /// Read a LEB128-prefixed UTF-8 string. Invalid UTF-8 is rejected as
    /// `InvalidData`, matching the previous reader.
    #[inline]
    pub fn read_varint_string(&mut self) -> io::Result<String> {
        let len = self.read_varint()? as usize;
        let bytes = self.read_slice(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Append a LEB128-encoded unsigned integer to an in-memory buffer.
///
/// This is the varint writer for the encode/insert path: `native::encode` builds
/// Native block bytes into a `Vec<u8>`, and every length, count, and framing word
/// on the wire is a LEB128 varint (server `writeVarUInt`, `src/IO/VarInt.h`). It
/// is the exact inverse of [`ByteReader::read_varint`]. The in-crate test builders
/// that synthesize wire bytes use it too. Appending to a `Vec` is infallible, so
/// this returns nothing and cannot error.
pub(crate) fn write_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            return;
        }
    }
}

/// Scan past one LEB128-encoded unsigned integer in `bytes` starting at `pos`,
/// returning the index just past its final byte.
///
/// This is the boundary-only twin of [`ByteReader::read_varint`]: the body is
/// `read_varint` with the `result |= ...` accumulation removed, since a boundary
/// walk discards the decoded value. It also operates on a borrowed slice with a
/// caller-held `pos` so the hot per-row loop keeps its cursor in a local index,
/// with no per-byte `ByteReader` field load/store and no value accumulation. That
/// restructuring is where the win is: dropping the fully-accumulate-then-discard
/// and the cursor-through-a-struct-field lifted `decode_aggregate_states` from
/// ~2.0 ns/row to ~1.2 ns/row on 1M mixed 1-3 byte states. A 10-byte-window fast
/// path that bounds-checks once was measured too and made no difference on the
/// real decode path, so it was left out in favor of this obvious equivalence to
/// `read_varint`.
///
/// The rejection contract is byte-for-byte identical to `read_varint` because the
/// byte reads, the `shift += 7`, and the `shift >= 64` overflow check are the
/// same operations in the same order:
///
/// - Running off the end (a continuation byte with no follow-up, or an empty
///   range) returns [`io::ErrorKind::UnexpectedEof`] with the same message, so
///   the streaming decoder still reads it as "need more bytes".
/// - A shift of 64 bits or more (a 10th continuation byte) returns
///   [`io::ErrorKind::InvalidData`] "varint overflow", at the exact same byte
///   `read_varint` rejects.
#[inline]
pub(crate) fn skip_varint(bytes: &[u8], pos: usize) -> io::Result<usize> {
    let mut i = pos;
    let mut shift: u32 = 0;
    loop {
        let byte = *bytes.get(i).ok_or_else(unexpected_eof)?;
        i += 1;
        if byte & 0x80 == 0 {
            return Ok(i);
        }
        shift += 7;
        if shift >= 64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "varint overflow",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(value: u64) {
        let mut buf = Vec::new();
        write_varint(&mut buf, value);
        let decoded = ByteReader::new(&buf).read_varint().unwrap();
        assert_eq!(decoded, value, "roundtrip failed for {value}");
    }

    #[test]
    fn test_varint_roundtrips() {
        roundtrip(0);
        roundtrip(1);
        roundtrip(127);
        roundtrip(128);
        roundtrip(300);
        roundtrip(16384);
        roundtrip(u64::MAX);
    }

    #[test]
    fn test_varint_known_encodings() {
        // 0 encodes as [0x00]
        assert_eq!(ByteReader::new(&[0x00u8]).read_varint().unwrap(), 0);
        // 1 encodes as [0x01]
        assert_eq!(ByteReader::new(&[0x01u8]).read_varint().unwrap(), 1);
        // 300 = 0b100101100 -> [0xAC, 0x02]
        assert_eq!(ByteReader::new(&[0xAC, 0x02]).read_varint().unwrap(), 300);
    }

    #[test]
    fn test_varint_overflow_rejected() {
        // Ten continuation bytes overflow the 64-bit accumulator.
        let bytes = [0xFFu8; 10];
        let err = ByteReader::new(&bytes).read_varint().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_varint_truncated_is_eof() {
        // A continuation byte with no follow-up must report EOF, not InvalidData,
        // so the streaming decoder treats it as "need more bytes".
        let bytes = [0x80u8];
        let err = ByteReader::new(&bytes).read_varint().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn test_varint_string() {
        // Encode: length=5 then "hello"
        let data = {
            let mut buf = Vec::new();
            write_varint(&mut buf, 5);
            buf.extend_from_slice(b"hello");
            buf
        };
        assert_eq!(
            ByteReader::new(&data).read_varint_string().unwrap(),
            "hello"
        );
    }

    #[test]
    fn test_varint_empty_string() {
        let data = [0x00u8]; // length=0
        assert_eq!(ByteReader::new(&data).read_varint_string().unwrap(), "");
    }

    #[test]
    fn test_varint_string_invalid_utf8_rejected() {
        // length=2 then bytes that are not valid UTF-8.
        let data = [0x02u8, 0xFF, 0xFE];
        let err = ByteReader::new(&data).read_varint_string().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_capacity_for_caps_at_producible_items() {
        // 80 bytes remaining, each item at least 8 bytes: at most 10 items, so a
        // hostile count of 60 is trimmed to 10.
        let data = [0u8; 80];
        assert_eq!(ByteReader::new(&data).capacity_for(60, 8), 10);

        // No trim when the input could hold the whole count: 80 bytes remaining
        // holds 10 eight-byte items, so a count of 10 returns exactly 10, and any
        // smaller count is returned unchanged.
        assert_eq!(ByteReader::new(&data).capacity_for(10, 8), 10);
        assert_eq!(ByteReader::new(&data).capacity_for(3, 8), 3);

        // A partial cursor advance shrinks the producible bound: after consuming
        // 8 bytes, 72 remain -> 9 eight-byte items.
        let mut r = ByteReader::new(&data);
        r.skip(8).unwrap();
        assert_eq!(r.capacity_for(60, 8), 9);
    }

    #[test]
    fn test_capacity_for_edge_cases() {
        let data = [0u8; 80];
        // A zero count reserves nothing regardless of remaining bytes.
        assert_eq!(ByteReader::new(&data).capacity_for(0, 8), 0);

        // No remaining bytes can produce no items.
        assert_eq!(ByteReader::new(&[]).capacity_for(60, 8), 0);

        // `min_item_bytes` of 0 is clamped to 1 so the division cannot panic; the
        // cap then falls back to one byte per item (remaining bytes).
        assert_eq!(ByteReader::new(&data).capacity_for(60, 0), 60);
        assert_eq!(ByteReader::new(&data).capacity_for(200, 0), 80);
    }

    #[test]
    fn test_read_slice_borrows_and_advances() {
        let data = [10u8, 20, 30, 40];
        let mut r = ByteReader::new(&data);
        assert_eq!(r.read_slice(2).unwrap(), &[10, 20]);
        assert_eq!(r.position(), 2);
        assert_eq!(r.remaining(), 2);
        assert_eq!(r.read_slice(2).unwrap(), &[30, 40]);
        assert_eq!(
            r.read_slice(1).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn test_skip_varint_matches_read_varint_on_valid_encodings() {
        // Boundary and mixed-width values, including the 10-byte u64::MAX
        // encoding. `skip_varint` must report the same end the reader consumes to,
        // and must work from a nonzero start offset with trailing bytes present.
        for value in [
            0u64,
            1,
            13,
            79,
            127,
            128,
            300,
            16383,
            16384,
            20000,
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            write_varint(&mut buf, value);

            let mut reader = ByteReader::new(&buf);
            assert_eq!(reader.read_varint().unwrap(), value);
            assert_eq!(reader.position(), buf.len());

            // Trailing bytes are left untouched: the end is the varint's length.
            let mut with_tail = buf.clone();
            with_tail.extend_from_slice(&[0x13, 0x4f]);
            assert_eq!(skip_varint(&with_tail, 0).unwrap(), buf.len());

            // Starting mid-buffer scans from `pos` and returns an absolute index.
            let mut prefixed = vec![0xaa, 0xbb, 0xcc];
            let offset = prefixed.len();
            prefixed.extend_from_slice(&buf);
            assert_eq!(skip_varint(&prefixed, offset).unwrap(), offset + buf.len());
        }
    }

    #[test]
    fn test_skip_varint_boundary_lengths() {
        // 127 is the largest 1-byte varint, 128 the smallest 2-byte, and
        // 16383/16384 straddle the 2/3-byte boundary.
        for (value, len) in [(127u64, 1usize), (128, 2), (16383, 2), (16384, 3)] {
            let mut buf = Vec::new();
            write_varint(&mut buf, value);
            assert_eq!(buf.len(), len);
            assert_eq!(skip_varint(&buf, 0).unwrap(), len);
        }
    }

    #[test]
    fn test_skip_varint_truncated_is_eof() {
        // A continuation byte with no follow-up: EOF, matching read_varint, so
        // the streaming decoder treats it as "need more bytes".
        let bytes = [0x80u8];
        assert_eq!(
            ByteReader::new(&bytes).read_varint().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            skip_varint(&bytes, 0).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn test_skip_varint_empty_or_past_end_is_eof() {
        // An empty range and a `pos` past the end both report EOF, never a panic.
        assert_eq!(
            skip_varint(&[], 0).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            skip_varint(&[0x13u8], 5).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn test_skip_varint_overlong_rejected_like_read_varint() {
        // Ten continuation bytes overflow the 64-bit shift, rejected as
        // InvalidData at the same byte read_varint rejects.
        let ten = [0xFFu8; 10];
        assert_eq!(
            ByteReader::new(&ten).read_varint().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            skip_varint(&ten, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        // An 11-byte all-continuation varint is rejected at the same point and
        // never scanned to the end; both readers agree on the error kind.
        let eleven = [0xFFu8; 11];
        assert_eq!(
            skip_varint(&eleven, 0).unwrap_err().kind(),
            ByteReader::new(&eleven).read_varint().unwrap_err().kind()
        );
        assert_eq!(
            skip_varint(&eleven, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
