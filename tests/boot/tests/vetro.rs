//! M3 exit criterion: the guest kernel boots under Vetro up to the
//! shell, with the same script as the test under QEMU (`qemu.rs`): `/init`
//! marker, self-test without errors, a command written on the console, `poweroff
//! -f` via PSCI. The log, without timings and without the known differences
//! (`KNOWN_DIFFERENCES`), must match QEMU's on the same
//! files: `target/guest-kernel/qemu-boot.log`, written by the `qemu` test that
//! cargo runs first, if it is newer than kernel and initramfs; otherwise the
//! versioned reference `guest/kernel/reference/qemu-boot.log`.
//!
//! The limits are in instructions, not in seconds: the machine is deterministic.
//! It must be run in release (`cargo test --release -p vetro-boot-tests`): in
//! debug the interpreter is too slow and the test is skipped.
//!
//! With `VETRO_JIT=1` (`VETRO_JIT_THRESHOLD=N` for the threshold) the same
//! boot also runs with the system-mode JIT (wasmtime, ADR 0013), and
//! must give exactly the same instructions and the same log, byte for
//! byte, as the interpreter (`target/guest-kernel/vetro-boot-jit.log`).

use vetro_boot_tests::*;
use vetro_machine::{Machine, MachineConfig, Stop};

/// Instructions granted to each phase (at a nominal 100 MHz, 60 s of guest time).
const PHASE_BUDGET: u64 = 6_000_000_000;

struct Run {
    m: Machine,
    log: Vec<u8>,
    /// Instructions per `Machine::run` (the host's quantum).
    quantum: u64,
}

