//! Esportazione HAR 1.2 (<http://www.softwareishard.com/blog/har-12-spec/>).
//!
//! Una voce per richiesta HTTP, in ordine di inizio. Scelte:
//! - `startedDateTime`: `epoch_us` più il tempo del guest, in UTC con i
//!   millisecondi (con `epoch_us = 0` l'accensione è il 1970-01-01);
//! - `timings` dall'ispettore (millisecondi con tre decimali, cioè i
//!   microsecondi del guest), `-1` per le fasi assenti, `ssl` sempre `-1`;
//!   `time` è la loro somma;
//! - `content.text` è il corpo dopo `Content-Encoding`; se non è UTF-8
//!   va in base64 con `encoding: "base64"`. Per il corpo della richiesta
//!   (dove HAR 1.2 non prevede `encoding`) si usa il campo personalizzato
//!   `_encoding` (i campi che cominciano con `_` sono ammessi dalla
//!   specifica);
//! - la resa dei decodificatori (JSON, form, multipart, protobuf) va nel
//!   `comment` di `postData` e `content`;
//! - senza risposta nella cattura: `status: 0` e un commento;
//! - `connection` è l'indice del flusso TCP, `serverIPAddress` l'indirizzo
//!   visto dal guest.

use std::fmt::Write as _;

use super::body::{self, Decoded};
use super::http::Headers;
use super::inspector::{HttpExchange, NetworkAnalysis};
use super::json::quote;

/// Opzioni dell'esportazione.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HarOptions {
    /// Microsecondi Unix corrispondenti al tempo 0 del guest.
    pub epoch_us: u64,
}

