//! L'ispettore di rete in JSON, per l'app web (M7, ADR 0023): la lista
//! delle richieste ([`requests_json`]) e il dettaglio di una richiesta
//! ([`exchange_json`]) con intestazioni e corpi decodificati. Il formato è
//! in `docs/specs/analysis.md`; i campi sono in camelCase come le API del
//! JS che li legge.
//!
//! Deterministico (stesso ingresso, stessi byte) e senza dipendenze, come
//! il resto di `net`.

use std::fmt::Write as _;

use super::body::Decoded;
use super::dns;
use super::har::base64;
use super::http::{Body, Headers};
use super::inspector::{HttpExchange, NetworkAnalysis, Timings};
use super::json::quote_into;

/// Byte del corpo decodificato messi in `base64` nel dettaglio (oltre, il
/// dettaglio dice `truncated`).
pub const BODY_BASE64_LIMIT: usize = 256 * 1024;

/// Caratteri della resa testuale di un corpo nel dettaglio.
pub const BODY_TEXT_LIMIT: usize = 512 * 1024;

/// Host e percorso (con la query) di un URL `schema://host/percorso`.
pub fn split_url(url: &str) -> (&str, &str) {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    }
}

fn q(out: &mut String, s: &str) {
    quote_into(out, s);
}

fn opt_str(out: &mut String, s: Option<&str>) {
    match s {
        Some(s) => q(out, s),
        None => out.push_str("null"),
    }
}

fn opt_u64(out: &mut String, v: Option<u64>) {
    match v {
        Some(v) => {
            let _ = write!(out, "{v}");
        }
        None => out.push_str("null"),
    }
}

fn timings(out: &mut String, t: &Timings) {
    let _ = write!(out, "{{\"startedUs\":{},\"blockedUs\":", t.started_us);
    opt_u64(out, t.blocked_us);
    out.push_str(",\"dnsUs\":");
    opt_u64(out, t.dns_us);
    out.push_str(",\"connectUs\":");
    opt_u64(out, t.connect_us);
    let _ = write!(
        out,
        ",\"sendUs\":{},\"waitUs\":{},\"receiveUs\":{},\"totalUs\":{}}}",
        t.send_us,
        t.wait_us,
        t.receive_us,
        t.total_us()
    );
}

fn headers(out: &mut String, h: &Headers) {
    out.push('[');
    for (i, (k, v)) in h.0.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        q(out, k);
        out.push(',');
        q(out, v);
        out.push(']');
    }
    out.push(']');
}

/// Taglia `s` a `max` byte su un confine di carattere.
fn cut(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

/// Il corpo decodificato: resa testuale e, secondo il tipo, la struttura.
fn decoded(out: &mut String, d: &Decoded) {
    out.push_str("\"kind\":");
    q(out, d.kind());
    let text = d.to_text();
    let (t, cut_text) = cut(&text, BODY_TEXT_LIMIT);
    out.push_str(",\"text\":");
    q(out, t);
    let _ = write!(out, ",\"textTruncated\":{cut_text}");
    match d {
        Decoded::Json(v) => {
            let (j, cut_json) = cut_json(v);
            if !cut_json {
                out.push_str(",\"json\":");
                out.push_str(&j);
            }
        }
        Decoded::Form(fields) => {
            out.push_str(",\"fields\":[");
            for (i, (k, v)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('[');
                q(out, k);
                out.push(',');
                q(out, v);
                out.push(']');
            }
            out.push(']');
        }
        Decoded::Multipart(parts) => {
            out.push_str(",\"parts\":[");
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str("{\"name\":");
                opt_str(out, p.name.as_deref());
                out.push_str(",\"filename\":");
                opt_str(out, p.filename.as_deref());
                out.push_str(",\"contentType\":");
                opt_str(out, p.content_type.as_deref());
                let _ = write!(out, ",\"size\":{},", p.data.len());
                decoded(out, &p.decoded);
                out.push('}');
            }
            out.push(']');
        }
        Decoded::Binary { note, .. } => {
            out.push_str(",\"note\":");
            opt_str(out, note.as_deref());
        }
        Decoded::Empty | Decoded::Protobuf(_) | Decoded::Text(_) => {}
    }
}

