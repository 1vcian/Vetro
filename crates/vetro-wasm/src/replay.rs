//! Record & replay nel browser (ABI 8, M10, ADR 0019 e 0023).
//!
//! La registrazione e il replay sono quelli di `vetro_machine` (un solo
//! punto d'ingresso, log con eventi e keyframe, impronta alla fine). Qui:
//!
//! - il log finito (o caricato da un file) resta nella [`Vm`]; i keyframe
//!   (snapshot completi, ~10 MB l'uno) si possono **spostare fuori** uno per
//!   uno ([`vetro_log_keyframe_take`]: il JS li scrive in OPFS) e
//!   **rimettere** quando servono ([`vetro_log_keyframe_put`]): nel log
//!   resta solo la loro posizione (istruzione, conto della console);
//! - il file del log ([`vetro_log_encode`]) contiene i keyframe presenti in
//!   quel momento; [`vetro_log_load`] lo rilegge;
//! - [`vetro_replay_start`] rifà la registrazione dal keyframe più vicino a
//!   un'istruzione (serve che sia presente): il JS poi esegue come sempre,
//!   `vetro_run` si ferma agli eventi e alla fine confronta l'impronta. Il
//!   salto a un'istruzione è lo stesso avvio seguito da `vetro_run` con il
//!   quanto limitato fino a lì (come `Machine::goto`, ma con i dischi serviti
//!   dal JS mentre si va);
//! - lettura dello stato al punto raggiunto: registri in testo, memoria
//!   virtuale e fisica.
//!
//! Durante il replay gli ingressi del JS si ignorano (`Reply::Ignored`);
//! il client del gestore dei file si chiude (il JS lo riapre alla fine).

use vetro_analysis::timeline::UserInput;
use vetro_machine::record::{EventKind, Keyframe};
use vetro_machine::{Log, RecordOptions, ReplayStatus};

use crate::Vm;
use crate::analysis::{Describer, vm_ref};

/// Stati di [`vetro_rr_status`].
pub mod rr {
    pub const IDLE: u32 = 0;
    pub const RECORDING: u32 = 1;
    pub const REPLAYING: u32 = 2;
    /// Replay arrivato alla fine con lo stesso stato: replay identico.
    pub const FINISHED: u32 = 3;
    /// Replay fermato su una differenza (motivo nel messaggio).
    pub const DIVERGED: u32 = 4;
}

/// Codici di [`vetro_replay_start`].
pub mod replay_start {
    pub const OK: u32 = 0;
    /// Nessun log (né registrato né caricato).
    pub const NO_LOG: u32 = 1;
    /// Il keyframe da cui partire è fuori (`vetro_log_keyframe_for` dice
    /// quale): va rimesso con `vetro_log_keyframe_put`.
    pub const KEYFRAME_MISSING: u32 = 2;
    /// Il log non si applica a questa macchina (motivo nel messaggio).
    pub const REFUSED: u32 = 3;
}

impl Vm {
    /// Dopo che lo stato della macchina è cambiato da fuori (ripristino,
    /// keyframe): uscita della console non letta scartata, overlay da
    /// riallineare per intero.
    pub(crate) fn after_state_change(&mut self) {
        self.out.clear();
        self.out_pos = 0;
        for o in self.overlays.iter_mut().flatten() {
            o.full_sync = true;
        }
    }

    /// Timeline del replay: gli ingressi dell'utente del log da `from` in poi.
    fn timeline_from_log(&mut self, from: usize) {
        self.timeline.clear();
        self.describer = Describer::default();
        let Some(log) = &self.log else { return };
        let mut d = Describer::default();
        for e in &log.events[from.min(log.events.len())..] {
            if let EventKind::Input(i) = &e.kind
                && let Some((kind, label, weak)) = d.describe(i)
            {
                self.timeline.push_input(UserInput::new(e.step, kind, label, weak));
            }
        }
    }

