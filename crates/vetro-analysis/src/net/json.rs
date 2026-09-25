//! JSON (RFC 8259): un parser che valida e conserva l'ordine delle chiavi
//! (per decodificare i corpi) e un piccolo scrittore (per l'HAR).

use std::fmt::Write as _;

/// Profondità massima accettata.
const MAX_DEPTH: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    /// Numero com'era scritto (nessuna perdita di precisione).
    Number(String),
    String(String),
    Array(Vec<Value>),
    /// Coppie in ordine; chiavi ripetute conservate.
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Campo `key` di un oggetto (il primo).
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

    /// Serializzazione compatta.
    pub fn to_compact(&self) -> String {
        let mut s = String::new();
        self.write(&mut s, None, 0);
        s
    }

    /// Serializzazione indentata di due spazi.
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

/// Errore di sintassi con la posizione in byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError {
    pub at: usize,
    pub msg: &'static str,
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON non valido a {}: {}", self.at, self.msg)
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
            self.err("letterale non valido")
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, JsonError> {
        if depth > MAX_DEPTH {
            return self.err("annidamento troppo profondo");
        }
        self.ws();
        match self.s.get(self.pos) {
            None => self.err("fine inattesa"),
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
                        return self.err("attesa una chiave");
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
            Some(_) => self.err("carattere inatteso"),
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
            _ => return self.err("numero non valido"),
        }
        if self.s.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            if self.digits() == 0 {
                return self.err("mancano le cifre decimali");
            }
        }
        if let Some(b'e' | b'E') = self.s.get(self.pos) {
            self.pos += 1;
            if let Some(b'+' | b'-') = self.s.get(self.pos) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return self.err("esponente senza cifre");
            }
        }
        Ok(Value::Number(String::from_utf8_lossy(&self.s[start..self.pos]).into_owned()))
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let h = self.s.get(self.pos..self.pos + 4).ok_or(JsonError { at: self.pos, msg: "\\u troncato" })?;
        let v = std::str::from_utf8(h)
            .ok()
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .ok_or(JsonError { at: self.pos, msg: "\\u non esadecimale" })?;
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
                Err(_) => return Err(JsonError { at: start, msg: "UTF-8 non valido" }),
            }
            match self.s.get(self.pos) {
                None => return self.err("stringa non chiusa"),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let e =
                        *self.s.get(self.pos).ok_or(JsonError { at: self.pos, msg: "escape troncato" })?;
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
                        _ => return self.err("escape non valido"),
                    }
                }
                Some(_) => return self.err("carattere di controllo nella stringa"),
            }
        }
    }
}

/// Decodifica un documento JSON completo (spazi attorno ammessi, BOM
/// UTF-8 tollerato).
pub fn parse(s: &[u8]) -> Result<Value, JsonError> {
    let s = s.strip_prefix(b"\xef\xbb\xbf").unwrap_or(s);
    let mut p = Parser { s, pos: 0 };
    let v = p.value(0)?;
    p.ws();
    if p.pos != s.len() {
        return p.err("dati dopo il documento");
    }
    Ok(v)
}

/// Aggiunge `s` fra virgolette con gli escape di JSON.
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
