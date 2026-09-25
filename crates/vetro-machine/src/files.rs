//! Client del gestore dei file (M8, ADR 0020, `docs/specs/files.md`): parla
//! con `vetro-files`, il demone di Vetro nel guest, su virtio-vsock.
//!
//! Il demone legge e scrive i file passando dal kernel del guest (niente
//! accesso diretto all'immagine del disco). Il client sta dalla parte
//! dell'host e non tocca la macchina se non con [`Machine::input`]
//! (connessione, byte mandati, byte letti: tutti ingressi registrati per il
//! replay, ADR 0019) e con [`Machine::vsock_view`] (stato della connessione
//! e byte pronti, sola lettura). Chi lo usa chiama [`FilesClient::pump`] fra
//! un quanto e l'altro, come per la console: le richieste partono e le
//! risposte arrivano lì, sempre agli stessi numeri d'istruzione con lo
//! stesso copione.
//!
//! Le operazioni ([`FilesClient::list`], [`FilesClient::read`],
//! [`FilesClient::write_file`], [`FilesClient::sql`], ...) restituiscono un
//! id; la
//! [`Completion`] con quell'id esce da [`FilesClient::take_completion`]. Una
//! lettura lunga diventa più richieste READ da [`proto::CHUNK`] byte una
//! dopo l'altra; una scrittura diventa WOPEN, i WDATA e WCOMMIT mandati
//! insieme (il demone li serve in ordine: il file vero cambia solo al
//! WCOMMIT, con un rename atomico). Gli eventi di inotify escono da
//! [`FilesClient::take_event`]. I percorsi sono byte del file system del
//! guest (`impl AsRef<[u8]>`: anche `&str`), non per forza UTF-8 (ADR 0021).
//!
//! Connessione: il client si collega alla porta [`proto::PORT`] del guest;
//! se nessuno ascolta (demone non ancora partito) riprova ogni
//! [`RETRY_NS`] di tempo del guest. Se la connessione cade dopo il saluto,
//! le operazioni in corso finiscono con [`FilesError::Disconnected`], le
//! osservazioni si perdono (il demone le toglie) e il client si ricollega:
//! [`FilesClient::generation`] cresce a ogni saluto, così chi osserva sa di
//! dover rifare i WATCH. Le connessioni verso la porta del demone rimaste
//! da una sessione precedente (uno snapshot ripristinato) si chiudono al
//! primo collegamento.
//!
//! Le **radici da mostrare** ([`FilesClient::set_roots`]) le decide chi
//! chiama: oggi a mano o dalla riga di comando; con Android le imposterà il
//! rilevamento dell'app in primo piano dal decoder Binder
//! ([`app_roots`] dà le cartelle di un pacchetto).

pub mod proto;

use std::collections::{BTreeMap, VecDeque};

use core::fmt;

use vetro_platform::virtio::{VsockConn, VsockState};

use crate::{Input, Machine, ReplayStatus, Reply, VsockOp};
use proto::{Decoder, Entry, Event, Frame, Hello, Request, SqlResult, SqlValue, Stat};

/// Attesa fra due tentativi di collegamento (tempo del guest): 100 ms.
pub const RETRY_NS: u64 = 100_000_000;

/// Le cartelle di un'app Android (pacchetto `package`, utente 0) da
/// mostrare quando è in primo piano (docs/PLAN.md, M8).
pub fn app_roots(package: &str) -> Vec<String> {
    ["/data/data/", "/data/user_de/0/", "/sdcard/Android/data/", "/sdcard/Android/media/"]
        .iter()
        .map(|p| format!("{p}{package}"))
        .collect()
}

/// Esito di un'operazione.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Stat(Stat),
    List(Vec<Entry>),
    /// Byte letti e dimensione del file al primo pezzo.
    Data {
        size: u64,
        data: Vec<u8>,
    },
    /// Scrittura fatta: i metadati del file dopo il rename.
    Written(Stat),
    /// Osservazione aperta: il suo id (lo stesso degli [`Event::wd`]).
    Watch(u32),
    /// SQL eseguito e confermato nel guest (ADR 0021).
    Sql(SqlResult),
    Done,
}

/// Perché un'operazione non è riuscita.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilesError {
    /// Errore del guest (errno di Linux).
    Errno(u32),
    /// Il demone ha mandato byte senza senso: connessione chiusa.
    Protocol(String),
    /// La connessione è caduta prima della risposta.
    Disconnected,
    /// SQLite ha rifiutato (codice primario e messaggio): la transazione è
    /// annullata, il database non è cambiato.
    Sql { code: u32, message: String },
}

