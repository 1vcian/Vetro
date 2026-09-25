//! Record & replay nella macchina (M10, ADR 0019, `docs/specs/replay.md`).
//!
//! - **Registrazione** ([`Machine::start_recording`]): ogni
//!   [`Machine::input`] finisce nel log con il numero d'istruzione, un hash
//!   dei registri e i byte usciti dalla console fino a lì; ogni accesso
//!   opaco dell'host (`Machine::device`, `Machine::net` con una chiusura)
//!   come evento opaco. Alla fine dei quanti, a intervalli fissi, uno
//!   snapshot (keyframe). Un ingresso che arriva mentre la macchina è ferma
//!   su un disco ([`Stop::Blocked`]) si applica alla fine del primo quanto
//!   dopo lo sblocco: così il suo istante non dipende da quando arrivano i
//!   dati del disco, e il replay lo ritrova anche con un disco sempre
//!   pronto.
//! - **Replay** ([`Machine::start_replay`], [`Machine::replay_from`],
//!   [`Machine::goto`]): `run` taglia i quanti agli istanti degli eventi (il
//!   JIT riceve come limite la fine del quanto, quindi non li supera mai),
//!   controlla registri e console e applica l'ingresso; alla fine della
//!   registrazione confronta l'impronta dello stato ([`Digest`]).
//!
//! I confini dei quanti non cambiano l'esecuzione (ADR 0014, 0015): per
//! questo un ingresso applicato fra due quanti allo stesso numero
//! d'istruzione dà lo stesso risultato qualunque sia il quanto dell'host.

use vetro_cpu::Access;
use vetro_mmu::{BusError, PhysMemory};
use vetro_platform::virtio::{VirtioGpu, VirtioInput, VirtioNet, VirtioVsock};
use vetro_snapshot::{Snapshot, Writer, hash64};

use super::{Machine, Stop};
use crate::record::{
    CONSOLE_HASH_INIT, Digest, Divergence, Event, EventKind, HostNetOp, Input, Keyframe, Log, ReplayStatus,
    Reply, VsockOp, console_hash,
};

/// Opzioni di una registrazione.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordOptions {
    /// Istruzioni fra due keyframe (snapshot per il salto a
    /// un'istruzione); 0 = nessuno. Con i keyframe il primo si prende alla
    /// partenza, e il log basta da solo a ripartire.
    pub keyframe_every: u64,
}

/// Uscita della console tolta dalla UART: i byte non ancora dati all'host
/// e il conto (byte, hash) dall'inizio della registrazione o del replay.
pub(super) struct ConsoleTap {
    pub(super) buf: Vec<u8>,
    len: u64,
    hash: u64,
}

impl Default for ConsoleTap {
    fn default() -> Self {
        ConsoleTap { buf: Vec::new(), len: 0, hash: CONSOLE_HASH_INIT }
    }
}

pub(super) struct Recorder {
    log: Log,
    next_keyframe: u64,
    /// Ingressi arrivati con la macchina ferma su un disco.
    deferred: Vec<Input>,
}

pub(super) struct Replayer {
    /// Il log senza keyframe.
    log: Log,
    next: usize,
    /// Il replay ha usato il JIT.
    jit: bool,
}

pub(super) enum Rr {
    Off,
    Record(Box<Recorder>),
    Replay(Box<Replayer>),
}

impl Rr {
    pub(super) fn recording(&self) -> bool {
        matches!(self, Rr::Record(_))
    }
    pub(super) fn replaying(&self) -> bool {
        matches!(self, Rr::Replay(_))
    }
    pub(super) fn note_jit(&mut self) {
        match self {
            Rr::Record(r) => r.log.jit = true,
            Rr::Replay(p) => p.jit = true,
            Rr::Off => {}
        }
    }
}

/// Memoria fisica in sola lettura, solo RAM: per le traduzioni del
/// debugger, che non devono toccare i dispositivi.
struct RamOnly<'a>(&'a crate::board::Ram);

impl PhysMemory for RamOnly<'_> {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if self.0.read(pa, buf) { Ok(()) } else { Err(BusError::Decode) }
    }
    fn write(&mut self, _pa: u64, _data: &[u8]) -> Result<(), BusError> {
        Err(BusError::Decode)
    }
}

impl Machine {
    // ---- Ingressi ------------------------------------------------------------

    /// Un ingresso dell'host: l'unico punto da cui, durante una
    /// registrazione, cambia ciò che il guest vede. Registrato con il
    /// numero d'istruzione corrente; arriva al guest prima della prossima
    /// istruzione. Durante un replay si ignora ([`Reply::Ignored`]); durante
    /// una registrazione con la macchina ferma su un disco si rimanda
    /// ([`Reply::Deferred`]).
    pub fn input(&mut self, input: Input) -> Reply {
        match &mut self.rr {
            Rr::Replay(_) => return Reply::Ignored,
            Rr::Record(r) if self.board.borrow().host_wait => {
                r.deferred.push(input);
                return Reply::Deferred;
            }
            Rr::Record(_) => self.log_event(EventKind::Input(input.clone())),
            Rr::Off => {}
        }
        self.apply(&input)
    }

