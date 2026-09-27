# Spec — vetro-cpu

## Scope
AArch64, ARMv8.0-A with the Cortex-A53 extensions (ADR 0005). Two
modes (ADR 0009):
- **user** (M1/M2): always EL0, exceptions returned to the caller (the
  Linux layer of `vetro-cli`), `Memory` memory without an MMU;
- **system** (M3): EL0 and EL1 (no EL2/EL3 and no AArch32), stage 1 MMU
  through `vetro-mmu`, exceptions delivered to the guest through VBAR_EL1,
  interrupts and platform registers through `CpuEnv`.

## Public interface
- `Cpu`: architectural state. `x[0..31]`, `sp` (stack pointer in use:
  SP_EL0 in user mode), `pc`, `nzcv` (bits 31:28), `tpidr_el0`,
  `tpidrro_el0`, local exclusive monitor, `v`, `fpcr`, `fpsr`, `sys`
  (`SysState`: mode, configuration, PSTATE.{EL,SP,DAIF,IL}, the SP not in
  use, EL1 registers, pending SError).
- `decode(u32) -> Insn`: pure function, independent of the exception
  level. `Insn` is the decoded form that the JIT (M4) will also use.
- `Cpu::step(&mut self, &mut impl Memory) -> Result<(), Exception>`
  (user mode): executes one instruction. On an exception the state does
  not change and `pc` points to the faulting instruction, except for `Svc`,
  where `pc` already points to the next instruction (like ELR for SVC).
- `trait Memory`: `read`, `write`, `fetch` on virtual addresses, with error
  `MemFault`; default methods `read_unpriv`/`write_unpriv` (LDTR/STTR) and
  `zero_block` (DC ZVA) which in user mode are the same as `read`/`write`.
  Implemented by `UserMemory` (regions with permissions).

### System mode
- `Cpu::reset_system(SysConfig)`: Cortex-A53 reset state without
  EL2/EL3, like `qemu-system-aarch64 -M virt`: EL1h, DAIF = 1111, MMU
  off, SCTLR_EL1 = 0x00c50838, CPACR_EL1 = 0 (FP/SIMD trapped), OS lock
  set. PC and general-purpose registers are set by the loader.
- `SysConfig { psci: PsciConduit (Hvc | Smc | None), mpidr (default
  0x8000_0000), gicv3 (default true) }`.
- `Cpu::step_system(&mut impl SysBus, &mut impl CpuEnv) -> SysEvent`:
  - `Executed`: one instruction executed;
  - `Exception { kind: Sync | Irq | Fiq | SError, esr, from_el }`: exception
    taken, PC at the vector, no instruction executed (informational, for
    traces and statistics);
  - `WaitForInterrupt`: WFI executed, PC after the WFI;
  - `Hvc(imm)` / `Smc(imm)`: call to the configured PSCI conduit, PC after
    the instruction, arguments in x0-x7, result to be written to x0 (for an
    unknown function QEMU answers NOT_SUPPORTED = -1);
  - `Unimplemented { raw, what }`: a Vetro limitation, state unchanged.
- `Cpu::take_exception(kind, esr, far, preferred)`: entry into an
  exception at EL1, also for the platform (e.g. delivering an
  Undefined after an HVC it does not want to handle).
- `Cpu::sp_el(n)`, `set_sp_el(n, v)`, `pstate_spsr()`.
- `trait SysBus` (implemented by `vetro_mmu::MmuBus`):
  `translate(&TranslationRegs, va, AccessReq { access, el, aligned })`
  → PA or `BusFault::{Abort { fsc, ea }, Unimplemented}`; `read_phys`,
  `write_phys` (one piece within a page); `at(...) -> AtResult`;
  `tlbi(TlbiOp, xt)`; `tlb_flush_all()`.
- `trait CpuEnv` (implemented by the platform): `irq_line()`, `fiq_line()`
  (default false), `read_sysreg(EnvReg)`, `write_sysreg(EnvReg, v)`.
  `EnvReg`: CNTFRQ/CNTPCT/CNTVCT, CNTP_{CTL,CVAL,TVAL}, CNTV_{CTL,CVAL,TVAL},
  ICC_{PMR,IAR0/1,EOIR0/1,HPPIR0/1,BPR0/1,AP0R0,AP1R0,DIR,RPR,SGI0R,SGI1R,
  ASGI1R,CTLR,SRE,IGRPEN0/1}_EL1. The CPU checks the access (level,
  CNTKCTL_EL1, read-only/write-only); the platform provides the semantics and
  the time, so the counter stays deterministic.
- `sysreg::SysReg::lookup(op0, op1, crn, crm, op2)`: catalogue of the
  modelled registers.

## Exceptions in user mode
| `Exception` | Linux signal in user mode |
|---|---|
| `Svc(imm)` | syscall |
| `Breakpoint(imm)` | SIGTRAP |
| `Undefined(raw)` | SIGILL |
| `Unimplemented { raw, what }` | SIGILL, but it is our limitation: it must be reported |
| `DataAbort { addr, write }`, `InstructionAbort { addr }` | SIGSEGV |
| `Alignment { addr }`, `PcAlignment { addr }` | SIGBUS |
| `SpAlignment` | never in user mode |

