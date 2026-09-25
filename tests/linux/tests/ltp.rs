//! LTP (criterio di uscita di M2): una selezione dei test di syscall del
//! Linux Test Project gira su Vetro e su `qemu-aarch64` con lo stesso
//! ambiente; l'esito (codice d'uscita e conteggi TPASS/TFAIL/TBROK/TCONF)
//! deve coincidere. Binari da `tools/ltp/build.sh` (in `target/ltp/bin`).
//!
//! Vetro riceve dall'oracolo versione del kernel e numero di CPU. I test di
//! `tools/ltp/qemu-divergent.txt`, su cui QEMU si discosta da Linux, si
//! confrontano con l'esecuzione nativa su un host Linux aarch64; quelli di
//! `tools/ltp/skip.txt` sono esclusi. Vedi l'ADR 0010.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use vetro_cli::linux::{Config, Exit};
use vetro_diff::qemu;

#[derive(Debug, PartialEq, Eq, Clone)]
struct Esito {
    status: String,
    pass: usize,
    fail: usize,
    brok: usize,
    conf: usize,
}

fn conta(out: &str, status: String) -> Esito {
    let n = |tag: &str| out.lines().filter(|l| l.contains(tag)).count();
    Esito { status, pass: n("TPASS"), fail: n("TFAIL"), brok: n("TBROK"), conf: n("TCONF") }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn skip_list() -> Vec<String> {
    list("tools/ltp/skip.txt")
}

/// Test su cui QEMU user mode si discosta da Linux (tools/ltp/qemu-divergent.txt):
/// l'oracolo è l'esecuzione nativa su un host Linux aarch64.
fn divergent_list() -> Vec<String> {
    list("tools/ltp/qemu-divergent.txt")
}

fn native_oracle() -> bool {
    cfg!(all(target_os = "linux", target_arch = "aarch64"))
}

fn list(file: &str) -> Vec<String> {
    let text = std::fs::read_to_string(root().join(file)).unwrap_or_default();
    text.lines()
        .filter_map(|l| l.split('#').next())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

fn workdir(name: &str) -> PathBuf {
    let d = root().join("target/tmp/ltp").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn env_for(wd: &Path) -> Vec<(String, String)> {
    vec![
        ("TMPDIR".into(), wd.to_string_lossy().into_owned()),
        ("KCONFIG_PATH".into(), root().join("tools/ltp/kconfig").to_string_lossy().into_owned()),
        ("LTP_TIMEOUT_MUL".into(), "4".into()),
        ("PATH".into(), "/bin:/usr/bin".into()),
    ]
}

/// Quello che l'oracolo mostra al guest e che Vetro deve imitare: versione
/// del kernel (uname -r) e CPU disponibili, chiesti a QEMU con BusyBox.
#[derive(Clone)]
struct Host {
    release: String,
    cpus: usize,
}

fn oracle_host(q: &Path) -> Host {
    let fallback = Host { release: "6.6.0-vetro".into(), cpus: 1 };
    let bb = root().join("target/guest-bins/busybox");
    if !bb.is_file() {
        return fallback;
    }
    let args: Vec<String> = ["sh", "-c", "uname -r; nproc"].map(String::from).to_vec();
    let wd = workdir("oracle-host");
    match qemu::run_program_with(q, &[], &bb, &args, &[], &wd, &[], Duration::from_secs(60)) {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout).into_owned();
            let mut l = out.lines();
            let release = l.next().map(str::trim).filter(|r| !r.is_empty()).map(String::from);
            let cpus = l.next().and_then(|n| n.trim().parse().ok());
            match (release, cpus) {
                (Some(release), Some(cpus)) => Host { release, cpus },
                _ => fallback,
            }
        }
        Err(_) => fallback,
    }
}

fn run_vetro(bin: &Path, name: &str, host: &Host) -> Esito {
    let wd = workdir(&format!("{name}.vetro"));
    let env: Vec<String> = env_for(&wd).into_iter().map(|(k, v)| format!("{k}={v}")).collect();
    let envp: Vec<&str> = env.iter().map(String::as_str).collect();
    let cfg = Config {
        cwd: wd.to_string_lossy().into_owned(),
        sysroot: Some(root().join("tools/rootfs").to_string_lossy().into_owned()),
        max_steps: 3_000_000_000,
        cpus: host.cpus,
        release: host.release.clone(),
        ..Config::default()
    }
    .jit_from_env();
    let image = std::fs::read(bin).unwrap();
    let out = vetro_cli::run_elf(&image, &[name], &envp, &bin.to_string_lossy(), cfg).unwrap();
    let status = match out.exit {
        Exit::Code(c) => format!("exit {c}"),
        Exit::Signal { signo, .. } => format!("segnale {signo}"),
        other => format!("{other:?}"),
    };
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    conta(&text, status)
}

/// Esegue su Vetro in un thread a parte: un panic o un blocco (per esempio
/// un'attesa sull'host) diventano l'esito del caso, non della corsa.
fn run_vetro_limited(bin: &Path, name: &str, host: &Host) -> Esito {
    let (tx, rx) = std::sync::mpsc::channel();
    let (bin, name2, host) = (bin.to_path_buf(), name.to_string(), host.clone());
    std::thread::spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_vetro(&bin, &name2, &host)));
        let _ = tx.send(r);
    });
    let vuoto = |s: &str| Esito { status: s.into(), pass: 0, fail: 0, brok: 0, conf: 0 };
    match rx.recv_timeout(Duration::from_secs(180)) {
        Ok(Ok(e)) => e,
        Ok(Err(_)) => vuoto("panic di Vetro"),
        Err(_) => vuoto("timeout di Vetro (180 s)"),
    }
}

