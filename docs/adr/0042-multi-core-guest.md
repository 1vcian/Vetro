# ADR 0042 — Multi-core guest: deterministic cores in turns, parallel cores in Workers

- Status: accepted (M4, 2026-10-02). Carries out the three steps of ADR 0038
  ("the recommendation for the big lever"). Extends ADR 0002 (threads), 0013,
  0024, 0036 (JIT), 0019 (record & replay), 0028 (Android in the browser).
  Measurements: `docs/progress/M4.md` (2026-10-02).

## Context
ADR 0038 measured QEMU on our Android image: 1.74x with 2 vCPUs and 2.76x with
4 under MTTCG, nothing with `-smp 2 thread=single`. The owner wants at least 4
cores in the browser, chosen automatically from the host, with record & replay
staying deterministic. The constraints are the project's: fidelity checked
against QEMU, determinism as the default, standard browser mechanisms
(WASM threads, SharedArrayBuffer, Atomics, Workers), measured costs.

## Decision

### Step 1 — threads build and isolation
- `tools/wasm-threads.sh` builds `vetro_wasm_threads.wasm` from the same
  sources: `+atomics,+bulk-memory,+mutable-globals`, `-Z build-std`
  (pinned nightly, ADR 0002), memory imported as a shared
  `WebAssembly.Memory` (32 MiB initial, 4 GiB maximum), TLS and stack exports
  for the core Workers. The ordinary build stays and is the default:
  `vetro_threads()` tells them apart.
- `web/node/vetro.mjs` creates the shared memory when the module imports
  one; JIT modules import a shared memory too (`MemoryImport::shared_max`).
  `TextDecoder` cannot read a SharedArrayBuffer view in Chrome: copies.
- Isolation: COOP/COEP from our server (`tools/web-serve.mjs`), and
  `coi-serviceworker` 0.1.7 (MIT, vendored unmodified) where the host cannot
  send headers (Pages). It registers only when the page asks for threads
  (`?threads=1` or `cpus` other than 1), so the default page is unchanged.
- Cost of the threads build with one core: guest kernel boot, JIT in V8, Mac:
  1754 against 1746 ms (5 interleaved runs, noise). Android in Node on the VM:
  same instructions in every phase.

### Step 2 — the SMP machine, deterministic
- `MachineConfig::cpus` (1..=16). One core keeps every byte of today's
  machine: device tree, snapshot sections, configuration hash (the prebuilt
  snapshots keep their keys), record log.
- Platform: GICv3 with one redistributor per core (`Gic::with_cpus`, banked
  SGIs/PPIs, SPI routing by IROUTER, ICC_SGI1R_EL1 with target list, RS and
  IRM, GICR_TYPER Last), one generic timer per core, the device tree with
  `cpu@N`, phandles and QEMU's `cpu-map`. PSCI CPU_ON / AFFINITY_INFO /
  CPU_OFF with QEMU 10's return codes.
- Scheduling: the cores run in turns of `QUANTUM` = 2^16 clock steps on the
  machine's thread; the running core lives in `Machine` (CPU, interpreter,
  pending WFI), the others are parked and swapped in. The clock is the sum of
  every core's instructions; CNTPCT is `clock / n`, so guest time advances as
  with one core. WFE and YIELD end the turn (`SysEvent::Yield`; the JIT
  leaves them to the interpreter when `set_yields`), WFI skips to the end of
  the turn, `Idle` only when every core waits with no deadline. One MMU/TLB
  (TLBI broadcast is the same TLB) and one JIT cache shared by the cores.
- Snapshot: `SMP ` section, `TMRS`, the GIC's extra per-core state; record
  log `HEAD` with an optional core count; the CPU hash covers every core.