    fn log_event(&mut self, kind: EventKind) {
        let ev = Event { step: self.steps, cpu: self.cpu_hash(), console: self.console_total(), kind };
        if let Rr::Record(r) = &mut self.rr {
            r.log.events.push(ev);
        }
    }

    /// Un accesso con una chiusura a un dispositivo: evento opaco durante
    /// una registrazione.
    pub(super) fn note_opaque(&mut self, slot: Option<u32>) {
        if self.rr.recording() {
            self.log_event(EventKind::Opaque { slot });
        }
    }

    fn apply(&mut self, input: &Input) -> Reply {
        let done = |r: Option<()>| if r.is_some() { Reply::Done } else { Reply::NoDevice };
        match input {
            Input::Console(bytes) => {
                let mut b = self.board.borrow_mut();
                b.virt.uart_mut().push_input(bytes);
                b.irq_dirty = true;
                Reply::Done
            }
            Input::Keyboard(ev) => {
                done(self.device_raw(self.slots.keyboard, |k: &mut VirtioInput| k.inject(ev)))
            }
            Input::Pointer(ev) => {
                done(self.device_raw(self.slots.pointer, |k: &mut VirtioInput| k.inject(ev)))
            }
            Input::Gpio { line, level } => {
                self.board.borrow_mut().gpio_input(*line, *level);
                Reply::Done
            }
            Input::Display { scanout, width, height } => done(
                self.device_raw(self.slots.gpu, |g: &mut VirtioGpu| g.set_display(*scanout, *width, *height)),
            ),
            Input::NetFrame(f) => done(self.net_input_link(|l| l.host_rx.push_back(f.clone()))),
            Input::NetLink(up) => {
                done(self.device_raw(self.slots.net, |d: &mut VirtioNet| d.set_link_up(*up)))
            }
            Input::Vsock(op) => self
                .device_raw(self.slots.vsock, |v: &mut VirtioVsock| match op {
                    VsockOp::Listen(p) => Reply::Vsock(v.listen(*p)),
                    VsockOp::Unlisten(p) => {
                        v.unlisten(*p);
                        Reply::Done
                    }
                    VsockOp::Accept(p) => Reply::Conn(v.accept(*p)),
                    VsockOp::Connect(p) => Reply::Conn(Some(v.connect(*p))),
                    VsockOp::Send(c, d) => Reply::Vsock(v.send(*c, d)),
                    VsockOp::Recv(c, max) => {
                        Reply::Data(v.recv(*c, usize::try_from(*max).unwrap_or(usize::MAX)))
                    }
                    VsockOp::ShutdownSend(c) => {
                        v.shutdown_send(*c);
                        Reply::Done
                    }
                    VsockOp::Close(c) => {
                        v.close(*c);
                        Reply::Done
                    }
                    VsockOp::Reset(c) => {
                        v.reset(*c);
                        Reply::Done
                    }
                    VsockOp::Release(c) => {
                        v.release(*c);
                        Reply::Done
                    }
                    VsockOp::TransportReset => {
                        v.transport_reset();
                        Reply::Done
                    }
                })
                .unwrap_or(Reply::NoDevice),
            Input::HostNet(op) => self
                .net_raw(|s| match op {
                    HostNetOp::Connect(p) => Reply::HostConn(s.host_connect(*p)),
                    HostNetOp::Send(c, d) => Reply::Accepted(s.host_send(*c, d) as u64),
                    HostNetOp::Recv(c, max) => {
                        let mut buf = vec![0; usize::try_from(*max).unwrap_or(usize::MAX).min(1 << 24)];
                        let n = s.host_recv(*c, &mut buf);
                        buf.truncate(n);
                        Reply::Data(buf)
                    }
                    HostNetOp::Shutdown(c) => {
                        s.host_shutdown(*c);
                        Reply::Done
                    }
                    HostNetOp::Abort(c) => {
                        s.host_abort(*c);
                        Reply::Done
                    }
                    HostNetOp::Release(c) => {
                        s.host_release(*c);
                        Reply::Done
                    }
                })
                .unwrap_or(Reply::NoDevice),
        }
    }

    // ---- Console e impronte ----------------------------------------------------

    /// Porta l'uscita della UART nel buffer della macchina, contandola.
    pub(super) fn drain_console(&mut self) {
        let out = self.board.borrow_mut().virt.uart_mut().take_output();
        self.console.len += out.len() as u64;
        self.console.hash = console_hash(self.console.hash, &out);
        self.console.buf.extend(out);
    }

    /// Byte usciti dalla console, compresi quelli ancora nella UART: non
    /// dipende da quando l'host legge.
    fn console_total(&self) -> u64 {
        self.console.len + self.board.borrow().virt.uart().output().len() as u64
    }

