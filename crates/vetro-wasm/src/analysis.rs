//! Ispettore di rete e timeline input→effetti (ABI 8, M7, ADR 0023).
//!
//! - **Cattura**: i frame di virtio-net (`Machine::net_tap`, ADR 0016) si
//!   raccolgono nella [`Vm`] a ogni `vetro_run` (non cambia l'esecuzione),
//!   al più [`MAX_CAPTURE_BYTES`]; l'analisi di `vetro_analysis::net` dà la
//!   lista delle richieste e il dettaglio in JSON
//!   (`vetro_analysis::net::view`), l'HAR e il pcapng.
//! - **Timeline**: gli ingressi dell'utente che passano dalle funzioni della
//!   macchina (tasti, pulsanti, tocchi, console, tasto di accensione,
//!   risoluzione) si annotano da soli con il numero d'istruzione
//!   ([`Describer::describe`]); i comandi del gestore dei file e gli effetti sui file
//!   li annota il JS (`vetro_timeline_input`, `vetro_timeline_effect`);
//!   l'uscita della console si annota quando il JS la legge; gli effetti di
//!   rete vengono dalla cattura. L'attribuzione è quella di
//!   `vetro_analysis::timeline`.
//!
//! I risultati (JSON, HAR, pcapng) vanno nel buffer dei risultati della
//! macchina: `vetro_result_ptr`, validi fino al prossimo risultato.

use vetro_analysis::net::har::HarOptions;
use vetro_analysis::net::pcapng::{self, PcapngOptions};
use vetro_analysis::net::{Direction, NetworkAnalysis, view};
use vetro_analysis::timeline::{
    self, DEFAULT_WINDOW_US, Effect, EffectKind, InputKind, LineEditor, UserInput, key_name, printable,
};
use vetro_machine::{FrameDir, Input, Reply};
use vetro_platform::virtio::input::{
    ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID, ABS_X, ABS_Y, BTN_TOUCH, EV_ABS,
    EV_KEY, InputEvent,
};

use crate::Vm;

/// Byte di frame tenuti al più nella cattura (oltre, i frame si contano e
/// si scartano).
pub const MAX_CAPTURE_BYTES: usize = 64 << 20;

/// Stato del descrittore degli ingressi: la riga della console in corso, la
/// posizione del puntatore, i contatti attivi del touchscreen.
#[derive(Clone, Debug, Default)]
pub struct Describer {
    line: LineEditor,
    pos: (u32, u32),
    slot: u32,
    touching: u32,
}

/// Percentuale di un asse assoluto (0..=32767).
fn pct(v: u32) -> u32 {
    (u64::from(v.min(32767)) * 100 / 32767) as u32
}

impl Describer {
    /// Che cosa vede l'utente di un ingresso: tipo, testo, debole. `None`
    /// per ciò che non è un'azione dell'utente (movimenti del puntatore,
    /// rilasci, risposte automatiche del terminale, vsock, rete).
    pub fn describe(&mut self, input: &Input) -> Option<(InputKind, String, bool)> {
        match input {
            Input::Console(bytes) => {
                let lines = self.line.feed(bytes);
                if let Some(last) = lines.last() {
                    return Some((InputKind::Console, format!("Invio: {last}"), false));
                }
                let t = printable(bytes);
                // Le risposte del terminale (ESC[r;cR) non sono dell'utente.
                (!t.is_empty()).then_some((InputKind::Console, t, true))
            }
            Input::Keyboard(ev) => {
                let pressed: Vec<u16> =
                    ev.iter().filter(|e| e.ty == EV_KEY && e.value == 1).map(|e| e.code).collect();
                if pressed.is_empty() {
                    return None;
                }
                let names: Vec<String> = pressed.iter().map(|&c| key_name(c)).collect();
                let strong = pressed.iter().any(|&c| timeline::is_enter(c));
                Some((InputKind::Key, names.join(" + "), !strong))
            }
            Input::Pointer(ev) => self.pointer(ev),
            Input::Gpio { line, level: true } if *line == vetro_platform::pl061::POWER_KEY_LINE => {
                Some((InputKind::Power, "tasto di accensione".into(), false))
            }
            Input::Display { width, height, .. } => {
                Some((InputKind::Display, format!("schermo {width}x{height}"), true))
            }
            _ => None,
        }
    }

