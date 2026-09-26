//! Testo in chiaro di TLS dagli hook (M7): le conversazioni catturate agli
//! hook di `SSL_write`/`SSL_read` diventano richieste HTTP decodificate e
//! si uniscono a quelle in chiaro nell'ispettore e nell'HAR.
//!
//! La cattura di rete (ADR 0016) di una connessione TLS è cifrata: qui il
//! testo in chiaro non viene dai frame ma dagli hook, con la connessione
//! (4-tupla), il processo e la libreria già risolti dalla macchina
//! (`SSL*` -> fd -> socket). Ogni conversazione è una connessione: i
//! blocchi verso il server (`SSL_write`) sono la richiesta, quelli dal
//! server (`SSL_read`) la risposta.

use std::net::SocketAddrV4;

use super::body;
use super::http;
use super::inspector::{Attribution, HttpExchange, Timings};

/// Un blocco di testo in chiaro di una connessione TLS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsMessage {
    /// Tempo del guest (microsecondi), come i frame della cattura.
    pub at_us: u64,
    /// Verso il server (`SSL_write`) o dal server (`SSL_read`).
    pub to_server: bool,
    pub data: Vec<u8>,
}

/// Una connessione TLS in chiaro, con attribuzione.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsConversation {
    pub client: SocketAddrV4,
    pub server: SocketAddrV4,
    /// Nome del server (SNI o Host), se noto.
    pub host: Option<String>,
    pub pid: i32,
    pub tid: i32,
    pub process: String,
    pub package: Option<String>,
    /// `libssl` di sistema o Conscrypt (APEX).
    pub library: String,
    pub messages: Vec<TlsMessage>,
}

/// Un verso ricostruito: byte e, per ogni offset, l'istante in cui è
/// arrivato (come `flow::Stream`).
#[derive(Default)]
struct Side {
    bytes: Vec<u8>,
    marks: Vec<(usize, u64)>,
}

impl Side {
    fn push(&mut self, at: u64, data: &[u8]) {
        if self.marks.last().map(|m| m.1) != Some(at) {
            self.marks.push((self.bytes.len(), at));
        }
        self.bytes.extend_from_slice(data);
    }

    fn time_at(&self, offset: usize) -> u64 {
        match self.marks.partition_point(|&(o, _)| o <= offset) {
            0 => self.marks.first().map_or(0, |m| m.1),
            i => self.marks[i - 1].1,
        }
    }
}

impl TlsConversation {
    fn sides(&self) -> (Side, Side) {
        let (mut req, mut resp) = (Side::default(), Side::default());
        for m in &self.messages {
            if m.to_server {
                req.push(m.at_us, &m.data);
            } else {
                resp.push(m.at_us, &m.data);
            }
        }
        (req, resp)
    }

    /// Le richieste HTTP di questa connessione, come [`HttpExchange`]
    /// (`secure`), pronte per l'ispettore.
    pub fn exchanges(&self, flow: usize) -> Vec<HttpExchange> {
        let (req, resp) = self.sides();
        if !http::looks_like_request(&req.bytes) {
            return Vec::new();
        }
        let reqs = http::parse_requests(&req.bytes);
        let methods: Vec<&str> = reqs.iter().map(|r| r.method.as_str()).collect();
        let mut resps = http::parse_responses(&resp.bytes, &methods, true).into_iter();
        let mut out = Vec::new();
        for (i, request) in reqs.into_iter().enumerate() {
            let response = resps.next();
            let host = request
                .headers
                .get("host")
                .map(str::to_string)
                .or_else(|| self.host.clone())
                .unwrap_or_else(|| self.server.to_string());
            let url = if request.target.starts_with("http") {
                request.target.clone()
            } else {
                format!("https://{host}{}", request.target)
            };
            let req_start = req.time_at(request.start);
            let req_end = req.time_at(request.end.saturating_sub(1).max(request.start));
            let mut t = Timings {
                started_us: req_start,
                send_us: req_end.saturating_sub(req_start),
                ..Timings::default()
            };
            if let Some(r) = &response {
                let rs = resp.time_at(r.start);
                let re = resp.time_at(r.end.saturating_sub(1).max(r.start));
                t.wait_us = rs.saturating_sub(req_end);
                t.receive_us = re.saturating_sub(rs.max(req_end));
            }
            let request_body = body::decode(request.headers.get("content-type"), &request.body.decoded);
            let response_body =
                response.as_ref().map(|r| body::decode(r.headers.get("content-type"), &r.body.decoded));
            out.push(HttpExchange {
                index: 0,
                flow,
                client: self.client,
                server: self.server,
                resolved_name: self.host.clone(),
                url,
                request,
                response,
                request_body,
                response_body,
                timings: t,
                first_on_connection: i == 0,
                secure: true,
                attribution: Some(Attribution {
                    pid: self.pid,
                    tid: self.tid,
                    process: self.process.clone(),
                    package: self.package.clone(),
                    library: self.library.clone(),
                }),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::NetworkAnalysis;

    fn conv() -> TlsConversation {
        TlsConversation {
            client: "10.0.2.15:44000".parse().unwrap(),
            server: "198.18.0.5:443".parse().unwrap(),
            host: Some("api.esempio.test".into()),
            pid: 4242,
            tid: 4250,
            process: "com.vetro.probe".into(),
            package: Some("com.vetro.probe".into()),
            library: "libssl (sistema)".into(),
            messages: vec![
                TlsMessage {
                    at_us: 1000,
                    to_server: true,
                    data: b"POST /v1/eventi HTTP/1.1\r\nHost: api.esempio.test\r\nContent-Type: application/json\r\nContent-Length: 22\r\n\r\n{\"ciao\":\"mondo\",\"n\":1}".to_vec(),
                },
                TlsMessage {
                    at_us: 2000,
                    to_server: false,
                    data: b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\n\r\n{\"ok\":true}\n".to_vec(),
                },
            ],
        }
    }

    #[test]
    fn richiesta_https_in_chiaro() {
        let x = conv().exchanges(7);
        assert_eq!(x.len(), 1);
        let e = &x[0];
        assert!(e.secure);
        assert_eq!(e.url, "https://api.esempio.test/v1/eventi");
        assert_eq!(e.request.method, "POST");
        assert_eq!(e.request.body.decoded, b"{\"ciao\":\"mondo\",\"n\":1}");
        assert_eq!(e.status(), Some(200));
        let a = e.attribution.as_ref().unwrap();
        assert_eq!((a.pid, a.package.as_deref()), (4242, Some("com.vetro.probe")));
        assert!(e.timings.wait_us > 0);
    }

    #[test]
    fn si_unisce_alle_richieste_in_chiaro() {
        let mut a = NetworkAnalysis::from_frames(&[]);
        a.merge_tls(&[conv()]);
        assert_eq!(a.http.len(), 1);
        assert_eq!(a.requests()[0].url, "https://api.esempio.test/v1/eventi");
        let har = a.to_har(&super::super::har::HarOptions::default());
        assert!(har.contains("https://api.esempio.test/v1/eventi"), "{har}");
        assert!(har.contains("com.vetro.probe"));
    }
}
