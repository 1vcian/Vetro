//! The machine's uplink: where the guest's connections go. Offline, the
//! [`Sinkhole`] (everything fake and recorded); online, the network gateway
//! (ADR 0048), through the frames of [`relay`](crate::relay).
//!
//! The uplink is pure state, without I/O, so the machine stays deterministic
//! (ADR 0019):
//! - what goes to the gateway becomes frames in an outbox that the host
//!   drains ([`Uplink::take_outbox`]): an output, recomputed in replay and
//!   left out of snapshots;
//! - what comes from the gateway enters with [`Uplink::input`], which the
//!   machine calls only from `Machine::input` (a recorded input);
//! - flow control is state: the guest's bytes are accepted only within the
//!   credit the gateway has acknowledged, and the gateway's bytes are
//!   acknowledged when the stack has taken them ([`WINDOW`] each way).
//!
//! The mode decides only for new connections and UDP flows: those opened
//! offline stay with the sinkhole until they end. Online, DNS queries go to
//! the gateway (except AAAA, answered here with no records: the guest has no
//! IPv6), other UDP is dropped, ICMP to the outside is not answered, and a
//! connection to one of the sinkhole's fake addresses (left in the guest's
//! DNS cache by an offline snapshot) goes out by name.

use std::collections::{BTreeMap, VecDeque};
use std::net::Ipv4Addr;

use vetro_snapshot::{Reader, Result, Snapshot, Writer};

use crate::relay::{Frame, MAX_DATA, Refusal, WINDOW};
use crate::upstream::{TcpRead, TcpStatus, Upstream};
use crate::{ConnId, Flow, Sinkhole, VirtualTime, dns};

/// Acknowledge the gateway's bytes at least every this many consumed.
const ACK_EVERY: u32 = 32 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Conn {
    /// `None` while waiting for `CONNECTED`/`REFUSED`.
    status: Option<std::result::Result<(), Refusal>>,
    /// Guest bytes the gateway can still take.
    credit: u32,
    /// Gateway bytes not yet taken by the stack.
    inbox: VecDeque<u8>,
    /// Taken by the stack and not yet acknowledged to the gateway.
    unacked: u32,
    eof: bool,
    reset: bool,
}

/// Counters of the online side (not part of the guest's view).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UplinkStats {
    /// Connections sent to the gateway.
    pub connects: u64,
    /// Of which refused (by the gateway or for `DOWN`).
    pub refused: u64,
    /// DNS queries sent to the gateway.
    pub dns_queries: u64,
    /// UDP datagrams dropped online (not DNS).
    pub udp_dropped: u64,
    /// Gateway frames that did not apply (unknown id, protocol violation).
    pub ignored: u64,
}

/// [`Upstream`] of the machine's stack: sinkhole or gateway.
#[derive(Debug)]
pub struct Uplink {
    sinkhole: Sinkhole,
    online: bool,
    /// TCP connections through the gateway.
    tcp: BTreeMap<ConnId, Conn>,
    /// UDP flows: true if they belong to the gateway.
    udp: BTreeMap<ConnId, bool>,
    /// Answers for gateway flows (DNS from the gateway, local AAAA).
    udp_in: VecDeque<(ConnId, Vec<u8>)>,
    stats: UplinkStats,
    /// Frames for the gateway, not yet taken by the host. Not in snapshots.
    outbox: Vec<Frame>,
}

impl Uplink {
    /// Offline, with the sinkhole.
    pub fn new(sinkhole: Sinkhole) -> Self {
        Uplink {
            sinkhole,
            online: false,
            tcp: BTreeMap::new(),
            udp: BTreeMap::new(),
            udp_in: VecDeque::new(),
            stats: UplinkStats::default(),
            outbox: Vec::new(),
        }
    }

    pub fn sinkhole(&self) -> &Sinkhole {
        &self.sinkhole
    }

    pub fn sinkhole_mut(&mut self) -> &mut Sinkhole {
        &mut self.sinkhole
    }

    pub fn online(&self) -> bool {
        self.online
    }

