//! Il protocollo di `vetro-files` (ADR 0020, `docs/specs/files.md`),
//! versione [`VERSION`]: codifica delle richieste, lettura dei frame del
//! demone e dei loro corpi. Nessuna macchina qui: solo byte.
//!
//! Tutto in little endian. Un frame è `u32 lunghezza` (byte che seguono),
//! `u8 tipo`, `u32 id`, corpo. Stringhe: `u16 lunghezza` e byte (UTF-8 per
//! l'host); blocchi di byte: `u32 lunghezza` e byte.

use core::fmt;

/// Porta vsock del demone nel guest.
pub const PORT: u32 = 5200;
/// "VTRF" in little endian, nel saluto del demone.
pub const MAGIC: u32 = u32::from_le_bytes(*b"VTRF");
/// Versione del protocollo parlata da questo client.
pub const VERSION: u16 = 1;
/// Frame più lungo accettato dal demone (una lista enorme è un errore di
/// protocollo, non un'allocazione senza limiti).
pub const MAX_FRAME: usize = 64 << 20;
/// Pezzo usato dal client per letture e scritture (al più il `max_chunk`
/// del saluto).
pub const CHUNK: usize = 256 << 10;

/// Tipi dei frame.
pub mod ty {
    pub const STAT: u8 = 1;
    pub const LIST: u8 = 2;
    pub const READ: u8 = 3;
    pub const WOPEN: u8 = 4;
    pub const WDATA: u8 = 5;
    pub const WCOMMIT: u8 = 6;
    pub const WABORT: u8 = 7;
    pub const MKDIR: u8 = 8;
    pub const CREATE: u8 = 9;
    pub const DELETE: u8 = 10;
    pub const RENAME: u8 = 11;
    pub const WATCH: u8 = 12;
    pub const UNWATCH: u8 = 13;
    pub const HELLO: u8 = 0x80;
    pub const REPLY: u8 = 0x81;
    pub const EVENT: u8 = 0x82;
}

/// Bit degli eventi di inotify (`<sys/inotify.h>`), come arrivano in
/// [`Event::mask`].
pub mod mask {
    pub const ACCESS: u32 = 0x1;
    pub const MODIFY: u32 = 0x2;
    pub const ATTRIB: u32 = 0x4;
    pub const CLOSE_WRITE: u32 = 0x8;
    pub const MOVED_FROM: u32 = 0x40;
    pub const MOVED_TO: u32 = 0x80;
    pub const CREATE: u32 = 0x100;
    pub const DELETE: u32 = 0x200;
    pub const DELETE_SELF: u32 = 0x400;
    pub const MOVE_SELF: u32 = 0x800;
    pub const Q_OVERFLOW: u32 = 0x4000;
    pub const IGNORED: u32 = 0x8000;
    pub const ISDIR: u32 = 0x4000_0000;
}

/// Tipo di un file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Other,
    File,
    Dir,
    Symlink,
    Char,
    Block,
    Fifo,
    Socket,
}

impl Kind {
    fn from_u8(v: u8) -> Kind {
        match v {
            1 => Kind::File,
            2 => Kind::Dir,
            3 => Kind::Symlink,
            4 => Kind::Char,
            5 => Kind::Block,
            6 => Kind::Fifo,
            7 => Kind::Socket,
            _ => Kind::Other,
        }
    }

    pub fn code(self) -> u8 {
        match self {
            Kind::Other => 0,
            Kind::File => 1,
            Kind::Dir => 2,
            Kind::Symlink => 3,
            Kind::Char => 4,
            Kind::Block => 5,
            Kind::Fifo => 6,
            Kind::Socket => 7,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Other => "other",
            Kind::File => "file",
            Kind::Dir => "dir",
            Kind::Symlink => "symlink",
            Kind::Char => "char",
            Kind::Block => "block",
            Kind::Fifo => "fifo",
            Kind::Socket => "socket",
        }
    }
}

