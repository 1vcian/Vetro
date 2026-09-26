//! Syscall tracciate dall'esterno in modalità sistema: il record, la
//! decodifica di base (percorsi, descrittori, dati letti e scritti,
//! indirizzi dei socket, transazioni binder) e la riga in stile strace.
//!
//! La decodifica legge la memoria del processo **nel momento** della
//! syscall: all'ingresso ciò che il programma passa (percorso, dati di
//! `write`, flusso di scrittura binder), all'uscita ciò che il kernel ha
//! scritto (dati di `read`, flusso di lettura binder).

use super::binder::{self, BINDER_WRITE_READ, Transaction, WriteRead};
use super::elf::VirtRead;
use crate::syscall;

/// Byte di dati al più conservati per record (read/write, Parcel).
pub const DATA_CAP: usize = 4096;

/// Una syscall.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyscallRecord {
    /// Numero d'istruzione all'ingresso (SVC) e al ritorno in EL0.
    pub step: u64,
    pub exit_step: Option<u64>,
    pub pid: i32,
    pub tid: i32,
    pub comm: String,
    pub nr: u64,
    pub args: [u64; 6],
    /// PC della SVC.
    pub pc: u64,
    /// Valore restituito (x0 al ritorno all'istruzione dopo la SVC).
    pub ret: Option<i64>,
    /// Ritorno in EL0 altrove (execve riuscita, gestore di un segnale,
    /// syscall da ripetere): il PC.
    pub diverted: Option<u64>,
    /// Primo argomento stringa (percorso).
    pub path: Option<String>,
    /// Percorso del descrittore del primo argomento.
    pub fd_path: Option<String>,
    /// Dati scritti (ingresso) o letti (uscita), al più [`DATA_CAP`].
    pub data: Vec<u8>,
    /// Indirizzo di `connect`/`bind`/`sendto`.
    pub sockaddr: Option<String>,
    /// Transazioni binder di `ioctl(BINDER_WRITE_READ)`: inviate
    /// all'ingresso, ricevute all'uscita.
    pub binder: Vec<Transaction>,
}

/// Syscall con un descrittore come primo argomento.
pub fn fd_first(nr: u64) -> bool {
    matches!(nr, 23..=25 | 29 | 44 | 46 | 50 | 52 | 55 | 57 | 61..=71 | 80 | 82 | 200..=212 | 242)
}

impl SyscallRecord {
    pub fn name(&self) -> &'static str {
        syscall::name(self.nr)
    }

    /// Riga in stile strace: `[tid] openat(AT_FDCWD, "/etc/x", 0x0, 0x0) = 3`,
    /// con i dettagli decodificati dopo `;`.
    pub fn line(&self) -> String {
        let path = self.path.clone();
        let ret = self.ret.unwrap_or(0);
        let mut s = format!("[{}] {}", self.tid, syscall::format(self.nr, &self.args, ret, |_| path.clone()));
        if self.ret.is_none() {
            // Senza ritorno: via il " = 0" finto.
            if let Some(i) = s.rfind(" = ") {
                s.truncate(i);
            }
            match self.diverted {
                Some(pc) => s += &format!(" = ? (ritorno a {pc:#x})"),
                None => s += " = ?",
            }
        }
        if let Some(p) = &self.fd_path {
            s += &format!(" ; fd={p}");
        }
        if let Some(a) = &self.sockaddr {
            s += &format!(" ; addr={a}");
        }
        if !self.data.is_empty() {
            s += &format!(" ; dati={}", escape(&self.data, 64));
        }
        for t in &self.binder {
            s += &format!(
                " ; {} target={:#x} code={:#x} flags={:#x} dati={}",
                t.command, t.target, t.code, t.flags, t.data_size
            );
            if let Some(i) = t.interface() {
                s += &format!(" if={i}");
            }
        }
        s
    }

    /// Decodifica all'ingresso: `user` legge la memoria del processo,
    /// `fd_path` dà il percorso di un descrittore del processo.
    pub fn decode_entry(&mut self, user: &impl VirtRead, fd_path: &dyn Fn(u32) -> Option<String>) {
        let a = self.args;
        if let Some((_, kinds)) = syscall::lookup(self.nr)
            && let Some(i) = kinds.iter().position(|k| *k == syscall::Arg::Str)
        {
            self.path = read_cstr(user, a[i], 4096).map(|b| String::from_utf8_lossy(&b).into_owned());
        }
        if fd_first(self.nr) {
            self.fd_path = fd_path(a[0] as u32);
        }
        match self.nr {
            // write, pwrite64, sendto: i dati.
            64 | 68 | 206 => self.data = read_n(user, a[1], a[2]),
            _ => {}
        }
        match self.nr {
            200 | 203 => self.sockaddr = read_sockaddr(user, a[1], a[2]),
            206 if a[4] != 0 => self.sockaddr = read_sockaddr(user, a[4], a[5]),
            29 if a[1] == BINDER_WRITE_READ => {
                if let Some(bwr) = read_bwr(user, a[2]) {
                    let start = bwr.write_buffer.wrapping_add(bwr.write_consumed);
                    let len = bwr.write_size.saturating_sub(bwr.write_consumed);
                    self.binder = binder_stream(user, start, len);
                }
            }
            _ => {}
        }
    }

    /// Decodifica al ritorno (con `ret` già impostato).
    pub fn decode_exit(&mut self, user: &impl VirtRead) {
        let Some(ret) = self.ret else { return };
        let a = self.args;
        match self.nr {
            // read, pread64, recvfrom: i dati letti.
            63 | 67 | 207 if ret > 0 => self.data = read_n(user, a[1], ret as u64),
            29 if a[1] == BINDER_WRITE_READ && ret == 0 => {
                if let Some(bwr) = read_bwr(user, a[2]) {
                    self.binder.extend(binder_stream(user, bwr.read_buffer, bwr.read_consumed));
                }
            }
            _ => {}
        }
    }
}