    fn cpu_hash(&self) -> u64 {
        let mut w = Writer::with_capacity(2048);
        self.cpu.save(&mut w);
        hash64(w.as_bytes())
    }

    /// Impronta dello stato: istruzioni, CPU, MMU, piattaforma, RAM e
    /// console. Prima porta l'uscita della UART nel buffer della macchina
    /// (l'host la ritrova con [`Machine::console_output`]), così l'impronta
    /// non dipende da quando l'host legge.
    pub fn digest(&mut self) -> Digest {
        self.drain_console();
        let hash = |s: &dyn Fn(&mut Writer)| {
            let mut w = Writer::new();
            s(&mut w);
            hash64(w.as_bytes())
        };
        let b = self.board.borrow();
        Digest {
            steps: self.steps,
            cpu: self.cpu_hash(),
            mmu: hash(&|w| self.mmu.save(w)),
            platform: hash(&|w| b.virt.save(w)),
            ram: hash64(b.ram.bytes()),
            console_len: self.console.len,
            console_hash: self.console.hash,
        }
    }

    // ---- Registrazione -----------------------------------------------------------

    /// Comincia a registrare da qui (dopo `load_linux`, dopo un ripristino,
    /// o in qualsiasi momento fra due `run`). Un replay in corso finisce.
    pub fn start_recording(&mut self, opts: RecordOptions) {
        self.rr = Rr::Off;
        self.drain_console();
        self.console.len = 0;
        self.console.hash = CONSOLE_HASH_INIT;
        let start = self.digest();
        let log = Log {
            config_hash: self.config_hash(),
            config: self.cfg.clone(),
            snapshot_version: vetro_snapshot::FORMAT_VERSION,
            jit: self.jit.is_some(),
            keyframe_every: opts.keyframe_every,
            start,
            events: Vec::new(),
            keyframes: Vec::new(),
            end: Digest::default(),
        };
        self.rr = Rr::Record(Box::new(Recorder { log, next_keyframe: self.steps, deferred: Vec::new() }));
        if opts.keyframe_every > 0 {
            self.keyframe();
        } else if let Rr::Record(r) = &mut self.rr {
            r.next_keyframe = u64::MAX;
        }
    }

    /// Registrazione in corso.
    pub fn is_recording(&self) -> bool {
        self.rr.recording()
    }

    /// Eventi registrati finora.
    pub fn recorded_events(&self) -> usize {
        match &self.rr {
            Rr::Record(r) => r.log.events.len(),
            _ => 0,
        }
    }

    /// Finisce la registrazione e restituisce il log, con l'impronta dello
    /// stato di adesso. Gli ingressi rimandati da una macchina ancora ferma
    /// su un disco non sono mai arrivati al guest e non entrano.
    pub fn stop_recording(&mut self) -> Option<Log> {
        let Rr::Record(r) = core::mem::replace(&mut self.rr, Rr::Off) else { return None };
        let mut log = r.log;
        log.end = self.digest();
        Some(log)
    }

    fn keyframe(&mut self) {
        self.drain_console();
        let snapshot = self.save();
        let (steps, len, hash) = (self.steps, self.console.len, self.console.hash);
        if let Rr::Record(r) = &mut self.rr {
            r.log.keyframes.push(Keyframe { step: steps, console_len: len, console_hash: hash, snapshot });
            r.next_keyframe = steps.saturating_add(r.log.keyframe_every);
        }
    }

    /// Fine di un quanto registrato: keyframe se è ora (mai dopo un evento
    /// allo stesso istante: gli eventi di un istante vengono dopo il suo
    /// keyframe), poi gli ingressi rimandati se la macchina non è più ferma.
    pub(super) fn after_quantum(&mut self) {
        if self.blocked() {
            return;
        }
        let Rr::Record(r) = &mut self.rr else { return };
        let due = self.steps >= r.next_keyframe && r.log.events.last().is_none_or(|e| e.step < self.steps);
        let deferred = core::mem::take(&mut r.deferred);
        if due {
            self.keyframe();
        }
        for i in deferred {
            self.input(i);
        }
    }

    // ---- Replay -------------------------------------------------------------------

    /// Comincia il replay di `log` dallo stato attuale, che dev'essere
    /// quello di partenza della registrazione (stessa configurazione, stessa
    /// impronta: stesso kernel caricato, o stesso snapshot ripristinato).
    pub fn start_replay(&mut self, log: &Log) -> Result<(), Divergence> {
        self.check_config(log)?;
        self.rr = Rr::Off;
        self.drain_console();
        self.console.len = 0;
        self.console.hash = CONSOLE_HASH_INIT;
        let now = self.digest();
        let tlb = !log.jit && self.jit.is_none();
        if let Some(what) = log.start.diff(&now, tlb) {
            return Err(self.refuse(Divergence::Start(format!(
                "lo stato di partenza non è quello registrato ({what})"
            ))));
        }
        self.begin_replay(log, 0);
        Ok(())
    }

