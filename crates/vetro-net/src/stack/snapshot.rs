//! Stato dello stack negli snapshot della macchina (M6, ADR 0015): MAC del
//! guest, frame in uscita, connessioni TCP (sequenze, finestre, timer, dati
//! in transito), flussi UDP, indici, contatori, registro degli eventi e
//! stato dell'upstream, connessioni aperte dall'host (inoltro di porte:
//! code nei due versi, richieste in attesa). La configurazione (`NetConfig`) non si salva: è
//! quella con cui lo stack è stato costruito, e la macchina la controlla con
//! l'hash della sua configurazione.
//!
//! In un file a parte (figlio di `stack`, per vedere i campi privati) così
//! le modifiche allo stack e quelle al formato non si pestano i piedi: chi
//! aggiunge un campo allo stato deve aggiungerlo anche qui (e cambiare
//! `vetro_snapshot::FORMAT_VERSION`).

use std::net::{Ipv4Addr, SocketAddrV4};

use vetro_snapshot::{Error, Reader, Result, Snapshot, Writer};

use super::{Stack, Stats, UdpFlow};
use crate::events::{CloseReason, DhcpMessage, Direction, EventKind, NetEvent};
use crate::hostfwd::HostEnd;
use crate::tcp::TcpConn;
use crate::upstream::Upstream;
use crate::wire::Mac;
use crate::{Flow, VirtualTime};

// ---- Pezzi comuni (usati anche da tcp e sinkhole) ---------------------------

pub(crate) fn put_ip(w: &mut Writer, a: Ipv4Addr) {
    w.raw(&a.octets());
}

pub(crate) fn get_ip(r: &mut Reader<'_>) -> Result<Ipv4Addr> {
    let b: [u8; 4] = r.raw(4)?.try_into().expect("4 byte");
    Ok(Ipv4Addr::from(b))
}

pub(crate) fn put_sock(w: &mut Writer, s: SocketAddrV4) {
    put_ip(w, *s.ip());
    w.u16(s.port());
}

pub(crate) fn get_sock(r: &mut Reader<'_>) -> Result<SocketAddrV4> {
    Ok(SocketAddrV4::new(get_ip(r)?, r.u16()?))
}

pub(crate) fn put_flow(w: &mut Writer, f: &Flow) {
    put_sock(w, f.guest);
    put_sock(w, f.remote);
}

pub(crate) fn get_flow(r: &mut Reader<'_>) -> Result<Flow> {
    Ok(Flow { guest: get_sock(r)?, remote: get_sock(r)? })
}

pub(crate) fn put_time(w: &mut Writer, t: VirtualTime) {
    w.u64(t.0);
}

pub(crate) fn get_time(r: &mut Reader<'_>) -> Result<VirtualTime> {
    r.u64().map(VirtualTime)
}

pub(crate) fn put_opt_str(w: &mut Writer, s: Option<&str>) {
    w.opt(s, Writer::str);
}

pub(crate) fn get_opt_str(r: &mut Reader<'_>) -> Result<Option<String>> {
    r.opt(Reader::string)
}

fn put_dir(w: &mut Writer, d: Direction) {
    w.u8(match d {
        Direction::ToRemote => 0,
        Direction::ToGuest => 1,
    });
}

fn get_dir(r: &mut Reader<'_>) -> Result<Direction> {
    match r.u8()? {
        0 => Ok(Direction::ToRemote),
        1 => Ok(Direction::ToGuest),
        v => Err(Error::invalid(format!("verso {v}"))),
    }
}

pub(crate) fn put_reason(w: &mut Writer, c: CloseReason) {
    w.u8(match c {
        CloseReason::Normal => 0,
        CloseReason::GuestReset => 1,
        CloseReason::RemoteReset => 2,
        CloseReason::Refused => 3,
        CloseReason::Timeout => 4,
        CloseReason::Idle => 5,
    });
}

pub(crate) fn get_reason(r: &mut Reader<'_>) -> Result<CloseReason> {
    Ok(match r.u8()? {
        0 => CloseReason::Normal,
        1 => CloseReason::GuestReset,
        2 => CloseReason::RemoteReset,
        3 => CloseReason::Refused,
        4 => CloseReason::Timeout,
        5 => CloseReason::Idle,
        v => return Err(Error::invalid(format!("motivo di chiusura {v}"))),
    })
}

