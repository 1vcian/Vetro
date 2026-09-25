//! Il motore JIT fornito da JavaScript (ADR 0012, `docs/specs/jit.md`,
//! `docs/specs/wasm.md`).
//!
//! Nel browser e in Node i moduli generati dal traduttore li compila ed
//! esegue l'API `WebAssembly` di JS (`web/node/jit-engine.mjs`). Questo
//! modulo è il lato Rust del ponte:
//! - [`JsEngine`] chiama gli import `vetro_jit.compile/run/drop` del JS;
//! - il JS istanzia ogni modulo generato con `env.mem` = la memoria di
//!   questo modulo e `env.ld`/`env.st` = gli export [`vetro_jit_ld`] e
//!   [`vetro_jit_st`], che richiamano l'[`Host`] del blocco in corso.
//!
//! [`JsEngine`] ha già la forma del trait `vetro_jit::Engine` della spec;
//! quando il crate `vetro-jit` lo esporrà, qui resterà solo l'`impl` e
//! [`Host`] diventerà un alias di quello di `vetro-jit`.
//!
//! [`vetro_jit_selftest`] prova il giro completo (Rust → JS → modulo
//! generato → `ld`/`st` in Rust) con un modulo scritto dal test JS.

use core::cell::Cell;

/// Accesso alla memoria del guest per i blocchi (come `vetro_jit::Host`).
/// Le firme sono quelle di `docs/specs/jit.md`: il dettaglio del fault lo
/// tiene l'host, quindi l'errore è `()`.
#[allow(clippy::result_unit_err)]
pub trait Host {
    fn ld(&mut self, va: u64, size: u32) -> Result<u64, ()>;
    /// `Ok(true)` = il blocco si ferma dopo questa istruzione.
    fn st(&mut self, va: u64, size: u32, value: u64) -> Result<bool, ()>;
}

/// Codici d'uscita dei blocchi (`docs/specs/jit.md`).
pub mod exit {
    pub const NEXT: u32 = 0;
    pub const FAULT: u32 = 1;
    pub const STOP: u32 = 2;
    pub const SVC: u32 = 3;
}

/// Offset dei campi di `JitState` (`docs/specs/jit.md`).
pub mod state {
    pub const X: usize = 0;
    pub const SP: usize = 248;
    pub const PC: usize = 256;
    pub const STEPS: usize = 264;
    pub const NZCV: usize = 272;
    pub const EXIT_DETAIL: usize = 276;
    pub const EL: usize = 280;
    pub const SIZE: usize = 288;
}

/// Dimensione della memoria condivisa di un [`JsEngine`] (dove l'host mette
/// uno o più `JitState`).
pub const SHARED_LEN: usize = 4096;

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
}

impl Default for JsEngine {
    fn default() -> Self {
        JsEngine { shared: Box::new(Shared([0; SHARED_LEN])) }
    }
}

impl JsEngine {
    pub fn compile(&mut self, wasm: &[u8]) -> Result<JsModule, String> {
        // SAFETY: import del JS, legge `wasm` durante la chiamata.
        let id = unsafe { js::compile(wasm.as_ptr(), wasm.len()) };
        if id < 0 { Err(format!("il motore JS ha rifiutato il modulo ({id})")) } else { Ok(JsModule(id)) }
    }

    /// Esegue il blocco `b<index>` sullo stato all'offset `state` della
    /// memoria condivisa; durante l'esecuzione `ld`/`st` chiamano `host`.
    pub fn run(&mut self, m: &JsModule, index: u32, state: u32, host: &mut dyn Host) -> u32 {
        assert!(
            state as usize + state::SIZE <= SHARED_LEN && state.is_multiple_of(16),
            "JitState fuori posto"
        );
        let addr = self.shared.0.as_ptr() as usize + state as usize;
        let mut h: &mut dyn Host = host;
        let prev = HOST.replace((&mut h as *mut &mut dyn Host).cast());
        // SAFETY: import del JS; il blocco scrive solo in `JitState` (dentro
        // `shared`) e chiama `ld`/`st`, che usano `h` finché è vivo.
        let r = unsafe { js::run(m.0, index, addr) };
        HOST.set(prev);
        r
    }

    pub fn memory(&mut self) -> &mut [u8] {
        &mut self.shared.0
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
}

fn with_host<R>(f: impl FnOnce(&mut dyn Host) -> R) -> R {
    let p = HOST.get().cast::<&mut dyn Host>();
    assert!(!p.is_null(), "ld/st fuori da un blocco");
    // SAFETY: impostato da `JsEngine::run` per la durata della chiamata al JS.
    f(unsafe { &mut **p })
}

/// `env.ld` dei moduli generati: lettura di `size` byte, estesa a zero. In
/// caso di fault scrive `FAULT` in `exit_detail` e restituisce 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_jit_ld(state: usize, va: u64, size: u32) -> u64 {
    match with_host(|h| h.ld(va, size)) {
        Ok(v) => v,
        Err(()) => {
            // SAFETY: `state` è l'indirizzo di un `JitState` (contratto).
            unsafe { set_exit_detail(state, exit::FAULT) };
            0
        }
    }
}

