# ADR 0008 — M3 guest kernel and direct boot protocol

- Status: accepted (M3, preparation, 2026-09-24)

## Context
M3 calls for an arm64 Linux kernel with an initramfs up to the shell, first under
the oracle (`qemu-system-aarch64 -M virt`) and then under Vetro. We need a
reproducible kernel, a way to verify it, and a loader that places the
kernel, initramfs and DTB where QEMU places them, so the two boots can be
compared.

## Decision
- **Kernel:** Linux 6.18.53 (longterm), kernel.org tarball pinned by
  SHA-256 in `tools/guest-kernel/build.sh`, without patches. Built in an
  Alpine 3.22 arm64 container on a Docker volume (the sources have names that
  differ only in case: APFS does not distinguish them). Build date, user and
  host are pinned (`KBUILD_BUILD_*`).
- **Configuration:** `make allnoconfig` + `guest/kernel/config/vetro.config`.
  The script checks that every option of the fragment made it into the
  `.config` and that the resulting `savedefconfig` matches
  `guest/kernel/config/defconfig` (versioned): any drift shows up in review.
  Contents: PL011 with earlycon, PL031, GICv3, generic timer, virtio-mmio,
  virtio-blk/net/console, devtmpfs, gzip initramfs, printk; no modules,
  no PCI (the virt board's virtio devices are also on mmio and those are
  enough for Vetro's platform), no KASLR (determinism), no
  extensions beyond ARMv8.0 (ADR 0005).
- **SMP:** on arm64 `CONFIG_SMP` is always on; `NR_CPUS=2` (the minimum
  allowed). The guest runs with one CPU (`-smp 1`, QEMU's default). The kernel
  queries PSCI via `HVC` anyway (version, and `SYSTEM_OFF` for
  `poweroff`): Vetro's platform must answer these calls.
- **Initramfs:** the kernel's `usr/gen_init_cpio` with `-t 0` and `gzip -n`
  (no root, no dates: reproducible), static BusyBox from
  `tools/guest-bins`, `/init` and `/etc/autotest.sh` in
  `guest/kernel/initramfs/`. Markers: `VETRO-BOOT-OK` (start of `/init`),
  `VETRO-AUTOTEST-FINE: ok` (autotest succeeded). Then a shell on `ttyAMA0`
  (`setsid cttyhack sh`). `vetro.noautotest` and `vetro.poweroff` on the
  command line change the flow for scripted boots.
- **System oracle:** native `qemu-system-aarch64` in CI (arm64 runner),
  on macOS `tools/guest-kernel/qemu-system-aarch64-docker.sh` with
  a dedicated Debian trixie image (`Dockerfile.qemu`, same base and
  same QEMU version as the user mode oracle). `tools/oracle/Dockerfile`
  stays unchanged: the system package is heavy and is only needed for these tests.
- **Verification:** `tests/boot` (`vetro-boot-tests`) runs the reference
  command, checks the markers within `VETRO_BOOT_TIMEOUT`, writes a
  command to the console and reads its result (the shell is alive), shuts down with
  `poweroff -f`. Reference log in `guest/kernel/reference/qemu-boot.log`.
- **Loader** (`vetro-cli::boot`, pure): `Image` header (magic, text_offset,
  image_size, flags; big-endian kernels and 16 KiB granule rejected, the
  Cortex-A53 does not have it). Layout as in QEMU's `hw/arm/boot.c`: kernel at
  `0x4000_0000 + text_offset`, moved by 2 MiB if `text_offset < 4 KiB`
  (QEMU keeps its stub there), hence `0x4020_0000` for modern kernels;
  initramfs at `base + min(ram/2, 128 MiB)` and in any case past the kernel's
  bss; DTB right after, aligned to 2 MiB. Entry: `x0` = DTB,
  `x1..x3` = 0, PC = start of the Image, EL1h, DAIF masked
  (PSTATE `0x3c5`), MMU off. QEMU's stub is not emulated: the registers are
  set directly.

## Consequences
- Licenses: `target/guest-kernel/sources/` contains the exact tarball, the
  fragment, the defconfig and the scripts; for BusyBox (GPL as well) we point to
  the Alpine package and the corresponding aports. Before distributing
  images, the BusyBox source tarball with the Alpine patches must be added.
- Changing kernel version: new SHA-256, `VETRO_KERNEL_UPDATE_CONFIG=1`,
  new reference log (`VETRO_BOOT_UPDATE_REFERENCE=1`).
