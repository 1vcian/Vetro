//! The network gateway's protocol (ADR 0048, `docs/specs/relay.md`): the
//! frames that carry the guest's TCP connections and DNS queries between the
//! machine and the gateway (`relay/`), which opens the real connections.
//!
//! Browsers cannot open raw sockets, so the page's Worker forwards these
//! frames over a WebSocket; the gateway is a Cloudflare Worker or the Node
//! implementation of the same protocol. This module has only the frames and
//! their encoding; the state that uses them is [`Uplink`](crate::Uplink).
//!
//! One frame = one WebSocket binary message: `op` (1 byte), `id` (u32 big
//! endian), payload. The same bytes are what `Machine::input` records for
//! replay. The JS implementation (`relay/src/protocol.mjs`) is checked
//! against the same vectors (`relay/test/vectors.json`).

use std::net::{Ipv4Addr, SocketAddrV4};

/// Protocol version announced in the gateway's `hello`.
pub const VERSION: u32 = 1;
/// Flow control window of each connection, per direction: bytes sent and
/// not yet acknowledged with `ACK` never exceed it.
pub const WINDOW: u32 = 256 * 1024;
/// Largest `DATA` payload.
pub const MAX_DATA: usize = 16 * 1024;
/// Largest DNS query or answer carried by a `DNS` frame.
pub const MAX_DNS: usize = 4096;
/// Longest host name of `CONNECT_NAME`.
pub const MAX_NAME: usize = 253;
/// Header bytes: op and id.
pub const HEADER: usize = 5;

/// Why the gateway (or the Worker on its behalf) refused a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Refusal {
    /// The destination refused, did not answer or is unreachable.
    Unreachable = 1,
    /// Port not allowed (only 80 and 443).
    Port = 2,
    /// Address not allowed (private, loopback, link-local, reserved...).
    Address = 3,
    /// A session limit (connections, bytes) was reached.
    Limit = 4,
    /// The platform refused the socket (e.g. Cloudflare's own ranges).
    Platform = 5,
    /// The name did not resolve.
    Resolve = 6,
    /// The gateway is not reachable or the session ended.
    Down = 7,
}

impl Refusal {
    pub fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            1 => Self::Unreachable,
            2 => Self::Port,
            3 => Self::Address,
            4 => Self::Limit,
            5 => Self::Platform,
            6 => Self::Resolve,
            7 => Self::Down,
            _ => return None,
        })
    }
}

/// A protocol frame. `id`s are the machine's connection ids (`ConnId`, which
/// fit in 32 bits in practice: the `Uplink` refuses the others) or, on a
/// lane, the ids the Worker gives them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    // ---- machine → gateway
    /// Open a TCP connection to an IPv4 address.
    Connect {
        id: u32,
        dst: SocketAddrV4,
    },
    /// Open a TCP connection to a name (resolved and checked by the gateway).
    ConnectName {
        id: u32,
        name: String,
        port: u16,
    },
    /// Bytes of the connection (both directions).
    Data {
        id: u32,
        data: Vec<u8>,
    },
    /// The sender closed its direction (FIN, both directions).
    Shutdown {
        id: u32,
    },
    /// The connection is over (both directions); `reset` if aborted.
    Close {
        id: u32,
        reset: bool,
    },
    /// The receiver consumed `n` more bytes of `DATA` (both directions).
    Ack {
        id: u32,
        n: u32,
    },
    /// A raw DNS query (machine → gateway) or its answer (gateway → machine).
    Dns {
        id: u32,
        msg: Vec<u8>,
        answer: bool,
    },
    // ---- gateway → machine
    Connected {
        id: u32,
    },
    Refused {
        id: u32,
        reason: Refusal,
    },
    // ---- host-local (the Worker to the machine, never on the gateway's wire)
    /// Online (gateway) or offline (sinkhole) for new connections.
    Online(bool),
    /// The gateway is gone: every connection through it is refused or reset.
    Down,
}

