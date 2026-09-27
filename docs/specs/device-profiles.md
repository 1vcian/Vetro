# Spec — device profiles (`vetro_machine::profile`, `web/node/profiles.mjs`)

Decision: ADR 0035. User documentation: `docs/user/device-profiles.md`.

## Scope
What the machine exposes to the guest, as a small versioned JSON file: the
virtio-gpu scanout size, the screen density, the RAM, the locale and time
zone, and the device strings the image lets the bootloader set. Only
settings the current image (`vetro_arm64`, ADR 0022/0030/0032) and machine
support without an AOSP rebuild; the rest is listed in ADR 0035.

Two implementations of the same rules: `vetro_machine::profile` (Rust, for
`vetro boot --profile`) and `web/node/profiles.mjs` (JavaScript, no Node API:
page, Worker, Node tools). For the same file both give the same machine,
the same `androidboot.*` string and the same adb commands; the tests of
both check the starter profiles against the same expected strings.

## Format (version 1)
A JSON object; unknown fields are errors (so a typo never silently changes
the machine), and so are repeated keys in the Rust parser.

| Field | Type | Required | Rule |
|---|---|---|---|
| `vetroProfile` | integer | yes | `1`; a larger integer = "needs a newer Vetro" |
| `id` | string | yes | `[a-z0-9][a-z0-9-]*`, at most 32 |
| `name` | string | yes | printable ASCII without `"` `$` `'` `\` `` ` `` `|`, 1–40 |
| `description` | string | no | one line, at most 300 |
| `screen.width`, `screen.height` | integer | yes | 320–3840, even |
| `screen.density` | integer | yes | 120–640 (dpi) |
| `ramMiB` | integer | yes | 1024–3072, multiple of 64 |
| `locale` | string | no | `ll[l][-Ssss][-RR or -DDD]` (default `en-US`) |
| `timezone` | string | no | `UTC`, `GMT` or `Area/Location[/Sub]` |
| `device.name` | string | no | like `name` |
| `device.serial` | string | no | `[A-Za-z0-9]{1,20}` (default `VETRO00001`) |
| `device.sku` | string | no | `[A-Za-z0-9._-]{1,32}` |

String rules exclude every character a shell or the bootconfig syntax would
interpret: values go on the kernel command line / bootconfig and into
`adb shell` commands without quoting problems.

## Image defaults
What the image says when the bootloader adds nothing
(`guest/aosp/device/vetro/vetro_arm64/BoardConfig.mk`, product locale):
density 240, serial `VETRO00001`, locale `en-US`. Only values that differ
are emitted, so the `default` profile yields exactly `ANDROID_MACHINE` and
`ANDROID_PARAMS` (`web/node/android.mjs`) and keeps the key of the prebuilt
snapshot (ADR 0031).

## What a profile produces
- **Machine:** RAM `ramMiB`; scanout `width` x `height`
  (`GpuConfig::width/height`, the EDID preferred mode; the ranchu HWC with
  `display_finder_mode=drm` takes it from DRM). Devices unchanged (touchscreen
  in the app).
