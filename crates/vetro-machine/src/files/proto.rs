//! The `vetro-files` protocol (ADR 0020, `docs/specs/files.md`),
//! version [`VERSION`]: request encoding, reading the daemon's frames
//! and their bodies. No machine here: only bytes.
//!
//! Everything in little endian. A frame is `u32 length` (bytes that follow),
//! `u8 type`, `u32 id`, body. Strings: `u16 length` and bytes; byte
//! blocks: `u32 length` and bytes. Paths, names and link
//! targets are bytes of the guest file system (`Vec<u8>`), possibly non-
//! UTF-8 (ADR 0021): [`display_name`] shows them.

use core::fmt;

/// vsock port of the daemon in the guest.
pub const PORT: u32 = 5200;
/// "VTRF" in little endian, in the daemon's greeting.
pub const MAGIC: u32 = u32::from_le_bytes(*b"VTRF");
/// Protocol version spoken by this client (2: SQL request,
/// ADR 0021).
pub const VERSION: u16 = 2;
/// Oldest accepted version (without SQL: the daemon answers `ENOSYS`).
pub const MIN_VERSION: u16 = 1;
/// Longest frame accepted from the daemon (a huge list is a protocol
/// error, not an unbounded allocation).
pub const MAX_FRAME: usize = 64 << 20;
/// Chunk used by the client for reads and writes (at most the greeting's
/// `max_chunk`).
pub const CHUNK: usize = 256 << 10;

/// Frame types.
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
    pub const SQL: u8 = 14;
    pub const HELLO: u8 = 0x80;
    pub const REPLY: u8 = 0x81;
    pub const EVENT: u8 = 0x82;
}

/// inotify event bits (`<sys/inotify.h>`), as they arrive in
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

/// Type of a file.
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

/// Metadata of a file (`lstat` in the guest).
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
    /// Target of a symbolic link (empty otherwise).
    pub link: Vec<u8>,
    /// SELinux context (xattr `security.selinux`), empty if absent.
    pub selinux: String,
}

/// An entry of a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: Vec<u8>,
    pub stat: Stat,
}

/// An inotify event on a watched folder or file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Id of the watch (`inotify_add_watch`); `u32::MAX` for
    /// `IN_Q_OVERFLOW`.
    pub wd: u32,
    pub mask: u32,
    pub cookie: u32,
    /// Name inside the watched folder (empty for the folder itself).
    pub name: Vec<u8>,
}

/// The daemon's greeting, first frame of every connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub version: u16,
    /// Bit 0: SELinux enabled in the guest (`/sys/fs/selinux`).
    pub flags: u16,
    /// Maximum bytes for one read or one write.
    pub max_chunk: u32,
}

/// A daemon frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Hello(Hello),
    /// Response to request `id`: `status` 0 or a guest errno.
    Reply {
        id: u32,
        status: u32,
        body: Vec<u8>,
    },
    Event(Event),
}

/// A SQLite value (bound parameter or column of a row).
#[derive(Clone, Debug, PartialEq)]
pub enum SqlValue {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// Value codes in the protocol.
mod vty {
    pub const NULL: u8 = 0;
    pub const INT: u8 = 1;
    pub const REAL: u8 = 2;
    pub const TEXT: u8 = 3;
    pub const BLOB: u8 = 4;
}

/// The outcome of a successful SQL request.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SqlResult {
    /// Rows changed directly by the statements (not by triggers).
    pub changes: u64,
    /// `sqlite3_last_insert_rowid` at the end.
    pub last_rowid: i64,
    /// Rows over the daemon's limit (10000 or 16 MiB), not returned.
    pub truncated: bool,
    /// Columns and rows of the last statement that returns any.
    pub columns: Vec<String>,
    pub rows: Vec<Vec<SqlValue>>,
}

