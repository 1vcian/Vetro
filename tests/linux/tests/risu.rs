//! RISU (ADR 0006): `risu` runs on Vetro as the apprentice and compares, after
//! each instruction, registers and memory with the trace recorded by
//! `qemu-aarch64 -cpu cortex-a53` as the master. Images and traces come from
//! `tools/risu/build.sh` (in `target/risu`).

use std::path::Path;
use vetro_cli::linux::{Config, Exit};

#[test]
fn risu_traces_match_qemu() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/risu");
    let risu = dir.join("risu");
    if !risu.is_file() {
        if std::env::var("VETRO_REQUIRE_GUEST_BINS").is_ok_and(|v| v == "1") {
            panic!("target/risu missing: run tools/risu/build.sh");
        }
        eprintln!("SKIP risu_traces_match_qemu: esegui tools/risu/build.sh");
        return;
    }
    let image = std::fs::read(&risu).unwrap();
    let mut images: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "bin") && p.with_extension("trace").is_file())
        .collect();
    images.sort();
    assert!(!images.is_empty(), "no RISU image in {}", dir.display());
    let mut failures = Vec::new();
    for bin in &images {
        let trace = bin.with_extension("trace");
        let args = ["risu", "-t", trace.to_str().unwrap(), bin.to_str().unwrap()];
        let cfg = Config { cwd: dir.to_string_lossy().into_owned(), ..Config::default() }.jit_from_env();
        let out = vetro_cli::run_elf(&image, &args, &[], risu.to_str().unwrap(), cfg).unwrap();
        let name = bin.file_stem().unwrap().to_string_lossy().into_owned();
        if out.exit != Exit::Code(0) {
            let report = String::from_utf8_lossy(&out.stderr);
            let mismatch: String = report
                .lines()
                .filter(|l| l.contains(" vs ") || l.contains("ismatch"))
                .take(8)
                .collect::<Vec<_>>()
                .join("\n");
            failures.push(format!("{name}: {:?}\n{mismatch}", out.exit));
        } else {
            eprintln!("RISU {name}: traccia identica");
        }
    }
    assert!(failures.is_empty(), "RISU diverges from QEMU:\n{}", failures.join("\n\n"));
}