// ---- Registro degli eventi --------------------------------------------------

fn put_event(w: &mut Writer, e: &NetEvent) {
    put_time(w, e.at);
    match &e.kind {
        EventKind::Dhcp { message, mac, ip, hostname } => {
            w.u8(0);
            w.u8(match message {
                DhcpMessage::Offer => 0,
                DhcpMessage::Ack => 1,
                DhcpMessage::Nak => 2,
                DhcpMessage::Release => 3,
                DhcpMessage::Decline => 4,
            });
            w.raw(&mac.0);
            put_ip(w, *ip);
            put_opt_str(w, hostname.as_deref());
        }
        EventKind::IcmpEcho { dst, answered } => {
            w.u8(1);
            put_ip(w, *dst);
            w.bool(*answered);
        }
        EventKind::TcpOpen { id, flow } => {
            w.u8(2);
            w.u64(*id);
            put_flow(w, flow);
        }
        EventKind::TcpEstablished { id } => {
            w.u8(3);
            w.u64(*id);
        }
        EventKind::TcpData { id, dir, len } => {
            w.u8(4);
            w.u64(*id);
            put_dir(w, *dir);
            w.len_of(*len);
        }
        EventKind::TcpClosed { id, reason, bytes_to_remote, bytes_to_guest } => {
            w.u8(5);
            w.u64(*id);
            put_reason(w, *reason);
            w.u64(*bytes_to_remote);
            w.u64(*bytes_to_guest);
        }
        EventKind::UdpOpen { id, flow } => {
            w.u8(6);
            w.u64(*id);
            put_flow(w, flow);
        }
        EventKind::UdpData { id, dir, len } => {
            w.u8(7);
            w.u64(*id);
            put_dir(w, *dir);
            w.len_of(*len);
        }
        EventKind::UdpClosed { id, reason, bytes_to_remote, bytes_to_guest } => {
            w.u8(8);
            w.u64(*id);
            put_reason(w, *reason);
            w.u64(*bytes_to_remote);
            w.u64(*bytes_to_guest);
        }
        EventKind::DnsQuery { id, txid, name, qtype } => {
            w.u8(9);
            w.u64(*id);
            w.u16(*txid);
            w.str(name);
            w.u16(*qtype);
        }
        EventKind::DnsAnswer { id, txid, name, qtype, rcode, addrs } => {
            w.u8(10);
            w.u64(*id);
            w.u16(*txid);
            w.str(name);
            w.u16(*qtype);
            w.u8(*rcode);
            w.seq(addrs, |w, a| put_ip(w, *a));
        }
        EventKind::TcpConnect { id, flow } => {
            w.u8(11);
            w.u64(*id);
            put_flow(w, flow);
        }
    }
}

fn get_event(r: &mut Reader<'_>) -> Result<NetEvent> {
    let at = get_time(r)?;
    let kind = match r.u8()? {
        0 => EventKind::Dhcp {
            message: match r.u8()? {
                0 => DhcpMessage::Offer,
                1 => DhcpMessage::Ack,
                2 => DhcpMessage::Nak,
                3 => DhcpMessage::Release,
                4 => DhcpMessage::Decline,
                v => return Err(Error::invalid(format!("messaggio DHCP {v}"))),
            },
            mac: Mac(r.raw(6)?.try_into().expect("6 byte")),
            ip: get_ip(r)?,
            hostname: get_opt_str(r)?,
        },
        1 => EventKind::IcmpEcho { dst: get_ip(r)?, answered: r.bool()? },
        2 => EventKind::TcpOpen { id: r.u64()?, flow: get_flow(r)? },
        3 => EventKind::TcpEstablished { id: r.u64()? },
        4 => EventKind::TcpData { id: r.u64()?, dir: get_dir(r)?, len: r.u64()? as usize },
        5 => EventKind::TcpClosed {
            id: r.u64()?,
            reason: get_reason(r)?,
            bytes_to_remote: r.u64()?,
            bytes_to_guest: r.u64()?,
        },
        6 => EventKind::UdpOpen { id: r.u64()?, flow: get_flow(r)? },
        7 => EventKind::UdpData { id: r.u64()?, dir: get_dir(r)?, len: r.u64()? as usize },
        8 => EventKind::UdpClosed {
            id: r.u64()?,
            reason: get_reason(r)?,
            bytes_to_remote: r.u64()?,
            bytes_to_guest: r.u64()?,
        },
        9 => EventKind::DnsQuery { id: r.u64()?, txid: r.u16()?, name: r.string()?, qtype: r.u16()? },
        10 => EventKind::DnsAnswer {
            id: r.u64()?,
            txid: r.u16()?,
            name: r.string()?,
            qtype: r.u16()?,
            rcode: r.u8()?,
            addrs: r.seq(4, get_ip)?,
        },
        11 => EventKind::TcpConnect { id: r.u64()?, flow: get_flow(r)? },
        v => return Err(Error::invalid(format!("evento di rete {v}"))),
    };
    Ok(NetEvent { at, kind })
}