/// A request from the host.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Stat {
        path: Vec<u8>,
    },
    List {
        path: Vec<u8>,
    },
    Read {
        path: Vec<u8>,
        offset: u64,
        len: u32,
    },
    /// Opens an atomic write on the temporary file `handle` (chosen by the
    /// client). `mode`: permissions of a new file; `excl`: fails if the
    /// file already exists.
    WOpen {
        handle: u32,
        path: Vec<u8>,
        mode: u32,
        excl: bool,
    },
    WData {
        handle: u32,
        offset: u64,
        data: Vec<u8>,
    },
    /// Owner, mode and xattrs, fsync, rename onto the real file.
    WCommit {
        handle: u32,
    },
    WAbort {
        handle: u32,
    },
    Mkdir {
        path: Vec<u8>,
        mode: u32,
    },
    Create {
        path: Vec<u8>,
        mode: u32,
    },
    Delete {
        path: Vec<u8>,
        recursive: bool,
    },
    Rename {
        from: Vec<u8>,
        to: Vec<u8>,
    },
    Watch {
        path: Vec<u8>,
    },
    Unwatch {
        wd: u32,
    },
    /// SQL statements on the database `path`, in the guest with SQLite (ADR 0021):
    /// in one transaction (except `readonly`), `?N` parameters bound by
    /// position; with `expect`, a different number of changed rows rolls
    /// everything back.
    Sql {
        path: Vec<u8>,
        sql: String,
        params: Vec<SqlValue>,
        expect: Option<u32>,
        readonly: bool,
    },
}

/// A protocol error: the daemon sent bytes that don't add up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtoError(pub String);

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "vetro-files protocol: {}", self.0)
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
    fn str(&mut self, s: &[u8]) {
        let b = &s[..s.len().min(0xffff)];
        self.u16(b.len() as u16);
        self.0.extend_from_slice(b);
    }
    fn value(&mut self, v: &SqlValue) {
        match v {
            SqlValue::Null => self.u8(vty::NULL),
            SqlValue::Int(i) => {
                self.u8(vty::INT);
                self.u64(*i as u64);
            }
            SqlValue::Real(f) => {
                self.u8(vty::REAL);
                self.u64(f.to_bits());
            }
            SqlValue::Text(t) => {
                self.u8(vty::TEXT);
                self.bytes(t.as_bytes());
            }
            SqlValue::Blob(b) => {
                self.u8(vty::BLOB);
                self.bytes(b);
            }
        }
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    /// Frame: length, type, id, then the body written by `body`.
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
    /// The frame type.
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
            Request::Sql { .. } => ty::SQL,
        }
    }

    /// The frame of the request with id `id`.
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
            Request::Sql { path, sql, params, expect, readonly } => {
                w.str(path);
                w.u8(u8::from(*readonly));
                w.u32(expect.unwrap_or(u32::MAX));
                w.bytes(sql.as_bytes());
                w.u16(params.len().min(0xffff) as u16);
                for v in params.iter().take(0xffff) {
                    w.value(v);
                }
            }
        })
    }

    /// Reads a request (type, id and body without the length): used by the
    /// fake daemons of the tests.
    pub fn decode(frame: &[u8]) -> Result<(u32, Request), ProtoError> {
        let mut r = R::new(frame);
        let t = r.u8()?;
        let id = r.u32()?;
        let req = match t {
            ty::STAT => Request::Stat { path: r.name()? },
            ty::LIST => Request::List { path: r.name()? },
            ty::WATCH => Request::Watch { path: r.name()? },
            ty::READ => Request::Read { path: r.name()?, offset: r.u64()?, len: r.u32()? },
            ty::WOPEN => {
                Request::WOpen { handle: r.u32()?, path: r.name()?, mode: r.u32()?, excl: r.u8()? & 1 != 0 }
            }
            ty::WDATA => Request::WData { handle: r.u32()?, offset: r.u64()?, data: r.bytes()?.to_vec() },
            ty::WCOMMIT => Request::WCommit { handle: r.u32()? },
            ty::WABORT => Request::WAbort { handle: r.u32()? },
            ty::MKDIR => Request::Mkdir { path: r.name()?, mode: r.u32()? },
            ty::CREATE => Request::Create { path: r.name()?, mode: r.u32()? },
            ty::DELETE => Request::Delete { path: r.name()?, recursive: r.u8()? & 1 != 0 },
            ty::RENAME => Request::Rename { from: r.name()?, to: r.name()? },
            ty::UNWATCH => Request::Unwatch { wd: r.u32()? },
            ty::SQL => {
                let path = r.name()?;
                let readonly = r.u8()? & 1 != 0;
                let expect = Some(r.u32()?).filter(|&e| e != u32::MAX);
                let sql = String::from_utf8_lossy(r.bytes()?).into_owned();
                let n = r.u16()?;
                let params = (0..n).map(|_| r.value()).collect::<Result<_, _>>()?;
                Request::Sql { path, sql, params, expect, readonly }
            }
            t => return Err(ProtoError(format!("request of type {t}"))),
        };
        r.end()?;
        Ok((id, req))
    }
}

