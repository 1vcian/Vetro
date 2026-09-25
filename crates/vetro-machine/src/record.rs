//! Record & replay (M10, ADR 0019, `docs/specs/replay.md`): gli ingressi
//! dell'host e il log che li registra.
//!
//! La macchina è deterministica (tempo = istruzioni, ADR 0011; tempo fermo
//! sui dischi, ADR 0014): due esecuzioni dallo stesso stato con gli stessi
//! ingressi agli stessi numeri d'istruzione sono identiche. Gli ingressi
//! dell'host sono quindi l'unica cosa da registrare, e passano tutti da un
//! punto solo, [`Machine::input`](crate::Machine::input) con un [`Input`].
//!
//! Il [`Log`] tiene: la configurazione, l'impronta ([`Digest`]) dello stato
//! di partenza, gli eventi (ingresso con il numero d'istruzione, più un
//! controllo dei registri e della console a quel punto), gli snapshot
//! periodici ([`Keyframe`], ADR 0015) per il salto a un'istruzione, e
//! l'impronta dello stato finale. Il file è un contenitore di
//! `vetro_snapshot` con magia [`LOG_MAGIC`] e versione [`LOG_VERSION`].

use core::fmt;

use vetro_net::ConnId;
use vetro_platform::virtio::{InputEvent, VsockConn, VsockError};
use vetro_snapshot::{Error, Reader, Writer};

use crate::MachineConfig;

/// Primi 8 byte di un log di registrazione.
pub const LOG_MAGIC: [u8; 8] = *b"VETROREC";

/// Versione del formato del log: cambia a ogni modifica di ciò che si
/// scrive (un log di un'altra versione si rifiuta).
pub const LOG_VERSION: u32 = 1;

/// Un ingresso dell'host verso il guest: l'unico modo, durante una
/// registrazione, di cambiare ciò che il guest vede.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// Byte sulla console PL011, come dalla tastiera del terminale.
    Console(Vec<u8>),
    /// Eventi della tastiera virtio-input (con i loro SYN_REPORT).
    Keyboard(Vec<InputEvent>),
    /// Eventi del tablet o del touchscreen virtio-input.
    Pointer(Vec<InputEvent>),
    /// Livello di una linea d'ingresso del GPIO PL061 (la 3 è il tasto di
    /// spegnimento).
    Gpio { line: u32, level: bool },
    /// Risoluzione chiesta per uno scanout di virtio-gpu (0x0 = spento),
    /// come il ridimensionamento della finestra.
    Display { scanout: u32, width: u32, height: u32 },
    /// Un frame Ethernet dell'host per il guest, consegnato da virtio-net
    /// prima dei frame dello stack di rete.
    NetFrame(Vec<u8>),
    /// Link di virtio-net su (vero) o giù.
    NetLink(bool),
    /// Un'operazione dell'host su virtio-vsock.
    Vsock(VsockOp),
    /// Un'operazione dell'host su una sua connessione TCP verso il guest
    /// (inoltro di porte, `Stack::host_*` di `vetro-net`).
    HostNet(HostNetOp),
}

/// Operazioni dell'host sulle connessioni verso il guest (i metodi
/// `Stack::host_*`). Anche le letture sono ingressi: liberano spazio e
/// riaprono la finestra TCP del guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostNetOp {
    /// `host_connect(porta del guest)`.
    Connect(u16),
    /// `host_send(connessione, byte)`.
    Send(ConnId, Vec<u8>),
    /// `host_recv(connessione, al più tanti byte)`.
    Recv(ConnId, u64),
    Shutdown(ConnId),
    Abort(ConnId),
    Release(ConnId),
}

/// Operazioni dell'host su virtio-vsock (i metodi di `VirtioVsock`). Anche
/// le letture sono ingressi: liberano credito, e il guest lo vede.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VsockOp {
    Listen(u32),
    Unlisten(u32),
    Accept(u32),
    Connect(u32),
    Send(VsockConn, Vec<u8>),
    Recv(VsockConn, u64),
    ShutdownSend(VsockConn),
    Close(VsockConn),
    Reset(VsockConn),
    Release(VsockConn),
    TransportReset,
}

