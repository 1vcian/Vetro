//! HTTP/1.1 (RFC 9112) sopra un flusso TCP ricostruito: richieste dal
//! cliente, risposte dal server, abbinate in ordine (pipelining compreso).
//!
//! Corpo: `Transfer-Encoding: chunked`, `Content-Length`, oppure (solo
//! risposte) fino alla chiusura; niente corpo per HEAD, 1xx, 204 e 304; le
//! risposte 1xx sono intermedie e non consumano la richiesta. Il corpo
//! decodificato toglie anche `Content-Encoding` gzip/x-gzip e deflate;
//! altre codifiche (br, zstd) restano com'erano, annotate.
//! Messaggi troncati dalla fine della cattura restano con `complete: false`.

use super::inflate;

/// Metodi riconosciuti all'inizio di un flusso per considerarlo HTTP.
pub const METHODS: &[&str] =
    &["GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH", "CONNECT", "TRACE"];

/// Limite delle intestazioni di un messaggio.
const MAX_HEAD: usize = 64 << 10;

/// Il flusso del cliente comincia con una richiesta HTTP/1.x?
pub fn looks_like_request(stream: &[u8]) -> bool {
    METHODS
        .iter()
        .any(|m| stream.len() > m.len() && stream.starts_with(m.as_bytes()) && stream[m.len()] == b' ')
}

/// Intestazioni in ordine, con il nome come arrivato.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headers(pub Vec<(String, String)>);

impl Headers {
    /// Primo valore del campo `name` (senza distinzione di maiuscole).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.0.iter().filter(move |(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Il campo contiene il token `tok` in una lista separata da virgole?
    pub fn has_token(&self, name: &str, tok: &str) -> bool {
        self.get_all(name).flat_map(|v| v.split(',')).any(|t| t.trim().eq_ignore_ascii_case(tok))
    }
}

/// Il corpo di un messaggio, come trasferito e decodificato.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Body {
    /// Byte del corpo sul filo (con le cornici di `chunked`).
    pub wire_len: usize,
    /// Corpo dopo `Transfer-Encoding` (quello che `Content-Encoding`
    /// descrive).
    pub raw: Vec<u8>,
    /// Corpo dopo `Content-Encoding` (uguale a `raw` se identità).
    pub decoded: Vec<u8>,
    /// Codifica applicata con successo ("gzip", "deflate"), se c'è.
    pub content_encoding: Option<String>,
    /// Perché `decoded` non è decodificato (codifica sconosciuta o dati
    /// corrotti).
    pub decode_error: Option<String>,
    pub chunked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Headers,
    pub body: Body,
    /// Offset nel flusso: inizio, fine delle intestazioni, fine.
    pub start: usize,
    pub head_end: usize,
    pub end: usize,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub version: String,
    pub status: u16,
    pub reason: String,
    pub headers: Headers,
    pub body: Body,
    pub start: usize,
    pub head_end: usize,
    pub end: usize,
    pub complete: bool,
    /// Risposte 1xx arrivate prima di questa (100 Continue...).
    pub interim: Vec<u16>,
}

struct Head {
    line: [String; 3],
    headers: Headers,
    end: usize,
}

/// Intestazioni a partire da `pos`; `None` se incomplete o malformate.
fn head(s: &[u8], pos: usize) -> Option<Head> {
    let window = &s[pos..s.len().min(pos + MAX_HEAD)];
    let n = window.windows(4).position(|w| w == b"\r\n\r\n")?;
    let text = String::from_utf8_lossy(&window[..n]);
    let mut lines = text.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.splitn(3, ' ');
    let line = [parts.next()?.to_string(), parts.next()?.to_string(), parts.next().unwrap_or("").to_string()];
    let mut headers = Headers::default();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.0.push((k.trim().to_string(), v.trim().to_string()));
        } else if l.starts_with([' ', '\t'])
            && let Some(last) = headers.0.last_mut()
        {
            // Riga di continuazione (obs-fold).
            last.1.push(' ');
            last.1.push_str(l.trim());
        }
    }
    Some(Head { line, headers, end: pos + n + 4 })
}

