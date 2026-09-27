//! The guest's network under Vetro: virtio-net connected to the
//! `vetro-net` stack with the sinkhole, exercised by the guest kernel with
//! BusyBox's tools. It is the part that the comparison with QEMU (`vetro.rs`, self-test:
//! DHCP and ping to the gateway) cannot cover, because with `-netdev user` QEMU
//! sends DNS and TCP onto the real network.
//!
//! - DHCP (`udhcpc`): 10.0.2.15, gateway 10.0.2.2, DNS 10.0.2.3;
//! - DNS (`nslookup`): the sinkhole answers with fake addresses in
//!   198.18.0.0/15, in order of first question (ADR 0007);
//! - HTTP (`wget`): a GET with a short response, a 300 KB GET (more
//!   than the 64 KiB window, `cksum` checksum compared with the host), a
//!   108 KB POST (the host finds the body again byte for byte);
//! - ICMP (`ping`): gateway and a fake address;
//! - the host checks the event log (DHCP, DNS, connections, bytes,
//!   closes) and what the sinkhole recorded (resolved name, guest
//!   bytes);
//! - determinism: two runs give the same log, the same instructions
//!   and the same event log.
//!
//! Release only, like `vetro.rs`.

use std::net::Ipv4Addr;

use vetro_boot_tests::*;
use vetro_machine::vetro_net::{CloseReason, DhcpMessage, Direction, EventKind, Mac, TcpReply};
use vetro_machine::{Devices, Machine, MachineConfig, NetSetup, Stop};

const PHASE_BUDGET: u64 = 6_000_000_000;

/// Body of the large response (port 8080): 300 KB, deterministic.
fn big_body() -> Vec<u8> {
    (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect()
}

fn http_ok(body: &[u8]) -> Vec<u8> {
    let mut r = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
        .into_bytes();
    r.extend_from_slice(body);
    r
}

fn reply(body: &[u8]) -> TcpReply {
    TcpReply { on_connect: Vec::new(), on_data: http_ok(body), close_after_reply: true }
}

/// POSIX `cksum` (MSB-first CRC-32 over the contents and the length), like
/// BusyBox's.
fn posix_cksum(data: &[u8]) -> (u32, usize) {
    let mut crc = 0u32;
    let mut feed = |b: u8| {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { crc << 1 ^ 0x04c1_1db7 } else { crc << 1 };
        }
    };
    data.iter().for_each(|&b| feed(b));
    let mut n = data.len();
    while n != 0 {
        feed(n as u8);
        n >>= 8;
    }
    (!crc, data.len())
}

struct Run {
    m: Machine,
    log: Vec<u8>,
}

impl Run {
    fn until(&mut self, needle: &str, from: usize) -> usize {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            if let Some(i) = find(&self.log[from.min(self.log.len())..], needle.as_bytes()) {
                return from + i + needle.len();
            }
            assert!(self.m.steps < limit, "{needle:?} did not arrive:\n{}", self.tail());
            let stop = self.m.run(1_000_000);
            self.log.extend(self.m.console_output());
            assert!(matches!(stop, Stop::Budget), "{stop:?} while waiting for {needle:?}:\n{}", self.tail());
        }
    }

    /// Sends a command and waits for the next prompt; returns the
    /// position after the prompt and the command's output.
    fn command(&mut self, cmd: &str, from: usize) -> (usize, String) {
        self.m.console_input(format!("{cmd}\n").as_bytes());
        let at = self.until(SHELL_PROMPT, from);
        (at, normalize(&String::from_utf8_lossy(&self.log[from..at])))
    }

    fn tail(&self) -> String {
        let log = normalize(&String::from_utf8_lossy(&self.log));
        let v: Vec<&str> = log.lines().rev().take(40).collect();
        v.into_iter().rev().collect::<Vec<_>>().join("\n")
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The line that starts with `key` in the output (the markers are written
/// `"K"EY=` in the command, so the echo does not contain them).
fn value<'a>(out: &'a str, key: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(key))
        .unwrap_or_else(|| panic!("missing {key:?} in the output:\n{out}"))
        .trim()
}

/// Outcome of a session: log, instructions and event log (in
/// text form, for the comparison between runs).
struct Session {
    log: String,
    steps: u64,
    events: String,
}