/// Metadati di un file (`lstat` nel guest).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    /// `st_mode` intero (tipo e permessi).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub mtime_s: i64,
    pub mtime_ns: u32,
    pub nlink: u32,
    /// Destinazione di un collegamento simbolico (vuota altrimenti).
    pub link: String,
    /// Contesto SELinux (xattr `security.selinux`), vuoto se non c'è.
    pub selinux: String,
}

/// Una voce di una cartella.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub stat: Stat,
}

/// Un evento di inotify su una cartella o un file osservato.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Id dell'osservazione (`inotify_add_watch`); `u32::MAX` per
    /// `IN_Q_OVERFLOW`.
    pub wd: u32,
    pub mask: u32,
    pub cookie: u32,
    /// Nome dentro la cartella osservata (vuoto per la cartella stessa).
    pub name: String,
}

/// Il saluto del demone, primo frame di ogni connessione.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub version: u16,
    /// Bit 0: SELinux attivo nel guest (`/sys/fs/selinux`).
    pub flags: u16,
    /// Byte al più per una lettura o una scrittura.
    pub max_chunk: u32,
}

/// Un frame del demone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Hello(Hello),
    /// Risposta alla richiesta `id`: `status` 0 o un errno del guest.
    Reply {
        id: u32,
        status: u32,
        body: Vec<u8>,
    },
    Event(Event),
}

/// Una richiesta dell'host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Stat {
        path: String,
    },
    List {
        path: String,
    },
    Read {
        path: String,
        offset: u64,
        len: u32,
    },
    /// Apre una scrittura atomica sul file temporaneo `handle` (scelto dal
    /// client). `mode`: permessi di un file nuovo; `excl`: fallisce se il
    /// file c'è già.
    WOpen {
        handle: u32,
        path: String,
        mode: u32,
        excl: bool,
    },
    WData {
        handle: u32,
        offset: u64,
        data: Vec<u8>,
    },
    /// Proprietario, modo e xattr, fsync, rename sul file vero.
    WCommit {
        handle: u32,
    },
    WAbort {
        handle: u32,
    },
    Mkdir {
        path: String,
        mode: u32,
    },
    Create {
        path: String,
        mode: u32,
    },
    Delete {
        path: String,
        recursive: bool,
    },
    Rename {
        from: String,
        to: String,
    },
    Watch {
        path: String,
    },
    Unwatch {
        wd: u32,
    },
}

/// Un errore di protocollo: il demone ha mandato byte che non tornano.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtoError(pub String);

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "protocollo di vetro-files: {}", self.0)
    }
}

// ---- Scrittura ---------------------------------------------------------------

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        let b = &s.as_bytes()[..s.len().min(0xffff)];
        self.u16(b.len() as u16);
        self.0.extend_from_slice(b);
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    /// Frame: lunghezza, tipo, id, poi il corpo scritto da `body`.
    fn frame(ty: u8, id: u32, body: impl FnOnce(&mut W)) -> Vec<u8> {
        let mut w = W(Vec::with_capacity(64));
        w.u32(0);
        w.u8(ty);
        w.u32(id);
        body(&mut w);
        let len = (w.0.len() - 4) as u32;
        w.0[..4].copy_from_slice(&len.to_le_bytes());
        w.0
    }
}

impl Request {
    /// Il tipo del frame.
    pub fn ty(&self) -> u8 {
        match self {
            Request::Stat { .. } => ty::STAT,
            Request::List { .. } => ty::LIST,
            Request::Read { .. } => ty::READ,
            Request::WOpen { .. } => ty::WOPEN,
            Request::WData { .. } => ty::WDATA,
            Request::WCommit { .. } => ty::WCOMMIT,
            Request::WAbort { .. } => ty::WABORT,
            Request::Mkdir { .. } => ty::MKDIR,
            Request::Create { .. } => ty::CREATE,
            Request::Delete { .. } => ty::DELETE,
            Request::Rename { .. } => ty::RENAME,
            Request::Watch { .. } => ty::WATCH,
            Request::Unwatch { .. } => ty::UNWATCH,
        }
    }

