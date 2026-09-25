//! Motore del JIT su wasmtime (ADR 0012): esegue fuori dal browser gli
//! stessi moduli che nel browser esegue il `WebAssembly` di JavaScript.
//!
//! Un solo `Store` con una memoria lineare (`env.mem`, dove sta
//! `JitState`) e le funzioni `env.ld`/`env.st`, che chiamano il [`Host`]
//! passato a [`Engine::run`]. Ogni modulo compilato diventa un'istanza nello
//! stesso store; wasmtime non libera le istanze prima dello store e ne
//! permette al più 10000: quando `compile` fallisce il driver chiama
//! [`Engine::reset`], che ricomincia con uno store nuovo.

use std::ptr::NonNull;

use vetro_jit::state::off;
use vetro_jit::{Engine, FAULT, Host, STOP};
use wasmtime::{Caller, Instance, Linker, Memory, MemoryType, Store, TypedFunc};

/// Dati dello store: il `Host` della corsa in corso (solo durante `run`).
struct Ctx {
    host: Option<NonNull<dyn Host + 'static>>,
}

pub struct NativeEngine {
    store: Store<Ctx>,
    memory: Memory,
    linker: Linker<Ctx>,
}

/// Modulo compilato e istanziato: una funzione per blocco.
pub struct NativeModule {
    _instance: Instance,
    funcs: Vec<TypedFunc<i32, i32>>,
}

/// Il `Host` della corsa in corso.
///
/// # Safety
/// Va chiamata solo dentro `NativeEngine::run`, che imposta il puntatore a
/// un `&mut dyn Host` vivo per tutta la chiamata e lo toglie alla fine.
unsafe fn host<'a>(caller: &Caller<'_, Ctx>) -> &'a mut dyn Host {
    let p = caller.data().host.expect("ld/st fuori da una corsa");
    // SAFETY: vedi sopra; nessun altro riferimento al Host è attivo mentre
    // il blocco WASM gira.
    unsafe { &mut *p.as_ptr() }
}

fn set_detail(memory: Memory, caller: &mut Caller<'_, Ctx>, state: i32, code: u32) {
    let o = state as u32 as usize + off::EXIT_DETAIL as usize;
    memory.data_mut(caller)[o..o + 4].copy_from_slice(&code.to_le_bytes());
}

impl NativeEngine {
    pub fn new() -> Self {
        let mut config = wasmtime::Config::new();
        config.cranelift_opt_level(wasmtime::OptLevel::Speed);
        let engine = wasmtime::Engine::new(&config).expect("configurazione di wasmtime");
        let (store, memory, linker) = Self::store(&engine);
        NativeEngine { store, memory, linker }
    }

    /// Store nuovo con la sua memoria e gli import `env.*`.
    fn store(engine: &wasmtime::Engine) -> (Store<Ctx>, Memory, Linker<Ctx>) {
        let mut store = Store::new(engine, Ctx { host: None });
        let memory = Memory::new(&mut store, MemoryType::new(1, None)).expect("memoria del JIT");
        let mut linker = Linker::new(engine);
        linker.define(&store, "env", "mem", memory).expect("env.mem");
        linker
            .func_wrap(
                "env",
                "ld",
                move |mut caller: Caller<'_, Ctx>, state: i32, va: i64, size: i32| -> i64 {
                    // SAFETY: chiamata solo da un blocco eseguito da `run`.
                    let h = unsafe { host(&caller) };
                    match h.ld(va as u64, size as u32) {
                        Ok(v) => v as i64,
                        Err(()) => {
                            set_detail(memory, &mut caller, state, FAULT);
                            0
                        }
                    }
                },
            )
            .expect("env.ld");
        linker
            .func_wrap(
                "env",
                "st",
                move |mut caller: Caller<'_, Ctx>, state: i32, va: i64, size: i32, value: i64| -> i32 {
                    // SAFETY: chiamata solo da un blocco eseguito da `run`.
                    let h = unsafe { host(&caller) };
                    match h.st(va as u64, size as u32, value as u64) {
                        Ok(false) => 0,
                        Ok(true) => {
                            set_detail(memory, &mut caller, state, STOP);
                            1
                        }
                        Err(()) => {
                            set_detail(memory, &mut caller, state, FAULT);
                            1
                        }
                    }
                },
            )
            .expect("env.st");
        (store, memory, linker)
    }
}

impl Default for NativeEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine for NativeEngine {
    type Module = NativeModule;

    fn compile(&mut self, wasm: &[u8]) -> Result<NativeModule, String> {
        let module = wasmtime::Module::new(self.store.engine(), wasm).map_err(|e| format!("{e:#}"))?;
        let instance = self.linker.instantiate(&mut self.store, &module).map_err(|e| format!("{e:#}"))?;
        let mut funcs = Vec::new();
        while let Ok(f) = instance.get_typed_func::<i32, i32>(&mut self.store, &format!("b{}", funcs.len())) {
            funcs.push(f);
        }
        Ok(NativeModule { _instance: instance, funcs })
    }

    fn run(&mut self, m: &NativeModule, index: u32, state: u32, host: &mut dyn Host) -> u32 {
        let p: NonNull<dyn Host + '_> = NonNull::from(host);
        // SAFETY: si cancella solo la durata; il puntatore resta valido per
        // tutta la chiamata e si toglie prima di tornare.
        let p: NonNull<dyn Host + 'static> = unsafe { std::mem::transmute(p) };
        self.store.data_mut().host = Some(p);
        let r = m.funcs[index as usize].call(&mut self.store, state as i32);
        self.store.data_mut().host = None;
        match r {
            Ok(code) => code as u32,
            // I moduli del traduttore non hanno trap (niente divisioni per
            // zero, niente accessi fuori dalla memoria): una trap è un bug.
            Err(e) => panic!("trap in un blocco del JIT: {e:#}"),
        }
    }

    fn memory(&mut self) -> &mut [u8] {
        self.memory.data_mut(&mut self.store)
    }

    /// Store nuovo: le istanze vecchie (e la memoria) si liberano con lui.
    fn reset(&mut self) {
        let (store, memory, linker) = Self::store(self.store.engine());
        self.store = store;
        self.memory = memory;
        self.linker = linker;
    }
}
