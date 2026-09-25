//! Criterio di uscita di M3: il kernel guest si avvia sotto Vetro fino alla
//! shell, con lo stesso copione del test sotto QEMU (`qemu.rs`): marcatore di
//! `/init`, autotest senza errori, un comando scritto sulla console, `poweroff
//! -f` via PSCI. Il log, senza tempi e senza le differenze note
//! (`KNOWN_DIFFERENCES`), deve coincidere con quello di QEMU sugli stessi
//! file: `target/guest-kernel/qemu-boot.log`, scritto dal test `qemu` che
//! cargo esegue prima, se è più recente di kernel e initramfs; altrimenti il
//! riferimento versionato `guest/kernel/reference/qemu-boot.log`.
//!
//! I limiti sono in istruzioni, non in secondi: la macchina è deterministica.
//! Va eseguito in release (`cargo test --release -p vetro-boot-tests`): in
//! debug l'interprete è troppo lento e il test si salta.
//!
//! Con `VETRO_JIT=1` (`VETRO_JIT_THRESHOLD=N` per la soglia) lo stesso
//! avvio gira anche col JIT della modalità sistema (wasmtime, ADR 0013), e
//! deve dare esattamente le stesse istruzioni e lo stesso log, byte per
//! byte, dell'interprete (`target/guest-kernel/vetro-boot-jit.log`).

use vetro_boot_tests::*;
use vetro_machine::{Machine, MachineConfig, Stop};

/// Istruzioni concesse a ogni fase (a 100 MHz nominali, 60 s di guest).
const PHASE_BUDGET: u64 = 6_000_000_000;

struct Run {
    m: Machine,
    log: Vec<u8>,
}

