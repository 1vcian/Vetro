//! Tabella delle syscall.

use std::cell::RefCell;
use std::rc::Rc;

use super::abi::*;
use super::fs::{self, Io, Kind, OpenFile};
use super::signal::{UNBLOCKABLE, bit};
use super::{Exit, Kernel, State, Wait, sig};
use vetro_cpu::UserMemory;

/// Esito di una syscall.
pub enum Sys {
    Ret(i64),
    /// Il task si blocca e l'SVC verrà rieseguita.
    Block(Wait),
    /// I registri sono già a posto (execve, rt_sigreturn, exit, vfork).
    NoReturn,
    /// Syscall che non conosciamo ancora.
    Unsupported,
}

type R = Result<Sys, i64>;

fn ret(v: i64) -> R {
    Ok(Sys::Ret(v))
}

/// Vero se open() con O_CREAT trova già una FIFO (senza O_EXCL).
fn m_is_fifo_creat(path: &str, flags: u64) -> bool {
    use std::os::unix::fs::FileTypeExt;
    flags & O_CREAT != 0
        && flags & O_EXCL == 0
        && std::fs::metadata(path).is_ok_and(|m| m.file_type().is_fifo())
}

/// Syscall che i programmi sondano e per cui ENOSYS è una risposta legittima.
const ENOSYS_OK: &[u64] = &[
    283, // membarrier
    293, // rseq
    435, // clone3 (glibc ripiega su clone)
];

impl Kernel {
    pub(super) fn syscall(&mut self, t: usize) -> Option<Exit> {
        let nr = self.tasks[t].cpu.x[8];
        let a: [u64; 6] = self.tasks[t].cpu.x[..6].try_into().unwrap();
        let pc = self.tasks[t].cpu.pc - 4;
        // Coerenza tra descrittori e MAP_SHARED dello stesso file.
        let keys = self.shared_keys_of(t, nr, &a);
        for &k in &keys {
            self.flush_shared_one(k);
        }
        let res = if self.uses_opath_fd(t, nr, &a) { Err(EBADF) } else { self.dispatch(t, nr, a) };
        if matches!(nr, 46 | 64 | 66 | 68 | 70 | 71 | 287) {
            for &k in &keys {
                self.reload_shared_one(k);
            }
        }
        let v = match res {
            Ok(Sys::Ret(v)) => v,
            Err(e) => -e,
            Ok(Sys::Block(w)) => {
                self.tasks[t].cpu.pc = pc;
                self.tasks[t].state = State::Blocked(w);
                return None;
            }
            Ok(Sys::NoReturn) => {
                if self.cfg.strace {
                    self.trace(t, nr, &a, 0);
                }
                return None;
            }
            Ok(Sys::Unsupported) => {
                if ENOSYS_OK.contains(&nr) {
                    -ENOSYS
                } else {
                    return Some(Exit::UnsupportedSyscall { nr, pc });
                }
            }
        };
        if self.tasks[t].state != State::Dead {
            self.tasks[t].cpu.x[0] = v as u64;
            self.tasks[t].deadline = None;
        }
        if self.cfg.strace {
            self.trace(t, nr, &a, v);
        }
        None
    }

