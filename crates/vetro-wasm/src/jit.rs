//! Il motore JIT fornito da JavaScript (ADR 0012 e 0013, `docs/specs/jit.md`,
//! `docs/specs/wasm.md`).
//!
//! Nel browser e in Node i moduli generati dal traduttore li compila ed
//! esegue l'API `WebAssembly` di JS (`web/node/jit-engine.mjs`). Questo
//! modulo è il lato Rust del ponte:
//! - [`JsEngine`] implementa `vetro_jit::Engine` chiamando gli import
//!   `vetro_jit.compile/run/drop/place/reset` del JS;
//! - il JS istanzia ogni modulo generato con `env.mem` = la memoria di
//!   questo modulo e `env.ld`/`env.st` = gli export [`vetro_jit_ld`] e
//!   [`vetro_jit_st`], che richiamano l'[`Host`] del blocco in corso; il
//!   dispatcher importa anche `env.tbl`, la tabella dei blocchi, in cui il
//!   JS mette le funzioni dei moduli (`vetro_jit.place`).
//!
//! Poiché `env.mem` è l'intera memoria di vetro-wasm, i blocchi raggiungono
//! direttamente anche la RAM del guest ([`Engine::host_address`]): la TLB
//! software della modalità sistema vi punta.
//!
//! [`vetro_jit_selftest`] prova il giro completo (Rust → JS → modulo
//! generato → `ld`/`st` in Rust) con un modulo scritto dal test JS.

use core::cell::Cell;

use vetro_jit::state::off;
use vetro_jit::{Engine, FAULT, Host, NEXT, STOP};

/// Dimensione della memoria condivisa di un [`JsEngine`]: `JitState` e
/// l'area della modalità sistema (`vetro_jit::state::area`).
pub const SHARED_LEN: usize = 256 * 1024;

#[repr(C, align(16))]
struct Shared([u8; SHARED_LEN]);

/// Un modulo compilato dal JS: un indice nella sua tabella.
#[derive(Debug)]
pub struct JsModule(i32);

/// Il motore che delega a `WebAssembly` di JS. La memoria condivisa è un
/// buffer di questo modulo: il suo indirizzo più `state` è ciò che riceve il
/// blocco, perché per il blocco `env.mem` è l'intera memoria lineare.
pub struct JsEngine {
    shared: Box<Shared>,
    /// Ultima funzione eseguita: (modulo, indice, voce nella tabella delle
    /// funzioni di vetro-wasm). Il dispatcher si esegue a ogni corsa.
    last: Option<(i32, u32, usize)>,
}

impl Default for JsEngine {
    fn default() -> Self {
        // SAFETY: tutti zeri è un valore valido per un array di byte.
        JsEngine { shared: unsafe { Box::<Shared>::new_zeroed().assume_init() }, last: None }
    }
}

impl JsEngine {
    /// Voce della tabella delle funzioni di vetro-wasm con `b<index>` del
    /// modulo: il JS ve la mette la prima volta (`vetro_jit.entry`), poi
    /// Rust la chiama come un puntatore a funzione, senza passare da JS.
    fn entry(&mut self, m: &JsModule, index: u32) -> usize {
        match self.last {
            Some((id, i, e)) if id == m.0 && i == index => e,
            _ => {
                // SAFETY: import del JS.
                let e = unsafe { js::entry(m.0, index) } as usize;
                self.last = Some((m.0, index, e));
                e
            }
        }
    }
}

impl Engine for JsEngine {
    type Module = JsModule;

    fn compile(&mut self, wasm: &[u8]) -> Result<JsModule, String> {
        // SAFETY: import del JS, legge `wasm` durante la chiamata.
        let id = unsafe { js::compile(wasm.as_ptr(), wasm.len()) };
        if id < 0 { Err(format!("il motore JS ha rifiutato il modulo ({id})")) } else { Ok(JsModule(id)) }
    }