/// JSON compatto di un valore, e se supera il limite del testo.
fn cut_json(v: &super::json::Value) -> (String, bool) {
    let s = v.to_compact();
    let long = s.len() > BODY_TEXT_LIMIT;
    (s, long)
}

fn body_json(out: &mut String, b: &Body, content_type: Option<&str>, d: &Decoded) {
    let _ = write!(
        out,
        "{{\"wire\":{},\"raw\":{},\"size\":{},\"encoding\":",
        b.wire_len,
        b.raw.len(),
        b.decoded.len()
    );
    opt_str(out, b.content_encoding.as_deref());
    out.push_str(",\"decodeError\":");
    opt_str(out, b.decode_error.as_deref());
    let _ = write!(out, ",\"chunked\":{},\"contentType\":", b.chunked);
    opt_str(out, content_type);
    out.push(',');
    decoded(out, d);
    let n = b.decoded.len().min(BODY_BASE64_LIMIT);
    out.push_str(",\"base64\":");
    q(out, &base64(&b.decoded[..n]));
    let _ = write!(out, ",\"truncated\":{}}}", n < b.decoded.len());
}

/// Una riga della lista (anche dentro il dettaglio).
fn row(out: &mut String, x: &HttpExchange) {
    let (host, path) = split_url(&x.url);
    let _ = write!(out, "{{\"i\":{},\"flow\":{},\"method\":", x.index, x.flow);
    q(out, &x.request.method);
    out.push_str(",\"url\":");
    q(out, &x.url);
    out.push_str(",\"host\":");
    q(out, host);
    out.push_str(",\"path\":");
    q(out, path);
    out.push_str(",\"status\":");
    opt_u64(out, x.status().map(u64::from));
    out.push_str(",\"reason\":");
    opt_str(out, x.response.as_ref().map(|r| r.reason.as_str()));
    out.push_str(",\"mime\":");
    opt_str(out, x.mime());
    let (resp_size, resp_wire) =
        x.response.as_ref().map_or((0, 0), |r| (r.body.decoded.len(), r.body.wire_len));
    let _ = write!(
        out,
        ",\"reqBytes\":{},\"respBytes\":{},\"reqWire\":{},\"respWire\":{},\"reqKind\":",
        x.request.body.decoded.len(),
        resp_size,
        x.request.body.wire_len,
        resp_wire
    );
    q(out, x.request_body.kind());
    out.push_str(",\"respKind\":");
    q(out, x.response_body.as_ref().map_or("-", Decoded::kind));
    let _ = write!(
        out,
        ",\"complete\":{},\"client\":\"{}\",\"server\":\"{}\",\"resolvedName\":",
        x.request.complete && x.response.as_ref().is_some_and(|r| r.complete),
        x.client,
        x.server
    );
    opt_str(out, x.resolved_name.as_deref());
    out.push_str(",\"timings\":");
    timings(out, &x.timings);
    out.push('}');
}