/// Encoding of a greeting (for the fake daemons of the tests).
pub fn encode_hello(h: &Hello) -> Vec<u8> {
    W::frame(ty::HELLO, 0, |w| {
        w.u32(MAGIC);
        w.u16(h.version);
        w.u16(h.flags);
        w.u32(h.max_chunk);
    })
}

/// Encoding of a response (for the fake daemons of the tests).
pub fn encode_reply(id: u32, status: u32, body: &[u8]) -> Vec<u8> {
    W::frame(ty::REPLY, id, |w| {
        w.u32(status);
        w.0.extend_from_slice(body);
    })
}

/// Encoding of an event (for the fake daemons of the tests).
pub fn encode_event(e: &Event) -> Vec<u8> {
    W::frame(ty::EVENT, 0, |w| {
        w.u32(e.wd);
        w.u32(e.mask);
        w.u32(e.cookie);
        w.str(&e.name);
    })
}

/// Body with a [`Stat`] (for the fake daemons of the tests).
pub fn encode_stat(s: &Stat) -> Vec<u8> {
    let mut w = W(Vec::new());
    put_stat(&mut w, s);
    w.0
}

/// Body of a list (for the fake daemons of the tests).
pub fn encode_list(entries: &[Entry]) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(entries.len() as u32);
    for e in entries {
        w.str(&e.name);
        put_stat(&mut w, &e.stat);
    }
    w.0
}

/// Body of the response to a successful SQL request (for the fake daemons
/// of the tests).
pub fn encode_sql_ok(res: &SqlResult) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(0);
    w.str(b"");
    w.u64(res.changes);
    w.u64(res.last_rowid as u64);
    w.u8(u8::from(res.truncated));
    w.u16(res.columns.len() as u16);
    for c in &res.columns {
        w.str(c.as_bytes());
    }
    w.u32(res.rows.len() as u32);
    for r in &res.rows {
        for v in r {
            w.value(v);
        }
    }
    w.0
}

/// Body of the response to a SQL request that failed in SQLite (for the
/// fake daemons of the tests).
pub fn encode_sql_err(code: u32, message: &str) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(code);
    w.str(message.as_bytes());
    w.0
}

/// Body of a read (for the fake daemons of the tests).
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
    w.str(s.selinux.as_bytes());
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
            return Err(ProtoError(format!("short frame ({} bytes, {} needed)", self.b.len(), self.at + n)));
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
    /// A string as bytes (guest paths and names).
    fn name(&mut self) -> Result<Vec<u8>, ProtoError> {
        let n = self.u16()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn value(&mut self) -> Result<SqlValue, ProtoError> {
        Ok(match self.u8()? {
            vty::NULL => SqlValue::Null,
            vty::INT => SqlValue::Int(self.u64()? as i64),
            vty::REAL => SqlValue::Real(f64::from_bits(self.u64()?)),
            vty::TEXT => SqlValue::Text(String::from_utf8_lossy(self.bytes()?).into_owned()),
            vty::BLOB => SqlValue::Blob(self.bytes()?.to_vec()),
            t => return Err(ProtoError(format!("SQL value of type {t}"))),
        })
    }
    fn bytes(&mut self) -> Result<&'a [u8], ProtoError> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn end(&self) -> Result<(), ProtoError> {
        if self.at != self.b.len() {
            return Err(ProtoError(format!("{} extra bytes in the frame", self.b.len() - self.at)));
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
            link: self.name()?,
            selinux: self.str()?,
        })
    }
}

/// The body of a response to STAT or WCOMMIT.
pub fn parse_stat(body: &[u8]) -> Result<Stat, ProtoError> {
    let mut r = R::new(body);
    let s = r.stat()?;
    r.end()?;
    Ok(s)
}

/// The body of a response to LIST.
pub fn parse_list(body: &[u8]) -> Result<Vec<Entry>, ProtoError> {
    let mut r = R::new(body);
    let n = r.u32()? as usize;
    // An entry takes at least 45 bytes: an impossible count is an error,
    // not a huge allocation.
    if n > body.len() / 45 {
        return Err(ProtoError(format!("list of {n} entries in {} bytes", body.len())));
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(Entry { name: r.name()?, stat: r.stat()? });
    }
    r.end()?;
    Ok(out)
}

