#!/bin/bash
# On the build VM (launched by tools/aosp/fetch.sh): collects in
# ~/$WORK/out the build artifacts to bring to the Mac. Copies only: the
# disk is assembled on the Mac (tools/aosp/mkdisk.sh), so the VM stays on as
# little as possible. vbmeta is not needed: the fstab does not ask for AVB and
# Vetro's bootloader (ADR 0018) and QEMU do not read it.
set -euo pipefail
cd
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
product="${VETRO_AOSP_PRODUCT:-vetro_arm64}"
p="$tree/out/target/product/$product"
o="$work/out"

# Checks on the result before copying (ADR 0030): if one fails the build
# is not the intended one and fetch.sh stops.
fail() { echo "ERROR: $*" >&2; exit 1; }
# Development CA: the <hash>.0 name comes from the conscrypt patch.
ca="$(sed -n 's|^+++ b/apex/ca-certificates/files/||p' "$work"/patches/external/conscrypt/*.patch)"
[ -n "$ca" ] || fail "no development CA in the external/conscrypt patches"
capex="$p/system/apex/com.android.conscrypt.capex"
[ -f "$capex" ] || capex="$p/system/apex/com.android.conscrypt.apex"
# apex_build_info.pb lists the payload files (canned_fs_config). No
# `| grep -q`: with pipefail unzip's SIGPIPE would make the check fail.
info="$(unzip -p "$capex" apex_build_info.pb | tr -c '[:print:]' '\n')"
case "$info" in
  *"/cacerts/$ca"*) ;;
  *) fail "$ca is not in the conscrypt APEX ($capex)" ;;
esac
[ -f "$p/system/etc/security/cacerts/$ca" ] || fail "$ca is not in /system/etc/security/cacerts"
# Branding: overlays installed, no QuickSearchBox, wallpaper and its property.
for f in product/overlay/VetroFrameworkOverlay.apk product/overlay/VetroPackageInstallerOverlay.apk product/media/wallpaper/vetro.png; do
  [ -f "$p/$f" ] || fail "/$f missing"
done
[ ! -e "$p/product/app/QuickSearchBox" ] || fail "QuickSearchBox is still in /product/app"
grep -qx 'ro.config.wallpaper=/product/media/wallpaper/vetro.png' "$p/product/etc/build.prop" || fail "ro.config.wallpaper missing in product/etc/build.prop"
grep -qx 'ro.product.system.brand=Vetro' "$p/system/build.prop" || fail "ro.product.system.brand is not Vetro"
rm -rf "$o"
mkdir -p "$o/props"
for f in boot.img vendor_boot.img init_boot.img super.img userdata.img; do
  cp --sparse=always "$p/$f" "$o/$f"
done
# Properties for the checks on the Mac (ART ISA variant, fingerprint...).
for part in system vendor product system_ext odm; do
  for f in "$p/$part/build.prop" "$p/$part/etc/build.prop"; do [ -f "$f" ] && cp "$f" "$o/props/$part.build.prop"; done
done
[ -f "$p/system/etc/build.prop" ] && cp "$p/system/etc/build.prop" "$o/props/system.etc.build.prop"
cp "$p/vendor_ramdisk/first_stage_ramdisk/fstab.vetro" "$o/props/" 2>/dev/null || true
[ -f "$p/vendor_ramdisk/lib/modules/modules.load" ] && cp "$p/vendor_ramdisk/lib/modules/modules.load" "$o/props/vendor_ramdisk.modules.load"
{
  echo "vetro_rev=$(cat "$work/build.rev" 2>/dev/null || echo sconosciuta)"
  echo "dev_ca=$ca"
  echo "build_id=$(sed -n 's/^BUILD_ID=//p' "$tree/build/make/core/build_id.mk")"
  echo "manifest_tag=$(cd "$tree/.repo/manifests" && git describe --tags --always 2>/dev/null || true)"
  echo "kernel=$(strings -a "$tree/kernel/prebuilts/6.6/arm64/kernel-6.6" | grep -m1 '^Linux version')"
  echo "kernel_build_id=$(tr -d ' \n' < "$tree/kernel/prebuilts/6.6/arm64/prebuilt-info.txt")"
  m="$tree/kernel/prebuilts/common-modules/virtual-device/6.6/arm64/virtio_mmio.ko"
  echo "modules_vermagic=$(strings -a "$m" | sed -n 's/^vermagic=//p' | head -n1)"
  echo "modules_scmversion=$(strings -a "$m" | sed -n 's/^scmversion=//p' | head -n1)"
  for d in device/google/cuttlefish kernel/prebuilts/6.6/arm64 kernel/prebuilts/common-modules/virtual-device/6.6/arm64 frameworks/base; do
    echo "git:$d=$(git -C "$tree/$d" rev-parse HEAD)"
  done
} > "$o/build-info.txt"
(cd "$o" && find . -type f ! -name SHA256SUMS | sort | xargs sha256sum > SHA256SUMS)
du -sh "$o"
