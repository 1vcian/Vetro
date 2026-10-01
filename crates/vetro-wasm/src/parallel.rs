//! Cores in parallel in Web Workers (ADR 0042, step 3): the threads build
//! (`tools/wasm-threads.sh`) instantiates this module once per Worker on the
//! same shared memory; core 0 stays with the machine's Worker
//! (`vetro_run`), every other core runs in a Worker of its own
//! (`vetro_core_run`, `web/node/core-worker.mjs`).
//!
//! The calls of one core's Worker are `vetro_core_set_jit` once, then
//! `vetro_core_run` until `vetro_core_stopped`, then `vetro_core_drop_jit`.
//! The machine's Worker starts them with `vetro_parallel_start` and, once
//! every core's Worker has finished (after `vetro_parallel_request_stop`),
//! takes them back with `vetro_parallel_stop`: the machine is then
//! deterministic again (cores in turns) and can be saved or recorded.

use vetro_machine::{Core, Stop};

use crate::{Vm, jit, stop};

/// The JIT for one core, on the JS engine of the calling Worker.
fn core_jit(hot_threshold: u32, batch: u32) -> Box<dyn vetro_jit::SysJitDyn> {
    let cfg = vetro_jit::SysJitConfig {
        hot_threshold,
        batch: batch.max(1) as usize,
        memory: jit::memory_import(),
        ..vetro_jit::SysJitConfig::default()
    };
    Box::new(vetro_jit::SysJit::new(jit::JsEngine::default(), cfg))
}

fn stop_code(s: Stop) -> u32 {
    match s {
        Stop::Budget => stop::BUDGET,
        Stop::PowerOff => stop::POWER_OFF,
        Stop::Reset => stop::RESET,
        Stop::Idle => stop::IDLE,
        Stop::Unimplemented { .. } => stop::UNIMPLEMENTED,
        Stop::Blocked => stop::BLOCKED,
    }
}

/// Splits the machine for parallel execution: the number of cores that need
/// a Worker (cores 1..n), or 0 if refused (one core, recording or replay;
/// `vetro_message` says why) or not the threads build.
///
/// # Safety
/// `vm` comes from `vetro_machine_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_parallel_start(vm: *mut Vm) -> u32 {
    // SAFETY: see above.
    let vm = unsafe { &mut *vm };
    if !jit::THREADS {
        vm.message = "parallel cores need the threads build".into();
        return 0;
    }
    match vm.m.start_parallel() {
        Ok(cores) => {
            vm.cores = cores;
            vm.cores.len() as u32
        }
        Err(e) => {
            vm.message = e.into();
            0
        }
    }
}

/// Core `i` (1..n) for its Worker; null outside parallel execution.
///
/// # Safety
/// `vm` comes from `vetro_machine_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_parallel_core(vm: *mut Vm, i: u32) -> *mut Core {
    // SAFETY: see above.
    let vm = unsafe { &mut *vm };
    match vm.cores.get_mut((i as usize).wrapping_sub(1)) {
        Some(c) => c,
        None => core::ptr::null_mut(),
    }
}

/// Asks the cores to stop: their `vetro_core_run` returns soon and
/// `vetro_core_stopped` becomes 1.
///
/// # Safety
/// `vm` comes from `vetro_machine_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_parallel_request_stop(vm: *mut Vm) {
    // SAFETY: see above.
    unsafe { &*vm }.m.request_stop();
}

/// Takes the cores back, once their Workers have finished (called
/// `vetro_core_drop_jit`): the cores are in turns again.
///
/// # Safety
/// `vm` comes from `vetro_machine_new*`; no Worker uses its cores any more.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_parallel_stop(vm: *mut Vm) {
    // SAFETY: see above.
    let vm = unsafe { &mut *vm };
    let cores = core::mem::take(&mut vm.cores);
    vm.m.stop_parallel(cores);
}

/// 1 while the cores run in parallel.
///
/// # Safety
/// `vm` comes from `vetro_machine_new*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_parallel_active(vm: *mut Vm) -> u32 {
    // SAFETY: see above.
    u32::from(unsafe { &*vm }.m.is_parallel())
}

/// On the core's Worker: its JIT with threshold `hot_threshold` and `batch`
/// blocks per module; `hot_threshold` 0 = interpreter only.
///
/// # Safety
/// `core` comes from `vetro_parallel_core`, used by this Worker only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_core_set_jit(core: *mut Core, hot_threshold: u32, batch: u32) {
    // SAFETY: see above.
    let c = unsafe { &mut *core };
    c.set_jit((hot_threshold > 0).then(|| core_jit(hot_threshold, batch)));
}

/// Runs up to `budget` instructions of the core: a `stop` code.
///
/// # Safety
/// As [`vetro_core_set_jit`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_core_run(core: *mut Core, budget: u64) -> u32 {
    // SAFETY: see above.
    stop_code(unsafe { &mut *core }.run(budget))
}

/// 1 once the cores must stop (the host asked, or the machine powered off).
///
/// # Safety
/// As [`vetro_core_set_jit`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_core_stopped(core: *mut Core) -> u32 {
    // SAFETY: see above.
    u32::from(unsafe { &*core }.stopped())
}

/// Drops the core's JIT, on its Worker, before the Worker finishes.
///
/// # Safety
/// As [`vetro_core_set_jit`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_core_drop_jit(core: *mut Core) {
    // SAFETY: see above.
    unsafe { &mut *core }.drop_jit();
}

/// Instructions the core executed since the cores went parallel.
///
/// # Safety
/// As [`vetro_core_set_jit`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_core_executed(core: *mut Core) -> u64 {
    // SAFETY: see above.
    unsafe { &*core }.executed()
}
