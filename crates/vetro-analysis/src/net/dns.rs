//! Messaggi DNS (RFC 1035) e scambi domanda/risposta ricostruiti dai
//! flussi UDP verso la porta 53 (il DNS del gateway, cioè il sinkhole).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4};

use super::flow::UdpFlow;

/// Dati di un record di risposta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordData {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    /// CNAME, NS, PTR: un nome.
    Name(String),
    Other(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub rtype: u16,
    pub ttl: u32,
    pub data: RecordData,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: u16,
    pub response: bool,
    pub opcode: u8,
    pub rcode: u8,
    /// (nome in minuscolo, tipo).
    pub questions: Vec<(String, u16)>,
    pub answers: Vec<Record>,
}

/// Nome del tipo di record, per la stampa.
pub fn type_name(t: u16) -> String {
    match t {
        1 => "A".into(),
        2 => "NS".into(),
        5 => "CNAME".into(),
        6 => "SOA".into(),
        12 => "PTR".into(),
        15 => "MX".into(),
        16 => "TXT".into(),
        28 => "AAAA".into(),
        33 => "SRV".into(),
        65 => "HTTPS".into(),
        t => format!("TYPE{t}"),
    }
}

fn name(msg: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut end = None;
    let mut jumps = 0;
    loop {
        let len = *msg.get(pos)?;
        match len >> 6 {
            0 if len == 0 => {
                return Some((out, end.unwrap_or(pos + 1)));
            }
            0 => {
                let label = msg.get(pos + 1..pos + 1 + usize::from(len))?;
                if !out.is_empty() {
                    out.push('.');
                }
                for &b in label {
                    if b.is_ascii_graphic() && b != b'.' && b != b'\\' {
                        out.push(b.to_ascii_lowercase() as char);
                    } else {
                        out.push_str(&format!("\\{b:03}"));
                    }
                }
                pos += 1 + usize::from(len);
            }
            3 => {
                let ptr = usize::from(u16::from_be_bytes([len, *msg.get(pos + 1)?]) & 0x3fff);
                end.get_or_insert(pos + 2);
                jumps += 1;
                if jumps > 32 {
                    return None;
                }
                pos = ptr;
            }
            _ => return None,
        }
        if out.len() > 1024 {
            return None;
        }
    }
}

/// Decodifica un messaggio DNS; `None` se malformato.
pub fn parse(msg: &[u8]) -> Option<Message> {
    if msg.len() < 12 {
        return None;
    }
    let be = |i: usize| u16::from_be_bytes([msg[i], msg[i + 1]]);
    let (qd, an) = (be(4), be(6));
    let mut m = Message {
        id: be(0),
        response: msg[2] & 0x80 != 0,
        opcode: (msg[2] >> 3) & 0x0f,
        rcode: msg[3] & 0x0f,
        questions: Vec::new(),
        answers: Vec::new(),
    };
    let mut pos = 12;
    for _ in 0..qd {
        let (n, p) = name(msg, pos)?;
        let t = u16::from_be_bytes([*msg.get(p)?, *msg.get(p + 1)?]);
        msg.get(p + 3)?;
        m.questions.push((n, t));
        pos = p + 4;
    }
    for _ in 0..an {
        let (n, p) = name(msg, pos)?;
        let h = msg.get(p..p + 10)?;
        let rtype = u16::from_be_bytes([h[0], h[1]]);
        let ttl = u32::from_be_bytes([h[4], h[5], h[6], h[7]]);
        let len = usize::from(u16::from_be_bytes([h[8], h[9]]));
        let data = msg.get(p + 10..p + 10 + len)?;
        let data = match (rtype, len) {
            (1, 4) => RecordData::A(Ipv4Addr::new(data[0], data[1], data[2], data[3])),
            (28, 16) => RecordData::Aaaa(Ipv6Addr::from(<[u8; 16]>::try_from(data).ok()?)),
            (2 | 5 | 12, _) => RecordData::Name(name(msg, p + 10)?.0),
            _ => RecordData::Other(data.to_vec()),
        };
        m.answers.push(Record { name: n, rtype, ttl, data });
        pos = p + 10 + len;
    }
    Some(m)
}

/// Una domanda DNS e la sua risposta (se arrivata).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsExchange {
    /// Indice del flusso UDP.
    pub flow: usize,
    pub client: SocketAddrV4,
    pub server: SocketAddrV4,
    pub id: u16,
    pub name: String,
    pub qtype: u16,
    pub query_at: u64,
    pub answer_at: Option<u64>,
    pub rcode: Option<u8>,
    pub answers: Vec<Record>,
}

