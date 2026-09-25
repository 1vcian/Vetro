//! Il modello dell'ispettore di rete: dai frame ai flussi, agli scambi DNS
//! e alle richieste HTTP con corpo decodificato e timing (le fasi
//! dell'HAR), più i nomi TLS (SNI) dei flussi cifrati.
//!
//! I tempi sono quelli dei frame al confine di virtio-net, cioè visti dal
//! guest: `dns` va dalla domanda alla risposta del DNS che ha dato
//! l'indirizzo del server (solo per la prima connessione che lo usa),
//! `connect` dal SYN all'ACK che chiude l'handshake (solo per la prima
//! richiesta di una connessione), `send` dal primo all'ultimo byte della
//! richiesta, `wait` dall'ultimo byte della richiesta al primo della
//! risposta, `receive` fino all'ultimo byte della risposta. `blocked` è ciò
//! che resta fra l'inizio e il primo byte della richiesta. Il totale è la
//! somma delle fasi, come chiede l'HAR.

use std::collections::BTreeSet;
use std::fmt;
use std::net::SocketAddrV4;

use super::body::{self, Decoded};
use super::capture::Frame;
use super::dns::{self, DnsExchange};
use super::flow::{Flows, TcpFlow};
use super::http::{self, Request, Response};

/// Le fasi di una richiesta, in microsecondi di tempo del guest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timings {
    /// Inizio della richiesta (domanda DNS, SYN o primo byte).
    pub started_us: u64,
    pub blocked_us: Option<u64>,
    pub dns_us: Option<u64>,
    pub connect_us: Option<u64>,
    pub send_us: u64,
    pub wait_us: u64,
    pub receive_us: u64,
}

impl Timings {
    /// Somma delle fasi presenti.
    pub fn total_us(&self) -> u64 {
        self.blocked_us.unwrap_or(0)
            + self.dns_us.unwrap_or(0)
            + self.connect_us.unwrap_or(0)
            + self.send_us
            + self.wait_us
            + self.receive_us
    }
}

/// Una richiesta HTTP con la sua risposta (se arrivata).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpExchange {
    /// Posizione nella lista delle richieste (ordine di inizio).
    pub index: usize,
    /// Indice del flusso TCP.
    pub flow: usize,
    pub client: SocketAddrV4,
    pub server: SocketAddrV4,
    /// Nome risolto dal DNS per l'indirizzo del server, se c'è.
    pub resolved_name: Option<String>,
    pub url: String,
    pub request: Request,
    pub response: Option<Response>,
    pub request_body: Decoded,
    pub response_body: Option<Decoded>,
    pub timings: Timings,
    /// Prima richiesta della sua connessione.
    pub first_on_connection: bool,
}

impl HttpExchange {
    pub fn status(&self) -> Option<u16> {
        self.response.as_ref().map(|r| r.status)
    }

    /// `Content-Type` della risposta.
    pub fn mime(&self) -> Option<&str> {
        self.response.as_ref().and_then(|r| r.headers.get("content-type"))
    }
}

/// Un flusso TLS con il nome chiesto nel ClientHello.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsFlow {
    pub flow: usize,
    pub server: SocketAddrV4,
    pub sni: Option<String>,
    pub started_us: u64,
}

/// Una riga della lista dell'ispettore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRow {
    pub index: usize,
    pub started_us: u64,
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    pub mime: Option<String>,
    /// Corpo decodificato della richiesta e della risposta, in byte.
    pub request_bytes: usize,
    pub response_bytes: usize,
    pub request_kind: &'static str,
    pub response_kind: &'static str,
    pub timings: Timings,
}

impl fmt::Display for RequestRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let t = self.timings.total_us();
        write!(
            f,
            "[{:6}.{:06}] #{} {} {} -> {} {} ({} B {}, risposta {} B {}) {}.{:03} ms",
            self.started_us / 1_000_000,
            self.started_us % 1_000_000,
            self.index,
            self.method,
            self.url,
            self.status.map_or_else(|| "---".to_string(), |s| s.to_string()),
            self.mime.as_deref().unwrap_or("-"),
            self.request_bytes,
            self.request_kind,
            self.response_bytes,
            self.response_kind,
            t / 1000,
            t % 1000
        )
    }
}

