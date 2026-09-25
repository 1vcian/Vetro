//! File descriptor, file aperti, pipe e console.
//!
//! I percorsi del guest vanno sul file system dell'host così come sono (come
//! fa QEMU user mode), tranne pochi file speciali emulati: /dev/null,
//! /dev/zero, /dev/urandom (deterministico) e /proc/self/exe.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::rc::Rc;

use super::abi::*;

/// stdin, stdout e stderr del processo iniziale.
pub struct Console {
    stdin: VecDeque<u8>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    echo: bool,
}

impl Console {
    pub fn new(stdin: Vec<u8>, echo: bool) -> Self {
        Console { stdin: stdin.into(), stdout: Vec::new(), stderr: Vec::new(), echo }
    }
}

/// Capacità di una pipe, come su Linux.
const PIPE_CAP: usize = 65536;

#[derive(Default)]
pub struct Pipe {
    buf: VecDeque<u8>,
    readers: usize,
    writers: usize,
    /// Capi di lettura con O_ASYNC: a ogni scrittura il loro proprietario
    /// riceve il segnale di I/O.
    pub async_readers: Vec<std::rc::Weak<RefCell<OpenFile>>>,
    /// Capacità scelta con F_SETPIPE_SZ; 0 = quella di default.
    size: usize,
}

impl Pipe {
    /// Byte in attesa di essere letti (FIONREAD).
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Capacità in byte (F_GETPIPE_SZ).
    pub fn capacity(&self) -> usize {
        if self.size == 0 { PIPE_CAP } else { self.size }
    }

    /// F_SETPIPE_SZ, come pipe_set_size: potenza di due di pagine, al più
    /// /proc/sys/fs/pipe-max-size senza privilegi, non sotto i dati presenti.
    pub fn set_capacity(&mut self, arg: u64, privileged: bool) -> Result<usize, i64> {
        const PIPE_MAX_SIZE: usize = 1 << 20;
        if arg > 1 << 31 {
            return Err(EINVAL);
        }
        let size = (arg as usize).div_ceil(4096).max(1).next_power_of_two() * 4096;
        if size > PIPE_MAX_SIZE && !privileged {
            return Err(EPERM);
        }
        if self.buf.len() > size {
            return Err(16); // EBUSY
        }
        self.size = size;
        Ok(size)
    }

    pub fn readers(&self) -> usize {
        self.readers
    }
    pub fn writers(&self) -> usize {
        self.writers
    }
}

pub struct DirEnt {
    pub ino: u64,
    pub dtype: u8,
    pub name: Vec<u8>,
}

pub enum Kind {
    Console(Rc<RefCell<Console>>, u8),
    Host {
        file: std::fs::File,
        path: PathBuf,
    },
    Dir {
        path: PathBuf,
        entries: Option<Vec<DirEnt>>,
        pos: usize,
    },
    PipeR(Rc<RefCell<Pipe>>),
    PipeW(Rc<RefCell<Pipe>>),
    /// FIFO aperta in lettura e scrittura (O_RDWR).
    PipeRW(Rc<RefCell<Pipe>>),
    Null,
    Zero,
    Random,
    /// Socket non connesso (AF_UNIX/AF_INET): la rete arriva con M7.
    Socket,
    /// Aperto con O_PATH: solo un riferimento al percorso (fstat, fchdir,
    /// dirfd delle *at); l'I/O fallisce con EBADF.
    Path {
        path: PathBuf,
        dir: bool,
    },
    /// /proc/<pid>/pagemap: 8 byte per pagina virtuale, letti dallo spazio
    /// d'indirizzamento del processo.
    Pagemap {
        mm: Rc<RefCell<super::mm::Mm>>,
        pos: u64,
    },
    /// File generato in memoria (procfs).
    Mem {
        data: Vec<u8>,
        pos: usize,
    },
}

pub struct OpenFile {
    pub kind: Kind,
    /// Flag di stato (modo d'accesso, O_APPEND, O_NONBLOCK).
    pub flags: u64,
    /// Percorso del guest, per fchdir e /proc/self/fd.
    pub guest_path: String,
    /// Destinatario dei segnali di I/O (F_SETOWN_EX): (tipo F_OWNER_*, id).
    pub owner: (i32, i32),
    /// Segnale di I/O (F_SETSIG); 0 = SIGIO.
    pub sigio: i32,
    /// Lease (F_SETLEASE): F_RDLCK, F_WRLCK o F_UNLCK.
    pub lease: i16,
    /// Creato da questa open (F_CREATED_QUERY).
    pub created: bool,
    /// Proprietario dei lock OFD (F_OFD_SETLK) di questa descrizione; 0 =
    /// ancora nessuno.
    pub ofd_owner: i32,
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        match &self.kind {
            Kind::PipeR(p) => p.borrow_mut().readers -= 1,
            Kind::PipeW(p) => p.borrow_mut().writers -= 1,
            Kind::PipeRW(p) => {
                let mut p = p.borrow_mut();
                p.readers -= 1;
                p.writers -= 1;
            }
            _ => {}
        }
    }
}

