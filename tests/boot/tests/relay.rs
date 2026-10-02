//! The guest online through the network gateway's protocol (ADR 0046), with
//! the gateway played by the test between one quantum and the next, as the
//! page's Worker does: the frames the guest produces are taken with
//! `Machine::relay_take`, the gateway's answers go in with
//! `Machine::input(Input::Relay(..))`.
//!
//! - DNS through the gateway: `nslookup` gets the gateway's (real-looking)
//!   address, not a sinkhole one;
//! - HTTP (`wget`): a short GET, a 600 KB GET (beyond the 256 KiB window:
//!   the test's gateway waits for the machine's ACKs, `cksum` compared) and
//!   a connection the gateway refuses (port 22: `wget` sees it refused);
//! - record & replay (ADR 0019): the session is recorded with the
//!   gateway's frames as inputs; the replay from boot, with no gateway at
//!   all, gives the same console, instructions and final state, and the same
//!   frames towards the gateway.
//!
//! Release only, like `net.rs`.

use std::collections::{BTreeMap, VecDeque};
use std::net::Ipv4Addr;

use vetro_boot_tests::*;
use vetro_machine::record::EventKind;
use vetro_machine::vetro_net::dns;
use vetro_machine::vetro_net::relay::{MAX_DATA, WINDOW};
use vetro_machine::vetro_net::{Frame, Refusal, Side};
use vetro_machine::{Devices, Input, Log, Machine, MachineConfig, RecordOptions, ReplayStatus, Reply, Stop};

const PHASE_BUDGET: u64 = 6_000_000_000;
const QUANTUM: u64 = 1_000_000;
const SERVER: Ipv4Addr = Ipv4Addr::new(93, 184, 215, 14);

fn big_body() -> Vec<u8> {
    (0..600_000u32).map(|i| (i.wrapping_mul(2_246_822_519) >> 11) as u8).collect()
}

fn http_ok(body: &[u8]) -> Vec<u8> {
    let mut r = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
        .into_bytes();
    r.extend_from_slice(body);
    r
}

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

/// One connection at the test's gateway: an HTTP server that answers the
/// first request and closes, within the window.
#[derive(Default)]
struct Conn {
    request: Vec<u8>,
    pending: VecDeque<u8>,
    in_flight: u32,
    answered: bool,
    shut_sent: bool,
}

/// The gateway played by the test (deterministic: it answers at the next
/// quantum boundary, in frame order).
#[derive(Default)]
struct Gateway {
    conns: BTreeMap<u32, Conn>,
    dns: Vec<String>,
    connects: Vec<(u32, u16)>,
    /// Every frame the machine sent, encoded (compared with the replay's).
    seen: Vec<Vec<u8>>,
}

impl Gateway {
    /// Frames from the machine; returns the answers for the machine.
    fn serve(&mut self, frames: Vec<Vec<u8>>) -> Vec<Frame> {
        let mut out = Vec::new();
        for bytes in frames {
            self.seen.push(bytes.clone());
            let (side, f) = Frame::decode(&bytes).expect("a valid frame from the machine");
            assert_eq!(side, Side::Machine);
            match f {
                Frame::Dns { id, msg, answer: false } => {
                    let q = dns::parse_query(&msg).expect("a DNS query");
                    self.dns.push(q.name.clone());
                    let (rcode, addrs) = if q.name.ends_with(".example") && q.qtype == dns::TYPE_A {
                        (dns::RCODE_NOERROR, vec![SERVER])
                    } else {
                        (dns::RCODE_NXDOMAIN, vec![])
                    };
                    out.push(Frame::Dns { id, msg: dns::build_response(&q, rcode, &addrs, 60), answer: true });
                }
                Frame::Connect { id, dst } => {
                    self.connects.push((id, dst.port()));
                    if *dst.ip() != SERVER || !matches!(dst.port(), 80 | 443) {
                        out.push(Frame::Refused { id, reason: Refusal::Port });
                    } else {
                        self.conns.insert(id, Conn::default());
                        out.push(Frame::Connected { id });
                    }
                }
                Frame::Data { id, data } => {
                    let c = self.conns.get_mut(&id).expect("data on an open connection");
                    assert!(data.len() <= MAX_DATA);
                    c.request.extend(&data);
                    out.push(Frame::Ack { id, n: data.len() as u32 });
                    if !c.answered && c.request.windows(4).any(|w| w == b"\r\n\r\n") {
                        c.answered = true;
                        let path = String::from_utf8_lossy(&c.request).split(' ').nth(1).unwrap_or("").to_owned();
                        let body = if path == "/big" { big_body() } else { b"VETRO-GATEWAY-OK\n".to_vec() };
                        c.pending.extend(http_ok(&body));
                    }
                }
                Frame::Ack { id, n } => {
                    if let Some(c) = self.conns.get_mut(&id) {
                        assert!(n <= c.in_flight, "the machine acked more than it got");
                        c.in_flight -= n;
                    }
                }
                Frame::Shutdown { .. } => {}
                Frame::Close { id, .. } => {
                    self.conns.remove(&id);
                }
                f => panic!("unexpected frame from the machine: {f:?}"),
            }
        }
        for (&id, c) in &mut self.conns {
            while !c.pending.is_empty() && c.in_flight < WINDOW {
                let n = c.pending.len().min(MAX_DATA).min((WINDOW - c.in_flight) as usize);
                c.in_flight += n as u32;
                out.push(Frame::Data { id, data: c.pending.drain(..n).collect() });
            }
            if c.answered && c.pending.is_empty() && !c.shut_sent {
                c.shut_sent = true;
                out.push(Frame::Shutdown { id });
            }
        }
        out
    }
}