/// Tutto ciò che l'analisi ricava da una cattura.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkAnalysis {
    pub frames: usize,
    pub flows: Flows,
    pub dns: Vec<DnsExchange>,
    pub http: Vec<HttpExchange>,
    pub tls: Vec<TlsFlow>,
}

/// Il nome del server nel ClientHello TLS (estensione server_name).
pub fn tls_sni(s: &[u8]) -> Option<String> {
    // Record handshake, ClientHello.
    if s.len() < 9 || s[0] != 0x16 || s[1] != 3 || s[5] != 1 {
        return None;
    }
    let rec_len = usize::from(u16::from_be_bytes([s[3], s[4]]));
    let h = s.get(5..5 + rec_len).unwrap_or(&s[5..]);
    let mut p = 4 + 2 + 32;
    let sid = usize::from(*h.get(p)?);
    p += 1 + sid;
    let cs = usize::from(u16::from_be_bytes([*h.get(p)?, *h.get(p + 1)?]));
    p += 2 + cs;
    let comp = usize::from(*h.get(p)?);
    p += 1 + comp;
    let ext_len = usize::from(u16::from_be_bytes([*h.get(p)?, *h.get(p + 1)?]));
    p += 2;
    let end = (p + ext_len).min(h.len());
    while p + 4 <= end {
        let t = u16::from_be_bytes([h[p], h[p + 1]]);
        let l = usize::from(u16::from_be_bytes([h[p + 2], h[p + 3]]));
        let e = h.get(p + 4..p + 4 + l)?;
        if t == 0 && e.len() >= 5 && e[2] == 0 {
            let n = usize::from(u16::from_be_bytes([e[3], e[4]]));
            return e.get(5..5 + n).map(|n| String::from_utf8_lossy(n).to_ascii_lowercase());
        }
        p += 4 + l;
    }
    None
}

fn span(flow: &TcpFlow, from_client: bool, start: usize, end: usize) -> (u64, u64) {
    let s = if from_client { &flow.client_data } else { &flow.server_data };
    (s.time_at(start), s.time_at(end.saturating_sub(1).max(start)))
}

impl NetworkAnalysis {
    /// Analizza una cattura (frame in ordine di tempo).
    pub fn from_frames(frames: &[Frame]) -> Self {
        let flows = Flows::from_frames(frames);
        let dns = dns::exchanges(&flows.udp);
        let mut used_dns = BTreeSet::new();
        let mut http = Vec::new();
        let mut tls = Vec::new();
        for f in &flows.tcp {
            let start = f.syn_at.unwrap_or(f.first_at);
            // L'ultima risposta DNS con l'indirizzo del server arrivata prima
            // della connessione.
            let lookup = dns
                .iter()
                .enumerate()
                .filter(|(_, d)| {
                    d.answer_at.is_some_and(|a| a <= start) && d.ipv4().any(|a| a == *f.server.ip())
                })
                .max_by_key(|(_, d)| d.answer_at);
            let resolved_name = lookup.map(|(_, d)| d.name.clone());
            if tls_sni(&f.client_data.bytes).is_some() || f.client_data.bytes.starts_with(&[0x16, 3]) {
                tls.push(TlsFlow {
                    flow: f.index,
                    server: f.server,
                    sni: tls_sni(&f.client_data.bytes),
                    started_us: start,
                });
                continue;
            }
            if !http::looks_like_request(&f.client_data.bytes) {
                continue;
            }
            let reqs = http::parse_requests(&f.client_data.bytes);
            let methods: Vec<&str> = reqs.iter().map(|r| r.method.as_str()).collect();
            let eof = f.server_data.fin || f.reset;
            let mut resps = http::parse_responses(&f.server_data.bytes, &methods, eof).into_iter();
            for (i, req) in reqs.into_iter().enumerate() {
                let resp = resps.next();
                let first = i == 0;
                let (req_start, req_end) = span(f, true, req.start, req.end);
                let mut t = Timings { started_us: req_start, ..Timings::default() };
                if first {
                    if let (Some(s), Some(e)) = (f.syn_at, f.established_at) {
                        t.connect_us = Some(e.saturating_sub(s));
                        t.started_us = s;
                    }
                    if let Some((k, d)) = lookup
                        && f.syn_at.is_some()
                        && used_dns.insert(k)
                    {
                        let a = d.answer_at.unwrap_or(d.query_at);
                        t.dns_us = Some(a - d.query_at);
                        t.started_us = d.query_at;
                    }
                }
                t.blocked_us = Some(
                    req_start
                        .saturating_sub(t.started_us)
                        .saturating_sub(t.dns_us.unwrap_or(0) + t.connect_us.unwrap_or(0)),
                )
                .filter(|&b| b > 0 || !first);
                t.send_us = req_end - req_start;
                if let Some(r) = &resp {
                    let (rs, re) = span(f, false, r.start, r.end);
                    t.wait_us = rs.saturating_sub(req_end);
                    t.receive_us = re.saturating_sub(rs.max(req_end));
                }
                let host = req
                    .headers
                    .get("host")
                    .map(str::to_string)
                    .or_else(|| {
                        resolved_name.as_ref().map(|n| {
                            if f.server.port() == 80 { n.clone() } else { format!("{n}:{}", f.server.port()) }
                        })
                    })
                    .unwrap_or_else(|| f.server.to_string());
                let url = if req.target.starts_with("http://") || req.target.starts_with("https://") {
                    req.target.clone()
                } else if req.method.eq_ignore_ascii_case("CONNECT") {
                    format!("https://{}", req.target)
                } else {
                    format!("http://{host}{}", req.target)
                };
                let request_body = body::decode(req.headers.get("content-type"), &req.body.decoded);
                let response_body =
                    resp.as_ref().map(|r| body::decode(r.headers.get("content-type"), &r.body.decoded));
                http.push(HttpExchange {
                    index: 0,
                    flow: f.index,
                    client: f.client,
                    server: f.server,
                    resolved_name: resolved_name.clone(),
                    url,
                    request: req,
                    response: resp,
                    request_body,
                    response_body,
                    timings: t,
                    first_on_connection: first,
                });
            }
        }
        http.sort_by_key(|x: &HttpExchange| (x.timings.started_us, x.flow, x.request.start));
        for (i, x) in http.iter_mut().enumerate() {
            x.index = i;
        }
        NetworkAnalysis { frames: frames.len(), flows, dns, http, tls }
    }

