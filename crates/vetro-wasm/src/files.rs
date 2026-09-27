//! The file manager (ABI 7, M8, ADR 0020; SQL and names as bytes with
//! ABI 9, ADR 0021): the client of
//! `vetro_machine::files` towards the guest's `vetro-files` daemon, over
//! virtio-vsock (`VSOCK` bit of `vetro_machine_new_with`).
//!
//! JS requests operations ([`vetro_files_request`], id > 0), advances the
//! client between one quantum and the next ([`vetro_files_pump`], like the console) and
//! reads the ready messages one at a time ([`vetro_files_take`] and
//! [`vetro_files_ptr`]): `u32` length of the JSON, the JSON (UTF-8), then the
//! bytes read by a read. The JSON is a response
//! (`{"kind":"reply","op":N,"ok":true,"type":...}`) or an inotify event
//! (`{"kind":"event","wd":N,"mask":N,"cookie":N,"name":"..."}`); the format
//! is in `docs/specs/wasm.md`.
//!
//! Paths and names are guest bytes: JS passes them as bytes, and in the
//! JSON a byte that is not part of valid UTF-8 becomes the lone
//! surrogate `\udcXX` (*surrogateescape*, ADR 0021).
//!
//! Connection, bytes sent and bytes read go through `Machine::input`:
//! they are inputs, recorded for replay (ADR 0019). State and messages
//! already ready don't touch the machine.

use vetro_machine::FilesClient;
use vetro_machine::files::proto::{self, SqlResult, SqlValue, Stat};
use vetro_machine::files::{Completion, FilesError, LinkState, Outcome};

use crate::Vm;

/// Operations of [`vetro_files_request`].
pub mod op {
    pub const STAT: u32 = 1;
    pub const LIST: u32 = 2;
    /// `x` = offset, `y` = bytes (`u64::MAX` = to the end).
    pub const READ: u32 = 3;
    /// `b` = contents, `x` = permissions if the file is new.
    pub const WRITE: u32 = 4;
    /// `x` = permessi.
    pub const MKDIR: u32 = 5;
    /// `x` = permessi.
    pub const CREATE: u32 = 6;
    /// `x` = 1 to delete a folder with all its contents.
    pub const DELETE: u32 = 7;
    /// `b` = nuovo percorso (UTF-8).
    pub const RENAME: u32 = 8;
    pub const WATCH: u32 = 9;
    /// `x` = id of the watch.
    pub const UNWATCH: u32 = 10;
    /// SQL on database `a` (ABI 9, ADR 0021): `b` = SQL and parameters in the
    /// format of `proto::encode_sql_args`, `x` = expected changed rows
    /// (`u64::MAX` = any), `y` bit 0 = read-only.
    pub const SQL: u32 = 11;
}

/// States of [`vetro_files_status`].
pub mod status {
    /// No client (`vetro_files_open` not called).
    pub const NONE: u32 = 0;
    /// Connecting (or waiting to retry: the daemon isn't listening
    /// yet).
    pub const CONNECTING: u32 = 1;
    pub const READY: u32 = 2;
}

/// A read doesn't return more than this many bytes in one message.
pub const MAX_READ: u64 = 256 << 20;

/// A JSON string (with the quotes).
fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Guest bytes as a JSON string: valid UTF-8 as it is, every other byte
/// as a lone surrogate `\udcXX` (U+DC80 + byte - 0x80).
fn json_bytes(out: &mut String, b: &[u8]) {
    out.push('"');
    for chunk in b.utf8_chunks() {
        let valid = chunk.valid();
        let mut tmp = String::new();
        json_str(&mut tmp, valid);
        out.push_str(&tmp[1..tmp.len() - 1]);
        for x in chunk.invalid() {
            out.push_str(&format!("\\u{:04x}", 0xdc00 + u32::from(*x)));
        }
    }
    out.push('"');
}

