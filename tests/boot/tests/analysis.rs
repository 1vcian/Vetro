//! Analisi di rete (M7, ADR 0016) sul traffico vero del kernel guest: il
//! guest usa `wget` di BusyBox contro il sinkhole (risposte configurate
//! per porta), la macchina cattura i frame al confine di virtio-net
//! (`Machine::net_tap`), `vetro-analysis` ne fa pcapng, flussi, HTTP,
//! corpi decodificati e HAR.
//!
//! - GET con risposta JSON; GET con risposta `chunked` e `gzip` (compressa
//!   dal `gzip` dell'host); POST JSON con risposta protobuf; POST form
//!   urlencoded; POST multipart (file JSON); POST protobuf costruito con
//!   `printf`; POST (mandato con `nc`) con corpo compresso dal `gzip` di
//!   BusyBox (`Content-Encoding: gzip`): i decodificatori girano su dati
//!   prodotti da codificatori esterni;
//! - il pcapng si rilegge col nostro lettore (stessi frame) e, con Docker,
//!   con `capinfos`, `tcpdump` e `tshark` (stesse richieste e risposte);
//! - l'HAR si rilegge col nostro parser JSON e, con Docker, si valida
//!   contro lo schema HAR 1.2 (`har-validator`) e si apre con `haralyzer`
//!   (`tools/analysis/check.sh`);
//! - determinismo: due esecuzioni danno lo stesso pcapng e lo stesso HAR,
//!   byte per byte (i tempi sono quelli virtuali del guest).
//!
//! Solo in release, come `net.rs`. File in `target/guest-kernel/analysis.*`.

use std::io::Write as _;
use std::process::{Command, Stdio};

use vetro_analysis::net::body::{Decoded, Wire};
use vetro_analysis::net::har::HarOptions;
use vetro_analysis::net::json::{self, Value};
use vetro_analysis::net::pcapng::{self, PcapngOptions};
use vetro_analysis::net::{Capture, Direction, NetworkAnalysis};
use vetro_boot_tests::*;
use vetro_machine::vetro_net::TcpReply;
use vetro_machine::{Devices, FrameDir, Machine, MachineConfig, NetSetup, Stop};

const PHASE_BUDGET: u64 = 6_000_000_000;

/// Porte del sinkhole usate dal test (tshark le decodifica come HTTP).
const PORTS: [u16; 7] = [80, 8081, 81, 82, 83, 84, 85];

fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut r = format!("HTTP/1.1 {status}\r\n");
    for (k, v) in headers {
        r.push_str(&format!("{k}: {v}\r\n"));
    }
    r.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
    let mut r = r.into_bytes();
    r.extend_from_slice(body);
    r
}

fn reply(bytes: Vec<u8>) -> TcpReply {
    TcpReply { on_connect: Vec::new(), on_data: bytes, close_after_reply: true }
}

/// `seq 1 n` come lo stampa BusyBox.
fn seq(n: u32) -> Vec<u8> {
    (1..=n).flat_map(|i| format!("{i}\n").into_bytes()).collect()
}