    /// Il frame della richiesta con l'id `id`.
    pub fn encode(&self, id: u32) -> Vec<u8> {
        W::frame(self.ty(), id, |w| match self {
            Request::Stat { path } | Request::List { path } | Request::Watch { path } => w.str(path),
            Request::Read { path, offset, len } => {
                w.str(path);
                w.u64(*offset);
                w.u32(*len);
            }
            Request::WOpen { handle, path, mode, excl } => {
                w.u32(*handle);
                w.str(path);
                w.u32(*mode);
                w.u8(u8::from(*excl));
            }
            Request::WData { handle, offset, data } => {
                w.u32(*handle);
                w.u64(*offset);
                w.bytes(data);
            }
            Request::WCommit { handle } | Request::WAbort { handle } => w.u32(*handle),
            Request::Mkdir { path, mode } | Request::Create { path, mode } => {
                w.str(path);
                w.u32(*mode);
            }
            Request::Delete { path, recursive } => {
                w.str(path);
                w.u8(u8::from(*recursive));
            }
            Request::Rename { from, to } => {
                w.str(from);
                w.str(to);
            }
            Request::Unwatch { wd } => w.u32(*wd),
        })
    }

    /// Legge una richiesta (tipo, id e corpo senza la lunghezza): serve ai
    /// demoni finti dei test.
    pub fn decode(frame: &[u8]) -> Result<(u32, Request), ProtoError> {
        let mut r = R::new(frame);
        let t = r.u8()?;
        let id = r.u32()?;
        let req = match t {
            ty::STAT => Request::Stat { path: r.str()? },
            ty::LIST => Request::List { path: r.str()? },
            ty::WATCH => Request::Watch { path: r.str()? },
            ty::READ => Request::Read { path: r.str()?, offset: r.u64()?, len: r.u32()? },
            ty::WOPEN => {
                Request::WOpen { handle: r.u32()?, path: r.str()?, mode: r.u32()?, excl: r.u8()? & 1 != 0 }
            }
            ty::WDATA => Request::WData { handle: r.u32()?, offset: r.u64()?, data: r.bytes()?.to_vec() },
            ty::WCOMMIT => Request::WCommit { handle: r.u32()? },
            ty::WABORT => Request::WAbort { handle: r.u32()? },
            ty::MKDIR => Request::Mkdir { path: r.str()?, mode: r.u32()? },
            ty::CREATE => Request::Create { path: r.str()?, mode: r.u32()? },
            ty::DELETE => Request::Delete { path: r.str()?, recursive: r.u8()? & 1 != 0 },
            ty::RENAME => Request::Rename { from: r.str()?, to: r.str()? },
            ty::UNWATCH => Request::Unwatch { wd: r.u32()? },
            t => return Err(ProtoError(format!("richiesta di tipo {t}"))),
        };
        r.end()?;
        Ok((id, req))
    }
}

/// Codifica di un saluto (per i demoni finti dei test).
pub fn encode_hello(h: &Hello) -> Vec<u8> {
    W::frame(ty::HELLO, 0, |w| {
        w.u32(MAGIC);
        w.u16(h.version);
        w.u16(h.flags);
        w.u32(h.max_chunk);
    })
}

/// Codifica di una risposta (per i demoni finti dei test).
pub fn encode_reply(id: u32, status: u32, body: &[u8]) -> Vec<u8> {
    W::frame(ty::REPLY, id, |w| {
        w.u32(status);
        w.0.extend_from_slice(body);
    })
}

/// Codifica di un evento (per i demoni finti dei test).
pub fn encode_event(e: &Event) -> Vec<u8> {
    W::frame(ty::EVENT, 0, |w| {
        w.u32(e.wd);
        w.u32(e.mask);
        w.u32(e.cookie);
        w.str(&e.name);
    })
}

