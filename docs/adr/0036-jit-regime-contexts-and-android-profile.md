# ADR 0036 — JIT: contexts per table base, TLBI by VA, the Android workload

- Status: accepted (M4, 2026-09-27). Extends ADR 0013, 0024 and 0026.

## Context
The prebuilt Android snapshot (ADR 0031) made the real workload measurable:
`tools/aosp/android-perf.mjs` restores it in Node/V8 on the app's machine and
runs what a visitor does (adb, launcher idle, `pm install` of a catalog app,
`am start` until focused, app idle), phase by phase, with the JIT and machine
counters. The host's actions depend only on the guest, so every build runs
the same instructions in every phase: only the time changes.

The first profile (numbers in `docs/progress/M4.md`) showed that the regime
bookkeeping of ADR 0013 does not fit a Linux with many processes:
- every change of TTBR0/TTBR1 (with the ASID) started a new jump cache epoch
  and flushed the whole software TLB. Linux's software PAN (no hardware PAN
  on a Cortex-A53) switches TTBR0 at every kernel entry and exit: 1.1 M epochs
  in 31 s of guest time, 43 M `env.resolve` calls, 39 M software TLB fills;
- every TLBI did the same, and Android issues a TLBI by VA for most unmaps,
  COW breaks and permission changes (65 thousand in the same 31 s);
- the kernel entry/exit code itself (MRS TTBR1, MSR TTBR0/TTBR1) and the
  uaccess routines (LDTR/STTR) left the regions for the interpreter at every
  syscall.

## Decision

### Contexts per table base
The jump cache context (`ctx`) is a number per (EL, TTBR0 base, TTBR1 base),
handed out on first use since the last epoch; the bases are the TTBRs without
the ASID. A new epoch (all numbers forgotten) comes only with a change of
SCTLR, TCR or MAIR, a TLBI other than by VA, or a code invalidation. Coming
back to the same tables gives the same number, so the entries of the kernel
and of each process survive the switches. The ASID does not change what a
walk from the same tables gives; as for the MMU's TLB, an entry can become
stale only if the guest changes the tables without a TLBI, which the
architecture leaves unpredictable (the accepted difference of ADR 0013).

### Software TLB groups
Entries are grouped per (EL, half of the address space) with the base they
were filled under; when a group's base differs from the current one at the
start of a run, the group is emptied (by the list of entries it filled, or by
scanning its half after 1024 entries).

### TLBI by VA
`Tlb` keeps a log of its last 64 invalidations (`Tlb::invalidations_since`):
`Inval::Va(page)` for the by-VA forms, `Inval::All` for the others. If since
the last run there were only by-VA invalidations and the granules are 4 KiB,
the JIT removes only the jump cache and software TLB entries within the
1 GiB around each address (the largest block an entry can come from);
otherwise a new epoch as before.

### More instructions in regions
- LDTR/STTR: at EL1 through the host with the permissions of EL0 (size bit
  `SIZE_UNPRIV`, never entered in the software TLB), at EL0 as ordinary
  accesses.
- MRS TTBR0/TTBR1/CONTEXTIDR/MIDR and MSR CONTEXTIDR at EL1, MRS FPCR/FPSR and
  MSR FPSR (with its mask) when FP is enabled. MSR TTBR0/TTBR1 ends the run
  with `YIELD` after the instruction, so the next run starts in the new regime
  (contexts, TLB groups); `JitState` grows to 1024 bytes.
- `SysJit::run` probes the jump cache for the entry `pc` before the host
  lookup (the same guarantee the dispatcher relies on).

### Snapshot restore
`vetro_snapshot_restore_stream` grows the heap once for the head (the disk's
copy-on-write clusters, 4 KiB allocations each: `memory.grow` 64 KiB at a time
of a multi-GiB memory cost V8 about half a millisecond each), and the LZH
decoder copies matches 8 bytes at a time.

### Not done, and why
- A larger software TLB (2048 entries per table): measured, see
  `docs/progress/M4.md`.
- WFI skip: already there (time jumps to the next deadline); the launcher
  keeps the CPU busy (no WFI steps at all in the measured phases), so there is
  nothing idle to skip.

## Verification
Targeted tests in `vetro-jit-native/tests/sys_parity.rs`, each red when the
point is broken on purpose: `ttbr0_switches_keep_code_and_data_apart` (same
user VA in two tables, EL1 code in TTBR0), `tlbi_by_va_forgets_the_whole_block`
(TLBI of one page of a remapped 2 MiB block), `msr_ttbr0_in_a_region_ends_the_run`,
`ldtr_sttr_at_el1_use_el0_permissions`; the new registers in the random
system programs; `vetro-mmu` `invalidations_since_remembers_the_last_ones`;
`vetro-snapshot` `match_copies_of_every_distance`. Plus the existing parity
suites, the kernel boot with the JIT on wasmtime and V8 (same instructions and
log), and the Android phases with the same instructions as before.

## Consequences
- JIT ABI (`docs/specs/jit.md`): `JitState` 1024 bytes, `SIZE_UNPRIV`, the
  context number, the new system registers.
- `vetro-mmu` exports `Inval` and `Tlb::invalidations_since`.
- `SysJitStats` gains counters (host ld/st, epoch reasons, base switches,
  partial TLBIs, jump cache probes, WASM bytes); `Machine::perf`; vetro-wasm
  exports `vetro_machine_set_jit_with`, `vetro_jit_profile`, `vetro_perf`
  (additive, ABI unchanged).
