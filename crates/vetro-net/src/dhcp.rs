//! Server DHCP (RFC 2131/2132) per un solo guest, come la rete user di QEMU.
//!
//! Qualsiasi client riceve lo stesso indirizzo (`NetConfig::guest_ip`): la
//! macchina virtuale ha una sola scheda di rete.

use std::net::Ipv4Addr;

use crate::wire::Mac;

pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;

pub const DISCOVER: u8 = 1;
pub const OFFER: u8 = 2;
pub const REQUEST: u8 = 3;
pub const DECLINE: u8 = 4;
pub const ACK: u8 = 5;
pub const NAK: u8 = 6;
pub const RELEASE: u8 = 7;
pub const INFORM: u8 = 8;

const MAGIC: [u8; 4] = [99, 130, 83, 99];
const FIXED_LEN: usize = 236;
/// Molti client scartano risposte BOOTP più corte di 300 byte.
const MIN_REPLY_LEN: usize = 300;

pub const OPT_SUBNET_MASK: u8 = 1;
pub const OPT_ROUTER: u8 = 3;
pub const OPT_DNS: u8 = 6;
pub const OPT_HOSTNAME: u8 = 12;
pub const OPT_REQUESTED_IP: u8 = 50;
pub const OPT_LEASE_TIME: u8 = 51;
pub const OPT_MESSAGE_TYPE: u8 = 53;
pub const OPT_SERVER_ID: u8 = 54;
pub const OPT_RENEWAL_TIME: u8 = 58;
pub const OPT_REBINDING_TIME: u8 = 59;
pub const OPT_END: u8 = 255;

/// Messaggio DHCP analizzato.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// 1 = BOOTREQUEST, 2 = BOOTREPLY.
    pub op: u8,
    pub xid: u32,
    pub flags: u16,
    pub ciaddr: Ipv4Addr,
    pub yiaddr: Ipv4Addr,
    pub chaddr: Mac,
    pub message_type: Option<u8>,
    pub requested_ip: Option<Ipv4Addr>,
    pub server_id: Option<Ipv4Addr>,
    pub hostname: Option<String>,
    /// Tutte le opzioni, in ordine: (codice, valore).
    pub options: Vec<(u8, Vec<u8>)>,
}

impl Packet {
    pub fn option(&self, code: u8) -> Option<&[u8]> {
        self.options.iter().find(|(c, _)| *c == code).map(|(_, v)| v.as_slice())
    }

    pub fn broadcast_flag(&self) -> bool {
        self.flags & 0x8000 != 0
    }
}

fn ip4(v: &[u8]) -> Option<Ipv4Addr> {
    (v.len() == 4).then(|| Ipv4Addr::new(v[0], v[1], v[2], v[3]))
}

pub fn parse(p: &[u8]) -> Option<Packet> {
    // htype Ethernet, hlen 6, cookie magico.
    if p.len() < FIXED_LEN + 4 || p[1] != 1 || p[2] != 6 || p[FIXED_LEN..FIXED_LEN + 4] != MAGIC {
        return None;
    }
    let mut chaddr = [0; 6];
    chaddr.copy_from_slice(&p[28..34]);
    let mut options = Vec::new();
    let mut rest = &p[FIXED_LEN + 4..];
    while let [code, tail @ ..] = rest {
        match *code {
            0 => rest = tail,
            OPT_END => break,
            c => {
                let (&len, tail) = tail.split_first()?;
                let len = usize::from(len);
                if len > tail.len() {
                    return None;
                }
                options.push((c, tail[..len].to_vec()));
                rest = &tail[len..];
            }
        }
    }
    let find = |code: u8| options.iter().find(|(c, _)| *c == code).map(|(_, v)| v.as_slice());
    let message_type = find(OPT_MESSAGE_TYPE).and_then(|v| v.first().copied());
    let requested_ip = find(OPT_REQUESTED_IP).and_then(ip4);
    let server_id = find(OPT_SERVER_ID).and_then(ip4);
    let hostname = find(OPT_HOSTNAME).map(|v| String::from_utf8_lossy(v).into_owned());
    Some(Packet {
        op: p[0],
        xid: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
        flags: u16::from_be_bytes([p[10], p[11]]),
        ciaddr: Ipv4Addr::new(p[12], p[13], p[14], p[15]),
        yiaddr: Ipv4Addr::new(p[16], p[17], p[18], p[19]),
        chaddr: Mac(chaddr),
        message_type,
        requested_ip,
        server_id,
        hostname,
        options,
    })
}

