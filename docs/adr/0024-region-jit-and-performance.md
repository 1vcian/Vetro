# ADR 0024 — Region JIT: compact code, lazy flags, SIMD, shared runtime

- Status: accepted (M4, maintenance, 2026-09-26). Extends ADR 0012 and
  ADR 0013.

## Context
Android asks for hundreds of sustained MIPS in the browser. Before this
work the guest kernel boot (284.8 M instructions with today's initramfs,
about half of it time skipped in WFI) took 1.91 s with the JIT in V8;
at steady state, in the guest, `sha256sum` ran at ~575 MIPS, `gzip` at ~360
and a shell loop (ash, system and user mixed) at ~109.

The measurements (V8 profile, macOS `sample`, counters; numbers in
`docs/progress/M4.md`) showed where the time goes:
- **compilation**: on V8's main thread, module validation and Liftoff's lazy
  compilation on the first use of each function took 25% of the boot (37%
  with the first, larger, regions). The cost is proportional to the bytes:
  ~1.1 KB of WASM per block, 150-370 bytes per instruction for memory
  instructions (inline slow path). TurboFan, on the background threads, has a
  more than linear cost in function size and in the number of live values
  (register allocation);
- **exits to the interpreter**: in BusyBox (musl) user code ~590
  thousand exits on a few SIMD instructions (LDR/STR/LDP/STP of Q registers,
  DUP, INS, UMOV, MOVI: `memcpy`/`memset`), with region fragmentation and
  cold interpretation after each one; in the kernel ~110 thousand exits on
  MRS/MSR DAIF, MSR DAIFSet/DAIFClr, ELR/SPSR/ESR/FAR;
- **dispatch**: at steady state ~17 instructions per region call; the
  dispatcher loop costs little, what costs is the region's prologue and tail
  (registers, flags) and the calls for every memory access.

## Decision

### Regions instead of blocks
- The unit is the **region**: the basic blocks of a page reachable from the
  entry through direct branches, taken and not taken, also backwards, at
  most 64 instructions. In the function: a `loop` with a `br_table`
  on the index of the next basic block; the next block in memory is
  reached without branches. Each basic block checks before starting that
  its steps fit in the limit, otherwise it exits at its start: the
  instruction count stays exact in loops, so interrupts, time and replay
  stop points (ADR 0019) do not change.
- **Multiple entries.** Every basic block with steps is an entry (6-bit
  index in the branch cache entry, written to `JitState::entry` by the
  dispatcher): a `pc` already inside a compiled region does not cause
  another one to be translated, which would duplicate the code.
- **64 instructions, no call returns.** 256-instruction regions give +10% at
  steady state but +40% of compilation CPU (TurboFan); also following the
  return address after BL/BLR takes the bytes from 31 to 53 MB and the boot
  from 2.0 to 4.1 s. The limit will be reassessed when compilation costs less
  (for example with a second tier).

### Compact code
- **Shared runtime.** The slow paths, pairs, Q and unaligned accesses, NZCV
  computation, region end and SIMD register copying live in a runtime module
  compiled once (`translate::runtime`, `Engine::runtime`): regions import it
  as `rt.*` (in V8 a direct call between instances). Defined in every module
  they cost 1.4 KB and 20 lazy compilations per module.
- The **TLB fast path** of aligned accesses stays inline (+10% at steady
  state compared to the call, at the cost of a few bytes); the rest is a
  call.
- **Short exits**: for FAULT, `pc` and `steps` have already been saved by
  the slow path; `NEXT` and `done = 0` are the initial value of the locals;
  the tail writes `pc`/`steps` without calls for `NEXT` and calls
  `rt.finish` for the other codes (STOP after a store: saved `pc` + 4).
- Direct forms for ADD/SUB without flags, MOV, UBFM/SBFM (LSL, LSR, ASR,
  UBFX, SBFX, UBFIZ, SBFIZ), addresses with offset; SP alignment is checked
  once per basic block as long as SP does not change in a way that could
  lose it.
- Result: 66 bytes per instruction in the sample of typical kernel
  instructions of the `codice_compatto` test (~130 before), 10.1 MB of
  modules for the boot versus 14.9 MB.

### Lazy flags
- ADDS/SUBS/CMP/CMN/ANDS/TST leave kind, operands and result (in locals
  inside the region, in `JitState` between one region and the next: `fk`,
  `fa`, `fb`, `fr`). B.cond, CSEL and CCMP compute the condition from the
  operands if the kind is known in the basic block (`cmp; b.ne` becomes a
  comparison); otherwise `rt.nzcv`. The host derives NZCV with
  `state::lazy_nzcv` (the same formula) when it copies the state back into
  the `Cpu`.