    /// Log senza i byte dei keyframe (per contarne le dimensioni).
    fn log_events_only(log: &Log) -> Log {
        Log {
            config_hash: log.config_hash,
            config: log.config.clone(),
            snapshot_version: log.snapshot_version,
            jit: log.jit,
            keyframe_every: log.keyframe_every,
            start: log.start,
            events: log.events.clone(),
            keyframes: log
                .keyframes
                .iter()
                .map(|k| Keyframe {
                    step: k.step,
                    console_len: k.console_len,
                    console_hash: k.console_hash,
                    snapshot: Vec::new(),
                })
                .collect(),
            end: log.end,
        }
    }

    fn set_log(&mut self, log: Log) {
        self.kf_sizes = log.keyframes.iter().map(|k| k.snapshot.len() as u64).collect();
        self.log = Some(log);
    }
}

/// Comincia a registrare da qui: ogni ingresso da ora entra nel log, con
/// un keyframe ogni `keyframe_every` istruzioni (0 = nessuno; nel browser
/// serve almeno quello iniziale per rifare la sessione, quindi la pagina
/// ne chiede sempre). Un replay in corso finisce.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_record_start(vm: *mut Vm, keyframe_every: u64) {
    let vm = unsafe { vm_ref(vm) };
    vm.replay_active = false;
    vm.m.start_recording(RecordOptions { keyframe_every });
}

/// Finisce la registrazione; il log resta nella macchina (al posto di
/// quello che c'era). 1 fatto, 0 non si registrava.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_record_stop(vm: *mut Vm) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    match vm.m.stop_recording() {
        Some(log) => {
            vm.set_log(log);
            1
        }
        None => 0,
    }
}

/// Stato di registrazione e replay (codici di [`rr`]); in `out` (al più
/// `cap`): eventi registrati finora (o prossimo evento del replay), eventi
/// del log, keyframe del log, istruzione di partenza e di fine del log, 1
/// se c'è un log. Con `DIVERGED` il motivo è nel messaggio.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_rr_status(vm: *mut Vm, out: *mut u64, cap: usize) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    let (code, progress) = if vm.m.is_recording() {
        (rr::RECORDING, vm.m.recorded_events() as u64)
    } else if vm.replay_active {
        match vm.m.replay_status() {
            Some(ReplayStatus::Running { next }) => (rr::REPLAYING, *next as u64),
            Some(ReplayStatus::Finished) => {
                (rr::FINISHED, vm.log.as_ref().map_or(0, |l| l.events.len() as u64))
            }
            Some(ReplayStatus::Diverged(d)) => {
                vm.message = d.to_string();
                (rr::DIVERGED, 0)
            }
            None => (rr::IDLE, 0),
        }
    } else {
        (rr::IDLE, 0)
    };
    let l = vm.log.as_ref();
    let v = [
        progress,
        l.map_or(0, |l| l.events.len() as u64),
        l.map_or(0, |l| l.keyframes.len() as u64),
        l.map_or(0, |l| l.start.steps),
        l.map_or(0, |l| l.end.steps),
        u64::from(l.is_some()),
    ];
    unsafe { crate::write_u64s(out, cap, &v) };
    code
}

/// Il file del log (formato di `docs/specs/replay.md`) nel buffer dei
/// risultati, con i keyframe presenti in questo momento; 0 senza log.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_encode(vm: *mut Vm) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let bytes = vm.log.as_ref().map(Log::encode).unwrap_or_default();
    vm.set_result(bytes)
}

/// Legge un file di log (al posto del log che c'era). 0 fatto, 1 file non
/// valido (motivo nel messaggio: altra versione, rovinato, troncato).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_load(vm: *mut Vm, data: *const u8, len: usize) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    match Log::decode(unsafe { crate::bytes(data, len) }) {
        Ok(log) => {
            vm.set_log(log);
            0
        }
        Err(e) => {
            vm.message = e.to_string();
            1
        }
    }
}

