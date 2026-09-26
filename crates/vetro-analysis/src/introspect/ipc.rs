//! Chiamate Binder decodificate (M8): dalle transazioni grezze di
//! [`super::binder`] a "chi chiama chi, quale interfaccia, quale metodo,
//! che cosa di sensibile".
//!
//! Ogni transazione si vede due volte: dal mittente (`BC_TRANSACTION`
//! nell'ingresso della sua `ioctl`, con un handle come destinazione) e dal
//! destinatario (`BR_TRANSACTION` all'uscita della sua `ioctl`, con pid e
//! euid del mittente messi dal kernel; pid 0 per le chiamate oneway). Il
//! [`BinderLog`] le accoppia per codice e byte del Parcel, così ogni
//! chiamata ha i due capi.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;

use super::aidl;
use super::binder::Transaction;
use super::parcel::{Parcel, strings16};
use super::privacy::{self, Sensitive};

/// Un capo di una chiamata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Party {
    pub pid: i32,
    pub tid: i32,
    pub uid: u32,
    /// `comm` del thread.
    pub comm: String,
    /// Nome del processo (riga di comando: per le app è il pacchetto).
    pub process: String,
}

impl Party {
    /// Il pacchetto: il nome del processo se è un'app (uid >= 10000),
    /// senza il suffisso `:servizio`.
    pub fn package(&self) -> Option<&str> {
        (self.uid % 100_000 >= 10_000 && !self.process.is_empty())
            .then(|| self.process.split(':').next().unwrap_or(&self.process))
    }

    fn label(&self) -> String {
        let name = if self.process.is_empty() { &self.comm } else { &self.process };
        format!("{}({}, uid {})", name, self.pid, self.uid)
    }
}

/// Una chiamata Binder decodificata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinderCall {
    /// Istruzione della macchina dell'osservazione più vecchia.
    pub step: u64,
    pub sender: Option<Party>,
    pub receiver: Option<Party>,
    /// Handle usato dal mittente.
    pub handle: Option<u64>,
    pub descriptor: Option<String>,
    pub code: u32,
    pub method: Option<&'static str>,
    pub one_way: bool,
    pub data_size: u64,
    /// Stringhe del Parcel (dopo il descrittore), al più 16.
    pub strings: Vec<String>,
    pub sensitive: Vec<Sensitive>,
}

impl BinderCall {
    /// Decodifica una transazione (non una risposta) vista da `observer`:
    /// il mittente se `BC_*`, il destinatario se `BR_*`.
    pub fn decode(step: u64, t: &Transaction, observer: Party) -> Option<BinderCall> {
        if t.reply {
            return None;
        }
        let mut p = Parcel::new(&t.data);
        let header = p.interface_header();
        let skip = if header.is_some() { p.position() } else { 0 };
        let strings: Vec<String> =
            strings16(&t.data).into_iter().filter(|(o, _)| *o >= skip).map(|(_, s)| s).take(16).collect();
        let descriptor = header.map(|h| h.descriptor);
        let method = descriptor
            .as_deref()
            .and_then(|d| aidl::method(d, t.code))
            .or_else(|| aidl::reserved_code(t.code));
        let sensitive =
            descriptor.as_deref().map(|d| privacy::classify(d, method, &strings)).unwrap_or_default();
        let (sender, receiver, handle) = if t.incoming {
            let s = Party { pid: t.sender_pid, uid: t.sender_euid, ..Party::default() };
            (Some(s), Some(observer), None)
        } else {
            (Some(observer), None, Some(t.target))
        };
        Some(BinderCall {
            step,
            sender,
            receiver,
            handle,
            descriptor,
            code: t.code,
            method,
            one_way: t.one_way(),
            data_size: t.data_size,
            strings,
            sensitive,
        })
    }

    /// Interfaccia e metodo leggibili: `android.content.IClipboard.getPrimaryClip`
    /// o `android.foo.IBar#7`.
    pub fn name(&self) -> String {
        let d = self.descriptor.as_deref().unwrap_or("?");
        match self.method {
            Some(m) => format!("{d}.{m}"),
            None => format!("{d}#{}", self.code),
        }
    }

    /// Una riga di log.
    pub fn line(&self) -> String {
        let who = |p: &Option<Party>| p.as_ref().map_or("?".to_string(), Party::label);
        let mut s = format!(
            "[{}] {} -> {} {}{} ({} byte)",
            self.step,
            who(&self.sender),
            who(&self.receiver),
            self.name(),
            if self.one_way { " oneway" } else { "" },
            self.data_size
        );
        for x in &self.sensitive {
            let _ = write!(s, " ; SENSIBILE {x}");
        }
        s
    }

    /// JSON (una chiamata).
    pub fn to_json(&self) -> String {
        let party = |p: &Option<Party>| match p {
            None => "null".to_string(),
            Some(p) => format!(
                "{{\"pid\":{},\"tid\":{},\"uid\":{},\"comm\":{},\"process\":{},\"package\":{}}}",
                p.pid,
                p.tid,
                p.uid,
                q(&p.comm),
                q(&p.process),
                p.package().map_or("null".into(), q)
            ),
        };
        let strings: Vec<String> = self.strings.iter().map(|s| q(s)).collect();
        let sens: Vec<String> = self
            .sensitive
            .iter()
            .map(|x| format!("{{\"category\":{},\"what\":{}}}", q(x.category.id()), q(&x.what)))
            .collect();
        format!(
            "{{\"step\":{},\"sender\":{},\"receiver\":{},\"handle\":{},\"interface\":{},\"code\":{},\"method\":{},\"oneway\":{},\"size\":{},\"strings\":[{}],\"sensitive\":[{}]}}",
            self.step,
            party(&self.sender),
            party(&self.receiver),
            self.handle.map_or("null".into(), |h| h.to_string()),
            self.descriptor.as_deref().map_or("null".into(), q),
            self.code,
            self.method.map_or("null".into(), q),
            self.one_way,
            self.data_size,
            strings.join(","),
            sens.join(",")
        )
    }
}