fn run_qemu(q: &Path, bin: &Path, name: &str) -> Esito {
    let wd = workdir(&format!("{name}.qemu"));
    let opts = vec!["-L".to_string(), root().join("tools/rootfs").to_string_lossy().into_owned()];
    let out = qemu::run_program_with(q, &opts, bin, &[], &env_for(&wd), &wd, &[], Duration::from_secs(300));
    match out {
        Ok(o) => {
            // QEMU scrive "uncaught target signal" anche quando muore un
            // figlio del test (atteso in molti casi): il segnale conta solo
            // se il processo non è uscito normalmente (codice assente, o
            // 128+N dal wrapper Docker).
            let status = match (o.exit_code, o.signal()) {
                (Some(c), _) if c < 128 => format!("exit {c}"),
                (_, Some(s)) => format!("segnale {s}"),
                (Some(c), None) => format!("exit {c}"),
                (None, None) => "?".into(),
            };
            let text = String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
            conta(&text, status)
        }
        Err(e) => Esito { status: format!("errore {e}"), pass: 0, fail: 0, brok: 0, conf: 0 },
    }
}

/// Esegue il binario direttamente sull'host (solo Linux aarch64).
fn run_native(bin: &Path, name: &str) -> Esito {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    let wd = workdir(&format!("{name}.native"));
    let nofile: u64 = std::env::var("VETRO_ORACLE_NOFILE").ok().and_then(|n| n.parse().ok()).unwrap_or(1024);
    let mut cmd = Command::new(bin);
    // SAFETY: tra fork ed exec solo setpgid e setrlimit, async-signal-safe.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut cmd, move || {
            // Un gruppo di processi a sé: allo scadere si uccidono anche i
            // figli del test, che altrimenti terrebbero aperti stdout/stderr.
            libc::setpgid(0, 0);
            let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
            libc::getrlimit(libc::RLIMIT_NOFILE, &mut r);
            r.rlim_cur = nofile as libc::rlim_t;
            libc::setrlimit(libc::RLIMIT_NOFILE, &r);
            Ok(())
        });
    }
    let mut child = match cmd
        .env_clear()
        .envs(env_for(&wd))
        .current_dir(&wd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Esito { status: format!("errore {e}"), pass: 0, fail: 0, brok: 0, conf: 0 },
    };
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let t_out = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out, &mut v);
        v
    });
    let t_err = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = std::io::Read::read_to_end(&mut err, &mut v);
        v
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if std::time::Instant::now() > deadline {
            // SAFETY: kill di un gruppo di processi che abbiamo creato noi.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let text = String::from_utf8_lossy(&t_out.join().unwrap()).into_owned()
        + &String::from_utf8_lossy(&t_err.join().unwrap());
    let st = match (status.signal(), status.code()) {
        (Some(s), _) => format!("segnale {s}"),
        (None, Some(c)) => format!("exit {c}"),
        _ => "?".into(),
    };
    conta(&text, st)
}

