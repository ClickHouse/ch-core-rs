//! Minimal JSON for the descriptors that cross the C ABI.
//!
//! Results leave the module as a JSON descriptor whose buffers are references
//! into linear memory, and encode input arrives the same way. The descriptor
//! carries per-column metadata only, never per-row data, so a small hand-written
//! writer and parser are enough and keep the crate free of dependencies.

use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Streaming JSON writer. Callers are responsible for balanced
/// `begin_*`/`end_*` calls and for writing a key before every object value.
#[derive(Default)]
pub(crate) struct Writer {
    out: String,
    /// True when the next key or value needs a leading comma.
    comma: bool,
}

impl Writer {
    pub(crate) fn finish(self) -> String {
        self.out
    }

    fn sep(&mut self) {
        if self.comma {
            self.out.push(',');
        }
    }

    pub(crate) fn begin_object(&mut self) {
        self.sep();
        self.out.push('{');
        self.comma = false;
    }

    pub(crate) fn end_object(&mut self) {
        self.out.push('}');
        self.comma = true;
    }

    pub(crate) fn begin_array(&mut self) {
        self.sep();
        self.out.push('[');
        self.comma = false;
    }

    pub(crate) fn end_array(&mut self) {
        self.out.push(']');
        self.comma = true;
    }

    pub(crate) fn key(&mut self, key: &str) {
        self.sep();
        write_escaped(&mut self.out, key);
        self.out.push(':');
        self.comma = false;
    }

    pub(crate) fn string(&mut self, value: &str) {
        self.sep();
        write_escaped(&mut self.out, value);
        self.comma = true;
    }

    /// Unsigned integer. Every value written here is a length, a count, a
    /// pointer or a small type parameter, all far below 2^53, so JS reads them
    /// exactly.
    pub(crate) fn uint(&mut self, value: u64) {
        self.sep();
        let _ = write!(self.out, "{value}");
        self.comma = true;
    }

    pub(crate) fn int(&mut self, value: i64) {
        self.sep();
        let _ = write!(self.out, "{value}");
        self.comma = true;
    }

    pub(crate) fn bool(&mut self, value: bool) {
        self.sep();
        self.out.push_str(if value { "true" } else { "false" });
        self.comma = true;
    }

    pub(crate) fn null(&mut self) {
        self.sep();
        self.out.push_str("null");
        self.comma = true;
    }

    pub(crate) fn field_str(&mut self, key: &str, value: &str) {
        self.key(key);
        self.string(value);
    }

    pub(crate) fn field_uint(&mut self, key: &str, value: u64) {
        self.key(key);
        self.uint(value);
    }
}

fn write_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parsed JSON value. Objects keep their keys in order as a list, which is
/// enough for the few keys a column descriptor has.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Look up `key` on an object. Missing keys, `null` values and non-objects
    /// all read as `None`.
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(fields) => fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v)
                .filter(|v| !matches!(v, Value::Null)),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// A non-negative integer that fits in `usize`.
    pub(crate) fn as_usize(&self) -> Option<usize> {
        match self {
            // Below 2^53 every integer is exact, so the cast loses nothing.
            Value::Number(n) if *n >= 0.0 && n.fract() == 0.0 && *n < 9_007_199_254_740_992.0 => {
                usize::try_from(*n as u64).ok()
            }
            _ => None,
        }
    }
}

/// Nesting limit. Column trees are a handful of levels deep; the limit keeps
/// a hostile descriptor from exhausting the stack.
const MAX_DEPTH: usize = 128;

