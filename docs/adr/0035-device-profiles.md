# ADR 0035 — Device profiles without an AOSP rebuild

- Status: accepted (M10, 2026-09-27). Builds on ADR 0018 (Android
  bootloader, bootconfig), 0022/0030/0032 (the `vetro_arm64` image), 0028
  (AOSP in the browser), 0031 (prebuilt snapshot). Details:
  `docs/specs/device-profiles.md`; user guide: `docs/user/device-profiles.md`.

## Context
M10 lists "device profiles": what the machine shows the guest (screen,
density, memory, locale, device strings) should be a choice, not a constant.
Until now the app had one machine (`ANDROID_MACHINE`: 1280x800, 2 GiB) and
one parameter line (`ANDROID_PARAMS`), and the image's vendor bootconfig
fixed the density (240) and the serial (`VETRO00001`). The image cannot be
rebuilt for this (and never in CI); profiles must use only what the current
image and machine already support.

What the current image reads, checked in the AOSP tree on the build VM
(`android-15.0.0_r36`, Cuttlefish device files):
- `init_graphics.vendor.rc` copies `ro.boot.lcd_density` to
  `ro.sf.lcd_density` at early-init: the density can come from the
  bootloader (`androidboot.lcd_density`).
- init's `ExportKernelBootProps` maps `ro.boot.serialno` to `ro.serialno`
  (`Build.getSerial()`); `Build.SKU` is `ro.boot.hardware.sku`.
- The ranchu HWC (`display_finder_mode=drm`) takes the display size from the
  DRM connector, i.e. from virtio-gpu's scanout size and EDID preferred mode:
  the screen size is a machine setting.
- `ro.product.{model,brand,manufacturer,device,name}` come from the
  partitions' build.prop (`ro.product.property_source_order`); init has no
  `ro.boot.*` override for them.
- The locale is `persist.sys.locale`, falling back to the build's
  `ro.product.locale`; the time zone is `persist.sys.timezone`. No
  `ro.boot.*` sets either; both can be set at run time from adb
  (`cmd alarm set-timezone` is live; the locale is read by
  ActivityTaskManager when the framework starts).
- Bootconfig parameters with the key of a vendor line replace that line in
  place (ADR 0028), so profile values override the image's.

## Decision
1. **A small JSON format, versioned and strict** (`vetroProfile: 1`): id,
   name, description, `screen.{width,height,density}`, `ramMiB`, `locale`,
   `timezone`, `device.{name,serial,sku}`. Unknown fields and out-of-range
   values are errors, and every string is restricted to characters that are
   safe on the kernel command line, in bootconfig and in an `adb shell`
   command. A newer version is refused with "needs a newer Vetro".
2. **Three places, only supported mechanisms:** the machine (RAM, scanout
   size); `androidboot.lcd_density`, `androidboot.serialno` and
   `androidboot.hardware.sku`, emitted only when they differ from the image;
   adb commands after the boot for the time zone, the device name
   (`settings put global device_name`) and the locale
   (`persist.sys.locale`, effective from the next system start). The adb
   commands are idempotent and run after every connection, so they also
   apply after a restore.
3. **The default profile is today's machine.** It emits no parameters and
   no commands: its snapshot key is unchanged, and the prebuilt snapshot of
   ADR 0031 still matches. Other profiles have their own key (it contains
   RAM, screen and parameters), so they cold-boot the first time unless a
   prebuilt snapshot is published for them (`prebuilt-snapshot.mjs
   --profile` makes one); each keeps its own snapshot in OPFS.
4. **Four starter profiles** shipped with the app (`web/app/profiles/`) and
   built into the CLI: `default` (1280x800, 240 dpi, 2 GiB), `phone`
   (720x1280, 320 dpi, 2 GiB), `small-phone` (480x800, 240 dpi, 1.5 GiB),
   `tablet` (1280x800, 213 dpi so the smallest width is 600 dp, 2 GiB). The
   three asked for (phone, small phone, tablet) plus the default machine, so
   the default goes through the same code and stays selectable.
5. **Two twin implementations** (Rust `vetro_machine::profile`, JavaScript
   `web/node/profiles.mjs`) rather than exporting the Rust parser through
   vetro-wasm: the app needs the profile before the machine exists (the
   snapshot key, the RAM), and the rules are small. Both are tested against
   the same expected strings for the starter profiles.
6. **Selection:** the app's "Device profile" menu (and a profile file, and
   `profile=<id>` in the URL); `vetro boot --profile=NAME|FILE` (an explicit
   `--mem` wins; the parameters go after `--append`; the adb commands are
   printed).

## Needs an AOSP rebuild (not in profiles today)
- **Model, brand, manufacturer, device and product names** seen by apps
  (`Build.MODEL` etc.): they are build properties. A rebuild could add a
  vendor init rule mapping e.g. `ro.boot.vetro.model` onto
  `ro.product.vendor.model` before property loading, or ship per-profile
  build properties.
- **Locale from the very first boot**, and a live locale change: needs an
  init rule copying e.g. `ro.boot.vetro.locale` to `persist.sys.locale` when
  it is unset, or a small privileged helper calling
  `updatePersistentConfiguration`.
- **Time zone before the framework starts** (today it is set when adb
  connects, after `sys.boot_completed`): same kind of init rule.
- **Build fingerprint, security patch, `ro.build.*`**: build properties.
- **Telephony identity** (IMEI, operator, phone number, SIM): the
  Cuttlefish RIL simulator values are compiled in.
- **Hardware features** advertised to apps (`/vendor/etc/permissions/*.xml`,
  e.g. a camera, NFC, telephony on a tablet): files in the vendor partition.
- **`ddr_size`** in the bootconfig (3072MB, cosmetic): could be overridden
  today, but changing it would change the default parameters and the
  prebuilt key for no visible effect.

## Consequences
- Portrait screens work in the app: the page fits a tall scanout to the
  window height.
- A profile other than the default costs a cold boot (about 45 minutes) the
  first time, until prebuilt snapshots are published for more profiles
  (0.5–0.8 GB each on R2, depending on the image).
- Profiles change what an app can observe (screen, density, serial, SKU),
  which is also a lever against naive emulator detection; the model name
  remains "Vetro" until the rebuild above.
- Recording and replay are unaffected: RAM and screen are part of the
  machine configuration a log carries, and the app refuses to replay a log
  of a differently configured machine; the boot parameters are in the
  keyframes' guest state.
