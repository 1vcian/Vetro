//! Il gateway virtuale: smista i frame del guest e produce le risposte.

use std::collections::{BTreeMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};

use crate::events::{CloseReason, DhcpMessage, Direction, EventKind, EventLog, NetEvent};
use crate::tcp::{Ctx, Segment, State, TcpConn};
use crate::upstream::Upstream;
use crate::wire::{self, Mac, TCP_ACK, TCP_FIN, TCP_RST, TCP_SYN, TcpOut};
use crate::{ConnId, Flow, VirtualTime, dhcp, dns};

/// Configurazione della rete virtuale. I valori predefiniti sono quelli
/// della rete "user" di QEMU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetConfig {
    /// MAC del gateway (e del DNS virtuale).
    pub gateway_mac: Mac,
    pub gateway_ip: Ipv4Addr,
    pub dns_ip: Ipv4Addr,
    /// Indirizzo assegnato al guest via DHCP.
    pub guest_ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    /// MTU del collegamento (senza intestazione Ethernet).
    pub mtu: u16,
    pub lease_secs: u32,
    /// Seme dei numeri di sequenza iniziali TCP.
    pub seed: u64,
    /// Scarta i pacchetti con checksum IP/TCP/UDP/ICMP errato. Richiede che
    /// virtio-net non offra l'offload del checksum (`VIRTIO_NET_F_CSUM`) o
    /// che la piattaforma completi i checksum parziali prima di consegnarli.
    pub verify_checksums: bool,
    /// Inattività dopo cui un flusso UDP si chiude.
    pub udp_idle_timeout_us: u64,
    /// Attesa massima dell'upstream per aprire una connessione TCP.
    pub tcp_connect_timeout_us: u64,
}

impl Default for NetConfig {
    fn default() -> Self {
        NetConfig {
            gateway_mac: Mac([0x52, 0x55, 0x0a, 0x00, 0x02, 0x02]),
            gateway_ip: Ipv4Addr::new(10, 0, 2, 2),
            dns_ip: Ipv4Addr::new(10, 0, 2, 3),
            guest_ip: Ipv4Addr::new(10, 0, 2, 15),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            mtu: 1500,
            lease_secs: 86_400,
            seed: 0,
            verify_checksums: true,
            udp_idle_timeout_us: 60_000_000,
            tcp_connect_timeout_us: 75_000_000,
        }
    }
}

/// Contatori dei frame, per diagnosi. Non entrano nel registro eventi.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub frames_in: u64,
    pub frames_out: u64,
    /// Frame o pacchetti troncati o incoerenti.
    pub malformed: u64,
    pub bad_checksum: u64,
    /// Destinazione che non è il gateway né un indirizzo esterno.
    pub not_for_us: u64,
    /// IPv6 non è supportato (vedi docs/specs/net.md).
    pub ipv6: u64,
    /// Frammenti IPv4: non si riassemblano.
    pub fragments: u64,
    /// Protocolli o messaggi ignorati.
    pub ignored: u64,
    /// Risposte UDP troppo grandi per l'MTU o per flussi già chiusi.
    pub udp_dropped: u64,
    /// RST per segmenti che non appartengono a nessuna connessione.
    pub tcp_rst_unknown: u64,
}

struct UdpFlow {
    flow: Flow,
    last_activity: VirtualTime,
    bytes_to_remote: u64,
    bytes_to_guest: u64,
}

/// Lo stack di rete lato host, con il suo upstream.
///
/// Uso tipico dalla piattaforma: [`Stack::receive`] per ogni frame che il
/// guest trasmette, [`Stack::poll`] quando scade [`Stack::next_deadline`] o
/// quando l'upstream ha dati nuovi, e [`Stack::pop_frame`] per consegnare
/// al guest i frame prodotti.
pub struct Stack<U: Upstream> {
    config: NetConfig,
    upstream: U,
    log: EventLog,
    stats: Stats,
    guest_mac: Option<Mac>,
    out: VecDeque<Vec<u8>>,
    tcp: BTreeMap<ConnId, TcpConn>,
    tcp_index: BTreeMap<Flow, ConnId>,
    udp: BTreeMap<ConnId, UdpFlow>,
    udp_index: BTreeMap<Flow, ConnId>,
    next_id: ConnId,
    ip_ident: u16,
}

