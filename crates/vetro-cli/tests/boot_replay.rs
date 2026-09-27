//! `vetro boot --record`, `--replay`, `--goto` and `--dump` (M10, ADR 0019):
//! a session at the guest kernel's shell recorded from stdin (the bytes
//! arrive when the host thread reads them, i.e. at times that depend
//! on the host) is redone in other processes, without stdin, from boot or from the
//! initial keyframe, with the interpreter and with the JIT: same output and "identical
//! replay". `--goto` prints the same registers that `Machine::goto` gives in the
//! test's process, and `--dump` the same bytes. A log with one input
//! fewer gives a different replay (code 1).
//!
//! In release (`cargo test --release -p vetro-cli`), like the other boots.

use std::process::{Command, Output, Stdio};
use std::time::Duration;

use vetro_boot_tests::{Console, SHELL_PROMPT, guest_kernel, normalize, skip_or_fail, timeout};
use vetro_machine::{Devices, Log, Machine, MachineConfig};

fn vetro(args: &[String]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vetro"))
        .arg("boot")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("vetro boot")
}

#[test]
fn registra_rifa_e_salta() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "boot under Vetro only in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel mancante");
    };
    let dir = std::env::temp_dir().join(format!("vetro-boot-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let rec = dir.join("sessione.vrec");
    let kernel = vec![
        format!("--kernel={}", image.display()),
        format!("--initrd={}", initrd.display()),
        "--append=console=ttyAMA0 vetro.noautotest".to_string(),
    ];

    // Recording: commands from stdin, then power-off.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vetro"));
    cmd.arg("boot")
        .args(&kernel)
        .args(["--mem=512", "--keyframes=40000000"])
        .arg(format!("--record={}", rec.display()));
    let mut c = Console::spawn(cmd).expect("vetro boot --record");
    let at = c.wait_for(SHELL_PROMPT, 0, timeout()).unwrap_or_else(|| panic!("no shell:\n{}", c.log()));
    c.send("echo \"V\"ETRO-$((6*7))\n");
    let at = c.wait_for("VETRO-42", at, timeout()).unwrap_or_else(|| panic!("no echo:\n{}", c.log()));
    c.wait_for(SHELL_PROMPT, at, timeout()).unwrap();
    c.send("poweroff -f\n");
    assert!(c.finish(Duration::from_secs(120)), "poweroff -f did not stop vetro:\n{}", c.log());
    let recorded = c.log();
    let log = Log::decode(&std::fs::read(&rec).expect("log scritto")).expect("log valido");
    assert!(
        !log.events.is_empty() && log.keyframes.len() >= 2,
        "{} eventi, {:?}",
        log.events.len(),
        log.keyframes
    );

    // Replay: from the initial keyframe (without a kernel), from boot, with the JIT.
    let replay = format!("--replay={}", rec.display());
    for (what, extra) in [
        ("from the keyframe", vec![]),
        ("from boot", kernel.clone()),
        ("with the JIT", vec!["--jit".to_string()]),
    ] {
        let mut args = extra;
        args.push("--mem=512".to_string());
        args.push(replay.clone());
        let out = vetro(&args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{what}: {err}");
        assert!(err.contains("replay identical"), "{what}: {err}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout == recorded, "{what}: uscita diversa:\n{}", normalize(&stdout));
    }

    // Jump to an instruction: the same registers as the jump in the process.
    let target = log.end.steps / 2;
    assert_eq!(log.config, MachineConfig { ram_size: 512 << 20, ..MachineConfig::default() });
    let mut m = Machine::with_devices(&log.config, &Devices::default());
    assert!(m.goto(&log, target).unwrap() >= target);
    let regs = m.registers_text();
    let mut code = [0u8; 32];
    m.read_virt(m.cpu.pc, &mut code).expect("the code at the PC is mapped");
    for jit in [false, true] {
        let mut args = vec![replay.clone(), format!("--goto={target}"), format!("--dump={:#x}:32", m.cpu.pc)];
        if jit {
            args.push("--jit".to_string());
        }
        let out = vetro(&args);
        assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.starts_with(&regs), "--goto (JIT {jit}):\n{stdout}\natteso:\n{regs}");
        let hex: Vec<String> = code[..16].iter().map(|b| format!("{b:02x}")).collect();
        assert!(stdout.contains(&hex.join(" ")), "--dump (JIT {jit}):\n{stdout}");
    }

    // One input fewer: the replay diverges and says so.
    let mut cut = log.clone();
    let i = cut.events.len() / 2;
    cut.events.remove(i);
    let bad = dir.join("tagliata.vrec");
    std::fs::write(&bad, cut.encode()).unwrap();
    let out = vetro(&["--mem=512".to_string(), format!("--replay={}", bad.display())]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("replay differs"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}
