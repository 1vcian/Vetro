#!/bin/bash
# On the build VM (launched by tools/aosp/fetch.sh): collects in ~/$WORK/out
# the build's artifacts to bring to the Mac. Only copies: the disk is composed
# on the Mac (tools/aosp/mkdisk.sh) or next to QEMU on the VM
# (tools/aosp/remote/qemu.sh). vbmeta isn't needed: the fstab doesn't ask for
# AVB and Vetro's bootloader (ADR 0018) and QEMU don't read it.
set -euo pipefail
cd
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
product="${VETRO_AOSP_PRODUCT:-vetro_arm64}"
p="$tree/out/target/product/$product"
o="${VETRO_PACK_OUT:-$work/out}"  # another directory: e.g. to keep the published image's pack

# Checks on the result before copying (ADR 0030): if one fails the build isn't
# the intended one and fetch.sh stops.
fail() { echo "ERROR: $*" >&2; exit 1; }
# Development CA: the <hash>.0 name comes from the conscrypt patch.
ca="$(sed -n 's|^+++ b/apex/ca-certificates/files/||p' "$work"/patches/external/conscrypt/*.patch)"
[ -n "$ca" ] || fail "no development CA in the external/conscrypt patches"
capex="$p/system/apex/com.android.conscrypt.capex"
[ -f "$capex" ] || capex="$p/system/apex/com.android.conscrypt.apex"
# apex_build_info.pb lists the payload's files (canned_fs_config). No
# `| grep -q`: with pipefail unzip's SIGPIPE would fail the check.
info="$(unzip -p "$capex" apex_build_info.pb | tr -c '[:print:]' '\n')"
case "$info" in
  *"/cacerts/$ca"*) ;;
  *) fail "$ca is not in the conscrypt APEX ($capex)" ;;
esac
[ -f "$p/system/etc/security/cacerts/$ca" ] || fail "$ca is not in /system/etc/security/cacerts"
# Trademarks: overlays installed, no QuickSearchBox, wallpaper and its property.
for f in product/overlay/VetroFrameworkOverlay.apk product/overlay/VetroPackageInstallerOverlay.apk product/overlay/VetroBrowserOverlay.apk product/overlay/VetroSettingsOverlay.apk product/overlay/VetroSettingsProviderOverlay.apk product/media/wallpaper/vetro.png; do
  [ -f "$p/$f" ] || fail "/$f missing"
done
[ ! -e "$p/product/app/QuickSearchBox" ] || fail "QuickSearchBox is still in /product/app"
grep -qx 'ro.config.wallpaper=/product/media/wallpaper/vetro.png' "$p/product/etc/build.prop" || fail "ro.config.wallpaper missing in product/etc/build.prop"
grep -qx 'ro.product.system.brand=Vetro' "$p/system/build.prop" || fail "ro.product.system.brand is not Vetro"
# First boot (docs/progress/M5.md, 2026-09-27): SystemUI, Launcher3 and
# Settings AOT-compiled, no first-boot dexopt, uncompressed APEXes.
for a in SystemUI Launcher3QuickStep Settings; do
  odex="$p/system_ext/priv-app/$a/oat/arm64/$a.odex"
  [ -f "$odex" ] || fail "$odex missing"
  # The odex key-value store has "compiler-filter\0<filter>". strings in a
  # group with || true: with pipefail awk's early exit (SIGPIPE) would fail.
  filter="$({ strings -n 3 "$odex" || true; } | awk 'f { print; exit } /^compiler-filter$/ { f = 1 }')"
  [ "$filter" = speed ] || fail "$a.odex compiled with '$filter', not speed"
