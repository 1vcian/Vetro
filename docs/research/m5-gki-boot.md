# M5 — booting the GKI android15-6.6 kernel on the Vetro machine (2026-09-25)

Experiment: the Android 15 arm64 "default" SDK emulator image
(`arm64-v8a-35_r02.zip`, kernel `6.6.30-android15-8`, build
`AE3A.240806.019`) booted on the Vetro machine under
`qemu-system-aarch64` 10.0 and under `vetro boot`, with the same
configuration, to see how far they get and where they diverge. Local use
only: the image is under the SDK license and must not be committed or distributed
(`tools/android-emu/README.md`).

## Common configuration
- Machine: `virt,gic-version=3,its=off`, Cortex-A53, one CPU, 2 GiB,
  modern virtio-mmio, no network, no GPU or input (QEMU's `-device`s
  and Vetro's `--no-devices`).
- Direct boot: `Image` (the decompressed `kernel-ranchu`) and `ramdisk.img`
  as is: two cpio archives (generic ramdisk and vendor ramdisk with
  `fstab.ranchu` and the virtio modules) in a legacy LZ4 stream; the GKI has
  `CONFIG_RD_LZ4=y`. `virtio_mmio`, `virtio_blk` and the rest are modules,
  loaded by the first stage of init (`modules.load`).
- Disks (copy-on-write: `snapshot=on` in QEMU, `CowBackend` over the file
  in Vetro), in the order of the `-device`s; the first goes into the highest
  virtio-mmio slot and Linux numbers the disks by increasing address:
  | Slot | Address | File | Linux | Use |
  |---|---|---|---|---|
  | 31 | `a003e00` | `userdata.img` (empty ext4, 2 GiB) | vdc | `/data` (the fstab wants `/dev/block/vdc`) |
  | 30 | `a003c00` | `encryptionkey.img` (GPT "metadata") | vdb | `/metadata` (the fstab wants `a003c00.virtio_mmio`) |
  | 29 | `a003a00` | `system.img` (GPT "vbmeta" + "super") | vda | logical partitions system, system_ext, product, vendor, system_dlkm |
  The separate `vendor.img` is not needed: vendor lives in super.
- Command line (`tools/android-emu/cmdline.sh`): `console=ttyAMA0
  nokaslr 8250.nr_uarts=1 printk.devkmsg=on loop.max_part=7
  androidboot.hardware=ranchu androidboot.qemu=1
  androidboot.selinux=permissive androidboot.boot_devices=a003a00.virtio_mmio
  androidboot.console=ttyAMA0` plus the three `androidboot.vbmeta.*` from
  `VerifiedBootParams.textproto`. `nokaslr` makes the addresses comparable
  (both provide `kaslr-seed` in the DTB). The fstab does not ask for
  AVB/verity, so this is enough.
- `/data`: with a zero-filled disk init does not format it (no `formattable`),
  vold finds metadata encryption without a key and init reboots into
  recovery (`init_user0_failed`). With an empty ext4 (`mkfs.ext4`, as the
  emulator host does) vold generates the key, mounts `/data` on
  dm-default-key and the boot continues. Without `/data` at all: `bpfloader`
  fails and init reboots (`netbpfload-missing`).

## How far they get
Times in guest seconds (printk timestamps):

| Stage | QEMU | Vetro |
|---|---|---|
| `/init` (first stage) | 0.53 | 2.25 |
| second stage of init | 1.70 | 3.73 |
| `mount_all --late` succeeded (vold, encrypted `/data`) | 19.6 | 39.2 |
| `init_user0` succeeded | 38.0 | 106.3 |
| `bpfloader` finished (status 0) | 60.2 | 135.3 |
| **zygote started** | **60.6** | **135.8** |
| surfaceflinger started | 71.7 | 151.4 |
| surfaceflinger aborts, zygote restarted | 84.3 | 167.5 |

Both reach the same point: **zygote starts, then surfaceflinger
aborts (SIGABRT) and init restarts zygote in a loop** (every ~16 guest s in
QEMU, ~20 s in Vetro). The reason is the same in both: the emulator
image composes via gfxstream and goldfish devices (pipe, sync,
address space) that virt does not have. Even with `-device virtio-gpu-device`
(2D, `number of cap sets: 0`) surfaceflinger aborts the same way in
QEMU. The goldfish services also fail the same way in both:
`vendor.sensors-hal-multihal` (SIGABRT), `goldfish-logcat` (status 6),
`qemu-props`, `misctrl`, `kcmdlinectrl`; `keystore2` crashes only
without `/data`. The set of dead services and signals matches.

Vetro's guest time is longer because its virtual CPU runs at a nominal 100
MHz (one instruction every 10 ns, ADR 0011): the same work that in
QEMU (TCG with host real time, about 1 billion instructions per
guest second) takes 2 to 10 times longer in guest seconds. Native Vetro
executes 35–55 million instructions per real second: zygote after
4–5 real minutes (13.6 billion instructions), QEMU in Docker after about
1.5 minutes.

## Divergences found
Line-by-line comparison without timestamps (`tools/android-emu/compare.py`),
first divergence and lines present in only one log.

1. **PL061 GPIO and `gpio-keys` missing in Vetro — fixed.** The QEMU
   virt DTB has `pl061@9030000` (SPI 7) and `gpio-keys/poweroff` (line 3,
   KEY_POWER). The GKI uses them: `input: gpio-keys as .../input0`, and init
   finds `/dev/input` (`EVIOCSMASK not supported`); under Vetro init
   printed `Could not add watch for /dev/input` and Android had no
   power button. Now `vetro-platform` has the PL061 (logic from
   `hw/gpio/pl061.c`, undriven lines at 0 as in virt), the device
   tree has the same nodes as QEMU and the host presses the button with
   `Board::gpio_input(3, ..)`. Under Vetro the same lines as
   QEMU appear (`PL061 GPIO chip registered`, `input: gpio-keys`, `EVIOCSMASK`).
   Tests: `pl061::tests` (6, including both edges as gpio-keys uses, level,
   DATA mask, PrimeCell ID), `virt::tests::tasto_di_spegnimento_sullo_spi_7`,
   `board::tests::tasto_di_spegnimento_dall_host`, and the nodes in the DTB test
   (`proprieta_della_piattaforma`, values taken from the QEMU 10.0 DTB with
   `dumpdtb`). The M3 guest kernel has no GPIO drivers: its logs do not
   change.
2. **`jitterentropy: Initialization failed ... requirements: 9` only in
   Vetro — noted, not fixed.** Error 9 is `JENT_EHEALTH`: the repetition
   test of the jitter entropy source fails because in Vetro
   the counter (CNTVCT) is an exact function of instructions, so two
   measurements of the same loop give the same time. It is the intended consequence
   of determinism (ADR 0010/0011); not verified with QEMU's `-icount`.
   The GKI is not in FIPS mode, so the failure is not fatal and the
   boot continues identically. Making it "real" would require deterministic noise
   in the counter: to be decided with an ADR, if it becomes necessary.
3. **Differences already known in `tests/boot`** (`KNOWN_DIFFERENCES`): QEMU declares
   LPIs without an ITS; Vetro has no AArch32 ("32-bit EL0/EL1 Support" is missing:
   the image is 64-bit only, `zygote_secondary` exists in neither
   of the two); the QEMU DTB also has PCIe (`pci-host-generic`, no
   devices), fw-cfg, flash and PMU (`hw perfevents: armv8_pmuv3`).
   The RTC time is Vetro's fixed one (2026-01-01); the `/data`
   encryption keys are random in both.
4. **Timing effects, not errors:** in Vetro init prints more
   `Command ... took Nms` lines (it prints them above 50 ms), `sched: RT throttling
   activated` appears once (a real-time thread exceeds 95% of
   a guest second on a slower CPU), the order of apexd's `loopN`
   changes, and in one boot `prng_seeder` wrote to kmsg (logd not yet
   ready) the failure that under QEMU goes to logcat: both lack
   `/dev/hw_random`, because neither has virtio-rng.

No missing instructions, no wrong system registers, no extra
crashes: up to the surfaceflinger loop Vetro's behaviour
matches QEMU, including the processes that crash (same signals,
same abort messages and, for `keystore2` without `/data`, the same abort
point in the tombstone: `abort+168` in libc with the same Rust chain).

## What is needed to go further
- **Graphics:** the emulator image wants gfxstream + goldfish; with
  a 2D virtio-gpu it does not compose. We need our own image (fork of
  `vsoc_arm64_only`, `m5-android-images.md`) with SwiftShader +
  drm_hwcomposer + minigbm on Vetro's 2D virtio-gpu, or a
  ranchu image with software composition (`ro.hardware.egl=swiftshader`,
  HWC on DRM). It is not a Vetro defect: QEMU stops at the same point.
- **virtio-rng** (for `prng_seeder` and kernel entropy): a small
  device, to be fed by a deterministic generator seeded from
  `MachineConfig::seed`. To be compared with `-device virtio-rng-device`.
- **Goldfish devices** (pipe, sync, address space, battery): needed
  only by the SDK image, not by ours.
- **PMU** (`armv8_pmuv3`): needed by simpleperf/perfetto, not by boot.
- **PCIe (ECAM)** for unmodified Cuttlefish images: see
  `m5-android-images.md`, to be decided with an ADR.
- **Speed:** at about 50 real MIPS the first zygote arrives after 4–5 minutes;
  the system JIT (M4, in progress) is the next multiplier.

## Reproduction
`tools/android-emu/README.md`: file preparation, `qemu.sh`, `vetro.sh
[guest seconds]` and `compare.py`. `vetro boot` has the new options
`--no-devices`, `--disk=FILE` (repeatable, copy-on-write in memory),
`--guest-secs=N` and `--stats`; the test
`crates/vetro-cli/tests/boot_disk.rs` exercises them with the M3 guest kernel
(read, write, file intact).