    pub fn stats(&self) -> UplinkStats {
        self.stats
    }

    /// Connections currently through the gateway.
    pub fn gateway_connections(&self) -> usize {
        self.tcp.len()
    }

    /// The frames for the gateway produced since the last call, in order.
    /// Taking them does not change what the guest sees.
    pub fn take_outbox(&mut self) -> Vec<Frame> {
        std::mem::take(&mut self.outbox)
    }

    /// Frames not yet taken.
    pub fn outbox_len(&self) -> usize {
        self.outbox.len()
    }

    /// Applies a frame from the gateway or from the host (`ONLINE`, `DOWN`).
    /// Returns false if it did not apply (machine-to-gateway frame, unknown
    /// connection, protocol violation): nothing changed for the guest except,
    /// for a violation, the reset of that connection.
    pub fn input(&mut self, frame: Frame) -> bool {
        let applied = self.apply(frame);
        if !applied {
            self.stats.ignored += 1;
        }
        applied
    }

    fn apply(&mut self, frame: Frame) -> bool {
        match frame {
            Frame::Online(on) => self.online = on,
            Frame::Down => {
                for c in self.tcp.values_mut() {
                    match c.status {
                        None => {
                            c.status = Some(Err(Refusal::Down));
                            self.stats.refused += 1;
                        }
                        Some(Ok(())) => c.reset = true,
                        Some(Err(_)) => {}
                    }
                }
            }
            Frame::Connected { id } => match self.conn(id) {
                Some(c) if c.status.is_none() => c.status = Some(Ok(())),
                _ => return false,
            },
            Frame::Refused { id, reason } => match self.conn(id) {
                Some(c) if c.status.is_none() => {
                    c.status = Some(Err(reason));
                    self.stats.refused += 1;
                }
                _ => return false,
            },
            Frame::Data { id, data } => {
                let Some(c) = self.conn(id) else { return false };
                if c.status != Some(Ok(())) || c.eof {
                    return false;
                }
                // The gateway must stay within the window: beyond it, the
                // connection is reset (the gateway is broken, or not ours).
                if c.inbox.len() + c.unacked as usize + data.len() > WINDOW as usize {
                    c.reset = true;
                    c.eof = true;
                    return false;
                }
                c.inbox.extend(data);
            }
            Frame::Shutdown { id } => match self.conn(id) {
                Some(c) if c.status == Some(Ok(())) => c.eof = true,
                _ => return false,
            },
            Frame::Close { id, reset } => {
                let Some(c) = self.conn(id) else { return false };
                match c.status {
                    None => {
                        c.status = Some(Err(Refusal::Unreachable));
                        self.stats.refused += 1;
                    }
                    Some(Ok(())) => {
                        c.eof = true;
                        c.reset |= reset;
                    }
                    Some(Err(_)) => return false,
                }
            }
            Frame::Ack { id, n } => {
                let Some(c) = self.conn(id) else { return false };
                let credit = u64::from(c.credit) + u64::from(n);
                if credit > u64::from(WINDOW) {
                    return false;
                }
                c.credit = credit as u32;
            }
            Frame::Dns { id, msg, answer: true } => {
                if self.udp.get(&u64::from(id)) != Some(&true) {
                    return false;
                }
                self.udp_in.push_back((u64::from(id), msg));
            }
            Frame::Dns { answer: false, .. } | Frame::Connect { .. } | Frame::ConnectName { .. } => {
                return false;
            }
        }
        true
    }

    fn conn(&mut self, id: u32) -> Option<&mut Conn> {
        self.tcp.get_mut(&u64::from(id))
    }

    fn send(&mut self, f: Frame) {
        self.outbox.push(f);
    }
}

/// The frames' 32-bit id (the `Uplink` refuses connections beyond it).
fn wire_id(id: ConnId) -> u32 {
    id as u32
}