impl fmt::Display for FilesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilesError::Errno(e) => write!(f, "{} ({e})", proto::errno_name(*e)),
            FilesError::Protocol(m) => write!(f, "protocollo: {m}"),
            FilesError::Disconnected => f.write_str("connessione con vetro-files caduta"),
            FilesError::Sql { code, message } => write!(f, "SQLite {code}: {message}"),
        }
    }
}

/// Un'operazione finita.
#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    pub op: u32,
    pub result: Result<Outcome, FilesError>,
}

/// Stato del collegamento con il demone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// Mai provato (prima di `pump`).
    Idle,
    /// Richiesta di connessione partita, saluto non ancora arrivato.
    Connecting,
    /// Saluto ricevuto: le richieste partono.
    Ready(Hello),
    /// Nessuno in ascolto (o connessione caduta): nuovo tentativo al tempo
    /// del guest indicato.
    Waiting { until_ns: u64 },
}

#[derive(Clone, Debug)]
enum Work {
    /// Una richiesta, una risposta.
    Simple(Request),
    Read {
        path: Vec<u8>,
        offset: u64,
        left: u64,
        size: Option<u64>,
        data: Vec<u8>,
    },
    Write {
        path: Vec<u8>,
        data: Vec<u8>,
        mode: u32,
        excl: bool,
        replies: u32,
        err: Option<FilesError>,
    },
}

#[derive(Clone, Debug)]
struct Op {
    work: Work,
    started: bool,
}

/// Il client (vedi il modulo).
#[derive(Clone, Debug)]
pub struct FilesClient {
    port: u32,
    conn: Option<VsockConn>,
    link: LinkState,
    cleaned: bool,
    dec: Decoder,
    out: Vec<u8>,
    next_id: u32,
    next_op: u32,
    next_handle: u32,
    ops: BTreeMap<u32, Op>,
    /// Richiesta del protocollo in attesa → operazione.
    waiting: BTreeMap<u32, u32>,
    done: VecDeque<Completion>,
    events: VecDeque<Event>,
    generation: u32,
    roots: Vec<String>,
}

impl Default for FilesClient {
    fn default() -> Self {
        Self::new(proto::PORT)
    }
}

impl FilesClient {
    /// Un client per il demone sulla porta vsock `port` del guest.
    pub fn new(port: u32) -> Self {
        FilesClient {
            port,
            conn: None,
            link: LinkState::Idle,
            cleaned: false,
            dec: Decoder::default(),
            out: Vec::new(),
            next_id: 1,
            next_op: 1,
            next_handle: 1,
            ops: BTreeMap::new(),
            waiting: BTreeMap::new(),
            done: VecDeque::new(),
            events: VecDeque::new(),
            generation: 0,
            roots: vec!["/".into()],
        }
    }

    pub fn port(&self) -> u32 {
        self.port
    }

