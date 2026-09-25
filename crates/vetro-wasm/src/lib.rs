//! Binding verso il browser (M4): la macchina di `vetro-machine` dietro
//! un'API C, per il modulo WebAssembly caricato da JavaScript (browser e
//! Node). Nessuna dipendenza esterna, niente wasm-bindgen: i tipi che passano
//! il confine sono interi e puntatori nella memoria lineare del modulo.
//!
//! Il contratto è in `docs/specs/wasm.md`:
//! - memoria: [`vetro_alloc`] e [`vetro_free`] danno a JS i buffer in cui
//!   copiare kernel, initramfs e riga di comando, e in cui leggere la console;
//! - macchina: [`vetro_machine_new`], [`vetro_load_linux`], [`vetro_run`]
//!   (un quanto di istruzioni, con il motivo dell'arresto), console,
//!   contatore di istruzioni;
//! - import dal JS: `vetro_host.panic` (messaggio di un panic prima della
//!   trappola) e il motore JIT di [`jit`] (`vetro_jit.*`).
//!
//! Sul target nativo le stesse funzioni si provano come funzioni Rust (i test
//! di questo crate); gli import dal JS lì non esistono e hanno un sostituto.

#![allow(clippy::missing_safety_doc)]

pub mod jit;

use std::alloc::Layout;

use vetro_machine::{Machine, MachineConfig, Stop};

/// Versione dell'API C: cambia a ogni modifica incompatibile delle firme.
/// 2: JIT della modalità sistema (`vetro_machine_set_jit`, import
/// `vetro_jit.reset` e tabella `env.tbl` dei blocchi).
pub const ABI_VERSION: u32 = 2;

/// Allineamento dei buffer di [`vetro_alloc`] (basta per `JitState`).
const ALLOC_ALIGN: usize = 16;

/// Codici di [`vetro_run`].
pub mod stop {
    pub const BUDGET: u32 = 0;
    pub const POWER_OFF: u32 = 1;
    pub const RESET: u32 = 2;
    pub const IDLE: u32 = 3;
    pub const UNIMPLEMENTED: u32 = 4;
}

/// Codici di [`vetro_load_linux`].
pub mod load {
    pub const OK: u32 = 0;
    /// Il caricatore ha rifiutato i file: il motivo è in `vetro_message_*`.
    pub const BOOT_ERROR: u32 = 1;
    /// La riga di comando non è UTF-8.
    pub const BAD_CMDLINE: u32 = 2;
}

/// Una macchina con i buffer di contorno per JS.
pub struct Vm {
    m: Machine,
    /// Uscita della console già tolta alla UART e non ancora letta da JS.
    out: Vec<u8>,
    out_pos: usize,
    /// Ultimo messaggio (errore di caricamento, istruzione non implementata).
    message: String,
    unimpl: (u64, u32),
}

impl Vm {
    pub fn new(cfg: &MachineConfig) -> Self {
        Vm { m: Machine::new(cfg), out: Vec::new(), out_pos: 0, message: String::new(), unimpl: (0, 0) }
    }

    pub fn machine(&mut self) -> &mut Machine {
        &mut self.m
    }

    /// Attiva il JIT della modalità sistema sul motore JS, con soglia
    /// `hot_threshold` e `batch` blocchi per modulo.
    pub fn set_jit(&mut self, hot_threshold: u32, batch: u32) {
        let cfg = vetro_jit::SysJitConfig {
            hot_threshold,
            batch: batch.max(1) as usize,
            ..vetro_jit::SysJitConfig::default()
        };
        self.m.set_jit(Some(Box::new(vetro_jit::SysJit::new(jit::JsEngine::default(), cfg))));
    }

    pub fn load_linux(&mut self, image: &[u8], initrd: Option<&[u8]>, cmdline: &[u8]) -> u32 {
        let Ok(cmdline) = core::str::from_utf8(cmdline) else {
            self.message = "riga di comando non UTF-8".into();
            return load::BAD_CMDLINE;
        };
        match self.m.load_linux(image, initrd, cmdline) {
            Ok(_) => load::OK,
            Err(e) => {
                self.message = e.to_string();
                load::BOOT_ERROR
            }
        }
    }

    pub fn run(&mut self, budget: u64) -> u32 {
        match self.m.run(budget) {
            Stop::Budget => stop::BUDGET,
            Stop::PowerOff => stop::POWER_OFF,
            Stop::Reset => stop::RESET,
            Stop::Idle => stop::IDLE,
            Stop::Unimplemented { pc, raw, what } => {
                self.unimpl = (pc, raw);
                self.message = what.into();
                stop::UNIMPLEMENTED
            }
        }
    }

    /// Copia in `dst` al più `dst.len()` byte dell'uscita della console, che
    /// consuma; il resto aspetta la chiamata successiva.
    pub fn console_read(&mut self, dst: &mut [u8]) -> usize {
        if self.out_pos == self.out.len() {
            self.out.clear();
            self.out_pos = 0;
        }
        self.out.extend(self.m.console_output());
        let n = dst.len().min(self.out.len() - self.out_pos);
        dst[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
        self.out_pos += n;
        n
    }
}

/// `&[u8]` da puntatore e lunghezza passati da JS (nullo o vuoto = vuoto).
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 { &[] } else { unsafe { core::slice::from_raw_parts(ptr, len) } }
}

