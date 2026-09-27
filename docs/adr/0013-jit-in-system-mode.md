# ADR 0013 — JIT in system mode: blocks per physical page, chaining, software TLB

- Status: accepted (M4, 2026-09-25). Extends ADR 0012.

## Context
ADR 0012 sets the block JIT to WebAssembly and the M4 threshold:
booting the guest kernel under the JIT in Node (V8) must not take longer
than the same boot with the native interpreter. In user mode the JIT already
exists (`JitCpu`). In system mode we additionally need:
- the MMU: code is found by virtual address, but it is valid for a physical
  page, and the translation changes with the registers, with TLBIs and with the EL;
- interrupts and devices: the clock (instructions executed) and the points
  where interrupts arrive must stay the same as the interpreter's;
- physical writes from everywhere (CPU, device DMA, image
  loading) onto translated code;
- speed: in V8 every transition between JS and WebAssembly has a cost, and every memory
  access that goes back to the host costs even more.

## Decision

### Who does what
- `vetro_jit::sys::SysJit` runs **only translated blocks** and stops
  before anything the interpreter has to do (untranslated instruction,
  fault, access outside RAM, SVC). It does not know the platform: it sees
  physical memory through the `SysPhys` trait.
- `Machine::run` alternates `SysJit::run` and `Cpu::step_system`. It calls the JIT
  only when the interpreter, at the next step, would not take an
  interrupt (IRQ/SError unmasked, PSTATE.IL, misaligned PC), and
  grants it at most the steps up to the next timer deadline (there
  the interpreter updates the interrupt lines before the instruction).
- Inside blocks nothing changes the interrupt lines: blocks do not
  touch MMIO (an access outside RAM is a JIT fault and the interpreter
  redoes it) nor the platform's system registers, and they do not change
  DAIF. So same instructions, same interrupts at the same points, same
  log: this is verified byte for byte (below).

### Blocks
- Key: (`pc`, EL, TCR_EL1.TBI0, TBI1, PSTATE.SP). Each key has one
  variant per physical page. At every entry from the host `pc` is translated for
  a fetch with the permissions of the EL (the same translation as the interpreter, including
  the cache of recent translations) and the variant for that page is used.
- The block ends after an unconditional branch, before SVC or
  an untranslated instruction, at the end of the page or after 64 instructions.
  Conditional branches do not close it: when taken, they exit the block (side exit).
- Beyond user mode, these are also translated: exclusives (the
  monitor lives in `JitState`), DC ZVA (always via the host, with the rules of
  `zero_block`), CRC32, MRS/MSR of TPIDR_EL0, TPIDRRO_EL0, TPIDR_EL1, SP_EL0,
  and MRS of TCR_EL1, DCZID_EL0, CurrentEL, with the permissions of the block's EL.
  WFI, LDTR/STTR, cache maintenance at EL0 and every other system register
  remain with the interpreter. With SP as the base and SP misaligned the
  block exits and the interpreter decides (SCTLR_EL1.SA/SA0).
- A block is translated after 16 entries with the interpreter; hot blocks are
  compiled in groups of 16 in one module, or earlier if the pending blocks
  are requested 16 times (a hot loop does not wait).

### Invalidation
- Every physical page with blocks is watched. `vetro_machine::Ram` keeps
  a bitmap of the watched pages: every write that goes through it (CPU,
  virtio DMA via `GuestRam`, `load_linux`) marks the page dirty, and its
  blocks are discarded before the next JIT run. For this reason the bytes
  of RAM are no longer public.
- A store from a block to a watched page makes the block exit
  right after (`STOP`).

### Chaining
- The blocks of all modules live in a function table (`env.tbl`,
  `TABLE_SIZE` entries) into which the engine places them (`Engine::place`). A generated
  module, the **dispatcher**, goes from one block to the next with
  `call_indirect` without returning to the host, as long as the **jump cache**
  (8192 entries `{pc, ctx, slot|steps}` in shared memory) has an entry for
  the new `pc` with the current context and the block fits in the step limit.
