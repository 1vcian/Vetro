#
# Vetro's own parts of the product, on top of Cuttlefish's vendor.
#

PRODUCT_SOONG_NAMESPACES += device/vetro/vetro_arm64

# fstab of the virt machine: in vendor (second stage) and in the
# vendor_ramdisk (first stage, first_stage_ramdisk/fstab.vetro). Chosen by
# androidboot.fstab_suffix.
PRODUCT_PACKAGES += \
    fstab.vetro \
    fstab.vetro.vendor_ramdisk

# Hardware the virt machine doesn't have (ADR 0037, "Idle guest"): UWB,
# Thread and NFC. Cuttlefish's HALs for them talk to the host (/dev/hvcN,
# vsock) and abort; init.vetro.rc stopped their restarts, but their clients
# (UwbService in system_server, ot-daemon, the NFC app) then asked
# servicemanager for them once a second forever. VetroMissingHardware
# (Android.mk) removes the HAL APEXes (LOCAL_OVERRIDES_MODULES) and declares
# the features unavailable, so the framework starts no service for them.
PRODUCT_PACKAGES += VetroMissingHardware

# Slim image (ADR 0041): apps, HALs and framework services an emulator in a
# browser tab does not need (VetroSlim in Android.mk, vetro_slim.xml), and the
# on-device personalization system service off (its standard switch).
PRODUCT_PACKAGES += VetroSlim
PRODUCT_SYSTEM_PROPERTIES += \
    ro.system_settings.service.odp_enabled=false

# Cuttlefish's audio policy (shared/config/audio/policy/
# audio_policy_configuration.xml) always includes the Bluetooth audio policy,
# but with BOARD_HAVE_BLUETOOTH := false shared/bluetooth/device_vendor.mk no
# longer installs it: the audio HAL then never registers IModule/default,
# audioserver waits for it and system_server aborts in
# ExternalCaptureStateTracker (a framework restart every ~400 s, measured).
# The file (AOSP's own) is installed anyway; its modules stay unused.
PRODUCT_COPY_FILES += \
    frameworks/av/services/audiopolicy/config/bluetooth_audio_policy_configuration_7_0.xml:$(TARGET_COPY_OUT_VENDOR)/etc/bluetooth_audio_policy_configuration_7_0.xml

# The board's init and ueventd.
PRODUCT_COPY_FILES += \
    device/vetro/vetro_arm64/init.vetro.rc:$(TARGET_COPY_OUT_VENDOR)/etc/init/init.vetro.rc

# Low-RAM "Go" defaults switchable at boot (androidboot.vetro.profile=go,
# ADR 0037); the vetro_arm64_go product sets them at build time.
PRODUCT_PACKAGES += init.vetro-go.rc

# Network: eth0 stays eth0 and EthernetService manages it with DHCP (Vetro's
# stack or QEMU's slirp). Cuttlefish renames it and goes through the
# simulated Wi-Fi towards an OpenWRT on the host, which isn't there.
PRODUCT_VENDOR_PROPERTIES += \
    ro.vendor.disable_rename_eth0=1

# adbd on TCP 5555 without authorization: ONLY in the userdebug development
# build (Cuttlefish does the same: shared/device.mk already sets
# persist.adb.tcp.port=5555 and ro.adb.secure=0). A build for users must drop
# ro.adb.secure=0 and use adb keys (ADR 0022).
PRODUCT_SYSTEM_EXT_PROPERTIES += \
    ro.adb.secure=0

# No lock screen at first boot (LockSettingsService): straight to the home
# screen, as the analysis needs.
PRODUCT_PRODUCT_PROPERTIES += \
    ro.lockscreen.disable.default=true

# ART: Vetro's CPU = Cortex-A53, ARMv8.0 + CRC32 + crypto (ADR 0005).
# TARGET_CPU_VARIANT := cortex-a53 (Cuttlefish's BoardConfig) makes the build
# write dalvik.vm.isa.arm64.variant=cortex-a53 and features=default in
# vendor/build.prop; tools/aosp/fetch.sh checks it on the artifacts. With
# newer variants the JIT would emit LSE and FP16, which the CPU doesn't have.

# First boot on one slow emulated CPU (measured under QEMU, docs/progress/M5.md,
# 2026-09-27): from boot_completed to the launcher drawn every process competes
# for the CPU, and the preopted apps are only "verify" (no profile), so
# SystemUI, Launcher3 and Settings (FallbackHome) run interpreted and feed
# ART's JIT. AOT-compiled ("speed") at build time instead: their code comes
# from the odex in /system_ext.
PRODUCT_DEXPREOPT_SPEED_APPS += \
    SystemUI \
    Launcher3QuickStep \
    Settings
# ArtService's first-boot dexopt verifies, before the system is ready, the
# apps inside APEXes (the build can't preopt them: MediaProvider, Bluetooth,
# PermissionController, AdServices...): about 220 s of guest time under QEMU.
# Skipped: those apps verify their classes at run time when they load them.
PRODUCT_SYSTEM_PROPERTIES += \
    pm.dexopt.first-boot=skip

# microG (GmsCore and Companion) as privileged apps, with signature spoofing
# limited to their certificates (guest/aosp/patches/frameworks/base).
PRODUCT_PACKAGES += \
    VetroGmsCore \
    VetroGmsCompanion \
    privapp-permissions-vetro-microg.xml \
    default-permissions-vetro-microg.xml \
    sysconfig-vetro-microg.xml

# Vetro's development CA (ADR 0030): no module here. The certificate enters
# the system trust store through two patches (guest/aosp/patches/external/
# conscrypt and system/ca-certificates, tools/aosp/dev-ca.sh), because AOSP 15
# reads the CAs from /apex/com.android.conscrypt/cacerts and the APEX is built
# from its own project.

# Trademarks (ADR 0030): the product never presents itself as "Android".
# - static overlays: default app icon, package installer icon and Browser2's
#   icon (the browser in Launcher3's hotseat) without the robot;
#   VetroFrameworkOverlay removes QuickSearchBox (the "Google" widget on
#   Launcher3's home screen) and says "Vetro is starting…" instead of "Phone
#   is starting…"; VetroSettingsOverlay drops FallbackHome's animated
#   progress bar (it burns CPU while the first boot finishes, ADR 0032);
# - our default wallpaper (WallpaperManager.openDefaultWallpaper reads
#   ro.config.wallpaper first), generated by tools/aosp/wallpaper.py;
# - ro.product.system.* like the other partitions (generic_system.mk sets
#   Android/mainline/generic, meant for the GSI).
PRODUCT_PACKAGES += \
    VetroFrameworkOverlay \
    VetroPackageInstallerOverlay \
    VetroBrowserOverlay \
    VetroSettingsOverlay

# Idle guest (ADR 0040): SettingsProvider defaults (screen on while powered,
# no auto-rotation) and, in VetroFrameworkOverlay, no flip-to-screen-off:
# both kept an accelerometer listener sampling forever.
PRODUCT_PACKAGES += VetroSettingsProviderOverlay
PRODUCT_COPY_FILES += \
    device/vetro/vetro_arm64/branding/wallpaper.png:$(TARGET_COPY_OUT_PRODUCT)/media/wallpaper/vetro.png
PRODUCT_PRODUCT_PROPERTIES += \
    ro.config.wallpaper=/product/media/wallpaper/vetro.png

# File manager daemon (ADR 0020): vsock, port 5200, only in development
# builds (vetro-files/vetro-files.rc). The source comes from
# guest/kernel/initramfs through tools/aosp/sync.sh.
PRODUCT_PACKAGES += vetro-files
