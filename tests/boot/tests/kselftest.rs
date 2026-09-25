//! Criterio di uscita di M3, seconda parte: una selezione di kselftest del
//! kernel guest (`guest/kernel/kselftest/targets.txt`, compilati da
//! `tools/guest-kernel/kselftest.sh`) gira nel guest sotto QEMU e sotto
//! Vetro, e l'esito di ogni test (ok, SKIP, fallito) deve coincidere.
//!
//! Ogni test deve essere ok o SKIP, tranne quelli di
//! `guest/kernel/kselftest/expected-failures.txt`, che falliscono anche sotto
//! QEMU per l'ambiente del guest (python, bash, opzioni del kernel), ciascuno
//! con il motivo: per quelli Vetro deve fallire allo stesso modo. Solo in release e con
//! `VETRO_KSELFTEST=1`. Con `VETRO_JIT=1` (`VETRO_JIT_THRESHOLD=N`) Vetro
//! gira anche col JIT della modalità sistema, che deve dare lo stesso log
//! byte per byte e le stesse istruzioni dell'interprete.

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
    let (image, initrd) = (std::fs::read(&image).unwrap(), std::fs::read(&initrd).unwrap());
    let (vlog, steps) = run_vetro(&image, &initrd, None);
    std::fs::write(dir.join("vetro-kselftest.log"), normalize(&vlog)).unwrap();
    let ours = results(&vlog);
    if let Some(t) = jit_threshold() {
        let (jlog, jsteps) = run_vetro(&image, &initrd, Some(t));
        std::fs::write(dir.join("vetro-kselftest-jit.log"), normalize(&jlog)).unwrap();
        assert_eq!(jsteps, steps, "istruzioni diverse col JIT");
        assert!(jlog == vlog, "log dei kselftest diverso col JIT (vetro-kselftest-jit.log)");
        eprintln!("kselftest col JIT (soglia {t}): stesso log, {jsteps} istruzioni");
    }

    let passed = theirs.iter().filter(|r| !r.ends_with("# FAIL")).count();
    eprintln!(
        "kselftest: {} test, {passed} ok o SKIP sotto QEMU; {} istruzioni su Vetro",
        theirs.len(),
        steps
    );
    // La selezione è verde: fuori dall'elenco documentato nessun fallimento.
    let allowed: Vec<String> =
        std::fs::read_to_string(root.join("guest/kernel/kselftest/expected-failures.txt"))
            .unwrap()
            .lines()
            .filter_map(|l| l.split('#').next())
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
    let name = |r: &str| {
        r.split_whitespace().nth(if r.starts_with("not") { 3 } else { 2 }).unwrap_or("").to_string()
    };
    let failing: Vec<String> = theirs.iter().filter(|r| r.ends_with("# FAIL")).map(|r| name(r)).collect();
    let unexpected: Vec<&String> = failing.iter().filter(|f| !allowed.contains(f)).collect();
    assert!(unexpected.is_empty(), "falliti sotto QEMU fuori da expected-failures.txt: {unexpected:?}");
    let stale: Vec<&String> = allowed.iter().filter(|a| !failing.contains(a)).collect();
    assert!(stale.is_empty(), "in expected-failures.txt ma non falliti: {stale:?} (aggiorna l'elenco)");

    let diff: Vec<String> = theirs
        .iter()
        .zip(ours.iter().chain(std::iter::repeat(&String::new())))
        .filter(|(a, b)| a != b)
        .map(|(a, b)| format!("  qemu : {a}\n  vetro: {b}"))
        .collect();
    assert!(diff.is_empty() && ours.len() == theirs.len(), "esiti diversi da QEMU:\n{}", diff.join("\n"));
}

fn jit_threshold() -> Option<u32> {
    if !std::env::var("VETRO_JIT").is_ok_and(|v| v == "1") {
        return None;
    }
    Some(std::env::var("VETRO_JIT_THRESHOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(16))
}

/// Tutta la selezione sotto Vetro (col JIT se `jit`): log e istruzioni.
fn run_vetro(image: &[u8], initrd: &[u8], jit: Option<u32>) -> (String, u64) {
    let mut m = Machine::new(&MachineConfig::default());
    m.load_linux(image, Some(initrd), CMDLINE).expect("caricamento del kernel");
    if let Some(t) = jit {
        m.set_jit(Some(vetro_jit_native::system_jit(t)));
    }
    let mut vlog = Vec::new();
    let stop = loop {
        let s = m.run(10_000_000);
        vlog.extend(m.console_output());
        if s != Stop::Budget || m.steps >= BUDGET {
            break s;
        }
    };
    let vlog = String::from_utf8_lossy(&vlog).into_owned();
    assert_eq!(
        stop,
        Stop::PowerOff,
        "Vetro non ha finito i kselftest (JIT: {jit:?}); coda:\n{}",
        tail(&vlog)
    );
    (vlog, m.steps)
}

fn tail(log: &str) -> String {
    let n = normalize(log);
    let v: Vec<&str> = n.lines().rev().take(30).collect();
    v.into_iter().rev().collect::<Vec<_>>().join("\n")
}
