//! `vetro boot --no-devices --net --net-events`: il kernel guest ottiene
//! l'indirizzo via DHCP dallo stack di `vetro-net`, risolve un nome col DNS
//! del sinkhole e fa una richiesta HTTP; gli eventi di rete arrivano su
//! stderr (qui unito a stdout) in tempo virtuale.
//!
//! In release (`cargo test --release -p vetro-cli`): in debug l'interprete è
//! troppo lento e il test si salta.

use std::process::Command;
use std::time::Duration;

use vetro_boot_tests::{BOOT_MARKER, Console, SHELL_PROMPT, guest_kernel, skip_or_fail, timeout};

#[test]
fn boot_con_rete_ed_eventi() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "avvio sotto Vetro solo in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel mancante");
    };
    // stderr su stdout: gli eventi si leggono dalla stessa console.
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
        .arg("--net-events");
    let mut c = Console::spawn(cmd).expect("vetro boot");
    let limit = timeout();
    let at = c.wait_for(BOOT_MARKER, 0, limit).unwrap_or_else(|| panic!("niente /init:\n{}", c.log()));
    let at = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("niente shell:\n{}", c.log()));
    c.send(concat!(
        "udhcpc -i eth0 -n -q >/dev/null 2>&1; ",
        "echo \"R\"ISPOSTA=$(wget -q -S -O /dev/null http://cli.example/pagina 2>&1 | head -n 1); ",
        "sleep 5; poweroff -f\n"
    ));
    let (_, r) = c.wait_line("RISPOSTA=", at, limit).unwrap_or_else(|| panic!("niente wget:\n{}", c.log()));
    assert_eq!(r.split_whitespace().collect::<Vec<_>>(), ["RISPOSTA=", "HTTP/1.1", "200", "OK"]);
    assert!(c.finish(Duration::from_secs(60)), "poweroff -f non ha fermato vetro:\n{}", c.log());
    let log = c.log();
    let events: Vec<&str> = log.lines().filter_map(|l| l.strip_prefix("vetro-net: ")).collect();
    let has = |what: &str| events.iter().any(|e| e.contains(what));
    for what in [
        "dhcp Ack 10.0.2.15 52:54:00:12:34:56",
        "dns 1 domanda cli.example tipo 1",
        "risposta cli.example tipo 1 rcode 0",
        "-> 198.18.0.1:80",
        "stabilita",
        "chiusa Normal (guest->remoto",
    ] {
        assert!(has(what), "manca l'evento {what:?}:\n{}", events.join("\n"));
    }
}