fn json_value(out: &mut String, v: &SqlValue) {
    match v {
        SqlValue::Null => out.push_str("null"),
        SqlValue::Int(i) => out.push_str(&format!("[\"i\",\"{i}\"]")),
        SqlValue::Real(f) => out.push_str(&format!("[\"f\",\"{f:?}\"]")),
        SqlValue::Text(t) => {
            out.push_str("[\"t\",");
            json_str(out, t);
            out.push(']');
        }
        SqlValue::Blob(b) => {
            out.push_str("[\"b\",\"");
            for x in b {
                out.push_str(&format!("{x:02x}"));
            }
            out.push_str("\"]");
        }
    }
}

fn json_sql(out: &mut String, r: &SqlResult) {
    out.push_str(&format!(
        "\"type\":\"sql\",\"changes\":{},\"lastRowid\":\"{}\",\"truncated\":{},\"columns\":[",
        r.changes, r.last_rowid, r.truncated
    ));
    for (i, c) in r.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_str(out, c);
    }
    out.push_str("],\"rows\":[");
    for (i, row) in r.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (k, v) in row.iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            json_value(out, v);
        }
        out.push(']');
    }
    out.push(']');
}

fn json_stat(out: &mut String, s: &Stat) {
    out.push_str(&format!(
        "{{\"kind\":\"{}\",\"mode\":{},\"uid\":{},\"gid\":{},\"size\":{},\"mtime\":{},\"mtimeNs\":{},\"nlink\":{},\"link\":",
        s.kind.name(),
        s.mode,
        s.uid,
        s.gid,
        s.size,
        s.mtime_s,
        s.mtime_ns,
        s.nlink
    ));
    json_bytes(out, &s.link);
    out.push_str(",\"selinux\":");
    json_str(out, &s.selinux);
    out.push('}');
}

/// A message for JS: length of the JSON, JSON, bytes.
fn message(json: &str, data: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(4 + json.len() + data.len());
    m.extend_from_slice(&(json.len() as u32).to_le_bytes());
    m.extend_from_slice(json.as_bytes());
    m.extend_from_slice(data);
    m
}

/// The message of a response.
pub fn reply_message(c: &Completion) -> Vec<u8> {
    let mut j = format!("{{\"kind\":\"reply\",\"op\":{},", c.op);
    let mut data: &[u8] = &[];
    match &c.result {
        Err(e) => {
            j.push_str("\"ok\":false,\"error\":");
            json_str(&mut j, &e.to_string());
            match e {
                FilesError::Errno(n) => {
                    j.push_str(&format!(",\"errno\":{n},\"code\":\"{}\"", proto::errno_name(*n)))
                }
                FilesError::Protocol(_) => j.push_str(",\"errno\":null,\"code\":\"PROTOCOL\""),
                FilesError::Disconnected => j.push_str(",\"errno\":null,\"code\":\"DISCONNECTED\""),
                FilesError::Sql { code, .. } => {
                    j.push_str(&format!(",\"errno\":null,\"code\":\"SQLITE\",\"sqlite\":{code}"))
                }
            }
        }
        Ok(o) => {
            j.push_str("\"ok\":true,");
            match o {
                Outcome::Stat(s) => {
                    j.push_str("\"type\":\"stat\",\"stat\":");
                    json_stat(&mut j, s);
                }
                Outcome::Written(s) => {
                    j.push_str("\"type\":\"written\",\"stat\":");
                    json_stat(&mut j, s);
                }
                Outcome::List(entries) => {
                    j.push_str("\"type\":\"list\",\"entries\":[");
                    for (i, e) in entries.iter().enumerate() {
                        if i > 0 {
                            j.push(',');
                        }
                        j.push_str("{\"name\":");
                        json_bytes(&mut j, &e.name);
                        j.push_str(",\"stat\":");
                        json_stat(&mut j, &e.stat);
                        j.push('}');
                    }
                    j.push(']');
                }
                Outcome::Data { size, data: d } => {
                    j.push_str(&format!("\"type\":\"data\",\"size\":{size},\"length\":{}", d.len()));
                    data = d;
                }
                Outcome::Watch(wd) => j.push_str(&format!("\"type\":\"watch\",\"wd\":{wd}")),
                Outcome::Sql(r) => json_sql(&mut j, r),
                Outcome::Done => j.push_str("\"type\":\"done\""),
            }
        }
    }
    j.push('}');
    message(&j, data)
}