    /// Comincia il replay di `log` dall'ultimo keyframe non oltre
    /// l'istruzione `step`; senza keyframe utili, dallo stato attuale come
    /// [`Machine::start_replay`].
    pub fn replay_from(&mut self, log: &Log, step: u64) -> Result<(), Divergence> {
        let Some(k) = log.keyframe_before(step) else { return self.start_replay(log) };
        self.check_config(log)?;
        if log.snapshot_version != vetro_snapshot::FORMAT_VERSION {
            return Err(self.refuse(Divergence::Start(format!(
                "keyframe nel formato di snapshot {}, questa versione legge il {}",
                log.snapshot_version,
                vetro_snapshot::FORMAT_VERSION
            ))));
        }
        self.rr = Rr::Off;
        if let Err(e) = self.load_state(&k.snapshot) {
            return Err(self.refuse(Divergence::Start(format!("keyframe a {}: {e}", k.step))));
        }
        self.console = ConsoleTap { buf: Vec::new(), len: k.console_len, hash: k.console_hash };
        self.drain_console();
        let first = log.events.partition_point(|e| e.step < k.step);
        self.begin_replay(log, first);
        Ok(())
    }

    /// Porta la macchina all'istruzione `step` della registrazione (al primo
    /// confine fra quanti con almeno `step` istruzioni: una WFI può saltare
    /// oltre), ripartendo dal keyframe più vicino e rifacendo gli ingressi.
    /// Restituisce le istruzioni raggiunte; da lì si leggono registri
    /// (`Machine::cpu`) e memoria ([`Machine::read_phys`],
    /// [`Machine::read_virt`]), e il replay può continuare con `run`.
    pub fn goto(&mut self, log: &Log, step: u64) -> Result<u64, Divergence> {
        self.replay_from(log, step)?;
        while self.steps < step {
            let stop = self.run(step - self.steps);
            match &self.replay_status {
                Some(ReplayStatus::Diverged(d)) => return Err(d.clone()),
                Some(ReplayStatus::Finished) => break,
                _ => {}
            }
            if stop != Stop::Budget {
                break;
            }
        }
        Ok(self.steps)
    }

    /// Stato dell'ultimo replay (`None` se non ce n'è stato uno).
    pub fn replay_status(&self) -> Option<&ReplayStatus> {
        self.replay_status.as_ref()
    }

    fn check_config(&mut self, log: &Log) -> Result<(), Divergence> {
        let here = self.config_hash();
        if log.config_hash != here {
            return Err(self.refuse(Divergence::Start(format!(
                "macchina configurata diversamente (hash {:016x}, questa {here:016x}): servono la stessa \
                 RAM, gli stessi dispositivi e lo stesso seme",
                log.config_hash
            ))));
        }
        Ok(())
    }

    fn refuse(&mut self, d: Divergence) -> Divergence {
        self.replay_status = Some(ReplayStatus::Diverged(d.clone()));
        d
    }

    fn begin_replay(&mut self, log: &Log, next: usize) {
        // Senza i keyframe (grandi): al replay servono solo gli eventi.
        let l = Log {
            config_hash: log.config_hash,
            config: log.config.clone(),
            snapshot_version: log.snapshot_version,
            jit: log.jit,
            keyframe_every: log.keyframe_every,
            start: log.start,
            events: log.events.clone(),
            keyframes: Vec::new(),
            end: log.end,
        };
        self.replay_status = Some(ReplayStatus::Running { next });
        self.rr = Rr::Replay(Box::new(Replayer { log: l, next, jit: self.jit.is_some() }));
    }

    /// Fine del replay per una differenza: la macchina continua libera.
    fn diverge(&mut self, d: Divergence, stop: Stop) -> Stop {
        self.rr = Rr::Off;
        self.replay_status = Some(ReplayStatus::Diverged(d));
        stop
    }

    /// Arrivati alla fine della registrazione: si confronta l'impronta.
    fn finish_replay(&mut self, stop: Stop) -> Stop {
        let Rr::Replay(p) = core::mem::replace(&mut self.rr, Rr::Off) else { return stop };
        let now = self.digest();
        let tlb = !p.log.jit && !p.jit;
        self.replay_status = Some(match p.log.end.diff(&now, tlb) {
            None => ReplayStatus::Finished,
            Some(what) => ReplayStatus::Diverged(Divergence::End { what }),
        });
        stop
    }