/// Operation codes.
pub mod op {
    pub const CONNECT: u8 = 0x01;
    pub const CONNECT_NAME: u8 = 0x02;
    pub const DATA: u8 = 0x03;
    pub const SHUTDOWN: u8 = 0x04;
    pub const CLOSE: u8 = 0x05;
    pub const ACK: u8 = 0x06;
    pub const DNS: u8 = 0x07;
    pub const CONNECTED: u8 = 0x81;
    pub const REFUSED: u8 = 0x82;
    pub const DATA_IN: u8 = 0x83;
    pub const SHUTDOWN_IN: u8 = 0x84;
    pub const CLOSE_IN: u8 = 0x85;
    pub const ACK_IN: u8 = 0x86;
    pub const DNS_ANSWER: u8 = 0x87;
    pub const ONLINE: u8 = 0xf0;
    pub const DOWN: u8 = 0xf1;
}

/// Who sends the frame: `DATA`, `SHUTDOWN`, `CLOSE` and `ACK` exist in both
/// directions, with different codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// From the machine (the guest) to the gateway.
    Machine,
    /// From the gateway (or the host) to the machine.
    Gateway,
}

impl Frame {
    /// Encodes the frame as sent by `side` (`Data`, `Shutdown`, `Close` and
    /// `Ack` have one code per direction).
    pub fn encode_from(&self, side: Side) -> Vec<u8> {
        let g = side == Side::Gateway;
        let pick = |m: u8, gw: u8| if g { gw } else { m };
        let (o, id, payload): (u8, u32, Vec<u8>) = match self {
            Frame::Connect { id, dst } => {
                let mut p = dst.ip().octets().to_vec();
                p.extend(dst.port().to_be_bytes());
                (op::CONNECT, *id, p)
            }
            Frame::ConnectName { id, name, port } => {
                let mut p = port.to_be_bytes().to_vec();
                p.extend(name.as_bytes());
                (op::CONNECT_NAME, *id, p)
            }
            Frame::Data { id, data } => (pick(op::DATA, op::DATA_IN), *id, data.clone()),
            Frame::Shutdown { id } => (pick(op::SHUTDOWN, op::SHUTDOWN_IN), *id, Vec::new()),
            Frame::Close { id, reset } => (pick(op::CLOSE, op::CLOSE_IN), *id, vec![*reset as u8]),
            Frame::Ack { id, n } => (pick(op::ACK, op::ACK_IN), *id, n.to_be_bytes().to_vec()),
            Frame::Dns { id, msg, answer } => {
                (if *answer { op::DNS_ANSWER } else { op::DNS }, *id, msg.clone())
            }
            Frame::Connected { id } => (op::CONNECTED, *id, Vec::new()),
            Frame::Refused { id, reason } => (op::REFUSED, *id, vec![*reason as u8]),
            Frame::Online(on) => (op::ONLINE, 0, vec![*on as u8]),
            Frame::Down => (op::DOWN, 0, Vec::new()),
        };
        let mut out = Vec::with_capacity(HEADER + payload.len());
        out.push(o);
        out.extend(id.to_be_bytes());
        out.extend(payload);
        out
    }

    /// Decodes one frame; `None` if it is malformed (wrong length, unknown
    /// code, payload over the limits, invalid name).
    pub fn decode(b: &[u8]) -> Option<(Side, Frame)> {
        if b.len() < HEADER {
            return None;
        }
        let o = b[0];
        let id = u32::from_be_bytes([b[1], b[2], b[3], b[4]]);
        let p = &b[HEADER..];
        let side = if o & 0x80 != 0 { Side::Gateway } else { Side::Machine };
        let empty = |f: Frame| p.is_empty().then_some(f);
        let flag = || match p {
            [0] => Some(false),
            [1] => Some(true),
            _ => None,
        };
        let frame = match o {
            op::CONNECT => {
                let [a, b2, c, d, ph, pl] = p else { return None };
                Frame::Connect {
                    id,
                    dst: SocketAddrV4::new(Ipv4Addr::new(*a, *b2, *c, *d), u16::from_be_bytes([*ph, *pl])),
                }
            }
            op::CONNECT_NAME => {
                if p.len() < 3 {
                    return None;
                }
                let port = u16::from_be_bytes([p[0], p[1]]);
                let name = valid_name(&p[2..])?;
                Frame::ConnectName { id, name, port }
            }
            op::DATA | op::DATA_IN => {
                if p.is_empty() || p.len() > MAX_DATA {
                    return None;
                }
                Frame::Data { id, data: p.to_vec() }
            }
            op::SHUTDOWN | op::SHUTDOWN_IN => empty(Frame::Shutdown { id })?,
            op::CLOSE | op::CLOSE_IN => Frame::Close { id, reset: flag()? },
            op::ACK | op::ACK_IN => {
                let [a, b2, c, d] = p else { return None };
                Frame::Ack { id, n: u32::from_be_bytes([*a, *b2, *c, *d]) }
            }
            op::DNS | op::DNS_ANSWER => {
                if p.len() < 12 || p.len() > MAX_DNS {
                    return None;
                }
                Frame::Dns { id, msg: p.to_vec(), answer: o == op::DNS_ANSWER }
            }
            op::CONNECTED => empty(Frame::Connected { id })?,
            op::REFUSED => {
                let [c] = p else { return None };
                Frame::Refused { id, reason: Refusal::from_code(*c)? }
            }
            op::ONLINE if id == 0 => Frame::Online(flag()?),
            op::DOWN if id == 0 => empty(Frame::Down)?,
            _ => return None,
        };
        Some((side, frame))
    }