    fn pointer(&mut self, ev: &[InputEvent]) -> Option<(InputKind, String, bool)> {
        let mut out = None;
        let mut touch_pos = (None, None);
        let mut new_touch = false;
        for e in ev {
            match (e.ty, e.code) {
                (EV_ABS, ABS_X) => self.pos.0 = e.value,
                (EV_ABS, ABS_Y) => self.pos.1 = e.value,
                (EV_ABS, ABS_MT_SLOT) => self.slot = e.value.min(31),
                (EV_ABS, ABS_MT_TRACKING_ID) => {
                    let bit = 1u32 << self.slot;
                    if e.value == u32::MAX {
                        self.touching &= !bit;
                    } else if self.touching & bit == 0 {
                        self.touching |= bit;
                        new_touch = true;
                    }
                }
                (EV_ABS, ABS_MT_POSITION_X) => touch_pos.0 = Some(e.value),
                (EV_ABS, ABS_MT_POSITION_Y) => touch_pos.1 = Some(e.value),
                (EV_KEY, BTN_TOUCH) => {}
                (EV_KEY, code) if e.value == 1 => {
                    out = Some((
                        InputKind::Pointer,
                        format!("{} ({}%, {}%)", key_name(code), pct(self.pos.0), pct(self.pos.1)),
                        false,
                    ));
                }
                _ => {}
            }
        }
        if new_touch {
            let (x, y) = (touch_pos.0.unwrap_or(0), touch_pos.1.unwrap_or(0));
            return Some((
                InputKind::Touch,
                format!("tocco {} ({}%, {}%)", self.slot, pct(x), pct(y)),
                false,
            ));
        }
        out
    }
}

impl Vm {
    /// Un ingresso dell'utente: lo annota nella timeline (se è un'azione e
    /// la macchina lo accetta) e lo passa a `Machine::input`.
    pub fn user_input(&mut self, input: Input) -> Reply {
        let step = self.m.steps;
        let desc = self.describer.describe(&input);
        let r = self.m.input(input);
        if let Some((kind, label, weak)) = desc
            && !matches!(r, Reply::Ignored | Reply::NoDevice)
        {
            self.timeline.push_input(UserInput::new(step, kind, label, weak));
        }
        r
    }

    /// Accende o spegne la cattura; falso senza rete.
    pub fn capture_set(&mut self, on: bool) -> bool {
        if !on {
            self.collect_capture();
        }
        let ok = self.m.net_tap(on);
        self.capture_on = ok && on;
        ok
    }

    /// Porta i frame catturati dalla macchina nella cattura della `Vm`.
    pub fn collect_capture(&mut self) {
        if !self.capture_on {
            return;
        }
        for f in self.m.net_tap_take() {
            if self.capture_bytes + f.data.len() > MAX_CAPTURE_BYTES {
                self.capture_dropped += 1;
                continue;
            }
            self.capture_bytes += f.data.len();
            let dir = match f.dir {
                FrameDir::FromGuest => Direction::FromGuest,
                FrameDir::ToGuest => Direction::ToGuest,
            };
            self.capture.push(f.at.0, dir, f.data);
        }
    }

    pub fn capture_clear(&mut self) {
        self.collect_capture();
        self.capture = Default::default();
        self.capture_bytes = 0;
        self.capture_dropped = 0;
        self.capture_gen += 1;
        self.analysis = None;
    }

    /// L'analisi della cattura (rifatta solo se sono arrivati frame).
    pub fn analysis(&mut self) -> &NetworkAnalysis {
        self.collect_capture();
        let n = self.capture.len();
        if self.analysis.as_ref().is_none_or(|(k, _)| *k != n) {
            self.analysis = Some((n, NetworkAnalysis::from_frames(self.capture.frames())));
        }
        &self.analysis.as_ref().expect("appena calcolata").1
    }