/// Dati del log in `out` (al più `cap`): istruzione di partenza, di fine,
/// eventi, keyframe, intervallo dei keyframe, 1 se registrato col JIT, 1
/// se è di una macchina configurata come questa, byte degli eventi.
/// Restituisce quanti valori (0 senza log).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_info(vm: *mut Vm, out: *mut u64, cap: usize) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let Some(l) = &vm.log else { return 0 };
    let events_len = Vm::log_events_only(l).encode().len() as u64;
    let v = [
        l.start.steps,
        l.end.steps,
        l.events.len() as u64,
        l.keyframes.len() as u64,
        l.keyframe_every,
        u64::from(l.jit),
        u64::from(l.config_hash == vm.m.config_hash()),
        events_len,
    ];
    unsafe { crate::write_u64s(out, cap, &v) }
}

/// Il keyframe `index` in `out` (al più `cap`): istruzione, byte e hash
/// della console, dimensione dello snapshot, 1 se è presente (0 se è stato
/// preso). Restituisce quanti valori (0 = non c'è).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_keyframe(vm: *mut Vm, index: u32, out: *mut u64, cap: usize) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let Some(k) = vm.log.as_ref().and_then(|l| l.keyframes.get(index as usize)) else { return 0 };
    let size = vm.kf_sizes.get(index as usize).copied().unwrap_or(0);
    let v = [k.step, k.console_len, k.console_hash, size, u64::from(!k.snapshot.is_empty())];
    unsafe { crate::write_u64s(out, cap, &v) }
}

/// Sposta i byte del keyframe `index` nel buffer dei risultati (nel log
/// resta la sua posizione); restituisce la lunghezza, 0 se non c'è o è già
/// fuori.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_keyframe_take(vm: *mut Vm, index: u32) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let bytes = vm
        .log
        .as_mut()
        .and_then(|l| l.keyframes.get_mut(index as usize))
        .map(|k| core::mem::take(&mut k.snapshot))
        .unwrap_or_default();
    vm.set_result(bytes)
}

/// Rimette i byte del keyframe `index` (presi con
/// [`vetro_log_keyframe_take`]). 1 fatto, 0 indice sbagliato o lunghezza
/// diversa da quella del keyframe (se nota: un log riletto da un file senza
/// i byte dei keyframe non la sa).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_keyframe_put(vm: *mut Vm, index: u32, data: *const u8, len: usize) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    let i = index as usize;
    let Some(k) = vm.log.as_mut().and_then(|l| l.keyframes.get_mut(i)) else { return 0 };
    // Dimensione 0 = sconosciuta (log riletto senza i byte dei keyframe).
    let size = vm.kf_sizes.get(i).copied().unwrap_or(0);
    if len == 0 || (size != 0 && size != len as u64) {
        return 0;
    }
    k.snapshot = unsafe { crate::bytes(data, len) }.to_vec();
    if let Some(s) = vm.kf_sizes.get_mut(i) {
        *s = len as u64;
    }
    1
}

/// Indice del keyframe da cui parte il replay verso l'istruzione `step`
/// (l'ultimo non oltre), -1 se nessuno.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_keyframe_for(vm: *mut Vm, step: u64) -> i32 {
    let vm = unsafe { vm_ref(vm) };
    let Some(l) = &vm.log else { return -1 };
    l.keyframes.iter().rposition(|k| k.step <= step).map_or(-1, |i| i as i32)
}

