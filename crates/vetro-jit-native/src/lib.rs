//! JIT engine on wasmtime (ADR 0012): runs outside the browser the
//! same modules that JavaScript's `WebAssembly` runs in the browser.
//!
//! A single `Store` with one linear memory (`env.mem`, where
//! `JitState` lives) and the functions `env.ld`/`env.st`, which call the [`Host`]
//! passed to [`Engine::run`]. Every compiled module becomes an instance in the
//! same store; wasmtime does not free instances before the store and
//! allows at most 10000: when `compile` fails the driver calls
//! [`Engine::reset`], which starts again with a new store.

use std::ptr::NonNull;

use vetro_jit::engine::TABLE_SIZE;
use vetro_jit::state::off;
use vetro_jit::{Engine, FAULT, Host, STOP};
use wasmtime::{
    Caller, Instance, Linker, Memory, MemoryType, Ref, RefType, Store, Table, TableType, TypedFunc,
};

/// Store data: the `Host` of the current run (only during `run`).
struct Ctx {
    host: Option<NonNull<dyn Host + 'static>>,
}

pub struct NativeEngine {
    store: Store<Ctx>,
    memory: Memory,
    table: Table,
    linker: Linker<Ctx>,
    /// The runtime module (`rt.*`), to re-instantiate after `reset`.
    runtime: Option<wasmtime::Module>,
}

/// Compiled and instantiated module: one function per block.
pub struct NativeModule {
    _instance: Instance,
    funcs: Vec<TypedFunc<i32, i32>>,
}

/// The `Host` of the current run.
///
/// # Safety
/// Must be called only inside `NativeEngine::run`, which sets the pointer to
/// a `&mut dyn Host` alive for the whole call and clears it at the end.
unsafe fn host<'a>(caller: &Caller<'_, Ctx>) -> &'a mut dyn Host {
    let p = caller.data().host.expect("ld/st outside a run");
    // SAFETY: see above; no other reference to the Host is live while
    // the WASM block runs.
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
        let engine = wasmtime::Engine::new(&config).expect("wasmtime configuration");
        let (store, memory, table, linker) = Self::store(&engine);
        NativeEngine { store, memory, table, linker, runtime: None }
    }

    /// New store with its memory and the `env.*` imports.
    fn store(engine: &wasmtime::Engine) -> (Store<Ctx>, Memory, Table, Linker<Ctx>) {
        let mut store = Store::new(engine, Ctx { host: None });
        let memory = Memory::new(&mut store, MemoryType::new(1, None)).expect("JIT memory");
        let table =
            Table::new(&mut store, TableType::new(RefType::FUNCREF, TABLE_SIZE, None), Ref::Func(None))
                .expect("JIT table");
        let mut linker = Linker::new(engine);
        linker.define(&store, "env", "mem", memory).expect("env.mem");
        linker.define(&store, "env", "tbl", table).expect("env.tbl");
        linker
            .func_wrap(
                "env",
                "ld",
                move |mut caller: Caller<'_, Ctx>, state: i32, va: i64, size: i32| -> i64 {
                    // SAFETY: called only from a block executed by `run`.
                    let h = unsafe { host(&caller) };
                    match h.ld(memory.data_mut(&mut caller), va as u64, size as u32) {
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
                    // SAFETY: called only from a block executed by `run`.
                    let h = unsafe { host(&caller) };
                    match h.st(memory.data_mut(&mut caller), va as u64, size as u32, value as u64) {
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
        linker
            .func_wrap("env", "vsync", move |mut caller: Caller<'_, Ctx>, state: i32| {
                // SAFETY: called only from a block executed by `run`.
                let h = unsafe { host(&caller) };
                h.vsync(memory.data_mut(&mut caller), state as u32)
            })
            .expect("env.vsync");
        linker
            .func_wrap(
                "env",
                "simd",
                move |mut caller: Caller<'_, Ctx>, state: i32, word: i32, x: i64, nzcv: i32| -> i64 {
                    let mem = memory.data_mut(&mut caller);
                    vetro_jit::helper::exec(mem, state as u32 as usize, word as u32, x as u64, nzcv as u32)
                        as i64
                },
            )
            .expect("env.simd");
        linker
            .func_wrap("env", "resolve", move |mut caller: Caller<'_, Ctx>, _state: i32| -> i32 {
                // SAFETY: called only from the dispatcher executed by `run`.
                let h = unsafe { host(&caller) };
                h.resolve(memory.data_mut(&mut caller)) as i32
            })
            .expect("env.resolve");
        (store, memory, table, linker)
    }
}