In user mode the system instructions stay as in M1: ERET, HVC,
SMC, MSR immediate, EL1 SYS and MSR TPIDRRO_EL0 → `Undefined`; WFI and WFE →
NOP; MRS/MSR of the EL0 debug channel (MDCCSR_EL0, DBGDTR*_EL0) →
`Undefined`, like QEMU user which (like Linux) sets MDSCR_EL1.TDCC; MRS/MSR
of registers other than NZCV, TPIDR_EL0, TPIDRRO_EL0, FPCR, FPSR, DCZID_EL0,
CTR_EL0 → `Unimplemented`. LDTR/STTR access like LDR/STR.

## System mode: behaviour
- **Order of a step**: unmasked FIQ, IRQ, SError (in this order,
  ELR = PC); misaligned PC (EC 0x22, FAR = PC); translated fetch
  (Instruction Abort, EC 0x20/0x21); PSTATE.IL (EC 0x0E); FP/SIMD trap from
  CPACR_EL1.FPEN (EC 0x07, ISS 0x1e00000); instruction.
- **Entry** (`AArch64.TakeException`): SPSR_EL1 = PSTATE (NZCV, IL,
  DAIF, M), ELR_EL1, ESR_EL1 for synchronous exceptions and SError, FAR_EL1
  for aborts and misaligned PC (otherwise unchanged, like QEMU), EL1h,
  DAIF = 1111, IL = 0. Vector = VBAR_EL1 + {0x000 EL1t, 0x200 EL1h, 0x400
  from EL0} + {0x000 sync, 0x080 IRQ, 0x100 FIQ, 0x180 SError}.
- **ERET**: SPSR with M ∈ {EL0t, EL1t, EL1h} and AArch64; otherwise illegal
  return (EL/SP unchanged, IL = 1, PC = ELR). NZCV and DAIF always from SPSR.
  The exclusive monitor is cleared; the TBI of the new level applies to ELR.
- **Branches**: the target goes through `AArch64.BranchAddr` (TBI from TCR_EL1).
- **ESR** (verified with QEMU): Unknown 0x02000000; SVC/HVC/SMC/BRK with
  imm16; WFI from EL0 with nTWI = 0: 0x07e00000; MSR/MRS/SYS trap (EC 0x18)
  with ISS from the encoding; aborts with EA (1 = slave error), CM (AT), WnR,
  FSC; alignment FSC 0x21; misaligned SP EC 0x26.
- **Accesses from EL0**: CTR_EL0 (UCT), DC ZVA (DZE; DCZID.DZP reflects it),
  DAIF and MSR DAIFSet/Clr (UMA), DC CVAU/CVAC/CIVAC and IC IVAU (UCI) → trap
  EC 0x18; CNT* according to CNTKCTL_EL1 (EL0PCTEN, EL0VCTEN, EL0PTEN,
  EL0VTEN; CNTFRQ readable with either of the first two) → trap; TPIDRRO_EL0
  and PMUSERENR_EL0 read-only; everything else of EL1 → UNDEFINED.
- **At EL1**: writing a read-only register or reading a write-only one
  → UNDEFINED; MRS/MSR SP_EL0 with SPSel = 0 → UNDEFINED; HVC/SMC
  outside the PSCI conduit → UNDEFINED (no EL2/EL3).
- **Registers** (QEMU `-cpu cortex-a53` values and masks, see
  `sys/id.rs`): MIDR 0x410fd034, REVIDR 0x100, MPIDR from `SysConfig`, CTR
  0x84448004, CLIDR 0x0a200023, CCSIDR for CSSELR 0/1/2, ID space
  (CRm 1..7) with QEMU's values and zero for reserved encodings.
  SCTLR_EL1 (MTE bits cleared; a write flushes the TLB), TCR_EL1 (flushes
  the TLB), TTBR0/1, MAIR, CONTEXTIDR, CPACR, TPIDR_EL1, PAR, CNTKCTL, ESR,
  FAR, ELR, SPSR: all 64 bits. VBAR_EL1: only bits [4:0] cleared.
  CSSELR: 4 bits. ACTLR, AMAIR, AFSR0/1, MDCCINT: RAZ/WI. A53
  IMPLEMENTATION DEFINED (L2CTLR, L2ECTLR, L2ACTLR, CPUACTLR, CPUECTLR,
  CPUMERRSR, L2MERRSR): RAZ/WI; CBAR_EL1 = `SysConfig::cbar` (0x0800_0000,
  the virt GICD), read-only. MRS/MSR encodings the A53 does not have
  (later extensions such as FPMR, ZCR, SMCR; free encodings) →
  UNDEFINED, like QEMU; the PMU, which the A53 has, → `Unimplemented` until
  Vetro models it. Debug (op0 = 2, like QEMU's `debug_cp_reginfo`,
  verified by the probe): MDSCR, OSLAR/OSLSR (OSLK at reset), OSDLR
  (1 bit), DBGBVR/BCR 0..5, DBGWVR 0..3 (bits [1:0] zero), DBGWCR 0..3,
  MDRAR = 0: storage only, no debug exceptions. OSDTRRX_EL1,
  OSDTRTX_EL1, OSECCR_EL1, MDCCINT_EL1: RAZ/WI. EL0 debug channel:
  MDCCSR_EL0 (read-only, 0), DBGDTR_EL0 and DBGDTRRX/TX_EL0 (RAZ/WI);
  accessible from EL0 with MDSCR_EL1.TDCC = 0, trapped (EC 0x18) with
  TDCC = 1; at EL1 TDCC does not matter. DBGCLAIMSET_EL1 reads 0xff and sets
  the written bits [7:0]; DBGCLAIMCLR_EL1 reads the CLAIM bits and clears the
  written ones. DBGPRCR_EL1, DBGAUTHSTATUS_EL1, DBGVCR32_EL2, breakpoints and
  watchpoints beyond the A53's count and the rest of op0 = 2: UNDEFINED,
  like QEMU.
