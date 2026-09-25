//! Il gestore dei file (ABI 7, M8, ADR 0020): il client di
//! `vetro_machine::files` verso il demone `vetro-files` del guest, su
//! virtio-vsock (bit `VSOCK` di `vetro_machine_new_with`).
//!
//! Il JS chiede operazioni ([`vetro_files_request`], id > 0), fa avanzare il
//! client fra un quanto e l'altro ([`vetro_files_pump`], come la console) e
//! legge i messaggi pronti uno alla volta ([`vetro_files_take`] e
//! [`vetro_files_ptr`]): `u32` lunghezza del JSON, il JSON (UTF-8), poi i
//! byte letti di una lettura. Il JSON è una risposta
//! (`{"kind":"reply","op":N,"ok":true,"type":...}`) o un evento di inotify
//! (`{"kind":"event","wd":N,"mask":N,"cookie":N,"name":"..."}`); il formato
//! è in `docs/specs/wasm.md`.
//!
//! Connessione, byte mandati e byte letti passano da `Machine::input`:
//! sono ingressi, registrati per il replay (ADR 0019). Stato e messaggi
//! già pronti non toccano la macchina.

use vetro_machine::FilesClient;
use vetro_machine::files::proto::{self, Stat};
use vetro_machine::files::{Completion, FilesError, LinkState, Outcome};

use crate::Vm;

/// Operazioni di [`vetro_files_request`].
pub mod op {
    pub const STAT: u32 = 1;
    pub const LIST: u32 = 2;
    /// `x` = offset, `y` = byte (`u64::MAX` = fino alla fine).
    pub const READ: u32 = 3;
    /// `b` = contenuto, `x` = permessi se il file è nuovo.
    pub const WRITE: u32 = 4;
    /// `x` = permessi.
    pub const MKDIR: u32 = 5;
    /// `x` = permessi.
    pub const CREATE: u32 = 6;
    /// `x` = 1 per cancellare una cartella con tutto il contenuto.
    pub const DELETE: u32 = 7;
    /// `b` = nuovo percorso (UTF-8).
    pub const RENAME: u32 = 8;
    pub const WATCH: u32 = 9;
    /// `x` = id dell'osservazione.
    pub const UNWATCH: u32 = 10;
}

/// Stati di [`vetro_files_status`].
pub mod status {
    /// Nessun client (`vetro_files_open` non chiamata).
    pub const NONE: u32 = 0;
    /// In collegamento (o in attesa di riprovare: il demone non ascolta
    /// ancora).
    pub const CONNECTING: u32 = 1;
    pub const READY: u32 = 2;
}

/// Una lettura non restituisce più di tanti byte in un messaggio.
pub const MAX_READ: u64 = 256 << 20;

/// Una stringa JSON (con le virgolette).
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
    json_str(out, &s.link);
    out.push_str(",\"selinux\":");
    json_str(out, &s.selinux);
    out.push('}');
}

/// Un messaggio per il JS: lunghezza del JSON, JSON, byte.
fn message(json: &str, data: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(4 + json.len() + data.len());
    m.extend_from_slice(&(json.len() as u32).to_le_bytes());
    m.extend_from_slice(json.as_bytes());
    m.extend_from_slice(data);
    m
}

/// Il messaggio di una risposta.
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
                        json_str(&mut j, &e.name);
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
                Outcome::Done => j.push_str("\"type\":\"done\""),
            }
        }
    }
    j.push('}');
    message(&j, data)
}

/// Il messaggio di un evento.
pub fn event_message(e: &proto::Event) -> Vec<u8> {
    let mut j =
        format!("{{\"kind\":\"event\",\"wd\":{},\"mask\":{},\"cookie\":{},\"name\":", e.wd, e.mask, e.cookie);
    json_str(&mut j, &e.name);
    j.push('}');
    message(&j, &[])
}

/// Stringa UTF-8 da un buffer del JS.
unsafe fn text(ptr: *const u8, len: usize) -> Option<String> {
    // SAFETY: chi chiama garantisce `len` byte validi in `ptr`.
    let b = unsafe { crate::bytes(ptr, len) };
    core::str::from_utf8(b).ok().map(str::to_string)
}

/// Crea il client del gestore dei file verso la porta vsock `port` del
/// guest (0 = 5200, quella di `vetro-files`), al posto di quello che c'era.
/// 1 = fatto, 0 = la macchina non ha virtio-vsock. Da chiamare dopo
/// `vetro_load_linux` o `vetro_snapshot_restore`: le connessioni al demone
/// rimaste nello snapshot si chiudono al primo `vetro_files_pump`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_open(vm: *mut Vm, port: u32) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
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