fn q(s: &str) -> String {
    crate::net::json::quote(s)
}

/// Le chiamate in ordine, con i due capi accoppiati.
#[derive(Debug, Default)]
pub struct BinderLog {
    pub calls: Vec<BinderCall>,
    /// Chiamate viste dal mittente, in attesa del destinatario: chiave
    /// (codice, hash del Parcel) -> indici in `calls`.
    waiting: BTreeMap<(u32, u64), VecDeque<usize>>,
    /// Oltre, le chiamate si contano soltanto.
    pub max_calls: usize,
    pub dropped: u64,
}

fn hash(d: &[u8]) -> u64 {
    // FNV-1a: basta a distinguere Parcel diversi con lo stesso codice.
    d.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

impl BinderLog {
    pub fn new() -> Self {
        BinderLog { max_calls: 1 << 20, ..Default::default() }
    }

    /// Aggiunge una transazione osservata da `observer` all'istruzione `step`.
    pub fn observe(&mut self, step: u64, t: &Transaction, observer: Party) {
        let Some(call) = BinderCall::decode(step, t, observer) else { return };
        let key = (t.code, hash(&t.data));
        if t.incoming {
            if let Some(q) = self.waiting.get_mut(&key)
                && let Some(i) = q.pop_front()
            {
                let c = &mut self.calls[i];
                c.receiver = call.receiver;
                // Il kernel dà pid 0 per le oneway: il mittente visto
                // dall'ingresso ha già pid e uid giusti.
                if q.is_empty() {
                    self.waiting.remove(&key);
                }
                return;
            }
            self.push(call, None);
        } else {
            self.push(call, Some(key));
        }
    }

    fn push(&mut self, call: BinderCall, key: Option<(u32, u64)>) {
        if self.calls.len() >= self.max_calls {
            self.dropped += 1;
            return;
        }
        if let Some(k) = key {
            let q = self.waiting.entry(k).or_default();
            q.push_back(self.calls.len());
            // Una destinazione morta non deve far crescere la coda.
            if q.len() > 64 {
                q.pop_front();
            }
        }
        self.calls.push(call);
    }

    /// Le chiamate con accessi sensibili.
    pub fn sensitive(&self) -> impl Iterator<Item = &BinderCall> {
        self.calls.iter().filter(|c| !c.sensitive.is_empty())
    }

    /// Tutto in JSON: `{"calls":[...]}`.
    pub fn to_json(&self) -> String {
        let v: Vec<String> = self.calls.iter().map(BinderCall::to_json).collect();
        format!("{{\"calls\":[\n{}\n],\"dropped\":{}}}\n", v.join(",\n"), self.dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::introspect::parcel::tests::Builder;

    fn txn(incoming: bool, code: u32, data: Vec<u8>) -> Transaction {
        Transaction {
            command: if incoming { "BR_TRANSACTION" } else { "BC_TRANSACTION" }.into(),
            reply: false,
            incoming,
            target: if incoming { 0xffff_0000_1234 } else { 5 },
            cookie: 0,
            code,
            flags: 0,
            sender_pid: if incoming { 4242 } else { 0 },
            sender_euid: if incoming { 10057 } else { 0 },
            data_size: data.len() as u64,
            offsets_size: 0,
            buffer: 0,
            offsets: 0,
            data,
            objects: Vec::new(),
        }
    }

    #[test]
    fn appunti_e_android_id_con_i_due_capi() {
        let app = Party {
            pid: 4242,
            tid: 4250,
            uid: 10057,
            comm: "vetro.probe".into(),
            process: "com.vetro.probe".into(),
        };
        let ss = Party {
            pid: 853,
            tid: 900,
            uid: 1000,
            comm: "binder:853_3".into(),
            process: "system_server".into(),
        };
        let clip =
            Builder::default().token("android.content.IClipboard").s16("com.vetro.probe").i32(0).i32(0).0;
        let mut log = BinderLog::new();
        log.observe(10, &txn(false, 4, clip.clone()), app.clone());
        log.observe(20, &txn(true, 4, clip), ss.clone());
        assert_eq!(log.calls.len(), 1);
        let c = &log.calls[0];
        assert_eq!(c.name(), "android.content.IClipboard.getPrimaryClip");
        assert_eq!(c.sender.as_ref().unwrap().package(), Some("com.vetro.probe"));
        assert_eq!(c.receiver.as_ref().unwrap().process, "system_server");
        assert_eq!(c.sensitive[0].category, privacy::Category::Clipboard);
        assert!(c.line().contains("SENSIBILE appunti"), "{}", c.line());
        // ANDROID_ID: IContentProvider.call verso il provider delle impostazioni.
        let call = Builder::default()
            .token("android.content.IContentProvider")
            .s16("com.vetro.probe")
            .i32(-1)
            .s16("settings")
            .s16("GET_secure")
            .s16("android_id")
            .0;
        log.observe(30, &txn(true, 21, call), ss);
        let c = &log.calls[1];
        assert_eq!(c.method, Some("call"));
        assert_eq!(c.sender.as_ref().unwrap().uid, 10057);
        assert!(c.sensitive.iter().any(|s| s.what.starts_with("ANDROID_ID")));
        let j = log.to_json();
        assert!(j.contains("\"package\":\"com.vetro.probe\""), "{j}");
        assert_eq!(log.sensitive().count(), 2);
    }
}