/// Esito di un'operazione su un file che può dover aspettare.
pub enum Io {
    Done(Vec<u8>),
    Written(usize),
    /// La pipe è vuota (o piena): il task deve bloccarsi.
    Block,
    Err(i64),
    /// Scrittura su una pipe senza lettori: EPIPE e SIGPIPE.
    BrokenPipe,
}

impl OpenFile {
    pub fn new(kind: Kind, flags: u64, guest_path: String) -> Rc<RefCell<OpenFile>> {
        match &kind {
            Kind::PipeR(p) => p.borrow_mut().readers += 1,
            Kind::PipeW(p) => p.borrow_mut().writers += 1,
            Kind::PipeRW(p) => {
                let mut p = p.borrow_mut();
                p.readers += 1;
                p.writers += 1;
            }
            _ => {}
        }
        Rc::new(RefCell::new(OpenFile {
            kind,
            flags,
            guest_path,
            owner: (0, 0),
            sigio: 0,
            lease: 2,
            created: false,
            ofd_owner: 0,
        }))
    }

    pub fn readable(&self) -> bool {
        self.flags & O_ACCMODE != O_WRONLY
    }

    pub fn writable(&self) -> bool {
        self.flags & O_ACCMODE != 0 || self.guest_path.ends_with("/oom_score_adj")
    }

    pub fn read(&mut self, len: usize, rng: &mut dyn FnMut(usize) -> Vec<u8>) -> Io {
        if !self.readable() {
            return Io::Err(EBADF);
        }
        let nonblock = self.flags & O_NONBLOCK != 0;
        match &mut self.kind {
            Kind::Console(c, 0) => {
                let mut c = c.borrow_mut();
                let n = len.min(c.stdin.len());
                Io::Done(c.stdin.drain(..n).collect())
            }
            Kind::Console(..) => Io::Err(EBADF),
            Kind::Host { file, .. } => {
                let mut b = vec![0u8; len];
                match file.read(&mut b) {
                    Ok(n) => {
                        b.truncate(n);
                        Io::Done(b)
                    }
                    Err(e) => Io::Err(host_errno(&e)),
                }
            }
            Kind::Dir { .. } => Io::Err(EISDIR),
            Kind::Path { .. } => Io::Err(EBADF),
            Kind::PipeR(p) | Kind::PipeRW(p) => {
                let mut p = p.borrow_mut();
                if p.buf.is_empty() {
                    if p.writers == 0 {
                        return Io::Done(Vec::new());
                    }
                    return if nonblock { Io::Err(EAGAIN) } else { Io::Block };
                }
                let n = len.min(p.buf.len());
                Io::Done(p.buf.drain(..n).collect())
            }
            Kind::PipeW(_) => Io::Err(EBADF),
            Kind::Socket => Io::Err(107), // ENOTCONN
            Kind::Null => Io::Done(Vec::new()),
            Kind::Zero => Io::Done(vec![0; len]),
            Kind::Random => Io::Done(rng(len)),
            Kind::Mem { data, pos } => {
                let n = len.min(data.len().saturating_sub(*pos));
                let out = data[*pos..*pos + n].to_vec();
                *pos += n;
                Io::Done(out)
            }
            Kind::Pagemap { mm, pos } => {
                // Solo voci intere; bit 63 = pagina presente (il PFN resta a 0,
                // come per chi non ha CAP_SYS_ADMIN).
                if !pos.is_multiple_of(8) || !len.is_multiple_of(8) {
                    return Io::Err(EINVAL);
                }
                let mm = mm.borrow();
                let mut out = Vec::with_capacity(len.min(1 << 20));
                for i in 0..(len / 8).min(1 << 17) as u64 {
                    let va = (*pos / 8 + i) * 4096;
                    let present = mm.mem.is_mapped(va, va + 1);
                    out.extend_from_slice(&(if present { 1u64 << 63 } else { 0 }).to_le_bytes());
                }
                *pos += out.len() as u64;
                Io::Done(out)
            }
        }
    }