/// Chiude la connessione e toglie il client.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_close(vm: *mut Vm) {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    if let Some(mut c) = vm.files.take() {
        c.close(&mut vm.m);
    }
}

/// Stato del client (codici di [`status`]); in `out` (al più `cap`
/// valori): operazioni non finite, collegamenti riusciti (cresce a ogni
/// saluto del demone: le osservazioni vanno rifatte), `max_chunk` e flag
/// del saluto (bit 0: SELinux nel guest). Non tocca la macchina.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_status(vm: *const Vm, out: *mut u32, cap: usize) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`, `out` vale per `cap` valori.
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

/// Chiede un'operazione (codici di [`op`]) sul percorso `a` (UTF-8); `b`,
/// `x` e `y` come scritto nei codici. Restituisce l'id (> 0) della
/// risposta, o 0 (nessun client, operazione sconosciuta, testo non UTF-8).
/// Parte al prossimo [`vetro_files_pump`] (subito se il demone è già
/// collegato, altrimenti al saluto).
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
    // SAFETY: `vm` viene da `vetro_machine_new`, i buffer valgono per le
    // loro lunghezze.
    let vm = unsafe { &mut *vm };
    let Some(c) = vm.files.as_mut() else { return 0 };
    let Some(path) = (unsafe { text(a, a_len) }) else { return 0 };
    let mode = (x & 0o7777) as u32;
    match op {
        op::STAT => c.stat(&path),
        op::LIST => c.list(&path),
        op::READ => c.read(&path, x, y.min(MAX_READ)),
        op::WRITE => c.write_file(&path, unsafe { crate::bytes(b, b_len) }, mode),
        op::MKDIR => c.mkdir(&path, mode),
        op::CREATE => c.create(&path, mode),
        op::DELETE => c.delete(&path, x & 1 != 0),
        op::RENAME => match unsafe { text(b, b_len) } {
            Some(to) => c.rename(&path, &to),
            None => 0,
        },
        op::WATCH => c.watch(&path),
        op::UNWATCH => c.unwatch(x as u32),
        _ => 0,
    }
}

/// Fa avanzare il client (connessione, byte arrivati, richieste in coda):
/// da chiamare fra un quanto e l'altro. Restituisce i messaggi pronti.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_pump(vm: *mut Vm) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
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

/// Prepara il prossimo messaggio e ne restituisce la lunghezza (0 = nessuno).
/// I byte stanno in [`vetro_files_ptr`] fino alla prossima chiamata.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_take(vm: *mut Vm) -> usize {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    vm.files_msg = vm.files_queue.pop_front().unwrap_or_default();
    vm.files_msg.len()
}

/// I byte dell'ultimo [`vetro_files_take`] (nullo se vuoto).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_files_ptr(vm: *const Vm) -> *const u8 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
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
            link: String::new(),
            selinux: "u:\"x\"\n".into(),
        };
        let m = reply_message(&Completion {
            op: 7,
            result: Ok(Outcome::List(vec![Entry { name: "a\\b".into(), stat: s }])),
        });
        assert_eq!(
            json(&m).0,
            r#"{"kind":"reply","op":7,"ok":true,"type":"list","entries":[{"name":"a\\b","stat":{"kind":"file","mode":33184,"uid":1,"gid":2,"size":3,"mtime":4,"mtimeNs":5,"nlink":1,"link":"","selinux":"u:\"x\"\n"}}]}"#
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
    }

    /// Senza kernel nessuno ascolta: il client resta in collegamento, le
    /// richieste aspettano. Senza vsock niente client.
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
            assert_eq!(
                vetro_files_request(vm, op::STAT, [0xffu8].as_ptr(), 1, core::ptr::null(), 0, 0, 0),
                0
            );
            assert_eq!(vetro_files_pump(vm), 0);
            assert_eq!(vetro_files_status(vm, st.as_mut_ptr(), 4), status::CONNECTING);
            assert_eq!(st, [1, 0, 0, 0]);
            assert_eq!(vetro_files_take(vm), 0);
            assert!(vetro_files_ptr(vm).is_null());
            vetro_files_close(vm);
            assert_eq!(vetro_files_status(vm, st.as_mut_ptr(), 4), status::NONE);
            vetro_machine_free(vm);
            vetro_machine_free(none);
        }
    }
}