/// Comprime con il `gzip` dell'host (un codificatore che non è il nostro).
fn host_gzip(data: &[u8]) -> Vec<u8> {
    let mut c = Command::new("gzip")
        .args(["-9", "-n", "-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("gzip dell'host");
    c.stdin.take().unwrap().write_all(data).unwrap();
    let out = c.wait_with_output().unwrap();
    assert!(out.status.success());
    out.stdout
}

/// Risposta `chunked` con corpo gzip in pezzi da 1000 byte.
fn chunked_gzip(plain: &[u8]) -> Vec<u8> {
    let gz = host_gzip(plain);
    let mut r = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Encoding: gzip\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for c in gz.chunks(1000) {
        r.extend(format!("{:x}\r\n", c.len()).as_bytes());
        r.extend(c);
        r.extend(b"\r\n");
    }
    r.extend(b"0\r\n\r\n");
    r
}

const PROTO: &[u8] = &[0x08, 0x96, 0x01, 0x12, 0x07, b't', b'e', b's', b't', b'i', b'n', b'g'];
const JSON_ITEMS: &str = r#"{"ok":true,"items":[1,2,3],"nome":"vetro"}"#;

struct Run {
    m: Machine,
    log: Vec<u8>,
}

impl Run {
    fn until(&mut self, needle: &str, from: usize) -> usize {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            if let Some(i) = self.log[from.min(self.log.len())..]
                .windows(needle.len())
                .position(|w| w == needle.as_bytes())
            {
                return from + i + needle.len();
            }
            assert!(self.m.steps < limit, "{needle:?} non arrivato:\n{}", self.tail());
            let stop = self.m.run(1_000_000);
            self.log.extend(self.m.console_output());
            assert!(matches!(stop, Stop::Budget), "{stop:?} in attesa di {needle:?}:\n{}", self.tail());
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

fn value<'a>(out: &'a str, key: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(key))
        .unwrap_or_else(|| panic!("manca {key:?} nell'uscita:\n{out}"))
        .trim()
}

/// Una sessione: il guest fa le sette richieste; restituisce la cattura.
fn session(image: &[u8], initrd: &[u8]) -> Capture {
    let mut net = NetSetup::default();
    let s = &mut net.sinkhole.tcp_by_port;
    s.insert(80, reply(response("200 OK", &[("Content-Type", "application/json")], JSON_ITEMS.as_bytes())));
    s.insert(8081, reply(chunked_gzip(&seq(3000))));
    s.insert(81, reply(response("201 Created", &[("Content-Type", "application/x-protobuf")], PROTO)));
    for p in [82, 83, 84, 85] {
        s.insert(
            p,
            reply(response("200 OK", &[("Content-Type", "text/plain")], format!("porta {p}\n").as_bytes())),
        );
    }
    let devices = Devices { net: Some(net), ..Devices::default() };
    let mut m = Machine::with_devices(&MachineConfig::default(), &devices);
    assert!(m.net_tap(true), "cattura su virtio-net");
    m.load_linux(image, Some(initrd), "console=ttyAMA0 vetro.noautotest").expect("caricamento del kernel");
    let mut r = Run { m, log: Vec::new() };
    let at = r.until(SHELL_PROMPT, 0);
    let (at, out) = r.command("udhcpc -i eth0 -n -q", at);
    assert!(out.contains("lease of 10.0.2.15 obtained"), "{out}");

    let (at, out) = r.command("echo \"J\"SON=$(wget -q -O - 'http://api.example/api/items?x=1&y=due')", at);
    assert_eq!(value(&out, "JSON="), JSON_ITEMS);
    // wget non decomprime: il guest riceve i byte gzip e li decomprime lui.
    let (at, out) =
        r.command("echo \"G\"Z=$(wget -q -O - http://gz.example:8081/testo | gzip -dc | tail -n 1)", at);
    assert_eq!(value(&out, "GZ="), "3000");
    let (at, _) = r.command(
        "wget -q -O /dev/null --header 'Content-Type: application/json' --post-data '{\"nome\":\"vetro\",\"n\":42}' http://post.example:81/json",
        at,
    );
    let (at, _) =
        r.command("wget -q -O /dev/null --post-data 'utente=mario&citta=Milano+centro&x=%C3%A8' http://form.example:82/form", at);
    let (at, _) = r.command(
        "printf '%s\\r\\nContent-Disposition: form-data; name=\"campo\"\\r\\n\\r\\nvalore\\r\\n%s\\r\\nContent-Disposition: form-data; name=\"file\"; filename=\"dati.json\"\\r\\nContent-Type: application/json\\r\\n\\r\\n{\"k\":[1,2]}\\r\\n%s--\\r\\n' --vetroXYZ --vetroXYZ --vetroXYZ > /tmp/mp",
        at,
    );
    let (at, _) = r.command(
        "wget -q -O /dev/null --header 'Content-Type: multipart/form-data; boundary=vetroXYZ' --post-file /tmp/mp http://multi.example:83/carica",
        at,
    );
    let (at, _) =
        r.command("printf '\\010\\226\\001\\022\\007testing\\032\\003\\010\\226\\001' > /tmp/pb", at);
    let (at, _) = r.command(
        "wget -q -O /dev/null --header 'Content-Type: application/x-protobuf' --post-file /tmp/pb http://pb.example:84/pb",
        at,
    );
    // Corpo binario: `wget --post-file` di BusyBox lo tronca al primo byte
    // zero (lo legge come stringa C), quindi la richiesta la scrive la shell
    // e la manda `nc`.
    let (at, _) = r.command("seq 1 5000 | gzip -c > /tmp/z.gz", at);
    let (at, _) = r.command(
        "{ printf 'POST /zip HTTP/1.1\\r\\nHost: zip.example:85\\r\\nContent-Type: text/plain\\r\\nContent-Encoding: gzip\\r\\nContent-Length: %d\\r\\nConnection: close\\r\\n\\r\\n' $(wc -c < /tmp/z.gz); cat /tmp/z.gz; } | nc zip.example 85 > /dev/null",
        at,
    );
    // TIME-WAIT e ultimi ACK prima di spegnere.
    let (_, _) = r.command("sleep 2", at);
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
    let mut cap = Capture::new();
    for f in r.m.net_tap_take() {
        let dir = if f.dir == FrameDir::FromGuest { Direction::FromGuest } else { Direction::ToGuest };
        cap.push(f.at.0, dir, f.data);
    }
    let frames_in = r.m.net_view(|s| s.stats().frames_in).unwrap();
    let from_guest = cap.frames().iter().filter(|f| f.dir == Direction::FromGuest).count() as u64;
    assert_eq!(from_guest, frames_in, "ogni frame del guest arrivato allo stack è nella cattura");
    cap
}

/// (metodo, url, stato) attesi, in ordine.
fn expected() -> Vec<(&'static str, &'static str, u16)> {
    vec![
        ("GET", "http://api.example/api/items?x=1&y=due", 200),
        ("GET", "http://gz.example:8081/testo", 200),
        ("POST", "http://post.example:81/json", 201),
        ("POST", "http://form.example:82/form", 200),
        ("POST", "http://multi.example:83/carica", 200),
        ("POST", "http://pb.example:84/pb", 200),
        ("POST", "http://zip.example:85/zip", 200),
    ]
}

fn check_analysis(a: &NetworkAnalysis) {
    let got: Vec<(&str, &str, Option<u16>)> =
        a.http.iter().map(|x| (x.request.method.as_str(), x.url.as_str(), x.status())).collect();
    let want: Vec<(&str, &str, Option<u16>)> =
        expected().into_iter().map(|(m, u, s)| (m, u, Some(s))).collect();
    assert_eq!(got, want);
    for (x, name) in a.http.iter().zip(["api", "gz", "post", "form", "multi", "pb", "zip"]) {
        let name = format!("{name}.example");
        assert!(x.request.complete && x.response.as_ref().unwrap().complete, "{name}: messaggi completi");
        assert_eq!(x.resolved_name.as_deref(), Some(name.as_str()), "nome dal DNS del sinkhole");
        let t = &x.timings;
        assert!(t.dns_us.is_some() && t.connect_us.is_some(), "{name}: {t:?}");
        assert!(t.total_us() > 0, "{name}: {t:?}");
    }
    assert!(a.http.windows(2).all(|w| w[0].timings.started_us < w[1].timings.started_us));

    // Corpi decodificati.
    let Some(Decoded::Json(v)) = &a.http[0].response_body else { panic!("{:?}", a.http[0].response_body) };
    assert_eq!(v.to_compact(), JSON_ITEMS);
    let gz = a.http[1].response.as_ref().unwrap();
    assert!(gz.body.chunked);
    assert_eq!(gz.body.content_encoding.as_deref(), Some("gzip"));
    assert_eq!(gz.body.decoded, seq(3000), "chunked + gzip dell'host decodificati");
    let Decoded::Json(v) = &a.http[2].request_body else { panic!("{:?}", a.http[2].request_body) };
    assert_eq!(v.to_compact(), r#"{"nome":"vetro","n":42}"#);
    let Some(Decoded::Protobuf(f)) = &a.http[2].response_body else { panic!() };
    assert_eq!((f[0].number, &f[0].value), (1, &Wire::Varint(150)));
    assert_eq!((f[1].number, &f[1].value), (2, &Wire::String("testing".into())));
    assert_eq!(
        a.http[3].request_body,
        Decoded::Form(vec![
            ("utente".into(), "mario".into()),
            ("citta".into(), "Milano centro".into()),
            ("x".into(), "è".into())
        ])
    );
    let Decoded::Multipart(parts) = &a.http[4].request_body else { panic!("{:?}", a.http[4].request_body) };
    assert_eq!(parts.len(), 2);
    assert_eq!((parts[0].name.as_deref(), parts[0].data.as_slice()), (Some("campo"), &b"valore"[..]));
    assert_eq!(parts[1].filename.as_deref(), Some("dati.json"));
    assert!(matches!(*parts[1].decoded, Decoded::Json(_)));
    let Decoded::Protobuf(f) = &a.http[5].request_body else { panic!("{:?}", a.http[5].request_body) };
    assert_eq!(vetro_analysis::net::body::protobuf_text(f), "1: 150\n2: \"testing\"\n3 {\n  1: 150\n}\n");
    let z = &a.http[6].request;
    assert_eq!(z.body.content_encoding.as_deref(), Some("gzip"), "{:?} {:?}", z.headers, z.body.decode_error);
    assert_eq!(z.body.decoded, seq(5000), "corpo compresso dal gzip di BusyBox");

    // DNS: una domanda A con risposta per nome, indirizzi finti in ordine.
    for (i, n) in ["api", "gz", "post", "form", "multi", "pb", "zip"].iter().enumerate() {
        let name = format!("{n}.example");
        let d = a.dns.iter().find(|d| d.name == name && d.qtype == 1).unwrap_or_else(|| panic!("DNS {name}"));
        assert_eq!(d.ipv4().collect::<Vec<_>>(), [std::net::Ipv4Addr::new(198, 18, 0, i as u8 + 1)]);
    }
}

fn check_har(har: &str) {
    let v = json::parse(har.as_bytes()).expect("HAR JSON valido");
    let Some(Value::Array(entries)) = v.get("log").and_then(|l| l.get("entries")) else { panic!() };
    assert_eq!(entries.len(), 7);
    for (e, (m, u, s)) in entries.iter().zip(expected()) {
        let req = e.get("request").unwrap();
        assert_eq!(req.get("method").and_then(Value::as_str), Some(m));
        assert_eq!(req.get("url").and_then(Value::as_str), Some(u));
        assert_eq!(e.get("response").unwrap().get("status"), Some(&Value::Number(s.to_string())));
    }
    let text = entries[1].get("response").and_then(|r| r.get("content")).and_then(|c| c.get("text"));
    assert_eq!(text.and_then(Value::as_str).map(str::as_bytes), Some(seq(3000).as_slice()));
}

/// Gli strumenti esterni in Docker (`tools/analysis/check.sh`).
fn check_external(pcap: &std::path::Path, har: &std::path::Path, frames: usize) {
    let docker = Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]).output();
    if !docker.is_ok_and(|o| o.status.success()) {
        return skip_or_fail(
            "VETRO_REQUIRE_ANALYSIS_TOOLS",
            "Docker non disponibile: tshark e i validatori HAR non girano",
        );
    }
    let ports: Vec<String> = PORTS.iter().map(u16::to_string).collect();
    let out = Command::new(repo_root().join("tools/analysis/check.sh"))
        .arg(pcap)
        .arg(har)
        .env("VETRO_HTTP_PORTS", ports.join(","))
        .output()
        .expect("tools/analysis/check.sh");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "check.sh:\n{text}\n{}", String::from_utf8_lossy(&out.stderr));
    eprintln!("{text}");
    let lines: Vec<&str> = text.lines().collect();
    let field = |p: &str| lines.iter().filter_map(|l| l.strip_prefix(p)).map(str::trim).collect::<Vec<_>>();
    assert_eq!(field("CAPINFOS-PACKETS "), [frames.to_string()]);
    assert_eq!(field("TCPDUMP-PACKETS "), [frames.to_string()]);
    let reqs: Vec<String> = expected()
        .iter()
        .map(|(m, u, _)| {
            format!(
                "{m} {}",
                &u[u.find("//").unwrap() + 2..][u[u.find("//").unwrap() + 2..].find('/').unwrap()..]
            )
        })
        .collect();
    assert_eq!(field("TSHARK-REQUEST "), reqs, "richieste viste da tshark");
    let codes: Vec<String> = expected().iter().map(|e| e.2.to_string()).collect();
    assert_eq!(field("TSHARK-RESPONSE "), codes, "risposte viste da tshark");
    assert!(lines.contains(&"HAR-VALID"), "schema HAR 1.2:\n{text}");
    assert_eq!(field("HARALYZER "), ["7"]);
    let entries: Vec<String> = expected().iter().map(|(m, u, s)| format!("{m} {u} {s}")).collect();
    assert_eq!(field("HARALYZER-ENTRY "), entries);
}

