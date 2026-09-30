# ADR 0040 — An idle guest on the home screen

- Status: accepted (M5/M6, 2026-09-30). Builds on ADR 0022 (AOSP image),
  0032 (ranchu HWC patches), 0037 (the prepared idle changes and the Go
  profile), 0039 (the light profile and responsiveness). Details:
  `docs/specs/guest-image.md`, "Idle guest"; measurements:
  `docs/progress/M5.md` (2026-09-30).

## Context
ADR 0039 found that a tap in the app takes seconds because the guest never
rests: after a restore its load average is 20–28 and the one emulated CPU is
always busy. ADR 0037 prepared some image changes from a first profile
(no Bluetooth, no proactive compaction, the Go profile) without building
them. This ADR measures the image on a settled home screen and removes what
keeps the CPU busy.

**Method** (`tools/aosp/qemu-vm.sh idle`, `remote/idle.sh`): QEMU 10 on the
build VM (TCG, one vCPU, guest time = real time), the app's `light` machine
(960x600 at 180 dpi, 2 GiB, `ANDROID_GRAPHICS.light`, screen kept on), first
boot. The script waits for the launcher focused, then at least 600 s and
until no dumpstate/dex2oat/crash_dump runs, then samples 120 s of guest time
with one adb command (`/proc/stat` and every `/proc/PID/stat` before and after,
`/proc/loadavg` every 10 s; `dumpsys cpuinfo` and `top` after). The sampler
itself costs about 4% (its `/proc` walk). A second sample at the same guest
time (~2550 s) shows the steady state.

**Image 64fcd35 (before)**: 10 min after the launcher, **0% idle**, load 21;
at ~2550 s, **48% idle**, load 1.7–5.9. The steady-state consumers were not
apps but polling loops:
- `servicemanager` 7%, `init` 4%, `logd` 2%, part of `system_server`: four
  clients asking once a second, forever, for HALs that never register: UWB
  (`UwbService`), Thread (`ot-daemon`), NFC (the NFC app) and the radio
  (`com.android.phone`, `IRadioModem/slot1`). Their Cuttlefish HALs talk to
  the host over `/dev/hvcN` or vsock and abort; init.vetro.rc had only
  stopped their restarts.
- the ranchu HWC 8% and SurfaceFlinger 7% on an unchanging screen:
  SurfaceFlinger kept hardware vsync on forever, so the HWC's vsync thread
  called `onVsync` over binder 75 times a second. The HWC declares
  `PRESENT_FENCE_IS_NOT_RELIABLE` and Cuttlefish's `init_graphics.vendor.rc`
  sets `debug.sf.vsync_reactor_ignore_present_fences=true`; either makes
  `VSyncReactor` "keep HWVSync on as long as we ignore present fences".
- accelerometer listeners sampling a still, emulated sensor: auto-rotation
  (`WindowOrientationListener`, 15 Hz, also kept for the rotation-suggestion
  button when rotation is locked) and flip-to-screen-off
  (`FaceDownDetector`, 5 Hz): SensorService, `android.ui` and the sensors HAL,
  about 4%.

## Decision
Remove, with the standard switches where they exist, what the virt machine
does not have and what polls on an idle screen:
1. **No modem**: `TARGET_NO_TELEPHONY := true` (Cuttlefish's own switch, used
   by its automotive products): no `rild`, none of the telephony features its
   APEX declares, so `PhoneGlobals` creates no phone.
2. **No UWB, Thread, NFC**: `VetroMissingHardware` declares the features
   unavailable (`<unavailable-feature>`, the mechanism of
   `aosp_excluded_hardware.xml`) and removes the HAL APEXes and the Thread
   demo app with `LOCAL_OVERRIDES_MODULES`. It is a make module
   (`Android.mk`): a Soong `prebuilt_etc`'s `overrides` never reaches make in
   AOSP 15 (the first build still had the APEXes), so `VetroGoRemovals`
   moved there too.
3. **No Bluetooth** (ADR 0037) — but Cuttlefish's audio policy always
   includes `bluetooth_audio_policy_configuration_7_0.xml`, which
   `BOARD_HAVE_BLUETOOTH := false` stops installing: the audio HAL never
   registered `IModule/default` and `system_server` aborted in
   `ExternalCaptureStateTracker`, a framework restart every ~400 s. The file
   (AOSP's) is installed by our device.mk.
4. **Vsync off when idle**: the HWC patch `0002` no longer declares
   `PRESENT_FENCE_IS_NOT_RELIABLE` (its present fence is the DRM atomic
   commit's `OUT_FENCE`, signalled by the kernel when virtio-gpu has flushed;
   `androidboot.vetro.hwc_present_fence=unreliable` restores upstream), and
   init.vetro.rc sets `debug.sf.vsync_reactor_ignore_present_fences=false`
   at `on init` (after Cuttlefish's early-init). `0003` makes the HWC's vsync
   thread sleep on a condition variable while vsync is disabled instead of
   waking at every period.
5. **No sensor-driven rotation or flip**: `config_supportAutoRotation=false`
   and `config_flipToScreenOffEnabled=false` (VetroFrameworkOverlay),
   `def_accelerometer_rotation=false` (new VetroSettingsProviderOverlay,
   above Cuttlefish's SettingsProvider overlay). The display is the host's
   window; apps that request a fixed orientation still get it.
6. **Screen on** comes from `def_stay_on_while_plugged_in=true` (in the same
   SettingsProvider overlay, as Cuttlefish already did) and the health HAL's
   AC power; the `exec_background … settings put` in init.vetro.rc, which
   never ran (init cannot run `settings`), is gone.

Also from ADR 0037, now built: `vm.compaction_proactiveness=0`,
`vm.watermark_boost_factor=0`. `tools/aosp/remote/pack.sh` checks 1–5 on
every build.

## Results (same method, image 8b519e5)
See `docs/progress/M5.md` for the table. Before/after at the steady state:
idle 48% → see M5.md; 10 minutes after the launcher: 0% → 65% (image
45df035, which lacks only item 5's rotation part); load 21 → 2.5. What is
left right after the first boot is the first media scan (MediaProvider,
mediaserver, media.extractor over the preinstalled sounds), which ends by
itself; the prebuilt snapshot is taken after it (`--settle`).

## Rejected
- Removing TeleService, Dialer and the rest of the telephony apps: they are
  part of the system image's handheld set; without the feature they idle.
- Stopping the pollers with more `on property:…=restarting` stops: the
  clients poll, not the HALs; only removing the feature stops the clients.
- Patching SurfaceFlinger: the property and the HWC capability are the
  intended knobs.
- A lower refresh rate for the virtual display: the mode comes from the host's
  EDID (QEMU 75 Hz); with vsync off it no longer matters when idle.

## Consequences
- A new image version; the prebuilt snapshots must be regenerated for it.
- The image has no telephony, UWB, Thread, NFC or Bluetooth features: apps
  that require them are not installable (as on a Wi-Fi-only tablet), which
  is what the virt machine offers anyway.
- SurfaceFlinger's vsync model now uses present fences; frame pacing is
  checked by the tap-to-frame numbers of the Chrome test.
