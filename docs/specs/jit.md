# JIT to WASM: ABI and interfaces (ADR 0012, ADR 0013, ADR 0024, ADR 0026, ADR 0036)

## Regions
The unit of translation is the **region** (ADR 0024): the basic blocks of a
4 KiB page reachable from an entry `pc` through direct branches (taken and
not taken, backwards too), at most `MAX_REGION` = 64 instructions. A
basic block ends with a branch (conditional too), an SVC, before
an untranslated instruction, the start of another block, the end of the
page, or after `MAX_BLOCK` = 64 instructions. Branches between blocks of the
region stay inside the function; every other branch (indirect, off-page, out
of the region) is an exit with `NEXT`.

Every basic block with at least one executed instruction is an **entry** of
the region (index < 64): the caller of the function writes the index into
`JitState::entry`. A `pc` that is a basic block of an already compiled region
uses that region instead of translating another one.

## Generated modules
A module contains one or more regions. It imports `env.mem` (the linear memory
with `JitState`) and the **runtime** functions `rt.<name>` (table below):
all the fixed ones, in the same order (indices 0..43), then only the fast
paths `rt.fp<k>` that its regions use, in order of first use.
It exports `b<N>: (state: i32) -> i32` for each region `N`. The result:

| Code | Meaning |
|---|---|
| 0 `NEXT` | region finished, `pc` is the next instruction |
| 1 `FAULT` | an access failed (or, in system mode, the interpreter must do it: MMIO, unaligned SP, unaligned exclusive, Q straddling a page...): `pc` and `steps` are those of the instruction, the registers as after the preceding instructions; the host keeps the details |
| 2 `STOP` | stop after the current instruction (write to watched code): `pc` is the next one |
| 3 `SVC` | the region ends with SVC: `pc` points to the instruction (the host executes it with the interpreter). BRK and HVC close the block *before* themselves with `NEXT` |
| 4 `YIELD` | (system mode) MSR DAIF/DAIFClr unmasked interrupts: `pc` is the next instruction, the host rechecks interrupts before continuing |

Every basic block, before starting, checks that `steps + block steps
<= limit`, otherwise it exits with `NEXT` at its start: the instruction count
stays exact even in loops inside the region.

### The runtime (`translate::runtime`)
A module compiled once per engine (`Engine::runtime`): it imports
`env.mem`, `env.ld`, `env.st`, `env.vsync`, `env.simd` and exports:

| Function | Type | Meaning |
|---|---|---|
| `save` | `(state, pc0: i64, steps: i64, packed: i32)` | `pc = pc0 + (packed & 0xfff)`, `steps = steps + (packed >> 12)` in `JitState` |
| `ld_slow`, `st_slow` | `(state, va, size, [value,] pc0, steps, packed) -> (value, fault)` / `-> outcome` | `save`, then `env.ld`/`env.st` |
| `nzcv` | `(k, a, b, r, old) -> i32` | NZCV from the lazy flags (`state::lazy_nzcv`) |
| `ld<el>_<n>`, `st<el>_<n>` | as above, without `size` | software TLB of the EL for aligned accesses, then the one for unaligned accesses, then the host |
| `ldp<el>_<n>`, `stp<el>_<n>`, `ldp_slow`, `stp_slow` | pairs at `va` and `va + n` | the second access is not done if the first fails; a pair load returns nothing if either fails |
| `ldq<el>`, `stq<el>`, `ldq_slow`, `stq_slow` | Q accesses (16 bytes) as two of 8 | straddling a page: `FAULT` without writing anything; aligned to 8 but not to 16 (system mode): halves with `SIZE_PART_OF_MISALIGNED` |
| `ldu<el>`, `stu<el>` | 8-byte half of a Q not aligned to 16 | unaligned TLB, then the host |
| `finish` | `(state, code, pc, steps) -> code` | end of a region with a code other than `NEXT` |
| `vsync` | `(state)` | if `v_valid` = 0, `env.vsync(state)` |
| `simd` | `(state, word: i32, x: i64, nzcv: i32) -> i64` | `env.simd` (ADR 0026) |
| `fp<k>` | `(state, word)` (`-> i32` NZCV for FCMP, `-> i64` for FCVT to an integer; SCVTF/UCVTF: `(state, word, x: i64)`) | FP fast path of the instruction `word` (table in `translate::fp`): writes the result if it is certainly equal to the interpreter's (FPCR = 0, no NaN, no denormals or overflows, IXC already 1 or exact result), otherwise `env.simd` |