    pub fn state(&self) -> LinkState {
        self.link
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.link, LinkState::Ready(_))
    }

    /// Saluti ricevuti: cresce a ogni (ri)collegamento.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Operazioni non ancora finite.
    pub fn pending(&self) -> usize {
        self.ops.len()
    }

    /// Le radici da mostrare (default `/`).
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// Imposta le radici da mostrare (l'app in primo piano, o a mano).
    pub fn set_roots(&mut self, roots: Vec<String>) {
        self.roots = roots;
    }

    // ---- Operazioni ------------------------------------------------------------

    fn push(&mut self, work: Work) -> u32 {
        let id = self.next_op;
        self.next_op = self.next_op.checked_add(1).unwrap_or(1);
        self.ops.insert(id, Op { work, started: false });
        if self.is_ready() {
            self.start(id);
        }
        id
    }

    pub fn stat(&mut self, path: impl AsRef<[u8]>) -> u32 {
        self.push(Work::Simple(Request::Stat { path: path.as_ref().to_vec() }))
    }

    pub fn list(&mut self, path: impl AsRef<[u8]>) -> u32 {
        self.push(Work::Simple(Request::List { path: path.as_ref().to_vec() }))
    }

    /// Legge `len` byte da `offset` (`u64::MAX` = fino alla fine), a pezzi.
    pub fn read(&mut self, path: impl AsRef<[u8]>, offset: u64, len: u64) -> u32 {
        self.push(Work::Read {
            path: path.as_ref().to_vec(),
            offset,
            left: len,
            size: None,
            data: Vec::new(),
        })
    }

    /// Il file intero.
    pub fn read_file(&mut self, path: impl AsRef<[u8]>) -> u32 {
        self.read(path, 0, u64::MAX)
    }

    /// Sostituisce (o crea, con permessi `mode`) il file con `data`, in modo
    /// atomico: un file che c'è già tiene proprietario, modo e xattr.
    pub fn write_file(&mut self, path: impl AsRef<[u8]>, data: &[u8], mode: u32) -> u32 {
        self.push(Work::Write {
            path: path.as_ref().to_vec(),
            data: data.to_vec(),
            mode,
            excl: false,
            replies: 0,
            err: None,
        })
    }

    /// Crea un file vuoto (fallisce se c'è già).
    pub fn create(&mut self, path: impl AsRef<[u8]>, mode: u32) -> u32 {
        self.push(Work::Simple(Request::Create { path: path.as_ref().to_vec(), mode }))
    }

    pub fn mkdir(&mut self, path: impl AsRef<[u8]>, mode: u32) -> u32 {
        self.push(Work::Simple(Request::Mkdir { path: path.as_ref().to_vec(), mode }))
    }

    /// Cancella un file o una cartella (vuota, o tutto con `recursive`).
    pub fn delete(&mut self, path: impl AsRef<[u8]>, recursive: bool) -> u32 {
        self.push(Work::Simple(Request::Delete { path: path.as_ref().to_vec(), recursive }))
    }

    pub fn rename(&mut self, from: impl AsRef<[u8]>, to: impl AsRef<[u8]>) -> u32 {
        self.push(Work::Simple(Request::Rename { from: from.as_ref().to_vec(), to: to.as_ref().to_vec() }))
    }

    /// Osserva una cartella (o un file) con inotify.
    pub fn watch(&mut self, path: impl AsRef<[u8]>) -> u32 {
        self.push(Work::Simple(Request::Watch { path: path.as_ref().to_vec() }))
    }

    pub fn unwatch(&mut self, wd: u32) -> u32 {
        self.push(Work::Simple(Request::Unwatch { wd }))
    }

    /// Esegue `sql` sul database SQLite `path` nel guest, con il motore vero
    /// e come il proprietario del file (ADR 0021): tutte le istruzioni in
    /// una transazione (tranne `readonly`), `params` legati a `?1`, `?2`, ...;
    /// con `expect` un numero diverso di righe cambiate annulla tutto
    /// ([`FilesError::Sql`]). L'esito è [`Outcome::Sql`].
    pub fn sql(
        &mut self,
        path: impl AsRef<[u8]>,
        sql: &str,
        params: Vec<SqlValue>,
        expect: Option<u32>,
        readonly: bool,
    ) -> u32 {
        self.push(Work::Simple(Request::Sql {
            path: path.as_ref().to_vec(),
            sql: sql.to_string(),
            params,
            expect,
            readonly,
        }))
    }

    /// La prossima operazione finita.
    pub fn take_completion(&mut self) -> Option<Completion> {
        self.done.pop_front()
    }

    /// Il prossimo evento di inotify.
    pub fn take_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    // ---- Protocollo (senza macchina) ---------------------------------------

    fn chunk(&self) -> usize {
        match self.link {
            LinkState::Ready(h) => proto::CHUNK.min(h.max_chunk.max(1) as usize),
            _ => proto::CHUNK,
        }
    }

    fn send(&mut self, op: u32, req: &Request) {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).unwrap_or(1);
        self.out.extend(req.encode(id));
        self.waiting.insert(id, op);
    }

    fn start(&mut self, id: u32) {
        let chunk = self.chunk();
        let Some(op) = self.ops.get_mut(&id) else { return };
        if op.started {
            return;
        }
        op.started = true;
        let mut reqs = Vec::new();
        match &mut op.work {
            Work::Simple(r) => reqs.push(r.clone()),
            Work::Read { path, offset, left, .. } => reqs.push(Request::Read {
                path: path.clone(),
                offset: *offset,
                len: (*left).min(chunk as u64) as u32,
            }),
            Work::Write { path, data, mode, excl, replies, .. } => {
                let handle = self.next_handle;
                self.next_handle = self.next_handle.checked_add(1).unwrap_or(1);
                reqs.push(Request::WOpen { handle, path: path.clone(), mode: *mode, excl: *excl });
                for (i, c) in data.chunks(chunk).enumerate() {
                    reqs.push(Request::WData { handle, offset: (i * chunk) as u64, data: c.to_vec() });
                }
                reqs.push(Request::WCommit { handle });
                *replies = reqs.len() as u32;
            }
        }
        for r in &reqs {
            self.send(id, r);
        }
    }

    fn finish(&mut self, op: u32, result: Result<Outcome, FilesError>) {
        self.ops.remove(&op);
        self.done.push_back(Completion { op, result });
    }

    /// Byte arrivati dal demone. `Err` = errore di protocollo: la
    /// connessione va chiusa.
    fn on_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.dec.push(bytes);
        while let Some(f) = self.dec.next_frame() {
            match f.map_err(|e| e.0)? {
                Frame::Hello(h) => {
                    if self.is_ready() {
                        return Err("secondo saluto".into());
                    }
                    if !(proto::MIN_VERSION..=proto::VERSION).contains(&h.version) {
                        return Err(format!(
                            "versione {} del demone, attese da {} a {}",
                            h.version,
                            proto::MIN_VERSION,
                            proto::VERSION
                        ));
                    }
                    self.link = LinkState::Ready(h);
                    self.generation += 1;
                    let ids: Vec<u32> = self.ops.keys().copied().collect();
                    for id in ids {
                        self.start(id);
                    }
                }
                Frame::Event(e) => self.events.push_back(e),
                Frame::Reply { id, status, body } => {
                    let Some(op) = self.waiting.remove(&id) else {
                        return Err(format!("risposta a una richiesta sconosciuta ({id})"));
                    };
                    self.on_reply(op, status, &body)?;
                }
            }
        }
        Ok(())
    }

    fn on_reply(&mut self, id: u32, status: u32, body: &[u8]) -> Result<(), String> {
        let mut op = self.ops.remove(&id).ok_or("operazione sconosciuta")?;
        match self.advance(id, &mut op, status, body) {
            Ok(None) => {
                self.ops.insert(id, op);
                Ok(())
            }
            Ok(Some(result)) => {
                self.done.push_back(Completion { op: id, result });
                Ok(())
            }
            Err(e) => {
                self.done.push_back(Completion { op: id, result: Err(FilesError::Protocol(e.clone())) });
                Err(e)
            }
        }
    }

    /// Una risposta per l'operazione `op` (tolta dalla tabella): l'esito se
    /// è finita, `None` se continua.
    fn advance(
        &mut self,
        id: u32,
        op: &mut Op,
        status: u32,
        body: &[u8],
    ) -> Result<Option<Result<Outcome, FilesError>>, String> {
        let chunk = self.chunk() as u64;
        let err = (status != 0).then_some(FilesError::Errno(status));
        match &mut op.work {
            Work::Simple(req) => Ok(Some(match err {
                Some(e) => Err(e),
                None => match req {
                    Request::Stat { .. } => Ok(Outcome::Stat(proto::parse_stat(body).map_err(|e| e.0)?)),
                    Request::List { .. } => Ok(Outcome::List(proto::parse_list(body).map_err(|e| e.0)?)),
                    Request::Watch { .. } => Ok(Outcome::Watch(proto::parse_watch(body).map_err(|e| e.0)?)),
                    Request::Sql { .. } => match proto::parse_sql(body).map_err(|e| e.0)? {
                        Ok(r) => Ok(Outcome::Sql(r)),
                        Err((code, message)) => Err(FilesError::Sql { code, message }),
                    },
                    _ => Ok(Outcome::Done),
                },
            })),
            Work::Read { path, offset, left, size, data } => {
                if let Some(e) = err {
                    return Ok(Some(Err(e)));
                }
                let (sz, got) = proto::parse_read(body).map_err(|e| e.0)?;
                let asked = (*left).min(chunk);
                if got.len() as u64 > asked {
                    return Err(format!("{} byte letti, chiesti {asked}", got.len()));
                }
                let size = *size.get_or_insert(sz);
                *offset += got.len() as u64;
                *left -= got.len() as u64;
                data.extend_from_slice(&got);
                if (got.len() as u64) < asked || *left == 0 {
                    return Ok(Some(Ok(Outcome::Data { size, data: core::mem::take(data) })));
                }
                let r = Request::Read { path: path.clone(), offset: *offset, len: (*left).min(chunk) as u32 };
                self.send(id, &r);
                Ok(None)
            }
            Work::Write { replies, err: first, .. } => {
                *replies -= 1;
                if first.is_none() {
                    *first = err;
                }
                if *replies > 0 {
                    return Ok(None);
                }
                // L'ultima risposta è quella di WCOMMIT.
                Ok(Some(match first.take() {
                    Some(e) => Err(e),
                    None => Ok(Outcome::Written(proto::parse_stat(body).map_err(|e| e.0)?)),
                }))
            }
        }
    }

    /// La connessione è finita: le operazioni partite falliscono con
    /// `why`, quelle non partite aspettano il prossimo collegamento.
    fn on_closed(&mut self, why: FilesError) {
        self.conn = None;
        self.dec = Decoder::default();
        self.out.clear();
        self.waiting.clear();
        let started: Vec<u32> = self.ops.iter().filter(|(_, o)| o.started).map(|(k, _)| *k).collect();
        for id in started {
            self.finish(id, Err(why.clone()));
        }
    }

    // ---- Con la macchina -----------------------------------------------------

    fn input(m: &mut Machine, op: VsockOp) -> Reply {
        m.input(Input::Vsock(op))
    }

    /// Fa avanzare il collegamento: connessione (o nuovo tentativo), byte
    /// arrivati, richieste in coda. Da chiamare fra un quanto e l'altro.
    /// Non fa niente senza virtio-vsock, durante un replay (gli ingressi
    /// vengono dal log) e con la macchina ferma su un disco (gli ingressi
    /// sarebbero rimandati).
    pub fn pump(&mut self, m: &mut Machine) {
        if m.blocked() || matches!(m.replay_status(), Some(ReplayStatus::Running { .. })) {
            return;
        }
        let Some(conns) = m.vsock_view(|v| v.connections()) else { return };
        let now = m.guest_ns();
        if !self.cleaned {
            // Connessioni verso il demone rimaste da una sessione precedente
            // (snapshot ripristinato): nessuno le legge più.
            self.cleaned = true;
            for c in conns.iter().filter(|c| c.guest_port == self.port && Some(**c) != self.conn) {
                Self::input(m, VsockOp::Release(*c));
            }
        }
        let Some(c) = self.conn else {
            if let LinkState::Waiting { until_ns } = self.link
                && now < until_ns
            {
                return;
            }
            if let Reply::Conn(Some(c)) = Self::input(m, VsockOp::Connect(self.port)) {
                self.conn = Some(c);
                self.link = LinkState::Connecting;
            }
            return;
        };
        let (state, avail) = m.vsock_view(|v| (v.state(c), v.available(c))).unwrap_or((None, 0));
        if avail > 0
            && let Reply::Data(d) = Self::input(m, VsockOp::Recv(c, avail as u64))
            && let Err(e) = self.on_bytes(&d)
        {
            Self::input(m, VsockOp::Release(c));
            self.on_closed(FilesError::Protocol(e));
            self.link = LinkState::Waiting { until_ns: now + RETRY_NS };
            return;
        }
        if self.is_ready() && !self.out.is_empty() && state == Some(VsockState::Connected) {
            let out = core::mem::take(&mut self.out);
            Self::input(m, VsockOp::Send(c, out));
        }
        let eof = m.vsock_view(|v| v.eof(c)).unwrap_or(true);
        if matches!(state, None | Some(VsockState::Closed)) || (eof && state != Some(VsockState::Connecting))
        {
            Self::input(m, VsockOp::Release(c));
            self.on_closed(FilesError::Disconnected);
            self.link = LinkState::Waiting { until_ns: now + RETRY_NS };
        }
    }

    /// Chiude la connessione (le operazioni partite falliscono).
    pub fn close(&mut self, m: &mut Machine) {
        if let Some(c) = self.conn {
            Self::input(m, VsockOp::Release(c));
            self.on_closed(FilesError::Disconnected);
        }
        self.link = LinkState::Idle;
    }
}