/// Corpo con un [`Stat`] (per i demoni finti dei test).
pub fn encode_stat(s: &Stat) -> Vec<u8> {
    let mut w = W(Vec::new());
    put_stat(&mut w, s);
    w.0
}

/// Corpo di una lista (per i demoni finti dei test).
pub fn encode_list(entries: &[Entry]) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(entries.len() as u32);
    for e in entries {
        w.str(&e.name);
        put_stat(&mut w, &e.stat);
    }
    w.0
}

/// Corpo di una lettura (per i demoni finti dei test).
pub fn encode_read(size: u64, data: &[u8]) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u64(size);
    w.bytes(data);
    w.0
}

fn put_stat(w: &mut W, s: &Stat) {
    w.u8(s.kind.code());
    w.u32(s.mode);
    w.u32(s.uid);
    w.u32(s.gid);
    w.u64(s.size);
    w.u64(s.mtime_s as u64);
    w.u32(s.mtime_ns);
    w.u32(s.nlink);
    w.str(&s.link);
    w.str(&s.selinux);
}

// ---- Lettura -----------------------------------------------------------------

struct R<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> R<'a> {
    fn new(b: &'a [u8]) -> Self {
        R { b, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if self.b.len() - self.at < n {
            return Err(ProtoError(format!(
                "frame corto ({} byte, ne servono {})",
                self.b.len(),
                self.at + n
            )));
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ProtoError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, ProtoError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, ProtoError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn str(&mut self) -> Result<String, ProtoError> {
        let n = self.u16()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    fn bytes(&mut self) -> Result<&'a [u8], ProtoError> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn end(&self) -> Result<(), ProtoError> {
        if self.at != self.b.len() {
            return Err(ProtoError(format!("{} byte in più nel frame", self.b.len() - self.at)));
        }
        Ok(())
    }
    fn stat(&mut self) -> Result<Stat, ProtoError> {
        Ok(Stat {
            kind: Kind::from_u8(self.u8()?),
            mode: self.u32()?,
            uid: self.u32()?,
            gid: self.u32()?,
            size: self.u64()?,
            mtime_s: self.u64()? as i64,
            mtime_ns: self.u32()?,
            nlink: self.u32()?,
            link: self.str()?,
            selinux: self.str()?,
        })
    }
}

/// Il corpo di una risposta a STAT o WCOMMIT.
pub fn parse_stat(body: &[u8]) -> Result<Stat, ProtoError> {
    let mut r = R::new(body);
    let s = r.stat()?;
    r.end()?;
    Ok(s)
}

/// Il corpo di una risposta a LIST.
pub fn parse_list(body: &[u8]) -> Result<Vec<Entry>, ProtoError> {
    let mut r = R::new(body);
    let n = r.u32()? as usize;
    // Una voce occupa almeno 45 byte: un conteggio impossibile è un errore,
    // non un'allocazione enorme.
    if n > body.len() / 45 {
        return Err(ProtoError(format!("lista di {n} voci in {} byte", body.len())));
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(Entry { name: r.str()?, stat: r.stat()? });
    }
    r.end()?;
    Ok(out)
}

/// Il corpo di una risposta a READ: dimensione del file e byte letti.
pub fn parse_read(body: &[u8]) -> Result<(u64, Vec<u8>), ProtoError> {
    let mut r = R::new(body);
    let size = r.u64()?;
    let data = r.bytes()?.to_vec();
    r.end()?;
    Ok((size, data))
}

/// Il corpo di una risposta a WATCH: l'id dell'osservazione.
pub fn parse_watch(body: &[u8]) -> Result<u32, ProtoError> {
    let mut r = R::new(body);
    let wd = r.u32()?;
    r.end()?;
    Ok(wd)
}

/// Accumula i byte del demone e ne estrae i frame completi.
#[derive(Clone, Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    at: usize,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        if self.at > 0 && self.at == self.buf.len() {
            self.buf.clear();
            self.at = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Byte ricevuti e non ancora consumati.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.at
    }

    /// Il prossimo frame completo, se c'è.
    pub fn next_frame(&mut self) -> Option<Result<Frame, ProtoError>> {
        let rest = &self.buf[self.at..];
        if rest.len() < 4 {
            return None;
        }
        let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        if !(5..=MAX_FRAME).contains(&len) {
            return Some(Err(ProtoError(format!("frame lungo {len} byte"))));
        }
        if rest.len() < 4 + len {
            return None;
        }
        let frame = &rest[4..4 + len];
        let out = Self::parse(frame);
        self.at += 4 + len;
        if self.at > 1 << 20 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        Some(out)
    }

    fn parse(frame: &[u8]) -> Result<Frame, ProtoError> {
        let mut r = R::new(frame);
        let t = r.u8()?;
        let id = r.u32()?;
        match t {
            ty::HELLO => {
                let magic = r.u32()?;
                if magic != MAGIC {
                    return Err(ProtoError(format!("saluto con magia {magic:#x}")));
                }
                let h = Hello { version: r.u16()?, flags: r.u16()?, max_chunk: r.u32()? };
                r.end()?;
                Ok(Frame::Hello(h))
            }
            ty::REPLY => {
                let status = r.u32()?;
                Ok(Frame::Reply { id, status, body: frame[r.at..].to_vec() })
            }
            ty::EVENT => {
                let e = Event { wd: r.u32()?, mask: r.u32()?, cookie: r.u32()?, name: r.str()? };
                r.end()?;
                Ok(Frame::Event(e))
            }
            t => Err(ProtoError(format!("frame di tipo {t:#x}"))),
        }
    }
}

