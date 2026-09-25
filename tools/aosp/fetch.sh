#!/bin/sh
# Porta sul Mac gli artefatti dell'ultima build riuscita:
#   VM: tools/aosp/remote/pack.sh raccoglie immagini, proprietà e build-info
#   Mac: target/aosp/out/ (rsync, poi SHA256SUMS verificato), controllo della
#        variante ISA di ART, poi tools/aosp/mkdisk.sh compone il disco GPT
#        (target/aosp/disk.img)
# Artefatti: boot.img, vendor_boot.img, init_boot.img, super.img e
# userdata.img (sparsi), props/, build-info.txt, SHA256SUMS.
# Non si committano mai (target/ è ignorato). Idempotente.
set -eu
. "$(cd "$(dirname "$0")" && pwd)/common.sh"
vm "s=\$(cat $VETRO_AOSP_WORK/build.status 2>/dev/null); [ \"\$s\" = OK ] || { echo \"build non riuscita: \$s\" >&2; exit 1; }"
vm_rsync -a --delete "$here/remote/" "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/remote/"
vm "VETRO_AOSP_TREE=$VETRO_AOSP_TREE VETRO_AOSP_WORK=$VETRO_AOSP_WORK bash $VETRO_AOSP_WORK/remote/pack.sh"
mkdir -p "$out/out"
vm_rsync -a --delete --partial "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/out/" "$out/out/"
(cd "$out/out" && shasum -a 256 -c --quiet SHA256SUMS)
# ART deve generare codice per la CPU di Vetro (Cortex-A53, ARMv8.0 + CRC32 +
# crypto): con varianti più nuove il JIT userebbe LSE e FP16 (ADR 0005, 0022).
variants="$(grep -h '^dalvik.vm.isa.arm64.variant=' "$out"/out/props/*.prop | sort -u)"
grep -h -E '^dalvik\.vm\.isa\.arm64\.(variant|features)=' "$out"/out/props/*.prop | sort -u
if [ "$variants" != "dalvik.vm.isa.arm64.variant=cortex-a53" ]; then
  echo "ERRORE: variante ISA di ART diversa da cortex-a53: ${variants:-assente}" >&2
  exit 1
fi
if grep -h '^ro.product.cpu.abilist32=.' "$out"/out/props/*.prop; then
  echo "ERRORE: l'immagine dichiara ABI a 32 bit" >&2
  exit 1
fi
"$here/mkdisk.sh"