/// The body of a response to READ: file size and bytes read.
pub fn parse_read(body: &[u8]) -> Result<(u64, Vec<u8>), ProtoError> {
    let mut r = R::new(body);
    let size = r.u64()?;
    let data = r.bytes()?.to_vec();
    r.end()?;
    Ok((size, data))
}

/// The body of a response to SQL: the outcome, or the SQLite code and its
/// message if SQLite refused (nothing changed).
pub fn parse_sql(body: &[u8]) -> Result<Result<SqlResult, (u32, String)>, ProtoError> {
    let mut r = R::new(body);
    let code = r.u32()?;
    let message = r.str()?;
    if code != 0 {
        r.end()?;
        return Ok(Err((code, message)));
    }
    let changes = r.u64()?;
    let last_rowid = r.u64()? as i64;
    let truncated = r.u8()? != 0;
    let ncols = r.u16()? as usize;
    let columns = (0..ncols).map(|_| r.str()).collect::<Result<Vec<_>, _>>()?;
    let nrows = r.u32()? as usize;
    // A value takes at least one byte: an impossible count is an error.
    if ncols > 0 && nrows > body.len() / ncols {
        return Err(ProtoError(format!("{nrows} rows of {ncols} columns in {} bytes", body.len())));
    }
    let mut rows = Vec::with_capacity(if ncols > 0 { nrows } else { 0 });
    for _ in 0..nrows {
        rows.push((0..ncols).map(|_| r.value()).collect::<Result<Vec<_>, _>>()?);
    }
    r.end()?;
    Ok(Ok(SqlResult { changes, last_rowid, truncated, columns, rows }))
}

/// SQL and parameters in the protocol format (`bytes` SQL, `u16` number
/// of parameters, values): the format in which the JS passes them to vetro-wasm.
pub fn encode_sql_args(sql: &str, params: &[SqlValue]) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.bytes(sql.as_bytes());
    w.u16(params.len().min(0xffff) as u16);
    for v in params.iter().take(0xffff) {
        w.value(v);
    }
    w.0
}

/// The inverse of [`encode_sql_args`].
pub fn decode_sql_args(b: &[u8]) -> Result<(String, Vec<SqlValue>), ProtoError> {
    let mut r = R::new(b);
    let sql = core::str::from_utf8(r.bytes()?).map_err(|_| ProtoError("non-UTF-8 SQL".into()))?.to_string();
    let n = r.u16()?;
    let params = (0..n).map(|_| r.value()).collect::<Result<_, _>>()?;
    r.end()?;
    Ok((sql, params))
}

/// A guest name or path to show: UTF-8 as is, every byte that
/// is not part of valid UTF-8 as `\xNN`.
pub fn display_name(b: &[u8]) -> String {
    let mut out = String::new();
    for chunk in b.utf8_chunks() {
        out.push_str(chunk.valid());
        for x in chunk.invalid() {
            out.push_str(&format!("\\x{x:02x}"));
        }
    }
    out
}

/// The body of a response to WATCH: the id of the watch.
pub fn parse_watch(body: &[u8]) -> Result<u32, ProtoError> {
    let mut r = R::new(body);
    let wd = r.u32()?;
    r.end()?;
    Ok(wd)
}

/// Accumulates the daemon's bytes and extracts the complete frames.
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

    /// Bytes received and not consumed yet.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.at
    }

    /// The next complete frame, if any.
    pub fn next_frame(&mut self) -> Option<Result<Frame, ProtoError>> {
        let rest = &self.buf[self.at..];
        if rest.len() < 4 {
            return None;
        }
        let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        if !(5..=MAX_FRAME).contains(&len) {
            return Some(Err(ProtoError(format!("frame {len} bytes long"))));
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
                    return Err(ProtoError(format!("greeting with magic {magic:#x}")));
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
                let e = Event { wd: r.u32()?, mask: r.u32()?, cookie: r.u32()?, name: r.name()? };
                r.end()?;
                Ok(Frame::Event(e))
            }
            t => Err(ProtoError(format!("frame of type {t:#x}"))),
        }
    }
}