/// SplitMix64: funzione di mescolamento per derivare gli ISN dal seme.
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl<U: Upstream> Stack<U> {
    pub fn new(config: NetConfig, upstream: U) -> Self {
        Stack {
            config,
            upstream,
            log: EventLog::default(),
            stats: Stats::default(),
            guest_mac: None,
            out: VecDeque::new(),
            tcp: BTreeMap::new(),
            tcp_index: BTreeMap::new(),
            udp: BTreeMap::new(),
            udp_index: BTreeMap::new(),
            next_id: 1,
            ip_ident: 0,
        }
    }

    pub fn config(&self) -> &NetConfig {
        &self.config
    }

    pub fn upstream(&self) -> &U {
        &self.upstream
    }

    pub fn upstream_mut(&mut self) -> &mut U {
        &mut self.upstream
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// MAC del guest, imparato dal primo frame ricevuto.
    pub fn guest_mac(&self) -> Option<Mac> {
        self.guest_mac
    }

    /// Registro degli eventi dall'inizio (o dall'ultimo `take_events`).
    pub fn events(&self) -> &[NetEvent] {
        &self.log.events
    }

    /// Svuota e restituisce il registro degli eventi.
    pub fn take_events(&mut self) -> Vec<NetEvent> {
        std::mem::take(&mut self.log.events)
    }

    /// Connessioni TCP ancora vive (compresa TIME-WAIT).
    pub fn tcp_connections(&self) -> usize {
        self.tcp.len()
    }

    /// Flussi UDP ancora vivi.
    pub fn udp_flows(&self) -> usize {
        self.udp.len()
    }

    /// Prossimo frame per il guest.
    pub fn pop_frame(&mut self) -> Option<Vec<u8>> {
        self.out.pop_front()
    }

    /// Frame in attesa di essere consegnati al guest.
    pub fn pending_frames(&self) -> usize {
        self.out.len()
    }

    /// Primo istante in cui un timer interno scade (ritrasmissioni,
    /// TIME-WAIT, attese dell'upstream, flussi UDP inattivi). La piattaforma
    /// deve chiamare `poll` entro quell'istante; `None` se non ci sono timer.
    pub fn next_deadline(&self) -> Option<VirtualTime> {
        let tcp = self.tcp.values().filter_map(TcpConn::next_deadline);
        let udp = self.udp.values().map(|f| f.last_activity.after(self.config.udp_idle_timeout_us));
        tcp.chain(udp).min()
    }

    /// Frame trasmesso dal guest. Esegue anche un giro di [`Stack::poll`].
    pub fn receive(&mut self, now: VirtualTime, frame: &[u8]) {
        self.stats.frames_in += 1;
        self.handle_frame(now, frame);
        self.poll(now);
    }

    /// Fa avanzare timer e scambi con l'upstream fino a `now`.
    pub fn poll(&mut self, now: VirtualTime) {
        // TCP, in ordine di id: l'ordine dei frame prodotti è deterministico.
        let ids: Vec<ConnId> = self.tcp.keys().copied().collect();
        for id in ids {
            let mut segs = Vec::new();
            let Some(conn) = self.tcp.get_mut(&id) else { continue };
            let mut ctx = Ctx { now, up: &mut self.upstream, log: &mut self.log, out: &mut segs };
            conn.on_timer(&mut ctx);
            conn.process(&mut ctx);
            let flow = conn.flow;
            self.emit_segments(flow, segs);
            self.reap_tcp(id);
        }

        // Risposte UDP.
        while let Some((id, data)) = self.upstream.udp_recv(now) {
            self.udp_to_guest(now, id, &data);
        }

        // Flussi UDP scaduti.
        let timeout = self.config.udp_idle_timeout_us;
        let expired: Vec<ConnId> = self
            .udp
            .iter()
            .filter(|(_, f)| now >= f.last_activity.after(timeout))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(f) = self.udp.remove(&id) {
                self.udp_index.remove(&f.flow);
                self.log.push(
                    now,
                    EventKind::UdpClosed {
                        id,
                        reason: CloseReason::Idle,
                        bytes_to_remote: f.bytes_to_remote,
                        bytes_to_guest: f.bytes_to_guest,
                    },
                );
                self.upstream.udp_close(now, id);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Uscita
    // -----------------------------------------------------------------------

    fn send_eth(&mut self, dst: Mac, ethertype: u16, payload: &[u8]) {
        self.stats.frames_out += 1;
        self.out.push_back(wire::build_eth(dst, self.config.gateway_mac, ethertype, payload));
    }

    fn guest_dst_mac(&self) -> Mac {
        self.guest_mac.unwrap_or(Mac::BROADCAST)
    }

    fn send_ipv4(&mut self, src: Ipv4Addr, dst: Ipv4Addr, proto: u8, payload: &[u8]) {
        self.send_ipv4_to(self.guest_dst_mac(), src, dst, proto, payload);
    }

    fn send_ipv4_to(&mut self, mac: Mac, src: Ipv4Addr, dst: Ipv4Addr, proto: u8, payload: &[u8]) {
        self.ip_ident = self.ip_ident.wrapping_add(1);
        let packet = wire::build_ipv4(src, dst, proto, self.ip_ident, payload);
        self.send_eth(mac, wire::ETHERTYPE_IPV4, &packet);
    }

    fn emit_segments(&mut self, flow: Flow, segs: Vec<Segment>) {
        for s in segs {
            let tcp = wire::build_tcp(&TcpOut {
                src: *flow.remote.ip(),
                src_port: flow.remote.port(),
                dst: *flow.guest.ip(),
                dst_port: flow.guest.port(),
                seq: s.seq,
                ack: s.ack,
                flags: s.flags,
                window: s.window,
                mss: s.mss,
                payload: &s.payload,
            });
            self.send_ipv4(*flow.remote.ip(), *flow.guest.ip(), wire::PROTO_TCP, &tcp);
        }
    }

    fn reap_tcp(&mut self, id: ConnId) {
        if self.tcp.get(&id).is_some_and(TcpConn::is_closed)
            && let Some(conn) = self.tcp.remove(&id)
        {
            self.tcp_index.remove(&conn.flow);
        }
    }

    // -----------------------------------------------------------------------
    // Ingresso
    // -----------------------------------------------------------------------

    fn is_gateway_addr(&self, ip: Ipv4Addr) -> bool {
        ip == self.config.gateway_ip || ip == self.config.dns_ip
    }

    fn in_subnet(&self, ip: Ipv4Addr) -> bool {
        let m = u32::from(self.config.netmask);
        u32::from(ip) & m == u32::from(self.config.gateway_ip) & m
    }

    fn handle_frame(&mut self, now: VirtualTime, frame: &[u8]) {
        let Some((eth, payload)) = wire::parse_eth(frame) else {
            self.stats.malformed += 1;
            return;
        };
        if eth.dst != self.config.gateway_mac && !eth.dst.is_broadcast() {
            self.stats.not_for_us += 1;
            return;
        }
        if !eth.src.is_multicast() && eth.src != Mac::ZERO {
            self.guest_mac = Some(eth.src);
        }
        match eth.ethertype {
            wire::ETHERTYPE_ARP => self.handle_arp(payload),
            wire::ETHERTYPE_IPV4 => self.handle_ipv4(now, payload),
            wire::ETHERTYPE_IPV6 => self.stats.ipv6 += 1,
            _ => self.stats.ignored += 1,
        }
    }

    fn handle_arp(&mut self, payload: &[u8]) {
        let Some(arp) = wire::parse_arp(payload) else {
            self.stats.malformed += 1;
            return;
        };
        // Si risponde solo per i nostri indirizzi: mai per quello del guest,
        // altrimenti il suo controllo di conflitto (ARP probe) fallirebbe.
        if arp.op != wire::ARP_REQUEST || !self.is_gateway_addr(arp.target_ip) {
            self.stats.ignored += 1;
            return;
        }
        let reply = wire::build_arp(&wire::Arp {
            op: wire::ARP_REPLY,
            sender_mac: self.config.gateway_mac,
            sender_ip: arp.target_ip,
            target_mac: arp.sender_mac,
            target_ip: arp.sender_ip,
        });
        self.send_eth(arp.sender_mac, wire::ETHERTYPE_ARP, &reply);
    }

    fn handle_ipv4(&mut self, now: VirtualTime, packet: &[u8]) {
        let Some((ip, payload)) = wire::parse_ipv4(packet) else {
            self.stats.malformed += 1;
            return;
        };
        if self.config.verify_checksums && !ip.checksum_ok {
            self.stats.bad_checksum += 1;
            return;
        }
        if ip.fragment {
            self.stats.fragments += 1;
            return;
        }
        // DHCP arriva in broadcast (o al gateway) da una sorgente ancora nulla.
        if ip.proto == wire::PROTO_UDP
            && (ip.dst == Ipv4Addr::BROADCAST || ip.dst == self.config.gateway_ip)
            && let Some((udp, data)) = wire::parse_udp(ip.src, ip.dst, payload)
            && udp.dst_port == dhcp::SERVER_PORT
        {
            if self.config.verify_checksums && !udp.checksum_ok {
                self.stats.bad_checksum += 1;
                return;
            }
            self.handle_dhcp(now, data);
            return;
        }
        let external = !self.in_subnet(ip.dst) && !ip.dst.is_broadcast() && !ip.dst.is_multicast();
        if !external && !self.is_gateway_addr(ip.dst) {
            self.stats.not_for_us += 1;
            return;
        }
        match ip.proto {
            wire::PROTO_ICMP => self.handle_icmp(now, &ip, payload),
            wire::PROTO_UDP => self.handle_udp(now, &ip, packet, payload),
            wire::PROTO_TCP => self.handle_tcp(now, &ip, payload),
            _ => self.stats.ignored += 1,
        }
    }

    fn handle_icmp(&mut self, now: VirtualTime, ip: &wire::Ipv4Header, msg: &[u8]) {
        if msg.len() < 8 {
            self.stats.malformed += 1;
            return;
        }
        if self.config.verify_checksums && wire::checksum(msg) != 0 {
            self.stats.bad_checksum += 1;
            return;
        }
        if msg[0] != wire::ICMP_ECHO_REQUEST || msg[1] != 0 {
            self.stats.ignored += 1;
            return;
        }
        let answered = self.is_gateway_addr(ip.dst) || self.upstream.ping(now, ip.dst);
        self.log.push(now, EventKind::IcmpEcho { dst: ip.dst, answered });
        if answered {
            let reply =
                wire::build_icmp(wire::ICMP_ECHO_REPLY, 0, [msg[4], msg[5], msg[6], msg[7]], &msg[8..]);
            self.send_ipv4(ip.dst, ip.src, wire::PROTO_ICMP, &reply);
        }
    }

    fn handle_dhcp(&mut self, now: VirtualTime, data: &[u8]) {
        let Some(req) = dhcp::parse(data) else {
            self.stats.malformed += 1;
            return;
        };
        if req.chaddr != Mac::ZERO && !req.chaddr.is_multicast() {
            self.guest_mac = Some(req.chaddr);
        }
        let c = &self.config;
        let lease = dhcp::Lease {
            server: c.gateway_ip,
            client: c.guest_ip,
            netmask: c.netmask,
            router: c.gateway_ip,
            dns: c.dns_ip,
            lease_secs: c.lease_secs,
        };
        let client_event = match req.message_type {
            Some(dhcp::RELEASE) => Some(DhcpMessage::Release),
            Some(dhcp::DECLINE) => Some(DhcpMessage::Decline),
            _ => None,
        };
        if let Some(message) = client_event {
            let ip = req.requested_ip.unwrap_or(req.ciaddr);
            self.log.push(now, EventKind::Dhcp { message, mac: req.chaddr, ip, hostname: req.hostname });
            return;
        }
        let Some(reply) = dhcp::respond(&req, &lease) else {
            self.stats.ignored += 1;
            return;
        };
        let message = match reply.message_type {
            dhcp::OFFER => DhcpMessage::Offer,
            dhcp::ACK => DhcpMessage::Ack,
            _ => DhcpMessage::Nak,
        };
        self.log.push(
            now,
            EventKind::Dhcp { message, mac: req.chaddr, ip: reply.yiaddr, hostname: req.hostname },
        );
        let server = self.config.gateway_ip;
        let udp = wire::build_udp(server, dhcp::SERVER_PORT, reply.dst_ip, dhcp::CLIENT_PORT, &reply.packet);
        self.send_ipv4_to(reply.dst_mac, server, reply.dst_ip, wire::PROTO_UDP, &udp);
    }

    fn dns_endpoint(&self) -> SocketAddrV4 {
        SocketAddrV4::new(self.config.dns_ip, 53)
    }

    fn handle_udp(&mut self, now: VirtualTime, ip: &wire::Ipv4Header, packet: &[u8], segment: &[u8]) {
        let Some((udp, data)) = wire::parse_udp(ip.src, ip.dst, segment) else {
            self.stats.malformed += 1;
            return;
        };
        if self.config.verify_checksums && !udp.checksum_ok {
            self.stats.bad_checksum += 1;
            return;
        }
        let flow = Flow {
            guest: SocketAddrV4::new(ip.src, udp.src_port),
            remote: SocketAddrV4::new(ip.dst, udp.dst_port),
        };
        if self.is_gateway_addr(ip.dst) && flow.remote != self.dns_endpoint() {
            // Nessun servizio UDP qui: ICMP port unreachable con l'intestazione
            // IP originale e i primi 8 byte (RFC 792).
            let ihl = usize::from(packet[0] & 0x0f) * 4;
            let quoted = &packet[..(ihl + 8).min(packet.len())];
            let msg =
                wire::build_icmp(wire::ICMP_DEST_UNREACHABLE, wire::ICMP_PORT_UNREACHABLE, [0; 4], quoted);
            self.send_ipv4(ip.dst, ip.src, wire::PROTO_ICMP, &msg);
            return;
        }
        let id = match self.udp_index.get(&flow) {
            Some(&id) => id,
            None => {
                let id = self.next_id;
                self.next_id += 1;
                self.udp_index.insert(flow, id);
                self.udp
                    .insert(id, UdpFlow { flow, last_activity: now, bytes_to_remote: 0, bytes_to_guest: 0 });
                self.log.push(now, EventKind::UdpOpen { id, flow });
                id
            }
        };
        if let Some(f) = self.udp.get_mut(&id) {
            f.last_activity = now;
            f.bytes_to_remote += data.len() as u64;
        }
        self.log.push(now, EventKind::UdpData { id, dir: Direction::ToRemote, len: data.len() });
        if flow.remote == self.dns_endpoint()
            && let Some(q) = dns::parse_query(data)
        {
            self.log.push(now, EventKind::DnsQuery { id, txid: q.id, name: q.name, qtype: q.qtype });
        }
        self.upstream.udp_send(now, id, flow, data);
    }

    fn udp_to_guest(&mut self, now: VirtualTime, id: ConnId, data: &[u8]) {
        let max = usize::from(self.config.mtu) - wire::IPV4_HEADER_LEN - wire::UDP_HEADER_LEN;
        let dns_endpoint = self.dns_endpoint();
        let Some(f) = self.udp.get_mut(&id) else {
            self.stats.udp_dropped += 1;
            return;
        };
        if data.len() > max {
            self.stats.udp_dropped += 1;
            return;
        }
        f.last_activity = now;
        f.bytes_to_guest += data.len() as u64;
        let flow = f.flow;
        self.log.push(now, EventKind::UdpData { id, dir: Direction::ToGuest, len: data.len() });
        if flow.remote == dns_endpoint
            && let Some(r) = dns::parse_response(data)
        {
            self.log.push(
                now,
                EventKind::DnsAnswer {
                    id,
                    txid: r.id,
                    name: r.name,
                    qtype: r.qtype,
                    rcode: r.rcode,
                    addrs: r.addrs,
                },
            );
        }
        let udp =
            wire::build_udp(*flow.remote.ip(), flow.remote.port(), *flow.guest.ip(), flow.guest.port(), data);
        self.send_ipv4(*flow.remote.ip(), *flow.guest.ip(), wire::PROTO_UDP, &udp);
    }

    fn handle_tcp(&mut self, now: VirtualTime, ip: &wire::Ipv4Header, segment: &[u8]) {
        let Some((h, payload)) = wire::parse_tcp(ip.src, ip.dst, segment) else {
            self.stats.malformed += 1;
            return;
        };
        if self.config.verify_checksums && !h.checksum_ok {
            self.stats.bad_checksum += 1;
            return;
        }
        let flow = Flow {
            guest: SocketAddrV4::new(ip.src, h.src_port),
            remote: SocketAddrV4::new(ip.dst, h.dst_port),
        };
        let is_new_syn = h.has(TCP_SYN) && !h.has(TCP_ACK) && !h.has(TCP_RST);

        if let Some(&id) = self.tcp_index.get(&flow) {
            let mut segs = Vec::new();
            if let Some(conn) = self.tcp.get_mut(&id) {
                let mut ctx = Ctx { now, up: &mut self.upstream, log: &mut self.log, out: &mut segs };
                if conn.state == State::TimeWait && is_new_syn {
                    // Riapertura della stessa quadrupla: la vecchia si chiude.
                    conn.end_time_wait(&mut ctx);
                } else {
                    conn.input(&mut ctx, &h, payload);
                    conn.process(&mut ctx);
                }
            }
            self.emit_segments(flow, segs);
            self.reap_tcp(id);
            if self.tcp_index.contains_key(&flow) || !is_new_syn {
                return;
            }
        }

        if is_new_syn {
            let id = self.next_id;
            self.next_id += 1;
            let iss = splitmix64(self.config.seed ^ id.wrapping_mul(0x2545_f491_4f6c_dd1d)) as u32;
            let local_mss = self.config.mtu - (wire::IPV4_HEADER_LEN + wire::TCP_HEADER_LEN) as u16;
            let mut conn =
                TcpConn::new(id, flow, &h, iss, local_mss, now, self.config.tcp_connect_timeout_us);
            self.log.push(now, EventKind::TcpOpen { id, flow });
            self.upstream.tcp_open(now, id, flow);
            let mut segs = Vec::new();
            let mut ctx = Ctx { now, up: &mut self.upstream, log: &mut self.log, out: &mut segs };
            conn.process(&mut ctx);
            self.emit_segments(flow, segs);
            if !conn.is_closed() {
                self.tcp_index.insert(flow, id);
                self.tcp.insert(id, conn);
            }
            return;
        }

        if h.has(TCP_RST) {
            return;
        }
        // Nessuna connessione: RST (RFC 9293 3.10.7.1).
        self.stats.tcp_rst_unknown += 1;
        let (seq, ack, flags) = if h.has(TCP_ACK) {
            (h.ack, 0, TCP_RST)
        } else {
            let len = payload.len() as u32 + u32::from(h.has(TCP_SYN)) + u32::from(h.has(TCP_FIN));
            (0, h.seq.wrapping_add(len), TCP_RST | TCP_ACK)
        };
        self.emit_segments(
            flow,
            vec![Segment { seq, ack, flags, window: 0, mss: None, payload: Vec::new() }],
        );
    }
}