fn session(image: &[u8], initrd: &[u8]) -> Session {
    let mut net = NetSetup::default();
    net.sinkhole.tcp_by_port.insert(80, reply(b"VETRO-SINKHOLE-OK\n"));
    net.sinkhole.tcp_by_port.insert(8080, reply(&big_body()));
    net.sinkhole.tcp_by_port.insert(81, reply(b"RICEVUTO\n"));
    let devices = Devices { net: Some(net), ..Devices::default() };
    let mut m = Machine::with_devices(&MachineConfig::default(), &devices);
    m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").expect("loading the kernel");
    let mut r = Run { m, log: Vec::new() };
    let at = r.until(SHELL_PROMPT, 0);

    let (at, out) = r.command("udhcpc -i eth0 -n -q", at);
    assert!(out.contains("udhcpc: bound eth0 10.0.2.15/255.255.255.0 router 10.0.2.2 dns 10.0.2.3"), "{out}");
    assert!(out.contains("lease of 10.0.2.15 obtained from 10.0.2.2"), "{out}");

    let (at, out) = r.command("nslookup vetro.example", at);
    assert!(out.contains("Name:\tvetro.example"), "nslookup:\n{out}");
    assert!(out.contains("Address: 198.18.0.1"), "nslookup:\n{out}");

    let (at, out) = r.command("echo \"G\"ET=$(wget -q -O - http://vetro.example/prova)", at);
    assert_eq!(value(&out, "GET="), "VETRO-SINKHOLE-OK");

    let (at, out) = r.command("echo \"C\"K=$(wget -q -O - http://grande.example:8080/dati | cksum)", at);
    let (crc, len) = posix_cksum(&big_body());
    assert_eq!(value(&out, "CK="), format!("{crc} {len}"), "300 KB from the sinkhole to the guest");

    let (at, _) = r.command("seq 1 20000 > /tmp/su", at);
    let (at, out) =
        r.command("echo \"P\"OST=$(wget -q -O - --post-file=/tmp/su http://su.example:81/carica)", at);
    assert_eq!(value(&out, "POST="), "RICEVUTO");

    let (at, out) = r.command("echo \"P\"ING=$(ping -c 2 -W 5 10.0.2.2 | grep -c ttl=)", at);
    assert_eq!(value(&out, "PING="), "2", "ping to the gateway");
    let (at, out) = r.command("echo \"F\"INTO=$(ping -c 1 -W 5 198.18.0.1 | grep -c ttl=)", at);
    assert_eq!(value(&out, "FINTO="), "1", "ping to a fake address (the sinkhole answers)");

    // TCP closes end with TIME-WAIT: the guest sleeps long enough
    // (in virtual time: WFI jumps to the deadline) for the stack to
    // record them.
    let (_, _) = r.command("sleep 5", at);

    r.m.console_input(b"poweroff -f\n");
    let limit = r.m.steps + PHASE_BUDGET;
    let stop = loop {
        let s = r.m.run(1_000_000);
        r.log.extend(r.m.console_output());
        if s != Stop::Budget || r.m.steps >= limit {
            break s;
        }
    };
    assert_eq!(stop, Stop::PowerOff, "{}", r.tail());

    let events = check_host_view(&r.m);
    Session { log: normalize(&String::from_utf8_lossy(&r.log)), steps: r.m.steps, events }
}