    /// Esegue il blocco `b<index>` sullo stato all'offset `state` della
    /// memoria condivisa; durante l'esecuzione `ld`/`st` chiamano `host`.
    fn run(&mut self, m: &JsModule, index: u32, state: u32, host: &mut dyn Host) -> u32 {
        assert!(state as usize + off::SIZE <= SHARED_LEN && state.is_multiple_of(16), "JitState fuori posto");
        let base = self.shared.0.as_mut_ptr();
        let addr = base as usize + state as usize;
        let mut h: &mut dyn Host = host;
        let prev = HOST.replace((&mut h as *mut &mut dyn Host).cast());
        let prev_mem = MEM.replace(base);
        let entry = self.entry(m, index);
        // SAFETY: la voce `entry` della tabella è la funzione `b<index>`
        // del modulo, di tipo (i32) -> i32; il blocco scrive solo nella
        // memoria condivisa e nella RAM del guest (tramite la TLB software),
        // e chiama `ld`/`st`/`resolve`, che usano `h` finché è vivo.
        let r = unsafe { js::call(entry, addr) };
        HOST.set(prev);
        MEM.set(prev_mem);
        r
    }

    fn memory(&mut self) -> &mut [u8] {
        &mut self.shared.0
    }

    fn place(&mut self, m: &JsModule, count: u32, base: u32) {
        // SAFETY: import del JS: mette gli export nella tabella dei blocchi.
        unsafe { js::place(m.0, count, base) }
    }

    fn reset(&mut self) {
        // SAFETY: import del JS: scarta istanze e tabella.
        unsafe { js::reset() }
    }

    /// In wasm32 `env.mem` è la memoria di questo modulo: ogni suo byte ha
    /// per indirizzo il puntatore stesso.
    fn host_address(&mut self, p: *const u8, len: usize) -> Option<u32> {
        if cfg!(target_arch = "wasm32") {
            let a = p as usize as u64;
            (a + len as u64 <= 1 << 32).then_some(a as u32)
        } else {
            None
        }
    }
}

impl Drop for JsModule {
    fn drop(&mut self) {
        // SAFETY: import del JS; l'indice non si usa più.
        unsafe { js::drop_module(self.0) }
    }
}

thread_local! {
    /// L'`Host` del blocco in esecuzione: un `*mut &mut dyn Host` senza tipo.
    static HOST: Cell<*mut ()> = const { Cell::new(core::ptr::null_mut()) };
    /// La memoria condivisa del motore in esecuzione.
    static MEM: Cell<*mut u8> = const { Cell::new(core::ptr::null_mut()) };
}

fn with_host<R>(f: impl FnOnce(&mut dyn Host, &mut [u8]) -> R) -> R {
    let p = HOST.get().cast::<&mut dyn Host>();
    let m = MEM.get();
    assert!(!p.is_null() && !m.is_null(), "ld/st fuori da un blocco");
    // SAFETY: impostati da `JsEngine::run` per la durata della chiamata al
    // JS; mentre gira `ld`/`st` nessun altro usa la memoria condivisa.
    let mem = unsafe { core::slice::from_raw_parts_mut(m, SHARED_LEN) };
    f(unsafe { &mut **p }, mem)
}

/// `env.ld` dei moduli generati: lettura di `size` byte, estesa a zero. In
/// caso di fault scrive `FAULT` in `exit_detail` e restituisce 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_jit_ld(state: usize, va: u64, size: u32) -> u64 {
    match with_host(|h, mem| h.ld(mem, va, size)) {
        Ok(v) => v,
        Err(()) => {
            // SAFETY: `state` è l'indirizzo di un `JitState` (contratto).
            unsafe { set_exit_detail(state, FAULT) };
            0
        }
    }
}

