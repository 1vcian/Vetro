# M5 — Android images for Vetro (research, 2026-09-25)

Basis for the M5 ADR. Verified against official sources and with a scan of
the Android 15 arm64 "default" emulator image: 1,681 ELF files
disassembled; `linker64` and `toybox` run under `qemu-aarch64 -cpu
cortex-a53`.

## Conclusions
- **ISA: no extension beyond ARMv8.0 + CRC32 + crypto**, which Vetro already has
  (ADR 0005). The sources:
  - Soong compiles with `-march=armv8-a -mcpu=cortex-a53`;
  - ART assumes CRC32 for the generic and cortex-a53 variants;
  - LSE, SHA512/SHA3, MTE, DotProd and SVE appear only behind runtime
    selection;
  - PAC/BTI live in the HINT space, i.e. NOP on v8.0.

  Constraints:
  - HWCAP without capabilities Vetro does not have;
  - `dalvik.vm.isa.arm64.variant` = `generic` or `cortex-a53`: with a55 or
    later, ART's JIT would emit LSE and FP16.

  Still to verify, with a full boot under `qemu-system-aarch64 -M virt
  -cpu cortex-a53`: about 361 `sdot` attributed to `libinput.so`, almost certainly
  data.
- **Prebuilt images:**
  - **Emulator (`sdk_phone64_arm64`, API 35)**
    - virtio-mmio with the virt memory map, direct boot.
    - Graphics only via host gfxstream and goldfish devices.
    - SDK license: not redistributable. Useful only as a local test.
  - **Cuttlefish `aosp_cf_arm64_only_phone`**
    - armv8-a/cortex-a53, 64-bit only; SwiftShader graphics on virtio-gpu 2D
      (suitable).
    - virtio-pci devices, and `virtio_mmio.ko` only in the second stage of
      init. Without PCI it does not find the disks.
    - u-boot, boot.img v4 with vendor_boot and init_boot, AVB.
  - **GSI:** system only, not enough.
- **microG**
  - GmsCore, Apache 2.0, as a prebuilt privileged app.
  - Requires signature spoofing, i.e. a patch to `frameworks/base`
    (LineageOS model, restricted to microG).
  - So we need our own build.
- **AOSP 15 build**
  - Only on a Linux x86_64 host: macOS has not been supported since Android 11,
    and a Linux arm64 host is not supported.
  - At least 400 GB of disk and 64 GB of RAM; official times: about 6 hours on 6
    cores, about 40 minutes on 72.
  - Built from `android-latest-release` (aosp-main has been read-only since
    2025-03-27).
- **Kernel:** prebuilt GKI android15-6.6, with the sources published per the GPL.
  The minimal 6.18 of ADR 0008 is not enough: binder, eBPF, dm-verity and
  more are missing.

## Recommendation
1. `guest/aosp` = fork of `vsoc_arm64_only`, built on the dedicated Linux
   x86_64 machine. Changes:
   - `virtio_mmio.ko` in the first stage, `boot_devices` on mmio;
   - SwiftShader + drm_hwcomposer + minigbm;
   - adbd over TCP;
   - vbmeta disabled (userdebug);
   - ART ISA variant cortex-a53;
   - microG with restricted spoofing.
2. Before building:
   - boot the API 35 emulator image (local only) and the repackaged
     Cuttlefish under `qemu-system-aarch64 -M virt -cpu cortex-a53 -smp 1`,
     to confirm ISA and devices;
   - extend the loader (ADR 0008) to boot.img v4 + vendor_boot +
     bootconfig.
3. Evaluate with an ADR a PCIe ECAM host (virtio-pci, INTx): it would make
   the Cuttlefish images bootable without modifying them.

## Risks
- **Devices:** PCI versus mmio in the first stage of init; the
  bootconfig/AVB/boot_devices chain is fragile.
- **Resources:** guest RAM of 2–3 GB (memory64 in the browser); first boot with
  a long dexopt.
- **Self-modifying code:** eBPF JIT and ART JIT, handled by the M4
  invalidation.
- **Legal:** the SDK image is not redistributable; the GKI sources must be
  published.

## Not verified
- Sizes and terms of the artifacts on ci.android.com.
- Boot with a single CPU.
- VINTF with kernels newer than 6.6.
- ART/dex2oat under cortex-a53.

Sources: links in the original report, including source.android.com
(Cuttlefish, 16KB, CDD 15, requirements), android.googlesource.com
(`device/google/cuttlefish`, `device/generic/goldfish`, `art`,
`build/soong`), github.com/microg/GmsCore.
