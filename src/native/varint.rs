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

/// Write a LEB128-encoded unsigned integer to a writer.
pub fn write_varint<W: io::Write>(writer: &mut W, mut value: u64) -> io::Result<()> {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        writer.write_all(&[byte])?;
        if value == 0 {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(value: u64) {
        let mut buf = Vec::new();
        write_varint(&mut buf, value).unwrap();
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
            write_varint(&mut buf, 5).unwrap();
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
}