    /// La timeline in JSON con gli effetti di rete della cattura.
    pub fn timeline_json(&mut self, window_us: u64) -> String {
        let net = timeline::network_effects(self.analysis());
        self.timeline.to_json(&net, if window_us == 0 { DEFAULT_WINDOW_US } else { window_us })
    }

    pub(crate) fn set_result(&mut self, bytes: Vec<u8>) -> usize {
        self.result = bytes;
        self.result.len()
    }
}

// ---- Buffer dei risultati ----------------------------------------------------

/// I byte dell'ultimo risultato (JSON, HAR, pcapng, log, keyframe, registri),
/// validi fino al prossimo risultato o a [`vetro_result_clear`]; nullo se
/// vuoto.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_result_ptr(vm: *const Vm) -> *const u8 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &*vm };
    if vm.result.is_empty() { core::ptr::null() } else { vm.result.as_ptr() }
}

/// Libera il buffer dei risultati.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_result_clear(vm: *mut Vm) {
    unsafe { vm_ref(vm) }.result = Vec::new();
}

/// La macchina da un puntatore del JS.
pub(crate) unsafe fn vm_ref<'a>(p: *mut Vm) -> &'a mut Vm {
    // SAFETY: `p` viene da `vetro_machine_new` (contratto dell'API).
    unsafe { &mut *p }
}

// ---- Cattura e ispettore -----------------------------------------------------

/// Accende (`on` = 1) o spegne la cattura dei frame di virtio-net. 1 fatto,
/// 0 la macchina non ha la rete. Non cambia l'esecuzione.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_capture_set(vm: *mut Vm, on: u32) -> u32 {
    unsafe { vm_ref(vm) }.capture_set(on != 0) as u32
}

/// Svuota la cattura (frame e analisi).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_capture_clear(vm: *mut Vm) {
    unsafe { vm_ref(vm) }.capture_clear();
}

/// Contatori della cattura in `out` (al più `cap`): accesa, frame, byte,
/// frame scartati oltre il limite. Restituisce quanti valori.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_capture_stats(vm: *mut Vm, out: *mut u64, cap: usize) -> usize {
    let vm = unsafe { vm_ref(vm) };
    vm.collect_capture();
    let v = [u64::from(vm.capture_on), vm.capture.len() as u64, vm.capture_bytes as u64, vm.capture_dropped];
    unsafe { crate::write_u64s(out, cap, &v) }
}

/// La lista dell'ispettore in JSON (`view::requests_json`) nel buffer dei
/// risultati; restituisce la lunghezza.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_inspect_requests(vm: *mut Vm) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let j = view::requests_json(vm.analysis());
    vm.set_result(j.into_bytes())
}

/// Il dettaglio della richiesta `index` in JSON (`view::exchange_json`);
/// 0 se non c'è.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_inspect_request(vm: *mut Vm, index: u32) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let j = vm.analysis().http.get(index as usize).map(view::exchange_json);
    vm.set_result(j.map(String::into_bytes).unwrap_or_default())
}

/// L'HAR 1.2 della cattura (`epoch_us`: microsecondi Unix del tempo 0 del
/// guest).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_inspect_har(vm: *mut Vm, epoch_us: u64) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let h = vm.analysis().to_har(&HarOptions { epoch_us });
    vm.set_result(h.into_bytes())
}

/// Il pcapng della cattura (`epoch_us` come per l'HAR).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_inspect_pcapng(vm: *mut Vm, epoch_us: u64) -> usize {
    let vm = unsafe { vm_ref(vm) };
    vm.collect_capture();
    let p = pcapng::write(vm.capture.frames(), &PcapngOptions { epoch_us, ..PcapngOptions::default() });
    vm.set_result(p)
}

// ---- Timeline ------------------------------------------------------------------

/// Annota un ingresso dell'utente che la macchina non vede come tale (un
/// comando del gestore dei file): tipo (`InputKind::code`), debole (1) o
/// di comando (0), testo UTF-8. All'istruzione corrente.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_timeline_input(
    vm: *mut Vm,
    kind: u32,
    weak: u32,
    text: *const u8,
    len: usize,
) {
    let vm = unsafe { vm_ref(vm) };
    let label = String::from_utf8_lossy(unsafe { crate::bytes(text, len) }).into_owned();
    let step = vm.m.steps;
    vm.timeline.push_input(UserInput::new(step, InputKind::from_code(kind), label, weak != 0));
}