    pub(super) fn run_replay(&mut self, budget: u64) -> Stop {
        let end = self.steps.saturating_add(budget);
        loop {
            // Gli eventi di questo istante.
            loop {
                let Rr::Replay(p) = &self.rr else { return Stop::Budget };
                let index = p.next;
                let Some(ev) = p.log.events.get(index) else { break };
                if ev.step > self.steps {
                    break;
                }
                let ev = ev.clone();
                if ev.step < self.steps {
                    let d = Divergence::Missed { index, step: ev.step, at: self.steps };
                    return self.diverge(d, Stop::Budget);
                }
                let what = if self.cpu_hash() != ev.cpu {
                    Some("registri")
                } else if self.console_total() != ev.console {
                    Some("byte della console")
                } else {
                    None
                };
                if let Some(what) = what {
                    return self.diverge(Divergence::Event { index, step: ev.step, what }, Stop::Budget);
                }
                match &ev.kind {
                    EventKind::Input(i) => {
                        self.apply(i);
                    }
                    EventKind::Opaque { slot } => {
                        let d = Divergence::Opaque { index, step: ev.step, slot: *slot };
                        return self.diverge(d, Stop::Budget);
                    }
                }
                if let Rr::Replay(p) = &mut self.rr {
                    p.next += 1;
                }
                self.replay_status = Some(ReplayStatus::Running { next: index + 1 });
            }
            let Rr::Replay(p) = &self.rr else { return Stop::Budget };
            let (next, index, last) = (p.log.events.get(p.next).map(|e| e.step), p.next, p.log.end.steps);
            if next.is_none() && self.steps == last {
                return self.finish_replay(Stop::Budget);
            }
            if self.steps > last {
                return self.diverge(Divergence::End { what: "istruzioni" }, Stop::Budget);
            }
            if self.steps >= end {
                return Stop::Budget;
            }
            let target = end.min(next.unwrap_or(last));
            let stop = self.run_quantum(target - self.steps);
            match stop {
                Stop::Budget => {}
                Stop::Blocked => return Stop::Blocked,
                // Ferma da sola: va bene se qui la registrazione aveva un
                // ingresso (la macchina lo aspettava) o la sua fine.
                other => {
                    if next == Some(self.steps) && other == Stop::Idle {
                        continue;
                    }
                    if next.is_none() && self.steps == last {
                        return self.finish_replay(other);
                    }
                    let d = match next {
                        Some(step) => Divergence::Missed { index, step, at: self.steps },
                        None => Divergence::End { what: "istruzioni" },
                    };
                    return self.diverge(d, other);
                }
            }
        }
    }

    // ---- Lettura dello stato -------------------------------------------------------

    /// I registri in forma di testo (per `vetro boot --goto` e i
    /// confronti): istruzioni, PC, SP, NZCV, EL, X0–X30 e i registri di
    /// sistema di EL1 che servono a leggere lo stato del kernel.
    pub fn registers_text(&self) -> String {
        use core::fmt::Write;
        let c = &self.cpu;
        let s = &c.sys;
        let mut t = String::new();
        let _ = writeln!(
            t,
            "istruzioni {}\npc   {:016x}  sp   {:016x}  nzcv {:08x}  el {}  daif {:03x}",
            self.steps, c.pc, c.sp, c.nzcv, s.el, s.daif
        );
        for (row, regs) in c.x.chunks(4).enumerate() {
            let line: Vec<String> = regs
                .iter()
                .enumerate()
                .map(|(i, v)| format!("{:<4} {v:016x}", format!("x{}", row * 4 + i)))
                .collect();
            let _ = writeln!(t, "{}", line.join("  "));
        }
        let sys = [
            ("sp_el0", s.sp_el[0]),
            ("sp_el1", s.sp_el[1]),
            ("elr_el1", s.elr_el1),
            ("spsr_el1", s.spsr_el1),
            ("esr_el1", s.esr_el1),
            ("far_el1", s.far_el1),
            ("vbar_el1", s.vbar_el1),
            ("sctlr_el1", s.sctlr_el1),
            ("tcr_el1", s.tcr_el1),
            ("ttbr0_el1", s.ttbr0_el1),
            ("ttbr1_el1", s.ttbr1_el1),
            ("tpidr_el0", c.tpidr_el0),
        ];
        for pair in sys.chunks(3) {
            let line: Vec<String> = pair.iter().map(|(n, v)| format!("{n:<9} {v:016x}")).collect();
            let _ = writeln!(t, "{}", line.join("  "));
        }
        t
    }

    /// Legge la RAM all'indirizzo fisico `pa`; falso fuori dalla RAM.
    pub fn read_phys(&self, pa: u64, buf: &mut [u8]) -> bool {
        self.board.borrow().ram.read(pa, buf)
    }

    /// Traduce l'indirizzo virtuale `va` con le tabelle correnti e
    /// l'EL corrente (senza TLB, senza toccare dispositivi).
    pub fn translate(&self, va: u64) -> Option<u64> {
        let b = self.board.borrow();
        self.mmu.walk(&mut RamOnly(&b.ram), va, Access::Read, self.cpu.sys.el).ok().map(|t| t.pa)
    }