impl Upstream for Uplink {
    fn tcp_open(&mut self, now: VirtualTime, id: ConnId, flow: Flow) {
        if !self.online {
            return self.sinkhole.tcp_open(now, id, flow);
        }
        let mut c = Conn { credit: WINDOW, ..Conn::default() };
        if id > u64::from(u32::MAX) {
            c.status = Some(Err(Refusal::Limit));
            self.stats.refused += 1;
        } else {
            let ip = *flow.remote.ip();
            let frame = match self.sinkhole.hostname(ip) {
                // A fake address left in the guest's DNS cache: by name.
                Some(name) if ip.octets()[0] == 198 && (ip.octets()[1] & 0xfe) == 18 => {
                    Frame::ConnectName { id: wire_id(id), name: name.to_owned(), port: flow.remote.port() }
                }
                _ => Frame::Connect { id: wire_id(id), dst: flow.remote },
            };
            self.stats.connects += 1;
            self.send(frame);
        }
        self.tcp.insert(id, c);
    }

    fn tcp_status(&mut self, now: VirtualTime, id: ConnId) -> TcpStatus {
        match self.tcp.get(&id) {
            Some(c) => match c.status {
                None => TcpStatus::Pending,
                Some(Ok(())) => TcpStatus::Connected,
                Some(Err(_)) => TcpStatus::Refused,
            },
            None => self.sinkhole.tcp_status(now, id),
        }
    }

    fn tcp_write(&mut self, now: VirtualTime, id: ConnId, data: &[u8]) -> usize {
        let Some(c) = self.tcp.get_mut(&id) else { return self.sinkhole.tcp_write(now, id, data) };
        if c.reset {
            // Nobody to deliver to: the next read resets the connection.
            return data.len();
        }
        let n = data.len().min(c.credit as usize);
        c.credit -= n as u32;
        for chunk in data[..n].chunks(MAX_DATA) {
            self.outbox.push(Frame::Data { id: wire_id(id), data: chunk.to_vec() });
        }
        n
    }

    fn tcp_read(&mut self, now: VirtualTime, id: ConnId, buf: &mut [u8]) -> TcpRead {
        let Some(c) = self.tcp.get_mut(&id) else { return self.sinkhole.tcp_read(now, id, buf) };
        if c.reset {
            return TcpRead::Reset;
        }
        let n = buf.len().min(c.inbox.len());
        for (dst, src) in buf.iter_mut().zip(c.inbox.drain(..n)) {
            *dst = src;
        }
        c.unacked += n as u32;
        if c.unacked >= ACK_EVERY || (c.inbox.is_empty() && c.unacked > 0 && !c.eof) {
            let n = std::mem::take(&mut c.unacked);
            self.outbox.push(Frame::Ack { id: wire_id(id), n });
        }
        let Some(c) = self.tcp.get(&id) else { return TcpRead::Reset };
        match n {
            0 if c.eof => TcpRead::Eof,
            0 => TcpRead::WouldBlock,
            n => TcpRead::Data(n),
        }
    }

    fn tcp_shutdown(&mut self, now: VirtualTime, id: ConnId) {
        match self.tcp.get(&id) {
            Some(c) => {
                if c.status == Some(Ok(())) && !c.reset {
                    self.send(Frame::Shutdown { id: wire_id(id) });
                }
            }
            None => self.sinkhole.tcp_shutdown(now, id),
        }
    }

    fn tcp_close(&mut self, now: VirtualTime, id: ConnId, reset: bool) {
        match self.tcp.remove(&id) {
            // A refused connection is already gone at the gateway.
            Some(c) => {
                if c.status == Some(Ok(())) {
                    self.send(Frame::Close { id: wire_id(id), reset });
                } else if c.status.is_none() {
                    // The guest gave up before the answer.
                    self.send(Frame::Close { id: wire_id(id), reset: true });
                }
            }
            None => self.sinkhole.tcp_close(now, id, reset),
        }
    }