- **Memory**: accesses go through `SysBus` with the privilege of the current
  level (0 for LDTR/STTR). Page-crossing accesses: all pages are translated
  first. SCTLR.A → alignment fault before translation; misaligned data
  access to Device memory (also with the MMU off, where data is Device) and
  DC ZVA on Device → alignment fault after the walk and before the
  permissions. SCTLR.EE/E0E = 1 → `Unimplemented`.
- **AT S1E{0,1}{R,W}**: walk without the TLB, result in PAR_EL1; an external
  abort on the walk is taken as a Data Abort (CM = 1, WnR = 1), like QEMU.
- **TLBI**: the 12 EL1 operations (IS too) go to `SysBus::tlbi`.
- **Cache**: IC IALLU/IALLUIS, IC IVAU, DC IVAC/CVAC/CVAU/CIVAC and
  set/way do nothing (like QEMU); DC ZVA zeroes.
- **WFE**: always NOP, never trapped (like QEMU, which does not wait).

## Known differences from QEMU (choice: the architecture)
- Misaligned access to Device memory with the MMU on, and DC ZVA on Device:
  Vetro raises the alignment fault, QEMU 10.0 does not (with the MMU off
  both do).
- SCTLR_EL1.SA/SA0: Vetro checks SP alignment (EC 0x26), QEMU
  does not. Linux sets SA0: a user program with a misaligned SP gets
  SIGBUS as on hardware.
- Trap of MSR DAIFSet/DAIFClr from EL0: ISS with Op1 = 3, Op2 = 6/7; QEMU 10
  swaps them.
- ID_AA64PFR0_EL1: EL0/EL1 = 1 (AArch64 only); QEMU also declares AArch32.
- Misaligned STXR: Vetro always raises the alignment fault; QEMU only if
  the monitor matches.

## Known limitations of system mode
- No debug exceptions (hardware breakpoints and watchpoints, MDSCR.SS
  single step, PSTATE.SS/D), no PMU (only PMUSERENR_EL0), no trace.
- DC CVAC/CVAU/CIVAC/IVAC and IC IVAU do not translate the address (like
  QEMU): no abort on an unmapped address.
- TBI applies to register branches and to ERET only in system mode;
  in user mode the PC keeps the tag (as in M1).
- A single CPU; IS TLBIs will have to be replicated by the platform if there
  are ever more. Big-endian not supported.

## Invariants
- No dependency on `std::fs`, `std::process`, threads: it compiles for
  `wasm32-unknown-unknown`.
- CONSTRAINED UNPREDICTABLE behaviours: QEMU's choice is replicated
  when it is cheap, and the random generators exclude them.
- Exclusives: the monitor records address, size and value read; STXR
  succeeds if address and size match and memory still contains
  that value (same model as QEMU user mode). ERET clears the monitor.
- User mode neither reads nor writes `sys` (test
  `sys::tests::modalita_utente_invariata`).

## Tests
- `tests/isa`: per-instruction cases with expected values, also verified
  against QEMU when the oracle is present; `tests/system_probe.rs` runs
  the bare-metal probe `system/probe.S` and compares the output with that of
  `qemu-system-aarch64` recorded in `system/probe.expected`
  (regenerable with `system/build.sh`, requires Docker).
- `tests/diff`: random programs compared with QEMU (ADR 0006).
- `cargo test -p vetro-cpu`: `sys::tests` (exceptions, ESR, traps from EL0,
  FP, aborts, alignment, IRQ/FIQ/SError, AT/TLBI, registers) on a test
  bus.
- `cargo test -p vetro-mmu --test system`: bare-metal program with real
  tables (VBAR, MMU, AT, LDTR, SVC from EL0), TLB and TLBI, Instruction Abort,
  Device.

## Snapshot (M6, ADR 0015)

`Cpu`, `SysState` and `SysConfig` implement `vetro_snapshot::Snapshot`
(`src/snapshot.rs`): all fields, exclusive monitor and pending SError
included. The CPU has no hidden state.
