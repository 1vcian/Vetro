# ADR 0022 — Vetro's AOSP 15 image: vetro_arm64 device derived from Cuttlefish

- Status: accepted (M5, 2026-09-26). Uses ADR 0005 (ISA), 0008 and 0018
  (direct boot and Android bootloader), 0020 (file manager), 0004
  (licences). Research: `docs/research/m5-android-images.md`,
  `docs/research/m5-gki-boot.md`. Details: `docs/specs/guest-image.md`.

## Context
M5 asks for the Android home screen in the browser. Prebuilt images do
not work: the SDK emulator's cannot be redistributed and composes only via
gfxstream/goldfish (surfaceflinger aborts on virt, under QEMU as under
Vetro); Cuttlefish's look for their disks on virtio-pci; the GSI is
system only. We need our own image, built from AOSP 15, that boots on the
Vetro and QEMU virt machine (GICv3, Cortex-A53, virtio-mmio, no PCI) with
the bootloader of ADR 0018, and that ships microG.

## Decision

### Base: our own `vetro_arm64` product on top of `vsoc_arm64_only`
- `guest/aosp/device/vetro/vetro_arm64` includes the `BoardConfig.mk` of
  `device/google/cuttlefish/vsoc_arm64_only` and the Cuttlefish phone
  vendor (`shared/phone/device_vendor.mk`): virtual HALs already designed
  for a VM, SwiftShader, minigbm, HWC ranchu, software KeyMint and
  Gatekeeper. Only what virt requires changes. A full fork of Cuttlefish
  would cost every AOSP update; a minimal device from scratch would mean
  rewriting dozens of HALs.
- 64-bit only (`core_64_bit_only.mk`, armv8-a, `TARGET_CPU_VARIANT :=
  cortex-a53`): Vetro's CPU has no AArch32. No
  `packages/modules/Virtualization` (there is no KVM).
- AOSP tag `android-15.0.0_r36`, target `vetro_arm64-bp1a-userdebug`
  (`bp1a` is the tag's release config; `trunk_staging` would turn on
  in-development flags). Build only on the dedicated Linux x86_64 VM
  (`tools/aosp`), never in CI.
- Branding: `PRODUCT_BRAND/MANUFACTURER := Vetro`, model "Vetro arm64",
  serial number `VETRO00001`. The product does not present itself as
  Android nor as Google; microG's package names (`com.google.android.gms`,
  `com.android.vending`) are technical identifiers imposed by
  compatibility, and the apps are called "microG Services" and "microG
  Companion".

### Kernel and first stage
- Prebuilt GKI android15-6.6 kernel from the tree
  (`kernel/prebuilts/6.6/arm64`,
  `6.6.57-android15-8-g8b48c9979699-ab12748506`), modules from
  `kernel/prebuilts/common-modules/virtual-device/6.6/arm64`: the same as
  Cuttlefish, not recompiled.
- `virtio_mmio.ko` in the vendor_ramdisk, next to those Cuttlefish already
  puts there (virtio_blk, virtio_net, virtio-gpu, virtio_input,
  virtio_console, vmw_vsock_virtio_transport, virtio-rng, …): Cuttlefish
  loads it only in the second stage because it uses PCI; on virt without
  it the first stage does not see the disk. vsock is built into the
  kernel.
- `androidboot.boot_devices` lists all 32 virtio-mmio slots of virt
  (`a000000.virtio_mmio` … `a003e00.virtio_mmio`): the disk is found in
  whatever slot it ends up in, with or without vsock, under QEMU and under
  Vetro.
- The parameters that on Cuttlefish the launcher sets (`assemble_cvd`,
  `qemu_manager.cpp` with `--gpu_mode=guest_swiftshader`) are in the
  vendor_boot bootconfig section (`BOARD_BOOTCONFIG`): neither Vetro's
  bootloader nor QEMU has to add anything. Slot A, `force_normal_boot=1`,
  `verifiedbootstate=orange`.

