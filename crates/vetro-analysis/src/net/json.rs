//! JSON (RFC 8259): a parser that validates and preserves key order
//! (to decode bodies) and a small writer (for the HAR).

use std::fmt::Write as _;

/// Maximum accepted depth.
const MAX_DEPTH: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    /// Number as it was written (no loss of precision).
    Number(String),
    String(String),
    Array(Vec<Value>),
    /// Pairs in order; repeated keys preserved.
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Field `key` of an object (the first one).
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(v) => v.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// Compact serialization.
    pub fn to_compact(&self) -> String {
        let mut s = String::new();
        self.write(&mut s, None, 0);
        s
    }

    /// Serialization indented by two spaces.
    pub fn to_pretty(&self) -> String {
        let mut s = String::new();
        self.write(&mut s, Some(2), 0);
        s
    }

    fn write(&self, out: &mut String, indent: Option<usize>, depth: usize) {
        let nl = |out: &mut String, d: usize| {
            if let Some(i) = indent {
                out.push('\n');
                out.extend(std::iter::repeat_n(' ', i * d));
            }
        };
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(n),
            Value::String(s) => quote_into(out, s),
            Value::Array(v) if v.is_empty() => out.push_str("[]"),
            Value::Object(v) if v.is_empty() => out.push_str("{}"),
            Value::Array(v) => {
                out.push('[');
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    nl(out, depth + 1);
                    x.write(out, indent, depth + 1);
                }
                nl(out, depth);
                out.push(']');
            }
            Value::Object(v) => {
                out.push('{');
                for (i, (k, x)) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    nl(out, depth + 1);
                    quote_into(out, k);
                    out.push(':');
                    if indent.is_some() {
                        out.push(' ');
                    }
                    x.write(out, indent, depth + 1);
                }
                nl(out, depth);
                out.push('}');
            }
        }
    }
}

/// Syntax error with the byte position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError {
    pub at: usize,
    pub msg: &'static str,
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid JSON at {}: {}", self.at, self.msg)
    }
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn err<T>(&self, msg: &'static str) -> Result<T, JsonError> {
        Err(JsonError { at: self.pos, msg })
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.s.get(self.pos) {
            self.pos += 1;
        }
    }

    fn lit(&mut self, word: &[u8], v: Value) -> Result<Value, JsonError> {
        if self.s[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(v)
        } else {
            self.err("invalid literal")
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, JsonError> {
        if depth > MAX_DEPTH {
            return self.err("nesting too deep");
        }
        self.ws();
        match self.s.get(self.pos) {
            None => self.err("unexpected end"),
            Some(b'n') => self.lit(b"null", Value::Null),
            Some(b't') => self.lit(b"true", Value::Bool(true)),
            Some(b'f') => self.lit(b"false", Value::Bool(false)),
            Some(b'"') => self.string().map(Value::String),
            Some(b'[') => {
                self.pos += 1;
                let mut v = Vec::new();
                self.ws();
                if self.s.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    return Ok(Value::Array(v));
                }
                loop {
                    v.push(self.value(depth + 1)?);
                    self.ws();
                    match self.s.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(Value::Array(v));
                        }
                        _ => return self.err("atteso ',' o ']'"),
                    }
                }
            }
            Some(b'{') => {
                self.pos += 1;
                let mut v = Vec::new();
                self.ws();
                if self.s.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                    return Ok(Value::Object(v));
                }
                loop {
                    self.ws();
                    if self.s.get(self.pos) != Some(&b'"') {
                        return self.err("expected a key");
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.s.get(self.pos) != Some(&b':') {
                        return self.err("atteso ':'");
                    }
                    self.pos += 1;
                    v.push((k, self.value(depth + 1)?));
                    self.ws();
                    match self.s.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Ok(Value::Object(v));
                        }
                        _ => return self.err("atteso ',' o '}'"),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => self.err("unexpected character"),
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while self.s.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<Value, JsonError> {
        let start = self.pos;
        if self.s[self.pos] == b'-' {
            self.pos += 1;
        }
        match self.s.get(self.pos) {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return self.err("invalid number"),
        }
        if self.s.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            if self.digits() == 0 {
                return self.err("missing decimal digits");
            }
        }
        if let Some(b'e' | b'E') = self.s.get(self.pos) {
            self.pos += 1;
            if let Some(b'+' | b'-') = self.s.get(self.pos) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return self.err("exponent without digits");
            }
        }
        Ok(Value::Number(String::from_utf8_lossy(&self.s[start..self.pos]).into_owned()))
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let h = self.s.get(self.pos..self.pos + 4).ok_or(JsonError { at: self.pos, msg: "\\u truncated" })?;
        let v = std::str::from_utf8(h)
            .ok()
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .ok_or(JsonError { at: self.pos, msg: "\\u not hexadecimal" })?;
        self.pos += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, JsonError> {
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
            match std::str::from_utf8(&self.s[start..self.pos]) {
                Ok(t) => out.push_str(t),
                Err(_) => return Err(JsonError { at: start, msg: "invalid UTF-8" }),
            }
            match self.s.get(self.pos) {
                None => return self.err("unterminated string"),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let e =
                        *self.s.get(self.pos).ok_or(JsonError { at: self.pos, msg: "truncated escape" })?;
                    self.pos += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut c = self.hex4()?;
                            if (0xd800..0xdc00).contains(&c) && self.s[self.pos..].starts_with(b"\\u") {
                                self.pos += 2;
                                let lo = self.hex4()?;
                                if (0xdc00..0xe000).contains(&lo) {
                                    c = 0x10000 + ((c - 0xd800) << 10) + (lo - 0xdc00);
                                } else {
                                    out.push('\u{fffd}');
                                    c = lo;
                                }
                            }
                            out.push(char::from_u32(c).unwrap_or('\u{fffd}'));
                        }
                        _ => return self.err("invalid escape"),
                    }
                }
                Some(_) => return self.err("control character in string"),
            }
        }
    }
}