/// Installa (una volta) il gancio che manda a JS il messaggio di un panic:
/// su wasm32-unknown-unknown il panic è una trappola `unreachable` muta.
fn install_panic_hook() {
    #[cfg(target_arch = "wasm32")]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            std::panic::set_hook(Box::new(|info| {
                let msg = info.to_string();
                // SAFETY: import di `vetro_host`, legge `msg` durante la chiamata.
                unsafe { host::panic(msg.as_ptr(), msg.len()) };
            }))
        });
    }
}

#[cfg(target_arch = "wasm32")]
mod host {
    #[link(wasm_import_module = "vetro_host")]
    unsafe extern "C" {
        /// Messaggio UTF-8 di un panic, subito prima della trappola.
        pub fn panic(ptr: *const u8, len: usize);
    }
}

/// Versione dell'API ([`ABI_VERSION`]).
#[unsafe(no_mangle)]
pub extern "C" fn vetro_abi_version() -> u32 {
    ABI_VERSION
}

/// Alloca `len` byte allineati a 16 nella memoria del modulo; nullo se
/// `len == 0` o se la memoria non basta. Attenzione: l'allocazione può far
/// crescere la memoria, e le viste JS su `memory.buffer` vanno rifatte.
#[unsafe(no_mangle)]
pub extern "C" fn vetro_alloc(len: usize) -> *mut u8 {
    match Layout::from_size_align(len, ALLOC_ALIGN) {
        // SAFETY: dimensione non nulla.
        Ok(l) if len > 0 => unsafe { std::alloc::alloc(l) },
        _ => core::ptr::null_mut(),
    }
}

/// Libera un buffer di [`vetro_alloc`] con la stessa lunghezza.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() && len > 0 {
        // SAFETY: `ptr` viene da `vetro_alloc(len)` (contratto dell'API).
        unsafe { std::alloc::dealloc(ptr, Layout::from_size_align_unchecked(len, ALLOC_ALIGN)) }
    }
}

/// Crea una macchina: `ram_size` in byte (0 = 1 GiB), ora dell'RTC in
/// secondi dall'epoca e seme del device tree (entrambi 0 = i valori di
/// `MachineConfig::default`, quelli dei test nativi).
#[unsafe(no_mangle)]
pub extern "C" fn vetro_machine_new(ram_size: u64, now_secs: u64, seed: u64) -> *mut Vm {
    install_panic_hook();
    let d = MachineConfig::default();
    let cfg = MachineConfig {
        ram_size: if ram_size == 0 { d.ram_size } else { ram_size },
        now_secs: if now_secs == 0 { d.now_secs } else { now_secs },
        seed: if seed == 0 { d.seed } else { seed },
    };
    Box::into_raw(Box::new(Vm::new(&cfg)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_machine_free(vm: *mut Vm) {
    if !vm.is_null() {
        // SAFETY: `vm` viene da `vetro_machine_new` e non si usa più.
        drop(unsafe { Box::from_raw(vm) });
    }
}

/// Carica kernel (`Image`), initramfs (nullo o lunghezza 0 = nessuno) e riga
/// di comando (UTF-8). I buffer si possono liberare subito dopo. Restituisce
/// un codice di [`load`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_load_linux(
    vm: *mut Vm,
    image: *const u8,
    image_len: usize,
    initrd: *const u8,
    initrd_len: usize,
    cmdline: *const u8,
    cmdline_len: usize,
) -> u32 {
    // SAFETY: puntatori validi per le lunghezze date (contratto dell'API).
    let vm = unsafe { &mut *vm };
    let (image, initrd, cmdline) =
        unsafe { (bytes(image, image_len), bytes(initrd, initrd_len), bytes(cmdline, cmdline_len)) };
    vm.load_linux(image, (!initrd.is_empty()).then_some(initrd), cmdline)
}

/// Esegue al più `budget` istruzioni; restituisce un codice di [`stop`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_run(vm: *mut Vm, budget: u64) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &mut *vm }.run(budget)
}

/// Attiva il JIT (ADR 0013) con soglia `hot_threshold` (ingressi prima di
/// tradurre un blocco) e `batch` blocchi per modulo (0 = 1). Il risultato
/// dell'esecuzione non cambia; cambia solo la velocità.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_machine_set_jit(vm: *mut Vm, hot_threshold: u32, batch: u32) {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &mut *vm }.set_jit(hot_threshold, batch);
}

