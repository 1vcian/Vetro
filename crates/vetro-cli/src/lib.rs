//! Native execution of static arm64 Linux programs in user mode.
//!
//! [`linux`] emulates the kernel: processes, threads, files, memory, signals and
//! virtual time. It is the CPU's test bench before booting the real
//! kernel (M3) and the engine of the differential tests against QEMU.

/// The kernel loader lives in `vetro-machine` (the browser needs it too).
pub use vetro_machine::boot;
pub mod analysis;
pub mod disk;
pub mod elf;
pub mod files;
pub mod hostfwd;
pub mod linux;
pub mod netcap;

use linux::{Config, Exit, Kernel};

/// Result of a complete run.
pub struct Outcome {
    pub exit: Exit,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Instructions executed (all processes).
    pub steps: u64,
    /// JIT counters, if enabled.
    pub jit: Option<vetro_jit::JitStats>,
}

/// Runs an ELF with the given arguments and environment.
/// Raises the host's soft descriptor limit to the maximum allowed: every
/// file opened by the guest is a host descriptor, and the guest must reach
/// its own RLIMIT_NOFILE (EMFILE) before the host runs out of its own.
/// Returns the previous soft limit.
pub fn raise_fd_limit() -> u64 {
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    let before;
    // SAFETY: `r` is a valid struct rlimit for get/setrlimit.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) != 0 {
            return 1024;
        }
        before = r.rlim_cur;
        if r.rlim_cur < r.rlim_max {
            // macOS rejects values above OPEN_MAX even with an unlimited hard limit.
            r.rlim_cur = r.rlim_max.min(1 << 20);
            if libc::setrlimit(libc::RLIMIT_NOFILE, &r) != 0 {
                r.rlim_cur = 10240;
                libc::setrlimit(libc::RLIMIT_NOFILE, &r);
            }
        }
    }
    before
}

pub fn run_elf(
    image: &[u8],
    argv: &[&str],
    envp: &[&str],
    exe: &str,
    cfg: Config,
) -> Result<Outcome, String> {
    let mut k = Kernel::new(cfg);
    let argv: Vec<Vec<u8>> = argv.iter().map(|s| s.as_bytes().to_vec()).collect();
    let envp: Vec<Vec<u8>> = envp.iter().map(|s| s.as_bytes().to_vec()).collect();
    k.spawn(image, &argv, &envp, exe)?;
    let exit = k.run();
    if let Some(p) = k.jit_profile() {
        eprintln!("vetro: {}vetro: calls to env.simd: {}", p.report(40), vetro_jit::helper::calls());
        if let Some(r) = vetro_jit::helper::profile_report(20) {
            eprint!("vetro: env.simd, {r}");
        }
    }
    Ok(Outcome { exit, stdout: k.stdout(), stderr: k.stderr(), steps: k.steps(), jit: k.jit_stats() })
}