    /// La lista dell'ispettore: una riga per richiesta, in ordine di inizio.
    pub fn requests(&self) -> Vec<RequestRow> {
        self.http
            .iter()
            .map(|x| RequestRow {
                index: x.index,
                started_us: x.timings.started_us,
                method: x.request.method.clone(),
                url: x.url.clone(),
                status: x.status(),
                mime: x.mime().map(str::to_string),
                request_bytes: x.request.body.decoded.len(),
                response_bytes: x.response.as_ref().map_or(0, |r| r.body.decoded.len()),
                request_kind: x.request_body.kind(),
                response_kind: x.response_body.as_ref().map_or("-", Decoded::kind),
                timings: x.timings,
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::net::capture::Direction;
    use crate::net::dns::build as dnsb;
    use crate::net::packet::{build, tcp_flags::*};

    fn sa(s: &str) -> SocketAddrV4 {
        s.parse().unwrap()
    }

    /// Una sessione come quella del guest: DNS, handshake, GET e POST sulla
    /// stessa connessione, chiusura.
    pub(crate) fn session() -> Vec<Frame> {
        let (g, dns_s, srv) = (sa("10.0.2.15:4000"), sa("10.0.2.3:53"), sa("198.18.0.1:8080"));
        let c = sa("10.0.2.15:40000");
        let mut v = Vec::new();
        let mut push = |at: u64, dir: Direction, data: Vec<u8>| v.push(Frame { at_us: at, dir, data });
        push(1_000, Direction::FromGuest, build::udp(g, dns_s, &dnsb::query(9, "api.example", 1)));
        push(
            1_300,
            Direction::ToGuest,
            build::udp(dns_s, g, &dnsb::answer(9, "api.example", &[[198, 18, 0, 1]])),
        );
        push(2_000, Direction::FromGuest, build::tcp(c, srv, 100, 0, SYN, b""));
        push(2_100, Direction::ToGuest, build::tcp(srv, c, 500, 101, SYN | ACK, b""));
        push(2_200, Direction::FromGuest, build::tcp(c, srv, 101, 501, ACK, b""));
        let r1 = b"GET /v1/items?id=7&q=a+b HTTP/1.1\r\nHost: api.example:8080\r\nCookie: s=1; t=2\r\n\r\n";
        push(2_500, Direction::FromGuest, build::tcp(c, srv, 101, 501, ACK | PSH, r1));
        let s1 =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"items\":[7]}";
        push(3_000, Direction::ToGuest, build::tcp(srv, c, 501, 101 + r1.len() as u32, ACK, &s1[..40]));
        push(3_400, Direction::ToGuest, build::tcp(srv, c, 541, 101 + r1.len() as u32, ACK, &s1[40..]));
        let r2 = b"POST /v1/form HTTP/1.1\r\nHost: api.example:8080\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: 7\r\n\r\na=1&b=2";
        let seq2 = 101 + r1.len() as u32;
        push(4_000, Direction::FromGuest, build::tcp(c, srv, seq2, 0, ACK, &r2[..20]));
        push(4_200, Direction::FromGuest, build::tcp(c, srv, seq2 + 20, 0, ACK, &r2[20..]));
        let s2 = b"HTTP/1.1 204 No Content\r\n\r\n";
        let sseq2 = 501 + s1.len() as u32;
        push(4_900, Direction::ToGuest, build::tcp(srv, c, sseq2, 0, ACK | FIN, s2));
        push(5_000, Direction::FromGuest, build::tcp(c, srv, seq2 + r2.len() as u32, 0, ACK | FIN, b""));
        v
    }

    #[test]
    fn richieste_con_timing() {
        let a = NetworkAnalysis::from_frames(&session());
        assert_eq!(a.dns.len(), 1);
        assert_eq!(a.http.len(), 2);
        let (x, y) = (&a.http[0], &a.http[1]);
        assert_eq!(x.url, "http://api.example:8080/v1/items?id=7&q=a+b");
        assert_eq!(x.resolved_name.as_deref(), Some("api.example"));
        assert_eq!(x.status(), Some(200));
        assert!(matches!(x.response_body, Some(Decoded::Json(_))));
        assert_eq!(
            x.timings,
            Timings {
                started_us: 1_000,
                // Da 1_000 al primo byte (2_500), meno dns e connect.
                blocked_us: Some(1_000),
                dns_us: Some(300),
                connect_us: Some(200),
                send_us: 0,
                wait_us: 500,
                receive_us: 400
            }
        );
        assert_eq!(x.timings.total_us(), 2_400);
        assert_eq!(y.request.method, "POST");
        assert_eq!(y.request_body, Decoded::Form(vec![("a".into(), "1".into()), ("b".into(), "2".into())]));
        assert_eq!(
            y.timings,
            Timings {
                started_us: 4_000,
                blocked_us: Some(0),
                dns_us: None,
                connect_us: None,
                send_us: 200,
                wait_us: 700,
                receive_us: 0
            }
        );
        assert_eq!(y.status(), Some(204));
        let rows = a.requests();
        assert_eq!(
            rows[0].to_string(),
            "[     0.001000] #0 GET http://api.example:8080/v1/items?id=7&q=a+b -> 200 application/json (0 B vuoto, risposta 13 B json) 2.400 ms"
        );
        assert_eq!(rows[1].response_kind, "vuoto");
    }

    #[test]
    fn sni_del_client_hello() {
        // ClientHello minimo con server_name "Esempio.org".
        let name = b"Esempio.org";
        let mut sni = vec![0, 0];
        let list_len = (3 + name.len()) as u16;
        let ext_len = 2 + list_len;
        sni.extend(ext_len.to_be_bytes());
        sni.extend(list_len.to_be_bytes());
        sni.push(0);
        sni.extend((name.len() as u16).to_be_bytes());
        sni.extend(name);
        let mut body = vec![3, 3];
        body.extend([0u8; 32]);
        body.extend([0, 0, 2, 0x13, 0x01, 1, 0]);
        body.extend((sni.len() as u16).to_be_bytes());
        body.extend(&sni);
        let mut hs = vec![1, 0, (body.len() >> 8) as u8, body.len() as u8];
        hs.extend(body);
        let mut rec = vec![0x16, 3, 1, (hs.len() >> 8) as u8, hs.len() as u8];
        rec.extend(&hs);
        assert_eq!(tls_sni(&rec).as_deref(), Some("esempio.org"));
        for n in 0..rec.len() {
            let _ = tls_sni(&rec[..n]);
        }
        assert_eq!(tls_sni(b"GET / HTTP/1.1"), None);
    }
}
