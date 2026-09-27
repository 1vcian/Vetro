//! `vetro boot --no-devices --net --net-events`: the guest kernel gets
//! its address via DHCP from the `vetro-net` stack, resolves a name with the
//! sinkhole's DNS and makes an HTTP request; the network events arrive on
//! stderr (here merged into stdout) in virtual time.
//!
//! In release (`cargo test --release -p vetro-cli`): in debug the interpreter is
//! too slow and the test is skipped.

use std::process::Command;
use std::time::Duration;

use vetro_boot_tests::{BOOT_MARKER, Console, SHELL_PROMPT, guest_kernel, skip_or_fail, timeout};

#[test]
fn boot_con_rete_ed_eventi() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "boot under Vetro only in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel missing");
    };
    // stderr onto stdout: the events are read from the same console.
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
    let at = c.wait_for(BOOT_MARKER, 0, limit).unwrap_or_else(|| panic!("no /init:\n{}", c.log()));
    let at = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("no shell:\n{}", c.log()));
    c.send(concat!(
        "udhcpc -i eth0 -n -q >/dev/null 2>&1; ",
        "echo \"R\"ISPOSTA=$(wget -q -S -O /dev/null http://cli.example/pagina 2>&1 | head -n 1); ",
        "sleep 5; poweroff -f\n"
    ));
    let (_, r) = c.wait_line("RISPOSTA=", at, limit).unwrap_or_else(|| panic!("no wget:\n{}", c.log()));
    assert_eq!(r.split_whitespace().collect::<Vec<_>>(), ["RISPOSTA=", "HTTP/1.1", "200", "OK"]);
    assert!(c.finish(Duration::from_secs(60)), "poweroff -f did not stop vetro:\n{}", c.log());
    let log = c.log();
    let events: Vec<&str> = log.lines().filter_map(|l| l.strip_prefix("vetro-net: ")).collect();
    let has = |what: &str| events.iter().any(|e| e.contains(what));
    for what in [
        "dhcp Ack 10.0.2.15 52:54:00:12:34:56",
        "dns 1 query cli.example type 1",
        "answer cli.example type 1 rcode 0",
        "-> 198.18.0.1:80",
        "established",
        "closed Normal (guest->remote",
    ] {
        assert!(has(what), "missing event {what:?}:\n{}", events.join("\n"));
    }
}
