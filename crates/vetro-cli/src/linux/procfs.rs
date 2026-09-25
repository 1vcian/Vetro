//! /proc virtuale. Il /proc dell'host descriverebbe l'emulatore (o la
//! macchina che lo ospita), non il guest: qui i contenuti sono generati e
//! deterministici, come si addice a una Cortex-A53 con 4 GiB e un core.

use super::abi::*;
use super::{Kernel, State};

/// Memoria totale dichiarata al guest, in kB.
const MEM_KB: u64 = 4 * 1024 * 1024;

impl Kernel {
    /// Contenuto del file `path` sotto /proc, se è uno di quelli emulati.
    /// `None` = non è un percorso di /proc; `Some(Err)` = /proc ma assente.
    pub(super) fn proc_content(&self, t: usize, path: &str) -> Option<Result<Vec<u8>, i64>> {
        let rest = path.strip_prefix("/proc/").or_else(|| (path == "/proc").then_some(""))?;
        let text = |s: String| Some(Ok(s.into_bytes()));
        let up_s = self.now() / 1_000_000_000;
        let up_cs = (self.now() / 10_000_000) % 100;
        match rest {
            "meminfo" => {
                let free = MEM_KB * 3 / 4;
                return text(format!(
                    "MemTotal:       {MEM_KB:>8} kB\nMemFree:        {free:>8} kB\nMemAvailable:   {:>8} kB\n\
                     Buffers:               0 kB\nCached:           262144 kB\nSwapCached:            0 kB\n\
                     Active:           131072 kB\nInactive:         131072 kB\nSwapTotal:             0 kB\n\
                     SwapFree:              0 kB\nDirty:                 0 kB\nWriteback:             0 kB\n\
                     AnonPages:        131072 kB\nMapped:            65536 kB\nShmem:                 0 kB\n\
                     Slab:              32768 kB\nCommitLimit:    {:>8} kB\nCommitted_AS:     262144 kB\n\
                     HugePages_Total:       0\nHugePages_Free:        0\nHugepagesize:       2048 kB\n",
                    MEM_KB * 7 / 8,
                    MEM_KB / 2
                ));
            }
            "cpuinfo" => {
                return text(
                    "processor\t: 0\nBogoMIPS\t: 125.00\nFeatures\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 cpuid\n\
                     CPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x0\nCPU part\t: 0xd03\n\
                     CPU revision\t: 4\n\n"
                        .into(),
                );
            }
            "uptime" => return text(format!("{up_s}.{up_cs:02} {up_s}.{up_cs:02}\n")),
            "loadavg" => {
                let running = self.tasks.iter().filter(|x| x.state == State::Runnable).count();
                return text(format!(
                    "0.00 0.00 0.00 {running}/{} {}\n",
                    self.tasks.len(),
                    self.tasks.len() + 100
                ));
            }
            "stat" => {
                return text(format!(
                    "cpu  0 0 0 {up_s} 0 0 0 0 0 0\ncpu0 0 0 0 {up_s} 0 0 0 0 0 0\nbtime {}\nprocesses {}\n",
                    self.realtime() / 1_000_000_000 - up_s,
                    self.tasks.len()
                ));
            }
            "filesystems" => {
                return text("nodev\tsysfs\nnodev\tproc\nnodev\ttmpfs\nnodev\tdevpts\n\text4\n".into());
            }
            "mounts" | "self/mounts" => {
                return text("/dev/root / ext4 rw,relatime 0 0\nproc /proc proc rw,relatime 0 0\n".into());
            }
            "sys/kernel/tainted" => return text("0\n".into()),
            "sys/kernel/pid_max" => return text("4194304\n".into()),
            "sys/kernel/threads-max" => return text("31000\n".into()),
            "sys/kernel/osrelease" => return text("6.6.0-vetro\n".into()),
            "sys/kernel/ostype" => return text("Linux\n".into()),
            "sys/kernel/hostname" => return text("vetro\n".into()),
            "sys/kernel/random/boot_id" => return text("00000000-0000-4000-8000-000000000000\n".into()),
            "sys/vm/overcommit_memory" => return text("0\n".into()),
            "sys/vm/max_map_count" => return text("65530\n".into()),
            "sys/fs/pipe-max-size" => return text("1048576\n".into()),
            _ => {}
        }
        let (who, file) = rest.split_once('/').unwrap_or((rest, ""));
        let task = &self.tasks[t];
        let pid = match who {
            "self" => task.tgid,
            "thread-self" => task.tid,
            p => match p.parse::<i32>() {
                Ok(p) => p,
                Err(_) => return Some(Err(ENOENT)),
            },
        };
        let Some(i) = self.tasks.iter().position(|x| x.tid == pid && x.state != State::Dead) else {
            return Some(Err(ENOENT));
        };
        let task = &self.tasks[i];
        let state = match task.state {
            State::Runnable => "R (running)",
            State::Blocked(_) => "S (sleeping)",
            _ => "Z (zombie)",
        };
        let st_char = &state[..1];
        match file {
            "comm" => text(format!("{}\n", task.comm)),
            "cmdline" => Some(Ok(format!("{}\0", task.exe).into_bytes())),
            "stat" => {
                // 52 campi come in proc(5); tempi e memoria a zero.
                let threads =
                    self.tasks.iter().filter(|x| x.tgid == task.tgid && x.state != State::Dead).count();
                text(format!(
                    "{} ({}) {st_char} {} {} {} 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 {threads} 0 0 0 0 \
                     18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
                    task.tid, task.comm, task.ppid, task.pgid, task.pgid
                ))
            }
            "status" => {
                // SAFETY: getuid/getgid non hanno precondizioni.
                let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
                let threads =
                    self.tasks.iter().filter(|x| x.tgid == task.tgid && x.state != State::Dead).count();
                text(format!(
                    "Name:\t{}\nUmask:\t{:04o}\nState:\t{state}\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nTracerPid:\t0\n\
                     Uid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t{gid}\t{gid}\t{gid}\t{gid}\nFDSize:\t64\nVmSize:\t    8192 kB\nVmRSS:\t    4096 kB\n\
                     Threads:\t{threads}\nSigPnd:\t{:016x}\nSigBlk:\t{:016x}\nCapEff:\t000001ffffffffff\n\
                     Cpus_allowed:\t1\nCpus_allowed_list:\t0\n",
                    task.comm, task.umask, task.tgid, task.tid, task.ppid, task.sig.pending, task.sig.mask
                ))
            }
            "maps" => {
                let mut s = String::new();
                for (a, b, p) in task.mm.borrow().mem.ranges() {
                    let perms = format!(
                        "{}{}{}p",
                        if p.read { 'r' } else { '-' },
                        if p.write { 'w' } else { '-' },
                        if p.exec { 'x' } else { '-' }
                    );
                    s += &format!("{a:08x}-{b:08x} {perms} 00000000 00:00 0\n");
                }
                text(s)
            }
            "oom_score_adj" => text(format!("{}\n", task.oom_score_adj)),
            _ => Some(Err(ENOENT)),
        }
    }
}

impl Kernel {
    /// Scrittura in /proc/<pid>/oom_score_adj: abbassarlo richiede privilegi.
    pub(super) fn write_oom_score_adj(&mut self, t: usize, path: &str, data: &[u8]) -> Result<usize, i64> {
        let v: i32 = std::str::from_utf8(data).ok().and_then(|s| s.trim().parse().ok()).ok_or(EINVAL)?;
        if !(-1000..=1000).contains(&v) {
            return Err(EINVAL);
        }
        let who = path.trim_start_matches("/proc/").split('/').next().unwrap_or("self");
        let pid = if who == "self" { self.tasks[t].tgid } else { who.parse().map_err(|_| ENOENT)? };
        // SAFETY: geteuid non ha precondizioni.
        let root = unsafe { libc::geteuid() } == 0;
        let cur = self.tasks.iter().find(|x| x.tgid == pid).ok_or(ENOENT)?.oom_score_adj;
        if v < cur && !root {
            return Err(EACCES);
        }
        for task in self.tasks.iter_mut().filter(|x| x.tgid == pid) {
            task.oom_score_adj = v;
        }
        Ok(data.len())
    }
}