#[cfg(test)]
mod tests {
    //! Il client contro un demone finto in memoria che parla il protocollo
    //! (le prove con il demone vero stanno in tests/boot/tests/files.rs).

    use super::proto::*;
    use super::*;

    /// Un file system finto: percorso → contenuto.
    struct Fake {
        files: BTreeMap<Vec<u8>, Vec<u8>>,
        handles: BTreeMap<u32, (Vec<u8>, Vec<u8>)>,
        max_chunk: u32,
        reads: usize,
    }

    impl Fake {
        fn new(max_chunk: u32) -> Self {
            Fake { files: BTreeMap::new(), handles: BTreeMap::new(), max_chunk, reads: 0 }
        }

        fn stat(size: usize) -> Stat {
            Stat {
                kind: Kind::File,
                mode: 0o100600,
                uid: 1,
                gid: 2,
                size: size as u64,
                mtime_s: 0,
                mtime_ns: 0,
                nlink: 1,
                link: Vec::new(),
                selinux: String::new(),
            }
        }

        /// Serve tutti i frame di `bytes`; restituisce le risposte.
        fn serve(&mut self, bytes: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            let mut at = 0;
            while at < bytes.len() {
                let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                let (id, req) = Request::decode(&bytes[at + 4..at + 4 + len]).unwrap();
                at += 4 + len;
                let (status, body) = match req {
                    Request::Read { path, offset, len } => {
                        self.reads += 1;
                        assert!(len <= self.max_chunk);
                        match self.files.get(&path) {
                            None => (2, vec![]),
                            Some(f) => {
                                let s = (offset as usize).min(f.len());
                                let e = (s + len as usize).min(f.len());
                                (0, encode_read(f.len() as u64, &f[s..e]))
                            }
                        }
                    }
                    Request::WOpen { handle, path, .. } => {
                        self.handles.insert(handle, (path, Vec::new()));
                        (0, vec![])
                    }
                    Request::WData { handle, offset, data } => match self.handles.get_mut(&handle) {
                        None => (9, vec![]),
                        Some((_, buf)) => {
                            assert!(data.len() <= self.max_chunk as usize);
                            assert_eq!(offset as usize, buf.len(), "pezzi in ordine");
                            buf.extend(data);
                            (0, vec![])
                        }
                    },
                    Request::WCommit { handle } => match self.handles.remove(&handle) {
                        None => (9, vec![]),
                        Some((path, buf)) => {
                            let s = Self::stat(buf.len());
                            self.files.insert(path, buf);
                            (0, encode_stat(&s))
                        }
                    },
                    Request::Stat { path } => match self.files.get(&path) {
                        None => (2, vec![]),
                        Some(f) => (0, encode_stat(&Self::stat(f.len()))),
                    },
                    Request::Watch { .. } => (0, 7u32.to_le_bytes().to_vec()),
                    // Un database finto: una tabella `t(v)` di interi.
                    Request::Sql { path, sql, params, expect, readonly } => match self.files.get_mut(&path) {
                        None => (2, vec![]),
                        Some(_) if sql.starts_with("SELECT") => (
                            0,
                            encode_sql_ok(&SqlResult {
                                columns: vec!["a".into(), "b".into(), "c".into()],
                                rows: vec![
                                    params.clone(),
                                    vec![SqlValue::Null, SqlValue::Blob(vec![1]), SqlValue::Int(0)],
                                ],
                                ..SqlResult::default()
                            }),
                        ),
                        Some(_) if readonly => (0, encode_sql_err(8, "attempt to write a readonly database")),
                        Some(_) if expect.is_some_and(|e| e != 1) => {
                            (0, encode_sql_err(19, "1 righe cambiate, attese 2: annullato"))
                        }
                        Some(db) => {
                            db.push(params.len() as u8);
                            (
                                0,
                                encode_sql_ok(&SqlResult {
                                    changes: 1,
                                    last_rowid: -5,
                                    ..SqlResult::default()
                                }),
                            )
                        }
                    },
                    _ => (38, vec![]),
                };
                out.extend(encode_reply(id, status, &body));
            }
            out
        }
    }