/// Decodes a complete JSON document (surrounding whitespace allowed, UTF-8
/// BOM tolerated).
pub fn parse(s: &[u8]) -> Result<Value, JsonError> {
    let s = s.strip_prefix(b"\xef\xbb\xbf").unwrap_or(s);
    let mut p = Parser { s, pos: 0 };
    let v = p.value(0)?;
    p.ws();
    if p.pos != s.len() {
        return p.err("data after the document");
    }
    Ok(v)
}

/// Appends `s` in quotes with JSON escapes.
pub fn quote_into(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn quote(s: &str) -> String {
    let mut out = String::new();
    quote_into(&mut out, s);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documenti_validi() {
        let v = parse(br#" {"a": [1, -2.5e3, true, null], "b": {"c": "x\"\u00e8\ud83d\ude00"}, "a": 0} "#)
            .unwrap();
        assert_eq!(
            v.get("a"),
            Some(&Value::Array(vec![
                Value::Number("1".into()),
                Value::Number("-2.5e3".into()),
                Value::Bool(true),
                Value::Null
            ]))
        );
        assert_eq!(v.get("b").and_then(|b| b.get("c")).and_then(Value::as_str), Some("x\"è😀"));
        assert_eq!(v.to_compact(), r#"{"a":[1,-2.5e3,true,null],"b":{"c":"x\"è😀"},"a":0}"#);
        assert_eq!(parse(v.to_pretty().as_bytes()).unwrap(), v);
        assert!(v.to_pretty().contains("\n  \"b\": {\n    \"c\""));
        assert_eq!(parse(b"\xef\xbb\xbf[]").unwrap(), Value::Array(vec![]));
    }

    #[test]
    fn documenti_non_validi() {
        for bad in [
            &b""[..],
            b"{",
            b"[1,]",
            b"01",
            b"1.",
            b"-",
            b"\"a",
            b"\"\x01\"",
            b"{\"a\" 1}",
            b"tru",
            b"[] []",
            b"\"\\x\"",
            b"\"\xff\"",
            b"{1:2}",
        ] {
            assert!(parse(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        let deep = "[".repeat(10_000);
        assert!(parse(deep.as_bytes()).is_err());
    }

    #[test]
    fn virgolette() {
        assert_eq!(quote("a\"b\\c\n\u{1}"), r#""a\"b\\c\n\u0001""#);
    }
}