/// The message of an event.
pub fn event_message(e: &proto::Event) -> Vec<u8> {
    let mut j =
        format!("{{\"kind\":\"event\",\"wd\":{},\"mask\":{},\"cookie\":{},\"name\":", e.wd, e.mask, e.cookie);
    json_bytes(&mut j, &e.name);
    j.push('}');
    message(&j, &[])
}

/// Creates the file manager client towards the guest's vsock port `port`
/// (0 = 5200, the one of `vetro-files`), replacing the one that was there.
/// 1 = done, 0 = the machine has no virtio-vsock. To be called after
/// `vetro_load_linux` or `vetro_snapshot_restore`: the connections to the daemon
/// left in the snapshot are closed at the first `vetro_files_pump`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_open(vm: *mut Vm, port: u32) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    if vm.m.slots().vsock.is_none() {
        return 0;
    }
    if let Some(mut old) = vm.files.take() {
        old.close(&mut vm.m);
    }
    vm.files = Some(FilesClient::new(if port == 0 { proto::PORT } else { port }));
    vm.files_msg.clear();
    1
}

/// Closes the connection and removes the client.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_close(vm: *mut Vm) {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    if let Some(mut c) = vm.files.take() {
        c.close(&mut vm.m);
    }
}

/// Client state (codes of [`status`]); in `out` (at most `cap`
/// values): unfinished operations, successful connections (grows at every
/// daemon greeting: watches must be redone), `max_chunk` and flags
/// of the greeting (bit 0: SELinux in the guest). It doesn't touch the machine.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_status(vm: *const Vm, out: *mut u32, cap: usize) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`, `out` is valid for `cap` values.
    let vm = unsafe { &*vm };
    let Some(c) = &vm.files else { return status::NONE };
    let (code, chunk, flags) = match c.state() {
        LinkState::Ready(h) => (status::READY, h.max_chunk, u32::from(h.flags)),
        _ => (status::CONNECTING, 0, 0),
    };
    let v = [c.pending() as u32, c.generation(), chunk, flags];
    let n = v.len().min(cap);
    if n > 0 {
        unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&v[..n]);
    }
    code
}