    /// Un client che ha chiesto la connessione e aspetta il saluto.
    fn connecting() -> FilesClient {
        FilesClient { link: LinkState::Connecting, ..FilesClient::default() }
    }

    fn hello(max_chunk: u32) -> Vec<u8> {
        encode_hello(&Hello { version: VERSION, flags: 0, max_chunk })
    }

    /// Fa girare client e demone finto finché ci sono byte da scambiare.
    fn exchange(c: &mut FilesClient, d: &mut Fake) {
        while !c.out.is_empty() {
            let req = core::mem::take(&mut c.out);
            let rep = d.serve(&req);
            c.on_bytes(&rep).unwrap();
        }
    }

    fn all(c: &mut FilesClient) -> Vec<Completion> {
        core::iter::from_fn(|| c.take_completion()).collect()
    }

    /// Le operazioni chieste prima del saluto partono al saluto; letture e
    /// scritture a pezzi (il pezzo più piccolo fra quello del client e il
    /// `max_chunk` del demone).
    #[test]
    fn letture_e_scritture_a_pezzi() {
        let mut d = Fake::new(1000);
        let big: Vec<u8> = (0..4321u32).map(|i| (i * 7) as u8).collect();
        let mut c = FilesClient::default();
        let w = c.write_file("/tmp/big", &big, 0o600);
        assert!(c.out.is_empty(), "niente prima del saluto");
        c.link = LinkState::Connecting;
        c.on_bytes(&hello(1000)).unwrap();
        assert!(c.is_ready());
        assert_eq!(c.generation(), 1);
        exchange(&mut c, &mut d);
        assert_eq!(all(&mut c), [Completion { op: w, result: Ok(Outcome::Written(Fake::stat(4321))) }]);
        assert_eq!(d.files[b"/tmp/big".as_slice()], big);

        let r = c.read_file("/tmp/big");
        let part = c.read("/tmp/big", 999, 1002);
        let tail = c.read("/tmp/big", 4000, u64::MAX);
        let missing = c.read_file("/nope");
        exchange(&mut c, &mut d);
        let done = all(&mut c);
        assert_eq!(done.len(), 4);
        let get = |op| done.iter().find(|x| x.op == op).unwrap().result.clone();
        assert_eq!(get(r), Ok(Outcome::Data { size: 4321, data: big.clone() }));
        assert_eq!(get(part), Ok(Outcome::Data { size: 4321, data: big[999..2001].to_vec() }));
        assert_eq!(get(tail), Ok(Outcome::Data { size: 4321, data: big[4000..].to_vec() }));
        assert_eq!(get(missing), Err(FilesError::Errno(2)));
        // 5 pezzi (4 pieni e l'ultimo corto) + 2 + 1 + 1.
        assert_eq!(d.reads, 5 + 2 + 1 + 1);

        // Un file vuoto: WOPEN e WCOMMIT soli; lettura di 0 byte.
        let e = c.write_file("/tmp/vuoto", &[], 0o644);
        exchange(&mut c, &mut d);
        assert!(matches!(all(&mut c)[0], Completion { op, result: Ok(Outcome::Written(_)) } if op == e));
        let r = c.read_file("/tmp/vuoto");
        exchange(&mut c, &mut d);
        assert_eq!(all(&mut c), [Completion { op: r, result: Ok(Outcome::Data { size: 0, data: vec![] }) }]);
    }

