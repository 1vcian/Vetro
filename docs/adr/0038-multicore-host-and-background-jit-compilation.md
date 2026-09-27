# ADR 0038 — More than one host core: the design, and JIT compilation in a Worker

- Status: accepted (M4, 2026-09-27). Design for M5–M10; the prototype
  (background JIT compilation) is in, off by default. Extends ADR 0002
  (threads), 0013/0024/0036 (JIT), 0019 (record & replay), 0028 (Android in
  the browser).

## Context
Vetro runs the whole machine on one thread: the app's Worker (Chrome) or
Node's main thread. The single-core JIT is now in the regions themselves
(ADR 0036): the remaining large lever is using the other cores of the host.
The owner's guidance: prefer what Chrome offers natively and standard
approaches (WASM threads, SharedArrayBuffer, Atomics, Workers,
OffscreenCanvas) over custom machinery, and weigh the maintenance burden.

Three families of options:
- **(a)** an SMP guest (2–4 vCPUs), one host thread (Worker) per vCPU;
- **(b)** an SMP guest scheduled round-robin on one host thread;
- **(c)** other work moved to other threads without guest SMP: JIT
  compilation, frame conversion, snapshot compression, I/O.

## Measurements
Build VM (8 vCPU x86-64, shared with other agents: load 7-12 during these
runs, so absolute wall times are inflated; comparisons are between runs made
at the same time), image 64fcd35.

### How parallel is Android? (QEMU, the oracle)
The same image under `qemu-system-aarch64` (TCG, `-cpu cortex-a53`, 3 GiB,
same devices as `tools/aosp/qemu.sh`), in-guest `/proc/pressure/cpu`,
`/proc/loadavg` and `/proc/stat` read with adb:

TBD-QEMU-TABLE

- With one vCPU, CPU pressure "some" (at least one runnable task waiting
  for the CPU) is **94% of the whole boot** (1362 of 1442 s up to
  sys.boot_completed) and 97% up to the launcher; `procs_running` 20–49,
  load average 21–31, idle time 0. Under Vetro the same holds in guest
  time: `top` after the launcher idle shows load 23 (ADR 0036) and there is
  never a WFI after the home screen. Android always has work for more CPUs.

### Where the machine thread's time goes (V8)
V8 CPU profile of the Android workload (`tools/aosp/android-perf.mjs`, main
before ADR 0036): WASM compilation on the machine thread
(`new WebAssembly.Module` 3.1%, `new WebAssembly.Instance` 0.9%, the JS
glue 0.3%) is about 4%; ADR 0036 measured 3–6% after its changes. The Rust
translator (`translate::module`, `discover`) is under 1%. Everything else
(regions 50%+, host ld/st, lookups, interpreter) is the guest's own work:
only more guest CPUs can take it to other cores.

## Options

### (a) SMP guest, one Worker per vCPU
What it needs, by area:
- **Platform (standard).** vetro-wasm built with `+atomics,+bulk-memory`
  and `-Z build-std` (the nightly of ADR 0002 already allows it), memory
  imported as a shared `WebAssembly.Memory` with a declared maximum (4 GiB),
  one module Worker per vCPU instantiating the same module on it. Shared
  memory needs cross-origin isolation (COOP/COEP): `tools/web-serve.mjs`
  already sends the headers; GitHub Pages cannot, so either our own host
  (`vetro.lol`, headers set at the edge: standard, preferred) or the
  `coi-serviceworker` shim (a service worker adding the headers, costs a
  reload on the first visit) as a fallback for Pages.
