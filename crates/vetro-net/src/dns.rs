//! DNS messages (RFC 1035): what is needed to record queries and answers
//! and to answer from the sinkhole. IN class only; of the records it extracts the A ones.

use std::net::Ipv4Addr;

pub const TYPE_A: u16 = 1;
pub const TYPE_CNAME: u16 = 5;
pub const TYPE_AAAA: u16 = 28;
pub const CLASS_IN: u16 = 1;

pub const RCODE_NOERROR: u8 = 0;
pub const RCODE_SERVFAIL: u8 = 2;
pub const RCODE_NXDOMAIN: u8 = 3;
pub const RCODE_NOTIMP: u8 = 4;

const HEADER_LEN: usize = 12;
/// Limit on the compression pointers followed: avoids loops.
const MAX_JUMPS: usize = 16;

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

/// Reads a name starting at `at`. Returns the name in lower case, with the
/// dots, without a final dot ("" for the root), and the position right after
/// the name in the message (after the first pointer, if any).
fn read_name(msg: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut end = None;
    let mut jumps = 0;
    loop {
        let len = *msg.get(at)?;
        match len >> 6 {
            0b00 => {
                if len == 0 {
                    return Some((name, end.unwrap_or(at + 1)));
                }
                let label = msg.get(at + 1..at + 1 + usize::from(len))?;
                if !name.is_empty() {
                    name.push('.');
                }
                for &b in label {
                    // Non-printable bytes become \DDD as in zone files.
                    let c = b.to_ascii_lowercase();
                    if c.is_ascii_graphic() && c != b'.' && c != b'\\' {
                        name.push(char::from(c));
                    } else {
                        name.push_str(&format!("\\{c:03}"));
                    }
                }
                if name.len() > 1024 {
                    return None;
                }
                at += 1 + usize::from(len);
            }
            0b11 => {
                let ptr = usize::from(be16(msg, at)? & 0x3fff);
                end.get_or_insert(at + 2);
                jumps += 1;
                if jumps > MAX_JUMPS {
                    return None;
                }
                at = ptr;
            }
            _ => return None,
        }
    }
}

/// DNS question (first question of the message).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub id: u16,
    /// RD bit of the request.
    pub recursion_desired: bool,
    pub opcode: u8,
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
    /// Bytes of the question section (name, type, class), to copy into the
    /// answer.
    pub question: Vec<u8>,
}

/// Parses a query (QR = 0, at least one question).
pub fn parse_query(msg: &[u8]) -> Option<Query> {
    if msg.len() < HEADER_LEN {
        return None;
    }
    let flags = be16(msg, 2)?;
    if flags & 0x8000 != 0 || be16(msg, 4)? == 0 {
        return None;
    }
    let (name, after) = read_name(msg, HEADER_LEN)?;
    let qtype = be16(msg, after)?;
    let qclass = be16(msg, after + 2)?;
    Some(Query {
        id: be16(msg, 0)?,
        recursion_desired: flags & 0x0100 != 0,
        opcode: ((flags >> 11) & 0xf) as u8,
        name,
        qtype,
        qclass,
        question: msg[HEADER_LEN..after + 4].to_vec(),
    })
}

/// Parsed DNS answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub id: u16,
    pub rcode: u8,
    pub name: String,
    pub qtype: u16,
    /// Addresses of the A records of the answer section, in order.
    pub addrs: Vec<Ipv4Addr>,
}

/// Parses an answer (QR = 1).
pub fn parse_response(msg: &[u8]) -> Option<Response> {
    if msg.len() < HEADER_LEN {
        return None;
    }
    let flags = be16(msg, 2)?;
    if flags & 0x8000 == 0 {
        return None;
    }
    let qd = be16(msg, 4)?;
    let an = be16(msg, 6)?;
    let mut at = HEADER_LEN;
    let (mut name, mut qtype) = (String::new(), 0);
    for i in 0..qd {
        let (n, after) = read_name(msg, at)?;
        if i == 0 {
            name = n;
            qtype = be16(msg, after)?;
        }
        at = after + 4;
    }
    let mut addrs = Vec::new();
    for _ in 0..an {
        let (_, after) = read_name(msg, at)?;
        let ty = be16(msg, after)?;
        let class = be16(msg, after + 2)?;
        let rdlen = usize::from(be16(msg, after + 8)?);
        let rdata = msg.get(after + 10..after + 10 + rdlen)?;
        if ty == TYPE_A && class == CLASS_IN && rdlen == 4 {
            addrs.push(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]));
        }
        at = after + 10 + rdlen;
    }
    Some(Response { id: be16(msg, 0)?, rcode: (flags & 0xf) as u8, name, qtype, addrs })
}

/// Builds the answer to `q`: A records for `addrs` (with TTL `ttl`) and
/// code `rcode`. The name in the records is a pointer to the question.
pub fn build_response(q: &Query, rcode: u8, addrs: &[Ipv4Addr], ttl: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(HEADER_LEN + q.question.len() + addrs.len() * 16);
    m.extend_from_slice(&q.id.to_be_bytes());
    // QR=1, opcode copied, AA=1, RD copied, RA=1.
    let flags: u16 = 0x8000
        | (u16::from(q.opcode) << 11)
        | 0x0400
        | if q.recursion_desired { 0x0100 } else { 0 }
        | 0x0080
        | u16::from(rcode & 0xf);
    m.extend_from_slice(&flags.to_be_bytes());
    m.extend_from_slice(&1u16.to_be_bytes());
    m.extend_from_slice(&(addrs.len() as u16).to_be_bytes());
    m.extend_from_slice(&[0, 0, 0, 0]);
    m.extend_from_slice(&q.question);
    for a in addrs {
        m.extend_from_slice(&0xc00cu16.to_be_bytes());
        m.extend_from_slice(&TYPE_A.to_be_bytes());
        m.extend_from_slice(&CLASS_IN.to_be_bytes());
        m.extend_from_slice(&ttl.to_be_bytes());
        m.extend_from_slice(&4u16.to_be_bytes());
        m.extend_from_slice(&a.octets());
    }
    m
}

/// Builds a query (used by the tests and the fake clients).
pub fn build_query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
    m.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.split('.').filter(|l| !l.is_empty()) {
        m.push(label.len() as u8);
        m.extend_from_slice(label.as_bytes());
    }
    m.push(0);
    m.extend_from_slice(&qtype.to_be_bytes());
    m.extend_from_slice(&CLASS_IN.to_be_bytes());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_response_roundtrip() {
        let raw = build_query(0x1234, "Example.COM", TYPE_A);
        let q = parse_query(&raw).unwrap();
        assert_eq!((q.id, q.name.as_str(), q.qtype, q.qclass), (0x1234, "example.com", TYPE_A, CLASS_IN));
        let addr = Ipv4Addr::new(198, 18, 0, 1);
        let resp = build_response(&q, RCODE_NOERROR, &[addr], 60);
        let r = parse_response(&resp).unwrap();
        assert_eq!((r.id, r.rcode, r.name.as_str(), r.qtype), (0x1234, 0, "example.com", TYPE_A));
        assert_eq!(r.addrs, vec![addr]);
        assert!(parse_query(&resp).is_none());
    }

    #[test]
    fn compression_loop_is_rejected() {
        let mut m = build_query(1, "a", TYPE_A);
        // Replaces the name with a pointer to itself.
        m.truncate(HEADER_LEN);
        m.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1]);
        assert!(parse_query(&m).is_none());
        for n in 0..m.len() {
            let _ = parse_response(&m[..n]);
        }
    }
}