impl DnsExchange {
    /// Indirizzi IPv4 della risposta.
    pub fn ipv4(&self) -> impl Iterator<Item = Ipv4Addr> + '_ {
        self.answers.iter().filter_map(|r| match r.data {
            RecordData::A(a) => Some(a),
            _ => None,
        })
    }
}

/// Scambi DNS nei flussi UDP con porta 53 (dal lato del server), in ordine
/// di domanda. Una risposta si abbina all'ultima domanda senza risposta con
/// lo stesso id nello stesso flusso.
pub fn exchanges(flows: &[UdpFlow]) -> Vec<DnsExchange> {
    let mut out: Vec<DnsExchange> = Vec::new();
    for f in flows.iter().filter(|f| f.server.port() == 53) {
        for d in &f.datagrams {
            let Some(m) = parse(&d.data) else { continue };
            if d.from_client && !m.response {
                let Some((name, qtype)) = m.questions.first().cloned() else { continue };
                out.push(DnsExchange {
                    flow: f.index,
                    client: f.client,
                    server: f.server,
                    id: m.id,
                    name,
                    qtype,
                    query_at: d.at,
                    answer_at: None,
                    rcode: None,
                    answers: Vec::new(),
                });
            } else if !d.from_client
                && m.response
                && let Some(x) =
                    out.iter_mut().rev().find(|x| x.flow == f.index && x.id == m.id && x.answer_at.is_none())
            {
                x.answer_at = Some(d.at);
                x.rcode = Some(m.rcode);
                x.answers = m.answers;
            }
        }
    }
    out.sort_by_key(|x| (x.query_at, x.flow));
    out
}

/// Costruttori di messaggi per i test.
#[cfg(test)]
pub mod build {
    pub fn query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut m = id.to_be_bytes().to_vec();
        m.extend([1, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        for l in name.split('.') {
            m.push(l.len() as u8);
            m.extend(l.as_bytes());
        }
        m.push(0);
        m.extend(qtype.to_be_bytes());
        m.extend([0, 1]);
        m
    }

    /// Risposta con un record A per ogni indirizzo (nome compresso).
    pub fn answer(id: u16, name: &str, addrs: &[[u8; 4]]) -> Vec<u8> {
        let mut m = query(id, name, 1);
        m[2] = 0x81;
        m[3] = 0x80;
        m[7] = addrs.len() as u8;
        for a in addrs {
            m.extend([0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
            m.extend(a);
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::flow::Datagram;

    #[test]
    fn domanda_e_risposta() {
        let q = parse(&build::query(0x1234, "Vetro.Example", 1)).unwrap();
        assert_eq!(
            (q.id, q.response, q.questions.clone()),
            (0x1234, false, vec![("vetro.example".into(), 1)])
        );
        let a = parse(&build::answer(0x1234, "vetro.example", &[[198, 18, 0, 1], [198, 18, 0, 2]])).unwrap();
        assert!(a.response);
        assert_eq!(a.rcode, 0);
        assert_eq!(
            a.answers.iter().map(|r| (r.name.as_str(), r.ttl, r.data.clone())).collect::<Vec<_>>(),
            [
                ("vetro.example", 60, RecordData::A(Ipv4Addr::new(198, 18, 0, 1))),
                ("vetro.example", 60, RecordData::A(Ipv4Addr::new(198, 18, 0, 2)))
            ]
        );
    }

    #[test]
    fn puntatori_in_ciclo_e_troncati() {
        let mut m = build::query(1, "a", 1);
        m[12] = 0xc0;
        m[13] = 12; // punta a sé stesso
        assert_eq!(parse(&m), None);
        let full = build::answer(1, "abc.de", &[[1, 2, 3, 4]]);
        for n in 0..full.len() {
            assert_eq!(parse(&full[..n]), None, "{n}");
        }
    }

    #[test]
    fn scambi_nei_flussi() {
        let flow = UdpFlow {
            index: 3,
            client: "10.0.2.15:5000".parse().unwrap(),
            server: "10.0.2.3:53".parse().unwrap(),
            datagrams: vec![
                Datagram { at: 10, from_client: true, data: build::query(7, "x.example", 1) },
                Datagram { at: 11, from_client: true, data: build::query(8, "x.example", 28) },
                Datagram {
                    at: 20,
                    from_client: false,
                    data: build::answer(7, "x.example", &[[198, 18, 0, 5]]),
                },
            ],
        };
        let x = exchanges(&[flow]);
        assert_eq!(x.len(), 2);
        assert_eq!((x[0].name.as_str(), x[0].qtype, x[0].answer_at), ("x.example", 1, Some(20)));
        assert_eq!(x[0].ipv4().collect::<Vec<_>>(), [Ipv4Addr::new(198, 18, 0, 5)]);
        assert_eq!((x[1].qtype, x[1].answer_at), (28, None));
        assert_eq!(type_name(28), "AAAA");
    }
}