- **Memory model.** Guest plain loads/stores stay plain WASM accesses on the
  shared memory (no cost). DMB/DSB → `atomic.fence`; LDAR/STLR/LDAPR →
  `i64.atomic.load/store` (sequentially consistent, stronger than needed,
  correct); LSE atomics (LDADD, SWP, CAS...) → `atomic.rmw*`; CASP and other
  128-bit atomics have no WASM form → a global "exclusive section" that stops
  the other vCPUs (QEMU's `cpu_exec_step_atomic`). The JS memory model gives
  no ordering to non-atomic accesses, so ARM's address-dependency ordering
  (Linux's `READ_ONCE`/RCU without barriers) relies on V8 not reordering a
  load before the load that computes its address, and on the host CPU (x86
  TSO, or ARM, which keeps dependencies): the same class of argument QEMU's
  MTTCG relies on; it needs litmus tests (herd7's AArch64 suite) as
  differential tests, since QEMU cannot be the oracle of a race.
- **Exclusives.** LDXR records address and value (already in `JitState`);
  STXR becomes an atomic compare-and-exchange against the value read
  (QEMU's approach: ABA is accepted, Linux does not depend on it). The
  interpreter must do the same, so both tiers agree.
- **TLB shootdown.** TLBI ...IS and IC IALLUIS/IVAU go to every vCPU: a
  per-vCPU request word (Atomics), applied at the next region boundary, and
  the issuer waits for the acknowledgements at its next DSB (QEMU's
  `*_synced` flushes). Regions must then notice requests promptly: the
  per-basic-block `limit` check has to read an atomic word in memory
  (today the limit is a value the region already has), also on back-edges
  (a spinlock loop inside one region would otherwise never exit). The code
  invalidation bitmap becomes shared; a write to another vCPU's code page
  requests an exit on all vCPUs (ARM already requires IC IVAU + ISB for
  cross-modifying code).
- **GIC and timers.** One redistributor per vCPU (the GICv3 model already has
  the frame layout), SGIs from ICC_SGI1R_EL1 set the target's pending bit and
  wake it: `Atomics.notify` if it sleeps in WFI (`Atomics.wait` with a
  timeout at its next timer deadline; allowed in Workers), an exit request if
  it runs. PSCI CPU_ON, one generic timer per vCPU, `/cpus` in the FDT.
- **Devices.** One lock around MMIO and device services (QEMU's big lock),
  virtio DMA into the shared RAM; device work stays single-threaded.
- **JIT.** Each Worker has its own engine, block table and dispatcher (V8
  tables are per agent), but compiled modules are shared: a
  `WebAssembly.Module` posted to another Worker keeps its compiled code
  (V8's NativeModule is shared; measured below). The background compiler of
  this ADR is exactly the piece a shared translation cache needs.
- **Time.** Instruction count as time (ADR 0011) cannot stay deterministic
  with parallel vCPUs. Fast mode: a global virtual clock advanced from the
  vCPUs' instruction counters (atomics), or the host clock (the app's
  `realtime` option already exists).
- **Determinism and replay.** Parallel vCPUs interleave shared-memory accesses
  in an order the host decides. Recording it needs either page-ownership
  tracking (SMP-ReVirt/CREW: a protection fault per ownership change, 2–10x
  slower on sharing-heavy code like Android's) or deterministic multithreading
  (Kendo/CoreDet: store buffers committed at quantum boundaries, incompatible
  with direct stores through the software TLB). QEMU's own record/replay
  requires single-threaded TCG. So: **two modes**. *Deterministic* (default
  whenever something is recorded or replayed, and for the analysis timeline):
  the vCPUs round-robin on one thread with fixed quanta (option b), time is
  instructions, replay is identical. *Fast* (opt-in, interactive use):
  parallel vCPUs, recording and replay disabled, snapshots taken at a
  stop-the-world point (a snapshot is a consistent state for either mode, so
  one can switch at a snapshot), analysis hooks still observe (they do not
  need determinism) but the timeline is best effort.
- **Gain.** Bounded by the guest's parallelism (plenty, above) and by
  contention (device lock, shootdowns, invalidations, atomics). QEMU's MTTCG
  on the same image: TBD-MTTCG-GAIN.
- **Cost.** Very high and permanent: the interpreter and the JIT (barriers,
  atomics, polled limits), the MMU (shootdown), the machine (per-CPU state,
  the lock), the platform (GIC, PSCI, timers, FDT), vetro-wasm (threads
  build, one instance per Worker), the web app (isolation, Workers), the
  snapshot format (N CPUs), replay (modes). A new class of bugs (races,
  heisenbugs) that the QEMU oracle can only check in deterministic mode.
  **Maintenance burden: 5/5.**

### (b) SMP guest round-robin on one thread
No speedup: the same host core runs all vCPUs. Android does not need it for
correctness: it reaches the home screen on one vCPU under Vetro (ADR 0028)
and under QEMU. Measured under QEMU: `-smp 2 -accel tcg,thread=single` reached
sys.boot_completed at 1402 s against 1429 s with `-smp 1` (no gain), and
later rebooted with `vold-failed` (two half-speed vCPUs hit a vold timeout).
Under Vetro, with time = instructions shared by the vCPUs, each vCPU would
get half the guest-time throughput; with a clock per epoch instead, guest
time would slow down in wall terms. It is only worth building as step 1 of
(a) (the deterministic mode, checkable against QEMU with `-smp 2
thread=single`). **Maintenance burden: 3/5** (the SMP machine model).

### (c) Offloading without guest SMP
- **JIT compilation in a Worker** (prototype below): standard APIs only
  (Worker, `WebAssembly.Module` posted between threads). Ceiling: the ~4% of
  the machine thread that V8 compilation takes. **Maintenance burden: 1/5.**
- **Frame conversion / presentation**: belongs to the GPU work (ADR 0037:
  WebGPU/OffscreenCanvas already move it off the machine thread).
- **Snapshot compression**: the save is 17–19 s in Chrome (ADR 0028) and
  pages are compressed independently, so N Workers give the same bytes; it
  needs either the threads build (to share RAM without copying) or a 2 GiB
  copy. A UX gain (M6), not a speed gain for the running guest. Burden 2/5.
- **Disk and network I/O**: already asynchronous (fetch, OPFS in the Worker's
  event loop; the browser does the I/O on its own threads); the network
  stack runs inside the machine on purpose (ADR 0016, determinism).

## Decision

### Prototype: JIT modules compiled in a Worker (behind a flag)
- `Engine::ready(&Module) -> bool` (default true, docs/specs/jit.md): an
  engine may finish compiling a module later. `SysJit` keeps a module that
  is not ready in a short list, polled at the start of every `run`, and
  until then `Cache::lookup` answers `Cold` for its regions (also for
  `env.resolve`) and `run` goes to the interpreter right after compiling it.
  So no jump cache entry can name a region that is not ready, and readiness
  only changes between runs.
- vetro-wasm: `JsEngine::ready` → new import `vetro_jit.ready(module)` (ABI 14).
- `web/node/jit-engine.mjs`: `startBackground()` starts
  `web/node/jit-compiler.mjs` (a module Worker in the browser, a
  `worker_threads` Worker in Node). `compile` posts the bytes and returns the
  id at once; `place` is recorded; when the Worker posts the module back (at
  the next turn of the event loop between two quanta), the machine thread
  instantiates it and places its regions. A module needed at once (`entry`:
  the dispatcher) is compiled on the spot; a reset discards late arrivals.
- **Why the Worker compiles more than it validates.** V8 compiles WASM
  lazily: `new WebAssembly.Module` decodes and validates, and Liftoff
  compiles each function on its first call, on the calling thread. The
  Worker therefore instantiates the module against a scratch memory and
  calls each region once on a zeroed `JitState` (limit 0: every region exits
  before its first instruction) — the code lands in V8's NativeModule, which
  the posted module shares. Measured (Node 22, synthetic module of 400
  functions, 2.2 MB): first calls on the main thread 39.7 ms when compiled
  there, 1.5 ms after the Worker warmed the module; instantiating 0.2 ms.
- Flags: `JitEngine.startBackground()`; `node web/node/boot.mjs --jit
  --jit-background`; `tools/aosp/android-perf.mjs --bg-compile`; the app's
  `?jitbg=1`; `tools/aosp/chrome-jit-bg.mjs` compares both in headless
  Chrome. Off by default everywhere.

### What stays guaranteed
- Flag off: nothing changes (the engine is always ready; same code path).
- Flag on: the guest-visible execution is the same as with the synchronous
  JIT and as with the interpreter: same instructions, same console, same
  state. Only *which tier* runs an instruction depends on host timing, and
  the tiers are equal by the parity suites. Record & replay (ADR 0019) is
  unaffected: events are placed by instruction count, and a log recorded with
  one mode replays with the other. The one documented exception is ADR 0013's:
  the MMU's TLB sees fewer accesses under the JIT, and with background
  compilation *how many* fewer depends on timing; the digest already ignores
  the TLB when the JIT is used, and only a guest that edits page tables
  without TLBI could tell.
- JIT counters (`jitStats`: regions, runs, interpreter steps) are no longer
  reproducible run to run with the flag on.

### Results of the prototype
TBD-RESULTS

### The recommendation for the big lever
TBD-RECOMMENDATION

## Verification
- `vetro-jit-native` `background_compilation_keeps_the_execution`: 200 random
  system programs on an engine whose modules become ready after 0–3 polls and
  reach the block table only then: identical to the interpreter. Red if the
  readiness check is removed from `Cache::lookup` or from `SysJit::run` (the
  dispatcher hits an empty table entry: "uninitialized element").
- Guest kernel boot in Node with `--jit-background`: same 285786580
  instructions and byte-identical log as the synchronous JIT (3 runs).
- Android workload in Node: the same instructions in every phase with and
  without `--bg-compile`.

## Consequences
- JIT ABI: `Engine::ready`; vetro-wasm ABI 14 (`vetro_jit.ready`).
- `web/node/jit-compiler.mjs` is served with the app (`tools/pages/build.sh`
  copies `web/node`).