The slow paths save the instruction's `pc` and `steps` before calling
the host (the spec wants them saved during `ld`/`st`): for `FAULT` the region
exits without rewriting them.

### Host imports
| Import | Type | Meaning |
|---|---|---|
| `env.ld` | `(state: i32, va: i64, size: i32) -> i64` | read of 1/2/4/8 bytes, zero-extended; on a fault it writes 1 to `exit_detail` and returns 0 |
| `env.st` | `(state: i32, va: i64, size: i32, value: i64) -> i32` | write; 0, or 1 if the region must stop: `exit_detail` = 1 for a fault, 2 for a write to a page with blocks (STOP). `size` = 64 (system mode) is DC ZVA: zeroes the 64 bytes aligned to `va` |
| `env.vsync` | `(state: i32)` | copies V0..V31 of the `Cpu` into `JitState::v` and sets `v_valid` = 1 |
| `env.simd` | `(state: i32, word: i32, x: i64, nzcv: i32) -> i64` | executes with the interpreter the memory-less SIMD/FP instruction `word` on the V, FPCR, FPSR of `JitState` (`v_valid` = 1); `x` is the general register read, `nzcv` the flags (FCCMP, FCSEL); returns the general register written or NZCV (bits 31:28), otherwise 0 (`vetro_jit::helper`) |

`size` with the `SIZE_PART_OF_MISALIGNED` bit (0x80): half of a 16-byte
access not aligned to 16; the host treats it as unaligned (SCTLR_EL1.A,
Device memory), as the interpreter treats the whole access. `size` with the
`SIZE_UNPRIV` bit (0x100, system mode, from `rt.ld_slow`/`rt.st_slow`):
LDTR/STTR at EL1, checked with the permissions of EL0 and never entered in
the software TLB.

### The dispatcher (system mode)
A separate module imports `env.mem`, `env.tbl` (a `funcref` table of
`TABLE_SIZE` = 2¹⁸ entries) and `env.resolve: (state: i32) -> i32`, and exports
`b0: (state: i32) -> i32`. In a loop: it looks up `pc` in the jump cache
(`area::JC`, index `(pc >> 2) & 8191`); if the entry belongs to another `pc` or
another `ctx` it calls `env.resolve` (1 = the host wrote the entry, 0 = return
with `NEXT`); if `steps + maximum steps of the entry > limit` it returns with
`NEXT`; otherwise it writes the entry into `JitState::entry` and calls the region
(`call_indirect` on the table entry) and continues as long as the region
returns `NEXT`. It returns the exit code of the last region.

Regions do not import the table: the engine puts them there
(`Engine::place`). V8 gives every instance that imports a table its own
dispatch table as large as that table.

## `JitState`
A `#[repr(C)]` struct in `vetro_jit::state`, at a 16-byte aligned address
chosen by the host (`state` is the absolute address in the `env.mem` memory):