- Oracle: the guest kernel booted with 2 cores gives QEMU 10's
  `-smp 2 -accel tcg,thread=single` boot log (`qemu-boot-smp2.log`); the JIT
  gives the same instruction count and log as the interpreter (native and
  V8); the kselftest suite with `VETRO_KSELFTEST_CPUS=2` has QEMU's outcomes;
  record and replay with 2 cores are identical.

### Step 3 — parallel cores (opt-in)
- `Machine::start_parallel` hands cores 1..n to host threads (one Worker per
  core in the browser, `worker_threads` in Node; `web/node/core-worker.mjs`
  instantiates the module on the same memory with its own stack, TLS and
  `JitEngine`). Core 0 stays in `Machine::run`, which also serves inputs,
  devices and the network. `stop_parallel` brings the cores back into turns:
  snapshots and recordings are always taken deterministic.
- Shared state: guest RAM (`Ram` with a `&self` API, atomic code-watch
  bitmap, aligned accesses up to 8 bytes atomic), the board (devices, GIC,
  timers) behind one mutex (`BoardCell`, debug re-entrance check). Each core
  has a slot: cached IRQ line, kick counter, abort flag (also zeroes the JIT's
  run limit), sleep condvar, inbox of TLBI and code-watch requests with
  acknowledgements.
- Memory model: AArch64 is weaker than WASM's sequentially consistent atomics,
  so making the guest's ordering points atomic is enough. Interpreter steps
  end with a fence; the JIT (`set_parallel`) turns DMB/DSB/ISB into
  `atomic.fence`, LDAR/STLR into atomic accesses, exclusive loads add a fence,
  STXR is an `i64.atomic.rmw.cmpxchg` against the monitor's value (value
  compare, like QEMU), STXP of 16 bytes goes to the interpreter (under the
  board lock).
- TLB maintenance: TLBI of the Inner Shareable domain is broadcast and waited
  for (each core flushes at its next safe point); code watches likewise
  (`SysPhys::watch_ready`: a block is not translated while another core may
  still write the page directly).
- Time: one shared clock, each core adds its steps (atomic add, also inside
  JIT regions for CNTPCT reads: `time_ok` 2); when every core is idle the
  clock jumps to the next deadline, never past the host quantum of core 0.
- Replay is refused while the cores are parallel; the app stops them before
  recording, replaying or saving.
- Litmus tests (`crates/vetro-machine/tests/litmus.rs` on wasmtime,
  `tests/web/litmus.mjs` in V8): SB+dmbs, MP+dmbs, MP+rel+acq never show the
  forbidden outcome; the exclusive counter loses no increment. Each is red
  when its fence or the cmpxchg is removed.

### The web app
`autoCpus(navigator.hardwareConcurrency)`: 4 with at least 6 logical cores, 2
with 3–5, 1 below. Selector 1/2/4/auto; **1 stays the default** until the
parallel mode is proven on Android. Without cross-origin isolation the app
falls back to 1. Record & replay forces one deterministic turn order (the
parallel cores are stopped). Devices are serviced by whichever core touches
them, under the board's lock; the accelerated graphics (ADR 0037,
experimental) call WebGL on the app's Worker, which a core Worker cannot
reach, so with `gpu=webgl` the cores stay in turns.

## Maintenance burden
- Step 1: 1/5 (a build script, a vendored file, the loader).
- Step 2: 3/5 (the multi-core GIC and timers; the turn scheduler; one more
  snapshot section). Checked by QEMU on every boot test.
- Step 3: 4/5. Concurrency bugs do not reproduce; the litmus tests and the
  parallel boot test are the only net. Kept opt-in for that reason.

## Consequences
- vetro-wasm exports for several cores (`docs/specs/wasm.md`); platform,
  snapshot, replay and JIT specs updated.
- `prebuilt-snapshot.mjs --cpus N [--parallel]`, `android-perf.mjs --cpus N
  [--parallel]`, `boot.mjs --cpus N [--parallel]`, `vetro-cli --smp=N`.
- Prebuilt snapshots exist per core count (the core count is part of the key).