// ---- Stack ------------------------------------------------------------------

fn stats_fields(s: &mut Stats) -> [&mut u64; 10] {
    [
        &mut s.frames_in,
        &mut s.frames_out,
        &mut s.malformed,
        &mut s.bad_checksum,
        &mut s.not_for_us,
        &mut s.ipv6,
        &mut s.fragments,
        &mut s.ignored,
        &mut s.udp_dropped,
        &mut s.tcp_rst_unknown,
    ]
}

impl<U: Upstream + Snapshot> Snapshot for Stack<U> {
    fn save(&self, w: &mut Writer) {
        w.opt(self.guest_mac, |w, m| w.raw(&m.0));
        w.seq(&self.out, |w, f| w.bytes(f));
        w.seq(&self.tcp, |w, (&id, c)| {
            w.u64(id);
            c.save(w);
        });
        w.seq(&self.tcp_index, |w, (f, &id)| {
            put_flow(w, f);
            w.u64(id);
        });
        w.seq(&self.udp, |w, (&id, u)| {
            w.u64(id);
            put_flow(w, &u.flow);
            put_time(w, u.last_activity);
            w.u64(u.bytes_to_remote);
            w.u64(u.bytes_to_guest);
        });
        w.seq(&self.udp_index, |w, (f, &id)| {
            put_flow(w, f);
            w.u64(id);
        });
        w.u64(self.next_id);
        w.u16(self.ip_ident);
        let mut stats = self.stats.clone();
        for v in stats_fields(&mut stats) {
            w.u64(*v);
        }
        w.seq(&self.log.events, put_event);
        w.section(b"UPST", |w| self.upstream.save(w));
        w.section(b"HFWD", |w| {
            w.u16(self.host.next_port);
            w.seq(&self.host.conns, |w, (&id, h)| {
                w.u64(id);
                put_flow(w, &h.flow);
                w.bool(h.pending_open);
                let (a, b) = h.to_guest.as_slices();
                w.bytes(&[a, b].concat());
                let (a, b) = h.from_guest.as_slices();
                w.bytes(&[a, b].concat());
                w.bool(h.shutdown);
                w.bool(h.abort);
                w.bool(h.guest_fin);
                w.opt(h.closed, put_reason);
                w.bool(h.released);
            });
        });
    }