fn read_n(user: &impl VirtRead, va: u64, len: u64) -> Vec<u8> {
    let n = len.min(DATA_CAP as u64) as usize;
    let mut b = vec![0u8; n];
    if user.read_virt(va, &mut b) {
        return b;
    }
    // Pagina per pagina: la parte leggibile.
    let mut out = Vec::new();
    let mut at = va;
    while out.len() < n {
        let k = (4096 - (at & 4095) as usize).min(n - out.len());
        let mut c = vec![0u8; k];
        if !user.read_virt(at, &mut c) {
            break;
        }
        out.extend_from_slice(&c);
        at = at.wrapping_add(k as u64);
    }
    out
}

/// Stringa C nella memoria del processo.
pub fn read_cstr(user: &impl VirtRead, va: u64, max: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = va;
    while out.len() < max {
        let k = (4096 - (at & 4095) as usize).min(max - out.len()).min(256);
        let mut c = vec![0u8; k];
        if !user.read_virt(at, &mut c) {
            return (!out.is_empty()).then_some(out);
        }
        if let Some(z) = c.iter().position(|&b| b == 0) {
            out.extend_from_slice(&c[..z]);
            return Some(out);
        }
        out.extend_from_slice(&c);
        at = at.wrapping_add(k as u64);
    }
    Some(out)
}

fn read_bwr(user: &impl VirtRead, va: u64) -> Option<WriteRead> {
    let mut b = [0u8; 48];
    user.read_virt(va, &mut b).then(|| WriteRead::parse(&b)).flatten()
}

/// Le transazioni di un flusso binder, con i byte del Parcel.
fn binder_stream(user: &impl VirtRead, va: u64, len: u64) -> Vec<Transaction> {
    if len == 0 || len > 1 << 20 {
        return Vec::new();
    }
    let mut buf = vec![0u8; len as usize];
    if !user.read_virt(va, &mut buf) {
        return Vec::new();
    }
    binder::commands(&buf)
        .iter()
        .filter_map(|c| c.transaction())
        .map(|mut t| {
            t.data = read_n(user, t.buffer, t.data_size);
            let n = (t.offsets_size / 8).min(256);
            let raw = read_n(user, t.offsets, n * 8);
            t.objects =
                raw.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().expect("8 byte"))).collect();
            t
        })
        .collect()
}

/// `struct sockaddr` leggibile: `unix:/percorso`, `unix:@astratto`,
/// `10.0.2.2:80`, `[::1]:443`.
pub fn read_sockaddr(user: &impl VirtRead, va: u64, len: u64) -> Option<String> {
    let n = len.clamp(2, 128) as usize;
    let mut b = vec![0u8; n];
    if !user.read_virt(va, &mut b) {
        return None;
    }
    sockaddr(&b)
}

/// Decodifica una `struct sockaddr`.
pub fn sockaddr(b: &[u8]) -> Option<String> {
    let family = u16::from_le_bytes(b.get(..2)?.try_into().ok()?);
    match family {
        1 => {
            let p = b.get(2..)?;
            if p.first() == Some(&0) {
                let end = p[1..].iter().position(|&c| c == 0).map_or(p.len(), |e| e + 1);
                Some(format!("unix:@{}", String::from_utf8_lossy(&p[1..end])))
            } else {
                let end = p.iter().position(|&c| c == 0).unwrap_or(p.len());
                Some(format!("unix:{}", String::from_utf8_lossy(&p[..end])))
            }
        }
        2 => {
            let port = u16::from_be_bytes(b.get(2..4)?.try_into().ok()?);
            let ip = b.get(4..8)?;
            Some(format!("{}.{}.{}.{}:{port}", ip[0], ip[1], ip[2], ip[3]))
        }
        10 => {
            let port = u16::from_be_bytes(b.get(2..4)?.try_into().ok()?);
            let ip: [u8; 16] = b.get(8..24)?.try_into().ok()?;
            Some(format!("[{}]:{port}", std::net::Ipv6Addr::from(ip)))
        }
        f => Some(format!("famiglia {f}")),
    }
}

