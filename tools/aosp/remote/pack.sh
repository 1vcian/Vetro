#!/bin/bash
# Sulla VM di build (lanciato da tools/aosp/fetch.sh): raccoglie in
# ~/$WORK/out gli artefatti della build da portare sul Mac. Solo copie: il
# disco si compone sul Mac (tools/aosp/mkdisk.sh), così la VM resta accesa il
# meno possibile. vbmeta non serve: il fstab non chiede AVB e il bootloader di
# Vetro (ADR 0018) e QEMU non lo leggono.
set -euo pipefail
cd
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
product="${VETRO_AOSP_PRODUCT:-vetro_arm64}"
p="$tree/out/target/product/$product"
o="$work/out"

# Controlli sul risultato prima di copiare (ADR 0030): se uno manca la build
# non è quella voluta e fetch.sh si ferma.
fail() { echo "ERRORE: $*" >&2; exit 1; }
# CA di sviluppo: il nome <hash>.0 viene dalla patch di conscrypt.
ca="$(sed -n 's|^+++ b/apex/ca-certificates/files/||p' "$work"/patches/external/conscrypt/*.patch)"
[ -n "$ca" ] || fail "nessuna CA di sviluppo nelle patch di external/conscrypt"
capex="$p/system/apex/com.android.conscrypt.capex"
[ -f "$capex" ] || capex="$p/system/apex/com.android.conscrypt.apex"
# apex_build_info.pb elenca i file del payload (canned_fs_config). Niente
# `| grep -q`: con pipefail il SIGPIPE di unzip farebbe fallire il controllo.
info="$(unzip -p "$capex" apex_build_info.pb | tr -c '[:print:]' '\n')"
case "$info" in
  *"/cacerts/$ca"*) ;;
  *) fail "$ca non è nell'APEX di conscrypt ($capex)" ;;
esac
[ -f "$p/system/etc/security/cacerts/$ca" ] || fail "$ca non è in /system/etc/security/cacerts"
# Marchi: overlay installati, QuickSearchBox no, sfondo e sua proprietà.
for f in product/overlay/VetroFrameworkOverlay.apk product/overlay/VetroPackageInstallerOverlay.apk product/media/wallpaper/vetro.png; do
  [ -f "$p/$f" ] || fail "manca /$f"
done
[ ! -e "$p/product/app/QuickSearchBox" ] || fail "QuickSearchBox è ancora in /product/app"
grep -qx 'ro.config.wallpaper=/product/media/wallpaper/vetro.png' "$p/product/etc/build.prop" || fail "manca ro.config.wallpaper in product/etc/build.prop"
grep -qx 'ro.product.system.brand=Vetro' "$p/system/build.prop" || fail "ro.product.system.brand non è Vetro"
rm -rf "$o"
mkdir -p "$o/props"
for f in boot.img vendor_boot.img init_boot.img super.img userdata.img; do
  cp --sparse=always "$p/$f" "$o/$f"
done
# Proprietà per i controlli sul Mac (variante ISA di ART, fingerprint...).
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
