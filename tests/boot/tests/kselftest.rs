//! Criterio di uscita di M3, seconda parte: una selezione di kselftest del
//! kernel guest (`guest/kernel/kselftest/targets.txt`, compilati da
//! `tools/guest-kernel/kselftest.sh`) gira nel guest sotto QEMU e sotto
//! Vetro, e l'esito di ogni test (ok, SKIP, fallito) deve coincidere.
//!
//! I test che falliscono anche sotto QEMU sono quelli che nel nostro initramfs
//! non hanno ciò che serve (python, file system di prova, ecc.): contano lo
//! stesso, perché Vetro deve fallire allo stesso modo. Solo in release e con
//! `VETRO_KSELFTEST=1`.

use std::process::Command;
use std::time::Duration;
use vetro_boot_tests::*;
use vetro_machine::{Machine, MachineConfig, Stop};

const CMDLINE: &str = "console=ttyAMA0 vetro.noautotest vetro.kselftest";
/// Istruzioni concesse a Vetro per tutta la selezione (a 100 MHz nominali,
/// un'ora di guest).
const BUDGET: u64 = 360_000_000_000;

/// Esiti TAP (`ok N gruppo:test`, `... # SKIP`, `not ok ...`), con i fallimenti
/// ridotti a "FAIL" (il codice d'uscita di un test fallito può dipendere dai
/// tempi di QEMU, ma non il fatto che fallisca).
fn results(log: &str) -> Vec<String> {
    normalize(log)
        .lines()
        .filter(|l| l.starts_with("ok ") || l.starts_with("not ok "))
        .map(|l| match l.split_once(" # ") {
            Some((head, tail)) if tail.starts_with("SKIP") => format!("{head} # SKIP"),
            Some((head, _)) => format!("{head} # FAIL"),
            None if l.starts_with("not ok") => format!("{l} # FAIL"),
            None => l.to_string(),
        })
        .collect()
}

#[test]
fn kselftest_come_sotto_qemu() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "kselftest sotto Vetro solo in release");
    }
    // Lungo (decine di minuti con l'interprete): solo su richiesta.
    if !std::env::var("VETRO_KSELFTEST").is_ok_and(|v| v == "1") {
        eprintln!("SKIP kselftest_come_sotto_qemu: VETRO_KSELFTEST=1 per eseguirlo");
        return;
    }
    let root = repo_root();
    let dir = root.join("target/guest-kernel");
    let (image, initrd) = (dir.join("Image"), dir.join("initramfs-kselftest.cpio.gz"));
    if !image.is_file() || !initrd.is_file() {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "initramfs dei kselftest mancante: esegui tools/guest-kernel/kselftest.sh",
        );
    }
    let Some(qemu) = qemu_system() else {
        return skip_or_fail("VETRO_REQUIRE_SYSTEM_ORACLE", "qemu-system-aarch64 assente");
    };
    let expected: usize = std::fs::read_to_string(dir.join("kselftest-list.txt"))
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);

    // QEMU.
    let mut cmd = Command::new(qemu);
    cmd.args(QEMU_MACHINE).args(["-nographic", "-kernel"]).arg(&image).arg("-initrd").arg(&initrd);
    cmd.args(["-append", CMDLINE]);
    let mut con = Console::spawn(cmd).expect("avvio di qemu-system-aarch64");
    let limit = Duration::from_secs(3600);
    assert!(con.wait_for("VETRO-KSELFTEST-FINE", 0, limit).is_some(), "QEMU: kselftest non finiti");
    con.finish(Duration::from_secs(30));
    let qlog = con.log();
    std::fs::write(dir.join("qemu-kselftest.log"), normalize(&qlog)).unwrap();
    let theirs = results(&qlog);
    assert_eq!(theirs.len(), expected, "QEMU non ha eseguito tutti i test");

    // Vetro.
    let mut m = Machine::new(&MachineConfig::default());
    m.load_linux(&std::fs::read(&image).unwrap(), Some(&std::fs::read(&initrd).unwrap()), CMDLINE)
        .expect("caricamento del kernel");
    let mut vlog = Vec::new();
    let stop = loop {
        let s = m.run(10_000_000);
        vlog.extend(m.console_output());
        if s != Stop::Budget || m.steps >= BUDGET {
            break s;
        }
    };
    let vlog = String::from_utf8_lossy(&vlog).into_owned();
    std::fs::write(dir.join("vetro-kselftest.log"), normalize(&vlog)).unwrap();
    assert_eq!(stop, Stop::PowerOff, "Vetro non ha finito i kselftest; coda:\n{}", tail(&vlog));
    let ours = results(&vlog);

    let passed = theirs.iter().filter(|r| !r.ends_with("# FAIL")).count();
    eprintln!(
        "kselftest: {} test, {passed} ok o SKIP sotto QEMU; {} istruzioni su Vetro",
        theirs.len(),
        m.steps
    );
    let diff: Vec<String> = theirs
        .iter()
        .zip(ours.iter().chain(std::iter::repeat(&String::new())))
        .filter(|(a, b)| a != b)
        .map(|(a, b)| format!("  qemu : {a}\n  vetro: {b}"))
        .collect();
    assert!(diff.is_empty() && ours.len() == theirs.len(), "esiti diversi da QEMU:\n{}", diff.join("\n"));
}

fn tail(log: &str) -> String {
    let n = normalize(log);
    let v: Vec<&str> = n.lines().rev().take(30).collect();
    v.into_iter().rev().collect::<Vec<_>>().join("\n")
}