/// Risposta della macchina a un [`Input`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// Applicato.
    Done,
    /// Il dispositivo non c'è: niente è cambiato.
    NoDevice,
    /// Registrazione in corso e macchina ferma su un disco
    /// ([`Stop::Blocked`](crate::Stop::Blocked)): l'ingresso si applica
    /// (e si registra) alla fine del primo quanto dopo lo sblocco.
    Deferred,
    /// Replay in corso: gli ingressi vengono dal log, quelli dell'host si
    /// ignorano.
    Ignored,
    /// Esito di `listen`/`send`.
    Vsock(Result<(), VsockError>),
    /// Connessione accettata (`accept`) o chiesta (`connect`).
    Conn(Option<VsockConn>),
    /// Byte letti (`recv` di vsock, `host_recv` della rete).
    Data(Vec<u8>),
    /// Connessione dell'host aperta (`host_connect`; `None` senza porte
    /// effimere libere).
    HostConn(Option<ConnId>),
    /// Byte accettati da `host_send`.
    Accepted(u64),
}

impl Input {
    /// Un tasto premuto o rilasciato, con SYN_REPORT (come
    /// `VirtioInput::key`).
    pub fn key_events(code: u16, down: bool) -> Vec<InputEvent> {
        use vetro_platform::virtio::input::EV_KEY;
        vec![InputEvent::new(EV_KEY, code, down.into()), InputEvent::syn()]
    }

    /// Posizione assoluta del tablet, con SYN_REPORT (come
    /// `VirtioInput::move_abs`).
    pub fn move_abs_events(x: u32, y: u32) -> Vec<InputEvent> {
        use vetro_platform::virtio::input::{ABS_X, ABS_Y, EV_ABS};
        vec![
            InputEvent { ty: EV_ABS, code: ABS_X, value: x },
            InputEvent { ty: EV_ABS, code: ABS_Y, value: y },
            InputEvent::syn(),
        ]
    }

    /// Un contatto del touchscreen (come `VirtioInput::touch`).
    pub fn touch_events(slot: u32, pos: Option<(u32, u32)>) -> Vec<InputEvent> {
        use vetro_platform::virtio::input::{
            ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, BTN_TOUCH, EV_ABS, EV_KEY,
        };
        let mut ev = vec![
            InputEvent { ty: EV_ABS, code: ABS_MT_SLOT, value: slot },
            InputEvent::new(EV_ABS, ABS_MT_TRACKING_ID, if pos.is_some() { slot as i32 } else { -1 }),
        ];
        if let Some((x, y)) = pos {
            ev.push(InputEvent { ty: EV_ABS, code: ABS_MT_POSITION_X, value: x });
            ev.push(InputEvent { ty: EV_ABS, code: ABS_MT_POSITION_Y, value: y });
        }
        ev.push(InputEvent::new(EV_KEY, BTN_TOUCH, pos.is_some().into()));
        ev.push(InputEvent::syn());
        ev
    }

    fn save(&self, w: &mut Writer) {
        let events = |w: &mut Writer, ev: &[InputEvent]| {
            w.seq(ev, |w, e| {
                w.u16(e.ty);
                w.u16(e.code);
                w.u32(e.value);
            })
        };
        let conn = |w: &mut Writer, c: &VsockConn| {
            w.u32(c.host_port);
            w.u32(c.guest_port);
        };
        match self {
            Input::Console(b) => {
                w.u8(0);
                w.bytes(b);
            }
            Input::Keyboard(ev) => {
                w.u8(1);
                events(w, ev);
            }
            Input::Pointer(ev) => {
                w.u8(2);
                events(w, ev);
            }
            Input::Gpio { line, level } => {
                w.u8(3);
                w.u32(*line);
                w.bool(*level);
            }
            Input::Display { scanout, width, height } => {
                w.u8(4);
                w.u32(*scanout);
                w.u32(*width);
                w.u32(*height);
            }
            Input::NetFrame(f) => {
                w.u8(5);
                w.bytes(f);
            }
            Input::NetLink(up) => {
                w.u8(6);
                w.bool(*up);
            }
            Input::Vsock(op) => {
                w.u8(7);
                match op {
                    VsockOp::Listen(p) => {
                        w.u8(0);
                        w.u32(*p);
                    }
                    VsockOp::Unlisten(p) => {
                        w.u8(1);
                        w.u32(*p);
                    }
                    VsockOp::Accept(p) => {
                        w.u8(2);
                        w.u32(*p);
                    }
                    VsockOp::Connect(p) => {
                        w.u8(3);
                        w.u32(*p);
                    }
                    VsockOp::Send(c, d) => {
                        w.u8(4);
                        conn(w, c);
                        w.bytes(d);
                    }
                    VsockOp::Recv(c, max) => {
                        w.u8(5);
                        conn(w, c);
                        w.u64(*max);
                    }
                    VsockOp::ShutdownSend(c) => {
                        w.u8(6);
                        conn(w, c);
                    }
                    VsockOp::Close(c) => {
                        w.u8(7);
                        conn(w, c);
                    }
                    VsockOp::Reset(c) => {
                        w.u8(8);
                        conn(w, c);
                    }
                    VsockOp::Release(c) => {
                        w.u8(9);
                        conn(w, c);
                    }
                    VsockOp::TransportReset => w.u8(10),
                }
            }
            Input::HostNet(op) => {
                w.u8(8);
                match op {
                    HostNetOp::Connect(p) => {
                        w.u8(0);
                        w.u16(*p);
                    }
                    HostNetOp::Send(c, d) => {
                        w.u8(1);
                        w.u64(*c);
                        w.bytes(d);
                    }
                    HostNetOp::Recv(c, max) => {
                        w.u8(2);
                        w.u64(*c);
                        w.u64(*max);
                    }
                    HostNetOp::Shutdown(c) => {
                        w.u8(3);
                        w.u64(*c);
                    }
                    HostNetOp::Abort(c) => {
                        w.u8(4);
                        w.u64(*c);
                    }
                    HostNetOp::Release(c) => {
                        w.u8(5);
                        w.u64(*c);
                    }
                }
            }
        }
    }