    fn udp_send(&mut self, now: VirtualTime, id: ConnId, flow: Flow, data: &[u8]) {
        let relay = *self.udp.entry(id).or_insert(self.online);
        if !relay {
            return self.sinkhole.udp_send(now, id, flow, data);
        }
        let query = (flow.remote.port() == 53).then(|| dns::parse_query(data)).flatten();
        match query {
            Some(q) if q.qtype == dns::TYPE_AAAA => {
                self.udp_in.push_back((id, dns::build_response(&q, dns::RCODE_NOERROR, &[], 300)));
            }
            Some(_) if id <= u64::from(u32::MAX) => {
                self.stats.dns_queries += 1;
                self.send(Frame::Dns { id: wire_id(id), msg: data.to_vec(), answer: false });
            }
            _ => self.stats.udp_dropped += 1,
        }
    }

    fn udp_recv(&mut self, now: VirtualTime) -> Option<(ConnId, Vec<u8>)> {
        self.sinkhole.udp_recv(now).or_else(|| self.udp_in.pop_front())
    }

    fn udp_close(&mut self, now: VirtualTime, id: ConnId) {
        if self.udp.remove(&id) == Some(false) {
            self.sinkhole.udp_close(now, id);
        }
    }

    fn ping(&mut self, now: VirtualTime, dst: Ipv4Addr) -> bool {
        !self.online && self.sinkhole.ping(now, dst)
    }
}

/// Snapshot: the sinkhole as before, then the online state in an optional
/// `GWAY` section (absent in snapshots made before ADR 0048: offline, empty).
/// The outbox is an output and is not saved.
impl Snapshot for Uplink {
    fn save(&self, w: &mut Writer) {
        self.sinkhole.save(w);
        w.section(b"GWAY", |w| {
            w.bool(self.online);
            w.seq(&self.tcp, |w, (id, c)| {
                w.u64(*id);
                w.opt(c.status, |w, s| {
                    w.u8(match s {
                        Ok(()) => 0,
                        Err(r) => r as u8,
                    })
                });
                w.u32(c.credit);
                let (a, b) = c.inbox.as_slices();
                w.len_of(a.len() + b.len());
                w.raw(a);
                w.raw(b);
                w.u32(c.unacked);
                w.bool(c.eof);
                w.bool(c.reset);
            });
            w.seq(&self.udp, |w, (id, relay)| {
                w.u64(*id);
                w.bool(*relay);
            });
            w.seq(&self.udp_in, |w, (id, d)| {
                w.u64(*id);
                w.bytes(d);
            });
            let s = &self.stats;
            for v in [s.connects, s.refused, s.dns_queries, s.udp_dropped, s.ignored] {
                w.u64(v);
            }
        });
    }

    fn restore(&mut self, r: &mut Reader<'_>) -> Result<()> {
        self.sinkhole.restore(r)?;
        let fresh = Uplink::new(Sinkhole::new(self.sinkhole.config().clone()));
        self.online = fresh.online;
        self.tcp = fresh.tcp;
        self.udp = fresh.udp;
        self.udp_in = fresh.udp_in;
        self.stats = fresh.stats;
        self.outbox.clear();
        if r.peek_tag() != Some(*b"GWAY") {
            return Ok(());
        }
        let mut g = r.section(b"GWAY")?;
        self.online = g.bool()?;
        let tcp = g.seq(8 + 1 + 4 + 8 + 4 + 2, |r| {
            let id = r.u64()?;
            let status = r.opt(|r| match r.u8()? {
                0 => Ok(Ok(())),
                c => Refusal::from_code(c).map(Err).ok_or_else(|| vetro_snapshot::Error::invalid("refusal")),
            })?;
            let credit = r.u32()?;
            let inbox = r.vec()?.into();
            Ok((id, Conn { status, credit, inbox, unacked: r.u32()?, eof: r.bool()?, reset: r.bool()? }))
        })?;
        self.tcp = tcp.into_iter().collect();
        self.udp = g.seq(9, |r| Ok((r.u64()?, r.bool()?)))?.into_iter().collect();
        self.udp_in = g.seq(16, |r| Ok((r.u64()?, r.vec()?)))?.into();
        self.stats = UplinkStats {
            connects: g.u64()?,
            refused: g.u64()?,
            dns_queries: g.u64()?,
            udp_dropped: g.u64()?,
            ignored: g.u64()?,
        };
        g.finish()
    }
}