### Disks
- A single GPT disk (`tools/aosp/mkdisk.sh`): `misc`, `frp`, `metadata`
  (empty ext4), `super` (logical partitions system, system_ext, product,
  system_dlkm, vendor, odm, vendor_dlkm, odm_dlkm) and `userdata` (the
  build's empty f2fs; vold encrypts it on first boot). The first stage
  finds partitions by GPT name.
- Our own fstab (`fstab.vetro`, `androidboot.fstab_suffix=vetro`): the
  same entries as `fstab.cf.f2fs.hctr2` without the `avb` flag, without
  virtiofs shares and SD card.
- **AVB/vbmeta disabled**: the fstab does not ask for dm-verity, Vetro's
  bootloader does not read vbmeta (ADR 0018), the userdebug build is
  "orange". The build still produces a vbmeta signed with the test keys:
  it is not used.

### What of Cuttlefish is removed or stopped (from the tests under QEMU)
- **Lights and OEM lock** (`LOCAL_ENABLE_LIGHT/OEMLOCK := false` before
  inheriting the vendor): their HALs talk to the host and abort, but they
  are declared in the VINTF, and system_server waits for them forever
  (`LightsService` on `ILights/default`): without removing them the boot
  stops before `activity`.
- **HALs and services that abort or exit without a host** (UWB, Thread,
  ConfirmationUI, NFC, `bt_socket`, `seriallogging`): `init.vetro.rc`
  stops them the first time they go to `restarting`. Looping, they cost a
  tombstone every few seconds and `flags_health_check`: the guest's load
  average dropped from ~30 and the first boot took twice as long.
- **Odex in the partitions** (`BOARD_USES_SYSTEM_OTHER_ODEX :=`):
  Cuttlefish puts them in `system_other` (slot B) and copies them to
  `/data` on first boot; our disk has only slot A, and without odex
  ArtService recompiled every app on first boot (sys.boot_completed from
  ~1300 to 476 s of guest time under QEMU).
- **No lock screen** (`ro.lockscreen.disable.default=true`).

### Graphics
- SwiftShader (Vulkan "pastel") with ANGLE for GLES 3.1, minigbm gralloc,
  HWC ranchu with in-guest composition on the virtio-gpu 2D DRM, the same
  values as Cuttlefish in `guest_swiftshader`. No gfxstream nor virgl:
  Vetro's GPU is 2D (on WebGPU in the browser). Density 240, screen that
  of virtio-gpu (1280x800 by default).
- No boot animation and no setup wizard (`enable_bootanimation=0`,
  `setupwizard_mode=DISABLED`), `hw_timeout_multiplier=50` for a slow
  emulated CPU.

### Security, adb, SELinux
- **adbd on TCP 5555 without authorization (`ro.adb.secure=0`) ONLY in
  the userdebug development build**: Cuttlefish does the same. A build
  for users must remove `ro.adb.secure=0` and use adb keys. The host
  reaches it with `--hostfwd=tcp::5555-:5555` (Vetro) or QEMU's port
  forwarding.
- Software KeyMint and Gatekeeper in the guest (`rust_nonsecure`,
  `nonsecure`): on Cuttlefish they live on the host, which is not there
  here.
- SELinux permissive (`androidboot.selinux=permissive`, userdebug only)
  until the policy covers the virtio-mmio paths: first step in
  `sepolicy/file_contexts` (network and block sysfs like Cuttlefish's PCI
  paths); the goal is enforcing, with the denials logged in the first
  boots.
- eth0 stays eth0 and EthernetService manages it with DHCP
  (`ro.vendor.disable_rename_eth0=1`): no simulated Wi-Fi nor
  Cuttlefish's host OpenWRT.

### ART
- `dalvik.vm.isa.arm64.variant=cortex-a53` (from `TARGET_CPU_VARIANT`),
  checked by `tools/aosp/fetch.sh` on the artifacts: with a55 or newer
  variants the JIT would generate LSE and FP16, which Vetro's CPU does not
  have (ADR 0005). No 32-bit ABI (this is checked too).
- One CPU and 2–3 GiB of RAM (`androidboot.ddr_size=3072MB`, `vetro boot
  --mem=3072`, QEMU `-m 3G`).

### microG
- GmsCore and Companion, official release `v0.3.16.252432` from
  github.com/microg/GmsCore (Apache 2.0), downloaded on the VM with a
  pinned sha256 (`guest/aosp/vendor/vetro/microg/microg.lock`), never
  committed. Prebuilt privileged apps in `/product/priv-app`, with the
  privileged permissions allowlist generated by `aapt2 dump permissions`,
  the default runtime permissions and the battery-saving exemption.
- Presigned: the two APKs have targetSdk 29 and a v1 signature, the build
  uncompresses dex and JNI libraries and the v1 signature stays valid
  (system partition apps are verified without v2's stripping
  protection).
- **Signature spoofing limited to microG** in `frameworks/base`
  (`guest/aosp/patches/frameworks/base/0001-…patch`, applied by
  `tools/aosp/remote/prepare.sh`): `FAKE_PACKAGE_SIGNATURE` permission
  (`signature|privileged`, `@hide`), and in
  `ComputerEngine.generatePackageInfo` the signature from the
  `fake-signature` meta-data replaces the real one only if the package is
  `com.google.android.gms` or `com.android.vending`, is signed with
  microG's certificate (SHA-256 `9bd06727…d14165`, verified on the
  releases), requests the permission and has been granted it. No other app
  can pretend to be another: it is LineageOS's "restricted" variant,
  stricter.