/// The system-mode JIT on wasmtime, for `Machine::set_jit`
/// (default configuration, threshold `hot_threshold`; with
/// `VETRO_JIT_PROFILE=1` it counts the interpreter's instructions by class).
pub fn system_jit(hot_threshold: u32) -> Box<dyn vetro_jit::SysJitDyn> {
    let profile = std::env::var("VETRO_JIT_PROFILE").is_ok_and(|v| v == "1");
    let cfg = vetro_jit::SysJitConfig { hot_threshold, profile, ..vetro_jit::SysJitConfig::default() };
    Box::new(vetro_jit::SysJit::new(NativeEngine::new(), cfg))
}

impl Default for NativeEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeEngine {
    /// Instantiates the runtime in the store and offers its exports as `rt.*`.
    fn link_runtime(&mut self) -> Result<(), String> {
        let Some(m) = &self.runtime else { return Ok(()) };
        let inst = self.linker.instantiate(&mut self.store, m).map_err(|e| format!("{e:#}"))?;
        self.linker.instance(&mut self.store, "rt", inst).map_err(|e| format!("{e:#}"))?;
        Ok(())
    }
}

impl Engine for NativeEngine {
    type Module = NativeModule;

    fn runtime(&mut self, wasm: &[u8]) -> Result<(), String> {
        self.runtime = Some(wasmtime::Module::new(self.store.engine(), wasm).map_err(|e| format!("{e:#}"))?);
        self.link_runtime()
    }

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
        // SAFETY: only the lifetime is erased; the pointer stays valid for
        // the whole call and is cleared before returning.
        let p: NonNull<dyn Host + 'static> = unsafe { std::mem::transmute(p) };
        self.store.data_mut().host = Some(p);
        let r = m.funcs[index as usize].call(&mut self.store, state as i32);
        self.store.data_mut().host = None;
        match r {
            Ok(code) => code as u32,
            // The translator's modules have no traps (no divisions by
            // zero, no out-of-memory accesses): a trap is a bug.
            Err(e) => panic!("trap in a JIT block: {e:#}"),
        }
    }

    fn memory(&mut self) -> &mut [u8] {
        self.memory.data_mut(&mut self.store)
    }

    fn place(&mut self, m: &NativeModule, count: u32, base: u32) {
        for i in 0..count {
            let f = *m.funcs[i as usize].func();
            self.table.set(&mut self.store, (base + i) as u64, Ref::Func(Some(f))).expect("table entry");
        }
    }

    fn reserve(&mut self, bytes: usize) {
        let have = self.memory.data_size(&self.store);
        if have < bytes {
            let pages = (bytes - have).div_ceil(65536) as u64;
            self.memory.grow(&mut self.store, pages).expect("JIT memory");
        }
    }

    /// Blocks reach only the store's memory: this holds for the host's
    /// bytes that live in it (the tests use it as guest RAM).
    fn host_address(&mut self, p: *const u8, len: usize) -> Option<u32> {
        let base = self.memory.data_ptr(&self.store) as usize;
        let size = self.memory.data_size(&self.store);
        let off = (p as usize).checked_sub(base)?;
        (off.checked_add(len)? <= size && off + len <= u32::MAX as usize).then_some(off as u32)
    }

    /// New store: the old instances (and the memory) are freed with it.
    fn reset(&mut self) {
        let (store, memory, table, linker) = Self::store(self.store.engine());
        self.store = store;
        self.memory = memory;
        self.table = table;
        self.linker = linker;
        self.link_runtime().expect("JIT runtime");
    }
}
