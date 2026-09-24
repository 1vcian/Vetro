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
    Null,
    Zero,
    Random,
    /// Socket non connesso (AF_UNIX/AF_INET): la rete arriva con M7.
    Socket,
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
}

impl Drop for OpenFile {
    fn drop(&mut self) {
        match &self.kind {
            Kind::PipeR(p) => p.borrow_mut().readers -= 1,
            Kind::PipeW(p) => p.borrow_mut().writers -= 1,
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
            _ => {}
        }
        Rc::new(RefCell::new(OpenFile { kind, flags, guest_path }))
    }

    pub fn readable(&self) -> bool {
        self.flags & O_ACCMODE != O_WRONLY
    }

    pub fn writable(&self) -> bool {
        self.flags & O_ACCMODE != 0
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
            Kind::PipeR(p) => {
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
            Kind::Dir { .. } => Io::Err(EBADF),
            Kind::PipeW(p) => {
                let mut p = p.borrow_mut();
                if p.readers == 0 {
                    return Io::BrokenPipe;
                }
                let room = PIPE_CAP - p.buf.len();
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
            Kind::Mem { .. } => Io::Err(EACCES),
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
            _ => Err(ESPIPE),
        }
    }

    pub fn stat(&self) -> Result<Stat, i64> {
        match &self.kind {
            Kind::Host { file, .. } => {
                file.metadata().map(|m| Stat::from_host(&m)).map_err(|e| host_errno(&e))
            }
            Kind::Dir { path, .. } => {
                std::fs::metadata(path).map(|m| Stat::from_host(&m)).map_err(|e| host_errno(&e))
            }
            Kind::PipeR(_) | Kind::PipeW(_) => {
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
                p.buf.len() < PIPE_CAP || p.readers == 0
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
}

/// Limite di descrittori per processo (RLIMIT_NOFILE).
pub const NOFILE: usize = 1024;

impl FdTable {
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
        if i >= NOFILE {
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
        if fd >= NOFILE {
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

    /// Descrittori aperti (per /proc/self/fd).
    pub fn open_fds(&self) -> Vec<usize> {
        self.fds.iter().enumerate().filter(|(_, f)| f.is_some()).map(|(i, _)| i).collect()
    }
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
        if flags & O_EXCL != 0 {
            o.create_new(true);
        } else {
            o.create(true);
        }
        o.mode(mode & !umask & 0o7777);
    }
    if flags & O_TRUNC != 0 && flags & O_ACCMODE != 0 {
        o.truncate(true);
    }
    let file = o.open(&host).map_err(|e| host_errno(&e))?;
    if flags & O_TRUNC != 0 && flags & O_ACCMODE == 0 {
        // O_TRUNC con O_RDONLY: Linux tronca comunque (comportamento non
        // specificato da POSIX); servirebbe la scrittura, lo ignoriamo.
    }
    Ok(OpenFile::new(Kind::Host { file, path: host }, status, guest.into()))
}
