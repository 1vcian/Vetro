#!/bin/bash
# Sulla VM di build (lanciato da tools/aosp/fetch.sh): raccoglie in
# ~/$WORK/out gli artefatti della build da portare sul Mac. Solo copie e un
# vbmeta disattivato: il disco si compone sul Mac (tools/aosp/mkdisk.sh),
# così la VM resta accesa il meno possibile.
set -euo pipefail
cd
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
product="${VETRO_AOSP_PRODUCT:-vetro_arm64}"
p="$tree/out/target/product/$product"
host="$tree/out/host/linux-x86/bin"
o="$work/out"
rm -rf "$o"
mkdir -p "$o/props"
for f in boot.img vendor_boot.img init_boot.img super.img vbmeta.img; do
  cp "$p/$f" "$o/$f"
done
# vbmeta con la verifica disattivata (flag 3: hashtree e verifica spenti),
# per chi avvia con un bootloader che legge vbmeta; il bootloader di Vetro
# (ADR 0018) e QEMU lo ignorano, e il fstab non chiede AVB.
"$host/avbtool" make_vbmeta_image --flags 3 --padding_size 4096 --output "$o/vbmeta_disabled.img"
# Proprietà per i controlli sul Mac (variante ISA di ART, fingerprint...).
for part in system vendor product system_ext odm; do
  [ -f "$p/$part/build.prop" ] && cp "$p/$part/build.prop" "$o/props/$part.build.prop"
done
[ -f "$p/system/etc/build.prop" ] && cp "$p/system/etc/build.prop" "$o/props/system.etc.build.prop"
cp "$p/vendor_ramdisk/first_stage_ramdisk/fstab.vetro" "$o/props/" 2>/dev/null || true
[ -f "$p/vendor_ramdisk/lib/modules/modules.load" ] && cp "$p/vendor_ramdisk/lib/modules/modules.load" "$o/props/vendor_ramdisk.modules.load"
{
  echo "build_id=$(sed -n 's/^BUILD_ID=//p' "$tree/build/make/core/build_id.mk")"
  echo "manifest_tag=$(cd "$tree/.repo/manifests" && git describe --tags --always 2>/dev/null || true)"
  echo "kernel=$(strings -a "$tree/kernel/prebuilts/6.6/arm64/kernel-6.6" | grep -m1 '^Linux version')"
  echo "kernel_build_id=$(tr -d ' \n' < "$tree/kernel/prebuilts/6.6/arm64/prebuilt-info.txt")"
  for d in device/google/cuttlefish kernel/prebuilts/6.6/arm64 kernel/prebuilts/common-modules/virtual-device frameworks/base; do
    echo "git:$d=$(git -C "$tree/$d" rev-parse HEAD)"
  done
} > "$o/build-info.txt"
(cd "$o" && find . -type f ! -name SHA256SUMS | sort | xargs sha256sum > SHA256SUMS)
du -sh "$o"