/// La lista dell'ispettore: richieste HTTP (in ordine di inizio), scambi
/// DNS e flussi TLS.
///
/// ```json
/// {"frames":N,"requests":[{"i":0,"method":"POST","url":"...","host":"...",
///   "path":"/v1?x=1","status":200,"reqBytes":12,"respBytes":0,"reqKind":"json",
///   "timings":{"startedUs":...,"totalUs":...},...}],
///  "dns":[{"name":"api.example","type":"A","queryUs":1000,"answerUs":1300,
///   "rcode":0,"addrs":["198.18.0.1"]}],
///  "tls":[{"flow":3,"server":"198.18.0.2:443","sni":"x.org","startedUs":...}]}
/// ```
pub fn requests_json(a: &NetworkAnalysis) -> String {
    let mut out = String::new();
    let _ = write!(out, "{{\"frames\":{},\"requests\":[", a.frames);
    for (i, x) in a.http.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        row(&mut out, x);
    }
    out.push_str("],\"dns\":[");
    for (i, d) in a.dns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        q(&mut out, &d.name);
        out.push_str(",\"type\":");
        q(&mut out, &dns::type_name(d.qtype));
        let _ = write!(out, ",\"queryUs\":{},\"answerUs\":", d.query_at);
        opt_u64(&mut out, d.answer_at);
        out.push_str(",\"rcode\":");
        opt_u64(&mut out, d.rcode.map(u64::from));
        out.push_str(",\"addrs\":[");
        for (k, ip) in d.ipv4().enumerate() {
            if k > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{ip}\"");
        }
        out.push_str("]}");
    }
    out.push_str("],\"tls\":[");
    for (i, t) in a.tls.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"flow\":{},\"server\":\"{}\",\"sni\":", t.flow, t.server);
        opt_str(&mut out, t.sni.as_deref());
        let _ = write!(out, ",\"startedUs\":{}}}", t.started_us);
    }
    out.push_str("]}");
    out
}