/// Data e ora UTC in ISO 8601 con i millisecondi.
pub fn iso8601(unix_us: u64) -> String {
    let secs = unix_us / 1_000_000;
    let ms = (unix_us % 1_000_000) / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Da giorni dall'epoca a data civile (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Millisecondi con tre decimali da microsecondi.
fn ms(us: u64) -> String {
    format!("{}.{:03}", us / 1000, us % 1000)
}

fn opt_ms(us: Option<u64>) -> String {
    us.map_or_else(|| "-1".to_string(), ms)
}

pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = u32::from(c[0]) << 16
            | u32::from(*c.get(1).unwrap_or(&0)) << 8
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn name_values(pairs: impl Iterator<Item = (String, String)>) -> String {
    let v: Vec<String> =
        pairs.map(|(n, v)| format!("{{\"name\":{},\"value\":{}}}", quote(&n), quote(&v))).collect();
    format!("[{}]", v.join(","))
}

fn headers(h: &Headers) -> String {
    name_values(h.0.iter().cloned())
}

fn request_cookies(h: &Headers) -> String {
    name_values(h.get_all("cookie").flat_map(|v| v.split(';')).filter_map(|c| {
        let (n, v) = c.split_once('=')?;
        Some((n.trim().to_string(), v.trim().to_string()))
    }))
}

fn response_cookies(h: &Headers) -> String {
    let v: Vec<String> = h
        .get_all("set-cookie")
        .filter_map(|c| {
            let mut parts = c.split(';');
            let (n, v) = parts.next()?.split_once('=')?;
            let mut s = format!("{{\"name\":{},\"value\":{}", quote(n.trim()), quote(v.trim()));
            for a in parts {
                let (k, val) = a.split_once('=').map_or((a.trim(), ""), |(k, v)| (k.trim(), v.trim()));
                match k.to_ascii_lowercase().as_str() {
                    "path" => write!(s, ",\"path\":{}", quote(val)).unwrap_or(()),
                    "domain" => write!(s, ",\"domain\":{}", quote(val)).unwrap_or(()),
                    "httponly" => s.push_str(",\"httpOnly\":true"),
                    "secure" => s.push_str(",\"secure\":true"),
                    _ => {}
                }
            }
            s.push('}');
            Some(s)
        })
        .collect();
    format!("[{}]", v.join(","))
}

fn query_string(url: &str) -> String {
    let q = url.split_once('?').map_or("", |(_, q)| q);
    let q = q.split('#').next().unwrap_or("");
    name_values(body::form(q).into_iter())
}

/// Commento con la resa del decodificatore, per i tipi strutturati.
fn decoded_comment(d: &Decoded) -> Option<String> {
    match d {
        Decoded::Json(_) | Decoded::Form(_) | Decoded::Multipart(_) | Decoded::Protobuf(_) => {
            Some(format!("vetro: corpo decodificato ({})\n{}", d.kind(), d.to_text()))
        }
        Decoded::Binary { note: Some(_), .. } => Some(format!("vetro: {}", d.to_text())),
        _ => None,
    }
}

fn post_data(x: &HttpExchange) -> Option<String> {
    let body = &x.request.body;
    if body.decoded.is_empty() {
        return None;
    }
    let mime = x.request.headers.get("content-type").unwrap_or("application/octet-stream");
    let mut s = format!("{{\"mimeType\":{}", quote(mime));
    match std::str::from_utf8(&body.decoded) {
        Ok(t) => write!(s, ",\"text\":{}", quote(t)).unwrap_or(()),
        Err(_) => {
            write!(s, ",\"text\":{},\"_encoding\":\"base64\"", quote(&base64(&body.decoded))).unwrap_or(())
        }
    }
    let params: Option<Vec<String>> = match &x.request_body {
        Decoded::Form(p) => {
            Some(p.iter().map(|(n, v)| format!("{{\"name\":{},\"value\":{}}}", quote(n), quote(v))).collect())
        }
        Decoded::Multipart(parts) => Some(
            parts
                .iter()
                .map(|p| {
                    let mut e = format!("{{\"name\":{}", quote(p.name.as_deref().unwrap_or("")));
                    if let Some(f) = &p.filename {
                        write!(e, ",\"fileName\":{}", quote(f)).unwrap_or(());
                    } else if let Ok(t) = std::str::from_utf8(&p.data) {
                        write!(e, ",\"value\":{}", quote(t)).unwrap_or(());
                    }
                    if let Some(c) = &p.content_type {
                        write!(e, ",\"contentType\":{}", quote(c)).unwrap_or(());
                    }
                    e.push('}');
                    e
                })
                .collect(),
        ),
        _ => None,
    };
    if let Some(p) = params {
        write!(s, ",\"params\":[{}]", p.join(",")).unwrap_or(());
    }
    if let Some(c) = decoded_comment(&x.request_body) {
        write!(s, ",\"comment\":{}", quote(&c)).unwrap_or(());
    }
    s.push('}');
    Some(s)
}

fn entry(x: &HttpExchange, opts: &HarOptions) -> String {
    let t = &x.timings;
    let req = &x.request;
    let mut s = String::new();
    write!(
        s,
        "{{\"startedDateTime\":{},\"time\":{},\"request\":{{\"method\":{},\"url\":{},\"httpVersion\":{},\"cookies\":{},\"headers\":{},\"queryString\":{}",
        quote(&iso8601(opts.epoch_us.saturating_add(t.started_us))),
        ms(t.total_us()),
        quote(&req.method),
        quote(&x.url),
        quote(if req.version.is_empty() { "HTTP/1.1" } else { &req.version }),
        request_cookies(&req.headers),
        headers(&req.headers),
        query_string(&x.url),
    )
    .unwrap_or(());
    if let Some(p) = post_data(x) {
        write!(s, ",\"postData\":{p}").unwrap_or(());
    }
    write!(s, ",\"headersSize\":{},\"bodySize\":{}}}", req.head_end - req.start, req.body.wire_len)
        .unwrap_or(());
    match &x.response {
        Some(r) => {
            let mime = r.headers.get("content-type").unwrap_or("x-unknown");
            write!(
                s,
                ",\"response\":{{\"status\":{},\"statusText\":{},\"httpVersion\":{},\"cookies\":{},\"headers\":{},\"content\":{{\"size\":{}",
                r.status,
                quote(&r.reason),
                quote(&r.version),
                response_cookies(&r.headers),
                headers(&r.headers),
                r.body.decoded.len()
            )
            .unwrap_or(());
            if r.body.content_encoding.is_some() {
                write!(s, ",\"compression\":{}", r.body.decoded.len() as i64 - r.body.raw.len() as i64).unwrap_or(());
            }
            write!(s, ",\"mimeType\":{}", quote(mime)).unwrap_or(());
            if !r.body.decoded.is_empty() {
                match std::str::from_utf8(&r.body.decoded) {
                    Ok(txt) => write!(s, ",\"text\":{}", quote(txt)).unwrap_or(()),
                    Err(_) => write!(s, ",\"text\":{},\"encoding\":\"base64\"", quote(&base64(&r.body.decoded))).unwrap_or(()),
                }
            }
            let mut notes = Vec::new();
            if let Some(e) = &r.body.decode_error {
                notes.push(format!("vetro: corpo non decodificato ({e})"));
            }
            if let Some(c) = x.response_body.as_ref().and_then(decoded_comment) {
                notes.push(c);
            }
            if !r.complete {
                notes.push("vetro: risposta troncata nella cattura".into());
            }
            if !notes.is_empty() {
                write!(s, ",\"comment\":{}", quote(&notes.join("\n"))).unwrap_or(());
            }
            write!(
                s,
                "}},\"redirectURL\":{},\"headersSize\":{},\"bodySize\":{}}}",
                quote(r.headers.get("location").unwrap_or("")),
                r.head_end - r.start,
                r.body.wire_len
            )
            .unwrap_or(());
        }
        None => s.push_str(
            ",\"response\":{\"status\":0,\"statusText\":\"\",\"httpVersion\":\"\",\"cookies\":[],\"headers\":[],\"content\":{\"size\":0,\"mimeType\":\"x-unknown\"},\"redirectURL\":\"\",\"headersSize\":-1,\"bodySize\":-1,\"comment\":\"vetro: nessuna risposta nella cattura\"}",
        ),
    }
    write!(
        s,
        ",\"cache\":{{}},\"timings\":{{\"blocked\":{},\"dns\":{},\"connect\":{},\"send\":{},\"wait\":{},\"receive\":{},\"ssl\":-1}},\"serverIPAddress\":{},\"connection\":{}",
        opt_ms(t.blocked_us),
        opt_ms(t.dns_us),
        opt_ms(t.connect_us),
        ms(t.send_us),
        ms(t.wait_us),
        ms(t.receive_us),
        quote(&x.server.ip().to_string()),
        quote(&x.flow.to_string()),
    )
    .unwrap_or(());
    if !req.complete {
        s.push_str(",\"comment\":\"vetro: richiesta troncata nella cattura\"");
    }
    if x.secure {
        s.push_str(",\"_secure\":true");
        if let Some(a) = &x.attribution {
            write!(
                s,
                ",\"_vetro\":{{\"pid\":{},\"tid\":{},\"process\":{},\"package\":{},\"library\":{}}}",
                a.pid,
                a.tid,
                quote(&a.process),
                a.package.as_deref().map_or("null".into(), quote),
                quote(&a.library)
            )
            .unwrap_or(());
        }
    }
    s.push('}');
    s
}

/// Il documento HAR 1.2 dell'analisi (JSON, una voce per riga).
pub fn to_har(a: &NetworkAnalysis, opts: &HarOptions) -> String {
    let mut s = format!(
        "{{\"log\":{{\"version\":\"1.2\",\"creator\":{{\"name\":\"Vetro\",\"version\":{}}},\"entries\":[",
        quote(env!("CARGO_PKG_VERSION"))
    );
    for (i, x) in a.http.iter().enumerate() {
        s.push_str(if i == 0 { "\n" } else { ",\n" });
        s.push_str(&entry(x, opts));
    }
    s.push_str("\n]}}\n");
    s
}

impl NetworkAnalysis {
    /// L'HAR 1.2 della cattura (vedi [`to_har`]).
    pub fn to_har(&self, opts: &HarOptions) -> String {
        to_har(self, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::json::{self, Value};

    #[test]
    fn date_iso() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_500_000), "1970-01-01T00:00:01.500Z");
        // 2024-02-29T12:34:56.789Z (anno bisestile).
        assert_eq!(iso8601(1_709_210_096_789_000), "2024-02-29T12:34:56.789Z");
        assert_eq!(iso8601(4_102_444_800_000_000), "2100-01-01T00:00:00.000Z");
    }

    #[test]
    fn base64_rfc4648() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    fn num(v: &Value) -> f64 {
        match v {
            Value::Number(n) => n.parse().unwrap(),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn har_della_sessione() {
        let a = NetworkAnalysis::from_frames(&crate::net::inspector::tests::session());
        let har = a.to_har(&HarOptions::default());
        let v = json::parse(har.as_bytes()).expect("HAR è JSON valido");
        let log = v.get("log").unwrap();
        assert_eq!(log.get("version").and_then(Value::as_str), Some("1.2"));
        let Some(Value::Array(entries)) = log.get("entries") else { panic!() };
        assert_eq!(entries.len(), 2);
        let e = &entries[0];
        assert_eq!(e.get("startedDateTime").and_then(Value::as_str), Some("1970-01-01T00:00:00.001Z"));
        let t = e.get("timings").unwrap();
        let sum: f64 = ["blocked", "dns", "connect", "send", "wait", "receive"]
            .iter()
            .map(|k| num(t.get(k).unwrap()))
            .filter(|&x| x >= 0.0)
            .sum();
        assert_eq!(num(e.get("time").unwrap()), sum);
        assert_eq!(num(t.get("dns").unwrap()), 0.3);
        let req = e.get("request").unwrap();
        assert_eq!(
            req.get("url").and_then(Value::as_str),
            Some("http://api.example:8080/v1/items?id=7&q=a+b")
        );
        assert_eq!(
            req.get("queryString").unwrap().to_compact(),
            r#"[{"name":"id","value":"7"},{"name":"q","value":"a b"}]"#
        );
        assert_eq!(
            req.get("cookies").unwrap().to_compact(),
            r#"[{"name":"s","value":"1"},{"name":"t","value":"2"}]"#
        );
        let content = e.get("response").unwrap().get("content").unwrap();
        assert_eq!(content.get("text").and_then(Value::as_str), Some("{\"items\":[7]}"));
        assert!(content.get("comment").and_then(Value::as_str).unwrap().contains("(json)"));
        let post = entries[1].get("request").unwrap().get("postData").unwrap();
        assert_eq!(
            post.get("params").unwrap().to_compact(),
            r#"[{"name":"a","value":"1"},{"name":"b","value":"2"}]"#
        );
        assert_eq!(entries[1].get("timings").unwrap().get("dns").map(num), Some(-1.0));
        assert_eq!(entries[1].get("connection").and_then(Value::as_str), Some("0"));
    }
}