/// What the host sees: event log and the sinkhole's recordings.
fn check_host_view(m: &Machine) -> String {
    m.net_view(|s| {
        let stats = s.stats();
        assert_eq!((stats.bad_checksum, stats.malformed), (0, 0), "{stats:?}");
        let ev: Vec<&EventKind> = s.events().iter().map(|e| &e.kind).collect();
        let guest_mac = Mac([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        let guest = Ipv4Addr::new(10, 0, 2, 15);
        for msg in [DhcpMessage::Offer, DhcpMessage::Ack] {
            assert!(
                ev.iter().any(|k| matches!(k, EventKind::Dhcp { message, mac, ip, .. }
                    if *message == msg && *mac == guest_mac && *ip == guest)),
                "DHCP {msg:?} missing: {ev:#?}"
            );
        }
        let answers: Vec<(&str, &[Ipv4Addr])> = ev
            .iter()
            .filter_map(|k| match k {
                EventKind::DnsAnswer { name, qtype: 1, addrs, .. } => Some((name.as_str(), addrs.as_slice())),
                _ => None,
            })
            .collect();
        let fake = |n: u8| Ipv4Addr::new(198, 18, 0, n);
        for (name, addr) in [("vetro.example", fake(1)), ("grande.example", fake(2)), ("su.example", fake(3))]
        {
            assert!(
                answers.iter().any(|(n, a)| *n == name && *a == [addr]),
                "DNS answer for {name}: {answers:?}"
            );
        }
        let pings: Vec<(Ipv4Addr, bool)> = ev
            .iter()
            .filter_map(|k| match k {
                EventKind::IcmpEcho { dst, answered } => Some((*dst, *answered)),
                _ => None,
            })
            .collect();
        assert_eq!(
            pings,
            [(Ipv4Addr::new(10, 0, 2, 2), true), (Ipv4Addr::new(10, 0, 2, 2), true), (fake(1), true)]
        );

        // Connections: name, port, bytes in both directions, clean close.
        let conns: Vec<_> = s.upstream().tcp_connections().collect();
        assert_eq!(conns.len(), 3, "{conns:#?}");
        let expect = [
            ("vetro.example", 80, http_ok(b"VETRO-SINKHOLE-OK\n").len()),
            ("grande.example", 8080, http_ok(&big_body()).len()),
            ("su.example", 81, http_ok(b"RICEVUTO\n").len()),
        ];
        let upload: Vec<u8> = (1..=20000).flat_map(|i| format!("{i}\n").into_bytes()).collect();
        for (c, (name, port, to_guest)) in conns.iter().zip(expect) {
            assert_eq!(c.hostname.as_deref(), Some(name));
            assert_eq!(*c.flow.guest.ip(), guest);
            assert_eq!(c.flow.remote.port(), port);
            assert_eq!(c.to_guest, to_guest as u64, "{name}");
            assert!(!c.reset && c.guest_shutdown && c.closed_at.is_some(), "{name}: {c:?}");
            let req = String::from_utf8_lossy(&c.from_guest);
            assert!(req.contains(&format!("Host: {name}")), "{name}: {req}");
            let data_to_remote: u64 = ev
                .iter()
                .filter_map(|k| match k {
                    EventKind::TcpData { id, dir: Direction::ToRemote, len } if *id == c.id => {
                        Some(*len as u64)
                    }
                    _ => None,
                })
                .sum();
            assert_eq!(data_to_remote, c.from_guest.len() as u64, "{name}: bytes in the event log");
            assert!(
                ev.iter().any(|k| matches!(k, EventKind::TcpEstablished { id } if *id == c.id)),
                "{name}: handshake"
            );
            assert!(
                ev.iter().any(|k| matches!(k, EventKind::TcpClosed { id, reason: CloseReason::Normal,
                    bytes_to_remote, bytes_to_guest }
                    if *id == c.id && *bytes_to_remote == c.from_guest.len() as u64
                        && *bytes_to_guest == to_guest as u64)),
                "{name}: normal close in the event log: {ev:#?}"
            );
        }
        assert!(String::from_utf8_lossy(&conns[0].from_guest).starts_with("GET /prova HTTP/1.1\r\n"));
        assert!(String::from_utf8_lossy(&conns[1].from_guest).starts_with("GET /dati HTTP/1.1\r\n"));
        let post = &conns[2].from_guest;
        assert!(String::from_utf8_lossy(post).starts_with("POST /carica HTTP/1.1\r\n"));
        assert!(post.ends_with(&upload), "POST body ({} bytes) different", post.len());
        eprintln!(
            "Vetro: {} network events, {} frames from the guest, {} to the guest",
            ev.len(),
            stats.frames_in,
            stats.frames_out
        );
        format!("{:?}", s.events())
    })
    .expect("virtio-net present")
}

#[test]
fn rete_del_guest_con_il_sinkhole() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "network under Vetro only in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel missing: run tools/guest-kernel/build.sh",
        );
    };
    let (image, initrd) = (std::fs::read(image).unwrap(), std::fs::read(initrd).unwrap());
    let a = session(&image, &initrd);
    std::fs::write(repo_root().join("target/guest-kernel/vetro-net.log"), &a.log).unwrap();
    eprintln!("Vetro: network exercised in {} instructions", a.steps);
    let b = session(&image, &initrd);
    assert_eq!(a.steps, b.steps, "different instructions between two identical runs");
    assert!(a.log == b.log, "different logs between two identical runs");
    assert!(a.events == b.events, "different network event logs between two identical runs");
}