/// Contatori del JIT (`SysJitStats`, nell'ordine dei campi) in `out`, al
/// più `cap` valori; restituisce quanti ne ha scritti (0 senza JIT).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_jit_stats(vm: *const Vm, out: *mut u64, cap: usize) -> usize {
    // SAFETY: `vm` viene da `vetro_machine_new`, `out` vale per `cap` valori.
    let vm = unsafe { &*vm };
    let Some(s) = vm.m.jit_stats() else { return 0 };
    let v = [
        s.jit_steps,
        s.runs,
        s.resolves,
        s.calls,
        s.blocks,
        s.modules,
        s.reused,
        s.invalidated_pages,
        s.faults,
        s.svcs,
        s.stops,
        s.epochs,
        s.tlb_flushes,
        s.tlb_fills,
        s.resets,
    ];
    let n = v.len().min(cap);
    if n > 0 {
        unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&v[..n]);
    }
    n
}

/// Istruzioni eseguite (l'orologio del guest).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_steps(vm: *const Vm) -> u64 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &*vm }.m.steps
}

/// Tempo del guest in nanosecondi.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_guest_ns(vm: *const Vm) -> u64 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &*vm }.m.guest_ns()
}

/// Legge e consuma al più `cap` byte dell'uscita della console in `dst`;
/// restituisce quanti. 0 = niente di nuovo.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_console_read(vm: *mut Vm, dst: *mut u8, cap: usize) -> usize {
    // SAFETY: `vm` viene da `vetro_machine_new`, `dst` vale per `cap` byte.
    let vm = unsafe { &mut *vm };
    if dst.is_null() || cap == 0 {
        return 0;
    }
    vm.console_read(unsafe { core::slice::from_raw_parts_mut(dst, cap) })
}

/// Accoda `len` byte sulla console, come dalla tastiera.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_console_write(vm: *mut Vm, src: *const u8, len: usize) {
    // SAFETY: `vm` viene da `vetro_machine_new`, `src` vale per `len` byte.
    let vm = unsafe { &mut *vm };
    vm.m.console_input(unsafe { bytes(src, len) });
}

/// Ultimo messaggio (UTF-8): errore di caricamento o istruzione non
/// implementata. Valido fino alla chiamata successiva sulla macchina.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_message_ptr(vm: *const Vm) -> *const u8 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &*vm }.message.as_ptr()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_message_len(vm: *const Vm) -> usize {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &*vm }.message.len()
}

/// PC dell'ultima istruzione non implementata (`stop::UNIMPLEMENTED`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_unimplemented_pc(vm: *const Vm) -> u64 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &*vm }.unimpl.0
}

/// Codifica dell'ultima istruzione non implementata.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_unimplemented_raw(vm: *const Vm) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    unsafe { &*vm }.unimpl.1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> *mut Vm {
        vetro_machine_new(64 << 20, 0, 0)
    }

    #[test]
    fn alloc_e_free() {
        assert!(vetro_alloc(0).is_null());
        let p = vetro_alloc(100);
        assert!(!p.is_null());
        assert_eq!(p as usize % ALLOC_ALIGN, 0);
        unsafe { vetro_free(p, 100) };
    }

    #[test]
    fn caricamento_rifiutato_con_messaggio() {
        let vm = small();
        let junk = [0u8; 16];
        let code = unsafe {
            vetro_load_linux(vm, junk.as_ptr(), junk.len(), core::ptr::null(), 0, b"x".as_ptr(), 1)
        };
        assert_eq!(code, load::BOOT_ERROR);
        let msg = unsafe {
            core::str::from_utf8(bytes(vetro_message_ptr(vm), vetro_message_len(vm))).unwrap().to_string()
        };
        assert!(msg.contains("troncata"), "{msg}");
        let bad = [0xffu8];
        let code =
            unsafe { vetro_load_linux(vm, junk.as_ptr(), junk.len(), core::ptr::null(), 0, bad.as_ptr(), 1) };
        assert_eq!(code, load::BAD_CMDLINE);
        unsafe { vetro_machine_free(vm) };
    }

    #[test]
    fn lettura_della_console_a_pezzi() {
        let mut vm = Vm::new(&MachineConfig { ram_size: 64 << 20, ..MachineConfig::default() });
        vm.out = b"abcdef".to_vec();
        let mut buf = [0u8; 4];
        assert_eq!(vm.console_read(&mut buf), 4);
        assert_eq!(&buf, b"abcd");
        assert_eq!(vm.console_read(&mut buf), 2);
        assert_eq!(&buf[..2], b"ef");
        assert_eq!(vm.console_read(&mut buf), 0);
        assert_eq!(vm.out_pos, 0);
    }

    /// Una macchina senza kernel: il PC di reset non è in RAM, e ogni fetch
    /// è un'eccezione verso un vettore che non è in RAM. Il quanto si esaurisce
    /// e il contatore avanza esattamente del quanto (10 ns a istruzione).
    #[test]
    fn quanto_e_contatore() {
        let vm = small();
        assert_eq!(unsafe { vetro_run(vm, 1000) }, stop::BUDGET);
        assert_eq!(unsafe { vetro_steps(vm) }, 1000);
        assert_eq!(unsafe { vetro_guest_ns(vm) }, 10_000);
        unsafe { vetro_machine_free(vm) };
    }
}