    /// Un errore in mezzo a una scrittura la fa fallire con il primo errore.
    #[test]
    fn scrittura_con_errore() {
        let mut c = connecting();
        c.on_bytes(&hello(1 << 20)).unwrap();
        let w = c.write_file("/tmp/x", b"abc", 0o600);
        let reqs = core::mem::take(&mut c.out);
        // WOPEN rifiutato (EACCES), WDATA e WCOMMIT senza handle (EBADF).
        let mut ids = Vec::new();
        let mut at = 0;
        while at < reqs.len() {
            let len = u32::from_le_bytes(reqs[at..at + 4].try_into().unwrap()) as usize;
            ids.push(Request::decode(&reqs[at + 4..at + 4 + len]).unwrap().0);
            at += 4 + len;
        }
        assert_eq!(ids.len(), 3);
        let mut rep = encode_reply(ids[0], 13, &[]);
        rep.extend(encode_reply(ids[1], 9, &[]));
        c.on_bytes(&rep).unwrap();
        assert!(c.take_completion().is_none(), "aspetta WCOMMIT");
        c.on_bytes(&encode_reply(ids[2], 9, &[])).unwrap();
        assert_eq!(c.take_completion(), Some(Completion { op: w, result: Err(FilesError::Errno(13)) }));
        assert_eq!(c.pending(), 0);
    }