/// Symbolic name of a Linux errno (arm64 uses the generic numbers).
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
            link: Vec::new(),
            selinux: "u:object_r:app_data_file:s0:c57,c256,c512,c768".into(),
        }
    }

    /// Every request reads back the same, and the frame has the right length.
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
            Request::Rename { from: b"/tmp/\xff".to_vec(), to: b"/tmp/\xfe\x80".to_vec() },
            Request::Sql {
                path: "/data/data/org.example/databases/a.db".into(),
                sql: "UPDATE t SET a = ?1, b = ?2, c = ?3, d = ?4 WHERE rowid = ?5".into(),
                params: vec![
                    SqlValue::Null,
                    SqlValue::Real(1.5),
                    SqlValue::Text("è".into()),
                    SqlValue::Blob(vec![0, 255]),
                    SqlValue::Int(-2),
                ],
                expect: Some(1),
                readonly: false,
            },
            Request::Sql {
                path: "/a".into(),
                sql: String::new(),
                params: vec![],
                expect: None,
                readonly: true,
            },
        ];
        for (i, r) in reqs.iter().enumerate() {
            let f = r.encode(i as u32 + 100);
            assert_eq!(u32::from_le_bytes(f[..4].try_into().unwrap()) as usize, f.len() - 4);
            assert_eq!(f[4], r.ty());
            assert_eq!(Request::decode(&f[4..]).unwrap(), (i as u32 + 100, r.clone()));
        }
        // Exact bytes of a request: the format is the contract with the C code.
        assert_eq!(
            Request::Read { path: "/a".into(), offset: 2, len: 3 }.encode(9),
            [21, 0, 0, 0, 3, 9, 0, 0, 0, 2, 0, b'/', b'a', 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0]
        );
        assert_eq!(
            Request::Sql {
                path: "/d".into(),
                sql: "S".into(),
                params: vec![SqlValue::Int(1), SqlValue::Text("x".into())],
                expect: None,
                readonly: true
            }
            .encode(4),
            [
                36, 0, 0, 0, 14, 4, 0, 0, 0, 2, 0, b'/', b'd', 1, 255, 255, 255, 255, 1, 0, 0, 0, b'S', 2, 0,
                1, 1, 0, 0, 0, 0, 0, 0, 0, 3, 1, 0, 0, 0, b'x'
            ]
        );
    }

    /// The daemon's frames arrive in arbitrary pieces (even one byte at a
    /// time) and are reassembled.
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
        assert!(parse_watch(&[1, 0, 0, 0, 0]).is_err(), "extra bytes");
        assert_eq!(errno_name(2), "ENOENT");
        assert!(parse_sql(&encode_sql_err(1, "x")[..6]).is_err(), "messaggio corto");
        let mut ok = encode_sql_ok(&SqlResult {
            columns: vec!["a".into()],
            rows: vec![vec![SqlValue::Int(1)]],
            ..SqlResult::default()
        });
        let n = ok.len();
        ok[n - 9] = 9;
        assert!(parse_sql(&ok).is_err(), "unknown value type");
    }

    /// SQL responses round trip, and names shown with `\xNN`.
    #[test]
    fn sql_e_nomi() {
        let res = SqlResult {
            changes: 3,
            last_rowid: -1,
            truncated: true,
            columns: vec!["id".into(), "è".into()],
            rows: vec![
                vec![SqlValue::Int(i64::MAX), SqlValue::Real(f64::INFINITY)],
                vec![SqlValue::Text(String::new()), SqlValue::Blob(vec![])],
            ],
        };
        assert_eq!(parse_sql(&encode_sql_ok(&res)).unwrap(), Ok(res));
        assert_eq!(
            parse_sql(&encode_sql_err(5, "database is locked")).unwrap(),
            Err((5, "database is locked".into()))
        );
        assert_eq!(display_name(b"a\xffb\xe2\x82"), "a\\xffb\\xe2\\x82");
        assert_eq!(display_name("città".as_bytes()), "città");
        let params = vec![SqlValue::Null, SqlValue::Int(7), SqlValue::Blob(vec![9])];
        let args = encode_sql_args("SELECT ?1", &params);
        assert_eq!(decode_sql_args(&args).unwrap(), ("SELECT ?1".to_string(), params));
        assert!(decode_sql_args(&args[..args.len() - 1]).is_err());
        assert!(decode_sql_args(&[1, 0, 0, 0, 0xff, 0, 0]).is_err(), "non-UTF-8 SQL");
    }
}