#[test]
fn pcapng_e_har_dal_traffico_del_guest() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "rete sotto Vetro solo in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel mancante: esegui tools/guest-kernel/build.sh",
        );
    };
    let (image, initrd) = (std::fs::read(image).unwrap(), std::fs::read(initrd).unwrap());
    let cap = session(&image, &initrd);
    let pcap = pcapng::write(cap.frames(), &PcapngOptions::default());
    assert_eq!(pcapng::read(&pcap).expect("pcapng rileggibile").frames, cap.frames(), "nostro lettore");
    let a = NetworkAnalysis::from_frames(cap.frames());
    check_analysis(&a);
    let har = a.to_har(&HarOptions::default());
    check_har(&har);
    for row in a.requests() {
        eprintln!("{row}");
    }
    let dir = repo_root().join("target/guest-kernel");
    let (pcap_path, har_path) = (dir.join("analysis.pcapng"), dir.join("analysis.har"));
    std::fs::write(&pcap_path, &pcap).unwrap();
    std::fs::write(&har_path, &har).unwrap();
    check_external(&pcap_path, &har_path, cap.len());

    let again = session(&image, &initrd);
    assert!(
        pcapng::write(again.frames(), &PcapngOptions::default()) == pcap,
        "pcapng diverso fra due esecuzioni"
    );
    assert!(
        NetworkAnalysis::from_frames(again.frames()).to_har(&HarOptions::default()) == har,
        "HAR diverso"
    );
}