### SIMD, DAIF and exception registers in regions
- LDR/STR of B/H/S/D/Q registers, LDP/STP of S/D/Q, DUP, INS, UMOV/SMOV,
  immediate MOVI/MVNI/ORR/BIC. The V registers live in `JitState` and get
  there only when needed: the first region of the run that uses them calls
  `rt.vsync` → `env.vsync` (the host copies `Cpu::v`, `v_valid` = 1), and
  whoever copies the state back into the `Cpu` brings the registers back if
  `v_valid`. No cost for runs without SIMD.
- A Q access is two 8-byte accesses with the rules of the integer access of
  `SysMem::access`: across a page FAULT (the interpreter translates all the
  pages before writing: no partial writes); aligned to 8 but not to 16, in
  system mode, the halves go to the host marked as unaligned
  (`SIZE_PART_OF_MISALIGNED`: SCTLR_EL1.A, Device memory).
- **Unaligned TLB**: a second software TLB, filled only after a successful
  unaligned access (hence Normal memory and SCTLR_EL1.A at 0), for unaligned
  accesses that stay within the page. Flushed like the other one.
- In system mode SIMD instructions are translated only if CPACR_EL1.FPEN
  allows them at the EL (region parameter `fp`); the region parameters (EL,
  TBI, SPSel, FP) also enter the branch cache context (before only EL: an
  entry could be valid with a different SPSel).
- At EL1: MRS/MSR DAIF, ELR_EL1, SPSR_EL1, MRS ESR_EL1, FAR_EL1, MSR
  DAIFSet/DAIFClr. An MSR that unmasks (a DAIF bit from 1 to 0) exits with
  the new code `YIELD` after the instruction: `SysJit::run` returns to the
  caller, which rechecks interrupts before the next instruction, like the
  interpreter.

### Host
- Cold code (no variant, below the threshold) does not translate the
  `pc` with the MMU: counting it is enough.
- Default threshold 64 entries (16 before): less lukewarm code compiled;
  measured 16/32/64/128, 64 the best for the boot.

### Not done, and why
- **Direct chaining between modules** (tail call): the dispatcher is already
  WebAssembly and does not return to JS; its loop is a small part of the
  per-region cost (prologue, tail and calls count more). `return_call`
  exists in V8 and wasmtime, but the expected gain is small compared to the
  risk of another ABI: it remains a possibility, to be measured.
- **Compilation in a Worker or asynchronous**: in V8 Liftoff's lazy
  compilation happens anyway on first use on the main thread;
  asynchronous `WebAssembly.compile` would only move validation (~8% of the
  main thread) and requires the execution loop to yield to the event loop.
  Determinism is not an obstacle: interpreter and JIT give the same
  architectural state (same instructions, same log, verified by the parity
  tests and by the boot), so until a module is ready the code runs in the
  interpreter and the result does not depend on when compilation finishes;
  only the JIT counters would change. To be done with a second tier (hot
  regions recompiled larger) when worthwhile.
- **User mode**: `JitCpu` still calls the engine for every region
  (no dispatcher) and every access goes through `UserMemory`: the next
  gain there is chaining, not the translator.

## Verification
- `vetro-jit-native/tests/parity.rs` and `sys_parity.rs` (random programs in
  user and system mode, now with SIMD, DAIF, ELR/SPSR, random UMA): a
  deliberately introduced bug is found at each of the new points (lazy flags
  in the host, MOVI, half of a Q, Q aligned to 8 on Device memory,
  unaligned TLB filled by aligned accesses).
- Targeted tests: Q across a page (user mode, memory equal to the
  interpreter), an unaligned access crossing from a page in the unaligned TLB
  into an unmapped one (system mode), YIELD after DAIFClr (and no exit for
  DAIFSet or for a bit already at zero), regions with loops and code size
  (`codice_compatto`).
- Kernel boot with the JIT on wasmtime and on V8 (`tools/wasm-boot.sh --jit`):
  same instructions and same log as the interpreter; replay and snapshot with
  the JIT; tests/linux with `VETRO_JIT=1`; tests/diff and tests/isa with the
  oracle.

## Consequences
- JIT ABI (`docs/specs/jit.md`): `JitState` at 976 bytes (entry, DAIF,
  ELR/SPSR/ESR/FAR, V registers, lazy flags), system mode area of
  1024 with the unaligned TLB, branch cache entry with the entry, `YIELD`
  code, `rt.*` imports, `env.vsync`, `Engine::runtime`, `Host::vsync`.
  vetro-wasm moves to ABI 10 (`vetro_jit.runtime`, `vetro_jit_vsync`,
  `yields`).
- `vetro-cpu` exports `simd::{CopyOp, MovImmOp}` (only the type names).
