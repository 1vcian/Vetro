# ADR 0011 — `vetro-machine`: the complete machine in a crate of its own

- Status: accepted (M3, 2026-09-25).

## Context
In M3 the system mode pieces are ready and each has been tested on
its own:
- the CPU at EL0/EL1 with `SysBus` and `CpuEnv` (ADR 0009);
- the MMU with `MmuBus` on top of a `PhysMemory`;
- the virt platform (GICv3, timer, PL011, PL031, virtio-mmio, device tree);
- the kernel loader.

What is missing is something that puts them together. `vetro-platform` does not depend on the CPU, and must
stay that way, because its devices are tested without a CPU. The loader
lived in `vetro-cli`, which is native. The browser (M5) will need the same
machine.

## Decision
- New crate `vetro-machine`, compilable for wasm32 and without external
  dependencies. It depends on `vetro-cpu`, `vetro-mmu` and `vetro-platform`, and contains:
  - `boot`: the loader, moved from `vetro-cli`, which re-exports it;
  - `Board`: RAM, platform and counter. It implements `PhysMemory` (RAM first,
    then the MMIO bus at 1/2/4/8 bytes; no response = decode error) and
    `CpuEnv` (generic timer, the GIC's ICC_* with group 1 only, IRQ line);
  - `Machine`: CPU, MMU and `Board`, with `load_linux` (like QEMU's
    `-kernel/-initrd/-append`) and `run(budget)`, which stops on
    budget exhausted, PowerOff, Reset, Idle or Unimplemented;
  - PSCI 1.1 via HVC, with the responses of `target/arm/psci.c` for one CPU.
- **Deterministic time.** Time is the number of instructions: CNTPCT
  advances by 5 every 8 instructions, i.e. 62.5 MHz on a nominal 100 MHz, the
  same step as the user mode layer (ADR 0010). A WFI with no interrupts
  pending jumps to the next timer deadline; with no deadlines the machine
  is `Idle` and waits for input.
- **Interrupt lines.** They are updated only when something may have
  changed them: an MMIO access, a timer register, console input
  or passing the next timer deadline (cached). Not at every
  instruction.
- **Device tree like QEMU virt**, for the part that matters to Linux:
  - `rng-seed` and `kaslr-seed` in `/chosen`, derived from `MachineConfig::seed`;
  - total size of 1 MiB (QEMU does not compact its DTB, and Linux
    reserves all of it);
  - no `model`.
- **M3 oracle:** `qemu-system-aarch64 -M virt,gic-version=3,its=off
  -cpu cortex-a53 -m 1G`, i.e. the same platform as Vetro (GICv3 without
  ITS). The reference log is versioned in
  `guest/kernel/reference/qemu-boot.log`.

## Known differences from the oracle
They are listed in `tests/boot/src/lib.rs` (`KNOWN_DIFFERENCES`,
`MEMORY_LINES`):
- QEMU's GICv3 declares LPIs even without an ITS, and Linux prints "ITS: No
  ITS available";
- Vetro does not run AArch32 (ADR 0005), so the "32-bit EL0
  Support" line is missing;
- QEMU's DTB describes more devices (PCIe, fw-cfg, flash, GPIO,
  PMU): in the memory lines a few KiB change, and they are compared without
  numbers;
- the order of some asynchronous initcalls depends on QEMU's real timing:
  the comparison is by set of lines.

## Consequences
- `vetro-cli boot` and the test `tests/boot/tests/vetro.rs` use
  `vetro-machine`; the browser will use it too.
- A single CPU. SMP (CPU_ON, multiple redistributors) is an extension of
  `Machine`, to be done when Android needs it.
