# ADR 0009 — CPU system mode: EL1 state in the CPU, MMU and platform behind traits

- Status: accepted (M3, 2026-09-25)

## Context
In M3 the CPU must run the kernel: EL0 and EL1, exceptions with the vectors
of VBAR_EL1, system registers, stage 1 MMU, interrupts from the GIC and the
generic timer. Three constraints:

1. the user mode of M1/M2 (`Cpu::step` with `UserMemory`, exceptions
   returned to the Linux layer) must remain identical and cost-free;
2. `vetro-mmu` already depends on `vetro-cpu` (`Access`, `MemFault`,
   `Memory`): the CPU cannot depend on the MMU;
3. the platform (GIC, timer, PSCI) lives in `vetro-platform`, which the CPU
   must not know about; time must come in from a single recordable point.

## Decision
- **Two modes, one interpreter.** `Cpu` has a field `sys: SysState`
  (PSTATE beyond NZCV, SP per level, EL1 registers). `Default` is user
  mode: `Cpu::step` does not look at it. `Cpu::reset_system(SysConfig)` brings the
  CPU to the Cortex-A53 reset state without EL2/EL3;
  `Cpu::step_system(&mut impl SysBus, &mut impl CpuEnv) -> SysEvent` executes
  one step in system mode reusing the interpreter's same `execute`.
- **A single decoder, independent of the level.** `decode` also recognises
  ERET, HVC, SMC, WFI/WFE, MSR immediate, SYS (TLBI, AT, cache), LDTR/STTR
  (`unpriv`) and all the modelled system registers. In user mode
  these instructions give exactly the same outcome as before (Undefined, NOP or
  `Unimplemented`); the random generator of `tests/diff` does not produce them.
- **Memory: `SysBus` trait defined in `vetro-cpu`, implemented by
  `vetro-mmu` (`MmuBus`).** The CPU is the sole owner of SCTLR, TCR,
  TTBR0/1 and MAIR and passes them to every translation (`TranslationRegs`); the MMU
  translates (with a TLB) and performs the physical accesses. The CPU splits accesses by
  page, checks SCTLR.A and builds ESR/FAR. `TlbiOp` moves into
  `vetro-cpu` and `vetro-mmu` re-exports it.
- **Platform: `CpuEnv` trait.** IRQ/FIQ lines (level-triggered, read before
  every instruction) and MRS/MSR of the registers that don't live in the CPU (`EnvReg`:
  CNT* of the generic timer and ICC_* of the GICv3). The CPU does the access
  checks (EL0, CNTKCTL, read-only/write-only), the platform the
  semantics. The counter comes only from here.
- **Events to the caller.** WFI (`WaitForInterrupt`), HVC/SMC of the configured PSCI
  conduit (`Hvc`, `Smc`, with PC already advanced) and Vetro's limits
  (`Unimplemented`, state unchanged). Everything else (synchronous exceptions,
  IRQ, FIQ, SError) is delivered to the guest.
- **System mode oracle.** ID values, masks and syndromes are
  verified with a bare-metal probe (`tests/isa/system/probe.S`) run
  on `qemu-system-aarch64 -M virt,gic-version=3 -cpu cortex-a53`; the recorded
  output is versioned and the test compares it with Vetro line by line, so
  CI does not need `qemu-system`.
- **Architecture before QEMU when they diverge** in cases the Linux
  kernel does not observe: alignment faults on Device memory with the MMU on and
  for DC ZVA on Device, the SCTLR.SA/SA0 check, the syndrome of the
  MSR DAIFSet trap. Every difference is written down in `docs/specs/cpu.md` and covered by
  a test.
- **ID_AA64PFR0_EL1** declares EL0/EL1 as AArch64 only (QEMU: also AArch32),
  because Vetro does not implement AArch32 (ADR 0005).

## Consequences
- The platform builds, for every step (or block of steps), an `MmuBus`
  on top of its own physical memory and implements `CpuEnv`; it handles PSCI and WFI.
- The JIT (M4) will be able to reuse the same `SysState` and the same traits.
- Adding a system register: a line in `SysReg::lookup`, the
  checks in `sysreg_access`, the semantics in `sysreg_read/write`, a
  line in the probe.