    fn load(r: &mut Reader<'_>) -> vetro_snapshot::Result<Input> {
        let events = |r: &mut Reader<'_>| {
            r.seq(8, |r| Ok(InputEvent { ty: r.u16()?, code: r.u16()?, value: r.u32()? }))
        };
        let conn = |r: &mut Reader<'_>| -> vetro_snapshot::Result<VsockConn> {
            Ok(VsockConn { host_port: r.u32()?, guest_port: r.u32()? })
        };
        Ok(match r.u8()? {
            0 => Input::Console(r.vec()?),
            1 => Input::Keyboard(events(r)?),
            2 => Input::Pointer(events(r)?),
            3 => Input::Gpio { line: r.u32()?, level: r.bool()? },
            4 => Input::Display { scanout: r.u32()?, width: r.u32()?, height: r.u32()? },
            5 => Input::NetFrame(r.vec()?),
            6 => Input::NetLink(r.bool()?),
            7 => Input::Vsock(match r.u8()? {
                0 => VsockOp::Listen(r.u32()?),
                1 => VsockOp::Unlisten(r.u32()?),
                2 => VsockOp::Accept(r.u32()?),
                3 => VsockOp::Connect(r.u32()?),
                4 => VsockOp::Send(conn(r)?, r.vec()?),
                5 => VsockOp::Recv(conn(r)?, r.u64()?),
                6 => VsockOp::ShutdownSend(conn(r)?),
                7 => VsockOp::Close(conn(r)?),
                8 => VsockOp::Reset(conn(r)?),
                9 => VsockOp::Release(conn(r)?),
                10 => VsockOp::TransportReset,
                k => return Err(Error::invalid(format!("operazione vsock {k}"))),
            }),
            8 => Input::HostNet(match r.u8()? {
                0 => HostNetOp::Connect(r.u16()?),
                1 => HostNetOp::Send(r.u64()?, r.vec()?),
                2 => HostNetOp::Recv(r.u64()?, r.u64()?),
                3 => HostNetOp::Shutdown(r.u64()?),
                4 => HostNetOp::Abort(r.u64()?),
                5 => HostNetOp::Release(r.u64()?),
                k => return Err(Error::invalid(format!("operazione di rete dell'host {k}"))),
            }),
            k => return Err(Error::invalid(format!("tipo di ingresso {k}"))),
        })
    }
}

/// Che cosa è successo a un evento del log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// Un ingresso dell'host.
    Input(Input),
    /// Un accesso dell'host a un dispositivo che il log non sa descrivere
    /// (`Machine::device` e i suoi derivati, con una chiusura): se ha cambiato
    /// qualcosa, il replay non può rifarlo. Il replay si ferma qui con
    /// [`Divergence::Opaque`].
    Opaque { slot: Option<u32> },
}