/// Byte leggibili: ASCII stampabile, il resto `\xNN`, al più `max` byte.
pub fn escape(b: &[u8], max: usize) -> String {
    let mut s = String::from("\"");
    for &c in b.iter().take(max) {
        match c {
            b'\n' => s += "\\n",
            b'\t' => s += "\\t",
            b'"' => s += "\\\"",
            b'\\' => s += "\\\\",
            0x20..=0x7e => s.push(char::from(c)),
            _ => s += &format!("\\x{c:02x}"),
        }
    }
    s.push('"');
    if b.len() > max {
        s += "...";
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Memoria finta del processo da 0x1000.
    fn mem(img: &[u8]) -> impl Fn(u64, &mut [u8]) -> bool + '_ {
        move |va: u64, buf: &mut [u8]| {
            let Some(o) = va.checked_sub(0x1000) else { return false };
            match img.get(o as usize..o as usize + buf.len()) {
                Some(s) => {
                    buf.copy_from_slice(s);
                    true
                }
                None => false,
            }
        }
    }

    #[test]
    fn openat_write_read_connect() {
        let mut img = vec![0u8; 0x2000];
        img[0x10..0x1a].copy_from_slice(b"/etc/hosts");
        img[0x100..0x105].copy_from_slice(b"ciao\n");
        img[0x200..0x210].copy_from_slice(&[2, 0, 0, 80, 10, 0, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0]);
        let m = mem(&img);
        let fdp = |fd: u32| (fd == 1).then(|| "/dev/console".to_string());
        let mut r = SyscallRecord {
            nr: 56,
            args: [(-100i64) as u64, 0x1010, 0, 0, 0, 0],
            tid: 7,
            ..Default::default()
        };
        r.decode_entry(&m, &fdp);
        r.ret = Some(3);
        assert_eq!(r.line(), "[7] openat(AT_FDCWD, \"/etc/hosts\", 0x0, 0x0) = 3");
        let mut w = SyscallRecord { nr: 64, args: [1, 0x1100, 5, 0, 0, 0], tid: 7, ..Default::default() };
        w.decode_entry(&m, &fdp);
        w.ret = Some(5);
        assert_eq!(w.line(), "[7] write(1, 0x1100, 5) = 5 ; fd=/dev/console ; dati=\"ciao\\n\"");
        let mut rd =
            SyscallRecord { nr: 63, args: [0, 0x1100, 100, 0, 0, 0], ret: Some(2), ..Default::default() };
        rd.decode_exit(&m);
        assert_eq!(rd.data, b"ci");
        let mut c = SyscallRecord { nr: 203, args: [4, 0x1200, 16, 0, 0, 0], ..Default::default() };
        c.decode_entry(&m, &fdp);
        assert_eq!(c.sockaddr.as_deref(), Some("10.0.2.2:80"));
        let e = SyscallRecord {
            nr: 221,
            args: [0x1010, 0, 0, 0, 0, 0],
            diverted: Some(0x40_0000),
            ..Default::default()
        };
        assert!(e.line().ends_with("= ? (ritorno a 0x400000)"), "{}", e.line());
        assert_eq!(sockaddr(b"\x01\x00\0abc\0").as_deref(), Some("unix:@abc"));
    }

    #[test]
    fn transazione_binder_all_ingresso() {
        let mut img = vec![0u8; 0x3000];
        // bwr a 0x1000: write_size 68, consumed 0, write_buffer 0x1100.
        img[0..8].copy_from_slice(&68u64.to_le_bytes());
        img[16..24].copy_from_slice(&0x1100u64.to_le_bytes());
        let mut t = 0x4040_6300u32.to_le_bytes().to_vec();
        let mut d = vec![0u8; 64];
        d[0..8].copy_from_slice(&0u64.to_le_bytes());
        d[16..20].copy_from_slice(&2u32.to_le_bytes());
        d[32..40].copy_from_slice(&4u64.to_le_bytes());
        d[48..56].copy_from_slice(&0x1200u64.to_le_bytes());
        t.extend_from_slice(&d);
        img[0x100..0x100 + 68].copy_from_slice(&t);
        img[0x200..0x204].copy_from_slice(b"PARC");
        let m = mem(&img);
        let mut r =
            SyscallRecord { nr: 29, args: [3, BINDER_WRITE_READ, 0x1000, 0, 0, 0], ..Default::default() };
        r.decode_entry(&m, &|_| Some("/dev/binderfs/binder".into()));
        assert_eq!(r.binder.len(), 1);
        assert_eq!(r.binder[0].data, b"PARC");
        assert_eq!(r.binder[0].code, 2);
        assert!(r.line().contains("BC_TRANSACTION target=0x0 code=0x2"), "{}", r.line());
    }
}
