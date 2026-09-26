# ADR 0012 — Block JIT to WebAssembly (M4)

- Status: accepted (M4, 2026-09-25). Also sets the M4 threshold left
  open by M3.

## Context
The interpreter is correct: M1-M3 are green against QEMU. Natively it does about 50
MIPS, and in the browser it will be slower. M4 calls for:
- workable performance;
- the same tests green with the JIT;
- no difference between interpreter and JIT on the differential set.

In the browser the generated code must be WebAssembly, because it is the only
code a page can create and run. The JIT must still run
outside the browser too, in tests and in CI.

## Decision
- **Translation unit: the basic block.** It is a sequence of instructions already
  decoded by `vetro_cpu::decode`, within a 4 KiB page. It ends:
  - after a branch, a synchronous exception (SVC, BRK, HVC...) or a system
    instruction;
  - before the first instruction the translator does not cover (that one is
    run by the interpreter, and the JIT resumes afterwards);
  - after at most 64 instructions.

  Every block becomes an exported WASM function. Several blocks live in one
  module, to amortise compilation.
- **A single ABI for all engines** (`docs/specs/jit.md`):
  - the CPU state lives in a `JitState` structure in a linear memory
    shared with the host;
  - accesses to guest memory go through functions imported
    from the host (`ld`/`st`), which handle MMU, permissions and bus;
  - the block returns an exit code.

  The same module runs in V8 (browser and Node) and in wasmtime (native tests).
- **Engines, behind the `vetro_jit::Engine` trait:**
  - wasmtime for the tests and for `vetro --jit`: crate `vetro-jit-native`,
    the only one with external dependencies, never in the core;
  - JavaScript `WebAssembly` for `vetro-wasm` (browser, Node).
- **Precision.**
  - Before every memory access the block saves in `JitState` the PC
    of the instruction and the number of instructions already executed. A fault in the middle of a
    block therefore leaves the state exactly as the interpreter would, and the
    delivery of the exception is left to the code that already exists.
  - The number of instructions remains the guest clock (ADR 0010 and 0011): the
    same program has the same time trace with and without the JIT.
- **Invalidation.**
  - Blocks are indexed by page: virtual in user mode,
    physical in system mode, together with the EL and the translation options.
  - Every write to a page that contains blocks invalidates them, whether it comes
    from the guest (`st`) or from the emulated kernel. If the write falls in the block
    currently running, the block exits right after.
  - In system mode a block is valid only if the VA→PA translation of its
    start is still the one it was compiled with: this is checked at every
    entry, with the MMU's cache of recent translations.
- **Parity.** The differential set runs with the interpreter and with the JIT, and the
  results must be identical. The set includes:
  - the random programs of `vetro-diff`;
  - the per-instruction tests;
  - BusyBox, quick LTP and RISU;
  - the kernel boot.

  The JIT has no behaviour of its own: what it cannot translate, the
  interpreter runs.

## M4 threshold
Booting the guest kernel with the same script as `tests/boot` (up to
`poweroff -f`) under the JIT in Node (V8) must take at most as long as the
same boot with the native interpreter, measured in the same CI job. Today
the native interpreter takes about 2 s on an M2 Pro.

## Consequences
- `vetro-cpu` exposes what the translator needs (decoding, semantics
  of the covered instructions) without duplicating it. The translator generates WASM for
  groups of instructions, and the groups not covered remain with the interpreter.
- `UserMemory` and the `Board` report writes to watched pages.
- CI adds the tests with the JIT (wasmtime) and the benchmark in Node.
