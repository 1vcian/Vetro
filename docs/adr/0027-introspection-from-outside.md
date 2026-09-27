# ADR 0027 — Guest introspection from outside: kernel profile, hooks in the machine

- Status: accepted (M7–M9, common base, 2026-09-26). Extends ADR 0011
  (machine loop), 0013 and 0024 (system JIT), 0019 (replay).

## Context
The M7 TLS hooks (SSL_read/SSL_write), the M8 Binder decoder and M9
scripting need the same things: knowing which processes are running, where
their libraries are mapped, what they call (syscalls, binder ioctls) and
stopping "invisibly" at the entry of a user function. The guest must not be
able to find out: no agents, modules, BRKs or modified pages. Determinism
(and the M10 replay) must not change.

## Decision

### Kernel profile: symbols + BTF, without per-version tables
- **Symbols**: `System.map` if there is one, otherwise the kallsyms table
  inside the `Image`, found without symbols (token table from the digits
  `0`..`9`, verified by the index; markers, names, offsets in both of the
  kernel's orders). `kallsyms_relative_base` in the file is 0 with
  RELA relocations (`--no-apply-dynamic-relocs`, GCC/BFD): it is taken
  from the addend of the right R_AARCH64_RELATIVE (the one whose base makes
  the places to relocate zero); with RELR (GKI) it is already in the file.
- **Types**: our own BTF parser (`vetro_analysis::introspect::btf`), from
  the blob in the `Image` (GKI: `CONFIG_DEBUG_INFO_BTF`) or from a separate
  file.
- **6.18 test kernel**: no BTF (it would need `BPF_SYSCALL`, which changes
  the kernel). `tools/guest-kernel/build.sh` builds in a separate folder
  `vmlinux` with the same `.config` plus `DEBUG_KERNEL` +
  `DEBUG_INFO_DWARF5` (with `DEBUG_MISC` and `RCU_TRACE` off, and a
  check that nothing else changes) and extracts from it with `pahole` the
  separate BTF `target/guest-kernel/vmlinux.btf`. The `Image` that boots does
  not change (verified: same bytes).
- **KASLR**: our Android boot uses `nokaslr` (and the test kernel does
  not have `RANDOMIZE_BASE`); in any case the offset is derived from VBAR_EL1
  minus `vectors`.
- **Translation**: AArch64 table walker (4 KiB, levels from TxSZ,
  blocks) on read-only `PhysMem`; a process's user space is the kernel's
  with TTBR0 = physical address of `mm->pgd`. `vmemmap` (for the page
  cache) is calibrated with a thread's stack (`stack_vm_area->pages[0]`
  against the physical address of `task->stack`), without version constants.

### Hooks in the machine (`vetro_machine::hooks`)
- A single `Tracer` (trait `Any`) receives `Event::SyscallEnter`,
  `SyscallExit` and `Breakpoint` with a read-only `GuestView`
  (registers and RAM): it cannot change the state.
- **Syscalls**: the interpreter always executes SVC (the JIT closes blocks
  before it) and ERET (not translated). Entry = synchronous exception from
  EL0 with EC 0x15; exit = first EL1→EL0 transition of the same thread, keyed
  by SP_EL1 (top of the thread's kernel stack, equal at entry and at
  return). The return PC distinguishes normal return from execve,
  signals, syscalls to restart. The process is derived from `__entry_task`
  per CPU (+ TPIDR_EL1). `exit`/`exit_group` are recorded at entry.
- **Breakpoints**: before every interpreter step at EL0 a 64-bit filter
  and a map by address (with an optional filter on the process's TTBR0).
  The event arrives only if the instruction was executed (or is an
  SVC): an exception in between (fetch fault, IRQ) does not duplicate it.
  Registers from before the instruction, RAM from after.
- **JIT**: `SysJit::set_stops(addresses)` (minimal hook in the JIT): the
  regions do not contain those addresses (discovery stops before, a
  region does not start there), so the interpreter always executes them.
  Changing the set forgets blocks and branch cache entries (new epoch),
  without resetting the engine. Dedicated parity test.
- The cost with hooks on is only in the steps that change EL or have
  a point at the PC; nothing in snapshots or logs.

### Dependencies
`vetro-machine` depends on `vetro-analysis` (which stays without dependencies
and compiles to wasm): the machine provides the `GuestView` as `PhysMem` and
hosts the ready-made tracer (`introspect::SyscallTracer`) and `Machine::linux`.

## Rejected alternatives
- **BRK in the guest code**: the guest would see it (checksums, reading its
  own code) and it would change RAM, hence the replay.
- **Hand-written offset tables per version**: they break with every
  kernel; BTF is there in GKI and for the test kernel it costs one build.
- **Rebuilding the test kernel with BTF**: it would change `Image`, logs and
  comparisons with QEMU.
- **Syscall exit on return to `svc_pc + 4`**: it misses execve and
  signals; the SP_EL1 key covers them.

## Verification
- `vetro-analysis`: unit tests for BTF (anonymous unions, arbitrary bytes),
  kallsyms (fake table, RELA), walker, ELF (file and memory with
  GNU_HASH), binder, strace.
- `vetro-jit-native`, `sys_parity::punti_di_fermata_restano_all_interprete`:
  150 programs with three random stops, identical to the interpreter and with
  every step on the stops done by the interpreter (fails without the check).
- `tests/boot/tests/introspect.rs` (test kernel, release):
  `ps` = processes from memory (29); `/proc/<pid>/maps` = reconstructed
  text; `/proc/<pid>/fd`; cmdline; symbols of vetro-dev from the page
  cache = file on the host (and the whole file); syscalls of `cat` as user
  501 = `qemu-aarch64 -strace` (16, in order); breakpoints on
  `open`/`ioctl` = `openat`/`ioctl` syscalls of the process with the same
  arguments; BusyBox entry once per exec; same console and
  same final state without hooks; replay with the JIT and with hooks:
  same syscalls and same breakpoints, identical replay.
  `profili_dei_kernel`: 6.18 kallsyms = System.map (18834 symbols);
  GKI 6.6 from `boot.img`: 107807 symbols, BTF with binder_proc,
  binder_transaction.

## Consequences
- TLS hooks (M7), Binder decoder (M8), scripting (M9) are written as
  `Tracer`s or on top of `SyscallTracer`/`Linux`.
- The long test on Android (processes with system_server/zygote, first
  binder transaction) is not done yet: see `docs/progress/M7.md`.
