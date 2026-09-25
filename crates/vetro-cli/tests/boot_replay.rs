//! `vetro boot --record`, `--replay`, `--goto` e `--dump` (M10, ADR 0019):
//! una sessione alla shell del kernel guest registrata da stdin (i byte
//! arrivano quando li legge il thread dell'host, cioè in tempi che dipendono
//! dall'host) si rifà in altri processi, senza stdin, dall'avvio o dal
//! keyframe iniziale, con l'interprete e col JIT: stessa uscita e "replay
//! identico". `--goto` stampa gli stessi registri che dà `Machine::goto` nel
//! processo del test, e `--dump` gli stessi byte. Un log con un ingresso in
//! meno dà un replay diverso (codice 1).
//!
//! In release (`cargo test --release -p vetro-cli`), come gli altri avvii.

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
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "avvio sotto Vetro solo in release");
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

    // Registrazione: comandi da stdin, poi spegnimento.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vetro"));
    cmd.arg("boot")
        .args(&kernel)
        .args(["--mem=512", "--keyframes=40000000"])
        .arg(format!("--record={}", rec.display()));
    let mut c = Console::spawn(cmd).expect("vetro boot --record");
    let at = c.wait_for(SHELL_PROMPT, 0, timeout()).unwrap_or_else(|| panic!("niente shell:\n{}", c.log()));
    c.send("echo \"V\"ETRO-$((6*7))\n");
    let at = c.wait_for("VETRO-42", at, timeout()).unwrap_or_else(|| panic!("niente eco:\n{}", c.log()));
    c.wait_for(SHELL_PROMPT, at, timeout()).unwrap();
    c.send("poweroff -f\n");
    assert!(c.finish(Duration::from_secs(120)), "poweroff -f non ha fermato vetro:\n{}", c.log());
    let recorded = c.log();
    let log = Log::decode(&std::fs::read(&rec).expect("log scritto")).expect("log valido");
    assert!(
        !log.events.is_empty() && log.keyframes.len() >= 2,
        "{} eventi, {:?}",
        log.events.len(),
        log.keyframes
    );

    // Replay: dal keyframe iniziale (senza kernel), dall'avvio, col JIT.
    let replay = format!("--replay={}", rec.display());
    for (what, extra) in
        [("dal keyframe", vec![]), ("dall'avvio", kernel.clone()), ("col JIT", vec!["--jit".to_string()])]
    {
        let mut args = extra;
        args.push("--mem=512".to_string());
        args.push(replay.clone());
        let out = vetro(&args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{what}: {err}");
        assert!(err.contains("replay identico"), "{what}: {err}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout == recorded, "{what}: uscita diversa:\n{}", normalize(&stdout));
    }

    // Salto a un'istruzione: gli stessi registri del salto nel processo.
    let target = log.end.steps / 2;
    assert_eq!(log.config, MachineConfig { ram_size: 512 << 20, ..MachineConfig::default() });
    let mut m = Machine::with_devices(&log.config, &Devices::default());
    assert!(m.goto(&log, target).unwrap() >= target);
    let regs = m.registers_text();
    let mut code = [0u8; 32];
    m.read_virt(m.cpu.pc, &mut code).expect("il codice al PC è mappato");
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

    // Un ingresso in meno: il replay diverge e lo dice.
    let mut cut = log.clone();
    let i = cut.events.len() / 2;
    cut.events.remove(i);
    let bad = dir.join("tagliata.vrec");
    std::fs::write(&bad, cut.encode()).unwrap();
    let out = vetro(&["--mem=512".to_string(), format!("--replay={}", bad.display())]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("replay diverso"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}
