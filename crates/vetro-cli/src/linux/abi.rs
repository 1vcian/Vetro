//! Costanti e strutture dell'ABI Linux arm64 (asm-generic).

use vetro_cpu::{Memory, UserMemory};

pub const EPERM: i64 = 1;
pub const ENOENT: i64 = 2;
pub const ESRCH: i64 = 3;
pub const EINTR: i64 = 4;
pub const EIO: i64 = 5;
pub const E2BIG: i64 = 7;
pub const ENOEXEC: i64 = 8;
pub const EBADF: i64 = 9;
pub const ECHILD: i64 = 10;
pub const EAGAIN: i64 = 11;
pub const EDEADLK: i64 = 35;
pub const ENOMEM: i64 = 12;
pub const EACCES: i64 = 13;
pub const EFAULT: i64 = 14;
pub const EEXIST: i64 = 17;
pub const EXDEV: i64 = 18;
pub const ENOTDIR: i64 = 20;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;
pub const EMFILE: i64 = 24;
pub const ENOTTY: i64 = 25;
pub const ESPIPE: i64 = 29;
pub const EPIPE: i64 = 32;
pub const ERANGE: i64 = 34;
pub const ENAMETOOLONG: i64 = 36;
pub const ENOSYS: i64 = 38;
pub const ENOTEMPTY: i64 = 39;
pub const ETIMEDOUT: i64 = 110;

pub const AT_FDCWD: i32 = -100;
pub const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
pub const AT_REMOVEDIR: u64 = 0x200;
pub const AT_EMPTY_PATH: u64 = 0x1000;

// Flag di open(2) su arm64 (diversi da x86 per O_DIRECTORY e O_NOFOLLOW).
pub const O_ACCMODE: u64 = 3;
pub const O_WRONLY: u64 = 1;
pub const O_RDWR: u64 = 2;
pub const O_CREAT: u64 = 0o100;
pub const O_EXCL: u64 = 0o200;
pub const O_TRUNC: u64 = 0o1000;
pub const O_APPEND: u64 = 0o2000;
pub const O_NONBLOCK: u64 = 0o4000;
pub const O_ASYNC: u64 = 0o20000;
pub const O_DIRECTORY: u64 = 0o40000;
pub const O_NOFOLLOW: u64 = 0o100000;
pub const O_CLOEXEC: u64 = 0o2000000;
pub const O_PATH: u64 = 0o10000000;
/// __O_TMPFILE (O_TMPFILE = __O_TMPFILE | O_DIRECTORY).
pub const O_TMPFILE: u64 = 0o20000000;

/// Errore di una syscall: il valore da restituire è `-errno`.
pub type SysResult = Result<i64, i64>;

/// Nanosecondi di una `struct timespec` del guest: EINVAL se `tv_sec < 0` o
/// `tv_nsec` fuori da [0, 1e9), come Linux; i valori enormi saturano (una
/// scadenza così lontana non arriva mai) invece di traboccare.
pub fn timespec_ns(sec: u64, nsec: u64) -> Result<u64, i64> {
    let (s, n) = (sec as i64, nsec as i64);
    if s < 0 || !(0..1_000_000_000).contains(&n) {
        return Err(EINVAL);
    }
    Ok((s as u64).saturating_mul(1_000_000_000).saturating_add(n as u64))
}

/// Come [`timespec_ns`] per una `struct timeval` (microsecondi).
pub fn timeval_ns(sec: u64, usec: u64) -> Result<u64, i64> {
    let (s, u) = (sec as i64, usec as i64);
    if s < 0 || !(0..1_000_000).contains(&u) {
        return Err(EINVAL);
    }
    Ok((s as u64).saturating_mul(1_000_000_000).saturating_add(u as u64 * 1000))
}

/// Converte un errore di I/O dell'host nell'errno Linux corrispondente.
pub fn host_errno(e: &std::io::Error) -> i64 {
    match e.raw_os_error() {
        Some(n) => linux_errno(n),
        None => match e.kind() {
            std::io::ErrorKind::NotFound => ENOENT,
            std::io::ErrorKind::PermissionDenied => EACCES,
            std::io::ErrorKind::AlreadyExists => EEXIST,
            std::io::ErrorKind::InvalidInput => EINVAL,
            _ => EIO,
        },
    }
}

/// Errno dell'host → errno Linux arm64. Su Linux sono gli stessi numeri.
#[cfg(target_os = "linux")]
fn linux_errno(n: i32) -> i64 {
    n as i64
}

/// Errno dell'host → errno Linux arm64. Da 1 a 34 i numeri coincidono con
/// quelli di Linux tranne 11 (EDEADLK sull'host, EAGAIN su Linux); il resto
/// si traduce per nome.
#[cfg(not(target_os = "linux"))]
fn linux_errno(n: i32) -> i64 {
    use libc::*;
    let l = match n {
        EDEADLK => 35,
        EAGAIN => 11,
        1..=34 => n,
        ENAMETOOLONG => 36,
        ENOLCK => 37,
        ENOSYS => 38,
        ENOTEMPTY => 39,
        ELOOP => 40,
        ENOMSG => 42,
        EIDRM => 43,
        EOVERFLOW => 75,
        EILSEQ => 84,
        EUSERS => 87,
        ENOTSOCK => 88,
        EDESTADDRREQ => 89,
        EMSGSIZE => 90,
        EPROTOTYPE => 91,
        ENOPROTOOPT => 92,
        EPROTONOSUPPORT => 93,
        ESOCKTNOSUPPORT => 94,
        EOPNOTSUPP | ENOTSUP => 95,
        EPFNOSUPPORT => 96,
        EAFNOSUPPORT => 97,
        EADDRINUSE => 98,
        EADDRNOTAVAIL => 99,
        ENETDOWN => 100,
        ENETUNREACH => 101,
        ENETRESET => 102,
        ECONNABORTED => 103,
        ECONNRESET => 104,
        ENOBUFS => 105,
        EISCONN => 106,
        ENOTCONN => 107,
        ESHUTDOWN => 108,
        ETOOMANYREFS => 109,
        ETIMEDOUT => 110,
        ECONNREFUSED => 111,
        EHOSTDOWN => 112,
        EHOSTUNREACH => 113,
        EALREADY => 114,
        EINPROGRESS => 115,
        ESTALE => 116,
        EDQUOT => 122,
        ECANCELED => 125,
        EOWNERDEAD => 130,
        ENOTRECOVERABLE => 131,
        _ => 5, // EIO
    };
    l as i64
}