/// Costruisce un messaggio BOOTP con le opzioni date (in quest'ordine).
pub fn build(
    op: u8,
    xid: u32,
    flags: u16,
    ciaddr: Ipv4Addr,
    yiaddr: Ipv4Addr,
    chaddr: Mac,
    options: &[(u8, Vec<u8>)],
) -> Vec<u8> {
    let mut p = vec![0u8; FIXED_LEN];
    p[0] = op;
    p[1] = 1;
    p[2] = 6;
    p[4..8].copy_from_slice(&xid.to_be_bytes());
    p[10..12].copy_from_slice(&flags.to_be_bytes());
    p[12..16].copy_from_slice(&ciaddr.octets());
    p[16..20].copy_from_slice(&yiaddr.octets());
    p[28..34].copy_from_slice(&chaddr.0);
    p.extend_from_slice(&MAGIC);
    for (code, value) in options {
        p.push(*code);
        p.push(value.len() as u8);
        p.extend_from_slice(value);
    }
    p.push(OPT_END);
    if p.len() < MIN_REPLY_LEN {
        p.resize(MIN_REPLY_LEN, 0);
    }
    p
}

/// Parametri della rete annunciati dal server.
#[derive(Clone, Copy, Debug)]
pub struct Lease {
    pub server: Ipv4Addr,
    pub client: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub router: Ipv4Addr,
    pub dns: Ipv4Addr,
    pub lease_secs: u32,
}

/// Risposta del server: messaggio da inviare e tipo.
#[derive(Clone, Debug)]
pub struct Reply {
    pub message_type: u8,
    pub packet: Vec<u8>,
    /// Indirizzo IP e MAC di destinazione.
    pub dst_ip: Ipv4Addr,
    pub dst_mac: Mac,
    pub yiaddr: Ipv4Addr,
}

/// Decide la risposta a un messaggio del client. `None` se non va risposto
/// (messaggio non per noi, Release, Decline).
pub fn respond(req: &Packet, lease: &Lease) -> Option<Reply> {
    if req.op != 1 {
        return None;
    }
    let mt = req.message_type?;
    let (reply_type, yiaddr) = match mt {
        DISCOVER => (OFFER, lease.client),
        REQUEST => {
            // Un REQUEST con server id diverso dal nostro è la scelta di un
            // altro server: si tace (RFC 2131 4.3.2).
            if req.server_id.is_some_and(|s| s != lease.server) {
                return None;
            }
            let wanted = req.requested_ip.unwrap_or(req.ciaddr);
            if wanted == lease.client { (ACK, lease.client) } else { (NAK, Ipv4Addr::UNSPECIFIED) }
        }
        INFORM => (ACK, Ipv4Addr::UNSPECIFIED),
        _ => return None,
    };
    let mut opts =
        vec![(OPT_MESSAGE_TYPE, vec![reply_type]), (OPT_SERVER_ID, lease.server.octets().to_vec())];
    if reply_type != NAK {
        if mt != INFORM {
            opts.push((OPT_LEASE_TIME, lease.lease_secs.to_be_bytes().to_vec()));
            opts.push((OPT_RENEWAL_TIME, (lease.lease_secs / 2).to_be_bytes().to_vec()));
            opts.push((OPT_REBINDING_TIME, (lease.lease_secs / 8 * 7).to_be_bytes().to_vec()));
        }
        opts.push((OPT_SUBNET_MASK, lease.netmask.octets().to_vec()));
        opts.push((OPT_ROUTER, lease.router.octets().to_vec()));
        opts.push((OPT_DNS, lease.dns.octets().to_vec()));
    }
    let packet = build(2, req.xid, req.flags, req.ciaddr, yiaddr, req.chaddr, &opts);
    // RFC 2131 4.1: al ciaddr se il client ce l'ha già (e non è un NAK),
    // altrimenti broadcast IP; a livello Ethernet broadcast solo se il
    // client ha chiesto il bit B.
    let (dst_ip, dst_mac) = if !req.ciaddr.is_unspecified() && reply_type != NAK {
        (req.ciaddr, req.chaddr)
    } else if req.broadcast_flag() || reply_type == NAK {
        (Ipv4Addr::BROADCAST, Mac::BROADCAST)
    } else {
        (Ipv4Addr::BROADCAST, req.chaddr)
    };
    Some(Reply { message_type: reply_type, packet, dst_ip, dst_mac, yiaddr })
}
