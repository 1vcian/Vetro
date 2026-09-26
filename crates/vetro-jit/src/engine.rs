//! Interfacce verso il motore che esegue i moduli WASM (docs/specs/jit.md).

/// Voci della tabella di funzioni `env.tbl` che ogni motore fornisce al
/// dispatcher della modalità sistema: il motore vi mette i blocchi
/// ([`Engine::place`]) e il dispatcher li chiama con `call_indirect`.
pub const TABLE_SIZE: u32 = 1 << 18;

/// Motore WASM: compila i moduli del traduttore e ne esegue i blocchi.
///
/// Le implementazioni: `vetro-jit-native` (wasmtime, test e `vetro --jit`)
/// e, in `vetro-wasm`, il `WebAssembly` di JavaScript.
///
/// Ogni modulo si istanzia con gli import `env.mem` (la memoria di
/// [`memory`](Self::memory)), `env.ld`/`env.st`/`env.resolve` (che
/// chiamano l'[`Host`] della corsa) e, se li dichiara, `env.tbl`: una
/// tabella `funcref` di [`TABLE_SIZE`] voci, la stessa fino al prossimo
/// [`reset`](Self::reset). Solo il dispatcher importa la tabella (V8 dà a
/// ogni istanza che importa una tabella una sua tabella di dispatch grande
/// quanto quella) e `env.resolve`.
pub trait Engine {
    type Module;
    /// Installa il modulo di runtime (`translate::runtime`, ADR 0024): il
    /// motore lo istanzia con gli import `env.*` e ne offre gli export come
    /// import `rt.<nome>` a tutti i moduli compilati dopo. Resta installato
    /// anche dopo [`reset`](Self::reset) (il motore lo reistanzia se serve).
    fn runtime(&mut self, wasm: &[u8]) -> Result<(), String>;
    /// Compila un modulo WASM generato dal traduttore.
    fn compile(&mut self, wasm: &[u8]) -> Result<Self::Module, String>;
    /// Esegue il blocco `index` del modulo sullo stato all'indirizzo
    /// `state` della memoria condivisa; `ld`/`st` chiamano `host`.
    fn run(&mut self, m: &Self::Module, index: u32, state: u32, host: &mut dyn Host) -> u32;
    /// La memoria condivisa (dove sta `JitState`).
    fn memory(&mut self) -> &mut [u8];
    /// Mette gli export `b0`..`b<count-1>` del modulo nelle voci `base..`
    /// della tabella `env.tbl`. Il default rifiuta: serve solo alla
    /// modalità sistema.
    fn place(&mut self, _m: &Self::Module, _count: u32, _base: u32) {
        unimplemented!("questo motore non ha la tabella dei blocchi");
    }
    /// Scarta tutti i moduli compilati (e può ricreare la memoria
    /// condivisa e la tabella): il chiamante non li userà più. Serve ai
    /// motori che non liberano i moduli da soli (wasmtime tiene ogni istanza
    /// fino alla fine dello store) e quando la tabella è piena.
    fn reset(&mut self) {}
    /// Garantisce che [`memory`](Self::memory) abbia almeno `bytes` byte.
    /// Il default controlla soltanto.
    fn reserve(&mut self, bytes: usize) {
        assert!(self.memory().len() >= bytes, "memoria del motore JIT troppo piccola: servono {bytes} byte");
    }
    /// Indirizzo nella memoria `env.mem` dei `len` byte dell'host che
    /// iniziano a `p`, se i blocchi li possono raggiungere direttamente
    /// (nel browser `env.mem` è la memoria stessa di vetro-wasm). `None`: i
    /// blocchi passano sempre da `ld`/`st`.
    fn host_address(&mut self, _p: *const u8, _len: usize) -> Option<u32> {
        None
    }
}

/// Accessi alla memoria del guest per conto dei blocchi (`env.ld`,
/// `env.st`). Un errore è un fault: il motore scrive
/// `JitState::exit_detail = FAULT` e il blocco esce. `mem` è la memoria del
/// motore (quella di [`Engine::memory`]): l'host vi può aggiornare la TLB
/// software dei blocchi.
// La firma è quella della spec (docs/specs/jit.md): il fault non ha dettagli,
// che restano all'host.
#[allow(clippy::result_unit_err)]
pub trait Host {
    fn ld(&mut self, mem: &mut [u8], va: u64, size: u32) -> Result<u64, ()>;
    /// Ok(true) = fermati dopo questa istruzione.
    fn st(&mut self, mem: &mut [u8], va: u64, size: u32, value: u64) -> Result<bool, ()>;
    /// `env.resolve` del dispatcher: manca la voce della cache dei salti per
    /// il `pc` di `JitState`; vero se l'host l'ha scritta (il dispatcher
    /// continua), falso se il dispatcher deve tornare all'host con `NEXT`.
    fn resolve(&mut self, _mem: &mut [u8]) -> bool {
        false
    }
    /// `env.vsync` (ADR 0024): copia i registri SIMD/FP della `Cpu` nel
    /// `JitState` all'indirizzo `state` di `mem` e mette `v_valid` = 1
    /// ([`crate::state::vsync_in`]). La chiama la prima regione della corsa
    /// che usa i registri SIMD; chi ricopia lo stato nella `Cpu` riporta
    /// anche i registri se `v_valid`.
    fn vsync(&mut self, _mem: &mut [u8], _state: u32) {
        unreachable!("questo host non ha registri SIMD");
    }
}
