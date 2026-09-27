# ADR 0032 — BGRA scanout in the composer and a lighter first boot of the AOSP image

- Status: accepted (M5/M6, 2026-09-27). Builds on ADR 0022 (AOSP image),
  ADR 0028 (Android in the browser) and ADR 0030 (image build). Details:
  `docs/specs/guest-image.md`; measurements: `docs/progress/M5.md`.

## Context
The browser runs of image `…-bd09e2f` (ADR 0028) showed three problems that
belong to the image, all reproduced under QEMU on the build VM with the same
image (`tools/aosp/qemu-vm.sh`, QEMU's monitor `screendump` = what the
scanout shows, `adb screencap` = SurfaceFlinger's picture):
1. **Swapped colours.** The test app's blue (`0x1565c0`) reaches the scanout
   as `0xc06515`; the home screen too (Launcher3's blue camera icon is
   orange), in both composition paths of the ranchu HWC (device and client,
   checked with `androidboot.hardware.hwcomposer.mode=client`). Cause: on
   virtio-gpu without 3D, minigbm allocates every buffer as a DRM dumb
   buffer, and the kernel (`virtio_gpu_mode_dumb_create`) creates the host
   resource as `DRM_FORMAT_HOST_XRGB8888` = `B8G8R8X8`, whatever DRM format
   the framebuffer is later added with. The HWC's swapchain and
   SurfaceFlinger's client target are `RGBA_8888` (SurfaceFlinger hard-codes
   it in `RenderSurface`; `ro.surface_flinger.default_composition_pixel_format`
   only changes RenderEngine, tried: the framebuffer stays format 1), and
   GuestFrameComposer copies RGBA layers byte for byte. QEMU and Vetro both
   honour the resource format, so both show R and B swapped. The bootconfig
   property `display_framebuffer_format` exists (Cuttlefish passes
   `bgra`/`rgba`) but this AOSP 15 hwc3 no longer reads it.
2. **Stale "Phone is starting…".** GuestFrameComposer composes the device
   layers into a swapchain image without clearing it: where no layer is
   drawn (the translucent launcher before the wallpaper is drawn) the image
   keeps an older frame, FallbackHome's.
3. **Slow first boot.** Under QEMU (6.2) on the VM: `boot_completed` at 1118 s of
   guest time, Launcher3 displayed at 1816 s. Nothing is stuck: one emulated
   CPU runs the user unlock (37 s in `UM.onBeforeUnlockUser`, 22 s in the
   services' `onUserUnlocking`), BOOT_COMPLETED receivers, SystemUI's start
   (one CoreStartable alone took 295 s) and Launcher3, all preopted only as
   `verify` (no profiles) and so interpreted and fed to ART's JIT. Before
   `boot_completed`, ArtService's first-boot dexopt verifies the apps inside
   APEXes (220 s) and apexd decompresses ~23 `.capex` into `/data`. And
   FallbackHome's indeterminate progress bar redraws for the whole wait
   (Settings' RenderThread 17% of the CPU in `top`, plus SurfaceFlinger and
   the composer), on the same single CPU.

## Decision
- **The composer converts its frame to BGRA** (patch
  `guest/aosp/patches/device/generic/goldfish-opengl/0001-…`): when
  `ro.vendor.hwcomposer.display_framebuffer_format=bgra` (bootconfig
  `androidboot.hardware.hwcomposer.display_framebuffer_format=bgra`, the
  property Cuttlefish already defines), GuestFrameComposer swaps R and B in
  place (`libyuv::ABGRToARGB`) after composing and before the DRM flush, in
  both paths. One pass over the frame (4 MiB at 1280x800) per presented
  frame. Rejected: client composition (still RGBA), SurfaceFlinger's pixel
  format property (ignored for the display), a patched kernel or minigbm
  (the kernel is the prebuilt GKI; creating 2D resources with their real
  format through the virtgpu ioctl would touch every allocation).
- **Device composition starts from a cleared frame** (same patch): the
  swapchain image is zeroed before the layers are composed, as SurfaceFlinger
  composes uncovered areas (black).
- **First boot:** `PRODUCT_DEXPREOPT_SPEED_APPS += SystemUI Launcher3QuickStep
  Settings` (AOT code in `/system_ext`, bigger odex), `pm.dexopt.first-boot=skip`
  (the APEX apps verify their classes when they load them), and
  `PRODUCT_COMPRESSED_APEX := false` (bigger `super.img`, read on demand by
  the browser; nothing decompressed into `/data`, so less data in the
  browser's copy-on-write overlay and snapshot).
- **FallbackHome without the progress bar**: RRO `VetroSettingsOverlay`
  replaces `layout/fallback_home_finishing_boot` with the still text, and
  `VetroFrameworkOverlay` makes `android_start_title` "Vetro is starting…"
  (English; the translations stay AOSP's).
- `pack.sh` checks all of it on the build's output (odex filter `speed`,
  the property, no `.capex`, `bgra` in vendor_boot).

## Verification
QEMU 10 on the build VM, guest seconds, old image `…-bd09e2f` → new
`…-64fcd35` (numbers and caveats in `docs/specs/guest-image.md`, "Test
status"): first-boot dexopt window 294 → 85 s, `boot_completed` 1085 → 644,
user unlocked 1211 → 696, Launcher3 displayed > 1984 → 1552; no SystemUI ANR;
the scanout (QEMU `screendump`) equals SurfaceFlinger's `screencap`, the test
app's centre is `#1565c0` and `#ef6c00` after a touch; the home screen has no
stale area. What remains after `boot_completed` is SystemUI's start (836 s,
its main thread waiting for the CPU) with Launcher3 drawing after it.

## Consequences
- The scanout of QEMU and Vetro has the right colours; tests that accepted
  both orders (`colorSeen` in `tests/web`) can require RGB once the site uses
  the new image.
- A new image version: the prebuilt browser snapshot (ADR 0031) is keyed to
  the image, so switching the site's default image means regenerating it.
- The patch lives in `device/generic/goldfish-opengl` (Apache 2.0): it is
  published with the other Vetro patches next to the image.