/// Gli eventi del log in JSON: `[{"i":0,"step":N,"kind":"console",
/// "label":"...","weak":false,"user":true}, ...]`; gli eventi che non sono
/// azioni dell'utente (vsock, rete dell'host, rilasci) hanno `user: false`
/// e il tipo dell'ingresso.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_events(vm: *mut Vm) -> usize {
    use std::fmt::Write as _;
    use vetro_analysis::net::json::quote_into;
    use vetro_machine::Input;
    let vm = unsafe { vm_ref(vm) };
    let Some(l) = &vm.log else { return vm.set_result(Vec::new()) };
    let mut d = Describer::default();
    let mut out = String::from("[");
    for (i, e) in l.events.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let (kind, label, weak, user) = match &e.kind {
            EventKind::Opaque { slot } => ("opaco", format!("accesso opaco allo slot {slot:?}"), true, false),
            EventKind::Input(input) => match d.describe(input) {
                Some((k, label, weak)) => (k.name(), label, weak, true),
                None => {
                    let k = match input {
                        Input::Console(_) => "console",
                        Input::Keyboard(_) => "tasto",
                        Input::Pointer(_) => "puntatore",
                        Input::Gpio { .. } => "gpio",
                        Input::Display { .. } => "schermo",
                        Input::NetFrame(_) | Input::NetLink(_) | Input::HostNet(_) => "rete",
                        Input::Vsock(_) => "vsock",
                    };
                    (k, String::new(), true, false)
                }
            },
        };
        let _ = write!(out, "{{\"i\":{i},\"step\":{},\"kind\":\"{kind}\",\"label\":", e.step);
        quote_into(&mut out, &label);
        let _ = write!(out, ",\"weak\":{weak},\"user\":{user}}}");
    }
    out.push(']');
    vm.set_result(out.into_bytes())
}

/// Comincia il replay del log dall'ultimo keyframe non oltre `step` (0 =
/// dall'inizio della registrazione), che dev'essere presente. Codici di
/// [`replay_start`]. Da qui `vetro_run` rifà la sessione (si ferma agli
/// eventi; alla fine confronta l'impronta: `vetro_rr_status`); per saltare
/// a `step` il JS esegue con il quanto limitato fino a lì. La cattura e la
/// timeline ripartono (la timeline con gli ingressi del log), il client del
/// gestore dei file si chiude.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_replay_start(vm: *mut Vm, step: u64) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    let Some(log) = vm.log.take() else { return replay_start::NO_LOG };
    let step = step.max(log.start.steps);
    let kf = log.keyframes.iter().rposition(|k| k.step <= step);
    if kf.is_some_and(|i| log.keyframes[i].snapshot.is_empty()) {
        vm.log = Some(log);
        vm.message =
            "il keyframe da cui partire non è presente: rimetterlo con vetro_log_keyframe_put".into();
        return replay_start::KEYFRAME_MISSING;
    }
    vm.files = None;
    vm.files_queue.clear();
    // Senza keyframe utili si parte dallo stato attuale (dev'essere quello
    // di partenza).
    let r = vm.m.replay_from(&log, step);
    let from = kf.map_or(0, |i| log.events.partition_point(|e| e.step < log.keyframes[i].step));
    vm.log = Some(log);
    match r {
        Ok(()) => {
            vm.replay_active = true;
            vm.after_state_change();
            vm.capture_clear();
            vm.timeline_from_log(from);
            replay_start::OK
        }
        Err(d) => {
            vm.message = d.to_string();
            replay_start::REFUSED
        }
    }
}

/// I registri al punto raggiunto (`Machine::registers_text`) nel buffer dei
/// risultati.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_registers_text(vm: *mut Vm) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let t = vm.m.registers_text();
    vm.set_result(t.into_bytes())
}

/// Legge `len` byte all'indirizzo virtuale `va` (tabelle correnti, EL
/// corrente, solo RAM, senza toccare i dispositivi) in `dst`. 1 fatto; 0 se
/// un indirizzo non è leggibile: il primo va in `fault`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_read_virt(
    vm: *mut Vm,
    va: u64,
    dst: *mut u8,
    len: usize,
    fault: *mut u64,
) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    if len == 0 {
        return 1;
    }
    // SAFETY: `dst` vale per `len` byte (contratto dell'API).
    let buf = unsafe { core::slice::from_raw_parts_mut(dst, len) };
    match vm.m.read_virt(va, buf) {
        Ok(()) => 1,
        Err(at) => {
            if !fault.is_null() {
                unsafe { *fault = at };
            }
            0
        }
    }
}