/// `env.st` dei moduli generati: 0 = continua, 1 = il blocco si ferma
/// (fault, con `FAULT` in `exit_detail`, o scrittura su codice sorvegliato,
/// con `STOP`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_jit_st(state: usize, va: u64, size: u32, value: u64) -> u32 {
    match with_host(|h, mem| h.st(mem, va, size, value)) {
        Ok(false) => 0,
        Ok(true) => {
            // SAFETY: come sotto.
            unsafe { set_exit_detail(state, STOP) };
            1
        }
        Err(()) => {
            // SAFETY: `state` è l'indirizzo di un `JitState` (contratto).
            unsafe { set_exit_detail(state, FAULT) };
            1
        }
    }
}

/// `env.resolve` del dispatcher: 1 se l'host ha scritto la voce della
/// cache dei salti per il `pc` di `JitState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_jit_resolve(_state: usize) -> u32 {
    with_host(|h, mem| h.resolve(mem)) as u32
}

unsafe fn set_exit_detail(state: usize, v: u32) {
    let p = (state + off::EXIT_DETAIL as usize) as *mut u32;
    // SAFETY: `state` punta a un `JitState` allineato a 16 (contratto).
    unsafe { p.write(v) }
}

#[cfg(target_arch = "wasm32")]
mod js {
    #[link(wasm_import_module = "vetro_jit")]
    unsafe extern "C" {
        /// Compila un modulo generato; indice >= 0, o < 0 se rifiutato.
        pub fn compile(ptr: *const u8, len: usize) -> i32;
        /// Mette l'export `b<index>` del modulo in una voce nuova della
        /// tabella delle funzioni di vetro-wasm; restituisce la voce.
        pub fn entry(module: i32, index: u32) -> u32;
        /// Libera il modulo.
        #[link_name = "drop"]
        pub fn drop_module(module: i32);
        /// Mette gli export `b0..b<count-1>` del modulo nelle voci
        /// `base..` della tabella dei blocchi.
        pub fn place(module: i32, count: u32, base: u32);
        /// Scarta tutte le istanze e ricrea la tabella dei blocchi.
        pub fn reset();
    }

    /// Chiama la funzione alla voce `entry` della tabella delle funzioni
    /// (un puntatore a funzione in wasm32) con `state`.
    ///
    /// # Safety
    /// `entry` deve essere una funzione di tipo (i32) -> i32.
    pub unsafe fn call(entry: usize, state: usize) -> u32 {
        // SAFETY: vedi sopra; in wasm32 un puntatore a funzione è la sua
        // voce nella tabella.
        let f = unsafe { core::mem::transmute::<usize, extern "C" fn(usize) -> u32>(entry) };
        f(state)
    }
}

/// Fuori da wasm32 non c'è un motore JS: il motore nativo è wasmtime
/// (`vetro-jit-native`).
#[cfg(not(target_arch = "wasm32"))]
mod js {
    pub unsafe fn compile(_ptr: *const u8, _len: usize) -> i32 {
        -1
    }
    pub unsafe fn entry(_module: i32, _index: u32) -> u32 {
        unreachable!("nessun modulo compilato fuori da wasm32")
    }
    pub unsafe fn call(_entry: usize, _state: usize) -> u32 {
        unreachable!("nessun modulo compilato fuori da wasm32")
    }
    pub unsafe fn drop_module(_module: i32) {}
    pub unsafe fn place(_module: i32, _count: u32, _base: u32) {}
    pub unsafe fn reset() {}
}

/// RAM piatta di prova per [`vetro_jit_selftest`]: 4 KiB da `SELFTEST_BASE`.
pub const SELFTEST_BASE: u64 = 0x1000;

struct TestRam([u8; 4096]);

impl Host for TestRam {
    fn ld(&mut self, _mem: &mut [u8], va: u64, size: u32) -> Result<u64, ()> {
        let o = va.checked_sub(SELFTEST_BASE).ok_or(())? as usize;
        let b = self.0.get(o..o + size as usize).ok_or(())?;
        let mut v = [0u8; 8];
        v[..b.len()].copy_from_slice(b);
        Ok(u64::from_le_bytes(v))
    }
    fn st(&mut self, _mem: &mut [u8], va: u64, size: u32, value: u64) -> Result<bool, ()> {
        let o = va.checked_sub(SELFTEST_BASE).ok_or(())? as usize;
        let b = self.0.get_mut(o..o + size as usize).ok_or(())?;
        b.copy_from_slice(&value.to_le_bytes()[..size as usize]);
        Ok(false)
    }
}