/// Un evento del log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// Istruzioni eseguite quando l'ingresso è arrivato: il replay lo
    /// applica fra due quanti esattamente a questo numero.
    pub step: u64,
    /// `hash64` dei registri della CPU (`Cpu` nello snapshot) subito prima
    /// dell'ingresso: il replay lo confronta.
    pub cpu: u64,
    /// Byte usciti dalla console fino a quel momento.
    pub console: u64,
    pub kind: EventKind,
}

/// Snapshot preso durante la registrazione (ADR 0015), per ripartire vicino
/// a un'istruzione senza rifare tutto dall'inizio. Gli eventi con lo stesso
/// `step` vengono dopo lo snapshot.
#[derive(Clone, PartialEq, Eq)]
pub struct Keyframe {
    pub step: u64,
    /// Console fino a questo punto: byte e hash (vedi [`Digest`]).
    pub console_len: u64,
    pub console_hash: u64,
    /// `Machine::save` (con l'uscita della console già tolta dalla UART).
    pub snapshot: Vec<u8>,
}

impl fmt::Debug for Keyframe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keyframe")
            .field("step", &self.step)
            .field("console_len", &self.console_len)
            .field("snapshot", &format_args!("{} byte", self.snapshot.len()))
            .finish()
    }
}

/// Impronta dello stato della macchina in un punto: ciò che il replay deve
/// ritrovare identico.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Digest {
    /// Istruzioni eseguite.
    pub steps: u64,
    /// `hash64` della CPU (registri generali, SIMD/FP, PSTATE, registri di
    /// sistema, monitor esclusivo).
    pub cpu: u64,
    /// `hash64` della MMU con il TLB (col JIT il TLB vede meno accessi,
    /// ADR 0013: non si confronta se una delle due esecuzioni ha il JIT).
    pub mmu: u64,
    /// `hash64` della piattaforma: timer, GIC, UART, RTC, GPIO e tutti i
    /// dispositivi virtio con i loro backend interni (stack di rete, dischi
    /// copy-on-write).
    pub platform: u64,
    /// `hash64` della RAM.
    pub ram: u64,
    /// Byte usciti dalla console dall'inizio della registrazione.
    pub console_len: u64,
    /// Loro hash (FNV-1a a 64 bit, incrementale).
    pub console_hash: u64,
}

impl Digest {
    /// La prima differenza con `other`, se c'è (`tlb`: confronta anche la
    /// MMU).
    pub fn diff(&self, other: &Digest, tlb: bool) -> Option<&'static str> {
        if self.steps != other.steps {
            Some("istruzioni")
        } else if self.console_len != other.console_len || self.console_hash != other.console_hash {
            Some("console")
        } else if self.cpu != other.cpu {
            Some("CPU")
        } else if self.ram != other.ram {
            Some("RAM")
        } else if self.platform != other.platform {
            Some("dispositivi")
        } else if tlb && self.mmu != other.mmu {
            Some("MMU e TLB")
        } else {
            None
        }
    }

    fn save(&self, w: &mut Writer) {
        for v in
            [self.steps, self.cpu, self.mmu, self.platform, self.ram, self.console_len, self.console_hash]
        {
            w.u64(v);
        }
    }

    fn load(r: &mut Reader<'_>) -> vetro_snapshot::Result<Digest> {
        Ok(Digest {
            steps: r.u64()?,
            cpu: r.u64()?,
            mmu: r.u64()?,
            platform: r.u64()?,
            ram: r.u64()?,
            console_len: r.u64()?,
            console_hash: r.u64()?,
        })
    }
}

/// Una registrazione completa.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Log {
    /// `Machine::config_hash` della macchina registrata.
    pub config_hash: u64,
    /// La sua configurazione (per ricostruirla).
    pub config: MachineConfig,
    /// Versione del formato degli snapshot dei [`Keyframe`].
    pub snapshot_version: u32,
    /// La registrazione ha usato il JIT (in qualche momento).
    pub jit: bool,
    /// Istruzioni fra due keyframe (0 = nessuno).
    pub keyframe_every: u64,
    /// Stato di partenza.
    pub start: Digest,
    /// Eventi in ordine di `step` (non decrescente).
    pub events: Vec<Event>,
    /// Keyframe in ordine di `step` crescente.
    pub keyframes: Vec<Keyframe>,
    /// Stato alla fine della registrazione.
    pub end: Digest,
}