    fn restore(&mut self, r: &mut Reader<'_>) -> Result<()> {
        self.guest_mac = r.opt(|r| Ok(Mac(r.raw(6)?.try_into().expect("6 byte"))))?;
        self.out = r.seq(8, Reader::vec)?.into();
        let n = r.len_of(8)?;
        self.tcp.clear();
        for _ in 0..n {
            let id = r.u64()?;
            let c = TcpConn::restore(r)?;
            if c.id != id {
                return Err(Error::invalid(format!("connessione {} sotto la chiave {id}", c.id)));
            }
            self.tcp.insert(id, c);
        }
        self.tcp_index = r.seq(20, |r| Ok((get_flow(r)?, r.u64()?)))?.into_iter().collect();
        let n = r.len_of(36)?;
        self.udp.clear();
        for _ in 0..n {
            let id = r.u64()?;
            let flow = UdpFlow {
                flow: get_flow(r)?,
                last_activity: get_time(r)?,
                bytes_to_remote: r.u64()?,
                bytes_to_guest: r.u64()?,
            };
            self.udp.insert(id, flow);
        }
        self.udp_index = r.seq(20, |r| Ok((get_flow(r)?, r.u64()?)))?.into_iter().collect();
        self.next_id = r.u64()?;
        self.ip_ident = r.u16()?;
        for v in stats_fields(&mut self.stats) {
            *v = r.u64()?;
        }
        self.log.events = r.seq(9, get_event)?;
        let mut up = r.section(b"UPST")?;
        self.upstream.restore(&mut up)?;
        up.finish()?;
        let mut h = r.section(b"HFWD")?;
        self.host.next_port = h.u16()?;
        let conns = h.seq(42, |r| {
            let id = r.u64()?;
            let end = HostEnd {
                flow: get_flow(r)?,
                pending_open: r.bool()?,
                to_guest: r.vec()?.into(),
                from_guest: r.vec()?.into(),
                shutdown: r.bool()?,
                abort: r.bool()?,
                guest_fin: r.bool()?,
                closed: r.opt(get_reason)?,
                released: r.bool()?,
            };
            Ok((id, end))
        })?;
        self.host.conns = conns.into_iter().collect();
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventKind;
    use crate::sinkhole::{Sinkhole, SinkholeConfig};
    use crate::stack::NetConfig;
    use crate::wire;

    /// Un giro di DHCP e una connessione TCP a metà: lo stack salvato e
    /// ripristinato in uno nuovo dà gli stessi byte e, dagli stessi frame,
    /// le stesse risposte e lo stesso registro di quello originale.
    #[test]
    fn stack_ripristinato_prosegue_uguale() {
        let cfg = NetConfig { seed: 7, ..NetConfig::default() };
        let mut a = Stack::new(cfg.clone(), Sinkhole::new(SinkholeConfig::default()));
        let guest = Mac([0x52, 0x54, 0, 0x12, 0x34, 0x56]);
        let t0 = VirtualTime::from_millis(5);
        // SYN del guest verso 198.18.0.9:80 (con l'MSS), senza DHCP: lo
        // stack impara il MAC dal frame.
        let remote = Ipv4Addr::new(198, 18, 0, 9);
        let seg = wire::build_tcp(&wire::TcpOut {
            src: cfg.guest_ip,
            src_port: 40000,
            dst: remote,
            dst_port: 80,
            seq: 1000,
            ack: 0,
            flags: wire::TCP_SYN,
            window: 64240,
            mss: Some(1460),
            payload: b"",
        });
        let ip = wire::build_ipv4(cfg.guest_ip, remote, wire::PROTO_TCP, 1, &seg);
        let syn = wire::build_eth(cfg.gateway_mac, guest, wire::ETHERTYPE_IPV4, &ip);
        a.receive(t0, &syn);
        assert!(a.pending_frames() > 0, "SYN-ACK in uscita");
        assert_eq!(a.tcp_connections(), 1);
        assert!(matches!(a.events()[0].kind, EventKind::TcpOpen { .. }));

        let mut w = Writer::new();
        a.save(&mut w);
        let bytes = w.into_bytes();
        let mut b = Stack::new(cfg, Sinkhole::new(SinkholeConfig::default()));
        let mut r = Reader::new(&bytes);
        b.restore(&mut r).unwrap();
        r.finish().unwrap();
        let mut w2 = Writer::new();
        b.save(&mut w2);
        assert_eq!(w2.into_bytes(), bytes);

        // Da qui in poi i due stack vanno di pari passo, timer compresi.
        assert_eq!(a.next_deadline(), b.next_deadline());
        let later = VirtualTime::from_secs(3);
        a.poll(later);
        b.poll(later);
        let (mut fa, mut fb) = (Vec::new(), Vec::new());
        while let Some(f) = a.pop_frame() {
            fa.push(f);
        }
        while let Some(f) = b.pop_frame() {
            fb.push(f);
        }
        assert!(fa.len() >= 2, "SYN-ACK e ritrasmissione");
        assert_eq!(fa, fb);
        assert_eq!(a.events(), b.events());
    }
}