struct Run {
    m: Machine,
    gw: Option<Gateway>,
    log: Vec<u8>,
    max_in_flight: u32,
}

impl Run {
    fn pump(&mut self) {
        let frames = self.m.relay_take();
        let Some(gw) = &mut self.gw else {
            return;
        };
        for f in gw.serve(frames) {
            assert_eq!(self.m.input(Input::Relay(f.encode_from(Side::Gateway))), Reply::Done, "{f:?}");
        }
        self.max_in_flight = self.max_in_flight.max(gw.conns.values().map(|c| c.in_flight).max().unwrap_or(0));
    }

    fn until(&mut self, needle: &str, from: usize) -> usize {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            if let Some(i) = find(&self.log[from.min(self.log.len())..], needle.as_bytes()) {
                return from + i + needle.len();
            }
            assert!(self.m.steps < limit, "{needle:?} did not arrive:\n{}", self.tail());
            let stop = self.m.run(QUANTUM);
            self.log.extend(self.m.console_output());
            self.pump();
            assert!(matches!(stop, Stop::Budget), "{stop:?} while waiting for {needle:?}:\n{}", self.tail());
        }
    }

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

fn value<'a>(out: &'a str, key: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(key))
        .unwrap_or_else(|| panic!("missing {key:?} in the output:\n{out}"))
        .trim()
}

fn booted(image: &[u8], initrd: &[u8]) -> Machine {
    let mut m = Machine::with_devices(&MachineConfig::default(), &Devices::default());
    m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").expect("loading the kernel");
    m
}

#[test]
fn guest_online_through_the_gateway_and_identical_replay() {
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

    let mut m = booted(&image, &initrd);
    m.start_recording(RecordOptions::default());
    // Online from the start, as the web app does after a restore.
    assert_eq!(m.input(Input::Relay(Frame::Down.encode_from(Side::Gateway))), Reply::Done);
    assert_eq!(m.input(Input::Relay(Frame::Online(true).encode_from(Side::Gateway))), Reply::Done);
    let mut r = Run { m, gw: Some(Gateway::default()), log: Vec::new(), max_in_flight: 0 };
    let at = r.until(SHELL_PROMPT, 0);
    let (at, out) = r.command("udhcpc -i eth0 -n -q", at);
    assert!(out.contains("udhcpc: bound eth0 10.0.2.15"), "{out}");

    let (at, out) = r.command("nslookup vetro.example", at);
    assert!(out.contains("Address: 93.184.215.14"), "nslookup through the gateway:\n{out}");

    let (at, out) = r.command("echo \"G\"ET=$(wget -q -O - http://vetro.example/hello)", at);
    assert_eq!(value(&out, "GET="), "VETRO-GATEWAY-OK");

    let (at, out) = r.command("echo \"C\"K=$(wget -q -O - http://big.example:443/big | cksum)", at);
    let (crc, len) = posix_cksum(&big_body());
    assert_eq!(value(&out, "CK="), format!("{crc} {len}"), "600 KB through the gateway");

    let (at, out) = r.command("wget -q -O - http://vetro.example:22/ ; echo \"R\"C=$?", at);
    assert_eq!(value(&out, "RC="), "1", "refused by the gateway:\n{out}");
    assert!(out.contains("refused"), "{out}");

    // Closes (TIME-WAIT) reach the gateway before the end.
    let (_, _) = r.command("sleep 5", at);
    let gw = r.gw.take().unwrap();
    assert!(r.max_in_flight <= WINDOW && r.max_in_flight > WINDOW / 2, "window used: {}", r.max_in_flight);
    assert!(gw.conns.is_empty(), "every connection closed at the gateway");
    assert_eq!(gw.connects.iter().map(|c| c.1).collect::<Vec<_>>(), [80, 443, 22]);
    assert!(gw.dns.iter().any(|n| n == "vetro.example") && gw.dns.iter().any(|n| n == "big.example"));
    r.m.net_view(|s| {
        let st = s.upstream().stats();
        assert_eq!((st.connects, st.refused), (3, 1), "{st:?}");
        assert_eq!(s.upstream().sinkhole().tcp_connections().count(), 0, "nothing went to the sinkhole");
    });
    let log = r.m.stop_recording().expect("recording");
    let log = Log::decode(&log.encode()).expect("the log reads back");
    let relay_inputs = log.events.iter().filter(|e| matches!(e.kind, EventKind::Input(Input::Relay(_)))).count();
    assert!(relay_inputs > 100, "{relay_inputs} gateway frames recorded");
    assert!(log.events.iter().all(|e| matches!(e.kind, EventKind::Input(_))), "no opaque event");
    let (steps, end, console) = (r.m.steps, r.m.digest(), r.log.clone());

    // Replay from boot with no gateway: the frames come from the log.
    let mut m = booted(&image, &initrd);
    m.start_replay(&log).expect("starting state");
    let mut out = m.console_output();
    let mut sent = Vec::new();
    let limit = m.steps + 40 * PHASE_BUDGET;
    while matches!(m.replay_status(), Some(ReplayStatus::Running { .. })) {
        assert!(m.steps < limit, "replay without an end");
        m.run(3 * QUANTUM + 17);
        out.extend(m.console_output());
        sent.extend(m.relay_take());
    }
    assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished));
    assert!(out == console, "replay console differs from the recording");
    assert_eq!(m.steps, steps);
    assert_eq!(m.digest(), end, "final state of the replay");
    assert!(sent == gw.seen, "the replay sends the same frames to the (absent) gateway");
    eprintln!(
        "gateway session: {relay_inputs} gateway frames, {} machine frames, {} instructions, max in flight {}",
        sent.len(),
        steps,
        r.max_in_flight
    );
}
