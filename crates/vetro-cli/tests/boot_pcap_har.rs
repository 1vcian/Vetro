//! `vetro boot --pcap FILE --har=FILE --net-requests` (M7, ADR 0016): il
//! guest fa una GET col wget di BusyBox, a fine esecuzione `vetro` scrive la
//! cattura in pcapng e le richieste in HAR, e stampa la lista
//! dell'ispettore su stderr. I file si rileggono con `vetro-analysis`.
//!
//! In release (`cargo test --release -p vetro-cli`), come `boot_net.rs`.

use std::process::Command;
use std::time::Duration;

use vetro_analysis::net::json::{self, Value};
use vetro_analysis::net::{NetworkAnalysis, pcapng};
use vetro_boot_tests::{BOOT_MARKER, Console, SHELL_PROMPT, guest_kernel, repo_root, skip_or_fail, timeout};

#[test]
fn boot_con_pcap_e_har() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "avvio sotto Vetro solo in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel mancante");
    };
    let dir = repo_root().join("target/guest-kernel");
    let (pcap, har) = (dir.join("cli.pcapng"), dir.join("cli.har"));
    let _ = std::fs::remove_file(&pcap);
    let _ = std::fs::remove_file(&har);
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("exec \"$0\" \"$@\" 2>&1")
        .arg(env!("CARGO_BIN_EXE_vetro"))
        .arg("boot")
        .arg(format!("--kernel={}", image.display()))
        .arg(format!("--initrd={}", initrd.display()))
        .arg("--append=console=ttyAMA0 vetro.noautotest")
        .arg("--mem=256")
        .arg("--no-devices")
        .arg("--net")
        .arg("--pcap")
        .arg(&pcap)
        .arg(format!("--har={}", har.display()))
        .arg("--net-requests");
    let mut c = Console::spawn(cmd).expect("vetro boot");
    let limit = timeout();
    let at = c.wait_for(BOOT_MARKER, 0, limit).unwrap_or_else(|| panic!("niente /init:\n{}", c.log()));
    let at = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("niente shell:\n{}", c.log()));
    c.send(concat!(
        "udhcpc -i eth0 -n -q >/dev/null 2>&1; ",
        "wget -q -O /dev/null 'http://cli.example/pagina?a=1'; echo \"F\"ATTO; ",
        "sleep 2; poweroff -f\n"
    ));
    c.wait_line("FATTO", at, limit).unwrap_or_else(|| panic!("niente wget:\n{}", c.log()));
    assert!(c.finish(Duration::from_secs(60)), "poweroff -f non ha fermato vetro:\n{}", c.log());
    let log = c.log();
    assert!(
        log.lines()
            .any(|l| l.starts_with("vetro: http: [")
                && l.contains("#0 GET http://cli.example/pagina?a=1 -> 200")),
        "lista dell'ispettore:\n{log}"
    );
    assert!(log.contains("vetro: har: 1 richieste in"), "{log}");

    let file = pcapng::read(&std::fs::read(&pcap).expect("pcapng scritto")).expect("pcapng valido");
    assert!(log.contains(&format!("vetro: pcapng: {} frame in", file.frames.len())), "{log}");
    let a = NetworkAnalysis::from_frames(&file.frames);
    assert_eq!(a.http.len(), 1);
    assert_eq!(a.http[0].url, "http://cli.example/pagina?a=1");
    assert!(a.dns.iter().any(|d| d.name == "cli.example"));

    let v = json::parse(&std::fs::read(&har).expect("HAR scritto")).expect("HAR JSON");
    let Some(Value::Array(e)) = v.get("log").and_then(|l| l.get("entries")) else { panic!() };
    assert_eq!(e.len(), 1);
    assert_eq!(
        e[0].get("request").and_then(|r| r.get("url")).and_then(Value::as_str),
        Some("http://cli.example/pagina?a=1")
    );
}