| Offset | Field | Type |
|---|---|---|
| 0 | `x[0..31]` | 31 × u64 |
| 248 | `sp` | u64 |
| 256 | `pc` | u64 |
| 264 | `steps` | u64: instructions executed, updated as in the interpreter |
| 272 | `nzcv` | u32, bits 31:28 like `Cpu::nzcv` (valid if `fk` = 0) |
| 276 | `exit_detail` | u32: 0 on entry (the host clears it), 1 fault, 2 STOP |
| 280 | `el` | u32, exception level (0 in user mode) |
| 284 | `ctx` | u32: jump cache context (system mode) |
| 288 | `limit` | u64: maximum steps of the run |
| 296 | `tpidr_el0` | u64 |
| 304 | `tpidrro_el0` | u64 |
| 312 | `tpidr_el1` | u64 |
| 320 | `sp_el0` | u64: SP_EL0 when it is not the SP in use (EL1, SPSel = 1) |
| 328 | `tcr` | u64: TCR_EL1 (read-only) |
| 336 | `dczid` | u64: DCZID_EL0 for the current EL (read-only) |
| 344 | `mon_addr` | u64: exclusive monitor, address |
| 352 | `mon_lo`, `mon_hi` | 2 × u64: value read (128 bits) |
| 368 | `mon_valid` | u32: 1 if the monitor is active |
| 372 | `mon_bytes` | u32: bytes of the exclusive access |
| 376 | `entry` | u32: entry basic block of the called region |
| 380 | `daif` | u32: PSTATE.DAIF (bits 9:6) |
| 384 | `elr_el1`, `spsr_el1` | 2 × u64 (MRS/MSR at EL1) |
| 400 | `esr_el1`, `far_el1` | 2 × u64 (MRS only, at EL1) |
| 416 | `v_valid` | u32: 1 if `v` holds the `Cpu` registers |
| 420 | `fk` | u32: kind of lazy flags (0 = NZCV in `nzcv`) |
| 424 | `fpcr` | u32: FPCR (regions read it) |
| 428 | `fpsr` | u32: FPSR (cumulative flags: regions and `env.simd` write them) |
| 432 | `v[0..32]` | 32 × 16 bytes: V0..V31 (low half, then high) |
| 944 | `fa`, `fb`, `fr` | 3 × u64: operands and result of the lazy flags |
| 968 | `time_base` | u64: (system mode) machine instructions at the start of the run: the CNTPCT of an instruction is `counter(time_base + steps + index)` |
| 976 | `cntvoff` | u64: CNTVCT = CNTPCT - `cntvoff` |
| 984 | `time_ok` | u32: 1 if `time_base` and `cntvoff` are valid for the run (otherwise MRS of the counter exits) |
| 988 | — | padding |
| 992 | `ttbr0`, `ttbr1`, `contextidr` | 3 × u64: (system mode) TTBR0_EL1, TTBR1_EL1, CONTEXTIDR_EL1 (MRS/MSR at EL1; an MSR of a TTBR exits with `YIELD`) |
| 1016 | — | padding up to 1024 |

The fields from 284 on (except `ctx`, `limit`, the monitor, `entry`,
`v_valid`, `fk`, `fpcr`, `fpsr`, `v`, `fa`, `fb`, `fr`, also used in
user mode) serve system mode. Before a run the host
copies the `Cpu` fields into `JitState` (`from_cpu`, `from_cpu_sys`:
`v_valid` = 0 and `fk` = 0, the V registers are not copied), and afterwards copies them
back (`to_cpu`, `to_cpu_sys`: V only if `v_valid`, NZCV computed from the
lazy flags with `state::lazy_nzcv`, FPSR and the monitor). For chained regions
the copy in `JitState` stays valid.

**Lazy flags.** An instruction that writes NZCV (ADDS/SUBS/CMP/CMN, ANDS/TST)
leaves the kind in `fk` (1 add 64, 2 subtract 64, 3 add 32, 4
subtract 32, 5 logical 64, 6 logical 32), the operands in `fa`, `fb` and the
result in `fr` (truncated to 32 bits for the 32-bit kinds). Conditional branches
and CSEL/CCMP with flags of a kind known within the basic block compute the condition
from the operands; otherwise `rt.nzcv`. Regions pass the lazy flags on to the
next one as they are.

### System mode area (`vetro_jit::state::area`)
Offsets from the start of `JitState`:

| Offset | Contents |
|---|---|
| 1024 | jump cache: 8192 entries of 16 bytes `{pc: u64, ctx: u32, w: u32}`, `w = entry << 26 \| slot << 8 \| maximum steps of the entry` |
| 132096 | software TLB for aligned accesses: 4 tables (EL0 read, EL0 write, EL1 read, EL1 write) of 512 entries of 16 bytes `{tag: u64, addend: u64}`, index `(va >> 12) & 511` |
| 164864 | TLB for unaligned accesses: 4 tables as above (`area::tlb_u`) |

An aligned access of `n` bytes at `va` uses the entry if `tag == va & (!0xfff
| (n - 1))`; an unaligned one uses the unaligned TLB if `tag == va &
!0xfff` and it does not spill into the next page. The address in the engine's
memory is `(va + addend) mod 2³²`. `tag = 0x800` is an empty entry. An
entry of the unaligned TLB exists only after a successful unaligned access
(Normal memory, SCTLR_EL1.A = 0). Total area: 197632 bytes.

`ctx` = context number << 7 | region parameters (EL, TBI0, TBI1, SPSel,
FP and, at EL0, CNTKCTL_EL1.EL0PCTEN/EL0VCTEN): a jump cache entry is valid
only for the same parameters. The context number (ADR 0036) is one per
(EL, TTBR0 base, TTBR1 base) since the last new epoch (SCTLR/TCR/MAIR
changed, a TLBI other than by VA, a code invalidation); it stays the same
when the regime comes back to the same table bases, so entries survive the
TTBR0 switches of every kernel entry and exit. A TLBI by VA removes only the
jump cache and software TLB entries within the 1 GiB around the address
(`Tlb::invalidations_since`). The software TLB entries are grouped per (EL,
half of the address space) with the table base they were filled under; a
group is emptied when its base changes. In user mode (`JitCpu`, ADR 0026) the
same jump cache and the same dispatcher, with `ctx` = the context of the
address space, new at every invalidation of its pages.

