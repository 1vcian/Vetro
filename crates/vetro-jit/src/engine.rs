//! Interfacce verso il motore che esegue i moduli WASM (docs/specs/jit.md).

/// Motore WASM: compila i moduli del traduttore e ne esegue i blocchi.
///
/// Le implementazioni: `vetro-jit-native` (wasmtime, test e `vetro --jit`)
/// e, in `vetro-wasm`, il `WebAssembly` di JavaScript.
pub trait Engine {
    type Module;
    /// Compila un modulo WASM generato dal traduttore.
    fn compile(&mut self, wasm: &[u8]) -> Result<Self::Module, String>;
    /// Esegue il blocco `index` del modulo sullo stato all'indirizzo
    /// `state` della memoria condivisa; `ld`/`st` chiamano `host`.
    fn run(&mut self, m: &Self::Module, index: u32, state: u32, host: &mut dyn Host) -> u32;
    /// La memoria condivisa (dove sta `JitState`).
    fn memory(&mut self) -> &mut [u8];
    /// Scarta tutti i moduli compilati (e può ricreare la memoria
    /// condivisa): il chiamante non li userà più. Serve ai motori che non
    /// liberano i moduli da soli (wasmtime tiene ogni istanza fino alla
    /// fine dello store); il chiamante lo usa quando `compile` fallisce.
    /// Estensione rispetto a docs/specs/jit.md, con un default vuoto.
    fn reset(&mut self) {}
}

/// Accessi alla memoria del guest per conto dei blocchi (`env.ld`,
/// `env.st`). Un errore è un fault: il motore scrive
/// `JitState::exit_detail = FAULT` e il blocco esce.
// La firma è quella della spec (docs/specs/jit.md): il fault non ha dettagli,
// che restano all'host.
#[allow(clippy::result_unit_err)]
pub trait Host {
    fn ld(&mut self, va: u64, size: u32) -> Result<u64, ()>;
    /// Ok(true) = fermati dopo questa istruzione.
    fn st(&mut self, va: u64, size: u32, value: u64) -> Result<bool, ()>;
}