done
grep -qx 'pm.dexopt.first-boot=skip' "$p/system/build.prop" || fail "pm.dexopt.first-boot is not skip"
if ls "$p"/system/apex/*.capex >/dev/null 2>&1; then fail "compressed APEXes in /system/apex"; fi
# HWC: BGRA framebuffer (guest/aosp/patches/device/generic/goldfish-opengl).
bootconfig="$(strings -a "$p/vendor_boot.img")"
case "$bootconfig" in
  *androidboot.hardware.hwcomposer.display_framebuffer_format=bgra*) ;;
  *) fail "display_framebuffer_format is not bgra in vendor_boot" ;;
esac
# Idle guest (ADR 0040): no HALs for hardware the virt machine lacks, the
# features declared unavailable, the Bluetooth audio policy the audio HAL
# includes, SurfaceFlinger allowed to use present fences.
for a in com.android.hardware.uwb com.android.hardware.threadnetwork com.google.cf.nfc com.google.cf.rild com.google.cf.bt; do
  [ ! -e "$p/vendor/apex/$a.apex" ] || fail "/vendor/apex/$a.apex is still installed"
done
[ -f "$p/vendor/etc/permissions/vetro_missing_hardware.xml" ] || fail "vetro_missing_hardware.xml missing"
[ -f "$p/vendor/etc/bluetooth_audio_policy_configuration_7_0.xml" ] || fail "the Bluetooth audio policy the audio HAL includes is missing"
grep -q 'setprop debug.sf.vsync_reactor_ignore_present_fences false' "$p/vendor/etc/init/init.vetro.rc" || fail "init.vetro.rc does not turn present fences back on"
# Slim image (ADR 0043): a sample of each removal group (telephony, printing,
# backup, demo apps, Cuttlefish's host service, product apps, vendor HALs)
# and the unavailable features.
# (APKs, not their directories: an app with JNI keeps a lib/ symlink there.)
for f in system/priv-app/TeleService/TeleService.apk system/app/PrintSpooler/PrintSpooler.apk \
    system/priv-app/LocalTransport/LocalTransport.apk system/app/Traceur/Traceur.apk \
    system_ext/priv-app/CarrierConfig/CarrierConfig.apk product/priv-app/Dialer/Dialer.apk \
    product/app/messaging/messaging.apk product/app/Camera2/Camera2.apk \
    vendor/priv-app/CuttlefishService/CuttlefishService.apk vendor/apex/com.google.emulated.camera.provider.hal.apex \
    vendor/apex/com.android.hardware.neuralnetworks.apex vendor/bin/hw/android.hardware.biometrics.face-service.default; do
  [ ! -e "$p/$f" ] || fail "/$f is still installed"
done
[ -f "$p/vendor/etc/permissions/vetro_slim.xml" ] || fail "vetro_slim.xml missing"
grep -qx 'ro.system_settings.service.odp_enabled=false' "$p/system/build.prop" || fail "on-device personalization is not off"
rm -rf "$o"
mkdir -p "$o/props"
for f in boot.img vendor_boot.img init_boot.img super.img userdata.img; do
  cp --sparse=always "$p/$f" "$o/$f"
done
# Properties for the checks on the Mac (ART's ISA variant, fingerprint...).
for part in system vendor product system_ext odm; do
  for f in "$p/$part/build.prop" "$p/$part/etc/build.prop"; do [ -f "$f" ] && cp "$f" "$o/props/$part.build.prop"; done
done
[ -f "$p/system/etc/build.prop" ] && cp "$p/system/etc/build.prop" "$o/props/system.etc.build.prop"
cp "$p/vendor_ramdisk/first_stage_ramdisk/fstab.vetro" "$o/props/" 2>/dev/null || true
[ -f "$p/vendor_ramdisk/lib/modules/modules.load" ] && cp "$p/vendor_ramdisk/lib/modules/modules.load" "$o/props/vendor_ramdisk.modules.load"
{
  echo "vetro_rev=$(cat "$work/build.rev" 2>/dev/null || echo sconosciuta)"  # value read by upload.sh
  echo "dev_ca=$ca"
  echo "build_id=$(sed -n 's/^BUILD_ID=//p' "$tree/build/make/core/build_id.mk")"
  echo "manifest_tag=$(cd "$tree/.repo/manifests" && git describe --tags --always 2>/dev/null || true)"
  echo "kernel=$(strings -a "$tree/kernel/prebuilts/6.6/arm64/kernel-6.6" | grep -m1 '^Linux version')"
  echo "kernel_build_id=$(tr -d ' \n' < "$tree/kernel/prebuilts/6.6/arm64/prebuilt-info.txt")"
  m="$tree/kernel/prebuilts/common-modules/virtual-device/6.6/arm64/virtio_mmio.ko"
  echo "modules_vermagic=$(strings -a "$m" | sed -n 's/^vermagic=//p' | head -n1)"
  echo "modules_scmversion=$(strings -a "$m" | sed -n 's/^scmversion=//p' | head -n1)"
  for d in device/google/cuttlefish device/generic/goldfish-opengl kernel/prebuilts/6.6/arm64 kernel/prebuilts/common-modules/virtual-device/6.6/arm64 frameworks/base; do
    echo "git:$d=$(git -C "$tree/$d" rev-parse HEAD)"
  done
} > "$o/build-info.txt"
(cd "$o" && find . -type f ! -name SHA256SUMS | sort | xargs sha256sum > SHA256SUMS)
du -sh "$o"