/// Requests an operation (codes of [`op`]) on path `a` (guest
/// bytes, possibly non-UTF-8); `b`, `x` and `y` as written in the codes.
/// Returns the id (> 0) of the response, or 0 (no client, unknown
/// operation, empty path, broken SQL arguments).
/// It leaves at the next [`vetro_files_pump`] (immediately if the daemon is already
/// connected, otherwise at the greeting).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_request(
    vm: *mut Vm,
    op: u32,
    a: *const u8,
    a_len: usize,
    b: *const u8,
    b_len: usize,
    x: u64,
    y: u64,
) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`, the buffers are valid for
    // their lengths.
    let vm = unsafe { &mut *vm };
    let Some(c) = vm.files.as_mut() else { return 0 };
    let path = unsafe { crate::bytes(a, a_len) }.to_vec();
    let b = unsafe { crate::bytes(b, b_len) };
    if path.is_empty() {
        return 0;
    }
    let mode = (x & 0o7777) as u32;
    match op {
        op::STAT => c.stat(&path),
        op::LIST => c.list(&path),
        op::READ => c.read(&path, x, y.min(MAX_READ)),
        op::WRITE => c.write_file(&path, b, mode),
        op::MKDIR => c.mkdir(&path, mode),
        op::CREATE => c.create(&path, mode),
        op::DELETE => c.delete(&path, x & 1 != 0),
        op::RENAME if !b.is_empty() => c.rename(&path, b),
        op::WATCH => c.watch(&path),
        op::UNWATCH => c.unwatch(x as u32),
        op::SQL => match proto::decode_sql_args(b) {
            Ok((sql, params)) => {
                let expect = u32::try_from(x).ok().filter(|&e| e != u32::MAX);
                c.sql(&path, &sql, params, expect, y & 1 != 0)
            }
            Err(_) => 0,
        },
        _ => 0,
    }
}

/// Advances the client (connection, arrived bytes, queued requests):
/// to be called between one quantum and the next. Returns the ready messages.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_pump(vm: *mut Vm) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    let Some(c) = vm.files.as_mut() else { return 0 };
    c.pump(&mut vm.m);
    while let Some(e) = c.take_event() {
        vm.files_queue.push_back(event_message(&e));
    }
    while let Some(r) = c.take_completion() {
        vm.files_queue.push_back(reply_message(&r));
    }
    vm.files_queue.len() as u32
}

/// Prepares the next message and returns its length (0 = none).
/// The bytes stay in [`vetro_files_ptr`] until the next call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_take(vm: *mut Vm) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.files_msg = vm.files_queue.pop_front().unwrap_or_default();
    vm.files_msg.len()
}

/// The bytes of the last [`vetro_files_take`] (null if empty).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_ptr(vm: *const Vm) -> *const u8 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &*vm };
    if vm.files_msg.is_empty() { core::ptr::null() } else { vm.files_msg.as_ptr() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{dev, vetro_machine_free, vetro_machine_new_with};
    use vetro_machine::files::proto::{Entry, Event, Kind};

    fn json(m: &[u8]) -> (&str, &[u8]) {
        let n = u32::from_le_bytes(m[..4].try_into().unwrap()) as usize;
        (core::str::from_utf8(&m[4..4 + n]).unwrap(), &m[4 + n..])
    }

    #[test]
    fn messaggi_json() {
        let s = Stat {
            kind: Kind::File,
            mode: 0o100640,
            uid: 1,
            gid: 2,
            size: 3,
            mtime_s: 4,
            mtime_ns: 5,
            nlink: 1,
            link: b"l\xff".to_vec(),
            selinux: "u:\"x\"\n".into(),
        };
        let m = reply_message(&Completion {
            op: 7,
            result: Ok(Outcome::List(vec![Entry { name: b"a\\b\xc3\xa0\xc3".to_vec(), stat: s }])),
        });
        assert_eq!(
            json(&m).0,
            r#"{"kind":"reply","op":7,"ok":true,"type":"list","entries":[{"name":"a\\bà\udcc3","stat":{"kind":"file","mode":33184,"uid":1,"gid":2,"size":3,"mtime":4,"mtimeNs":5,"nlink":1,"link":"l\udcff","selinux":"u:\"x\"\n"}}]}"#
        );
        let m = reply_message(&Completion {
            op: 3,
            result: Ok(Outcome::Sql(SqlResult {
                changes: 1,
                last_rowid: -9_007_199_254_740_993,
                truncated: false,
                columns: vec!["a".into(), "\"b".into()],
                rows: vec![vec![
                    SqlValue::Null,
                    SqlValue::Int(i64::MAX),
                    SqlValue::Real(1.0),
                    SqlValue::Real(-2.5e-10),
                    SqlValue::Text("x\ny".into()),
                    SqlValue::Blob(vec![0, 0xab]),
                ]],
            })),
        });
        assert_eq!(
            json(&m).0,
            r#"{"kind":"reply","op":3,"ok":true,"type":"sql","changes":1,"lastRowid":"-9007199254740993","truncated":false,"columns":["a","\"b"],"rows":[[null,["i","9223372036854775807"],["f","1.0"],["f","-2.5e-10"],["t","x\ny"],["b","00ab"]]]}"#
        );
        let m = reply_message(&Completion {
            op: 4,
            result: Err(FilesError::Sql { code: 5, message: "database is locked".into() }),
        });
        assert_eq!(
            json(&m).0,
            r#"{"kind":"reply","op":4,"ok":false,"error":"SQLite 5: database is locked","errno":null,"code":"SQLITE","sqlite":5}"#
        );
        let m = reply_message(&Completion {
            op: 8,
            result: Ok(Outcome::Data { size: 9, data: b"ciao".to_vec() }),
        });
        assert_eq!(
            json(&m),
            (r#"{"kind":"reply","op":8,"ok":true,"type":"data","size":9,"length":4}"#, &b"ciao"[..])
        );
        let m = reply_message(&Completion { op: 9, result: Err(FilesError::Errno(2)) });
        assert_eq!(
            json(&m).0,
            r#"{"kind":"reply","op":9,"ok":false,"error":"ENOENT (2)","errno":2,"code":"ENOENT"}"#
        );
        let m = event_message(&Event { wd: 1, mask: 8, cookie: 0, name: "\u{1}".into() });
        assert_eq!(json(&m).0, r#"{"kind":"event","wd":1,"mask":8,"cookie":0,"name":"\u0001"}"#);
        let m = event_message(&Event { wd: 1, mask: 8, cookie: 0, name: vec![0x80, b'"', 0xff] });
        assert_eq!(json(&m).0, r#"{"kind":"event","wd":1,"mask":8,"cookie":0,"name":"\udc80\"\udcff"}"#);
    }

    /// Without a kernel nobody listens: the client stays connecting, the
    /// requests wait. Without vsock no client.
    #[test]
    fn client_dall_api() {
        let vm = vetro_machine_new_with(64 << 20, 0, 0, dev::VSOCK, 0, 0);
        let none = vetro_machine_new_with(64 << 20, 0, 0, 0, 0, 0);
        unsafe {
            assert_eq!(vetro_files_open(none, 0), 0);
            assert_eq!(vetro_files_request(none, op::LIST, b"/".as_ptr(), 1, core::ptr::null(), 0, 0, 0), 0);
            let mut st = [9u32; 4];
            assert_eq!(vetro_files_status(vm, st.as_mut_ptr(), 4), status::NONE);
            assert_eq!(vetro_files_open(vm, 0), 1);
            let id = vetro_files_request(vm, op::LIST, b"/".as_ptr(), 1, core::ptr::null(), 0, 0, 0);
            assert!(id > 0);
            assert_eq!(vetro_files_request(vm, 99, b"/".as_ptr(), 1, core::ptr::null(), 0, 0, 0), 0);
            // Non-UTF-8 paths: they are bytes, they are accepted.
            assert!(vetro_files_request(vm, op::STAT, [0xffu8].as_ptr(), 1, core::ptr::null(), 0, 0, 0) > 0);
            assert_eq!(vetro_files_request(vm, op::STAT, b"".as_ptr(), 0, core::ptr::null(), 0, 0, 0), 0);
            let args = proto::encode_sql_args("SELECT ?1", &[SqlValue::Int(1)]);
            let sql =
                vetro_files_request(vm, op::SQL, b"/d".as_ptr(), 2, args.as_ptr(), args.len(), u64::MAX, 1);
            assert!(sql > 0);
            assert_eq!(vetro_files_request(vm, op::SQL, b"/d".as_ptr(), 2, args.as_ptr(), 3, u64::MAX, 1), 0);
            assert_eq!(vetro_files_request(vm, op::RENAME, b"/d".as_ptr(), 2, core::ptr::null(), 0, 0, 0), 0);
            assert_eq!(vetro_files_pump(vm), 0);
            assert_eq!(vetro_files_status(vm, st.as_mut_ptr(), 4), status::CONNECTING);
            assert_eq!(st, [3, 0, 0, 0]);
            assert_eq!(vetro_files_take(vm), 0);
            assert!(vetro_files_ptr(vm).is_null());
            vetro_files_close(vm);
            assert_eq!(vetro_files_status(vm, st.as_mut_ptr(), 4), status::NONE);
            vetro_machine_free(vm);
            vetro_machine_free(none);
        }
    }
}
