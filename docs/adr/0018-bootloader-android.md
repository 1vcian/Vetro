# ADR 0018 — Vetro's Android bootloader: boot.img, vendor_boot, init_boot, bootconfig

- Status: accepted (M5, 2026-09-25). Extends ADR 0008 (direct boot).

## Context
Android images from GKI onwards (Cuttlefish, our `guest/aosp`) do not
provide an `Image` and an initrd, but `boot.img` (kernel, header v4),
`vendor_boot.img` (vendor ramdisk in fragments, vendor command line,
bootconfig section) and `init_boot.img` (generic ramdisk). Between the
images and the kernel there is a bootloader (Cuttlefish's u-boot, phones'
ABL) that combines them; QEMU does not do this: it wants
`-kernel/-initrd/-append`. Vetro already has direct boot with the QEMU
layout (ADR 0008) and must stay comparable with the oracle.

## Decision
- **The bootloader is a pure module of `vetro-machine`**
  (`vetro_machine::android`), in front of the M3 loader: it produces
  `Image`, initrd and command line, then `Machine::load_android` =
  `load_linux`. No firmware in the guest, no layout different from QEMU:
  the same three pieces (written by `vetro boot --android-dump`) go to
  `qemu-system-aarch64 -kernel -initrd -append`, and that is the
  comparison with the oracle. In the browser the module runs the same (no
  dependencies).
- **Our own decompressors, without dependencies** (gzip with CRC32, LZ4
  legacy and frame), only for the kernel: `vetro-machine` compiles for
  wasm32 and the core stays without external crates. The ramdisks are not
  touched: the kernel opens them, as on a phone.
- **Ramdisk order:** vendor fragments in table order, skipping the
  recovery ones (unless `--recovery`), then the generic one (`init_boot`
  if present), without alignment: this is the source.android.com example
  for normal boot.
- **Bootloader parameters** (`--append`): with `vendor_boot` v4 the
  `androidboot.*` go into bootconfig, after the vendor section (that is
  where the bootloader adds parameters known only at boot), the others at
  the end of the command line (`boot`, `vendor`, bootloader, like u-boot).
  Without v4 everything goes on the command line. The `androidboot.*`
  already written in the images' command lines are left there: they are a
  build choice (AOSP's incremental migration keeps them in both).
- **`bootconfig` on the command line:** AOSP puts it in the build
  (`BOARD_KERNEL_CMDLINE += bootconfig`); Vetro adds it if there is a
  block and the line does not contain it, otherwise the `androidboot.*`
  we moved would silently disappear. The guest kernel has
  `CONFIG_BOOT_CONFIG=y` without `FORCE`, like GKI.
- **Block format** that of the kernel's `tools/bootconfig -a` (text,
  NUL, padding to the initrd's 4-byte alignment, size, checksum, magic):
  verified byte for byte with the tool built from the guest kernel
  sources. Values in quotes (commas, `#`, `;` would otherwise remain
  syntax); repeated keys in the parameters rejected (the kernel would
  discard the whole block). A parameter key that repeats a key of the
  vendor section is not checked: the kernel rejects the block and says so
  in the log (`Failed to parse bootconfig: Value is redefined`, verified
  with `tools/bootconfig`).
- **AOSP's mkbootimg as the test reference**, unmodified copy in
  `tools/mkbootimg/` at a pinned commit (git blob and sha256 in the
  README): the test images are those AOSP produces, not those we believe
  it produces. Only `python3` is needed; in CI (`VETRO_REQUIRE_ORACLE=1`)
  its absence makes the tests fail.
- **Ignored** is whatever virt does not use: load addresses, the images'
  DTB (Vetro generates its own, like QEMU), `second`, `recovery_dtbo`,
  GKI signature and AVB (no verification: Vetro's images are userdebug
  with vbmeta disabled, `m5-android-images.md`).

## Consequences
- `vetro boot --boot-img/--vendor-boot/--init-boot` boots the images of a
  GKI build without manual steps; `--android-dump` gives the files for
  QEMU.
- The guest kernel changes configuration (`BOOT_CONFIG`, `RD_LZ4`): the CI
  cache renews itself (key on `guest/kernel/config/**`), the QEMU
  reference log is regenerated.
- Choosing vendor fragments by `board_id`, verifying AVB and the GKI
  signature stay out: if needed, a new ADR.