## Traits
```rust
pub trait Engine {
    type Module;
    /// Installs the runtime module: its exports become the `rt.*`
    /// imports of the modules compiled afterwards (also after `reset`).
    fn runtime(&mut self, wasm: &[u8]) -> Result<(), String>;
    /// Compiles a WASM module generated by the translator.
    fn compile(&mut self, wasm: &[u8]) -> Result<Self::Module, String>;
    /// True once the module can run (ADR 0038). An engine may compile in the
    /// background; until then the module's regions run in the interpreter.
    /// Readiness may change only between two `SysJit::run`. Default: true.
    fn ready(&mut self, m: &Self::Module) -> bool { true }
    /// Runs region `index` of the module on the state at address
    /// `state` of the shared memory; `ld`/`st`/`resolve`/`vsync`
    /// call `host`.
    fn run(&mut self, m: &Self::Module, index: u32, state: u32, host: &mut dyn Host) -> u32;
    /// The shared memory (where `JitState` lives).
    fn memory(&mut self) -> &mut [u8];
    /// Puts `b0..b<count-1>` of the module into entries `base..` of `env.tbl`.
    fn place(&mut self, m: &Self::Module, count: u32, base: u32);
    /// Frees all modules (and the table; the runtime stays).
    fn reset(&mut self) {}
    /// At least `bytes` bytes in `memory()`. Default: checks.
    fn reserve(&mut self, bytes: usize);
    /// Address in `env.mem` of `len` host bytes from `p`, if the blocks can
    /// reach them (browser: always; wasmtime: if they are inside its memory).
    fn host_address(&mut self, p: *const u8, len: usize) -> Option<u32> { None }
}

pub trait Host {
    fn ld(&mut self, mem: &mut [u8], va: u64, size: u32) -> Result<u64, ()>;
    /// Ok(true) = stop after this instruction.
    fn st(&mut self, mem: &mut [u8], va: u64, size: u32, value: u64) -> Result<bool, ()>;
    /// `env.resolve`: true if the host wrote the jump cache entry
    /// for the `pc` of `JitState`. Default false.
    fn resolve(&mut self, mem: &mut [u8]) -> bool { false }
    /// `env.vsync`: V0..V31 of the `Cpu` into the `JitState` at `state`.
    fn vsync(&mut self, mem: &mut [u8], state: u32);
}
```
`mem` is the engine's memory (`Engine::memory`): the host writes the software
TLB, the jump cache and the V registers into it.

### System mode (`vetro_jit::sys`)
```rust
pub trait SysPhys: PhysMemory {
    fn ram_read(&mut self, pa: u64, buf: &mut [u8]) -> bool;          // RAM only, no side effects
    fn ram_write(&mut self, pa: u64, data: &[u8]) -> Option<bool>;    // Some(true): watched page touched
    fn watch_code(&mut self, page: u64) -> bool;
    fn is_watched(&self, page: u64) -> bool;
    fn take_code_dirty(&mut self, out: &mut Vec<u64>);
    fn ram_region(&mut self) -> Option<(u64, *mut u8, usize)> { None }
}

impl<E: Engine> SysJit<E> {
    pub fn new(engine: E, cfg: SysJitConfig) -> Self;
    /// Translated regions only, at most `budget` steps.
    pub fn run(&mut self, cpu: &mut Cpu, mmu: &mut Mmu, phys: &mut dyn SysPhys, budget: u64) -> SysRun;
    /// The clock of the next `run` (MRS CNTPCT/CNTVCT in the regions).
    pub fn set_time(&mut self, c: Clock);
}
pub struct Clock { pub steps: u64, pub cntvoff: u64 }
pub struct SysRun { pub steps: u64, pub next: Next }
pub enum Next { Jit, One, Cold }   // then: JIT, one interpreter step, interpreter up to the next branch
```
The caller (`Machine::run`) does not call `run` when the interpreter
would take an interrupt, with PSTATE.IL or with an unaligned PC, and does not
grant more steps than those up to the next platform event. After
`YIELD`, `run` returns with `Next::Jit`: the caller rechecks interrupts.
`SysJitDyn` is the same as a trait object (for `Machine::set_jit`), with
`set_time` and the per-class profile (`profiling`, `profile_step`,
`profile`). Default threshold: 64 entries before translating. The machine
calls `set_time` before every `run`.