impl Run {
    /// Runs until `needle` appears in the log after `from`; returns the
    /// position right after it, or an error with the reason for the stop.
    fn until(&mut self, needle: &str, from: usize) -> Result<usize, String> {
        let limit = self.m.steps + PHASE_BUDGET;
        loop {
            if let Some(i) = find(&self.log[from.min(self.log.len())..], needle.as_bytes()) {
                return Ok(from + i + needle.len());
            }
            if self.m.steps >= limit {
                return Err(format!("{needle:?} did not arrive within {PHASE_BUDGET} instructions"));
            }
            let stop = self.m.run(self.quantum);
            self.log.extend(self.m.console_output());
            match stop {
                Stop::Budget => {}
                Stop::Idle => return Err(format!("guest idle while waiting for {needle:?}")),
                other => return Err(format!("{other:?} while waiting for {needle:?}")),
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

/// Outcome of a complete boot: normalised log, instructions, guest time
/// at the `/init` marker and at power-off.
struct Boot {
    log: String,
    steps: u64,
    t_boot: u64,
    t_end: u64,
}

/// The complete script, with or without the JIT (threshold).
fn boot(image: &[u8], initrd: &[u8], jit: Option<u32>) -> Boot {
    boot_with(image, initrd, jit, 1, 1_000_000)
}

/// [`boot`] with `cpus` cores and the host's quantum.
fn boot_with(image: &[u8], initrd: &[u8], jit: Option<u32>, cpus: u32, quantum: u64) -> Boot {
    let mut m = Machine::new(&MachineConfig { cpus, ..MachineConfig::default() });
    m.load_linux(image, Some(initrd), "console=ttyAMA0").expect("loading the kernel");
    if let Some(t) = jit {
        m.set_jit(Some(vetro_jit_native::system_jit(t)));
    }
    let mut r = Run { m, log: Vec::new(), quantum };
    script(&mut r)
}

/// ADR 0042: the boot with `cpus` cores in parallel on host threads (core 0
/// on this thread), with the JIT on every core if `jit`. Not deterministic:
/// only the outcome and the log's lines count.
fn boot_parallel(image: &[u8], initrd: &[u8], jit: Option<u32>, cpus: u32) -> Boot {
    let mut m = Machine::new(&MachineConfig { cpus, ..MachineConfig::default() });
    m.load_linux(image, Some(initrd), "console=ttyAMA0").expect("loading the kernel");
    if let Some(t) = jit {
        m.set_jit(Some(vetro_jit_native::system_jit(t)));
    }
    let cores = m.start_parallel().expect("parallel");
    let mut r = Run { m, log: Vec::new(), quantum: 1_000_000 };
    let (b, executed) = std::thread::scope(|s| {
        let handles: Vec<_> = cores
            .into_iter()
            .map(|mut c| {
                s.spawn(move || {
                    c.set_jit(jit.map(vetro_jit_native::system_jit));
                    while !c.stopped() {
                        match c.run(1_000_000) {
                            Stop::Budget | Stop::Idle => {}
                            Stop::PowerOff | Stop::Reset => break,
                            other => panic!("core {}: {other:?}", c.index()),
                        }
                    }
                    c.drop_jit();
                    c
                })
            })
            .collect();
        let b = script(&mut r);
        r.m.request_stop();
        let cores: Vec<_> = handles.into_iter().map(|h| h.join().expect("core thread")).collect();
        let executed: Vec<u64> = cores.iter().map(|c| c.executed()).collect();
        r.m.stop_parallel(cores);
        (b, executed)
    });
    eprintln!("parallel: instructions of cores 1..: {executed:?}");
    b
}

/// The script of the test on a machine ready to boot.
fn script(r: &mut Run) -> Boot {
    let fail = |r: &Run, e: String| -> ! { panic!("{e}; last lines of the console:\n{}", r.tail()) };

    let at = r.until(BOOT_MARKER, 0).unwrap_or_else(|e| fail(r, e));
    let t_boot = r.m.guest_ns();
    // Up to the end of the line: a block of instructions can end halfway.
    let at_end = r.until(AUTOTEST_END, at).unwrap_or_else(|e| fail(r, e));
    let end = r.until("\n", at_end).unwrap_or_else(|e| fail(r, e));
    let line = String::from_utf8_lossy(&r.log[at_end - AUTOTEST_END.len()..end]).into_owned();
    assert_eq!(line.trim_end(), AUTOTEST_OK, "self-test with errors:\n{}", r.tail());
    // Same script as the QEMU test: input only at a complete prompt.
    let prompt = r.until(SHELL_PROMPT, end).unwrap_or_else(|e| fail(r, e));
    r.m.console_input(b"echo VETRO-SHELL-$((6*7))\n");
    let out = r.until("VETRO-SHELL-42", prompt).unwrap_or_else(|e| fail(r, e));
    r.until(SHELL_PROMPT, out).unwrap_or_else(|e| fail(r, e));
    r.m.console_input(b"poweroff -f\n");
    let limit = r.m.steps + PHASE_BUDGET;
    let stop = loop {
        let s = r.m.run(r.quantum);
        r.log.extend(r.m.console_output());
        if s != Stop::Budget || r.m.steps >= limit {
            break s;
        }
    };
    assert_eq!(stop, Stop::PowerOff, "poweroff -f did not power off the machine:\n{}", r.tail());
    if let Some(s) = r.m.jit_stats() {
        eprintln!("JIT: {s:?}");
    }
    let log = normalize(&String::from_utf8_lossy(&r.log));
    Boot { log, steps: r.m.steps, t_boot, t_end: r.m.guest_ns() }
}

/// ADR 0042: the same script with the two cores in parallel on two host
/// threads, with the interpreter and (`VETRO_JIT=1`) the JIT on each: the
/// self-test passes, the shell answers, the machine powers off, and the log's
/// lines are QEMU's with `-smp 2` (the comparison by set of lines of
/// `qemu-boot.log`).
#[test]
fn vetro_boots_guest_kernel_parallel2() {
    if cfg!(debug_assertions) {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "boot under Vetro only in release (cargo test --release)",
        );
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel missing: run tools/guest-kernel/build.sh",
        );
    };
    let (image, initrd) = (std::fs::read(image).unwrap(), std::fs::read(initrd).unwrap());
    let root = repo_root();
    let (_, reference) = qemu_reference(&root, "qemu-boot-smp2.log");
    let mut runs: Vec<Option<u32>> = vec![None];
    if let Some(t) = jit_threshold() {
        runs.push(Some(t));
    }
    for jit in runs {
        let t0 = std::time::Instant::now();
        let b = boot_parallel(&image, &initrd, jit, 2);
        eprintln!(
            "Vetro, 2 cores in parallel (JIT {jit:?}): powered off at {:.2} s of guest time ({} instructions, {:.2} s)",
            b.t_end as f64 / 1e9,
            b.steps,
            t0.elapsed().as_secs_f64()
        );
        std::fs::write(root.join("target/guest-kernel/vetro-boot-par2.log"), &b.log).unwrap();
        assert!(
            b.log.contains("smp: Brought up 1 node, 2 CPUs"),
            "the second core did not come up:\n{}",
            b.log
        );
        let (ours, theirs) = (comparable_lines(&b.log), comparable_lines(&reference));
        let diff = line_diff(&theirs, &ours);
        assert!(
            diff.is_empty(),
            "parallel two-core log differs from QEMU's (- QEMU only, + Vetro only):\n{}",
            diff.join("\n")
        );
    }
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
            "boot under Vetro only in release (cargo test --release)",
        );
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel missing: run tools/guest-kernel/build.sh",
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
        "Vetro: /init at {:.2} s of guest time, powered off at {:.2} s ({} instructions)",
        b.t_boot as f64 / 1e9,
        b.t_end as f64 / 1e9,
        b.steps
    );
    eprintln!("interpreter: {:.2} s", interp_time.as_secs_f64());