- **`androidboot.*`** (`Profile::android_params`, `profileAndroidParams`), in
  this order, only when different from the image:
  `androidboot.lcd_density=<density>` (→ `ro.boot.lcd_density` →
  `ro.sf.lcd_density` via Cuttlefish's `init_graphics.vendor.rc`),
  `androidboot.serialno=<serial>` (→ `ro.serialno`, `Build.getSerial()`),
  `androidboot.hardware.sku=<sku>` (→ `Build.SKU`). Vetro's bootloader puts
  them in the bootconfig, replacing the vendor line with the same key in place
  (`docs/specs/android-boot.md`, rule 4). The app's full parameter line is
  `ANDROID_PARAMS` followed by these (`profileBootParams`); `vetro boot`
  appends them after `--append`.
- **adb commands after the boot** (`Profile::adb_commands`,
  `profileAdbCommands`), idempotent, run after every adb connection (the app's
  Worker after `ANDROID_WAKE`; printed by `vetro boot`):
  `cmd alarm set-timezone <tz>` if `timezone`;
  `settings put global device_name '<name>'` if `device.name`;
  `su 0 setprop persist.sys.locale <locale>` if the locale differs from
  `en-US` (read by ActivityTaskManager at framework start: effective from the
  next system start).
- **Check** (`PROFILE_REPORT`, `parseProfileReport`, `profileMismatches`): an
  adb command that prints `size` (`wm size`), `density`
  (`ro.sf.lcd_density`), `serial`, `sku`, `timezone`
  (`persist.sys.timezone`) and `deviceName`, compared with the profile.

## Starter profiles
`web/app/profiles/<id>.json` (served with the app, built into the CLI with
`include_str!`), in menu order:

| id | Screen | Density | RAM | `androidboot.*` |
|---|---|---|---|---|
| `light` | 960x600 | 180 | 2048 | `lcd_density=180` |
| `default` | 1280x800 | 240 | 2048 | (none) |
| `phone` | 720x1280 | 320 | 2048 | `lcd_density=320 serialno=VETROPHONE01 hardware.sku=phone` |
| `small-phone` | 480x800 | 240 | 1536 | `serialno=VETROSMALL01 hardware.sku=small-phone` |
| `tablet` | 1280x800 | 213 | 2048 | `lcd_density=213 serialno=VETROTABLET1 hardware.sku=tablet` |

`phone`, `small-phone` and `tablet` also set `timezone: UTC` and a device
name. The web app selects `light` (`DEFAULT_PROFILE`, ADR 0037); `default` is
the image's own machine (`ANDROID_MACHINE`, `ANDROID_PARAMS`), what `vetro
boot` uses without `--profile`. `PREBUILT_PROFILES` lists the profiles with a
ready-made snapshot for the default image (`light`, `default`).

## Interfaces
- Rust: `Profile::parse(&[u8]) -> Result<Profile, ProfileError>` (`field`,
  `message`), `android_params()`, `adb_commands()`, `apply_gpu(&mut
  GpuConfig)`, `starter(id)`, `load(name_or_path)`, `STARTERS`,
  `PROFILE_VERSION`.
- JS: `parseProfile(textOrObject)` (throws `ProfileError` with `field`),
  `profileAndroidParams`, `profileBootParams`, `profileMachine`,
  `profileAdbCommands`, `profileExpect`, `PROFILE_REPORT`,
  `parseProfileReport`, `profileMismatches`, `profileUrl`,
  `STARTER_PROFILES`, `DEFAULT_PROFILE`, `IMAGE_DEFAULTS`, `LIMITS`.
- CLI: `vetro boot --profile=NAME|FILE` (RAM unless `--mem` is given,
  scanout, `androidboot.*` after `--append` with `--boot-img`, adb commands
  printed on stderr).
- App: "Device profile" menu and "load a profile file" in the AOSP
  fieldset, URL `profile=<id>`; the profile fills screen and RAM; the Worker
  gets `config.android.params` and `config.android.setup`.
  `window.vetroAndroid.state().profile`.
- Tool: `tools/aosp/prebuilt-snapshot.mjs --profile=<id|file>` boots the
  profile's machine in Node, applies the adb commands, checks the report at
  the home screen and writes the scanout as `<key>.png`; the snapshot it
  makes is a valid prebuilt snapshot for that profile.

## Tests
- `vetro_machine::profile` unit tests: starters parse and give the expected
  parameters; the default profile is the image and the app machine; phone
  machine and adb commands; defaults; versions; rejections (field of each
  error).
- `tests/web/unit.mjs` ("device profiles"): the same cases for the JS twin,
  the default profile equal to `ANDROID_MACHINE`/`ANDROID_PARAMS`, the report
  comparison.
- `crates/vetro-cli/tests/boot_android.rs` (release, guest kernel):
  `--profile phone` → `/proc/bootconfig` with the vendor's density and serial
  replaced in place and the SKU appended, 2 GiB of RAM, DRM preferred mode
  720x1280; `--profile` errors and messages.
- On the build VM (not in CI: a cold boot): `prebuilt-snapshot.mjs
  --profile=phone|small-phone|tablet` up to the home screen with a matching
  report (results in `docs/progress/M10.md`).
