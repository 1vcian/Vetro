//! Syscall Linux arm64 (tabella asm-generic): nomi e formato degli argomenti.

/// Tipo di un argomento, per la stampa.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arg {
    /// Intero con segno (fd, flag piccoli, lunghezze).
    Int,
    /// Valore esadecimale (flag, maschere).
    Hex,
    /// Puntatore.
    Ptr,
    /// Stringa C terminata da zero nella memoria del guest.
    Str,
    /// Descrittore di directory per le *at (AT_FDCWD = -100).
    DirFd,
}

use Arg::*;

/// (numero, nome, argomenti)
pub const TABLE: &[(u64, &str, &[Arg])] = &[
    (17, "getcwd", &[Ptr, Int]),
    (23, "dup", &[Int]),
    (24, "dup3", &[Int, Int, Hex]),
    (25, "fcntl", &[Int, Int, Hex]),
    (29, "ioctl", &[Int, Hex, Ptr]),
    (34, "mkdirat", &[DirFd, Str, Hex]),
    (35, "unlinkat", &[DirFd, Str, Hex]),
    (36, "symlinkat", &[Str, DirFd, Str]),
    (37, "linkat", &[DirFd, Str, DirFd, Str, Hex]),
    (38, "renameat", &[DirFd, Str, DirFd, Str]),
    (43, "statfs", &[Str, Ptr]),
    (44, "fstatfs", &[Int, Ptr]),
    (46, "ftruncate", &[Int, Int]),
    (48, "faccessat", &[DirFd, Str, Hex]),
    (49, "chdir", &[Str]),
    (50, "fchdir", &[Int]),
    (52, "fchmod", &[Int, Hex]),
    (53, "fchmodat", &[DirFd, Str, Hex]),
    (54, "fchownat", &[DirFd, Str, Int, Int, Hex]),
    (55, "fchown", &[Int, Int, Int]),
    (56, "openat", &[DirFd, Str, Hex, Hex]),
    (57, "close", &[Int]),
    (59, "pipe2", &[Ptr, Hex]),
    (61, "getdents64", &[Int, Ptr, Int]),
    (62, "lseek", &[Int, Int, Int]),
    (63, "read", &[Int, Ptr, Int]),
    (64, "write", &[Int, Ptr, Int]),
    (65, "readv", &[Int, Ptr, Int]),
    (66, "writev", &[Int, Ptr, Int]),
    (67, "pread64", &[Int, Ptr, Int, Int]),
    (68, "pwrite64", &[Int, Ptr, Int, Int]),
    (71, "sendfile", &[Int, Int, Ptr, Int]),
    (72, "pselect6", &[Int, Ptr, Ptr, Ptr, Ptr, Ptr]),
    (73, "ppoll", &[Ptr, Int, Ptr, Ptr, Int]),
    (78, "readlinkat", &[DirFd, Str, Ptr, Int]),
    (79, "newfstatat", &[DirFd, Str, Ptr, Hex]),
    (80, "fstat", &[Int, Ptr]),
    (82, "fsync", &[Int]),
    (88, "utimensat", &[DirFd, Str, Ptr, Hex]),
    (90, "capget", &[Ptr, Ptr]),
    (91, "capset", &[Ptr, Ptr]),
    (92, "personality", &[Hex]),
    (93, "exit", &[Int]),
    (94, "exit_group", &[Int]),
    (96, "set_tid_address", &[Ptr]),
    (98, "futex", &[Ptr, Int, Hex, Ptr, Ptr, Hex]),
    (99, "set_robust_list", &[Ptr, Int]),
    (101, "nanosleep", &[Ptr, Ptr]),
    (102, "getitimer", &[Int, Ptr]),
    (103, "setitimer", &[Int, Ptr, Ptr]),
    (113, "clock_gettime", &[Int, Ptr]),
    (114, "clock_getres", &[Int, Ptr]),
    (115, "clock_nanosleep", &[Int, Hex, Ptr, Ptr]),
    (124, "sched_yield", &[]),
    (129, "kill", &[Int, Int]),
    (130, "tkill", &[Int, Int]),
    (131, "tgkill", &[Int, Int, Int]),
    (132, "sigaltstack", &[Ptr, Ptr]),
    (133, "rt_sigsuspend", &[Ptr, Int]),
    (134, "rt_sigaction", &[Int, Ptr, Ptr, Int]),
    (135, "rt_sigprocmask", &[Int, Ptr, Ptr, Int]),
    (136, "rt_sigpending", &[Ptr, Int]),
    (137, "rt_sigtimedwait", &[Ptr, Ptr, Ptr, Int]),
    (139, "rt_sigreturn", &[]),
    (153, "times", &[Ptr]),
    (154, "setpgid", &[Int, Int]),
    (155, "getpgid", &[Int]),
    (156, "getsid", &[Int]),
    (157, "setsid", &[]),
    (160, "uname", &[Ptr]),
    (163, "getrlimit", &[Int, Ptr]),
    (164, "setrlimit", &[Int, Ptr]),
    (165, "getrusage", &[Int, Ptr]),
    (166, "umask", &[Hex]),
    (167, "prctl", &[Int, Hex, Hex, Hex, Hex]),
    (169, "gettimeofday", &[Ptr, Ptr]),
    (172, "getpid", &[]),
    (173, "getppid", &[]),
    (174, "getuid", &[]),
    (175, "geteuid", &[]),
    (176, "getgid", &[]),
    (177, "getegid", &[]),
    (178, "gettid", &[]),
    (179, "sysinfo", &[Ptr]),
    (214, "brk", &[Ptr]),
    (215, "munmap", &[Ptr, Int]),
    (216, "mremap", &[Ptr, Int, Int, Hex, Ptr]),
    (220, "clone", &[Hex, Ptr, Ptr, Ptr, Ptr]),
    (221, "execve", &[Str, Ptr, Ptr]),
    (222, "mmap", &[Ptr, Int, Hex, Hex, Int, Int]),
    (226, "mprotect", &[Ptr, Int, Hex]),
    (227, "msync", &[Ptr, Int, Hex]),
    (233, "madvise", &[Ptr, Int, Int]),
    (260, "wait4", &[Int, Ptr, Hex, Ptr]),
    (261, "prlimit64", &[Int, Int, Ptr, Ptr]),
    (278, "getrandom", &[Ptr, Int, Hex]),
    (291, "statx", &[DirFd, Str, Hex, Hex, Ptr]),
    (435, "clone3", &[Ptr, Int]),
    (439, "faccessat2", &[DirFd, Str, Hex, Hex]),
    (449, "futex_waitv", &[Ptr, Int, Hex, Ptr, Int]),
];