/// Esito della lettura di un corpo: (raw, fine, completo).
fn chunked(s: &[u8], mut pos: usize) -> (Vec<u8>, usize, bool) {
    let mut raw = Vec::new();
    loop {
        let Some(nl) = s[pos..].windows(2).position(|w| w == b"\r\n") else { return (raw, s.len(), false) };
        let line = String::from_utf8_lossy(&s[pos..pos + nl]);
        let size = line.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size, 16) else { return (raw, s.len(), false) };
        pos += nl + 2;
        if size == 0 {
            // Trailer fino a una riga vuota.
            loop {
                let Some(nl) = s[pos..].windows(2).position(|w| w == b"\r\n") else {
                    return (raw, s.len(), false);
                };
                pos += nl + 2;
                if nl == 0 {
                    return (raw, pos, true);
                }
            }
        }
        let end = pos.saturating_add(size);
        if end > s.len() {
            raw.extend_from_slice(&s[pos..]);
            return (raw, s.len(), false);
        }
        raw.extend_from_slice(&s[pos..end]);
        pos = end;
        if s.len() < pos + 2 {
            return (raw, s.len(), false);
        }
        pos += 2;
    }
}

enum Framing {
    None,
    Length(usize),
    Chunked,
    UntilClose,
}

fn body(s: &[u8], at: usize, framing: Framing, headers: &Headers, eof: bool) -> (Body, usize, bool) {
    let (raw, end, complete, is_chunked) = match framing {
        Framing::None => (Vec::new(), at, true, false),
        Framing::Length(n) => {
            let end = at.saturating_add(n).min(s.len());
            (s[at..end].to_vec(), end, end - at == n, false)
        }
        Framing::Chunked => {
            let (raw, end, ok) = chunked(s, at);
            (raw, end, ok, true)
        }
        Framing::UntilClose => (s[at..].to_vec(), s.len(), eof, false),
    };
    let mut b = Body { wire_len: end - at, chunked: is_chunked, ..Body::default() };
    let codings: Vec<String> = headers
        .get_all("content-encoding")
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty() && t != "identity")
        .collect();
    let mut decoded = raw.clone();
    let mut applied = Vec::new();
    for c in codings.iter().rev() {
        let r = match c.as_str() {
            "gzip" | "x-gzip" => inflate::gunzip(&decoded),
            "deflate" => inflate::zlib_or_raw(&decoded),
            other => {
                b.decode_error = Some(format!("Content-Encoding {other} non supportato"));
                break;
            }
        };
        match r {
            Ok(d) => {
                decoded = d;
                applied.push(c.clone());
            }
            Err(e) => {
                b.decode_error =
                    Some(format!("{c}: {e}{}", if complete { "" } else { " (corpo incompleto)" }));
                break;
            }
        }
    }
    if b.decode_error.is_some() {
        decoded = raw.clone();
    } else if !applied.is_empty() {
        applied.reverse();
        b.content_encoding = Some(applied.join(", "));
    }
    b.raw = raw;
    b.decoded = decoded;
    (b, end, complete)
}

fn content_length(h: &Headers) -> Option<usize> {
    h.get("content-length").and_then(|v| v.split(',').next()?.trim().parse().ok())
}