    if let Some(t) = jit_threshold() {
        let t0 = std::time::Instant::now();
        let j = boot(&image, &initrd, Some(t));
        let jit_time = t0.elapsed();
        std::fs::write(root.join("target/guest-kernel/vetro-boot-jit.log"), &j.log).unwrap();
        eprintln!(
            "JIT (threshold {t}): {:.2} s, {} instructions (interpreter {:.2} s)",
            jit_time.as_secs_f64(),
            j.steps,
            interp_time.as_secs_f64()
        );
        assert_eq!(j.steps, b.steps, "instructions differ with the JIT");
        assert_eq!((j.t_boot, j.t_end), (b.t_boot, b.t_end), "guest times different with the JIT");
        if j.log != log {
            let (a, c): (Vec<&str>, Vec<&str>) = (log.lines().collect(), j.log.lines().collect());
            let i = a.iter().zip(&c).position(|(x, y)| x != y).unwrap_or(a.len().min(c.len()));
            panic!(
                "log different with the JIT from line {}:\ninterpreter: {:?}\nJIT:         {:?}",
                i + 1,
                a.get(i),
                c.get(i)
            );
        }
    }

    // Comparison with the boot under QEMU of the same files, if there is one.
    let (ref_path, reference) = qemu_reference(&root, "qemu-boot.log");
    eprintln!("comparing with {}", ref_path.display());
    let (ours, theirs) = (comparable_lines(&log), comparable_lines(&reference));
    let diff = line_diff(&theirs, &ours);
    assert!(
        diff.is_empty(),
        "Vetro's log differs from {} (- QEMU only, + Vetro only):\n{}",
        ref_path.display(),
        diff.join("\n")
    );
}

/// ADR 0042: the same script on two cores in turns on one thread
/// (deterministic). The secondary core comes up through PSCI CPU_ON; the log
/// must match QEMU's with `-smp 2 -accel tcg,thread=single`; another host
/// quantum gives the same instructions and log (the interleaving depends only
/// on the clock), and so does the JIT (`VETRO_JIT=1`).
#[test]
fn vetro_boots_guest_kernel_smp2() {
    if cfg!(debug_assertions) {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "boot under Vetro only in release (cargo test --release)",
        );
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel missing: run tools/guest-kernel/build.sh",
        );
    };
    let (image, initrd) = (std::fs::read(image).unwrap(), std::fs::read(initrd).unwrap());
    let t0 = std::time::Instant::now();
    let b = boot_with(&image, &initrd, None, 2, 1_000_000);
    eprintln!(
        "Vetro, 2 cores: /init at {:.2} s of guest time, powered off at {:.2} s ({} instructions, {:.2} s)",
        b.t_boot as f64 / 1e9,
        b.t_end as f64 / 1e9,
        b.steps,
        t0.elapsed().as_secs_f64()
    );
    let root = repo_root();
    std::fs::write(root.join("target/guest-kernel/vetro-boot-smp2.log"), &b.log).unwrap();
    assert!(
        b.log.contains("smp: Brought up 1 node, 2 CPUs"),
        "the second core did not come up:
{}",
        b.log
    );

    let q = boot_with(&image, &initrd, None, 2, 999_983);
    // (`t_boot` is read at the end of the host's quantum that printed the marker.)
    assert_eq!((q.steps, q.t_end), (b.steps, b.t_end), "another host quantum");
    assert!(q.log == b.log, "another host quantum changes the log");
    if let Some(t) = jit_threshold() {
        let j = boot_with(&image, &initrd, Some(t), 2, 1_000_000);
        assert_eq!(
            (j.steps, j.t_boot, j.t_end),
            (b.steps, b.t_boot, b.t_end),
            "instructions differ with the JIT"
        );
        assert!(j.log == b.log, "log different with the JIT on two cores");
    }

    let (ref_path, reference) = qemu_reference(&root, "qemu-boot-smp2.log");
    eprintln!("comparing with {}", ref_path.display());
    let (ours, theirs) = (comparable_lines(&b.log), comparable_lines(&reference));
    let diff = line_diff(&theirs, &ours);
    assert!(
        diff.is_empty(),
        "Vetro's two-core log differs from {} (- QEMU only, + Vetro only):\n{}",
        ref_path.display(),
        diff.join("\n")
    );
}

/// QEMU log `name` to compare with: the one just written by the `qemu`
/// test on the same files, or the versioned reference.
fn qemu_reference(root: &std::path::Path, name: &str) -> (std::path::PathBuf, String) {
    let fresh = root.join("target/guest-kernel").join(name);
    let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let inputs = ["Image", "initramfs.cpio.gz"].map(|f| mtime(&root.join("target/guest-kernel").join(f)));
    if let Some(t) = mtime(&fresh)
        && inputs.iter().all(|i| i.is_some_and(|i| i <= t))
    {
        return (fresh.clone(), std::fs::read_to_string(&fresh).unwrap());
    }
    let r = root.join("guest/kernel/reference").join(name);
    let text = std::fs::read_to_string(&r).unwrap_or_else(|_| {
        panic!("guest/kernel/reference/{name} (VETRO_BOOT_UPDATE_REFERENCE=1 in the QEMU test)")
    });
    (r, text)
}