/// Traduce l'indirizzo virtuale `va`; `u64::MAX` se non è mappato.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_translate(vm: *mut Vm, va: u64) -> u64 {
    unsafe { vm_ref(vm) }.m.translate(va).unwrap_or(u64::MAX)
}

/// Legge `len` byte di RAM all'indirizzo fisico `pa`. 1 fatto, 0 fuori
/// dalla RAM.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_read_phys(vm: *mut Vm, pa: u64, dst: *mut u8, len: usize) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    if len == 0 {
        return 1;
    }
    // SAFETY: `dst` vale per `len` byte.
    let buf = unsafe { core::slice::from_raw_parts_mut(dst, len) };
    vm.m.read_phys(pa, buf) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::*;
    use crate::{
        dev, vetro_console_write, vetro_input_key, vetro_machine_free, vetro_machine_new_with, vetro_run,
    };
    use vetro_analysis::net::json::{self, Value};

    fn new(ram: u64) -> *mut Vm {
        vetro_machine_new_with(ram, 0, 0, dev::KEYBOARD | dev::NET, 0, 0)
    }

    fn result(vm: *mut Vm, n: usize) -> Vec<u8> {
        unsafe { crate::bytes(vetro_result_ptr(vm), n) }.to_vec()
    }

    fn status(vm: *mut Vm) -> (u32, [u64; 6]) {
        let mut v = [0u64; 6];
        let c = unsafe { vetro_rr_status(vm, v.as_mut_ptr(), 6) };
        (c, v)
    }

    /// Esegue fino all'istruzione `to` (a quanti di al più 300).
    fn run_to(vm: *mut Vm, to: u64) {
        loop {
            let now = unsafe { (*vm).m.steps };
            if now >= to {
                break;
            }
            unsafe { vetro_run(vm, (to - now).min(300)) };
        }
    }

    /// Una macchina senza kernel (il PC di reset non è in RAM: eccezioni a
    /// ripetizione, deterministiche) registrata con ingressi della console
    /// e della tastiera e keyframe ogni 500 istruzioni; il log passa da un
    /// file, i keyframe escono e rientrano come farebbe il JS con OPFS; il
    /// replay su un'altra macchina finisce con lo stesso stato; il salto a
    /// un'istruzione ritrova i registri registrati lì; un log di un'altra
    /// macchina si rifiuta.
    #[test]
    fn registrazione_e_replay_dall_api() {
        let a = new(64 << 20);
        unsafe {
            vetro_run(a, 1000);
            vetro_record_start(a, 500);
            assert_eq!(status(a).0, rr::RECORDING);
            run_to(a, 1700);
            vetro_console_write(a, b"ab\r".as_ptr(), 3);
            run_to(a, 2000);
            vetro_input_key(a, 30, 1);
            vetro_input_key(a, 30, 0);
            run_to(a, 2500);
            let regs = (*a).m.registers_text();
            run_to(a, 4000);
            assert_eq!(vetro_record_stop(a), 1);
            assert_eq!(vetro_record_stop(a), 0);
            let (code, v) = status(a);
            assert_eq!(code, rr::IDLE);
            // Keyframe alla fine dei quanti, appena passate 500 istruzioni dal
            // precedente: 1000, 1600, 2300, 2800, 3400, 4000.
            assert_eq!(v[1..], [3, 6, 1000, 4000, 1], "3 ingressi, 6 keyframe");

            // Timeline: la riga della console e il tasto (il rilascio no).
            let n = vetro_timeline_json(a, 0);
            let t = json::parse(&result(a, n)).unwrap();
            let Some(Value::Array(inputs)) = t.get("inputs") else { panic!() };
            let labels: Vec<&str> = inputs.iter().filter_map(|i| i.get("label")?.as_str()).collect();
            assert_eq!(labels, ["Invio: ab", "A"]);
            assert_eq!(inputs[0].get("step"), Some(&Value::Number("1700".into())));

            let mut info = [0u64; 8];
            assert_eq!(vetro_log_info(a, info.as_mut_ptr(), 8), 8);
            assert_eq!(info[..7], [1000, 4000, 3, 6, 500, 0, 1]);
            let n = vetro_log_events(a);
            let ev = json::parse(&result(a, n)).unwrap();
            let Value::Array(ev) = ev else { panic!() };
            assert_eq!(ev.len(), 3);
            assert_eq!(ev[2].get("user"), Some(&Value::Bool(false)), "rilascio del tasto");
            let n = vetro_log_encode(a);
            let file = result(a, n);
            assert_eq!(&file[..8], b"VETROREC");

            // Un'altra macchina: log dal file, keyframe fuori (in OPFS) e
            // rimessi solo quando servono.
            let b = new(64 << 20);
            assert_eq!(vetro_log_load(b, b"rotto".as_ptr(), 5), 1);
            assert_eq!(vetro_log_load(b, file.as_ptr(), file.len()), 0);
            let mut kfs = Vec::new();
            for (i, step) in [1000, 1600, 2300, 2800, 3400, 4000].into_iter().enumerate() {
                let i = i as u32;
                let n = vetro_log_keyframe_take(b, i);
                assert!(n > 0);
                kfs.push(result(b, n));
                let mut k = [0u64; 5];
                assert_eq!(vetro_log_keyframe(b, i, k.as_mut_ptr(), 5), 5);
                assert_eq!((k[0], k[3], k[4]), (step, n as u64, 0));
            }
            assert_eq!(vetro_log_keyframe_take(b, 0), 0, "già fuori");
            assert_eq!(vetro_log_keyframe_for(b, 0), -1);
            assert_eq!(vetro_log_keyframe_for(b, 2600), 2);
            assert_eq!(vetro_replay_start(b, 0), replay_start::KEYFRAME_MISSING);
            assert_eq!(vetro_log_keyframe_put(b, 0, kfs[0].as_ptr(), kfs[0].len() - 1), 0, "lunghezza");
            assert_eq!(vetro_log_keyframe_put(b, 0, kfs[0].as_ptr(), kfs[0].len()), 1);
            assert_eq!(vetro_replay_start(b, 0), replay_start::OK);
            assert_eq!((*b).m.steps, 1000);
            assert_eq!(status(b).0, rr::REPLAYING);
            // Durante il replay gli ingressi si ignorano e non vanno nella timeline.
            vetro_input_key(b, 31, 1);
            while status(b).0 == rr::REPLAYING {
                vetro_run(b, 777);
            }
            assert_eq!(status(b).0, rr::FINISHED, "{}", (*b).message);
            assert_eq!((*b).m.steps, 4000);
            assert_eq!((*a).m.digest(), (*b).m.digest());
            let n = vetro_timeline_json(b, 0);
            let t = json::parse(&result(b, n)).unwrap();
            let Some(Value::Array(inputs)) = t.get("inputs") else { panic!() };
            assert_eq!(inputs.len(), 2, "gli ingressi del log, non quello ignorato");

            // Salto a 2500 dal keyframe a 2300: gli stessi registri.
            let c = new(64 << 20);
            assert_eq!(vetro_log_load(c, file.as_ptr(), file.len()), 0);
            assert_eq!(vetro_replay_start(c, 2400), replay_start::OK);
            assert_eq!((*c).m.steps, 2300);
            run_to(c, 2500);
            let n = vetro_registers_text(c);
            assert_eq!(String::from_utf8(result(c, n)).unwrap(), regs);
            let mut buf = [0u8; 16];
            let mut fault = 0u64;
            let ram = vetro_platform::map::RAM_BASE;
            assert_eq!(vetro_read_virt(c, ram, buf.as_mut_ptr(), 16, &mut fault), 1, "MMU spenta: VA = PA");
            assert_eq!(vetro_read_virt(c, 0x1000, buf.as_mut_ptr(), 16, &mut fault), 0);
            assert_eq!(fault, 0x1000);
            assert_eq!(vetro_translate(c, ram + 8), ram + 8);
            assert_eq!(vetro_read_phys(c, ram, buf.as_mut_ptr(), 16), 1);
            assert_eq!(vetro_read_phys(c, 0, buf.as_mut_ptr(), 16), 0);

            // Un'altra RAM: rifiutato con il motivo.
            let d = new(128 << 20);
            assert_eq!(vetro_log_load(d, file.as_ptr(), file.len()), 0);
            let mut info = [0u64; 8];
            vetro_log_info(d, info.as_mut_ptr(), 8);
            assert_eq!(info[6], 0, "configurazione diversa");
            assert_eq!(vetro_replay_start(d, 0), replay_start::REFUSED);
            assert!((*d).message.contains("configurata diversamente"), "{}", (*d).message);
            let e = new(64 << 20);
            assert_eq!(vetro_replay_start(e, 0), replay_start::NO_LOG);
            for vm in [a, b, c, d, e] {
                vetro_machine_free(vm);
            }
        }
    }

    /// Cattura: senza rete non si accende; con la rete la lista è vuota e
    /// valida, HAR e pcapng si scrivono. Timeline con ingressi ed effetti
    /// annotati dal JS.
    #[test]
    fn cattura_e_ispettore_dall_api() {
        let none = vetro_machine_new_with(64 << 20, 0, 0, 0, 0, 0);
        let vm = new(64 << 20);
        unsafe {
            assert_eq!(vetro_capture_set(none, 1), 0);
            assert_eq!(vetro_capture_set(vm, 1), 1);
            vetro_run(vm, 1000);
            let mut st = [9u64; 4];
            assert_eq!(vetro_capture_stats(vm, st.as_mut_ptr(), 4), 4);
            assert_eq!(st, [1, 0, 0, 0]);
            let n = vetro_inspect_requests(vm);
            let l = json::parse(&result(vm, n)).unwrap();
            assert_eq!(l.get("requests"), Some(&Value::Array(vec![])));
            assert_eq!(vetro_inspect_request(vm, 0), 0);
            let n = vetro_inspect_har(vm, 0);
            assert!(json::parse(&result(vm, n)).is_ok());
            let n = vetro_inspect_pcapng(vm, 0);
            assert_eq!(result(vm, n)[..4], [0x0a, 0x0d, 0x0d, 0x0a]);
            let v0 = vetro_timeline_version(vm);
            vetro_timeline_input(vm, 4, 0, b"scrivi /tmp/x".as_ptr(), 13);
            assert_eq!(vetro_timeline_effect(vm, 3, b"CLOSE_WRITE /tmp/x".as_ptr(), 18), 1);
            assert_eq!(vetro_timeline_effect(vm, 99, b"x".as_ptr(), 1), 0);
            assert!(vetro_timeline_version(vm) > v0);
            let n = vetro_timeline_json(vm, 1_000);
            let t = json::parse(&result(vm, n)).unwrap();
            let Some(Value::Array(e)) = t.get("effects") else { panic!() };
            assert_eq!(e[0].get("kind").and_then(Value::as_str), Some("file"));
            assert_eq!(e[0].get("cause"), Some(&Value::Number("0".into())));
            vetro_timeline_clear(vm);
            vetro_capture_clear(vm);
            assert_eq!(vetro_capture_set(vm, 0), 1);
            vetro_result_clear(vm);
            assert!(vetro_result_ptr(vm).is_null());
            vetro_machine_free(vm);
            vetro_machine_free(none);
        }
    }
}
