//! The input of an invocation as text, with its bytes that are not UTF-8.
//!
//! `clickhouse-client` sends the bytes of a statement as they are, and some
//! tests have bytes that are not UTF-8 in string literals, heredocs and quoted
//! names (`SELECT hex($$\xff\xfe$$)`). The splitter, the test hints and the
//! statement checks of the shim work on `str`. So each byte of an invalid
//! sequence becomes one character of a private-use range (byte `b` is
//! `U+10FF00 + b`), and the text that leaves the shim (the request body over
//! HTTP, `echo`, stderr) gets the original bytes back. These characters are
//! not ASCII, so the splitter, which stops only at ASCII bytes, sees them as
//! the server sees the bytes: inside the quotes that hold them.

use std::borrow::Cow;

/// Byte `b` of an invalid sequence is the character `RAW_FIRST + b`.
const RAW_FIRST: u32 = 0x10FF00;

fn raw_byte(c: char) -> Option<u8> {
    u8::try_from(u32::from(c).checked_sub(RAW_FIRST)?).ok()
}

/// Tells if the text has a byte that is not UTF-8.
pub(crate) fn has_raw_bytes(text: &str) -> bool {
    text.chars().any(|c| raw_byte(c).is_some())
}

/// The input as text. An input that already has a character of the range
/// cannot be read without loss: that is an error.
pub(crate) fn decode(bytes: Vec<u8>) -> Result<String, String> {
    let conflict = |s: &str| {
        if has_raw_bytes(s) {
            Err(format!(
                "the input has a character of U+{RAW_FIRST:X}..U+10FFFF, which the shim uses for \
                 bytes that are not UTF-8"
            ))
        } else {
            Ok(())
        }
    };
    let bytes = match String::from_utf8(bytes) {
        Ok(text) => {
            conflict(&text)?;
            return Ok(text);
        }
        Err(e) => e.into_bytes(),
    };
    let mut text = String::with_capacity(bytes.len() + 16);
    for chunk in bytes.utf8_chunks() {
        conflict(chunk.valid())?;
        text.push_str(chunk.valid());
        for &b in chunk.invalid() {
            text.push(char::from_u32(RAW_FIRST + u32::from(b)).expect("a scalar value"));
        }
    }
    Ok(text)
}

/// The bytes of the text, with the original bytes of the input.
pub(crate) fn encode(text: &str) -> Cow<'_, [u8]> {
    if !has_raw_bytes(text) {
        return Cow::Borrowed(text.as_bytes());
    }
    let mut out = Vec::with_capacity(text.len());
    let mut buf = [0_u8; 4];
    for c in text.chars() {
        match raw_byte(c) {
            Some(b) => out.push(b),
            None => out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::split::split_statements;

    #[test]
    fn utf8_input_is_unchanged() {
        let text = decode("SELECT '\u{e9}', '\u{10FEFF}';".as_bytes().to_vec()).unwrap();
        assert_eq!(text, "SELECT '\u{e9}', '\u{10FEFF}';");
        assert!(!has_raw_bytes(&text));
        assert!(matches!(encode(&text), Cow::Borrowed(_)));
    }

    #[test]
    fn bytes_that_are_not_utf8_round_trip() {
        // A lone continuation byte, a cut 2-byte sequence before ASCII, an
        // overlong form, a surrogate, a cut 4-byte sequence at the end, and
        // the bytes of 01957_heredoc_more.
        let input: &[u8] =
            b"SELECT '\x80', '\xC3(', '\xC0\xAF', '\xED\xA0\x80', hex($$\xff\xfe\x80\xbb$$), '\xF0\x9F\x98";
        let text = decode(input.to_vec()).unwrap();
        assert!(has_raw_bytes(&text));
        assert_eq!(encode(&text).as_ref(), input);
        // The text keeps the valid characters as they are.
        assert!(text.starts_with("SELECT '"));
        assert!(text.contains("', hex($$"));
    }

    #[test]
    fn statements_keep_their_bytes() {
        // 03356: a quoted name with bytes that are not UTF-8, and a `;` after
        // it. The split is the one of the bytes: the bytes are inside the
        // quotes.
        let input: &[u8] =
            b"CREATE TABLE t (`\xFF\0a;` UInt8) ENGINE = Memory; -- x\nSELECT hex('\xfe;') AS h;\n";
        let stmts = split_statements(&decode(input.to_vec()).unwrap());
        assert_eq!(stmts.len(), 2);
        assert_eq!(
            encode(&stmts[0].sql).as_ref(),
            b"CREATE TABLE t (`\xFF\0a;` UInt8) ENGINE = Memory;"
        );
        assert_eq!(
            encode(&stmts[0].full).as_ref(),
            b"CREATE TABLE t (`\xFF\0a;` UInt8) ENGINE = Memory; -- x"
        );
        assert_eq!(encode(&stmts[1].sql).as_ref(), b"SELECT hex('\xfe;') AS h;");
    }

    #[test]
    fn a_character_of_the_range_in_the_input_is_an_error() {
        let valid = "SELECT '\u{10FF41}'".as_bytes().to_vec();
        assert!(decode(valid).unwrap_err().contains("U+10FF00..U+10FFFF"));
        let mut mixed = b"SELECT '\xFF', '".to_vec();
        mixed.extend_from_slice("\u{10FFFF}'".as_bytes());
        assert!(decode(mixed).is_err());
    }
}
