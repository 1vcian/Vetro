//! Criterio di uscita M1 (ADR 0006): programmi casuali di istruzioni intere,
//! stato finale di Vetro identico a `qemu-aarch64 -cpu cortex-a53`.
//!
//! - `VETRO_DIFF_CASES`: numero di programmi (default 250).
//! - `VETRO_DIFF_SEED`: primo seme (default 0); i casi usano semi
//!   consecutivi, quindi un caso fallito si riproduce con
//!   `VETRO_DIFF_SEED=<seme> VETRO_DIFF_CASES=1`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use vetro_diff::harness::{Program, compare, run_qemu, run_vetro};
use vetro_diff::qemu;
use vetro_diff::random::{Case, generate_fp_focused, generate_with};

const BODY_LEN: usize = 48;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn describe(p: &Program) -> String {
    let mut s = String::new();
    for (i, w) in p.body.iter().enumerate() {
        s += &format!(
            "    {:#x}: {w:08x}  {:?}\n",
            vetro_diff::elf::insn_addr(Program::body_index(i)),
            vetro_cpu::decode(*w)
        );
    }
    s
}

#[test]
fn random_programs_match_qemu() {
    run("random_programs_match_qemu", false, env_u64("VETRO_DIFF_CASES", 250));
}

/// Stesso confronto con le classi SIMD e virgola mobile.
#[test]
fn random_simd_programs_match_qemu() {
    run("random_simd_programs_match_qemu", true, env_u64("VETRO_DIFF_CASES", 250));
}

/// Programmi brevi di sola virgola mobile su casi speciali.
#[test]
fn random_fp_focused_match_qemu() {
    run_with("random_fp_focused_match_qemu", "fp-", env_u64("VETRO_DIFF_FP_CASES", 600), generate_fp_focused);
}

fn run(name: &str, simd: bool, cases: u64) {
    let prefix = if simd { "simd-" } else { "" };
    run_with(name, prefix, cases, move |seed| generate_with(seed, BODY_LEN, simd));
}

fn run_with(name: &str, prefix: &str, cases: u64, make: impl Fn(u64) -> Case + Sync) {
    let Some(q) = qemu::locate_or_skip(name) else { return };
    let first = env_u64("VETRO_DIFF_SEED", 0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);

    let next = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());
    let (sigill, dumps) = (AtomicUsize::new(0), AtomicUsize::new(0));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed) as u64;
                    if i >= cases {
                        break;
                    }
                    let seed = first + i;
                    let case = make(seed);
                    let image = case.program.build();
                    let ours = run_vetro(&image);
                    let theirs = run_qemu(&q, &format!("random-{prefix}{seed}"), &image);
                    let diff = compare(&ours, &theirs);
                    match &theirs {
                        vetro_diff::harness::Run::Signal(4) => sigill.fetch_add(1, Ordering::Relaxed),
                        vetro_diff::harness::Run::Dump(_) => dumps.fetch_add(1, Ordering::Relaxed),
                        _ => 0,
                    };
                    if !diff.is_empty() {
                        failures.lock().unwrap().push((seed, diff, describe(&case.program)));
                    }
                }
            });
        }
    });

    let mut failures = failures.into_inner().unwrap();
    failures.sort_by_key(|f| f.0);
    eprintln!(
        "{name}: {cases} programmi: {} con dump, {} con SIGILL atteso in coda, {} differenze",
        dumps.into_inner(),
        sigill.into_inner(),
        failures.len()
    );
    if let Some((_, _, body)) = failures.first() {
        let seeds: Vec<u64> = failures.iter().map(|f| f.0).take(20).collect();
        let mut report = String::new();
        for (seed, diff, _) in failures.iter().take(5) {
            report += &format!("seme {seed}:\n{diff}\n");
        }
        panic!(
            "{} programmi diversi da QEMU (semi {seeds:?}...)\n\n{report}corpo del primo:\n{body}",
            failures.len()
        );
    }
}