    /// Chiavi (dispositivo, inode) dei file con una MAP_SHARED su cui la
    /// syscall fa I/O da descrittore.
    fn shared_keys_of(&self, t: usize, nr: u64, a: &[u64; 6]) -> Vec<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        if self.shared_files.is_empty() {
            return Vec::new();
        }
        let fds: &[u64] = match nr {
            46 | 63..=70 | 82 | 83 | 286 | 287 => &a[..1],
            71 => &a[..2],
            _ => return Vec::new(),
        };
        let files = self.tasks[t].files.borrow();
        fds.iter()
            .filter_map(|&fd| {
                let f = files.get(fd as i64).ok()?;
                let f = f.borrow();
                let Kind::Host { file, .. } = &f.kind else { return None };
                let m = file.metadata().ok()?;
                let key = (m.dev(), m.ino());
                self.shared_files.contains_key(&key).then_some(key)
            })
            .collect()
    }

    /// Vero se la syscall fa I/O su un descrittore O_PATH: Linux la rifiuta
    /// con EBADF (fdget invece di fdget_raw).
    fn uses_opath_fd(&self, t: usize, nr: u64, a: &[u64; 6]) -> bool {
        let fds: &[u64] = match nr {
            // fsetxattr, fgetxattr, flistxattr, fremovexattr, ioctl, ftruncate,
            // fchmod, fchown, getdents64, lseek, read..pwritev, fsync, fdatasync,
            // preadv2, pwritev2
            7 | 10 | 13 | 16 | 29 | 46 | 52 | 55 | 61 | 62 | 63..=70 | 82 | 83 | 286 | 287 => &a[..1],
            71 => &a[..2], // sendfile
            222 if a[3] & super::mm::MAP_ANONYMOUS == 0 => &a[4..5],
            _ => return false,
        };
        let files = self.tasks[t].files.borrow();
        fds.iter().any(|&fd| files.get(fd as i64).is_ok_and(|f| matches!(f.borrow().kind, Kind::Path { .. })))
    }

    fn trace(&self, t: usize, nr: u64, a: &[u64; 6], v: i64) {
        let mm = self.tasks[t].mm.clone();
        let line = vetro_analysis::syscall::format(nr, a, v, |p| {
            read_cstr(&mut mm.borrow_mut().mem, p).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
        });
        eprintln!("[{}] {line}", self.tasks[t].tid);
    }

    fn mem(&self, t: usize) -> Rc<RefCell<super::mm::Mm>> {
        self.tasks[t].mm.clone()
    }

    /// Percorso assoluto del guest per una *at: `path` rispetto a `dirfd`. I
    /// link magici `/proc/<pid>/fd/N` portano al file aperto.
    fn at_path(&self, t: usize, dirfd: u64, path: &[u8]) -> Result<String, i64> {
        let p = self.at_path_raw(t, dirfd, path)?;
        Ok(self.proc_fd_target(t, &p).unwrap_or(p))
    }

    /// Il file dietro `/proc/{self,thread-self,<pid>}/fd/N`, se è un file o
    /// una directory dell'host.
    fn proc_fd_target(&self, t: usize, path: &str) -> Option<String> {
        let rest = path.strip_prefix("/proc/")?;
        let (who, rest) = rest.split_once('/')?;
        let fd: i64 = rest.strip_prefix("fd/")?.parse().ok()?;
        let task = match who {
            "self" | "thread-self" => t,
            pid => self.find(pid.parse().ok()?)?,
        };
        let f = self.tasks[task].files.borrow().get(fd).ok()?;
        let f = f.borrow();
        matches!(f.kind, Kind::Host { .. } | Kind::Dir { .. } | Kind::Path { .. })
            .then(|| f.guest_path.clone())
    }

    fn at_path_raw(&self, t: usize, dirfd: u64, path: &[u8]) -> Result<String, i64> {
        let dirfd = dirfd as i64 as i32;
        if path.first() == Some(&b'/') {
            // Come `qemu -L`: un percorso assoluto si cerca prima nel sysroot.
            if let Some(root) = &self.cfg.sysroot {
                let inside = fs::join(root, &path[1..]);
                if std::fs::symlink_metadata(&inside).is_ok() {
                    return Ok(inside);
                }
            }
            return Ok(fs::join("/", path));
        }
        if dirfd == AT_FDCWD {
            return Ok(fs::join(&self.tasks[t].cwd.borrow(), path));
        }
        let f = self.tasks[t].files.borrow().get(dirfd as i64)?;
        let f = f.borrow();
        if !matches!(f.kind, Kind::Dir { .. } | Kind::Path { dir: true, .. }) {
            return Err(ENOTDIR);
        }
        Ok(fs::join(&f.guest_path, path))
    }

    /// Percorso senza seguire l'ultimo componente (unlink, rename, mkdir,
    /// lstat...): il secondo valore dice se è un link magico
    /// `/proc/<pid>/fd/N`, che Linux tratta come un link di procfs.
    fn path_arg_nofollow(&self, t: usize, dirfd: u64, p: u64) -> Result<(String, bool), i64> {
        let raw = read_cstr(&mut self.mem(t).borrow_mut().mem, p)?;
        if raw.is_empty() {
            return Err(ENOENT);
        }
        let path = self.at_path_raw(t, dirfd, &raw)?;
        let link = self.proc_fd_target(t, &path).is_some();
        Ok((path, link))
    }

    fn path_arg(&self, t: usize, dirfd: u64, p: u64) -> Result<String, i64> {
        let raw = read_cstr(&mut self.mem(t).borrow_mut().mem, p)?;
        if raw.is_empty() {
            return Err(ENOENT);
        }
        self.at_path(t, dirfd, &raw)
    }

    fn dispatch(&mut self, t: usize, nr: u64, a: [u64; 6]) -> R {
        match nr {
            // --- file ---
            56 => self.sys_openat(t, a[0], a[1], a[2], a[3] as u32),
            57 => {
                let f = self.tasks[t].files.borrow().get(a[0] as i64)?;
                self.release_locks_on_close(t, &f);
                ret(self.tasks[t].files.borrow_mut().close(a[0] as i64)?)
            }
            63 => self.sys_read(t, a[0] as i64, a[1], a[2] as usize),
            64 => self.sys_write(t, a[0] as i64, a[1], a[2] as usize),
            65 => self.sys_readv(t, a[0] as i64, a[1], a[2] as usize),
            66 => self.sys_writev(t, a[0] as i64, a[1], a[2] as usize),
            67 | 68 => self.sys_pio(t, nr == 68, a[0] as i64, a[1], a[2] as usize, a[3] as i64),
            69 | 70 | 286 | 287 => self.sys_piov(t, nr, a),
            62 => {
                ret(self.tasks[t].files.borrow().get(a[0] as i64)?.borrow_mut().lseek(a[1] as i64, a[2])?)
            }
            61 => {
                let f = self.tasks[t].files.borrow().get(a[0] as i64)?;
                let data = f.borrow_mut().getdents(a[2] as usize)?;
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[1], &data)?;
                ret(data.len() as i64)
            }
            79 => self.sys_fstatat(t, a[0], a[1], a[2], a[3]),
            80 => {
                let st = self.tasks[t].files.borrow().get(a[0] as i64)?.borrow().stat()?;
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[1], &st.to_bytes())?;
                ret(0)
            }
            291 => self.sys_statx(t, a[0], a[1], a[2], a[4]),
            78 => self.sys_readlinkat(t, a[0], a[1], a[2], a[3] as usize),
            48 | 439 => {
                let p = self.path_arg(t, a[0], a[1])?;
                // /proc è sempre quello virtuale: quello dell'host descrive l'emulatore.
                if let Some(r) = self.proc_content(t, &p) {
                    r?;
                    return ret(0);
                }
                std::fs::metadata(&p).map_err(|e| host_errno(&e))?;
                ret(0)
            }
            17 => {
                let cwd = self.tasks[t].cwd.borrow().clone();
                let mut b = cwd.into_bytes();
                b.push(0);
                if b.len() > a[1] as usize {
                    return Err(ERANGE);
                }
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[0], &b)?;
                ret(b.len() as i64)
            }
            49 => {
                let p = self.path_arg(t, AT_FDCWD as u64, a[0])?;
                let m = std::fs::metadata(&p).map_err(|e| host_errno(&e))?;
                if !m.is_dir() {
                    return Err(ENOTDIR);
                }
                // Come il kernel, che tiene la dentry: la cwd è il percorso
                // canonico, senza link simbolici.
                let p = std::fs::canonicalize(&p).map(|c| c.to_string_lossy().into_owned()).unwrap_or(p);
                *self.tasks[t].cwd.borrow_mut() = p;
                ret(0)
            }
            50 => {
                let f = self.tasks[t].files.borrow().get(a[0] as i64)?;
                let f = f.borrow();
                if !matches!(f.kind, Kind::Dir { .. } | Kind::Path { dir: true, .. }) {
                    return Err(ENOTDIR);
                }
                *self.tasks[t].cwd.borrow_mut() = f.guest_path.clone();
                ret(0)
            }
            34 => {
                let (p, link) = self.path_arg_nofollow(t, a[0], a[1])?;
                if link {
                    return Err(EEXIST);
                }
                use std::os::unix::fs::DirBuilderExt;
                let want = a[2] as u32 & !self.tasks[t].umask & 0o7777;
                std::fs::DirBuilder::new().mode(want).create(&p).map_err(|e| host_errno(&e))?;
                fs::fix_mode(std::path::Path::new(&p), want);
                ret(0)
            }
            35 => {
                let (p, link) = self.path_arg_nofollow(t, a[0], a[1])?;
                // Le voci di /proc/<pid>/fd non si tolgono.
                if link {
                    return Err(EPERM);
                }
                if a[2] & AT_REMOVEDIR != 0 {
                    std::fs::remove_dir(&p).map_err(|e| host_errno(&e))?;
                } else {
                    let m = std::fs::symlink_metadata(&p).map_err(|e| host_errno(&e))?;
                    if m.is_dir() {
                        return Err(EISDIR);
                    }
                    std::fs::remove_file(&p).map_err(|e| host_errno(&e))?;
                }
                ret(0)
            }
            36 => {
                let target = read_cstr(&mut self.mem(t).borrow_mut().mem, a[0])?;
                let (link, magic) = self.path_arg_nofollow(t, a[1], a[2])?;
                if magic {
                    return Err(EEXIST);
                }
                std::os::unix::fs::symlink(String::from_utf8_lossy(&target).as_ref(), &link)
                    .map_err(|e| host_errno(&e))?;
                ret(0)
            }
            37 => {
                // linkat: con AT_SYMLINK_FOLLOW (0x400) si segue il link, anche
                // quelli di /proc/<pid>/fd (file O_TMPFILE).
                if a[4] & !(0x400 | 0x1000) != 0 {
                    return Err(EINVAL);
                }
                let old = if a[4] & 0x400 != 0 {
                    self.path_arg(t, a[0], a[1])?
                } else {
                    self.path_arg_nofollow(t, a[0], a[1])?.0
                };
                let (new, magic) = self.path_arg_nofollow(t, a[2], a[3])?;
                if magic {
                    return Err(EEXIST);
                }
                let c = |p: &str| std::ffi::CString::new(p).map_err(|_| EINVAL);
                let (co, cn) = (c(&old)?, c(&new)?);
                let follow = if a[4] & 0x400 != 0 { libc::AT_SYMLINK_FOLLOW } else { 0 };
                // SAFETY: percorsi C validi.
                let r =
                    unsafe { libc::linkat(libc::AT_FDCWD, co.as_ptr(), libc::AT_FDCWD, cn.as_ptr(), follow) };
                if r < 0 {
                    return Err(host_errno(&std::io::Error::last_os_error()));
                }
                ret(0)
            }
            38 | 276 => {
                let (old, l1) = self.path_arg_nofollow(t, a[0], a[1])?;
                let (new, l2) = self.path_arg_nofollow(t, a[2], a[3])?;
                // procfs è un altro file system.
                if l1 || l2 {
                    return Err(EXDEV);
                }
                if nr == 276 && a[4] & 1 != 0 && std::fs::symlink_metadata(&new).is_ok() {
                    return Err(EEXIST); // RENAME_NOREPLACE
                }
                std::fs::rename(&old, &new).map_err(|e| host_errno(&e))?;
                ret(0)
            }
            52 | 53 => {
                use std::os::unix::fs::PermissionsExt;
                let (path, mode) = if nr == 52 {
                    let f = self.tasks[t].files.borrow().get(a[0] as i64)?;
                    let p = f.borrow().guest_path.clone();
                    (p, a[1])
                } else {
                    (self.path_arg(t, a[0], a[1])?, a[2])
                };
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode as u32 & 0o7777))
                    .map_err(|e| host_errno(&e))?;
                ret(0)
            }
            54 | 55 => ret(0), // fchownat/fchown: proprietari invariati (siamo "root" per finta)
            88 => self.sys_utimensat(t, a[0], a[1], a[2], a[3]),
            33 => {
                // mknodat: FIFO e file regolari; i dispositivi solo con privilegi.
                let (p, link) = self.path_arg_nofollow(t, a[0], a[1])?;
                if link {
                    return Err(EEXIST);
                }
                let mode = (a[2] as u32 & !self.tasks[t].umask) as libc::mode_t;
                let cpath = std::ffi::CString::new(p.clone()).map_err(|_| EINVAL)?;
                // SAFETY: percorso C valido.
                let r = match a[2] as u32 & S_IFMT {
                    0 | S_IFREG => unsafe {
                        libc::open(
                            cpath.as_ptr(),
                            libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY,
                            mode as libc::c_uint,
                        )
                    },
                    S_IFIFO => unsafe { libc::mkfifo(cpath.as_ptr(), mode & 0o7777) },
                    _ => unsafe { libc::mknod(cpath.as_ptr(), a[2] as libc::mode_t, a[3] as libc::dev_t) },
                };
                if r < 0 {
                    return Err(host_errno(&std::io::Error::last_os_error()));
                }
                if a[2] as u32 & S_IFMT == 0 || a[2] as u32 & S_IFMT == S_IFREG {
                    // SAFETY: r è il descrittore appena aperto.
                    unsafe { libc::close(r) };
                }
                fs::fix_mode(std::path::Path::new(&p), mode as u32 & 0o7777);
                ret(0)
            }
            47 => {
                // fallocate: modo 0 estende il file; KEEP_SIZE non cambia nulla.
                let f = self.tasks[t].files.borrow().get(a[0] as i64)?;
                let f = f.borrow();
                let Kind::Host { file, .. } = &f.kind else { return Err(19) };
                let (mode, off, len) = (a[1], a[2] as i64, a[3] as i64);
                if off < 0 || len <= 0 {
                    return Err(EINVAL);
                }
                match mode {
                    0 => {
                        let end = (off + len) as u64;
                        let cur = file.metadata().map_err(|e| host_errno(&e))?.len();
                        if end > cur {
                            file.set_len(end).map_err(|e| host_errno(&e))?;
                        }
                        ret(0)
                    }
                    1 => ret(0),
                    _ => Err(95), // EOPNOTSUPP
                }
            }
            194 => ret(self.sys_shmget(a[0] as i32, a[1], a[2])?),
            195 => ret(self.sys_shmctl(t, a[0], a[1], a[2])?),
            196 => ret(self.sys_shmat(t, a[0], a[1], a[2])?),
            197 => ret(self.sys_shmdt(t, a[0])?),
            198 => {
                // socket: esiste, ma non si connette a nulla (niente rete fino a M7).
                let f = OpenFile::new(Kind::Socket, O_RDWR, "socket:".into());
                ret(self.tasks[t].files.borrow_mut().install(f, a[1] & O_CLOEXEC != 0, 0)?)
            }
            203 => {
                // connect: nessun servizio in ascolto (es. nscd di musl).
                let f = self.tasks[t].files.borrow().get(a[0] as i64)?;
                if !matches!(f.borrow().kind, Kind::Socket) {
                    return Err(88); // ENOTSOCK
                }
                Err(ENOENT)
            }
            82 | 83 => ret(0),
            46 => {
                let f = self.tasks[t].files.borrow().get(a[0] as i64)?;
                let f = f.borrow();
                let Kind::Host { file, .. } = &f.kind else { return Err(EINVAL) };
                file.set_len(a[1]).map_err(|e| host_errno(&e))?;
                ret(0)
            }
            43 | 44 => {
                // statfs: valori plausibili di un ext4.
                let mut b = [0u8; 120];
                b[..8].copy_from_slice(&0xEF53u64.to_le_bytes());
                b[8..16].copy_from_slice(&4096u64.to_le_bytes());
                b[16..24].copy_from_slice(&(1u64 << 20).to_le_bytes());
                b[24..32].copy_from_slice(&(1u64 << 19).to_le_bytes());
                b[32..40].copy_from_slice(&(1u64 << 19).to_le_bytes());
                b[64..72].copy_from_slice(&255u64.to_le_bytes());
                b[72..80].copy_from_slice(&4096u64.to_le_bytes());
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[1], &b)?;
                ret(0)
            }
            23 => {
                let f = self.tasks[t].files.borrow().get(a[0] as i64)?;
                ret(self.tasks[t].files.borrow_mut().install(f, false, 0)?)
            }
            24 => {
                if a[0] == a[1] || a[2] & !O_CLOEXEC != 0 {
                    return Err(EINVAL);
                }
                let f = self.tasks[t].files.borrow().get(a[0] as i64)?;
                ret(self.tasks[t].files.borrow_mut().install_at(a[1] as usize, f, a[2] & O_CLOEXEC != 0)?)
            }
            25 => self.sys_fcntl(t, a[0] as i64, a[1], a[2]),
            29 => {
                const FIONREAD: u64 = 0x541b;
                let f = self.tasks[t].files.borrow().get(a[0] as i64)?;
                let n = match (&f.borrow_mut().kind, a[1]) {
                    (Kind::PipeR(p) | Kind::PipeW(p) | Kind::PipeRW(p), FIONREAD) => {
                        p.borrow().pending() as i64
                    }
                    (Kind::Host { file, .. }, FIONREAD) => {
                        use std::io::Seek;
                        let size = file.metadata().map_err(|e| host_errno(&e))?.len() as i64;
                        let pos = (&*file).stream_position().map_err(|e| host_errno(&e))? as i64;
                        (size - pos).max(0)
                    }
                    // Nessun terminale: stdout è una pipe o un file.
                    _ => return Err(ENOTTY),
                };
                write_u32(&mut self.mem(t).borrow_mut().mem, a[2], n as u32)?;
                ret(0)
            }
            59 => {
                let (r, w) = fs::new_pipe(a[1]);
                let cloexec = a[1] & O_CLOEXEC != 0;
                let rfd = self.tasks[t].files.borrow_mut().install(r, cloexec, 0)?;
                let wfd = self.tasks[t].files.borrow_mut().install(w, cloexec, 0)?;
                let mm = self.mem(t);
                write_u32(&mut mm.borrow_mut().mem, a[0], rfd as u32)?;
                write_u32(&mut mm.borrow_mut().mem, a[0] + 4, wfd as u32)?;
                ret(0)
            }
            71 => self.sys_sendfile(t, a[0] as i64, a[1] as i64, a[2], a[3] as usize),
            73 => self.sys_ppoll(t, a[0], a[1] as usize, a[2], a[3]),
            72 => ret(a[0] as i64), // pselect6: tutto pronto (approssimazione)
            166 => {
                let old = self.tasks[t].umask;
                self.tasks[t].umask = a[0] as u32 & 0o777;
                ret(old as i64)
            }

            // --- memoria ---
            214 => ret(self.mem(t).borrow_mut().sys_brk(a[0])),
            222 => self.sys_mmap(t, a),
            215 => {
                let r = self.mem(t).borrow_mut().munmap(a[0], a[1])?;
                self.flush_shared();
                ret(r)
            }
            226 => ret(self.mem(t).borrow_mut().mprotect(a[0], a[1], a[2])?),
            216 => ret(self.mem(t).borrow_mut().mremap(a[0], a[1], a[2], a[3], a[4])?),
            233 => ret(self.mem(t).borrow_mut().madvise(a[0], a[1], a[2])?),
            227 => {
                self.flush_shared(); // msync
                ret(0)
            }

            // --- processi ---
            93 => {
                let status = ((a[0] & 0xff) << 8) as i32;
                self.exit_thread(t, status);
                Ok(Sys::NoReturn)
            }
            94 => {
                let tgid = self.tasks[t].tgid;
                self.exit_group(tgid, ((a[0] & 0xff) << 8) as i32);
                Ok(Sys::NoReturn)
            }
            220 => {
                let r = self.sys_clone(t, a[0], a[1], a[2], a[3], a[4])?;
                if matches!(self.tasks[t].state, State::Blocked(Wait::Vfork { .. })) {
                    return Ok(Sys::NoReturn);
                }
                ret(r)
            }
            221 => {
                let mm = self.mem(t);
                let raw = read_cstr(&mut mm.borrow_mut().mem, a[0])?;
                let argv = read_strv(&mut mm.borrow_mut().mem, a[1])?;
                let envp = read_strv(&mut mm.borrow_mut().mem, a[2])?;
                let path = self.at_path(t, AT_FDCWD as u64, &raw)?;
                self.sys_execve(t, &path, argv, envp)?;
                Ok(Sys::NoReturn)
            }
            260 => {
                let pid = a[0] as i64 as i32;
                match self.sys_wait4(t, pid, a[1], a[2])? {
                    Some(v) => {
                        if a[3] != 0 {
                            write_bytes(&mut self.mem(t).borrow_mut().mem, a[3], &[0u8; 144])?;
                        }
                        ret(v)
                    }
                    None => Ok(Sys::Block(Wait::Child { pid })),
                }
            }
            96 => {
                self.tasks[t].clear_child_tid = a[0];
                ret(self.tasks[t].tid as i64)
            }
            98 => self.sys_futex(t, a),
            449 => self.sys_futex_waitv(t, a),
            99 => ret(0), // set_robust_list
            172 => ret(self.tasks[t].tgid as i64),
            173 => ret(self.tasks[t].ppid as i64),
            178 => ret(self.tasks[t].tid as i64),
            // Come QEMU user mode: gli id del processo sono quelli dell'host, così
            // i permessi dei file si comportano in modo coerente.
            // SAFETY: getuid & co. non hanno precondizioni.
            174 => ret(unsafe { libc::getuid() } as i64),
            175 => ret(unsafe { libc::geteuid() } as i64),
            176 => ret(unsafe { libc::getgid() } as i64),
            177 => ret(unsafe { libc::getegid() } as i64),
            158 => ret(0), // getgroups: nessun gruppo supplementare
            146 | 144 | 143 | 147 | 149 | 151 | 152 => ret(0), // set*id
            154 => {
                let pid = if a[0] == 0 { self.tasks[t].tgid } else { a[0] as i32 };
                let pgid = if a[1] == 0 { pid } else { a[1] as i32 };
                let mut found = false;
                for task in self.tasks.iter_mut().filter(|x| x.tgid == pid) {
                    task.pgid = pgid;
                    found = true;
                }
                if found { ret(0) } else { Err(ESRCH) }
            }
            155 => {
                let pid = if a[0] == 0 { self.tasks[t].tgid } else { a[0] as i32 };
                let i = self.find(pid).ok_or(ESRCH)?;
                ret(self.tasks[i].pgid as i64)
            }
            156 => ret(self.tasks[t].pgid as i64),
            157 => {
                let tgid = self.tasks[t].tgid;
                for task in self.tasks.iter_mut().filter(|x| x.tgid == tgid) {
                    task.pgid = tgid;
                }
                ret(tgid as i64)
            }
            167 => self.sys_prctl(t, a),
            92 => {
                // personality(0xffffffff) legge soltanto. È del thread
                // (current->personality), non del processo.
                let old = self.tasks[t].personality;
                if a[0] as u32 != u32::MAX {
                    self.tasks[t].personality = a[0] as u32;
                }
                ret(old as i64)
            }
            90 | 91 => self.sys_capability(t, nr == 91, a[0], a[1]),
            124 => ret(0),
            123 => {
                // sched_getaffinity: le CPU 0..cpus (al più 64).
                let len = a[1] as usize;
                if len < 8 || !len.is_multiple_of(8) {
                    return Err(EINVAL);
                }
                let n = len.min(8);
                let cpus = self.cfg.cpus.clamp(1, 64);
                let mask = if cpus == 64 { u64::MAX } else { (1u64 << cpus) - 1 };
                let b = mask.to_le_bytes()[..n].to_vec();
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[2], &b)?;
                ret(n as i64)
            }

            // --- segnali ---
            // Il kernel accetta solo sigset_t da 8 byte.
            133..=135 if a[if nr == 133 { 1 } else { 3 }] != 8 => Err(EINVAL),
            136 if a[1] > 8 => Err(EINVAL),
            134 => ret(self.sys_sigaction(t, a[0] as i64, a[1], a[2])?),
            135 => ret(self.sys_sigprocmask(t, a[0], a[1], a[2])?),
            136 => {
                let p = self.tasks[t].sig.pending;
                write_u64(&mut self.mem(t).borrow_mut().mem, a[0], p)?;
                ret(0)
            }
            139 => {
                if self.sigreturn(t).is_err() {
                    let tgid = self.tasks[t].tgid;
                    self.kill_process(tgid, sig::SIGSEGV);
                }
                Ok(Sys::NoReturn)
            }
            133 => {
                // rt_sigsuspend: la prima volta sostituisce la maschera.
                if self.tasks[t].sig.saved_mask.is_none() {
                    let m = read_u64(&mut self.mem(t).borrow_mut().mem, a[0])?;
                    let old = self.tasks[t].sig.mask;
                    self.tasks[t].sig.saved_mask = Some(old);
                    self.tasks[t].sig.mask = m & !UNBLOCKABLE;
                }
                Ok(Sys::Block(Wait::Signal))
            }
            137 => match self.sys_sigtimedwait(t, a)? {
                Ok(s) => ret(s),
                Err(w) => Ok(Sys::Block(w)),
            },
            132 => self.sys_sigaltstack(t, a[0], a[1]),
            129 => self.sys_kill(t, a[0] as i64 as i32, a[1] as i32),
            130 | 131 => {
                let (tid, s) =
                    if nr == 130 { (a[0] as i32, a[1] as i32) } else { (a[1] as i32, a[2] as i32) };
                let i = self.find(tid).ok_or(ESRCH)?;
                if !(0..=64).contains(&s) {
                    return Err(EINVAL);
                }
                let from = self.tasks[t].tgid;
                self.send_to_thread(i, s, from, -6, 0); // SI_TKILL
                ret(0)
            }
            102 | 103 => self.sys_itimer(t, nr, a[0], a[1], a[2]),

            // --- tempo ---
            113 | 114 => {
                let ns = match (nr, a[0]) {
                    (114, _) => 1,
                    (_, 0 | 5 | 8 | 11) => self.realtime(),
                    (_, 1 | 4 | 6 | 7 | 9) => self.now(),
                    (_, 2 | 3) => self.now(), // CPU time: tempo del processo ≈ monotono
                    _ => return Err(EINVAL),
                };
                if a[1] != 0 {
                    let mm = self.mem(t);
                    write_u64(&mut mm.borrow_mut().mem, a[1], ns / 1_000_000_000)?;
                    write_u64(&mut mm.borrow_mut().mem, a[1] + 8, ns % 1_000_000_000)?;
                }
                ret(0)
            }
            169 => {
                let ns = self.realtime();
                if a[0] != 0 {
                    let mm = self.mem(t);
                    write_u64(&mut mm.borrow_mut().mem, a[0], ns / 1_000_000_000)?;
                    write_u64(&mut mm.borrow_mut().mem, a[0] + 8, ns % 1_000_000_000 / 1000)?;
                }
                if a[1] != 0 {
                    // struct timezone: minuti a ovest di Greenwich e DST, entrambi 0.
                    write_u64(&mut self.mem(t).borrow_mut().mem, a[1], 0)?;
                }
                ret(0)
            }
            // clock_nanosleep: gli orologi senza nsleep danno EOPNOTSUPP
            // (THREAD_CPUTIME, MONOTONIC_RAW, *_COARSE), quelli ignoti EINVAL.
            115 if matches!(a[0] as i32, 3..=6) => Err(95),
            115 if !matches!(a[0] as i32, 0..=2 | 7..=9 | 11) => Err(EINVAL),
            101 | 115 => {
                let (ts, abs) = if nr == 101 { (a[0], false) } else { (a[2], a[1] & 1 != 0) };
                if self.tasks[t].deadline.is_none() {
                    let mm = self.mem(t);
                    let s = read_u64(&mut mm.borrow_mut().mem, ts)?;
                    let n = read_u64(&mut mm.borrow_mut().mem, ts + 8)?;
                    let d = timespec_ns(s, n)?;
                    let realtime = nr == 115 && a[0] == 0;
                    let until = if abs {
                        if realtime { d.saturating_sub(self.realtime() - self.now()) } else { d }
                    } else {
                        self.now().saturating_add(d)
                    };
                    self.tasks[t].deadline = Some(until);
                }
                let until = self.tasks[t].deadline.unwrap();
                if self.now() >= until {
                    return ret(0);
                }
                Ok(Sys::Block(Wait::Sleep { until }))
            }
            153 => {
                let ticks = self.now() / 10_000_000;
                if a[0] != 0 {
                    let mut b = [0u8; 32];
                    b[..8].copy_from_slice(&ticks.to_le_bytes());
                    write_bytes(&mut self.mem(t).borrow_mut().mem, a[0], &b)?;
                }
                ret(ticks as i64)
            }

            // --- sistema ---
            160 => {
                let mut b = vec![0u8; 65 * 6];
                // UNAME26: la versione come la riscrive override_release.
                let uname26 =
                    (self.tasks[t].personality & 0x002_0000 != 0).then(|| uname26(&self.cfg.release));
                let release = uname26.as_deref().unwrap_or(&self.cfg.release);
                for (i, s) in ["Linux", "vetro", release, "#1 SMP", "aarch64", "(none)"].iter().enumerate() {
                    let n = s.len().min(64);
                    b[i * 65..i * 65 + n].copy_from_slice(&s.as_bytes()[..n]);
                }
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[0], &b)?;
                ret(0)
            }
            163 => self.sys_prlimit(t, 0, a[0], 0, a[1]),
            164 => self.sys_prlimit(t, 0, a[0], a[1], 0),
            261 => self.sys_prlimit(t, a[0] as i64 as i32, a[1], a[2], a[3]),
            165 => {
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[1], &[0u8; 144])?;
                ret(0)
            }
            179 => {
                let mut b = [0u8; 112];
                b[..8].copy_from_slice(&(self.now() / 1_000_000_000).to_le_bytes());
                b[32..40].copy_from_slice(&(4u64 << 30).to_le_bytes());
                b[40..48].copy_from_slice(&(3u64 << 30).to_le_bytes());
                b[80..82].copy_from_slice(&(self.tasks.len() as u16).to_le_bytes());
                b[104..108].copy_from_slice(&1u32.to_le_bytes());
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[0], &b)?;
                ret(0)
            }
            278 => {
                // GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE; RANDOM e INSECURE insieme no.
                if a[2] & !7 != 0 || a[2] & 6 == 6 {
                    return Err(EINVAL);
                }
                let n = (a[1] as usize).min(1 << 20);
                let bytes = self.random_bytes(n);
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[0], &bytes)?;
                ret(n as i64)
            }
            _ => Ok(Sys::Unsupported),
        }
    }

    fn sys_openat(&mut self, t: usize, dirfd: u64, p: u64, flags: u64, mode: u32) -> R {
        // O_NOFOLLOW su un link di /proc/<pid>/fd: ELOOP (con O_PATH si apre il
        // link stesso, che qui si tratta come il file).
        if flags & O_NOFOLLOW != 0 && flags & O_PATH == 0 && self.path_arg_nofollow(t, dirfd, p)?.1 {
            return Err(40);
        }
        let path = self.path_arg(t, dirfd, p)?;
        if let Some(entries) = self.proc_dir(t, &path) {
            if flags & O_ACCMODE != 0 {
                return Err(EISDIR);
            }
            let kind = Kind::Dir { path: "/".into(), entries: Some(entries), pos: 0 };
            let f = OpenFile::new(kind, flags & (O_ACCMODE | O_NONBLOCK), path.trim_end_matches('/').into());
            return ret(self.tasks[t].files.borrow_mut().install(f, flags & O_CLOEXEC != 0, 0)?);
        }
        if let Some(pid) = path.strip_prefix("/proc/").and_then(|r| r.strip_suffix("/pagemap")) {
            let pid = match pid {
                "self" => self.tasks[t].tgid,
                "thread-self" => self.tasks[t].tid,
                p => p.parse().map_err(|_| ENOENT)?,
            };
            let i = self.find(pid).ok_or(ENOENT)?;
            let kind = Kind::Pagemap { mm: self.tasks[i].mm.clone(), pos: 0 };
            let f = OpenFile::new(kind, flags & O_ACCMODE, path.clone());
            return ret(self.tasks[t].files.borrow_mut().install(f, flags & O_CLOEXEC != 0, 0)?);
        }
        let file = match self.proc_file(t, &path) {
            Some(r) => r?,
            None => {
                use std::os::unix::fs::{FileTypeExt, MetadataExt};
                // O_DIRECTORY e O_NOFOLLOW (su un link) falliscono subito in
                // fs::open, senza aspettare l'altro capo della FIFO.
                let fifo_ok = flags & (O_DIRECTORY | O_PATH) == 0
                    && (flags & O_NOFOLLOW == 0
                        || std::fs::symlink_metadata(&path).is_ok_and(|m| !m.file_type().is_symlink()));
                match std::fs::metadata(&path) {
                    Ok(m)
                        if fifo_ok
                            && (m.file_type().is_fifo() && flags & O_CREAT == 0
                                || m_is_fifo_creat(&path, flags)) =>
                    {
                        let m = std::fs::metadata(&path).map_err(|e| host_errno(&e))?;
                        match self.open_fifo(t, (m.dev(), m.ino()), flags, &path)? {
                            Some(f) => f,
                            None => return Ok(Sys::Block(Wait::Retry)),
                        }
                    }
                    _ => fs::open(&path, flags, mode, self.tasks[t].umask)?,
                }
            }
        };
        ret(self.tasks[t].files.borrow_mut().install(file, flags & O_CLOEXEC != 0, 0)?)
    }

    /// open() di una FIFO: `None` se deve aspettare l'altro capo.
    fn open_fifo(
        &mut self,
        t: usize,
        key: (u64, u64),
        flags: u64,
        path: &str,
    ) -> Result<Option<Rc<RefCell<OpenFile>>>, i64> {
        let tid = self.tasks[t].tid;
        let fifo = self.fifos.entry(key).or_default();
        let nonblock = flags & O_NONBLOCK != 0;
        let status = flags & (O_ACCMODE | O_APPEND | O_NONBLOCK);
        let (readers, writers) = {
            let p = fifo.pipe.borrow();
            (p.readers() + fifo.waiting_readers.len(), p.writers() + fifo.waiting_writers.len())
        };
        let kind = match flags & O_ACCMODE {
            0 => {
                if !nonblock && writers == 0 {
                    if !fifo.waiting_readers.contains(&tid) {
                        fifo.waiting_readers.push(tid);
                    }
                    return Ok(None);
                }
                fifo.waiting_readers.retain(|&x| x != tid);
                Kind::PipeR(fifo.pipe.clone())
            }
            O_WRONLY => {
                if readers == 0 {
                    if nonblock {
                        return Err(6); // ENXIO
                    }
                    if !fifo.waiting_writers.contains(&tid) {
                        fifo.waiting_writers.push(tid);
                    }
                    return Ok(None);
                }
                fifo.waiting_writers.retain(|&x| x != tid);
                Kind::PipeW(fifo.pipe.clone())
            }
            _ => Kind::PipeRW(fifo.pipe.clone()),
        };
        Ok(Some(OpenFile::new(kind, status, path.to_string())))
    }

    /// File di /proc/self (e /proc/<pid>) generati: il /proc dell'host
    /// descriverebbe l'emulatore, non il guest.
    fn proc_file(&self, t: usize, path: &str) -> Option<Result<Rc<RefCell<OpenFile>>, i64>> {
        let data = match self.proc_content(t, path)? {
            Ok(d) => d,
            Err(e) => return Some(Err(e)),
        };
        Some(Ok(OpenFile::new(Kind::Mem { data, pos: 0 }, 0, path.to_string())))
    }

    fn rng_fn(&mut self) -> impl FnMut(usize) -> Vec<u8> + '_ {
        move |n| self.random_bytes(n)
    }

    fn sys_read(&mut self, t: usize, fd: i64, buf: u64, len: usize) -> R {
        let f = self.tasks[t].files.borrow().get(fd)?;
        let len = len.min(1 << 24);
        let io = {
            let mut rng = self.rng_fn();
            f.borrow_mut().read(len, &mut rng)
        };
        match io {
            Io::Done(data) => {
                write_bytes(&mut self.mem(t).borrow_mut().mem, buf, &data)?;
                ret(data.len() as i64)
            }
            Io::Block => Ok(Sys::Block(Wait::Pipe)),
            Io::Err(e) => Err(e),
            _ => Err(EIO),
        }
    }

    fn do_write(&mut self, t: usize, fd: i64, data: &[u8]) -> R {
        let f = self.tasks[t].files.borrow().get(fd)?;
        let oom = f.borrow().guest_path.clone();
        if matches!(f.borrow().kind, Kind::Mem { .. }) && oom.ends_with("/oom_score_adj") {
            return ret(self.write_oom_score_adj(t, &oom, data)? as i64);
        }
        let io = f.borrow_mut().write(data);
        match io {
            Io::Written(n) => {
                let pipe = match &f.borrow().kind {
                    Kind::PipeW(p) | Kind::PipeRW(p) => Some(p.clone()),
                    _ => None,
                };
                if let Some(p) = pipe {
                    self.notify_async(&p);
                }
                ret(n as i64)
            }
            Io::Block => Ok(Sys::Block(Wait::Pipe)),
            Io::BrokenPipe => {
                let tid = self.tasks[t].tid;
                self.send_to_thread(t, sig::SIGPIPE, tid, 0, 0);
                Err(EPIPE)
            }
            Io::Err(e) => Err(e),
            Io::Done(_) => Err(EIO),
        }
    }

    fn sys_write(&mut self, t: usize, fd: i64, buf: u64, len: usize) -> R {
        let data = read_bytes(&mut self.mem(t).borrow_mut().mem, buf, len.min(1 << 24))?;
        self.do_write(t, fd, &data)
    }

    fn iovecs(&self, t: usize, iov: u64, cnt: usize) -> Result<Vec<(u64, usize)>, i64> {
        if cnt > 1024 {
            return Err(EINVAL);
        }
        let mm = self.mem(t);
        let mut v = Vec::with_capacity(cnt);
        let mut total: u64 = 0;
        for i in 0..cnt as u64 {
            let base = read_u64(&mut mm.borrow_mut().mem, iov + 16 * i)?;
            let len = read_u64(&mut mm.borrow_mut().mem, iov + 16 * i + 8)?;
            // Una lunghezza negativa come ssize_t, o una somma che trabocca: EINVAL.
            total = total.checked_add(len).filter(|&s| s <= i64::MAX as u64).ok_or(EINVAL)?;
            v.push((base, len as usize));
        }
        Ok(v)
    }

    fn sys_writev(&mut self, t: usize, fd: i64, iov: u64, cnt: usize) -> R {
        let mut data = Vec::new();
        for (b, l) in self.iovecs(t, iov, cnt)? {
            data.extend(read_bytes(&mut self.mem(t).borrow_mut().mem, b, l)?);
        }
        self.do_write(t, fd, &data)
    }

    fn sys_readv(&mut self, t: usize, fd: i64, iov: u64, cnt: usize) -> R {
        let vecs = self.iovecs(t, iov, cnt)?;
        let total: usize = vecs.iter().map(|v| v.1).fold(0usize, usize::saturating_add).min(1 << 24);
        let f = self.tasks[t].files.borrow().get(fd)?;
        let io = {
            let mut rng = self.rng_fn();
            f.borrow_mut().read(total, &mut rng)
        };
        match io {
            Io::Done(data) => {
                let mut off = 0;
                for (b, l) in vecs {
                    if off >= data.len() {
                        break;
                    }
                    let n = l.min(data.len() - off);
                    write_bytes(&mut self.mem(t).borrow_mut().mem, b, &data[off..off + n])?;
                    off += n;
                }
                ret(data.len() as i64)
            }
            Io::Block => Ok(Sys::Block(Wait::Pipe)),
            Io::Err(e) => Err(e),
            _ => Err(EIO),
        }
    }

    fn sys_pio(&mut self, t: usize, write: bool, fd: i64, buf: u64, len: usize, off: i64) -> R {
        if write {
            let data = read_bytes(&mut self.mem(t).borrow_mut().mem, buf, len)?;
            ret(self.pwrite_at(t, fd, &data, off)? as i64)
        } else {
            let data = self.pread_at(t, fd, len, off)?;
            write_bytes(&mut self.mem(t).borrow_mut().mem, buf, &data)?;
            ret(data.len() as i64)
        }
    }

    /// Il file di un pread/pwrite: solo file dell'host (una directory dà
    /// EISDIR, una pipe ESPIPE), con il modo d'apertura giusto.
    fn positional_file(&self, t: usize, fd: i64, write: bool) -> Result<Rc<RefCell<OpenFile>>, i64> {
        let f = self.tasks[t].files.borrow().get(fd)?;
        {
            let fb = f.borrow();
            if write && !fb.writable() || !write && !fb.readable() {
                return Err(EBADF);
            }
            match &fb.kind {
                Kind::Host { .. } => {}
                Kind::Dir { .. } => return Err(EISDIR),
                _ => return Err(ESPIPE),
            }
        }
        Ok(f)
    }

    fn pwrite_at(&self, t: usize, fd: i64, data: &[u8], off: i64) -> Result<usize, i64> {
        use std::os::unix::fs::FileExt;
        if off < 0 {
            return Err(EINVAL);
        }
        let f = self.positional_file(t, fd, true)?;
        let fb = f.borrow();
        let Kind::Host { file, .. } = &fb.kind else { unreachable!() };
        // Con O_APPEND Linux scrive in fondo, qualunque sia l'offset.
        let off = if fb.flags & O_APPEND != 0 {
            file.metadata().map_err(|e| host_errno(&e))?.len()
        } else {
            off as u64
        };
        file.write_at(data, off).map_err(|e| host_errno(&e))
    }

    fn pread_at(&self, t: usize, fd: i64, len: usize, off: i64) -> Result<Vec<u8>, i64> {
        use std::os::unix::fs::FileExt;
        if off < 0 {
            return Err(EINVAL);
        }
        // I file generati di /proc (e pagemap) si leggono anche a un offset,
        // senza spostare la posizione del descrittore.
        {
            let f = self.tasks[t].files.borrow().get(fd)?;
            let mut fb = f.borrow_mut();
            if matches!(fb.kind, Kind::Mem { .. } | Kind::Pagemap { .. }) {
                let saved = fb.lseek(0, 1)?;
                fb.lseek(off, 0)?;
                let io = fb.read(len.min(1 << 24), &mut |_| Vec::new());
                fb.lseek(saved, 0)?;
                return match io {
                    Io::Done(d) => Ok(d),
                    Io::Err(e) => Err(e),
                    _ => Err(EIO),
                };
            }
        }
        let f = self.positional_file(t, fd, false)?;
        let fb = f.borrow();
        let Kind::Host { file, .. } = &fb.kind else { unreachable!() };
        let mut data = vec![0u8; len.min(1 << 24)];
        let n = file.read_at(&mut data, off as u64).map_err(|e| host_errno(&e))?;
        data.truncate(n);
        Ok(data)
    }

    /// preadv/pwritev (69/70) e preadv2/pwritev2 (286/287): come readv/writev
    /// a un offset. Con le varianti 2 un offset di -1 vuol dire la posizione
    /// corrente; i flag RWF_* non sono supportati (EOPNOTSUPP).
    fn sys_piov(&mut self, t: usize, nr: u64, a: [u64; 6]) -> R {
        let (fd, iov, cnt, off) = (a[0] as i64, a[1], a[2] as usize, a[3] as i64);
        let write = matches!(nr, 70 | 287);
        let v2 = nr >= 286;
        if v2 && a[5] != 0 {
            return Err(95);
        }
        if v2 && off == -1 {
            return if write { self.sys_writev(t, fd, iov, cnt) } else { self.sys_readv(t, fd, iov, cnt) };
        }
        let vecs = self.iovecs(t, iov, cnt)?;
        if write {
            let mut data = Vec::new();
            for (b, l) in vecs {
                data.extend(read_bytes(&mut self.mem(t).borrow_mut().mem, b, l)?);
            }
            ret(self.pwrite_at(t, fd, &data, off)? as i64)
        } else {
            let total: usize = vecs.iter().map(|v| v.1).fold(0usize, usize::saturating_add);
            let data = self.pread_at(t, fd, total, off)?;
            let mut done = 0;
            for (b, l) in vecs {
                if done >= data.len() {
                    break;
                }
                let n = l.min(data.len() - done);
                write_bytes(&mut self.mem(t).borrow_mut().mem, b, &data[done..done + n])?;
                done += n;
            }
            ret(data.len() as i64)
        }
    }

    fn sys_sendfile(&mut self, t: usize, out: i64, inp: i64, offp: u64, count: usize) -> R {
        let f = self.tasks[t].files.borrow().get(inp)?;
        let data = if offp != 0 {
            use std::os::unix::fs::FileExt;
            let off = read_u64(&mut self.mem(t).borrow_mut().mem, offp)?;
            let fb = f.borrow();
            let Kind::Host { file, .. } = &fb.kind else { return Err(EINVAL) };
            let mut d = vec![0u8; count.min(1 << 24)];
            let n = file.read_at(&mut d, off).map_err(|e| host_errno(&e))?;
            d.truncate(n);
            write_u64(&mut self.mem(t).borrow_mut().mem, offp, off + n as u64)?;
            d
        } else {
            let mut rng = self.rng_fn();
            match f.borrow_mut().read(count.min(1 << 24), &mut rng) {
                Io::Done(d) => d,
                Io::Err(e) => return Err(e),
                _ => return Err(EAGAIN),
            }
        };
        if data.is_empty() {
            return ret(0);
        }
        self.do_write(t, out, &data)
    }

    fn sys_ppoll(&mut self, t: usize, fds: u64, n: usize, timeout: u64, sigmask: u64) -> R {
        let mm = self.mem(t);
        // Maschera temporanea durante l'attesa, come rt_sigsuspend.
        if sigmask != 0 && self.tasks[t].sig.saved_mask.is_none() {
            let m = read_u64(&mut mm.borrow_mut().mem, sigmask)?;
            let old = self.tasks[t].sig.mask;
            self.tasks[t].sig.saved_mask = Some(old);
            self.tasks[t].sig.mask = m & !UNBLOCKABLE;
        }
        let mut ready = 0;
        for i in 0..n as u64 {
            let fd = read_u32(&mut mm.borrow_mut().mem, fds + 8 * i)? as i32;
            let events = read_u32(&mut mm.borrow_mut().mem, fds + 8 * i + 4)? as u16;
            let rev: u16 = if fd < 0 {
                0
            } else {
                match self.tasks[t].files.borrow().get(fd as i64) {
                    Err(_) => 0x20, // POLLNVAL
                    Ok(f) => {
                        let f = f.borrow();
                        match &f.kind {
                            Kind::PipeR(_) | Kind::PipeW(_) | Kind::PipeRW(_) => {
                                if f.is_pipe_ready() {
                                    events & 0x5
                                } else {
                                    0
                                }
                            }
                            _ => events & 0x5, // file e console: sempre pronti
                        }
                    }
                }
            };
            let mut b = [0u8; 2];
            b.copy_from_slice(&rev.to_le_bytes());
            write_bytes(&mut mm.borrow_mut().mem, fds + 8 * i + 6, &b)?;
            if rev != 0 {
                ready += 1;
            }
        }
        if ready == 0 {
            // Scadenza (relativa) fissata al primo blocco; NULL = per sempre.
            if timeout != 0 && self.tasks[t].deadline.is_none() {
                let s = read_u64(&mut mm.borrow_mut().mem, timeout)?;
                let ns = read_u64(&mut mm.borrow_mut().mem, timeout + 8)?;
                self.tasks[t].deadline = Some(self.now().saturating_add(timespec_ns(s, ns)?));
            }
            let until = self.tasks[t].deadline;
            if until.is_none_or(|u| self.now() < u) {
                // Senza descrittori (pause() di musl) sveglia solo un segnale.
                return Ok(Sys::Block(if n == 0 && until.is_none() {
                    Wait::Signal
                } else {
                    Wait::Poll { until }
                }));
            }
        }
        self.restore_saved_mask(t);
        ret(ready)
    }

    /// Fine di un'attesa con maschera temporanea terminata senza segnali.
    fn restore_saved_mask(&mut self, t: usize) {
        if let Some(m) = self.tasks[t].sig.saved_mask.take() {
            self.tasks[t].sig.mask = m;
        }
    }

    fn sys_fcntl(&mut self, t: usize, fd: i64, cmd: u64, arg: u64) -> R {
        let mut files = self.tasks[t].files.borrow_mut();
        let f = files.get(fd)?;
        match cmd {
            0 | 1030 => ret(files.install(f, cmd == 1030, arg as usize)?),
            1 => ret(files.entry(fd)?.cloexec as i64),
            2 => {
                files.entry(fd)?.cloexec = arg & 1 != 0;
                ret(0)
            }
            3 => ret(f.borrow().flags as i64),
            4 => {
                let mut fb = f.borrow_mut();
                fb.flags = (fb.flags & O_ACCMODE) | (arg & (O_APPEND | O_NONBLOCK | O_ASYNC));
                if arg & O_ASYNC != 0
                    && let Kind::PipeR(p) | Kind::PipeRW(p) = &fb.kind
                {
                    let mut p = p.borrow_mut();
                    p.async_readers
                        .retain(|w| w.strong_count() > 0 && !Rc::ptr_eq(&w.upgrade().unwrap(), &f));
                    p.async_readers.push(Rc::downgrade(&f));
                }
                ret(0)
            }
            // F_SETOWN / F_GETOWN: pid > 0 processo, < 0 gruppo.
            8 => {
                let who = arg as i32;
                drop(files);
                // -INT_MIN non è un gruppo (f_setown: EINVAL).
                if who == i32::MIN {
                    return Err(EINVAL);
                }
                // find_vpid trova qualunque id (anche il tid di un thread).
                if who != 0 && !self.id_exists(if who < 0 { 2 } else { 0 }, who.unsigned_abs() as i32) {
                    return Err(ESRCH);
                }
                f.borrow_mut().owner = if who < 0 { (2, -who) } else { (1, who) };
                ret(0)
            }
            9 => {
                let (ty, id) = f.borrow().owner;
                ret(if ty == 2 { -(id as i64) } else { id as i64 })
            }
            // F_SETSIG / F_GETSIG
            10 => {
                if arg > 64 {
                    return Err(EINVAL);
                }
                f.borrow_mut().sigio = arg as i32;
                ret(0)
            }
            11 => ret(f.borrow().sigio as i64),
            // F_SETOWN_EX / F_GETOWN_EX: struct f_owner_ex { int type; pid_t pid; }
            15 => {
                drop(files);
                let b = read_bytes(&mut self.mem(t).borrow_mut().mem, arg, 8)?;
                let ty = i32::from_le_bytes(b[0..4].try_into().unwrap());
                let id = i32::from_le_bytes(b[4..8].try_into().unwrap());
                if !(0..=2).contains(&ty) {
                    return Err(EINVAL);
                }
                if id != 0 && !self.id_exists(ty, id) {
                    return Err(ESRCH);
                }
                f.borrow_mut().owner = (ty, id);
                ret(0)
            }
            16 => {
                drop(files);
                let (ty, id) = f.borrow().owner;
                let mut b = ty.to_le_bytes().to_vec();
                b.extend_from_slice(&id.to_le_bytes());
                write_bytes(&mut self.mem(t).borrow_mut().mem, arg, &b)?;
                ret(0)
            }
            // F_SETLEASE / F_GETLEASE
            1024 => {
                drop(files);
                self.set_lease(&f, arg as i16)
            }
            1025 => ret(f.borrow().lease as i64),
            1028 => ret(f.borrow().created as i64), // F_CREATED_QUERY
            // F_SETPIPE_SZ / F_GETPIPE_SZ
            1031 | 1032 => {
                let fb = f.borrow();
                let (Kind::PipeR(p) | Kind::PipeW(p) | Kind::PipeRW(p)) = &fb.kind else { return Err(EBADF) };
                if cmd == 1032 {
                    return ret(p.borrow().capacity() as i64);
                }
                // SAFETY: geteuid non ha precondizioni.
                let root = unsafe { libc::geteuid() } == 0;
                ret(p.borrow_mut().set_capacity(arg, root)? as i64)
            }
            5..=7 | 36..=38 => {
                drop(files);
                match self.fcntl_lock(t, fd, cmd, arg)? {
                    super::locks::LockResult::Done(v) => ret(v),
                    super::locks::LockResult::Wait => Ok(Sys::Block(Wait::Retry)),
                }
            }
            _ => Err(EINVAL),
        }
    }

    /// prlimit64 (e getrlimit/setrlimit con pid 0).
    fn sys_prlimit(&mut self, t: usize, pid: i32, res: u64, new: u64, old: u64) -> R {
        let res = res as usize;
        if res >= super::RLIM_NLIMITS {
            return Err(EINVAL);
        }
        let target = if pid == 0 {
            t
        } else {
            self.tasks
                .iter()
                .position(|x| x.tgid == pid && x.tid == pid && x.state != State::Dead)
                .ok_or(ESRCH)?
        };
        let lim = if new != 0 {
            let mm = self.mem(t);
            let cur = read_u64(&mut mm.borrow_mut().mem, new)?;
            let max = read_u64(&mut mm.borrow_mut().mem, new + 8)?;
            if cur > max {
                return Err(EINVAL);
            }
            // SAFETY: geteuid non ha precondizioni.
            let root = unsafe { libc::geteuid() } == 0;
            if max > self.tasks[target].rlimits[res].1 && !root || res == 7 && max > 1 << 20 {
                return Err(EPERM);
            }
            Some((cur, max))
        } else {
            None
        };
        if old != 0 {
            let (cur, max) = self.tasks[target].rlimits[res];
            let mm = self.mem(t);
            write_u64(&mut mm.borrow_mut().mem, old, cur)?;
            write_u64(&mut mm.borrow_mut().mem, old + 8, max)?;
        }
        if let Some(l) = lim {
            let tgid = self.tasks[target].tgid;
            for x in self.tasks.iter_mut().filter(|x| x.tgid == tgid) {
                x.rlimits[res] = l;
                if res == 7 {
                    x.files.borrow_mut().limit = Some(l.0.min(1 << 20) as usize);
                }
            }
        }
        ret(0)
    }

    /// Esiste un thread (0), processo (1) o gruppo (2) con questo id?
    fn id_exists(&self, ty: i32, id: i32) -> bool {
        self.tasks.iter().any(|x| {
            x.state != State::Dead
                && !matches!(x.state, State::Zombie { .. })
                && match ty {
                    0 => x.tid == id,
                    1 => x.tgid == id,
                    _ => x.pgid == id,
                }
        })
    }

    /// Segnale di I/O ai proprietari dei capi di lettura con O_ASYNC.
    fn notify_async(&mut self, p: &Rc<RefCell<fs::Pipe>>) {
        const POLL_IN: i32 = 1;
        let readers: Vec<_> = p.borrow().async_readers.iter().filter_map(|w| w.upgrade()).collect();
        for r in readers {
            let (flags, (ty, id), sigio) = {
                let r = r.borrow();
                (r.flags, r.owner, r.sigio)
            };
            if flags & O_ASYNC == 0 || id == 0 {
                continue;
            }
            let (s, code) = if sigio == 0 { (sig::SIGIO, 0x80) } else { (sigio, POLL_IN) };
            match ty {
                0 => {
                    if let Some(i) = self.find(id) {
                        self.send_to_thread(i, s, 0, code, 0);
                    }
                }
                1 => {
                    self.send_to_process(id, s, 0, code);
                }
                _ => {
                    let mut tgids: Vec<i32> = self
                        .tasks
                        .iter()
                        .filter(|x| x.pgid == id && x.tid == x.tgid)
                        .map(|x| x.tgid)
                        .collect();
                    tgids.dedup();
                    for g in tgids {
                        self.send_to_process(g, s, 0, code);
                    }
                }
            }
        }
    }

    /// F_SETLEASE, con le regole di generic_setlease.
    fn set_lease(&mut self, f: &Rc<RefCell<OpenFile>>, kind: i16) -> R {
        use std::os::unix::fs::MetadataExt;
        const F_RDLCK: i16 = 0;
        const F_WRLCK: i16 = 1;
        const F_UNLCK: i16 = 2;
        let (key, uid, writable) = {
            let fb = f.borrow();
            let Kind::Host { file, .. } = &fb.kind else { return Err(EINVAL) };
            let m = file.metadata().map_err(|e| host_errno(&e))?;
            if !m.is_file() {
                return Err(EINVAL);
            }
            ((m.dev(), m.ino()), m.uid(), fb.flags & O_ACCMODE != 0)
        };
        if !matches!(kind, F_RDLCK | F_WRLCK | F_UNLCK) {
            return Err(EINVAL);
        }
        // SAFETY: geteuid non ha precondizioni.
        let euid = unsafe { libc::geteuid() };
        if kind != F_UNLCK && euid != uid && euid != 0 {
            return Err(EACCES);
        }
        // Gli altri aperti dello stesso file (descrizioni distinte).
        let mut seen: Vec<*const RefCell<OpenFile>> = Vec::new();
        let (mut others, mut other_writers) = (0, 0);
        for task in &self.tasks {
            if matches!(task.state, State::Dead | State::Zombie { .. }) {
                continue;
            }
            for o in task.files.borrow().files() {
                if Rc::ptr_eq(o, f) || seen.contains(&Rc::as_ptr(o)) {
                    continue;
                }
                seen.push(Rc::as_ptr(o));
                let ob = o.borrow();
                if let Kind::Host { file, .. } = &ob.kind
                    && file.metadata().is_ok_and(|m| (m.dev(), m.ino()) == key)
                {
                    others += 1;
                    if ob.flags & O_ACCMODE != 0 {
                        other_writers += 1;
                    }
                }
            }
        }
        match kind {
            F_RDLCK if writable || other_writers > 0 => return Err(EAGAIN),
            F_WRLCK if others > 0 => return Err(EAGAIN),
            _ => {}
        }
        f.borrow_mut().lease = kind;
        ret(0)
    }

    fn stat_path(&self, t: usize, dirfd: u64, p: u64, flags: u64) -> Result<Stat, i64> {
        let raw = read_cstr(&mut self.mem(t).borrow_mut().mem, p)?;
        if raw.is_empty() {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(ENOENT);
            }
            if dirfd as i64 as i32 == AT_FDCWD {
                let cwd = self.tasks[t].cwd.borrow().clone();
                return std::fs::metadata(cwd).map(|m| Stat::from_host(&m)).map_err(|e| host_errno(&e));
            }
            return self.tasks[t].files.borrow().get(dirfd as i64)?.borrow().stat();
        }
        let unresolved = self.at_path_raw(t, dirfd, &raw)?;
        if flags & AT_SYMLINK_NOFOLLOW != 0 && self.proc_fd_target(t, &unresolved).is_some() {
            // I link di /proc/<pid>/fd: lrwx------, 64 byte.
            return Ok(Stat {
                mode: S_IFLNK | 0o700,
                nlink: 1,
                size: 64,
                blksize: 1024,
                ..Default::default()
            });
        }
        let path = self.at_path(t, dirfd, &raw)?;
        if self.proc_dir(t, &path).is_some() {
            return Ok(Stat { mode: S_IFDIR | 0o555, nlink: 2, blksize: 1024, ..Default::default() });
        }
        if let Some(r) = self.proc_content(t, &path) {
            r?;
            return Ok(Stat { mode: S_IFREG | 0o444, nlink: 1, blksize: 1024, ..Default::default() });
        }
        match path.as_str() {
            "/dev/null" | "/dev/zero" | "/dev/urandom" | "/dev/random" => {
                return Ok(Stat {
                    mode: S_IFCHR | 0o666,
                    rdev: 0x103,
                    blksize: 4096,
                    nlink: 1,
                    ..Default::default()
                });
            }
            _ => {}
        }
        let m = if flags & AT_SYMLINK_NOFOLLOW != 0 {
            std::fs::symlink_metadata(&path)
        } else {
            std::fs::metadata(&path)
        };
        m.map(|m| Stat::from_host(&m)).map_err(|e| host_errno(&e))
    }

    fn sys_fstatat(&mut self, t: usize, dirfd: u64, p: u64, buf: u64, flags: u64) -> R {
        let st = self.stat_path(t, dirfd, p, flags)?;
        write_bytes(&mut self.mem(t).borrow_mut().mem, buf, &st.to_bytes())?;
        ret(0)
    }

    fn sys_statx(&mut self, t: usize, dirfd: u64, p: u64, flags: u64, buf: u64) -> R {
        let st = self.stat_path(t, dirfd, p, flags)?;
        let mut b = [0u8; 256];
        let mut put = |off: usize, v: &[u8]| b[off..off + v.len()].copy_from_slice(v);
        put(0, &0x7ffu32.to_le_bytes()); // STATX_BASIC_STATS
        put(4, &(st.blksize as u32).to_le_bytes());
        put(16, &st.nlink.to_le_bytes());
        put(20, &st.uid.to_le_bytes());
        put(24, &st.gid.to_le_bytes());
        put(28, &(st.mode as u16).to_le_bytes());
        put(32, &st.ino.to_le_bytes());
        put(40, &(st.size as u64).to_le_bytes());
        put(48, &(st.blocks as u64).to_le_bytes());
        for (off, (s, n)) in [(64, st.atime), (96, st.ctime), (112, st.mtime)] {
            put(off, &s.to_le_bytes());
            put(off + 8, &(n as u32).to_le_bytes());
        }
        put(128, &((st.rdev >> 8) as u32 & 0xfff).to_le_bytes());
        put(132, &(st.rdev as u32 & 0xff).to_le_bytes());
        put(136, &((st.dev >> 8) as u32 & 0xfff).to_le_bytes());
        put(140, &(st.dev as u32 & 0xff).to_le_bytes());
        write_bytes(&mut self.mem(t).borrow_mut().mem, buf, &b)?;
        ret(0)
    }

    fn sys_utimensat(&mut self, t: usize, dirfd: u64, p: u64, times: u64, flags: u64) -> R {
        const UTIME_NOW: u64 = 0x3fff_ffff;
        const UTIME_OMIT: u64 = 0x3fff_fffe;
        let path = if p == 0 {
            let f = self.tasks[t].files.borrow().get(dirfd as i64)?;
            f.borrow().guest_path.clone()
        } else {
            self.path_arg(t, dirfd, p)?
        };
        let mm = self.mem(t);
        let mut ts = [libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW }; 2];
        if times != 0 {
            for (i, slot) in ts.iter_mut().enumerate() {
                let off = times + 16 * i as u64;
                let s = read_u64(&mut mm.borrow_mut().mem, off)?;
                let n = read_u64(&mut mm.borrow_mut().mem, off + 8)?;
                slot.tv_sec = s as libc::time_t;
                slot.tv_nsec = match n {
                    UTIME_NOW => libc::UTIME_NOW,
                    UTIME_OMIT => libc::UTIME_OMIT,
                    n if n < 1_000_000_000 => n as _,
                    _ => return Err(EINVAL),
                };
            }
        }
        let cpath = std::ffi::CString::new(path).map_err(|_| EINVAL)?;
        let hflags = if flags & AT_SYMLINK_NOFOLLOW != 0 { libc::AT_SYMLINK_NOFOLLOW } else { 0 };
        // SAFETY: percorso C valido e array di due timespec, come vuole utimensat(2).
        let r = unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), ts.as_ptr(), hflags) };
        if r != 0 {
            return Err(host_errno(&std::io::Error::last_os_error()));
        }
        ret(0)
    }

    fn sys_readlinkat(&mut self, t: usize, dirfd: u64, p: u64, buf: u64, len: usize) -> R {
        let raw = read_cstr(&mut self.mem(t).borrow_mut().mem, p)?;
        if raw.is_empty() {
            return Err(ENOENT);
        }
        let unresolved = self.at_path_raw(t, dirfd, &raw)?;
        let target = if let Some(f) = self.proc_fd_target(t, &unresolved) {
            f.into_bytes()
        } else if unresolved == "/proc/self/exe" {
            self.tasks[t].exe.clone().into_bytes()
        } else {
            use std::os::unix::ffi::OsStrExt;
            std::fs::read_link(&unresolved).map_err(|e| host_errno(&e))?.as_os_str().as_bytes().to_vec()
        };
        let n = target.len().min(len);
        write_bytes(&mut self.mem(t).borrow_mut().mem, buf, &target[..n])?;
        ret(n as i64)
    }

    fn sys_mmap(&mut self, t: usize, a: [u64; 6]) -> R {
        let (addr, len, prot, flags, fd, off) = (a[0], a[1], a[2], a[3], a[4] as i64, a[5]);
        if off & 0xfff != 0 {
            return Err(EINVAL);
        }
        let anon = flags & super::mm::MAP_ANONYMOUS != 0;
        // Ordine dei controlli di Linux: il descrittore (ksys_mmap_pgoff),
        // poi in do_mmap lunghezza, tipo e modo d'apertura del file.
        if !anon {
            self.tasks[t].files.borrow().get(fd)?;
        }
        if len == 0 {
            return Err(EINVAL);
        }
        // Tipo: MAP_SHARED (1), MAP_PRIVATE (2) o MAP_SHARED_VALIDATE (3).
        let ty = flags & 0xf;
        if !(1..=3).contains(&ty) {
            return Err(EINVAL);
        }
        let shared_flag = ty != 2;
        // LEGACY_MAP_MASK di Linux: con MAP_SHARED_VALIDATE su un file ogni
        // altro flag è EOPNOTSUPP.
        const LEGACY_MAP_MASK: u64 = 0xf
            | 0x10
            | 0x20
            | 0x100
            | 0x800
            | 0x1000
            | 0x2000
            | 0x4000
            | 0x8000
            | 0x1_0000
            | 0x2_0000
            | 0x4_0000
            | 0x400_0000
            | (0x3f << 26);
        if ty == 3 && !anon && flags & !LEGACY_MAP_MASK != 0 {
            return Err(95); // EOPNOTSUPP
        }
        let mut data = None;
        let mut shared = None;
        if anon {
            if shared_flag {
                if len == 0 || len > super::mm::MAX_MAPPING {
                    return Err(if len == 0 { EINVAL } else { ENOMEM });
                }
                let size = len.next_multiple_of(4096) as usize;
                shared = Some((Rc::new(RefCell::new(vec![0u8; size])), 0, true));
            }
        } else {
            use std::os::unix::fs::{FileExt, MetadataExt};
            let f = self.tasks[t].files.borrow().get(fd)?;
            let f = f.borrow();
            // Come Linux: il file deve essere leggibile; una MAP_SHARED di un
            // file aperto senza scrittura non può mai diventare scrivibile.
            let fd_write = f.flags & O_ACCMODE != 0;
            if f.flags & O_ACCMODE == O_WRONLY || shared_flag && prot & 2 != 0 && !fd_write {
                return Err(EACCES);
            }
            match &f.kind {
                Kind::Host { file, path } => {
                    if shared_flag {
                        let meta = file.metadata().map_err(|e| host_errno(&e))?;
                        let key = (meta.dev(), meta.ino());
                        let entry = self.shared_files.entry(key).or_insert_with(|| {
                            let mut content = Vec::new();
                            let _ = std::fs::File::open(path).and_then(|mut h| {
                                use std::io::Read;
                                h.read_to_end(&mut content)
                            });
                            (path.clone(), Rc::new(RefCell::new(content)))
                        });
                        shared = Some((entry.1.clone(), off as usize, fd_write));
                    } else {
                        let mut d = vec![0u8; len as usize];
                        let n = file.read_at(&mut d, off).map_err(|e| host_errno(&e))?;
                        d.truncate(n);
                        data = Some(d);
                    }
                }
                // MAP_SHARED di /dev/zero: memoria anonima condivisa.
                Kind::Zero if shared_flag => {
                    let size = len.min(super::mm::MAX_MAPPING).next_multiple_of(4096) as usize;
                    shared = Some((Rc::new(RefCell::new(vec![0u8; size])), 0, fd_write));
                }
                Kind::Zero => {}
                _ => return Err(19), // ENODEV
            }
        }
        ret(self.mem(t).borrow_mut().mmap(addr, len, prot, flags, data, shared)?)
    }

    /// futex_waitv(waiters, nr, flags, timeout, clockid): come
    /// kernel/futex/syscalls.c; restituisce l'indice del futex svegliato.
    fn sys_futex_waitv(&mut self, t: usize, a: [u64; 6]) -> R {
        const FUTEX2_SIZE_U32: u32 = 2;
        const FUTEX2_PRIVATE: u32 = 128;
        if self.tasks[t].futex_woken {
            self.tasks[t].futex_woken = false;
            return ret(self.tasks[t].futex_index as i64);
        }
        if let Some(d) = self.tasks[t].deadline
            && self.now() >= d
        {
            return Err(ETIMEDOUT);
        }
        let (waiters, nr, flags, timeout, clockid) = (a[0], a[1], a[2], a[3], a[4]);
        if flags != 0 || nr == 0 || nr > 128 || waiters == 0 {
            return Err(EINVAL);
        }
        let mm = self.mem(t);
        if timeout != 0 && self.tasks[t].deadline.is_none() {
            if clockid > 1 {
                return Err(EINVAL);
            }
            let s = read_u64(&mut mm.borrow_mut().mem, timeout)?;
            let n = read_u64(&mut mm.borrow_mut().mem, timeout + 8)?;
            let d = timespec_ns(s, n)?;
            // Scadenza assoluta sull'orologio indicato.
            let until = if clockid == 0 { d.saturating_sub(self.realtime() - self.now()) } else { d };
            self.tasks[t].deadline = Some(until);
        }
        let raw = read_bytes(&mut mm.borrow_mut().mem, waiters, 24 * nr as usize)?;
        let mut keys = Vec::with_capacity(nr as usize);
        let mut vals = Vec::with_capacity(nr as usize);
        for w in raw.chunks(24) {
            let val = u64::from_le_bytes(w[0..8].try_into().unwrap());
            let uaddr = u64::from_le_bytes(w[8..16].try_into().unwrap());
            let fl = u32::from_le_bytes(w[16..20].try_into().unwrap());
            let reserved = u32::from_le_bytes(w[20..24].try_into().unwrap());
            if reserved != 0
                || fl & !(3 | FUTEX2_PRIVATE) != 0
                || fl & 3 != FUTEX2_SIZE_U32
                || uaddr % 4 != 0
                || val > u32::MAX as u64
            {
                self.tasks[t].deadline = None;
                return Err(EINVAL);
            }
            keys.push(self.futex_key(&mm, uaddr, fl & FUTEX2_PRIVATE != 0));
            vals.push((uaddr, val as u32));
        }
        for (uaddr, val) in vals {
            if read_u32(&mut mm.borrow_mut().mem, uaddr)? != val {
                self.tasks[t].deadline = None;
                return Err(EAGAIN);
            }
        }
        Ok(Sys::Block(Wait::FutexV { keys, until: self.tasks[t].deadline }))
    }

    fn sys_futex(&mut self, t: usize, a: [u64; 6]) -> R {
        const FUTEX_WAIT: u64 = 0;
        const FUTEX_WAKE: u64 = 1;
        const FUTEX_REQUEUE: u64 = 3;
        const FUTEX_CMP_REQUEUE: u64 = 4;
        const FUTEX_WAKE_OP: u64 = 5;
        const FUTEX_WAIT_BITSET: u64 = 9;
        const FUTEX_WAKE_BITSET: u64 = 10;
        let (uaddr, op, val) = (a[0], a[1] & 0x7f, a[2]);
        let mm = self.mem(t);
        let private = a[1] & 128 != 0; // FUTEX_PRIVATE_FLAG
        let key = self.futex_key(&mm, uaddr, private);
        match op {
            FUTEX_WAIT | FUTEX_WAIT_BITSET => {
                if self.tasks[t].futex_woken {
                    self.tasks[t].futex_woken = false;
                    return ret(0);
                }
                if let Some(d) = self.tasks[t].deadline
                    && self.now() >= d
                {
                    return Err(ETIMEDOUT);
                }
                let cur = read_u32(&mut mm.borrow_mut().mem, uaddr)?;
                if cur != val as u32 {
                    return Err(EAGAIN);
                }
                if a[3] != 0 && self.tasks[t].deadline.is_none() {
                    let s = read_u64(&mut mm.borrow_mut().mem, a[3])?;
                    let n = read_u64(&mut mm.borrow_mut().mem, a[3] + 8)?;
                    let d = timespec_ns(s, n)?;
                    // FUTEX_WAIT: relativo; WAIT_BITSET: assoluto (monotono o realtime)
                    let until = if op == FUTEX_WAIT {
                        self.now().saturating_add(d)
                    } else if a[1] & 256 != 0 {
                        d.saturating_sub(self.realtime() - self.now())
                    } else {
                        d
                    };
                    self.tasks[t].deadline = Some(until);
                }
                Ok(Sys::Block(Wait::Futex { key, until: self.tasks[t].deadline }))
            }
            FUTEX_WAKE | FUTEX_WAKE_BITSET => ret(self.futex_wake(key, val as usize) as i64),
            FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
                if (val as i32) < 0 || (a[3] as i32) < 0 {
                    return Err(EINVAL);
                }
                if op == FUTEX_CMP_REQUEUE && read_u32(&mut mm.borrow_mut().mem, uaddr)? != a[5] as u32 {
                    return Err(EAGAIN);
                }
                let woken = self.futex_wake(key, val as usize);
                let key2 = self.futex_key(&mm, a[4], private);
                // Sposta gli altri in attesa sul secondo indirizzo.
                let mut moved = 0;
                for task in self.tasks.iter_mut() {
                    if moved >= a[3] as usize {
                        break;
                    }
                    if let State::Blocked(Wait::Futex { key: k, until }) = task.state
                        && k == key
                        && !task.futex_woken
                    {
                        task.state = State::Blocked(Wait::Futex { key: key2, until });
                        moved += 1;
                    }
                }
                ret((woken + moved) as i64)
            }
            FUTEX_WAKE_OP => {
                let (uaddr2, encoded) = (a[4], a[5] as u32);
                let oparg = ((encoded << 8) as i32 >> 20) as u32;
                let cmparg = ((encoded << 20) as i32 >> 20) as u32;
                let old = read_u32(&mut mm.borrow_mut().mem, uaddr2)?;
                let arg = if encoded & (8 << 28) != 0 { 1u32 << (oparg & 31) } else { oparg };
                let new = match (encoded >> 28) & 7 {
                    0 => arg,
                    1 => old.wrapping_add(arg),
                    2 => old | arg,
                    3 => old & !arg,
                    _ => old ^ arg,
                };
                write_u32(&mut mm.borrow_mut().mem, uaddr2, new)?;
                let mut n = self.futex_wake(key, val as usize);
                let cond = match (encoded >> 24) & 15 {
                    0 => old == cmparg,
                    1 => old != cmparg,
                    2 => (old as i32) < cmparg as i32,
                    3 => old as i32 <= cmparg as i32,
                    4 => old as i32 > cmparg as i32,
                    _ => old as i32 >= cmparg as i32,
                };
                if cond {
                    let key2 = self.futex_key(&mm, uaddr2, private);
                    n += self.futex_wake(key2, a[3] as usize);
                }
                ret(n as i64)
            }
            _ => Err(ENOSYS),
        }
    }

    /// capget/capset. Come QEMU user mode le capability sono quelle del
    /// processo host; capset non cambia nulla (può solo toglierne).
    fn sys_capability(&mut self, t: usize, set: bool, hdr: u64, data: u64) -> R {
        const V1: u32 = 0x1998_0330;
        const V2: u32 = 0x2007_1026;
        const V3: u32 = 0x2008_0522;
        let h = read_bytes(&mut self.mem(t).borrow_mut().mem, hdr, 8)?;
        let version = u32::from_le_bytes(h[0..4].try_into().unwrap());
        let pid = i32::from_le_bytes(h[4..8].try_into().unwrap());
        let words = match version {
            V1 => 1,
            V2 | V3 => 2,
            _ => {
                write_bytes(&mut self.mem(t).borrow_mut().mem, hdr, &V3.to_le_bytes())?;
                return if !set && data == 0 { ret(0) } else { Err(EINVAL) };
            }
        };
        if set {
            // Solo il thread chiamante (task_pid_vnr(current), cioè il tid).
            if pid != 0 && pid != self.tasks[t].tid {
                return Err(EPERM);
            }
            let d = read_bytes(&mut self.mem(t).borrow_mut().mem, data, 12 * words)?;
            let (_, perm, inh) = host_caps();
            for w in 0..words {
                let v =
                    |i: usize| u32::from_le_bytes(d[w * 12 + i * 4..w * 12 + i * 4 + 4].try_into().unwrap());
                let (e, p, i) = (v(0), v(1), v(2));
                // Non si possono aggiungere capability, né avere effettive non permesse.
                if p & !perm[w] != 0 || i & !(inh[w] | perm[w]) != 0 || e & !p != 0 {
                    return Err(EPERM);
                }
            }
            return ret(0);
        }
        if data == 0 {
            return ret(0);
        }
        if pid < 0 {
            return Err(EINVAL);
        }
        if pid != 0 && self.find(pid).is_none() {
            return Err(ESRCH);
        }
        let (eff, perm, inh) = host_caps();
        let mut b = Vec::new();
        for w in 0..words {
            for v in [eff[w], perm[w], inh[w]] {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        write_bytes(&mut self.mem(t).borrow_mut().mem, data, &b)?;
        ret(0)
    }

    fn sys_prctl(&mut self, t: usize, a: [u64; 6]) -> R {
        const PR_SET_NAME: u64 = 15;
        const PR_GET_NAME: u64 = 16;
        match a[0] {
            PR_SET_NAME => {
                let n = read_cstr(&mut self.mem(t).borrow_mut().mem, a[1])?;
                self.tasks[t].comm = String::from_utf8_lossy(&n[..n.len().min(15)]).into_owned();
                ret(0)
            }
            PR_GET_NAME => {
                let mut b = self.tasks[t].comm.clone().into_bytes();
                b.resize(16, 0);
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[1], &b)?;
                ret(0)
            }
            _ => ret(0),
        }
    }

    fn sys_sigaltstack(&mut self, t: usize, ss: u64, old: u64) -> R {
        let mm = self.mem(t);
        let (sp, size, flags) = self.tasks[t].sig.altstack;
        if old != 0 {
            let cur = self.tasks[t].cpu.sp;
            let on = size != 0 && cur > sp && cur <= sp + size;
            let f = if size == 0 {
                2
            } else if on {
                1
            } else {
                flags
            };
            write_u64(&mut mm.borrow_mut().mem, old, sp)?;
            write_u32(&mut mm.borrow_mut().mem, old + 8, f)?;
            write_u32(&mut mm.borrow_mut().mem, old + 12, 0)?;
            write_u64(&mut mm.borrow_mut().mem, old + 16, size)?;
        }
        if ss != 0 {
            let nsp = read_u64(&mut mm.borrow_mut().mem, ss)?;
            let nflags = read_u32(&mut mm.borrow_mut().mem, ss + 8)?;
            let nsize = read_u64(&mut mm.borrow_mut().mem, ss + 16)?;
            if nflags & 2 != 0 {
                self.tasks[t].sig.altstack = (0, 0, 0);
            } else {
                if nsize < 2048 {
                    return Err(ENOMEM);
                }
                self.tasks[t].sig.altstack = (nsp, nsize, nflags);
            }
        }
        ret(0)
    }

    fn sys_kill(&mut self, t: usize, pid: i32, s: i32) -> R {
        if !(0..=64).contains(&s) {
            return Err(EINVAL);
        }
        let from = self.tasks[t].tgid;
        let targets: Vec<i32> = match pid {
            p if p > 0 => vec![p],
            0 => {
                let pg = self.tasks[t].pgid;
                self.processes()
                    .into_iter()
                    .filter(|&p| self.find(p).is_some_and(|i| self.tasks[i].pgid == pg))
                    .collect()
            }
            -1 => self.processes().into_iter().filter(|&p| p != from).collect(),
            p => self
                .processes()
                .into_iter()
                .filter(|&q| self.find(q).is_some_and(|i| self.tasks[i].pgid == -p))
                .collect(),
        };
        let mut any = false;
        for p in targets {
            if s == 0 {
                any |= self.find(p).is_some_and(|i| {
                    matches!(self.tasks[i].state, State::Runnable | State::Blocked(_) | State::Zombie { .. })
                });
            } else {
                any |= self.send_to_process(p, s, from, 0);
            }
        }
        if any { ret(0) } else { Err(ESRCH) }
    }

    fn processes(&self) -> Vec<i32> {
        self.tasks
            .iter()
            .filter(|x| x.tid == x.tgid && matches!(x.state, State::Runnable | State::Blocked(_)))
            .map(|x| x.tgid)
            .collect()
    }

    fn sys_itimer(&mut self, t: usize, nr: u64, which: u64, new: u64, old: u64) -> R {
        if which != 0 {
            return Err(EINVAL); // solo ITIMER_REAL
        }
        let mm = self.mem(t);
        let tgid = self.tasks[t].tgid;
        let leader = self.find(tgid).unwrap_or(t);
        let now = self.now();
        let (cur, iv) = (self.tasks[leader].sig.alarm, self.tasks[leader].sig.alarm_interval);
        let put_tv = |mem: &mut UserMemory, at: u64, ns: u64| -> Result<(), i64> {
            write_u64(mem, at, ns / 1_000_000_000)?;
            write_u64(mem, at + 8, ns % 1_000_000_000 / 1000)
        };
        let target = if nr == 102 { new } else { old };
        if target != 0 {
            let remaining = cur.map_or(0, |d| d.saturating_sub(now));
            put_tv(&mut mm.borrow_mut().mem, target, iv)?;
            put_tv(&mut mm.borrow_mut().mem, target + 16, remaining)?;
        }
        if nr == 103 && new != 0 {
            let mut m = mm.borrow_mut();
            let is = read_u64(&mut m.mem, new)?;
            let iu = read_u64(&mut m.mem, new + 8)?;
            let vs = read_u64(&mut m.mem, new + 16)?;
            let vu = read_u64(&mut m.mem, new + 24)?;
            let value = timeval_ns(vs, vu)?;
            let interval = timeval_ns(is, iu)?;
            drop(m);
            let s = &mut self.tasks[leader].sig;
            s.alarm_interval = interval;
            let at = now.saturating_add(value);
            s.alarm = if value == 0 { None } else { Some(at) };
            self.next_alarm = self.next_alarm.min(at);
        }
        let _ = bit;
        ret(0)
    }
}