/// Il dettaglio di una richiesta: la riga della lista più `request` e
/// `response` (linea iniziale, intestazioni in ordine, corpo con la resa
/// del decodificatore e i byte in base64).
pub fn exchange_json(x: &HttpExchange) -> String {
    let mut out = String::new();
    out.push_str("{\"row\":");
    row(&mut out, x);
    out.push_str(",\"request\":{\"method\":");
    q(&mut out, &x.request.method);
    out.push_str(",\"target\":");
    q(&mut out, &x.request.target);
    out.push_str(",\"version\":");
    q(&mut out, &x.request.version);
    let _ = write!(out, ",\"complete\":{},\"headers\":", x.request.complete);
    headers(&mut out, &x.request.headers);
    out.push_str(",\"body\":");
    body_json(&mut out, &x.request.body, x.request.headers.get("content-type"), &x.request_body);
    out.push_str("},\"response\":");
    match (&x.response, &x.response_body) {
        (Some(r), Some(d)) => {
            let _ = write!(out, "{{\"status\":{},\"reason\":", r.status);
            q(&mut out, &r.reason);
            out.push_str(",\"version\":");
            q(&mut out, &r.version);
            let _ = write!(out, ",\"complete\":{},\"interim\":[", r.complete);
            for (i, s) in r.interim.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{s}");
            }
            out.push_str("],\"headers\":");
            headers(&mut out, &r.headers);
            out.push_str(",\"body\":");
            body_json(&mut out, &r.body, r.headers.get("content-type"), d);
            out.push('}');
        }
        _ => out.push_str("null"),
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::body::decode as decode_body;
    use crate::net::inspector::tests::session;
    use crate::net::json::{self, Value};

    #[test]
    fn host_e_percorso() {
        assert_eq!(split_url("http://api.example:8080/v1?x=1"), ("api.example:8080", "/v1?x=1"));
        assert_eq!(split_url("https://h"), ("h", "/"));
        assert_eq!(split_url("h/p"), ("h", "/p"));
    }

    /// La lista e il dettaglio sono JSON validi (il nostro parser) con i
    /// campi che la pagina usa; il corpo JSON della risposta c'è anche come
    /// valore, il form come coppie.
    #[test]
    fn lista_e_dettaglio_in_json() {
        let a = NetworkAnalysis::from_frames(&session());
        let list = json::parse(requests_json(&a).as_bytes()).expect("lista JSON");
        let Some(Value::Array(reqs)) = list.get("requests") else { panic!("{list:?}") };
        assert_eq!(reqs.len(), 2);
        let r0 = &reqs[0];
        assert_eq!(r0.get("method").and_then(Value::as_str), Some("GET"));
        assert_eq!(r0.get("host").and_then(Value::as_str), Some("api.example:8080"));
        assert_eq!(r0.get("path").and_then(Value::as_str), Some("/v1/items?id=7&q=a+b"));
        assert_eq!(r0.get("status"), Some(&Value::Number("200".into())));
        assert_eq!(r0.get("respKind").and_then(Value::as_str), Some("json"));
        let t = r0.get("timings").unwrap();
        assert_eq!(t.get("totalUs"), Some(&Value::Number("2400".into())));
        assert_eq!(t.get("dnsUs"), Some(&Value::Number("300".into())));
        let Some(Value::Array(dns)) = list.get("dns") else { panic!() };
        assert_eq!(dns[0].get("name").and_then(Value::as_str), Some("api.example"));
        assert_eq!(dns[0].get("addrs"), Some(&Value::Array(vec![Value::String("198.18.0.1".into())])));

        let d = json::parse(exchange_json(&a.http[0]).as_bytes()).expect("dettaglio JSON");
        let resp = d.get("response").unwrap();
        let body = resp.get("body").unwrap();
        assert_eq!(body.get("kind").and_then(Value::as_str), Some("json"));
        assert_eq!(
            body.get("json").unwrap().get("items"),
            Some(&Value::Array(vec![Value::Number("7".into())]))
        );
        assert_eq!(body.get("base64").and_then(Value::as_str), Some(base64(b"{\"items\":[7]}").as_str()));
        let Some(Value::Array(h)) = d.get("request").unwrap().get("headers") else { panic!() };
        assert_eq!(
            h[1],
            Value::Array(vec![Value::String("Cookie".into()), Value::String("s=1; t=2".into())])
        );

        let d = json::parse(exchange_json(&a.http[1]).as_bytes()).unwrap();
        let body = d.get("request").unwrap().get("body").unwrap();
        assert_eq!(body.get("kind").and_then(Value::as_str), Some("form"));
        assert_eq!(
            body.get("fields"),
            Some(&Value::Array(vec![
                Value::Array(vec![Value::String("a".into()), Value::String("1".into())]),
                Value::Array(vec![Value::String("b".into()), Value::String("2".into())]),
            ]))
        );
        assert_eq!(d.get("response").unwrap().get("status"), Some(&Value::Number("204".into())));
    }

    /// Multipart con una parte JSON e protobuf senza schema: la struttura
    /// e la resa testuale arrivano nel dettaglio.
    #[test]
    fn corpi_multipart_e_protobuf() {
        let mp = b"--xx\r\nContent-Disposition: form-data; name=\"f\"; filename=\"a.json\"\r\nContent-Type: application/json\r\n\r\n{\"k\":1}\r\n--xx--\r\n";
        let d = decode_body(Some("multipart/form-data; boundary=xx"), mp);
        let mut s = String::from("{");
        decoded(&mut s, &d);
        s.push('}');
        let v = json::parse(s.as_bytes()).unwrap();
        let Some(Value::Array(parts)) = v.get("parts") else { panic!("{s}") };
        assert_eq!(parts[0].get("filename").and_then(Value::as_str), Some("a.json"));
        assert_eq!(parts[0].get("kind").and_then(Value::as_str), Some("json"));
        let pb = decode_body(Some("application/x-protobuf"), &[0x08, 0x96, 0x01, 0x12, 0x02, b'o', b'k']);
        let mut s = String::from("{");
        decoded(&mut s, &pb);
        s.push('}');
        let v = json::parse(s.as_bytes()).unwrap();
        assert_eq!(v.get("kind").and_then(Value::as_str), Some("protobuf"));
        assert_eq!(v.get("text").and_then(Value::as_str), Some("1: 150\n2: \"ok\"\n"));
    }

    #[test]
    fn tagli_su_confine_di_carattere() {
        assert_eq!(cut("àè", 3), ("à", true));
        assert_eq!(cut("abc", 3), ("abc", false));
    }
}