impl Run {
    /// Esegue finché `needle` compare nel log dopo `from`; restituisce la
    /// posizione subito dopo, o un errore con il motivo dell'arresto.
    fn until(&mut self, needle: &str, from: usize) -> Result<usize, String> {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            if let Some(i) = find(&self.log[from.min(self.log.len())..], needle.as_bytes()) {
                return Ok(from + i + needle.len());
            }
            if self.m.steps >= limit {
                return Err(format!("{needle:?} non arrivato entro {PHASE_BUDGET} istruzioni"));
            }
            let stop = self.m.run(1_000_000);
            self.log.extend(self.m.console_output());
            match stop {
                Stop::Budget => {}
                Stop::Idle => return Err(format!("guest inattivo in attesa di {needle:?}")),
                other => return Err(format!("{other:?} in attesa di {needle:?}")),
            }
        }
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

/// Esito di un avvio completo: log normalizzato, istruzioni, tempo del guest
/// al marcatore di `/init` e allo spegnimento.
struct Boot {
    log: String,
    steps: u64,
    t_boot: u64,
    t_end: u64,
}

/// Il copione completo, con o senza JIT (soglia).
fn boot(image: &[u8], initrd: &[u8], jit: Option<u32>) -> Boot {
    let mut m = Machine::new(&MachineConfig::default());
    m.load_linux(image, Some(initrd), "console=ttyAMA0").expect("caricamento del kernel");
    if let Some(t) = jit {
        m.set_jit(Some(vetro_jit_native::system_jit(t)));
    }
    let mut r = Run { m, log: Vec::new() };
    let fail = |r: &Run, e: String| -> ! { panic!("{e}; ultime righe della console:\n{}", r.tail()) };

    let at = r.until(BOOT_MARKER, 0).unwrap_or_else(|e| fail(&r, e));
    let t_boot = r.m.guest_ns();
    // Fino alla fine della riga: un blocco di istruzioni può finire a metà.
    let at_end = r.until(AUTOTEST_END, at).unwrap_or_else(|e| fail(&r, e));
    let end = r.until("\n", at_end).unwrap_or_else(|e| fail(&r, e));
    let line = String::from_utf8_lossy(&r.log[at_end - AUTOTEST_END.len()..end]).into_owned();
    assert_eq!(line.trim_end(), AUTOTEST_OK, "autotest con errori:\n{}", r.tail());
    // Stesso copione del test di QEMU: ingresso solo a prompt completo.
    let prompt = r.until(SHELL_PROMPT, end).unwrap_or_else(|e| fail(&r, e));
    r.m.console_input(b"echo VETRO-SHELL-$((6*7))\n");
    let out = r.until("VETRO-SHELL-42", prompt).unwrap_or_else(|e| fail(&r, e));
    r.until(SHELL_PROMPT, out).unwrap_or_else(|e| fail(&r, e));
    r.m.console_input(b"poweroff -f\n");
    let limit = r.m.steps + PHASE_BUDGET;
    let stop = loop {
        let s = r.m.run(1_000_000);
        r.log.extend(r.m.console_output());
        if s != Stop::Budget || r.m.steps >= limit {
            break s;
        }
    };
    assert_eq!(stop, Stop::PowerOff, "poweroff -f non ha spento la macchina:\n{}", r.tail());
    if let Some(s) = r.m.jit_stats() {
        eprintln!("JIT: {s:?}");
    }
    let log = normalize(&String::from_utf8_lossy(&r.log));
    Boot { log, steps: r.m.steps, t_boot, t_end: r.m.guest_ns() }
}

fn jit_threshold() -> Option<u32> {
    if !std::env::var("VETRO_JIT").is_ok_and(|v| v == "1") {
        return None;
    }
    Some(std::env::var("VETRO_JIT_THRESHOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(16))
}

#[test]
fn vetro_boots_guest_kernel_to_shell() {
    if cfg!(debug_assertions) {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "avvio sotto Vetro solo in release (cargo test --release)",
        );
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel mancante: esegui tools/guest-kernel/build.sh",
        );
    };
    let (image, initrd) = (std::fs::read(image).unwrap(), std::fs::read(initrd).unwrap());
    let t0 = std::time::Instant::now();
    let b = boot(&image, &initrd, None);
    let interp_time = t0.elapsed();
    let log = b.log;
    let root = repo_root();
    std::fs::write(root.join("target/guest-kernel/vetro-boot.log"), &log).unwrap();
    eprintln!(
        "Vetro: /init a {:.2} s di guest, spento a {:.2} s ({} istruzioni)",
        b.t_boot as f64 / 1e9,
        b.t_end as f64 / 1e9,
        b.steps
    );
    eprintln!("interprete: {:.2} s", interp_time.as_secs_f64());

    if let Some(t) = jit_threshold() {
        let t0 = std::time::Instant::now();
        let j = boot(&image, &initrd, Some(t));
        let jit_time = t0.elapsed();
        std::fs::write(root.join("target/guest-kernel/vetro-boot-jit.log"), &j.log).unwrap();
        eprintln!(
            "JIT (soglia {t}): {:.2} s, {} istruzioni (interprete {:.2} s)",
            jit_time.as_secs_f64(),
            j.steps,
            interp_time.as_secs_f64()
        );
        assert_eq!(j.steps, b.steps, "istruzioni diverse col JIT");
        assert_eq!((j.t_boot, j.t_end), (b.t_boot, b.t_end), "tempi del guest diversi col JIT");
        if j.log != log {
            let (a, c): (Vec<&str>, Vec<&str>) = (log.lines().collect(), j.log.lines().collect());
            let i = a.iter().zip(&c).position(|(x, y)| x != y).unwrap_or(a.len().min(c.len()));
            panic!(
                "log diverso col JIT dalla riga {}:\ninterprete: {:?}\nJIT:        {:?}",
                i + 1,
                a.get(i),
                c.get(i)
            );
        }
    }

    // Confronto con l'avvio sotto QEMU degli stessi file, se c'è.
    let (ref_path, reference) = qemu_reference(&root);
    eprintln!("confronto con {}", ref_path.display());
    let (ours, theirs) = (comparable_lines(&log), comparable_lines(&reference));
    let diff = line_diff(&theirs, &ours);
    assert!(
        diff.is_empty(),
        "il log di Vetro differisce da {} (- solo QEMU, + solo Vetro):\n{}",
        ref_path.display(),
        diff.join("\n")
    );
}

/// Log di QEMU con cui confrontarsi: quello appena scritto dal test `qemu`
/// sugli stessi file, o il riferimento versionato.
fn qemu_reference(root: &std::path::Path) -> (std::path::PathBuf, String) {
    let fresh = root.join("target/guest-kernel/qemu-boot.log");
    let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let inputs = ["Image", "initramfs.cpio.gz"].map(|f| mtime(&root.join("target/guest-kernel").join(f)));
    if let Some(t) = mtime(&fresh)
        && inputs.iter().all(|i| i.is_some_and(|i| i <= t))
    {
        return (fresh.clone(), std::fs::read_to_string(&fresh).unwrap());
    }
    let r = root.join("guest/kernel/reference/qemu-boot.log");
    let text = std::fs::read_to_string(&r)
        .expect("guest/kernel/reference/qemu-boot.log (VETRO_BOOT_UPDATE_REFERENCE=1 nel test di QEMU)");
    (r, text)
}