/// Capability (effettive, permesse, ereditabili) del processo host, in due
/// parole da 32 bit come `capget` versione 3.
fn host_caps() -> ([u32; 2], [u32; 2], [u32; 2]) {
    #[cfg(target_os = "linux")]
    {
        let mut hdr = [0x2008_0522u32, 0];
        let mut d = [0u32; 6];
        // SAFETY: header e dati sono buffer validi delle dimensioni attese.
        let r = unsafe { libc::syscall(libc::SYS_capget, hdr.as_mut_ptr(), d.as_mut_ptr()) };
        if r == 0 {
            return ([d[0], d[3]], [d[1], d[4]], [d[2], d[5]]);
        }
    }
    // SAFETY: geteuid non ha precondizioni.
    if unsafe { libc::geteuid() } == 0 {
        let all = [u32::MAX, 0x1ff];
        (all, all, [0, 0])
    } else {
        ([0, 0], [0, 0], [0, 0])
    }
}

/// La versione del kernel con UNAME26 (override_release di Linux): "2.6.",
/// il numero minore più 60, e ciò che segue i primi tre numeri. "6.18.53" →
/// "2.6.78", "6.12.5-linuxkit" → "2.6.72-linuxkit".
fn uname26(release: &str) -> String {
    let minor: u32 = release
        .split('.')
        .nth(1)
        .and_then(|m| m.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok())
        .unwrap_or(0);
    let mut dots = 0;
    let mut rest = release.len();
    for (i, c) in release.char_indices() {
        if c == '.' {
            dots += 1;
            if dots >= 3 {
                rest = i;
                break;
            }
        } else if !c.is_ascii_digit() {
            rest = i;
            break;
        }
    }
    format!("2.6.{}{}", minor + 60, &release[rest..])
}

#[cfg(test)]
mod tests {
    #[test]
    fn uname26_come_override_release() {
        assert_eq!(super::uname26("6.18.53"), "2.6.78");
        assert_eq!(super::uname26("6.12.5-linuxkit"), "2.6.72-linuxkit");
        assert_eq!(super::uname26("6.6.0-vetro"), "2.6.66-vetro");
        assert_eq!(super::uname26("6.8.0.1-x"), "2.6.68.1-x");
    }
}