    pub fn write(&mut self, data: &[u8]) -> Io {
        if !self.writable() {
            return Io::Err(EBADF);
        }
        let append = self.flags & O_APPEND != 0;
        let nonblock = self.flags & O_NONBLOCK != 0;
        match &mut self.kind {
            Kind::Console(c, which) => {
                let mut c = c.borrow_mut();
                let echo = c.echo;
                match which {
                    1 => {
                        if echo {
                            let _ = std::io::stdout().write_all(data);
                            let _ = std::io::stdout().flush();
                        }
                        c.stdout.extend_from_slice(data)
                    }
                    2 => {
                        if echo {
                            let _ = std::io::stderr().write_all(data);
                        }
                        c.stderr.extend_from_slice(data)
                    }
                    _ => return Io::Err(EBADF),
                }
                Io::Written(data.len())
            }
            Kind::Host { file, .. } => {
                if append && let Err(e) = file.seek(SeekFrom::End(0)) {
                    return Io::Err(host_errno(&e));
                }
                match file.write(data) {
                    Ok(n) => Io::Written(n),
                    Err(e) => Io::Err(host_errno(&e)),
                }
            }
            Kind::Dir { .. } | Kind::Path { .. } => Io::Err(EBADF),
            Kind::PipeW(p) | Kind::PipeRW(p) => {
                let mut p = p.borrow_mut();
                if p.readers == 0 {
                    return Io::BrokenPipe;
                }
                let room = p.capacity().saturating_sub(p.buf.len());
                if room == 0 {
                    return if nonblock { Io::Err(EAGAIN) } else { Io::Block };
                }
                let n = data.len().min(room);
                p.buf.extend(&data[..n]);
                Io::Written(n)
            }
            Kind::PipeR(_) => Io::Err(EBADF),
            Kind::Socket => Io::Err(107), // ENOTCONN
            Kind::Null | Kind::Zero | Kind::Random => Io::Written(data.len()),
            // /proc/<pid>/oom_score_adj si può scrivere (e non ha effetto);
            // il resto del /proc virtuale è in sola lettura.
            Kind::Mem { .. } if self.guest_path.ends_with("/oom_score_adj") => Io::Written(data.len()),
            Kind::Mem { .. } | Kind::Pagemap { .. } => Io::Err(EACCES),
        }
    }

    pub fn lseek(&mut self, off: i64, whence: u64) -> SysResult {
        match &mut self.kind {
            Kind::Host { file, .. } => {
                let pos = match whence {
                    0 => SeekFrom::Start(off as u64),
                    1 => SeekFrom::Current(off),
                    2 => SeekFrom::End(off),
                    _ => return Err(EINVAL),
                };
                file.seek(pos).map(|p| p as i64).map_err(|e| host_errno(&e))
            }
            Kind::Dir { pos, .. } => {
                if whence == 0 && off >= 0 {
                    *pos = off as usize;
                    Ok(off)
                } else {
                    Err(EINVAL)
                }
            }
            Kind::Mem { data, pos } => {
                let base = match whence {
                    0 => 0,
                    1 => *pos as i64,
                    2 => data.len() as i64,
                    _ => return Err(EINVAL),
                };
                let n = base + off;
                if n < 0 {
                    return Err(EINVAL);
                }
                *pos = n as usize;
                Ok(n)
            }
            Kind::Null | Kind::Zero | Kind::Random => Ok(0),
            Kind::Path { .. } => Err(EBADF),
            Kind::Pagemap { pos, .. } => {
                let n = match whence {
                    0 => off,
                    1 => *pos as i64 + off,
                    _ => return Err(EINVAL),
                };
                if n < 0 {
                    return Err(EINVAL);
                }
                *pos = n as u64;
                Ok(n)
            }
            _ => Err(ESPIPE),
        }
    }