    /// Legge la memoria all'indirizzo virtuale `va` (traduzione come
    /// [`Machine::translate`], pagina per pagina). Errore: il primo
    /// indirizzo virtuale non leggibile.
    pub fn read_virt(&self, va: u64, buf: &mut [u8]) -> Result<(), u64> {
        let mut done = 0usize;
        while done < buf.len() {
            let at = va.wrapping_add(done as u64);
            let n = (4096 - (at & 4095) as usize).min(buf.len() - done);
            let pa = self.translate(at).ok_or(at)?;
            if !self.read_phys(pa, &mut buf[done..done + n]) {
                return Err(at);
            }
            done += n;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::tests::{IRQ, IRQ_AT, MAIN, R, SVC, cfg};
    use super::super::tests::{Gate, blk_machine};
    use super::*;
    use crate::{Devices, MachineConfig};
    use vetro_cpu::Cpu;
    use vetro_platform::virtio::VirtioBlk;

    /// Il ciclo della sonda di `snapshot.rs` con in più l'eco della UART
    /// (`tools/a64asm.sh`): accende la UART, poi a ogni giro legge UARTFR e, se c'è un byte,
    /// lo legge, lo mescola in x21 e lo rimanda. Così un ingresso della
    /// console cambia i registri, e un ingresso arrivato un'istruzione
    /// prima o dopo cambia i punti interrotti dal timer (e la RAM).
    const ECHO: [u32; 14] = [
        0x52806025, // mov w5, #0x301 (UARTEN, TXE, RXE)
        0xb9003125, // str w5, [x9, #0x30] (UARTCR)
        0x91000442, // loop: add x2, x2, #0x1
        0xb9401923, // ldr w3, [x9, #0x18]
        0x372000a3, // tbnz w3, #0x4, idle
        0xb9400124, // ldr w4, [x9]
        0x12001c84, // and w4, w4, #0xff
        0xcad52495, // eor x21, x4, x21, ror #9
        0xb9000124, // str w4, [x9]
        0xf2402c5f, // idle: tst x2, #0xfff
        0x54ffff01, // b.ne loop
        0xd4000001, // svc #0
        0xd503207f, // wfi
        0x17fffff5, // b loop
    ];

    fn echo_probe() -> Machine {
        let mut m = Machine::with_devices(&cfg(), &Devices::none());
        {
            let mut b = m.board.borrow_mut();
            let main = &MAIN[..25]; // fino a `mov x2, #0`
            for (base, code) in
                [(R, main), (R + 4 * 25, &ECHO[..]), (R + 0xa00, &SVC[..]), (IRQ_AT, &IRQ[..])]
            {
                for (i, w) in code.iter().enumerate() {
                    assert!(b.ram.write(base + 4 * i as u64, &w.to_le_bytes()));
                }
            }
        }
        m.cpu.pc = R;
        m
    }

    const END: u64 = 300_000;

    /// Ingressi dell'host: al primo confine con almeno `step` istruzioni.
    fn schedule() -> Vec<(u64, Input)> {
        vec![
            (1_000, Input::Console(b"ciao".to_vec())),
            (1_000, Input::Console(b"!".to_vec())),
            (47_111, Input::Gpio { line: 3, level: true }),
            (90_001, Input::Console(b"vetro\n".to_vec())),
            (150_000, Input::Gpio { line: 3, level: false }),
            (222_222, Input::Console((0..=255).collect())),
        ]
    }

    /// Esito di una sessione: console, impronta finale e stati nei punti
    /// chiesti (istruzioni, CPU, hash della RAM).
    struct Session {
        out: Vec<u8>,
        end: Digest,
        at: Vec<(u64, Cpu, u64)>,
    }

    /// Esegue la sonda fino a `END` a quanti di `q`, dando gli ingressi di
    /// `inputs` e fermandosi esattamente in ogni punto di `stops`.
    fn drive(m: &mut Machine, q: u64, mut inputs: Vec<(u64, Input)>, stops: &[u64]) -> Session {
        inputs.reverse();
        let mut out = Vec::new();
        let mut at = Vec::new();
        let mut stops: Vec<u64> = stops.iter().rev().copied().collect();
        while m.steps < END {
            while stops.last().is_some_and(|&s| s <= m.steps) {
                stops.pop();
                at.push((m.steps, m.cpu.clone(), hash64(m.board.borrow().ram.bytes())));
            }
            while inputs.last().is_some_and(|i| i.0 <= m.steps) {
                let (_, i) = inputs.pop().unwrap();
                assert_eq!(m.input(i), Reply::Done);
            }
            let limit = [END, inputs.last().map_or(END, |i| i.0), stops.last().copied().unwrap_or(END)]
                .into_iter()
                .filter(|&l| l > m.steps)
                .min()
                .unwrap_or(END);
            let s = m.run(q.min(limit - m.steps));
            out.extend(m.console_output());
            assert_eq!(s, Stop::Budget);
        }
        let end = m.digest();
        out.extend(m.console_output());
        Session { out, end, at }
    }

    fn record(stops: &[u64]) -> (Log, Session) {
        let mut m = echo_probe();
        m.start_recording(RecordOptions { keyframe_every: 40_000 });
        let s = drive(&mut m, 5_000, schedule(), stops);
        let log = m.stop_recording().unwrap();
        assert!(!m.is_recording());
        assert_eq!(log.end, s.end);
        // Il log fa andata e ritorno dal file.
        let log = Log::decode(&log.encode()).unwrap();
        (log, s)
    }

    /// Replay fino alla fine a quanti di `q`: console e stato finale.
    fn replay(m: &mut Machine, q: u64) -> (Vec<u8>, Digest) {
        let mut out = Vec::new();
        for _ in 0..1_000_000 {
            if !matches!(m.replay_status(), Some(ReplayStatus::Running { .. })) {
                break;
            }
            assert_eq!(m.input(Input::Console(b"x".to_vec())), Reply::Ignored, "in replay l'host non entra");
            m.run(q);
            out.extend(m.console_output());
        }
        let d = m.digest();
        out.extend(m.console_output());
        (out, d)
    }

    /// Il criterio di M10 sulla sonda: la registrazione, rifatta con quanti
    /// diversi dall'inizio o da un keyframe su una macchina nuova, dà la
    /// stessa console, le stesse istruzioni e lo stesso stato; l'eco prova
    /// che gli ingressi sono arrivati.
    #[test]
    fn registra_e_riproduci_la_sonda() {
        let (log, rec) = record(&[]);
        let text = String::from_utf8_lossy(&rec.out);
        assert!(text.contains("ciao!") && text.contains("vetro\n"), "eco della console: {text:?}");
        assert_eq!(log.events.len(), schedule().len());
        assert_eq!(log.events[0].step, log.events[1].step, "due ingressi nello stesso istante");
        assert!(log.keyframes.len() >= 7, "{:?}", log.keyframes);
        assert_eq!(log.keyframes[0].step, 0);

        for q in [1, 7_919, 1 << 40] {
            let mut m = echo_probe();
            m.start_replay(&log).unwrap();
            let (out, end) = if q == 1 {
                // Un'istruzione alla volta per un tratto (confini
                // dappertutto, eventi compresi), poi a quanti grandi.
                for _ in 0..3_000 {
                    m.run(1);
                }
                let mut out = m.console_output();
                let (o, e) = replay(&mut m, 50_000);
                out.extend(o);
                (out, e)
            } else {
                replay(&mut m, q)
            };
            assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished), "quanto {q}");
            assert!(out == rec.out, "quanto {q}: console");
            assert_eq!(end, rec.end, "quanto {q}");
        }

        // Da un keyframe, su una macchina nuova senza la sonda in RAM.
        let mut m = Machine::with_devices(&cfg(), &Devices::none());
        m.replay_from(&log, 150_000).unwrap();
        assert_eq!(m.steps, log.keyframe_before(150_000).unwrap().step);
        let (_, end) = replay(&mut m, 3_001);
        assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished));
        assert_eq!(end, rec.end);
    }

    /// `goto` riporta la macchina a un'istruzione con i registri e la RAM
    /// dell'esecuzione registrata in quel punto, da qualsiasi stato di
    /// partenza, e da lì il replay continua fino alla fine.
    #[test]
    fn goto_come_l_esecuzione_diretta() {
        let targets = [0, 999, 1_000, 1_001, 40_000, 47_112, 123_457, 222_222, 299_999];
        let (log, rec) = record(&targets);
        assert_eq!(rec.at.len(), targets.len());
        let mut m = Machine::with_devices(&cfg(), &Devices::none());
        for (target, cpu, ram) in rec.at.iter().rev() {
            let reached = m.goto(&log, *target).unwrap();
            assert_eq!(reached, *target);
            assert_eq!(m.cpu, *cpu, "registri a {target}");
            assert_eq!(hash64(m.board.borrow().ram.bytes()), *ram, "RAM a {target}");
        }
        // Lettura della memoria al punto: la somma dei punti interrotti.
        m.goto(&log, 123_457).unwrap();
        let mut w = [0u8; 8];
        assert!(m.read_phys(R + 0x4008, &mut w));
        let mut v = [0u8; 8];
        m.read_virt(R + 0x4008, &mut v).unwrap(); // MMU spenta: identità
        assert_eq!(w, v);
        assert!(u64::from_le_bytes(w) > 0);
        assert_eq!(m.read_virt(0x1_0000_0000_0000, &mut v), Err(0x1_0000_0000_0000));
        let (_, end) = replay(&mut m, 10_000);
        assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished));
        assert_eq!(end, rec.end);
    }

    /// Un ingresso che sfugge al log (dato alla UART senza passare da
    /// `Machine::input`) si vede: il replay si ferma con una differenza al
    /// primo evento dopo che il guest l'ha letto (l'evento 2 cade nello
    /// stesso istante del byte di contrabbando, prima che il guest lo
    /// legga: i registri sono ancora quelli giusti).
    #[test]
    fn ingresso_sfuggito_si_vede() {
        let mut m = echo_probe();
        m.start_recording(RecordOptions::default());
        let mut inputs = schedule();
        inputs.truncate(4);
        let first = inputs.split_off(2);
        drive_until(&mut m, 60_000, inputs);
        // Il byte di contrabbando arriva fra due ingressi registrati.
        m.board.borrow_mut().virt.uart_mut().push_input(b"?");
        drive(&mut m, 5_000, first, &[]);
        let log = m.stop_recording().unwrap();
        let mut n = echo_probe();
        n.start_replay(&log).unwrap();
        replay(&mut n, 10_000);
        match n.replay_status() {
            Some(ReplayStatus::Diverged(Divergence::Event { index: 3, step, what: "registri" })) => {
                assert_eq!(*step, log.events[3].step)
            }
            other => panic!("atteso l'evento 3 diverso: {other:?}"),
        }

        // Un accesso opaco a un dispositivo: il replay si ferma lì.
        let mut m = echo_probe();
        m.start_recording(RecordOptions::default());
        m.run(1_000);
        m.device::<VirtioBlk, _>(Some(0), |_| ());
        m.run(1_000);
        let log = m.stop_recording().unwrap();
        assert_eq!(log.events[0].kind, EventKind::Opaque { slot: Some(0) });
        let mut n = echo_probe();
        n.start_replay(&log).unwrap();
        n.run(10_000);
        assert_eq!(
            n.replay_status(),
            Some(&ReplayStatus::Diverged(Divergence::Opaque { index: 0, step: 1_000, slot: Some(0) }))
        );
        assert!(!n.rr.replaying(), "dopo la differenza la macchina è libera");
    }

    /// Come `drive`, fino a `end` istruzioni.
    fn drive_until(m: &mut Machine, end: u64, mut inputs: Vec<(u64, Input)>) {
        inputs.reverse();
        while m.steps < end {
            while inputs.last().is_some_and(|i| i.0 <= m.steps) {
                m.input(inputs.pop().unwrap().1);
            }
            let limit = inputs.last().map_or(end, |i| i.0.min(end));
            assert_eq!(m.run(5_000.min(limit - m.steps)), Stop::Budget);
        }
    }

    /// Un log non si applica a una macchina configurata diversamente o in
    /// un altro stato di partenza.
    #[test]
    fn log_di_un_altra_macchina_rifiutato() {
        let (log, _) = record(&[]);
        let mut other =
            Machine::with_devices(&MachineConfig { ram_size: 2 << 20, ..cfg() }, &Devices::none());
        assert!(matches!(other.start_replay(&log), Err(Divergence::Start(_))));
        let mut moved = echo_probe();
        moved.run(10);
        let e = moved.start_replay(&log).unwrap_err();
        assert!(e.to_string().contains("stato di partenza"), "{e}");
        assert!(!moved.rr.replaying());
        let mut wrong = log.clone();
        wrong.snapshot_version += 1;
        let e = Machine::with_devices(&cfg(), &Devices::none()).replay_from(&wrong, 50_000).unwrap_err();
        assert!(e.to_string().contains("formato di snapshot"), "{e}");
    }

    /// Un ingresso arrivato mentre la macchina aspetta un disco
    /// (`Stop::Blocked`) si applica alla fine del primo quanto dopo lo
    /// sblocco; il replay con un disco sempre pronto dà la stessa
    /// esecuzione.
    #[test]
    fn ingresso_con_il_disco_in_attesa() {
        let (mut m, slot) = blk_machine(false);
        m.start_recording(RecordOptions { keyframe_every: 1 });
        assert_eq!(m.run(1000), Stop::Blocked);
        assert_eq!(m.input(Input::Console(b"k".to_vec())), Reply::Deferred);
        assert_eq!(m.run(1000), Stop::Blocked);
        assert_eq!(m.recorded_events(), 0, "rimandato, non ancora registrato");
        m.host_link::<VirtioBlk, _>(Some(slot), |b| b.backend_as_mut::<Gate>().unwrap().open = true).unwrap();
        assert_eq!(m.run(500), Stop::Budget);
        let at = m.steps;
        assert_eq!(m.recorded_events(), 1);
        m.run(500);
        let log = m.stop_recording().unwrap();
        assert_eq!(log.events[0].step, at, "registrato alla fine del quanto dopo lo sblocco");
        assert_eq!(log.events[0].kind, EventKind::Input(Input::Console(b"k".to_vec())));
        assert!(log.keyframes.iter().all(|k| k.step != 1), "nessun keyframe con la macchina ferma");

        let (mut n, _) = blk_machine(true);
        n.start_replay(&log).unwrap();
        while matches!(n.replay_status(), Some(ReplayStatus::Running { .. })) {
            assert_eq!(n.run(333), Stop::Budget, "il disco pronto non ferma la macchina");
        }
        assert_eq!(n.replay_status(), Some(&ReplayStatus::Finished));
        assert_eq!(n.digest(), log.end);
        assert_eq!(n.board.borrow().virt.uart().pending_input(), 1, "il byte è arrivato alla UART");
    }
}
