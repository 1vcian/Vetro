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
        let res = self.dispatch(t, nr, a);
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

    /// Percorso assoluto del guest per una *at: `path` rispetto a `dirfd`.
    fn at_path(&self, t: usize, dirfd: u64, path: &[u8]) -> Result<String, i64> {
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
        if !matches!(f.kind, Kind::Dir { .. }) {
            return Err(ENOTDIR);
        }
        Ok(fs::join(&f.guest_path, path))
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
                *self.tasks[t].cwd.borrow_mut() = p;
                ret(0)
            }
            50 => {
                let f = self.tasks[t].files.borrow().get(a[0] as i64)?;
                let f = f.borrow();
                if !matches!(f.kind, Kind::Dir { .. }) {
                    return Err(ENOTDIR);
                }
                *self.tasks[t].cwd.borrow_mut() = f.guest_path.clone();
                ret(0)
            }
            34 => {
                let p = self.path_arg(t, a[0], a[1])?;
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .mode(a[2] as u32 & !self.tasks[t].umask & 0o7777)
                    .create(&p)
                    .map_err(|e| host_errno(&e))?;
                ret(0)
            }
            35 => {
                let p = self.path_arg(t, a[0], a[1])?;
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
                let link = self.path_arg(t, a[1], a[2])?;
                std::os::unix::fs::symlink(String::from_utf8_lossy(&target).as_ref(), &link)
                    .map_err(|e| host_errno(&e))?;
                ret(0)
            }
            37 => {
                let old = self.path_arg(t, a[0], a[1])?;
                let new = self.path_arg(t, a[2], a[3])?;
                std::fs::hard_link(&old, &new).map_err(|e| host_errno(&e))?;
                ret(0)
            }
            38 | 276 => {
                let old = self.path_arg(t, a[0], a[1])?;
                let new = self.path_arg(t, a[2], a[3])?;
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
                let p = self.path_arg(t, a[0], a[1])?;
                let mode = (a[2] as u32 & !self.tasks[t].umask) as libc::mode_t;
                let cpath = std::ffi::CString::new(p).map_err(|_| EINVAL)?;
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
                if a[0] == a[1] {
                    return Err(EINVAL);
                }
                let f = self.tasks[t].files.borrow().get(a[0] as i64)?;
                ret(self.tasks[t].files.borrow_mut().install_at(a[1] as usize, f, a[2] & O_CLOEXEC != 0)?)
            }
            25 => self.sys_fcntl(t, a[0] as i64, a[1], a[2]),
            29 => {
                self.tasks[t].files.borrow().get(a[0] as i64)?;
                Err(ENOTTY) // nessun terminale: stdout è una pipe o un file
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
            73 => self.sys_ppoll(t, a[0], a[1] as usize, a[2]),
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
            124 => ret(0),
            123 => {
                // sched_getaffinity: una sola CPU
                let n = (a[1] as usize).min(8);
                let mut b = vec![0u8; n];
                if n > 0 {
                    b[0] = 1;
                }
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[2], &b)?;
                ret(n as i64)
            }

            // --- segnali ---
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
                ret(0)
            }
            101 | 115 => {
                let (ts, abs) = if nr == 101 { (a[0], false) } else { (a[2], a[1] & 1 != 0) };
                if self.tasks[t].deadline.is_none() {
                    let mm = self.mem(t);
                    let s = read_u64(&mut mm.borrow_mut().mem, ts)? as i64;
                    let n = read_u64(&mut mm.borrow_mut().mem, ts + 8)? as i64;
                    if s < 0 || !(0..1_000_000_000).contains(&n) {
                        return Err(EINVAL);
                    }
                    let d = s as u64 * 1_000_000_000 + n as u64;
                    let realtime = nr == 115 && a[0] == 0;
                    let until = if abs {
                        if realtime { d.saturating_sub(self.realtime() - self.now()) } else { d }
                    } else {
                        self.now() + d
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
                for (i, s) in
                    ["Linux", "vetro", "6.6.0-vetro", "#1 SMP", "aarch64", "(none)"].iter().enumerate()
                {
                    b[i * 65..i * 65 + s.len()].copy_from_slice(s.as_bytes());
                }
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[0], &b)?;
                ret(0)
            }
            163 | 164 | 261 => {
                let (res, old) = match nr {
                    163 => (a[0], a[1]),
                    164 => return ret(0),
                    _ => (a[1], a[3]),
                };
                if old != 0 {
                    let (cur, max) = match res {
                        3 => (8 << 20, u64::MAX), // RLIMIT_STACK
                        4 => (0, u64::MAX),       // RLIMIT_CORE: niente core dump
                        7 => (1024, 4096),        // RLIMIT_NOFILE
                        _ => (u64::MAX, u64::MAX),
                    };
                    let mm = self.mem(t);
                    write_u64(&mut mm.borrow_mut().mem, old, cur)?;
                    write_u64(&mut mm.borrow_mut().mem, old + 8, max)?;
                }
                ret(0)
            }
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
                let n = (a[1] as usize).min(1 << 20);
                let bytes = self.random_bytes(n);
                write_bytes(&mut self.mem(t).borrow_mut().mem, a[0], &bytes)?;
                ret(n as i64)
            }
            _ => Ok(Sys::Unsupported),
        }
    }

    fn sys_openat(&mut self, t: usize, dirfd: u64, p: u64, flags: u64, mode: u32) -> R {
        let path = self.path_arg(t, dirfd, p)?;
        let file = match self.proc_file(t, &path) {
            Some(r) => r?,
            None => fs::open(&path, flags, mode, self.tasks[t].umask)?,
        };
        ret(self.tasks[t].files.borrow_mut().install(file, flags & O_CLOEXEC != 0, 0)?)
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
        let io = f.borrow_mut().write(data);
        match io {
            Io::Written(n) => ret(n as i64),
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
        for i in 0..cnt as u64 {
            let base = read_u64(&mut mm.borrow_mut().mem, iov + 16 * i)?;
            let len = read_u64(&mut mm.borrow_mut().mem, iov + 16 * i + 8)?;
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
        use std::os::unix::fs::FileExt;
        if off < 0 {
            return Err(EINVAL);
        }
        let f = self.tasks[t].files.borrow().get(fd)?;
        let f = f.borrow();
        let Kind::Host { file, .. } = &f.kind else { return Err(ESPIPE) };
        if write {
            let data = read_bytes(&mut self.mem(t).borrow_mut().mem, buf, len)?;
            ret(file.write_at(&data, off as u64).map_err(|e| host_errno(&e))? as i64)
        } else {
            let mut data = vec![0u8; len.min(1 << 24)];
            let n = file.read_at(&mut data, off as u64).map_err(|e| host_errno(&e))?;
            write_bytes(&mut self.mem(t).borrow_mut().mem, buf, &data[..n])?;
            ret(n as i64)
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

    fn sys_ppoll(&mut self, t: usize, fds: u64, n: usize, timeout: u64) -> R {
        let mm = self.mem(t);
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
                            Kind::PipeR(_) | Kind::PipeW(_) => {
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
        if ready == 0 && n > 0 {
            if timeout != 0 {
                let s = read_u64(&mut mm.borrow_mut().mem, timeout)?;
                let ns = read_u64(&mut mm.borrow_mut().mem, timeout + 8)?;
                if s == 0 && ns == 0 {
                    return ret(0);
                }
            }
            return Ok(Sys::Block(Wait::Pipe));
        }
        ret(ready)
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
                let mut f = f.borrow_mut();
                f.flags = (f.flags & O_ACCMODE) | (arg & (O_APPEND | O_NONBLOCK));
                ret(0)
            }
            5..=7 => {
                drop(files);
                match self.fcntl_lock(t, fd, cmd, arg)? {
                    super::locks::LockResult::Done(v) => ret(v),
                    super::locks::LockResult::Wait => Ok(Sys::Block(Wait::Retry)),
                }
            }
            _ => Err(EINVAL),
        }
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
        let path = self.at_path(t, dirfd, &raw)?;
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
        let path = self.path_arg(t, dirfd, p)?;
        let target = match path.as_str() {
            "/proc/self/exe" => self.tasks[t].exe.clone().into_bytes(),
            _ => {
                use std::os::unix::ffi::OsStrExt;
                std::fs::read_link(&path).map_err(|e| host_errno(&e))?.as_os_str().as_bytes().to_vec()
            }
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
        let shared_flag = flags & super::mm::MAP_SHARED != 0;
        let mut data = None;
        let mut shared = None;
        if anon {
            if shared_flag {
                if len == 0 || len > super::mm::MAX_MAPPING {
                    return Err(if len == 0 { EINVAL } else { ENOMEM });
                }
                let size = len.next_multiple_of(4096) as usize;
                shared = Some((Rc::new(RefCell::new(vec![0u8; size])), 0));
            }
        } else {
            use std::os::unix::fs::{FileExt, MetadataExt};
            let f = self.tasks[t].files.borrow().get(fd)?;
            let f = f.borrow();
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
                        shared = Some((entry.1.clone(), off as usize));
                    } else {
                        let mut d = vec![0u8; len as usize];
                        let n = file.read_at(&mut d, off).map_err(|e| host_errno(&e))?;
                        d.truncate(n);
                        data = Some(d);
                    }
                }
                Kind::Zero => {}
                _ => return Err(19), // ENODEV
            }
        }
        ret(self.mem(t).borrow_mut().mmap(addr, len, prot, flags, data, shared)?)
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
                    let d = s * 1_000_000_000 + n;
                    // FUTEX_WAIT: relativo; WAIT_BITSET: assoluto (monotono o realtime)
                    let until = if op == FUTEX_WAIT {
                        self.now() + d
                    } else if a[1] & 256 != 0 {
                        d.saturating_sub(self.realtime() - self.now())
                    } else {
                        d
                    };
                    self.tasks[t].deadline = Some(until);
                }
                Ok(Sys::Block(Wait::Futex { addr: uaddr, until: self.tasks[t].deadline }))
            }
            FUTEX_WAKE | FUTEX_WAKE_BITSET => ret(self.futex_wake(&mm, uaddr, val as usize) as i64),
            FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
                if op == FUTEX_CMP_REQUEUE && read_u32(&mut mm.borrow_mut().mem, uaddr)? != a[5] as u32 {
                    return Err(EAGAIN);
                }
                let woken = self.futex_wake(&mm, uaddr, val as usize);
                // Sposta gli altri in attesa sul secondo indirizzo.
                let mut moved = 0;
                for task in self.tasks.iter_mut() {
                    if moved >= a[3] as usize {
                        break;
                    }
                    if let State::Blocked(Wait::Futex { addr, until }) = task.state
                        && addr == uaddr
                        && Rc::ptr_eq(&task.mm, &mm)
                        && !task.futex_woken
                    {
                        task.state = State::Blocked(Wait::Futex { addr: a[4], until });
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
                let mut n = self.futex_wake(&mm, uaddr, val as usize);
                let cond = match (encoded >> 24) & 15 {
                    0 => old == cmparg,
                    1 => old != cmparg,
                    2 => (old as i32) < cmparg as i32,
                    3 => old as i32 <= cmparg as i32,
                    4 => old as i32 > cmparg as i32,
                    _ => old as i32 >= cmparg as i32,
                };
                if cond {
                    n += self.futex_wake(&mm, uaddr2, a[3] as usize);
                }
                ret(n as i64)
            }
            _ => Err(ENOSYS),
        }
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
            let value = vs * 1_000_000_000 + vu * 1000;
            let s = &mut self.tasks[leader].sig;
            s.alarm_interval = is * 1_000_000_000 + iu * 1000;
            s.alarm = if value == 0 { None } else { Some(now + value) };
        }
        let _ = bit;
        ret(0)
    }
}