pub fn lookup(nr: u64) -> Option<(&'static str, &'static [Arg])> {
    TABLE.iter().find(|(n, _, _)| *n == nr).map(|(_, name, args)| (*name, *args))
}

pub fn name(nr: u64) -> &'static str {
    lookup(nr).map_or("?", |(n, _)| n)
}

/// Nome simbolico di un errno Linux (valori asm-generic).
pub fn errno_name(e: i64) -> &'static str {
    match e {
        1 => "EPERM",
        2 => "ENOENT",
        3 => "ESRCH",
        4 => "EINTR",
        5 => "EIO",
        7 => "E2BIG",
        8 => "ENOEXEC",
        9 => "EBADF",
        10 => "ECHILD",
        11 => "EAGAIN",
        12 => "ENOMEM",
        13 => "EACCES",
        14 => "EFAULT",
        17 => "EEXIST",
        20 => "ENOTDIR",
        21 => "EISDIR",
        22 => "EINVAL",
        24 => "EMFILE",
        25 => "ENOTTY",
        28 => "ENOSPC",
        29 => "ESPIPE",
        32 => "EPIPE",
        34 => "ERANGE",
        36 => "ENAMETOOLONG",
        38 => "ENOSYS",
        39 => "ENOTEMPTY",
        40 => "ELOOP",
        110 => "ETIMEDOUT",
        _ => "E?",
    }
}

/// Riga di trace nel formato di strace: `openat(AT_FDCWD, "/etc", 0x0) = 3`.
/// `read_str` legge una stringa dalla memoria del guest.
pub fn format(nr: u64, args: &[u64; 6], ret: i64, mut read_str: impl FnMut(u64) -> Option<String>) -> String {
    let (name, kinds) = lookup(nr).unwrap_or(("syscall", &[Hex, Hex, Hex, Hex, Hex, Hex]));
    let mut s = String::new();
    if lookup(nr).is_none() {
        s += &format!("syscall_{nr}(");
    } else {
        s += name;
        s.push('(');
    }
    for (i, k) in kinds.iter().enumerate() {
        if i > 0 {
            s += ", ";
        }
        let a = args[i];
        s += &match k {
            Int => format!("{}", a as i64),
            Hex => format!("{a:#x}"),
            Ptr => {
                if a == 0 {
                    "NULL".into()
                } else {
                    format!("{a:#x}")
                }
            }
            Str => match read_str(a) {
                Some(t) => format!("{t:?}"),
                None => format!("{a:#x}"),
            },
            DirFd => {
                if a as i64 as i32 == -100 {
                    "AT_FDCWD".into()
                } else {
                    format!("{}", a as i64 as i32)
                }
            }
        };
    }
    s.push(')');
    if (-4095..0).contains(&ret) {
        s += &format!(" = -1 {} ({})", errno_name(-ret), -ret);
    } else if matches!(nr, 214 | 222 | 216) {
        s += &format!(" = {ret:#x}");
    } else {
        s += &format!(" = {ret}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_strace() {
        let line = format(56, &[(-100i64) as u64, 0x1000, 0, 0, 0, 0], -2, |_| Some("/nope".into()));
        assert_eq!(line, "openat(AT_FDCWD, \"/nope\", 0x0, 0x0) = -1 ENOENT (2)");
        assert_eq!(format(64, &[1, 0x42, 6, 0, 0, 0], 6, |_| None), "write(1, 0x42, 6) = 6");
        assert_eq!(name(94), "exit_group");
    }
}