### User mode (`vetro_jit::JitCpu`)
Like system mode since ADR 0026: regions in the engine's table
(`Engine::place`, one module per region), the dispatcher and the jump cache
in the area after `JitState` (`Engine::reserve`). `Host::resolve` writes the
missing entries for the already compiled regions of the address space.

## Coverage
Translated:
- arithmetic and logic (immediate, register, extended, with carry, with flags),
  MOVZ/MOVN/MOVK, ADR/ADRP, bitfield, EXTR, CCMP/CCMN, CSEL and variants,
  RBIT/REV/CLZ/CLS, divisions and variable shifts, multiplications, CRC32;
- MRS/MSR NZCV; all branches;
- integer LDR/STR (immediate, pre/post, register, literal), LDP/STP/LDPSW,
  LDAR/STLR;
- SIMD (ADR 0024): LDR/STR of B/H/S/D/Q registers (immediate, pre/post,
  register), LDP/STP of S/D/Q, DUP (element and general), INS, UMOV/SMOV,
  immediate MOVI/MVNI/ORR/BIC (in system mode only with CPACR_EL1.FPEN
  allowing them at the EL);
- SIMD/FP (ADR 0026): every memory-less instruction (integer, FP,
  cryptographic): inline the exact forms of integer SIMD (including the
  saturating ones with QC) and FMOV/FABS/FNEG/FCSEL, common FP arithmetic
  with the `rt.fp<k>` fast paths, the others with `env.simd`; LD1/ST1 of 1-4
  registers, LD1R, single-lane LD1/ST1, LD2..LD4/ST2..ST4;
- LDXR/STXR and variants in user mode too (ADR 0026);
- system mode only: DC ZVA, MRS of CNTPCT_EL0/CNTVCT_EL0 (at EL0 if
  CNTKCTL_EL1 allows them; without a clock they exit),
  MRS/MSR of TPIDR_EL0, TPIDRRO_EL0 (MSR only at EL1), TPIDR_EL1 and SP_EL0
  (at EL1; SP_EL0 with SPSel = 1), MRS of TCR_EL1, DCZID_EL0, CurrentEL (at
  EL1); at EL1 also MRS/MSR of DAIF, ELR_EL1, SPSR_EL1, MRS of ESR_EL1 and
  FAR_EL1, MSR DAIFSet/DAIFClr.

- system mode (ADR 0036): LDTR/STTR (at EL1 through the host with the
  permissions of EL0), MRS FPCR/FPSR and MSR FPSR (FP enabled), at EL1 MRS
  TTBR0/TTBR1/CONTEXTIDR/MIDR, MSR CONTEXTIDR, MSR TTBR0/TTBR1 (exit with
  `YIELD` after the instruction).

Left to the interpreter: literal LDR of V registers, interleaved single
structures (single-lane LD2..LD4, LD2R..LD4R), the other system
registers (MSR FPCR included), SVC/BRK/HVC, ERET, and in system mode also
WFI and cache maintenance at EL0. Coverage grows only with parity tests.

## Implementation notes
- **Imported memory.** `env.mem` is declared according to the configuration
  (`MemoryImport`): in the browser with threads a shared memory
  (`shared`) is needed.
- **Code watching in user mode.** It lives in `UserMemory`:
  `space_id` distinguishes address spaces, `watch_code(page)` marks
  translated pages, `take_code_dirty()` returns those written by stores,
  `poke`, mmap, munmap, mprotect, mremap and stack growth.
  Details in `crates/vetro-jit/src/driver.rs`. Mappings are whole
  pages (like Linux): the two halves of a Q access in the same page have
  the same permissions.
- **Watching in system mode.** It lives in `vetro_machine::Ram` (bitmap
  per physical page): every physical write goes through there.
- **Common tail.** All exits of a region jump to a tail that
  writes back the registers written by the region (loaded on entry even if
  not read), the lazy flags, `pc`, `steps` and the code.
- **In the browser** the dispatcher is called from Rust as a function pointer
  in vetro-wasm's table (`docs/specs/wasm.md`).