/// Nome simbolico di un errno di Linux (arm64 usa i numeri generici).
pub fn errno_name(e: u32) -> &'static str {
    match e {
        1 => "EPERM",
        2 => "ENOENT",
        5 => "EIO",
        9 => "EBADF",
        12 => "ENOMEM",
        13 => "EACCES",
        16 => "EBUSY",
        17 => "EEXIST",
        18 => "EXDEV",
        20 => "ENOTDIR",
        21 => "EISDIR",
        22 => "EINVAL",
        24 => "EMFILE",
        27 => "EFBIG",
        28 => "ENOSPC",
        30 => "EROFS",
        36 => "ENAMETOOLONG",
        38 => "ENOSYS",
        39 => "ENOTEMPTY",
        40 => "ELOOP",
        61 => "ENODATA",
        71 => "EPROTO",
        90 => "EMSGSIZE",
        95 => "EOPNOTSUPP",
        _ => "E?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(kind: Kind, size: u64) -> Stat {
        Stat {
            kind,
            mode: 0o100644,
            uid: 10057,
            gid: 10057,
            size,
            mtime_s: -3,
            mtime_ns: 999_999_999,
            nlink: 1,
            link: String::new(),
            selinux: "u:object_r:app_data_file:s0:c57,c256,c512,c768".into(),
        }
    }

    /// Ogni richiesta si rilegge uguale, e il frame ha la lunghezza giusta.
    #[test]
    fn richieste_andata_e_ritorno() {
        let reqs = [
            Request::Stat { path: "/data/data/org.example".into() },
            Request::List { path: "/".into() },
            Request::Read { path: "/tmp/à".into(), offset: 1 << 40, len: 262144 },
            Request::WOpen { handle: 7, path: "/tmp/x".into(), mode: 0o600, excl: true },
            Request::WData { handle: 7, offset: 3, data: vec![0, 1, 2, 255] },
            Request::WCommit { handle: 7 },
            Request::WAbort { handle: 8 },
            Request::Mkdir { path: "/tmp/d".into(), mode: 0o755 },
            Request::Create { path: "/tmp/e".into(), mode: 0o644 },
            Request::Delete { path: "/tmp/d".into(), recursive: true },
            Request::Rename { from: "/tmp/a".into(), to: "/tmp/b".into() },
            Request::Watch { path: "/tmp".into() },
            Request::Unwatch { wd: 3 },
        ];
        for (i, r) in reqs.iter().enumerate() {
            let f = r.encode(i as u32 + 100);
            assert_eq!(u32::from_le_bytes(f[..4].try_into().unwrap()) as usize, f.len() - 4);
            assert_eq!(f[4], r.ty());
            assert_eq!(Request::decode(&f[4..]).unwrap(), (i as u32 + 100, r.clone()));
        }
        // Byte esatti di una richiesta: il formato è il contratto con il C.
        assert_eq!(
            Request::Read { path: "/a".into(), offset: 2, len: 3 }.encode(9),
            [21, 0, 0, 0, 3, 9, 0, 0, 0, 2, 0, b'/', b'a', 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0]
        );
    }

    /// I frame del demone arrivano a pezzi qualsiasi (anche un byte alla
    /// volta) e si ricompongono.
    #[test]
    fn frame_a_pezzi() {
        let hello = Hello { version: VERSION, flags: 1, max_chunk: 1 << 20 };
        let entries = vec![
            Entry { name: "shared_prefs".into(), stat: stat(Kind::Dir, 4096) },
            Entry { name: "db".into(), stat: Stat { link: "/x".into(), ..stat(Kind::Symlink, 2) } },
        ];
        let ev = Event { wd: 1, mask: mask::CREATE | mask::ISDIR, cookie: 0, name: "nuovo".into() };
        let mut all = encode_hello(&hello);
        all.extend(encode_reply(5, 0, &encode_list(&entries)));
        all.extend(encode_event(&ev));
        all.extend(encode_reply(6, 2, &[]));
        all.extend(encode_reply(7, 0, &encode_read(10, b"ciao")));
        let mut d = Decoder::default();
        let mut frames = Vec::new();
        for b in &all {
            d.push(&[*b]);
            while let Some(f) = d.next_frame() {
                frames.push(f.unwrap());
            }
        }
        assert_eq!(d.pending(), 0);
        assert_eq!(frames.len(), 5);
        assert_eq!(frames[0], Frame::Hello(hello));
        let Frame::Reply { id: 5, status: 0, body } = &frames[1] else { panic!("{:?}", frames[1]) };
        assert_eq!(parse_list(body).unwrap(), entries);
        assert_eq!(frames[2], Frame::Event(ev));
        assert_eq!(frames[3], Frame::Reply { id: 6, status: 2, body: vec![] });
        let Frame::Reply { body, .. } = &frames[4] else { panic!() };
        assert_eq!(parse_read(body).unwrap(), (10, b"ciao".to_vec()));
        assert_eq!(parse_stat(&encode_stat(&entries[0].stat)).unwrap(), entries[0].stat);
    }

    #[test]
    fn frame_rovinati() {
        let mut d = Decoder::default();
        d.push(&[4, 0, 0, 0, 0x81, 0, 0, 0]);
        assert!(d.next_frame().unwrap().is_err(), "lunghezza < 5");
        let mut d = Decoder::default();
        d.push(&encode_reply(1, 0, &[]).iter().map(|_| 0xffu8).collect::<Vec<_>>());
        assert!(d.next_frame().unwrap().is_err(), "lunghezza enorme");
        let mut bad = encode_hello(&Hello { version: 1, flags: 0, max_chunk: 1 });
        bad[9] ^= 1;
        let mut d = Decoder::default();
        d.push(&bad);
        assert!(d.next_frame().unwrap().is_err(), "magia sbagliata");
        assert!(parse_list(&[255, 255, 255, 255]).is_err(), "conteggio impossibile");
        assert!(parse_read(&encode_read(1, b"x")[..11]).is_err(), "corpo corto");
        assert!(parse_watch(&[1, 0, 0, 0, 0]).is_err(), "byte in più");
        assert_eq!(errno_name(2), "ENOENT");
    }
}