/// Perché un log non si può leggere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogError(pub Error);

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Error::BadMagic => write!(f, "non è una registrazione di Vetro (intestazione sconosciuta)"),
            Error::Version { found, expected } => write!(
                f,
                "registrazione nel formato versione {found}, questa versione di Vetro legge solo la {expected}"
            ),
            Error::Checksum => write!(f, "registrazione rovinata (somma di controllo sbagliata)"),
            Error::Truncated => write!(f, "registrazione troncata"),
            e => write!(f, "registrazione non valida: {e}"),
        }
    }
}

impl std::error::Error for LogError {}

impl From<Error> for LogError {
    fn from(e: Error) -> Self {
        LogError(e)
    }
}

impl Log {
    /// Il file: contenitore `vetro_snapshot` ([`LOG_MAGIC`], [`LOG_VERSION`],
    /// hash della configurazione) con le sezioni `HEAD`, `EVTS`, `KEYF`,
    /// `END `.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.section(b"HEAD", |w| {
            w.u32(self.snapshot_version);
            w.u64(self.config.ram_size);
            w.u64(self.config.now_secs);
            w.u64(self.config.seed);
            w.bool(self.jit);
            w.u64(self.keyframe_every);
            self.start.save(w);
        });
        w.section(b"EVTS", |w| {
            w.seq(&self.events, |w, e| {
                w.u64(e.step);
                w.u64(e.cpu);
                w.u64(e.console);
                match &e.kind {
                    EventKind::Input(i) => {
                        w.u8(0);
                        i.save(w);
                    }
                    EventKind::Opaque { slot } => {
                        w.u8(1);
                        w.opt(*slot, Writer::u32);
                    }
                }
            })
        });
        w.section(b"KEYF", |w| {
            w.seq(&self.keyframes, |w, k| {
                w.u64(k.step);
                w.u64(k.console_len);
                w.u64(k.console_hash);
                w.bytes(&k.snapshot);
            })
        });
        w.section(b"END ", |w| self.end.save(w));
        vetro_snapshot::encode_container(&LOG_MAGIC, LOG_VERSION, self.config_hash, w.as_bytes())
    }

    /// Legge un file di [`Log::encode`]: magia, versione, somma di
    /// controllo, poi il contenuto (eventi in ordine, keyframe crescenti).
    pub fn decode(bytes: &[u8]) -> Result<Log, LogError> {
        let (header, payload) = vetro_snapshot::decode_container(&LOG_MAGIC, LOG_VERSION, bytes)?;
        let mut r = Reader::new(payload);
        let mut s = r.section(b"HEAD")?;
        let snapshot_version = s.u32()?;
        let config = MachineConfig { ram_size: s.u64()?, now_secs: s.u64()?, seed: s.u64()? };
        let jit = s.bool()?;
        let keyframe_every = s.u64()?;
        let start = Digest::load(&mut s)?;
        s.finish()?;
        let mut s = r.section(b"EVTS")?;
        let events = s.seq(26, |r| {
            let (step, cpu, console) = (r.u64()?, r.u64()?, r.u64()?);
            let kind = match r.u8()? {
                0 => EventKind::Input(Input::load(r)?),
                1 => EventKind::Opaque { slot: r.opt(|r| r.u32())? },
                k => return Err(Error::invalid(format!("tipo di evento {k}"))),
            };
            Ok(Event { step, cpu, console, kind })
        })?;
        s.finish()?;
        let mut s = r.section(b"KEYF")?;
        let keyframes = s.seq(32, |r| {
            Ok(Keyframe { step: r.u64()?, console_len: r.u64()?, console_hash: r.u64()?, snapshot: r.vec()? })
        })?;
        s.finish()?;
        let mut s = r.section(b"END ")?;
        let end = Digest::load(&mut s)?;
        s.finish()?;
        r.finish()?;
        if events.windows(2).any(|w| w[1].step < w[0].step)
            || events.first().is_some_and(|e| e.step < start.steps)
            || events.last().is_some_and(|e| e.step > end.steps)
        {
            return Err(Error::invalid("eventi fuori ordine").into());
        }
        if keyframes.windows(2).any(|w| w[1].step <= w[0].step) {
            return Err(Error::invalid("keyframe fuori ordine").into());
        }
        Ok(Log {
            config_hash: header.config_hash,
            config,
            snapshot_version,
            jit,
            keyframe_every,
            start,
            events,
            keyframes,
            end,
        })
    }

    /// L'ultimo keyframe non oltre l'istruzione `step`.
    pub fn keyframe_before(&self, step: u64) -> Option<&Keyframe> {
        self.keyframes.iter().rev().find(|k| k.step <= step)
    }

    /// Byte degli eventi nel file (senza keyframe): la parte che cresce con
    /// gli ingressi.
    pub fn events_len(&self) -> usize {
        let mut l = self.clone();
        l.keyframes.clear();
        l.encode().len()
    }
}

