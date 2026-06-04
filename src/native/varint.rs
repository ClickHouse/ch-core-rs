use std::io;

/// Read a LEB128-encoded unsigned integer from a reader.
pub fn read_varint<R: io::Read>(reader: &mut R) -> io::Result<u64> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    let mut buf = [0u8; 1];

    loop {
        reader.read_exact(&mut buf)?;
        let byte = buf[0];
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

/// Read a LEB128-prefixed string from a reader.
pub fn read_varint_string<R: io::Read>(reader: &mut R) -> io::Result<String> {
    let len = read_varint(reader)? as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
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
    use std::io::Cursor;

    fn roundtrip(value: u64) {
        let mut buf = Vec::new();
        write_varint(&mut buf, value).unwrap();
        let mut cursor = Cursor::new(&buf);
        let decoded = read_varint(&mut cursor).unwrap();
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
        let mut cursor = Cursor::new(&[0x00u8]);
        assert_eq!(read_varint(&mut cursor).unwrap(), 0);

        // 1 encodes as [0x01]
        let mut cursor = Cursor::new(&[0x01u8]);
        assert_eq!(read_varint(&mut cursor).unwrap(), 1);

        // 300 = 0b100101100 → [0xAC, 0x02]
        let mut cursor = Cursor::new(&[0xAC, 0x02]);
        assert_eq!(read_varint(&mut cursor).unwrap(), 300);
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
        let mut cursor = Cursor::new(&data);
        assert_eq!(read_varint_string(&mut cursor).unwrap(), "hello");
    }

    #[test]
    fn test_varint_empty_string() {
        let data = [0x00u8]; // length=0
        let mut cursor = Cursor::new(&data[..]);
        assert_eq!(read_varint_string(&mut cursor).unwrap(), "");
    }
}