#[test]
fn ltp_matches_qemu() {
    let dir = root().join("target/ltp/bin");
    if !dir.is_dir() {
        if std::env::var("VETRO_REQUIRE_GUEST_BINS").is_ok_and(|v| v == "1") {
            panic!("target/ltp/bin mancante: esegui tools/ltp/build.sh");
        }
        eprintln!("SKIP ltp_matches_qemu: esegui tools/ltp/build.sh");
        return;
    }
    let Some(q) = qemu::locate_or_skip("ltp_matches_qemu") else { return };
    // Il wrapper Docker deve vedere il sysroot e la configurazione.
    let mounts = [root().join("tools"), root().join("target/tmp/ltp")];
    std::fs::create_dir_all(&mounts[1]).unwrap();
    let joined = mounts.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join(":");
    // SAFETY: il test è l'unico thread che tocca l'ambiente a questo punto.
    unsafe { std::env::set_var("VETRO_ORACLE_MOUNTS", joined) };

    // Per default un sottoinsieme rapido (tools/ltp/quick.txt); il giro
    // completo con VETRO_LTP_FULL=1 (job linux della CI, in release).
    let full = std::env::var("VETRO_LTP_FULL").is_ok_and(|v| v == "1");
    let quick = std::fs::read_to_string(root().join("tools/ltp/quick.txt")).unwrap_or_default();
    let only = std::env::var("VETRO_LTP_ONLY").ok().or_else(|| {
        (!full).then(|| {
            quick
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect::<Vec<_>>()
                .join(",")
        })
    });
    // Vetro gira in questo processo e usa un descrittore dell'host per ogni
    // file del guest; l'oracolo invece deve vedere il limite originale.
    let nofile = vetro_cli::raise_fd_limit();
    // SAFETY: il test è l'unico thread che tocca l'ambiente a questo punto.
    unsafe { std::env::set_var("VETRO_ORACLE_NOFILE", nofile.to_string()) };
    let host = oracle_host(&q);
    eprintln!("oracolo: kernel {}, {} CPU", host.release, host.cpus);
    let skip = skip_list();
    let divergent = divergent_list();
    if !native_oracle() && !divergent.is_empty() {
        eprintln!(
            "SKIP {} test di tools/ltp/qemu-divergent.txt: servono un host Linux aarch64 (oracolo nativo)",
            divergent.len()
        );
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !skip.contains(n))
        .filter(|n| native_oracle() || !divergent.contains(n))
        .filter(|n| only.as_ref().is_none_or(|o| o.split(',').any(|x| x == n)))
        .collect();
    names.sort();
    // I test di temporizzazione girano da soli alla fine (tools/ltp/timing.txt).
    let timing = list("tools/ltp/timing.txt");
    let (serial, parallel): (Vec<String>, Vec<String>) =
        names.iter().cloned().partition(|n| timing.contains(n));
    let results = std::sync::Mutex::new(BTreeMap::new());
    let run_one = |name: &String| {
        let bin = dir.join(name);
        // Un panic nel kernel emulato è un fallimento di questo caso, non
        // di tutta la corsa.
        let ours = run_vetro_limited(&bin, name, &host);
        let oracle =
            || if divergent.contains(name) { run_native(&bin, name) } else { run_qemu(&q, &bin, name) };
        // Vetro è deterministico, l'oracolo no (tempi reali su un host
        // carico): se diverge si riprova fino a due volte.
        let mut theirs = oracle();
        for _ in 0..2 {
            if theirs == ours {
                break;
            }
            theirs = oracle();
        }
        results.lock().unwrap().insert(name.clone(), (ours, theirs));
    };
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(2);
    let queue = std::sync::Mutex::new(parallel);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let Some(name) = queue.lock().unwrap().pop() else { break };
                    run_one(&name);
                }
            });
        }
    });
    for name in &serial {
        run_one(name);
    }
    let results = results.into_inner().unwrap();
    let mut diff = Vec::new();
    let (mut ok_pass, mut total_pass) = (0, 0);
    for (name, (ours, theirs)) in &results {
        total_pass += theirs.pass;
        if ours == theirs {
            ok_pass += ours.pass;
        } else {
            let who = if divergent.contains(name) { "nativo" } else { "qemu  " };
            diff.push(format!("  {name}: vetro ={ours:?}\n  {:>w$}  {who}={theirs:?}", "", w = name.len()));
        }
    }
    eprintln!(
        "LTP: {} test, {} identici a QEMU, {} diversi; TPASS negli identici: {ok_pass} su {total_pass}",
        results.len(),
        results.len() - diff.len(),
        diff.len()
    );
    assert!(diff.is_empty(), "LTP diverge da QEMU:\n{}", diff.join("\n"));
}