/// Perché un replay si è fermato prima della fine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Divergence {
    /// Il log non si applica a questa macchina: configurazione diversa,
    /// stato di partenza diverso, keyframe di un'altra versione.
    Start(String),
    /// A un evento i registri o la console non sono quelli registrati: un
    /// ingresso è sfuggito al log, o la macchina non è deterministica.
    Event { index: usize, step: u64, what: &'static str },
    /// L'esecuzione ha superato l'istruzione di un evento senza fermarcisi
    /// (o si è fermata, inattiva o spenta, prima di arrivarci).
    Missed { index: usize, step: u64, at: u64 },
    /// Un accesso non registrabile ([`EventKind::Opaque`]).
    Opaque { index: usize, step: u64, slot: Option<u32> },
    /// Alla fine lo stato non è quello registrato.
    End { what: &'static str },
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Divergence::Start(why) => write!(f, "il log non si applica a questa macchina: {why}"),
            Divergence::Event { index, step, what } => write!(
                f,
                "evento {index} all'istruzione {step}: {what} diversi dalla registrazione (un ingresso è \
                 sfuggito al log?)"
            ),
            Divergence::Missed { index, step, at } => write!(
                f,
                "evento {index} atteso all'istruzione {step}, la macchina è a {at}: l'esecuzione non è \
                 quella registrata"
            ),
            Divergence::Opaque { index, step, slot } => write!(
                f,
                "evento {index} all'istruzione {step}: accesso dell'host al dispositivo {slot:?} che il log \
                 non descrive, il replay non può continuare"
            ),
            Divergence::End { what } => write!(f, "alla fine della registrazione: {what} diversi"),
        }
    }
}

/// Stato di un replay ([`Machine::replay_status`](crate::Machine::replay_status)).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayStatus {
    /// In corso: `next` è il prossimo evento da applicare.
    Running { next: usize },
    /// Arrivato alla fine della registrazione con lo stesso stato: da qui
    /// la macchina continua libera.
    Finished,
    /// Fermato su una differenza: la macchina continua libera da lì.
    Diverged(Divergence),
}