    pub fn stat(&self) -> Result<Stat, i64> {
        match &self.kind {
            Kind::Host { file, .. } => {
                file.metadata().map(|m| Stat::from_host(&m)).map_err(|e| host_errno(&e))
            }
            Kind::Dir { path, .. } | Kind::Path { path, .. } => {
                std::fs::metadata(path).map(|m| Stat::from_host(&m)).map_err(|e| host_errno(&e))
            }
            Kind::PipeR(_) | Kind::PipeW(_) | Kind::PipeRW(_) => {
                Ok(Stat { mode: S_IFIFO | 0o600, blksize: 4096, nlink: 1, ..Default::default() })
            }
            Kind::Console(..) => {
                Ok(Stat { mode: S_IFIFO | 0o600, blksize: 4096, nlink: 1, ..Default::default() })
            }
            Kind::Socket => Ok(Stat { mode: 0o140777, blksize: 4096, nlink: 1, ..Default::default() }),
            Kind::Null | Kind::Zero | Kind::Random => {
                Ok(Stat { mode: S_IFCHR | 0o666, rdev: 0x103, blksize: 4096, nlink: 1, ..Default::default() })
            }
            Kind::Mem { data, .. } => Ok(Stat {
                mode: S_IFREG | 0o444,
                size: data.len() as i64,
                blksize: 1024,
                nlink: 1,
                ..Default::default()
            }),
            Kind::Pagemap { .. } => {
                Ok(Stat { mode: S_IFREG | 0o400, blksize: 1024, nlink: 1, ..Default::default() })
            }
        }
    }

    /// Voci di directory da `pos` in poi, già serializzate come
    /// linux_dirent64, fino a `cap` byte.
    pub fn getdents(&mut self, cap: usize) -> SysResult2 {
        let Kind::Dir { path, entries, pos } = &mut self.kind else { return Err(ENOTDIR) };
        if entries.is_none() {
            *entries = Some(read_dir_entries(path)?);
        }
        let list = entries.as_ref().unwrap();
        let mut out = Vec::new();
        while *pos < list.len() {
            let e = &list[*pos];
            let reclen = (19 + e.name.len() + 1).next_multiple_of(8);
            if out.len() + reclen > cap {
                if out.is_empty() {
                    return Err(EINVAL);
                }
                break;
            }
            let start = out.len();
            out.extend_from_slice(&e.ino.to_le_bytes());
            out.extend_from_slice(&((*pos + 1) as i64).to_le_bytes());
            out.extend_from_slice(&(reclen as u16).to_le_bytes());
            out.push(e.dtype);
            out.extend_from_slice(&e.name);
            out.resize(start + reclen, 0);
            *pos += 1;
        }
        Ok(out)
    }

    pub fn is_pipe_ready(&self) -> bool {
        match &self.kind {
            Kind::PipeR(p) => {
                let p = p.borrow();
                !p.buf.is_empty() || p.writers == 0
            }
            Kind::PipeW(p) => {
                let p = p.borrow();
                p.buf.len() < p.capacity() || p.readers == 0
            }
            Kind::PipeRW(p) => {
                let p = p.borrow();
                !p.buf.is_empty() || p.buf.len() < p.capacity()
            }
            _ => false,
        }
    }
}

pub type SysResult2 = Result<Vec<u8>, i64>;

fn dtype_of(ft: &std::fs::FileType) -> u8 {
    use std::os::unix::fs::FileTypeExt;
    if ft.is_dir() {
        4
    } else if ft.is_symlink() {
        10
    } else if ft.is_file() {
        8
    } else if ft.is_fifo() {
        1
    } else if ft.is_char_device() {
        2
    } else if ft.is_block_device() {
        6
    } else if ft.is_socket() {
        12
    } else {
        0
    }
}

fn read_dir_entries(path: &PathBuf) -> Result<Vec<DirEnt>, i64> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let mut v = Vec::new();
    let me = std::fs::metadata(path).map_err(|e| host_errno(&e))?;
    v.push(DirEnt { ino: me.ino(), dtype: 4, name: b".".to_vec() });
    let parent = std::fs::metadata(path.join("..")).map(|m| m.ino()).unwrap_or(me.ino());
    v.push(DirEnt { ino: parent, dtype: 4, name: b"..".to_vec() });
    for e in std::fs::read_dir(path).map_err(|e| host_errno(&e))? {
        let e = e.map_err(|e| host_errno(&e))?;
        let ino = e.metadata().map(|m| m.ino()).unwrap_or(0);
        let dtype = e.file_type().map(|t| dtype_of(&t)).unwrap_or(0);
        v.push(DirEnt { ino, dtype, name: e.file_name().as_bytes().to_vec() });
    }
    Ok(v)
}

#[derive(Clone)]
pub struct Fd {
    pub file: Rc<RefCell<OpenFile>>,
    pub cloexec: bool,
}

#[derive(Clone, Default)]
pub struct FdTable {
    fds: Vec<Option<Fd>>,
    /// RLIMIT_NOFILE corrente del processo; 0 = quello di default.
    pub limit: usize,
}

/// Limite di descrittori per processo (RLIMIT_NOFILE).
pub const NOFILE: usize = 1024;