    /// Eventi, risposte sconosciute, versioni diverse, connessione caduta.
    #[test]
    fn eventi_ed_errori() {
        let mut c = connecting();
        let mut bad = encode_hello(&Hello { version: VERSION + 1, flags: 0, max_chunk: 1 });
        assert!(c.on_bytes(&bad).is_err(), "versione diversa");
        let mut c = connecting();
        bad = encode_hello(&Hello { version: 0, flags: 0, max_chunk: 1 });
        assert!(c.on_bytes(&bad).is_err(), "versione 0");
        let mut c = connecting();
        c.on_bytes(&encode_hello(&Hello { version: MIN_VERSION, flags: 0, max_chunk: 1 << 20 })).unwrap();
        assert!(c.is_ready(), "un demone di versione 1 va bene (senza SQL)");
        let mut c = connecting();
        c.on_bytes(&hello(1 << 20)).unwrap();
        assert!(c.on_bytes(&hello(1 << 20)).is_err(), "secondo saluto");
        let mut c = connecting();
        c.on_bytes(&hello(1 << 20)).unwrap();
        bad = encode_reply(99, 0, &[]);
        assert!(c.on_bytes(&bad).is_err(), "risposta sconosciuta");

        let mut c = connecting();
        c.on_bytes(&hello(1 << 20)).unwrap();
        let ev = Event { wd: 7, mask: mask::CLOSE_WRITE, cookie: 0, name: "a.txt".into() };
        let w = c.watch("/tmp");
        let mut d = Fake::new(1 << 20);
        exchange(&mut c, &mut d);
        c.on_bytes(&encode_event(&ev)).unwrap();
        assert_eq!(c.take_completion(), Some(Completion { op: w, result: Ok(Outcome::Watch(7)) }));
        assert_eq!(c.take_event(), Some(ev));
        // Una lettura partita e una non ancora partita, poi la connessione
        // cade: la prima fallisce, la seconda aspetta il prossimo saluto.
        let r = c.read_file("/tmp/a");
        c.link = LinkState::Connecting;
        let later = c.stat("/tmp/a");
        c.on_closed(FilesError::Disconnected);
        assert_eq!(c.take_completion(), Some(Completion { op: r, result: Err(FilesError::Disconnected) }));
        assert_eq!(c.pending(), 1);
        c.on_bytes(&hello(1 << 20)).unwrap();
        assert_eq!(c.generation(), 2);
        d.files.insert("/tmp/a".into(), b"x".to_vec());
        exchange(&mut c, &mut d);
        assert_eq!(
            c.take_completion(),
            Some(Completion { op: later, result: Ok(Outcome::Stat(Fake::stat(1))) })
        );
    }

