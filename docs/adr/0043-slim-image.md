# ADR 0043 — A slim image

- Status: accepted (M5/M6, 2026-10-02). Builds on ADR 0022 (AOSP image),
  0033 (app catalog), 0040 (idle guest). Details:
  `docs/specs/guest-image.md`, "Slim image"; measurements:
  `docs/progress/M5.md` (2026-10-02).

## Context
After ADR 0040 the settled home screen is idle, but the image is still
Cuttlefish's phone: a modem stack without a modem, printing, backup, cameras,
biometrics, demo and diagnostic apps, and a dozen vendor HAL services for
hardware the virt machine does not have. On image 8b519e5, 10 minutes after
the launcher, `ps -A` lists 273 processes; among them `com.android.phone`
(persistent), the secure element app, Traceur, the dynamic system updater,
device diagnostics, Camera2, cell broadcast, messaging, IMS entitlement,
calendar, managed provisioning, Cuttlefish's `gceservice`, and the example
HALs for face, fingerprint, camera, consumer IR, context hub, CAS, neural
networks (three processes), tether offload, reboot escrow, secure element,
netlink interceptor, authsecret and secretkeeper. Each costs RAM in the
guest (and so in every snapshot the browser downloads or saves), boot time
on one emulated CPU, and some idle CPU.

The owner's goal: as slim as possible while ordinary apps keep working (the
catalog: Jenny, Chromium, Flowit, Minesweeper; apps installed with adb).

## Decision
Remove with AOSP's own configuration, no source patches:
1. **Vendor composition**: `vetro_vendor.mk` replaces Cuttlefish's
   `shared/phone/device_vendor.mk` with the same inherits minus camera,
   face, fingerprint, consumer IR, identity credential, reboot escrow and
   secure element (and the SIP/VoIP feature file). Same order, same
   `shared/device.mk`.
2. **Product composition**: `handheld_product.mk` instead of
   `aosp_product.mk` (no `telephony_product.mk`: Dialer,
   ImsServiceEntitlement; no messaging, PhotoTable), and AOSP's current
   sound set `AudioPackage14.mk` (57 files) instead of `AllAudio.mk` (220
   files the first media scan reads); no `telephony_system_ext.mk`
   (CarrierConfig, EmergencyInfo).
3. **`VetroSlim`** (Android.mk, the `LOCAL_OVERRIDES_MODULES` mechanism of
   ADR 0040) removes what `generic_system.mk`, `handheld_system.mk`,
   `telephony_system.mk` and Cuttlefish's `shared/device.mk` add: the rest of
   telephony (TeleService, cell broadcast, ONS, STK, MMS, carrier apps,
   QNS, satellite, GBA), printing, backup (transport and agents), Bluetooth
   MIDI, secure element and NFC tag apps, MTP, camera extras, dreams and
   live wallpapers, EasterEgg, Traceur, device diagnostics, dynamic system
   updates, managed provisioning, the accessibility menu, the old browser's
   bookmark providers, Music and Calendar, Cuttlefish's host-service app and
   test servers, and vendor HAL APEXes for hardware the virt machine lacks
   (authsecret, CAS, context hub, neural networks, netlink interceptor,
   tether offload, secretkeeper) or alternatives the bootconfig never selects
   (remote gatekeeper/KeyMint, Trusty KeyMint, drm_hwcomposer, confirmation
   UI).
4. **Features unavailable** (`vetro_slim.xml`, `<unavailable-feature>`):
   print, backup, MIDI, live wallpaper, managed users, SIP, Wi-Fi Direct,
   consumer IR, face, fingerprint, secure element: SystemServer then starts
   no PrintManagerService, BackupManagerService, MidiService, WifiP2pService,
   biometric or IR services, as on the watches and TVs that lack them.
5. `ro.system_settings.service.odp_enabled=false`: the standard switch that
   keeps the on-device personalization system service from starting.
6. Second batch: no `cameraserver` (no camera HAL is left;
   `config.disable_cameraservice=true` keeps SystemServer's CameraServiceProxy
   off, as automotive and TV builds do), no `update_engine` (no OTA: a Vetro
   image is a version replaced whole), no input classifier HAL, no
   screensavers (`config_dreamsSupported=false` in VetroFrameworkOverlay),
   and our own `initial-package-stopped-states-vetro.xml` instead of AOSP's:
   only Settings is exempt from the first-boot "stopped" state, so Contacts
   and DocumentsUI no longer run from their boot receivers until opened.

Kept: everything an ordinary app needs (package, activity and window
managers, input, graphics, network with Ethernet and the Wi-Fi service,
storage, WebView, notifications, the keyboard, the contacts, calendar,
telephony, media and download providers, DocumentsUI, the photo picker, the
permission controller, Telecom), sensors, GNSS, vibrator, audio and DRM,
microG, and every mainline APEX.

## Results
QEMU 10 on the build VM, the app's `light` machine (one vCPU, 2 GiB,
960x600 at 180 dpi), first boot, `tools/aosp/qemu-vm.sh idle` then `apps`
(ADR 0040's method, now also recording RAM, boot time and inventory). Full
table in `docs/progress/M5.md` (2026-10-02):

| | 8b519e5 (before) | batch 1 | batch 2 (f08b79e) |
|---|---|---|---|
| `boot_completed` (guest s) | 552 | 475 | 502 |
| launcher focused (guest s, ±30) | 750 | 584 | 613 |
| RAM used (MemTotal − MemAvailable) | 1037 MiB | 941 MiB | 877 MiB |
| anonymous memory | 513 MiB | 471 MiB | 402 MiB |
| processes (`ps -A`) | 273 | 228 | 223 |
| packages / APEX entries / features | 147 / 156 / 83 | 102 / 124 / 62 | 102 / 122 / 62 |
| `super.img` (sparse) | 1727 MB | 1564 MB | 1557 MB |
| catalog apps installed and opened | 4/4 | 4/4 | 4/4 |

Boot times vary by about ±30 s between runs of the same image; the "before"
run was still doing its first media scan 10 minutes after the launcher (30.6%
idle), the slim runs had finished theirs (88.4–88.8% idle, load 0.2–1.6).

## Rejected
- Removing mainline APEXes (AdServices, on-device personalization, Health
  Connect, rkpd, cell broadcast aside): their jars are on the boot and system
  server classpaths the build preopts against; without them ART rejects the
  preopted boot image and odex files (and SystemServer starts several of
  their services unconditionally). Android has no supported switch.
- Removing the Wi-Fi service (feature `android.hardware.wifi`): apps get a
  null `WifiManager`, and some do not check.
- Removing the USB HAL: small gain, Settings and SystemUI use `UsbManager`.
- Removing binaries that Cuttlefish's init scripts start
  (`socket_vsock_proxy`, `tombstone_transmit`, `metrics_helper`): init
  would restart the missing service every 5 s.
- Fonts and `PRODUCT_LOCALES`: disk only (the browser reads the disk on
  demand) and visible to apps in other scripts.

## Consequences
- A new image version, and new prebuilt snapshots for it.
- Apps that need telephony, printing, backup, MIDI, cameras, biometrics,
  NFC, UWB, Thread or Bluetooth find the feature missing (as on a Wi-Fi
  tablet without those parts).
- `tools/aosp/remote/pack.sh` checks a sample of every removal group.