/// Parse one JSON document. Trailing non-whitespace is an error.
pub(crate) fn parse(bytes: &[u8]) -> Result<Value, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "descriptor is not valid UTF-8")?;
    let mut p = Parser {
        s: text.as_bytes(),
        pos: 0,
    };
    let v = p.value(0)?;
    p.ws();
    if p.pos != p.s.len() {
        return Err(p.err("trailing characters"));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn err(&self, msg: &str) -> String {
        format!("invalid descriptor JSON at byte {}: {msg}", self.pos)
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.s.get(self.pos) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, lit: &[u8]) -> bool {
        if self.s[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        self.ws();
        match self.s.get(self.pos) {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Value::String),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) if self.eat(b"null") => Ok(Value::Null),
            Some(_) if self.eat(b"true") => Ok(Value::Bool(true)),
            Some(_) if self.eat(b"false") => Ok(Value::Bool(false)),
            Some(_) => Err(self.err("unexpected character")),
            None => Err(self.err("unexpected end")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, String> {
        self.pos += 1;
        let mut fields = Vec::new();
        self.ws();
        if self.eat(b"}") {
            return Ok(Value::Object(fields));
        }
        loop {
            self.ws();
            if self.s.get(self.pos) != Some(&b'"') {
                return Err(self.err("expected a key"));
            }
            let key = self.string()?;
            self.ws();
            if !self.eat(b":") {
                return Err(self.err("expected ':'"));
            }
            let value = self.value(depth + 1)?;
            fields.push((key, value));
            self.ws();
            if self.eat(b",") {
                continue;
            }
            if self.eat(b"}") {
                return Ok(Value::Object(fields));
            }
            return Err(self.err("expected ',' or '}'"));
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, String> {
        self.pos += 1;
        let mut items = Vec::new();
        self.ws();
        if self.eat(b"]") {
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.ws();
            if self.eat(b",") {
                continue;
            }
            if self.eat(b"]") {
                return Ok(Value::Array(items));
            }
            return Err(self.err("expected ',' or ']'"));
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        while let Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') = self.s.get(self.pos) {
            self.pos += 1;
        }
        // The slice is ASCII by construction, so it is valid UTF-8.
        std::str::from_utf8(&self.s[start..self.pos])
            .ok()
            .and_then(|t| t.parse::<f64>().ok())
            .filter(|n| n.is_finite())
            .map(Value::Number)
            .ok_or_else(|| self.err("invalid number"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .s
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| self.err("truncated \\u escape"))?;
        let mut v = 0u32;
        for &d in digits {
            let n = (d as char)
                .to_digit(16)
                .ok_or_else(|| self.err("invalid \\u escape"))?;
            v = v * 16 + n;
        }
        self.pos += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(&b) = self.s.get(self.pos) {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // `start..pos` stops before an ASCII byte or the end, so it never
            // splits a UTF-8 sequence of the already-validated input.
            out.push_str(std::str::from_utf8(&self.s[start..self.pos]).unwrap_or_default());
            match self.s.get(self.pos) {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let esc = *self
                        .s
                        .get(self.pos)
                        .ok_or_else(|| self.err("truncated escape"))?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&hi) {
                                if !self.eat(b"\\u") {
                                    return Err(self.err("unpaired surrogate"));
                                }
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(self.err("unpaired surrogate"));
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                hi
                            };
                            out.push(
                                char::from_u32(code)
                                    .ok_or_else(|| self.err("invalid code point"))?,
                            );
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                Some(_) => return Err(self.err("control character in string")),
                None => return Err(self.err("unterminated string")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_escapes_and_separates() {
        let mut w = Writer::default();
        w.begin_object();
        w.field_str("name", "a\"b\\c\n\u{1}é");
        w.key("list");
        w.begin_array();
        w.uint(13);
        w.int(-79);
        w.bool(true);
        w.null();
        w.begin_object();
        w.end_object();
        w.end_array();
        w.end_object();
        let out = w.finish();
        assert_eq!(
            out,
            r#"{"name":"a\"b\\c\n\u0001é","list":[13,-79,true,null,{}]}"#
        );
        let parsed = parse(out.as_bytes()).unwrap();
        assert_eq!(
            parsed.get("name").and_then(Value::as_str),
            Some("a\"b\\c\n\u{1}é")
        );
    }

    #[test]
    fn parser_reads_nested_values_and_escapes() {
        let v = parse(
            br#" {"a": [1, 2.5, -3e2], "s": "\u00e9\ud83d\ude00\/", "n": null, "t": false} "#,
        )
        .unwrap();
        assert_eq!(
            v.get("a").and_then(Value::as_array).map(|a| a.len()),
            Some(3)
        );
        assert_eq!(v.get("s").and_then(Value::as_str), Some("é😀/"));
        assert_eq!(v.get("n"), None);
        assert_eq!(v.get("t"), Some(&Value::Bool(false)));
        assert_eq!(v.get("missing"), None);
    }

    #[test]
    fn parser_rejects_malformed_input_without_panicking() {
        let bad: &[&[u8]] = &[
            b"",
            b"{",
            b"[1,",
            b"{\"a\" 1}",
            b"{\"a\":1,}",
            b"\"unterminated",
            b"\"bad \\x escape\"",
            b"\"\\ud83d\"",
            b"\"\\u12\"",
            b"1 2",
            b"nul",
            b"--1",
            b"\xff",
            b"\"tab\tinside\"",
        ];
        for input in bad {
            assert!(
                parse(input).is_err(),
                "{:?}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn parser_limits_depth() {
        let deep = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
        assert!(parse(deep.as_bytes()).is_err());
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(parse(ok.as_bytes()).is_ok());
    }

    #[test]
    fn as_usize_rejects_fractions_and_negatives() {
        assert_eq!(Value::Number(13.0).as_usize(), Some(13));
        assert_eq!(Value::Number(-1.0).as_usize(), None);
        assert_eq!(Value::Number(1.5).as_usize(), None);
        assert_eq!(Value::String("13".into()).as_usize(), None);
        assert_eq!(Value::Number(2f64.powi(64)).as_usize(), None);
        assert_eq!(Value::Number(2f64.powi(53)).as_usize(), None);
    }
}