pub fn read_u64(mem: &mut UserMemory, addr: u64) -> Result<u64, i64> {
    let mut b = [0u8; 8];
    mem.read(addr, &mut b).map_err(|_| EFAULT)?;
    Ok(u64::from_le_bytes(b))
}

pub fn read_u32(mem: &mut UserMemory, addr: u64) -> Result<u32, i64> {
    let mut b = [0u8; 4];
    mem.read(addr, &mut b).map_err(|_| EFAULT)?;
    Ok(u32::from_le_bytes(b))
}

pub fn write_u64(mem: &mut UserMemory, addr: u64, v: u64) -> Result<(), i64> {
    mem.write(addr, &v.to_le_bytes()).map_err(|_| EFAULT)
}

pub fn write_u32(mem: &mut UserMemory, addr: u64, v: u32) -> Result<(), i64> {
    mem.write(addr, &v.to_le_bytes()).map_err(|_| EFAULT)
}

/// Oltre questa lunghezza un buffer del guest non può essere tutto mappato
/// (e allocarlo potrebbe far abortire l'emulatore).
pub const MAX_IO: usize = 1 << 30;

pub fn read_bytes(mem: &mut UserMemory, addr: u64, len: usize) -> Result<Vec<u8>, i64> {
    if len > MAX_IO {
        return Err(EFAULT);
    }
    let mut b = vec![0u8; len];
    mem.read(addr, &mut b).map_err(|_| EFAULT)?;
    Ok(b)
}

pub fn write_bytes(mem: &mut UserMemory, addr: u64, data: &[u8]) -> Result<(), i64> {
    mem.write(addr, data).map_err(|_| EFAULT)
}

/// Stringa C (al massimo PATH_MAX byte).
pub fn read_cstr(mem: &mut UserMemory, addr: u64) -> Result<Vec<u8>, i64> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    loop {
        mem.read(addr + out.len() as u64, &mut b).map_err(|_| EFAULT)?;
        if b[0] == 0 {
            return Ok(out);
        }
        out.push(b[0]);
        if out.len() > 4096 {
            return Err(ENAMETOOLONG);
        }
    }
}

/// Vettore di stringhe terminato da NULL (argv, envp).
pub fn read_strv(mem: &mut UserMemory, mut addr: u64) -> Result<Vec<Vec<u8>>, i64> {
    let mut v = Vec::new();
    if addr == 0 {
        return Ok(v);
    }
    loop {
        let p = read_u64(mem, addr)?;
        if p == 0 {
            return Ok(v);
        }
        v.push(read_cstr(mem, p)?);
        addr += 8;
        if v.len() > 100_000 {
            return Err(E2BIG);
        }
    }
}

/// `struct stat` di arm64 (128 byte).
#[derive(Default, Clone, Copy, Debug)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: i64,
    pub blksize: i32,
    pub blocks: i64,
    pub atime: (i64, i64),
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
}

impl Stat {
    pub fn to_bytes(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        let mut put = |off: usize, v: &[u8]| b[off..off + v.len()].copy_from_slice(v);
        put(0, &self.dev.to_le_bytes());
        put(8, &self.ino.to_le_bytes());
        put(16, &self.mode.to_le_bytes());
        put(20, &self.nlink.to_le_bytes());
        put(24, &self.uid.to_le_bytes());
        put(28, &self.gid.to_le_bytes());
        put(32, &self.rdev.to_le_bytes());
        put(48, &self.size.to_le_bytes());
        put(56, &self.blksize.to_le_bytes());
        put(64, &self.blocks.to_le_bytes());
        put(72, &self.atime.0.to_le_bytes());
        put(80, &self.atime.1.to_le_bytes());
        put(88, &self.mtime.0.to_le_bytes());
        put(96, &self.mtime.1.to_le_bytes());
        put(104, &self.ctime.0.to_le_bytes());
        put(112, &self.ctime.1.to_le_bytes());
        b
    }

    pub fn from_host(m: &std::fs::Metadata) -> Stat {
        use std::os::unix::fs::MetadataExt;
        Stat {
            dev: m.dev(),
            ino: m.ino(),
            mode: m.mode(),
            nlink: m.nlink() as u32,
            uid: m.uid(),
            gid: m.gid(),
            rdev: m.rdev(),
            size: m.size() as i64,
            blksize: m.blksize() as i32,
            blocks: m.blocks() as i64,
            atime: (m.atime(), m.atime_nsec()),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
}

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFIFO: u32 = 0o010000;
pub const S_IFCHR: u32 = 0o020000;
