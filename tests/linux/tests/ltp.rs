//! LTP (criterio di uscita di M2): una selezione dei test di syscall del
//! Linux Test Project gira su Vetro e su `qemu-aarch64` con lo stesso
//! ambiente; l'esito (codice d'uscita e conteggi TPASS/TFAIL/TBROK/TCONF)
//! deve coincidere. Binari da `tools/ltp/build.sh` (in `target/ltp/bin`).

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
    let text = std::fs::read_to_string(root().join("tools/ltp/skip.txt")).unwrap_or_default();
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

fn run_vetro(bin: &Path, name: &str) -> Esito {
    let wd = workdir(&format!("{name}.vetro"));
    let env: Vec<String> = env_for(&wd).into_iter().map(|(k, v)| format!("{k}={v}")).collect();
    let envp: Vec<&str> = env.iter().map(String::as_str).collect();
    let cfg = Config {
        cwd: wd.to_string_lossy().into_owned(),
        sysroot: Some(root().join("tools/rootfs").to_string_lossy().into_owned()),
        max_steps: 3_000_000_000,
        ..Config::default()
    };
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

fn run_qemu(q: &Path, bin: &Path, name: &str) -> Esito {
    let wd = workdir(&format!("{name}.qemu"));
    let opts = vec!["-L".to_string(), root().join("tools/rootfs").to_string_lossy().into_owned()];
    let out = qemu::run_program_with(q, &opts, bin, &[], &env_for(&wd), &wd, &[], Duration::from_secs(300));
    match out {
        Ok(o) => {
            let status = match (o.signal(), o.exit_code) {
                (Some(s), _) => format!("segnale {s}"),
                (None, Some(c)) => format!("exit {c}"),
                _ => "?".into(),
            };
            let text = String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
            conta(&text, status)
        }
        Err(e) => Esito { status: format!("errore {e}"), pass: 0, fail: 0, brok: 0, conf: 0 },
    }
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

    let only = std::env::var("VETRO_LTP_ONLY").ok();
    let skip = skip_list();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !skip.contains(n))
        .filter(|n| only.as_ref().is_none_or(|o| o.split(',').any(|x| x == n)))
        .collect();
    names.sort();
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(4);
    let queue = std::sync::Mutex::new(names.clone());
    let results = std::sync::Mutex::new(BTreeMap::new());
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let Some(name) = queue.lock().unwrap().pop() else { break };
                    let bin = dir.join(&name);
                    // Un panic nel kernel emulato è un fallimento di questo
                    // caso, non di tutta la corsa.
                    let ours =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_vetro(&bin, &name)))
                            .unwrap_or_else(|_| Esito {
                                status: "panic di Vetro".into(),
                                pass: 0,
                                fail: 0,
                                brok: 0,
                                conf: 0,
                            });
                    let theirs = run_qemu(&q, &bin, &name);
                    results.lock().unwrap().insert(name, (ours, theirs));
                }
            });
        }
    });
    let results = results.into_inner().unwrap();
    let mut diff = Vec::new();
    let (mut ok_pass, mut total_pass) = (0, 0);
    for (name, (ours, theirs)) in &results {
        total_pass += theirs.pass;
        if ours == theirs {
            ok_pass += ours.pass;
        } else {
            diff.push(format!("  {name}: vetro={ours:?}\n  {:>w$}  qemu ={theirs:?}", "", w = name.len()));
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