### File manager
- The `vetro-files` daemon (ADR 0020) goes into `/vendor/bin`, compiled
  with bionic from the same source `guest/kernel/initramfs/vetro-files.c`
  (`tools/aosp/sync.sh` copies it into the tree: no copy in
  `guest/aosp`). Init service `vetro_files` started at `post-fs-data`
  only with `ro.debuggable=1`, `oneshot` (without vsock it exits and does
  not restart in a loop).
- Its own SELinux domain `vetro_files` (`init_daemon_domain`, vsock
  sockets allowed), **permissive only in userdebug/eng builds**: it must
  read and write every app's data as root, which no restricted rule grants
  without clashing with AOSP's neverallows. In a user build the domain
  stays confined and init does not start it.

### Artifacts and licences
- `tools/aosp/fetch.sh` brings to the Mac `boot.img`, `vendor_boot.img`,
  `init_boot.img`, `super.img`, `userdata.img` with `SHA256SUMS` and
  `build-info.txt` (tag, BUILD_ID, kernel, project commits);
  `tools/aosp/upload.sh` publishes them to Cloudflare R2 under
  `aosp/<tag>-<BUILD_ID>-<Vetro commit>/` with `manifest.json` (sha256,
  sizes). A published version does not change. Never binaries in git.
- Only redistributable artifacts: AOSP (Apache 2.0, with GPL/LGPL parts),
  microG (Apache 2.0). `tools/aosp/gpl-sources.sh` prepares the exact
  sources of the kernel (kernel/common at the commit of the version
  string, virtual-device modules at their commit) and of the GPL/LGPL AOSP
  projects, plus the pinned repo manifest and Vetro's patches: they are
  published together with the images.
- The device files and configurations are Vetro code (PolyForm
  Noncommercial 1.0.0, ADR 0004); for Soong `legacy_notice`, because the
  licence has no SPDX type in the build system.

## Rejected alternatives
- **Cuttlefish images as they are**, with a PCIe ECAM host in Vetro:
  still possible (future ADR), but it requires u-boot or a launcher, and
  AVB.
- **Rebuilt ranchu emulator image** with software composition: goldfish
  in every HAL, no advantage over Cuttlefish.
- **M3's 6.18 kernel**: it lacks binder, eBPF, dm-verity, and the GKI
  module ABI.
- **General signature spoofing** (any app with the permission):
  needlessly broad; two packages with a known certificate are enough.

## Consequences
- The image is rebuilt with `tools/aosp/build.sh start|wait`,
  `tools/aosp/fetch.sh`, and booted with `tools/aosp/qemu.sh` (oracle) and
  `tools/aosp/vetro.sh`, with the same devices in the same slots.
- Updating AOSP = change the tag, redo `repo sync`, check that the
  `frameworks/base` patch applies (`prepare.sh` stops if not).
- Updating microG = new `microg.lock` and regenerated allowlist.
- Trademarks in the AOSP UI (Launcher3's "Google" bar, robot,
  `ro.product.system.*`): prepared in ADR 0030 (overlay, wallpaper,
  QuickSearchBox removed), awaiting the build.
- To do: SELinux enforcing, deterministic virtio-rng in Vetro (the module
  is already in the first stage), adb over a virtio channel for the
  browser, disk via HTTP Range (M6).