    /// SQL: parametri e righe andata e ritorno, errori di SQLite come
    /// `FilesError::Sql`, database che non c'è come errno.
    #[test]
    fn sql() {
        let mut d = Fake::new(1 << 20);
        d.files.insert(b"/db".to_vec(), Vec::new());
        let mut c = connecting();
        c.on_bytes(&hello(1 << 20)).unwrap();
        let params = vec![SqlValue::Int(i64::MIN), SqlValue::Real(-0.5), SqlValue::Text("à".into())];
        let q = c.sql("/db", "SELECT ?1, ?2, ?3", params.clone(), None, true);
        let u = c.sql("/db", "UPDATE t SET v = ?1 WHERE rowid = ?2", params[..2].to_vec(), Some(1), false);
        let wrong = c.sql("/db", "UPDATE t SET v = 0", vec![], Some(2), false);
        let ro = c.sql("/db", "DELETE FROM t", vec![], None, true);
        let missing = c.sql("/manca", "SELECT 1", vec![], None, true);
        exchange(&mut c, &mut d);
        let done = all(&mut c);
        let get = |op| done.iter().find(|x| x.op == op).unwrap().result.clone();
        assert_eq!(
            get(q),
            Ok(Outcome::Sql(SqlResult {
                columns: vec!["a".into(), "b".into(), "c".into()],
                rows: vec![params, vec![SqlValue::Null, SqlValue::Blob(vec![1]), SqlValue::Int(0)]],
                ..SqlResult::default()
            }))
        );
        assert_eq!(
            get(u),
            Ok(Outcome::Sql(SqlResult { changes: 1, last_rowid: -5, ..SqlResult::default() }))
        );
        assert_eq!(
            get(wrong),
            Err(FilesError::Sql { code: 19, message: "1 righe cambiate, attese 2: annullato".into() })
        );
        assert!(matches!(get(ro), Err(FilesError::Sql { code: 8, .. })));
        assert_eq!(get(missing), Err(FilesError::Errno(2)));
        assert_eq!(d.files[b"/db".as_slice()], [2], "solo l'UPDATE riuscito");
        assert_eq!(get(wrong).unwrap_err().to_string(), "SQLite 19: 1 righe cambiate, attese 2: annullato");
    }

    /// Nomi non UTF-8: i byte arrivano e ripartono uguali.
    #[test]
    fn nomi_non_utf8() {
        let mut d = Fake::new(1 << 20);
        let name = b"/tmp/a\xffb\xc3".to_vec();
        d.files.insert(name.clone(), b"x".to_vec());
        let mut c = connecting();
        c.on_bytes(&hello(1 << 20)).unwrap();
        let r = c.read_file(&name);
        let lossy = c.read_file(String::from_utf8_lossy(&name).as_bytes());
        exchange(&mut c, &mut d);
        let done = all(&mut c);
        assert_eq!(done[0].result, Ok(Outcome::Data { size: 1, data: b"x".to_vec() }));
        assert_eq!((done[0].op, done[1].op), (r, lossy));
        assert_eq!(done[1].result, Err(FilesError::Errno(2)), "con U+FFFD non si riapre");
        let ev = Event { wd: 1, mask: 0, cookie: 0, name: b"\xfe".to_vec() };
        c.on_bytes(&encode_event(&ev)).unwrap();
        assert_eq!(c.take_event(), Some(ev));
        assert_eq!(display_name(&name), "/tmp/a\\xffb\\xc3");
    }

    #[test]
    fn radici() {
        let mut c = FilesClient::default();
        assert_eq!(c.roots(), ["/"]);
        c.set_roots(app_roots("org.example.app"));
        assert_eq!(
            c.roots(),
            [
                "/data/data/org.example.app",
                "/data/user_de/0/org.example.app",
                "/sdcard/Android/data/org.example.app",
                "/sdcard/Android/media/org.example.app"
            ]
        );
    }
}