    /// The frame's connection id (0 for the host-local ones).
    pub fn id(&self) -> u32 {
        match self {
            Frame::Connect { id, .. }
            | Frame::ConnectName { id, .. }
            | Frame::Data { id, .. }
            | Frame::Shutdown { id }
            | Frame::Close { id, .. }
            | Frame::Ack { id, .. }
            | Frame::Dns { id, .. }
            | Frame::Connected { id }
            | Frame::Refused { id, .. } => *id,
            Frame::Online(_) | Frame::Down => 0,
        }
    }
}

/// A host name as `CONNECT_NAME` carries it: 1-253 bytes of lowercase
/// letters, digits, `-` and `.`, no empty labels.
fn valid_name(b: &[u8]) -> Option<String> {
    if b.is_empty() || b.len() > MAX_NAME {
        return None;
    }
    let ok = b.split(|&c| c == b'.').all(|l| {
        !l.is_empty()
            && l.len() <= 63
            && l.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
    });
    ok.then(|| String::from_utf8(b.to_vec()).ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_rejects() {
        let frames = [
            (
                Side::Machine,
                Frame::Connect { id: 7, dst: SocketAddrV4::new(Ipv4Addr::new(93, 184, 215, 14), 443) },
            ),
            (Side::Machine, Frame::ConnectName { id: 8, name: "example.com".into(), port: 80 }),
            (Side::Machine, Frame::Data { id: 7, data: b"hello".to_vec() }),
            (Side::Gateway, Frame::Data { id: 7, data: vec![0; MAX_DATA] }),
            (Side::Machine, Frame::Shutdown { id: 7 }),
            (Side::Gateway, Frame::Close { id: 7, reset: true }),
            (Side::Gateway, Frame::Ack { id: 7, n: WINDOW }),
            (Side::Gateway, Frame::Refused { id: 9, reason: Refusal::Platform }),
            (Side::Gateway, Frame::Online(true)),
            (Side::Gateway, Frame::Down),
        ];
        for (side, f) in frames {
            assert_eq!(Frame::decode(&f.encode_from(side)), Some((side, f.clone())), "{f:?}");
        }
        for bad in [
            &[][..],
            &[op::DATA, 0, 0, 0, 1][..],                      // empty data
            &[op::CLOSE, 0, 0, 0, 1, 2][..],                  // bad flag
            &[op::REFUSED, 0, 0, 0, 1, 9][..],                // unknown reason
            &[op::CONNECT_NAME, 0, 0, 0, 1, 0, 80, b'A'][..], // uppercase
            &[op::CONNECT_NAME, 0, 0, 0, 1, 0, 80, b'a', b'.', b'.', b'b'][..],
            &[op::ONLINE, 0, 0, 0, 1, 1][..], // host-local with an id
            &[0x42, 0, 0, 0, 1][..],
        ] {
            assert_eq!(Frame::decode(bad), None, "{bad:?}");
        }
        let mut big = vec![op::DATA, 0, 0, 0, 1];
        big.resize(HEADER + MAX_DATA + 1, 0);
        assert_eq!(Frame::decode(&big), None);
    }
}
