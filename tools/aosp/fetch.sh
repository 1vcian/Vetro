#!/bin/sh
# Porta sul Mac gli artefatti dell'ultima build riuscita:
#   VM: tools/aosp/remote/pack.sh raccoglie immagini, proprietà e build-info
#   Mac: target/aosp/out/ (rsync, con SHA256SUMS verificato), poi
#        tools/aosp/mkdisk.sh compone il disco GPT (target/aosp/disk.img)
# Artefatti: boot.img, vendor_boot.img, init_boot.img, super.img (sparso),
# vbmeta.img (quello della build), vbmeta_disabled.img, props/, build-info.txt.
# Non si committano mai (target/ è ignorato).
set -eu
. "$(cd "$(dirname "$0")" && pwd)/common.sh"
vm "s=\$(cat $VETRO_AOSP_WORK/build.status 2>/dev/null); [ \"\$s\" = OK ] || { echo \"build non riuscita: \$s\" >&2; exit 1; }"
vm_rsync -a --delete "$here/remote/" "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/remote/"
vm "VETRO_AOSP_TREE=$VETRO_AOSP_TREE VETRO_AOSP_WORK=$VETRO_AOSP_WORK bash $VETRO_AOSP_WORK/remote/pack.sh"
mkdir -p "$out/out"
vm_rsync -a --delete --partial --info=progress2 "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/out/" "$out/out/"
(cd "$out/out" && shasum -a 256 -c --quiet SHA256SUMS)
grep -h -E '^dalvik\.vm\.isa\.arm64\.(variant|features)=' "$out"/out/props/*.prop | sort -u
if grep -h '^dalvik.vm.isa.arm64.variant=' "$out"/out/props/*.prop | grep -qv '=cortex-a53$'; then
  echo "ERRORE: variante ISA di ART diversa da cortex-a53" >&2
  exit 1
fi
"$here/mkdisk.sh"