/// `env.st` dei moduli generati: 0 = continua, 1 = il blocco si ferma
/// (fault, con `FAULT` in `exit_detail`, o scrittura su codice sorvegliato).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_jit_st(state: usize, va: u64, size: u32, value: u64) -> u32 {
    match with_host(|h| h.st(va, size, value)) {
        Ok(stop) => stop as u32,
        Err(()) => {
            // SAFETY: come sopra.
            unsafe { set_exit_detail(state, exit::FAULT) };
            1
        }
    }
}

unsafe fn set_exit_detail(state: usize, v: u32) {
    let p = (state + state::EXIT_DETAIL) as *mut u32;
    // SAFETY: `state` punta a un `JitState` allineato a 16 (contratto).
    unsafe { p.write(v) }
}

#[cfg(target_arch = "wasm32")]
mod js {
    #[link(wasm_import_module = "vetro_jit")]
    unsafe extern "C" {
        /// Compila un modulo generato; indice >= 0, o < 0 se rifiutato.
        pub fn compile(ptr: *const u8, len: usize) -> i32;
        /// Esegue l'export `b<index>` del modulo con `state` (indirizzo
        /// assoluto nella memoria di vetro-wasm); restituisce il codice.
        pub fn run(module: i32, index: u32, state: usize) -> u32;
        /// Libera il modulo.
        #[link_name = "drop"]
        pub fn drop_module(module: i32);
    }
}

/// Fuori da wasm32 non c'è un motore JS: il motore nativo è wasmtime
/// (`vetro-jit-native`).
#[cfg(not(target_arch = "wasm32"))]
mod js {
    pub unsafe fn compile(_ptr: *const u8, _len: usize) -> i32 {
        -1
    }
    pub unsafe fn run(_module: i32, _index: u32, _state: usize) -> u32 {
        unreachable!("nessun modulo compilato fuori da wasm32")
    }
    pub unsafe fn drop_module(_module: i32) {}
}

/// RAM piatta di prova per [`vetro_jit_selftest`]: 4 KiB da `SELFTEST_BASE`.
pub const SELFTEST_BASE: u64 = 0x1000;

struct TestRam([u8; 4096]);

impl Host for TestRam {
    fn ld(&mut self, va: u64, size: u32) -> Result<u64, ()> {
        let o = va.checked_sub(SELFTEST_BASE).ok_or(())? as usize;
        let b = self.0.get(o..o + size as usize).ok_or(())?;
        let mut v = [0u8; 8];
        v[..b.len()].copy_from_slice(b);
        Ok(u64::from_le_bytes(v))
    }
    fn st(&mut self, va: u64, size: u32, value: u64) -> Result<bool, ()> {
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
    mem[..state::SIZE].fill(0);
    mem[state::X..state::X + 8].copy_from_slice(&SELFTEST_BASE.to_le_bytes());
    mem[state::X + 8..state::X + 16].copy_from_slice(&5u64.to_le_bytes());
    mem[state::PC..state::PC + 8].copy_from_slice(&0x40_0000u64.to_le_bytes());
    let mut ram = TestRam([0; 4096]);
    ram.0[..8].copy_from_slice(&37u64.to_le_bytes());
    if e.run(&m, 0, 0, &mut ram) != exit::NEXT {
        return u64::MAX - 2;
    }
    let mem = e.memory();
    let rd = |o: usize| u64::from_le_bytes(mem[o..o + 8].try_into().unwrap());
    if rd(state::PC) != 0x40_000c || rd(state::STEPS) != 3 {
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
        assert_eq!(r.st(SELFTEST_BASE + 2, 2, 0xabcd), Ok(false));
        assert_eq!(r.ld(SELFTEST_BASE, 4), Ok(0xabcd_0000));
        assert_eq!(r.ld(SELFTEST_BASE + 4095, 2), Err(()));
        assert_eq!(r.ld(0, 1), Err(()));
    }

    /// `ld`/`st` passano dall'host impostato da `run` e segnalano i fault in
    /// `exit_detail`: qui l'host lo imposta il test, come farebbe `run`.
    #[test]
    fn ld_st_verso_host() {
        let mut e = JsEngine::default();
        let state = e.memory().as_mut_ptr() as usize;
        let mut ram = TestRam([0; 4096]);
        let mut h: &mut dyn Host = &mut ram;
        HOST.set((&mut h as *mut &mut dyn Host).cast());
        unsafe {
            assert_eq!(vetro_jit_st(state, SELFTEST_BASE, 8, 7), 0);
            assert_eq!(vetro_jit_ld(state, SELFTEST_BASE, 8), 7);
            assert_eq!(vetro_jit_ld(state, 0, 8), 0);
        }
        HOST.set(core::ptr::null_mut());
        let m = e.memory();
        assert_eq!(
            u32::from_le_bytes(m[state::EXIT_DETAIL..state::EXIT_DETAIL + 4].try_into().unwrap()),
            exit::FAULT
        );
        assert!(e.compile(&[0]).is_err(), "nessun motore JS fuori da wasm32");
    }
}