/// Prova del ponte JIT: compila `wasm` col motore JS ed esegue `b0` su un
/// `JitState` con `x0 = SELFTEST_BASE`, `x1 = 5`, `pc = 0x40_0000`, sopra una
/// RAM che contiene 37 in `SELFTEST_BASE`. Il modulo di prova
/// (`web/node/jit-selftest.mjs`) fa `x2 = [x0] + x1; [x0 + 8] = x2`,
/// `pc += 12`, `steps += 3`. Restituisce il valore in `SELFTEST_BASE + 8`
/// (42), oppure `u64::MAX - codice` se qualcosa non torna.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_jit_selftest(wasm: *const u8, len: usize) -> u64 {
    // SAFETY: `wasm` vale per `len` byte (contratto).
    let wasm = unsafe { crate::bytes(wasm, len) };
    let mut e = JsEngine::default();
    let Ok(m) = e.compile(wasm) else { return u64::MAX - 1 };
    let mem = e.memory();
    mem[..off::SIZE].fill(0);
    mem[0..8].copy_from_slice(&SELFTEST_BASE.to_le_bytes());
    mem[8..16].copy_from_slice(&5u64.to_le_bytes());
    let pc = off::PC as usize;
    mem[pc..pc + 8].copy_from_slice(&0x40_0000u64.to_le_bytes());
    let mut ram = TestRam([0; 4096]);
    ram.0[..8].copy_from_slice(&37u64.to_le_bytes());
    if e.run(&m, 0, 0, &mut ram) != NEXT {
        return u64::MAX - 2;
    }
    let mem = e.memory();
    let rd = |o: u32| u64::from_le_bytes(mem[o as usize..o as usize + 8].try_into().unwrap());
    if rd(off::PC) != 0x40_000c || rd(off::STEPS) != 3 {
        return u64::MAX - 3;
    }
    u64::from_le_bytes(ram.0[8..16].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_di_prova() {
        let mut r = TestRam([0; 4096]);
        let m = &mut [][..];
        assert_eq!(r.st(m, SELFTEST_BASE + 2, 2, 0xabcd), Ok(false));
        assert_eq!(r.ld(m, SELFTEST_BASE, 4), Ok(0xabcd_0000));
        assert_eq!(r.ld(m, SELFTEST_BASE + 4095, 2), Err(()));
        assert_eq!(r.ld(m, 0, 1), Err(()));
    }

    /// `ld`/`st` passano dall'host impostato da `run` e segnalano i fault in
    /// `exit_detail`: qui host e memoria li imposta il test, come farebbe
    /// `run`.
    #[test]
    fn ld_st_verso_host() {
        let mut e = JsEngine::default();
        let base = e.memory().as_mut_ptr();
        let state = base as usize;
        let mut ram = TestRam([0; 4096]);
        let mut h: &mut dyn Host = &mut ram;
        HOST.set((&mut h as *mut &mut dyn Host).cast());
        MEM.set(base);
        unsafe {
            assert_eq!(vetro_jit_st(state, SELFTEST_BASE, 8, 7), 0);
            assert_eq!(vetro_jit_ld(state, SELFTEST_BASE, 8), 7);
            assert_eq!(vetro_jit_ld(state, 0, 8), 0);
        }
        HOST.set(core::ptr::null_mut());
        MEM.set(core::ptr::null_mut());
        let m = e.memory();
        let d = off::EXIT_DETAIL as usize;
        assert_eq!(u32::from_le_bytes(m[d..d + 4].try_into().unwrap()), FAULT);
        assert!(e.compile(&[0]).is_err(), "nessun motore JS fuori da wasm32");
        assert_eq!(e.host_address(base, 16), None, "fuori da wasm32 la RAM non è raggiungibile");
    }
}