impl FdTable {
    fn max(&self) -> usize {
        if self.limit == 0 { NOFILE } else { self.limit }
    }

    pub fn with_console(c: &Rc<RefCell<Console>>) -> Self {
        let mut t = FdTable::default();
        for (fd, flags) in [(0u8, 0), (1, O_WRONLY), (2, O_WRONLY)] {
            let f = OpenFile::new(Kind::Console(c.clone(), fd), flags, format!("/dev/fd/{fd}"));
            t.fds.push(Some(Fd { file: f, cloexec: false }));
        }
        t
    }

    pub fn get(&self, fd: i64) -> Result<Rc<RefCell<OpenFile>>, i64> {
        if fd < 0 {
            return Err(EBADF);
        }
        self.fds.get(fd as usize).and_then(|f| f.as_ref()).map(|f| f.file.clone()).ok_or(EBADF)
    }

    pub fn entry(&mut self, fd: i64) -> Result<&mut Fd, i64> {
        if fd < 0 {
            return Err(EBADF);
        }
        self.fds.get_mut(fd as usize).and_then(|f| f.as_mut()).ok_or(EBADF)
    }

    /// Installa `file` nel primo descrittore libero ≥ `min`.
    pub fn install(&mut self, file: Rc<RefCell<OpenFile>>, cloexec: bool, min: usize) -> SysResult {
        let mut i = min;
        while i < self.fds.len() && self.fds[i].is_some() {
            i += 1;
        }
        if i >= self.max() {
            return Err(EMFILE);
        }
        if i >= self.fds.len() {
            self.fds.resize(i + 1, None);
        }
        self.fds[i] = Some(Fd { file, cloexec });
        Ok(i as i64)
    }

    /// Installa `file` esattamente in `fd`, chiudendo ciò che c'era (dup3).
    pub fn install_at(&mut self, fd: usize, file: Rc<RefCell<OpenFile>>, cloexec: bool) -> SysResult {
        if fd >= self.max() {
            return Err(EBADF);
        }
        if fd >= self.fds.len() {
            self.fds.resize(fd + 1, None);
        }
        self.fds[fd] = Some(Fd { file, cloexec });
        Ok(fd as i64)
    }

    pub fn close(&mut self, fd: i64) -> SysResult {
        if fd < 0 || fd as usize >= self.fds.len() || self.fds[fd as usize].is_none() {
            return Err(EBADF);
        }
        self.fds[fd as usize] = None;
        Ok(0)
    }

    pub fn close_on_exec(&mut self) {
        for f in self.fds.iter_mut() {
            if f.as_ref().is_some_and(|f| f.cloexec) {
                *f = None;
            }
        }
    }

    pub fn any_pipe_ready(&self) -> bool {
        self.fds.iter().flatten().any(|f| f.file.borrow().is_pipe_ready())
    }

    /// I file aperti della tabella.
    pub fn files(&self) -> impl Iterator<Item = &Rc<RefCell<OpenFile>>> {
        self.fds.iter().flatten().map(|f| &f.file)
    }

    /// Descrittori aperti (per /proc/self/fd).
    pub fn open_fds(&self) -> Vec<usize> {
        self.fds.iter().enumerate().filter(|(_, f)| f.is_some()).map(|(i, _)| i).collect()
    }
}

/// FIFO del file system: la pipe interna condivisa da tutti gli aperti dello
/// stesso file, e chi è fermo in open() aspettando l'altro capo.
#[derive(Default)]
pub struct Fifo {
    pub pipe: Rc<RefCell<Pipe>>,
    pub waiting_readers: Vec<i32>,
    pub waiting_writers: Vec<i32>,
}

/// Crea una pipe: (lettura, scrittura).
pub fn new_pipe(flags: u64) -> (Rc<RefCell<OpenFile>>, Rc<RefCell<OpenFile>>) {
    let p = Rc::new(RefCell::new(Pipe::default()));
    let nb = flags & O_NONBLOCK;
    (
        OpenFile::new(Kind::PipeR(p.clone()), nb, "pipe:".into()),
        OpenFile::new(Kind::PipeW(p), O_WRONLY | nb, "pipe:".into()),
    )
}