/// Le richieste nel flusso del cliente.
pub fn parse_requests(s: &[u8]) -> Vec<Request> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < s.len() {
        // Righe vuote fra un messaggio e l'altro sono tollerate.
        while s[pos..].starts_with(b"\r\n") {
            pos += 2;
        }
        if pos >= s.len() || !looks_like_request(&s[pos..]) {
            break;
        }
        let Some(h) = head(s, pos) else {
            let text = String::from_utf8_lossy(&s[pos..s.len().min(pos + 8192)]).into_owned();
            let mut p = text.split(' ');
            out.push(Request {
                method: p.next().unwrap_or("").to_string(),
                target: p.next().unwrap_or("").split("\r\n").next().unwrap_or("").to_string(),
                version: String::new(),
                headers: Headers::default(),
                body: Body::default(),
                start: pos,
                head_end: s.len(),
                end: s.len(),
                complete: false,
            });
            break;
        };
        let framing = if h.headers.has_token("transfer-encoding", "chunked") {
            Framing::Chunked
        } else {
            content_length(&h.headers).map_or(Framing::None, Framing::Length)
        };
        let (b, end, complete) = body(s, h.end, framing, &h.headers, false);
        let [method, target, version] = h.line;
        out.push(Request {
            method,
            target,
            version,
            headers: h.headers,
            body: b,
            start: pos,
            head_end: h.end,
            end,
            complete,
        });
        if !complete {
            break;
        }
        pos = end;
    }
    out
}