/// Hash incrementale dei byte della console (FNV-1a a 64 bit).
pub(crate) fn console_hash(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Valore iniziale di [`console_hash`].
pub(crate) const CONSOLE_HASH_INIT: u64 = 0xcbf2_9ce4_8422_2325;

#[cfg(test)]
mod tests {
    use super::*;
    use vetro_platform::virtio::input::BTN_LEFT;

    /// `KEY_A` di Linux.
    const KEY_A: u16 = 30;

    fn sample() -> Log {
        let c = VsockConn { host_port: 1025, guest_port: 7 };
        let inputs = vec![
            Input::Console(b"ls\n".to_vec()),
            Input::Keyboard(Input::key_events(KEY_A, true)),
            Input::Pointer(Input::move_abs_events(3, 4)),
            Input::Pointer(Input::touch_events(1, Some((5, 6)))),
            Input::Pointer(Input::touch_events(1, None)),
            Input::Gpio { line: 3, level: true },
            Input::Display { scanout: 0, width: 800, height: 600 },
            Input::NetFrame(vec![0xff; 60]),
            Input::NetLink(false),
            Input::Vsock(VsockOp::Listen(5000)),
            Input::Vsock(VsockOp::Unlisten(5000)),
            Input::Vsock(VsockOp::Accept(5000)),
            Input::Vsock(VsockOp::Connect(22)),
            Input::Vsock(VsockOp::Send(c, b"ciao".to_vec())),
            Input::Vsock(VsockOp::Recv(c, 99)),
            Input::Vsock(VsockOp::ShutdownSend(c)),
            Input::Vsock(VsockOp::Close(c)),
            Input::Vsock(VsockOp::Reset(c)),
            Input::Vsock(VsockOp::Release(c)),
            Input::Vsock(VsockOp::TransportReset),
            Input::HostNet(HostNetOp::Connect(5555)),
            Input::HostNet(HostNetOp::Send(1, b"CNXN".to_vec())),
            Input::HostNet(HostNetOp::Recv(1, 65536)),
            Input::HostNet(HostNetOp::Shutdown(1)),
            Input::HostNet(HostNetOp::Abort(2)),
            Input::HostNet(HostNetOp::Release(1)),
            Input::Keyboard(Input::key_events(BTN_LEFT, false)),
        ];
        let mut events: Vec<Event> = inputs
            .into_iter()
            .enumerate()
            .map(|(i, input)| Event {
                step: 10 + i as u64,
                cpu: i as u64 * 3,
                console: 1,
                kind: EventKind::Input(input),
            })
            .collect();
        events.push(Event { step: 90, cpu: 1, console: 2, kind: EventKind::Opaque { slot: Some(31) } });
        Log {
            config_hash: 0x1234,
            config: MachineConfig::default(),
            snapshot_version: vetro_snapshot::FORMAT_VERSION,
            jit: true,
            keyframe_every: 50,
            start: Digest { steps: 5, cpu: 1, mmu: 2, platform: 3, ram: 4, console_len: 0, console_hash: 7 },
            events,
            keyframes: vec![
                Keyframe { step: 5, console_len: 0, console_hash: 1, snapshot: vec![1, 2, 3] },
                Keyframe { step: 55, console_len: 9, console_hash: 2, snapshot: vec![4; 100] },
            ],
            end: Digest { steps: 100, ..Digest::default() },
        }
    }

    /// Ogni tipo di ingresso e di evento fa andata e ritorno dal file.
    #[test]
    fn log_andata_e_ritorno() {
        let log = sample();
        let bytes = log.encode();
        assert_eq!(Log::decode(&bytes).unwrap(), log);
        assert_eq!(log.keyframe_before(54).unwrap().step, 5);
        assert_eq!(log.keyframe_before(55).unwrap().step, 55);
        assert!(log.keyframe_before(4).is_none());
        assert!(log.events_len() < bytes.len());
    }

    /// Un log di un'altra versione, rovinato, troncato o incoerente si
    /// rifiuta con un messaggio che dice il motivo.
    #[test]
    fn log_rovinati_rifiutati() {
        let bytes = sample().encode();
        let mut other = bytes.clone();
        other[8..12].copy_from_slice(&(LOG_VERSION + 1).to_le_bytes());
        let e = Log::decode(&other).unwrap_err();
        assert!(e.to_string().contains("formato versione"), "{e}");
        let mut bad = bytes.clone();
        let n = bad.len() - 3;
        bad[n] ^= 1;
        assert_eq!(Log::decode(&bad).unwrap_err().0, Error::Checksum);
        assert!(Log::decode(&bytes[..bytes.len() - 1]).is_err());
        let snap = vetro_snapshot::encode_file(1, b"");
        assert!(Log::decode(&snap).unwrap_err().to_string().contains("non è una registrazione"));
        let mut disordered = sample();
        disordered.events.swap(0, 1);
        assert!(Log::decode(&disordered.encode()).is_err());
    }

    /// Gli aiuti per gli eventi di virtio-input danno esattamente quello che
    /// danno i metodi del dispositivo (stesso stato salvato).
    #[test]
    fn eventi_come_virtio_input() {
        use vetro_platform::VirtioDevice;
        use vetro_platform::virtio::{InputConfig, VirtioInput};
        let state = |d: &VirtioInput| {
            let mut w = Writer::new();
            d.save_state(&mut w);
            w.into_bytes()
        };
        let (mut a, mut b) =
            (VirtioInput::new(InputConfig::multitouch()), VirtioInput::new(InputConfig::multitouch()));
        // Senza driver attivo gli eventi si contano come scartati: basta per
        // confrontare quanti ne escono.
        a.key(KEY_A, true);
        a.move_abs(1, 2);
        a.touch(2, Some((3, 4)));
        a.touch(2, None);
        for ev in [
            Input::key_events(KEY_A, true),
            Input::move_abs_events(1, 2),
            Input::touch_events(2, Some((3, 4))),
            Input::touch_events(2, None),
        ] {
            b.inject(&ev);
        }
        assert_eq!(a.dropped(), 2 + 3 + 6 + 4);
        assert_eq!(state(&a), state(&b));
    }
}