/// Percorso assoluto del guest: `path` relativo a `base` (cwd o directory
/// di un dirfd), normalizzato solo lessicalmente per "." e "//".
pub fn join(base: &str, path: &[u8]) -> String {
    let p = String::from_utf8_lossy(path);
    let full =
        if p.starts_with('/') { p.into_owned() } else { format!("{}/{}", base.trim_end_matches('/'), p) };
    let mut out: Vec<&str> = Vec::new();
    for c in full.split('/') {
        match c {
            "" | "." => {}
            c => out.push(c),
        }
    }
    let s = format!("/{}", out.join("/"));
    if full.ends_with('/') && s != "/" { s + "/" } else { s }
}

/// Dopo una creazione: i permessi devono dipendere solo dalla umask del
/// guest, non da quella del processo host (che li avrebbe già ridotti).
pub fn fix_mode(host: &std::path::Path, want: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = std::fs::symlink_metadata(host)
        && m.permissions().mode() & 0o777 != want & 0o777
    {
        let keep = m.permissions().mode() & 0o7000;
        let _ = std::fs::set_permissions(host, std::fs::Permissions::from_mode(keep | (want & 0o777)));
    }
}

/// Apertura di un percorso del guest.
pub fn open(guest: &str, flags: u64, mode: u32, umask: u32) -> Result<Rc<RefCell<OpenFile>>, i64> {
    use std::os::unix::fs::OpenOptionsExt;
    let special = match guest {
        "/dev/null" => Some(Kind::Null),
        "/dev/zero" => Some(Kind::Zero),
        "/dev/urandom" | "/dev/random" => Some(Kind::Random),
        _ => None,
    };
    let status = flags & (O_ACCMODE | O_APPEND | O_NONBLOCK);
    if let Some(k) = special {
        return Ok(OpenFile::new(k, status, guest.into()));
    }
    let host = PathBuf::from(guest.trim_end_matches('/').to_string() + if guest == "/" { "/" } else { "" });
    let meta =
        if flags & O_NOFOLLOW != 0 { std::fs::symlink_metadata(&host) } else { std::fs::metadata(&host) };
    if flags & O_PATH != 0 {
        // Nessun permesso richiesto sul file: basta che esista.
        // Con O_NOFOLLOW il riferimento è al link stesso.
        let m = meta.map_err(|e| host_errno(&e))?;
        if flags & O_DIRECTORY != 0 && !m.is_dir() {
            return Err(ENOTDIR);
        }
        let kind = Kind::Path { path: host, dir: m.is_dir() };
        return Ok(OpenFile::new(kind, O_PATH, guest.into()));
    }
    match &meta {
        Ok(m) if m.is_dir() => {
            if flags & O_ACCMODE != 0 {
                return Err(EISDIR);
            }
            if flags & O_CREAT != 0 && flags & O_EXCL != 0 {
                return Err(EEXIST);
            }
            return Ok(OpenFile::new(Kind::Dir { path: host, entries: None, pos: 0 }, status, guest.into()));
        }
        Ok(m) if m.file_type().is_symlink() => return Err(40), // ELOOP con O_NOFOLLOW
        Ok(_) if flags & O_DIRECTORY != 0 => return Err(ENOTDIR),
        Err(_) if flags & O_DIRECTORY != 0 && flags & O_CREAT == 0 => {
            return Err(meta.as_ref().err().map(host_errno).unwrap_or(ENOENT));
        }
        _ => {}
    }
    if guest.ends_with('/') && flags & O_CREAT != 0 {
        return Err(EISDIR);
    }
    let mut o = std::fs::OpenOptions::new();
    match flags & O_ACCMODE {
        0 => o.read(true),
        O_WRONLY => o.write(true),
        _ => o.read(true).write(true),
    };
    if flags & O_CREAT != 0 {
        // Non con create(): std la rifiuta senza accesso in scrittura, Linux no.
        o.custom_flags(libc::O_CREAT | if flags & O_EXCL != 0 { libc::O_EXCL } else { 0 });
        o.mode(mode & !umask & 0o7777);
    }
    if flags & O_TRUNC != 0 && flags & O_ACCMODE != 0 {
        o.truncate(true);
    }
    let file = o.open(&host).map_err(|e| host_errno(&e))?;
    let created = flags & O_CREAT != 0 && meta.is_err();
    if created {
        fix_mode(&host, mode & !umask & 0o7777);
    }
    if flags & O_TRUNC != 0 && flags & O_ACCMODE == 0 {
        // O_TRUNC con O_RDONLY: Linux tronca comunque (comportamento non
        // specificato da POSIX); servirebbe la scrittura, lo ignoriamo.
    }
    let f = OpenFile::new(Kind::Host { file, path: host }, status, guest.into());
    f.borrow_mut().created = created;
    Ok(f)
}