/// Le risposte nel flusso del server; `methods` sono i metodi delle
/// richieste in ordine (per HEAD), `eof` dice se il server ha chiuso.
pub fn parse_responses(s: &[u8], methods: &[&str], eof: bool) -> Vec<Response> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut interim = Vec::new();
    while pos < s.len() && s[pos..].starts_with(b"HTTP/") {
        let Some(h) = head(s, pos) else {
            break;
        };
        let status: u16 = h.line[1].parse().unwrap_or(0);
        if (100..200).contains(&status) && status != 101 {
            interim.push(status);
            pos = h.end;
            continue;
        }
        let method = methods.get(out.len()).copied().unwrap_or("GET");
        let framing = if method.eq_ignore_ascii_case("HEAD")
            || (100..200).contains(&status)
            || status == 204
            || status == 304
            || (method.eq_ignore_ascii_case("CONNECT") && (200..300).contains(&status))
        {
            Framing::None
        } else if h.headers.has_token("transfer-encoding", "chunked") {
            Framing::Chunked
        } else if let Some(n) = content_length(&h.headers) {
            Framing::Length(n)
        } else {
            Framing::UntilClose
        };
        let (b, end, complete) = body(s, h.end, framing, &h.headers, eof);
        let [version, _, reason] = h.line;
        out.push(Response {
            version,
            status,
            reason,
            headers: h.headers,
            body: b,
            start: pos,
            head_end: h.end,
            end,
            complete,
            interim: std::mem::take(&mut interim),
        });
        if !complete || status == 101 {
            break;
        }
        pos = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_e_risposta_con_lunghezza() {
        let req = b"GET /a?b=1 HTTP/1.1\r\nHost: x.example\r\nUser-Agent: Wget\r\n\r\n";
        let r = parse_requests(req);
        assert_eq!(r.len(), 1);
        assert_eq!(
            (r[0].method.as_str(), r[0].target.as_str(), r[0].version.as_str()),
            ("GET", "/a?b=1", "HTTP/1.1")
        );
        assert_eq!(r[0].headers.get("host"), Some("x.example"));
        assert!(r[0].complete && r[0].body.raw.is_empty());
        assert_eq!(r[0].end, req.len());

        let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: text/plain\r\n\r\nciao!";
        let p = parse_responses(resp, &["GET"], true);
        assert_eq!(
            (p[0].status, p[0].reason.as_str(), p[0].body.decoded.as_slice()),
            (200, "OK", &b"ciao!"[..])
        );
        assert_eq!((p[0].head_end, p[0].end), (resp.len() - 5, resp.len()));
    }

    #[test]
    fn post_pipelining_head_e_100_continue() {
        let reqs = b"POST /x HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcHEAD /y HTTP/1.1\r\n\r\nGET /z HTTP/1.1\r\n\r\n";
        let r = parse_requests(reqs);
        assert_eq!(r.iter().map(|r| r.method.as_str()).collect::<Vec<_>>(), ["POST", "HEAD", "GET"]);
        assert_eq!(r[0].body.raw, b"abc");
        let resp = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nokHTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n";
        let p = parse_responses(resp, &["POST", "HEAD", "GET"], false);
        assert_eq!(p.iter().map(|r| r.status).collect::<Vec<_>>(), [201, 200, 204]);
        assert_eq!(p[0].interim, [100]);
        assert!(p[1].body.raw.is_empty() && p[1].complete, "HEAD senza corpo");
    }

    #[test]
    fn chunked_con_estensioni_e_trailer() {
        let resp = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4;x=y\r\nWiki\r\n6\r\npedia \r\nE\r\nin \r\n\r\nchunks.\r\n0\r\nX-T: 1\r\n\r\nHTTP/1.1 200 OK\r\n";
        let p = parse_responses(resp, &["GET", "GET"], false);
        assert_eq!(p.len(), 1, "la seconda è troncata e senza intestazioni complete");
        assert_eq!(p[0].body.decoded, b"Wikipedia in \r\n\r\nchunks.");
        assert!(p[0].body.chunked && p[0].complete);
        assert_eq!(&resp[p[0].end..], b"HTTP/1.1 200 OK\r\n");
        // Troncato a metà di un pezzo.
        let cut = &resp[..60];
        let p = parse_responses(cut, &["GET"], false);
        assert!(!p[0].complete);
    }

    #[test]
    fn fino_alla_chiusura_e_troncata() {
        let resp = b"HTTP/1.0 200 OK\r\n\r\ntutto fino alla fine";
        let p = parse_responses(resp, &["GET"], true);
        assert!(p[0].complete);
        assert_eq!(p[0].body.decoded, b"tutto fino alla fine");
        assert!(!parse_responses(resp, &["GET"], false)[0].complete);
        let p = parse_responses(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n12345", &["GET"], true);
        assert!(!p[0].complete && p[0].body.raw == b"12345");
    }

    #[test]
    fn content_encoding() {
        let gz: &[u8] = &[
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0x4b, 0xce, 0x4c, 0xcc, 0x57, 0x48,
            0x86, 0x13, 0x65, 0xa9, 0x25, 0x45, 0xf9, 0x5c, 0x00, 0xb6, 0x65, 0xcc, 0x5d, 0x15, 0x00, 0x00,
            0x00,
        ];
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            10
        )
        .into_bytes();
        resp.extend(&gz[..10]);
        resp.extend(format!("\r\n{:x}\r\n", gz.len() - 10).as_bytes());
        resp.extend(&gz[10..]);
        resp.extend(b"\r\n0\r\n\r\n");
        let p = parse_responses(&resp, &["GET"], true);
        assert_eq!(p[0].body.raw, gz);
        assert_eq!(p[0].body.decoded, b"ciao ciao ciao vetro\n");
        assert_eq!(p[0].body.content_encoding.as_deref(), Some("gzip"));
        assert!(p[0].body.wire_len > gz.len());

        let p = parse_responses(
            b"HTTP/1.1 200 OK\r\nContent-Encoding: br\r\nContent-Length: 2\r\n\r\nxx",
            &["GET"],
            true,
        );
        assert_eq!(p[0].body.decoded, b"xx");
        assert!(p[0].body.decode_error.as_deref().unwrap().contains("br"));
        let p = parse_responses(
            b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\n\r\nxx",
            &["GET"],
            true,
        );
        assert!(p[0].body.decode_error.is_some() && p[0].body.content_encoding.is_none());
    }

    #[test]
    fn non_http_e_malformati() {
        assert!(!looks_like_request(b"\x16\x03\x01\x02\x00"));
        assert!(!looks_like_request(b"GETX / HTTP/1.1"));
        assert!(parse_requests(b"\x16\x03\x01").is_empty());
        let r = parse_requests(b"GET /senza-fine HTTP/1.1\r\nHost: a");
        assert_eq!((r[0].target.as_str(), r[0].complete), ("/senza-fine", false));
        let mut x = 7u32;
        for len in 0..300 {
            let mut v = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
            v.extend((0..len).map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                b"0123456789abcdef\r\n;"[(x >> 24) as usize % 19]
            }));
            let _ = parse_responses(&v, &[], false);
        }
    }
}