- A missing entry is requested from the host with `env.resolve` (a direct call
  from WebAssembly to WebAssembly, in V8 too): if the block already exists,
  the host verifies the fetch translation and writes the entry.
- The context is the EL plus an epoch that changes with the translation registers
  (SCTLR, TCR, TTBR0/1, MAIR), with every TLBI (`Tlb::flushes`) and with every
  block invalidation: within an epoch the fetch translation of an
  entry cannot change.
- Only the dispatcher imports the table: V8 gives every instance that imports
  a table its own dispatch table as large as that table (with one instance
  per block module, memory ran out in a few seconds).

### Software TLB
- Four tables (EL0/EL1 × read/write) of 512 entries `{virtual
  page, addend}` in shared memory. An aligned load or store that
  finds its page accesses `va + addend` directly in the engine's
  memory; otherwise it calls `ld`/`st`.
- The host fills an entry only after a successful access through the MMU
  (same permissions, same page), only for RAM pages the engine
  can reach (`Engine::host_address`: in the browser the blocks' memory is
  vetro-wasm's, which contains the guest RAM; wasmtime only for the
  RAM that lives in its memory, i.e. in tests) and, for writes, only for
  pages without blocks (and the write tables are flushed when a
  new page acquires blocks). It is flushed with the translation registers and with
  TLBIs, like the MMU's TLB.
- Aligned accesses only: this way Device memory (where misaligned accesses
  fault) does not need a special case.

### Accepted difference
The MMU's TLB sees fewer accesses than with the interpreter (no fetches inside
blocks, no accesses from the fast path). The result can change
only for a guest that modifies the page tables without TLBI, which
the architecture leaves unpredictable and Linux does not do (and which already
distinguishes Vetro from QEMU).

### In the browser
- vetro-wasm exports its function table (`--export-table`,
  `--growable-table`, from `build.rs`): JS places the dispatcher in it and Rust
  calls it as a function pointer, without going through JS on every run.
- `ld`/`st`/`resolve` are vetro-wasm exports passed as imports to the
  modules: V8 calls them directly.

### ABI (docs/specs/jit.md)
- `Host::ld/st` also receive the engine's memory (for the software TLB);
  new `Host::resolve`.
- `Engine`: new `place`, `reserve`, `host_address`.
- `JitState` grows to 384 bytes (context, limit, system registers,
  monitor); after `JitState` comes the system mode area
  (`state::area`). `st` with `size` 64 is DC ZVA.
- A block's exits go to a common tail that writes back all the
  registers the block writes (those not yet written hold their entry
  value): code grows linearly instead of quadratically.

## Verification
- Guest kernel boot (script from `tests/boot`) with the JIT on wasmtime
  (`VETRO_JIT=1`) and on V8 (`tools/wasm-boot.sh --jit`): same instructions and
  same log, byte for byte, as the interpreter.
- The bare-metal probe of `tests/isa/system` with the JIT: same output as QEMU and
  same instructions as the interpreter.
- `vetro-jit-native/tests/sys_parity.rs`: random bare-metal programs with
  different MMU, permissions and attributes, EL0 and EL1, exceptions, exclusives,
  self-modifying code and the software TLB active; exceptions, steps, CPU and
  RAM identical to the interpreter (a bug deliberately introduced in the fast
  path or in the monitor is caught).
- Guest kernel kselftest with the JIT: same log as the interpreter.

## Consequences
- The M4 threshold is measured in the CI `boot` job with `tools/wasm-boot.sh
  --jit`, which fails if the JIT in V8 is slower than the native interpreter.
- `vetro boot --jit` and `VETRO_JIT=1` in `tests/boot` use wasmtime; there
  Cranelift compilation dominates and the JIT is slower than the interpreter:
  it serves parity, not speed.
- Coverage grows only with parity tests, as in user mode.