/// Annota un effetto osservato dal JS (un file cambiato: `EffectKind::code`)
/// all'istruzione corrente. 0 se il tipo non esiste.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_timeline_effect(vm: *mut Vm, kind: u32, text: *const u8, len: usize) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    let Some(kind) = EffectKind::from_code(kind) else { return 0 };
    let label = String::from_utf8_lossy(unsafe { crate::bytes(text, len) }).into_owned();
    let at_us = timeline::step_us(vm.m.steps);
    vm.timeline.push_effect(Effect::new(at_us, kind, label));
    1
}

/// La timeline in JSON (`Timeline::to_json`, con gli effetti di rete della
/// cattura) con finestra di attribuzione `window_us` (0 = 3 s).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_timeline_json(vm: *mut Vm, window_us: u64) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let j = vm.timeline_json(window_us);
    vm.set_result(j.into_bytes())
}

/// Cresce quando la timeline cambia (ingressi, effetti, frame catturati):
/// se è uguale all'ultima volta, niente da ridisegnare.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_timeline_version(vm: *mut Vm) -> u64 {
    let vm = unsafe { vm_ref(vm) };
    vm.collect_capture();
    vm.timeline.version + vm.capture.len() as u64 + vm.capture_gen
}

/// Svuota la timeline.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_timeline_clear(vm: *mut Vm) {
    let vm = unsafe { vm_ref(vm) };
    vm.timeline.clear();
    vm.describer = Describer::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descrizione_degli_ingressi() {
        let mut d = Describer::default();
        let c = |b: &[u8]| Input::Console(b.to_vec());
        assert_eq!(d.describe(&c(b"w")), Some((InputKind::Console, "w".into(), true)));
        assert_eq!(d.describe(&c(b"get x")), Some((InputKind::Console, "get x".into(), true)));
        assert_eq!(d.describe(&c(b"\r")), Some((InputKind::Console, "Invio: wget x".into(), false)));
        assert_eq!(d.describe(&c(b"\x1b[24;80R")), None, "risposta del terminale");
        let k = |code, down| Input::Keyboard(Input::key_events(code, down));
        assert_eq!(d.describe(&k(30, true)), Some((InputKind::Key, "A".into(), true)));
        assert_eq!(d.describe(&k(30, false)), None);
        assert_eq!(d.describe(&k(28, true)), Some((InputKind::Key, "Invio".into(), false)));
        let p = |ev: Vec<InputEvent>| Input::Pointer(ev);
        assert_eq!(d.describe(&p(Input::move_abs_events(16384, 32767))), None, "movimento");
        assert_eq!(
            d.describe(&p(Input::key_events(0x110, true))),
            Some((InputKind::Pointer, "clic sinistro (50%, 100%)".into(), false))
        );
        assert_eq!(d.describe(&p(Input::key_events(0x110, false))), None);
        assert_eq!(
            d.describe(&p(Input::touch_events(1, Some((0, 32767))))),
            Some((InputKind::Touch, "tocco 1 (0%, 100%)".into(), false))
        );
        assert_eq!(d.describe(&p(Input::touch_events(1, Some((10, 10))))), None, "trascinamento");
        assert_eq!(d.describe(&p(Input::touch_events(1, None))), None);
        assert!(d.describe(&p(Input::touch_events(1, Some((10, 10))))).is_some(), "nuovo tocco");
        let power = vetro_platform::pl061::POWER_KEY_LINE;
        assert_eq!(
            d.describe(&Input::Gpio { line: power, level: true }),
            Some((InputKind::Power, "tasto di accensione".into(), false))
        );
        assert_eq!(d.describe(&Input::Gpio { line: power, level: false }), None);
        assert_eq!(
            d.describe(&Input::Display { scanout: 0, width: 800, height: 600 }),
            Some((InputKind::Display, "schermo 800x600".into(), true))
        );
        assert_eq!(d.describe(&Input::NetLink(true)), None);
    }
}
