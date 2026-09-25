#!/bin/bash
# Sulla VM di build: costruisce l'immagine di Vetro (lanciato staccato da
# tools/aosp/build.sh). La build di AOSP è incrementale: se la VM (Spot)
# viene fermata, rilanciare riprende da dove era.
# Stato in ~/$WORK/build.status: RUNNING, OK o FAIL <codice>; log in
# ~/$WORK/build.log (in coda a ogni tentativo).
set -uo pipefail
cd
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
lunch_target="${VETRO_AOSP_LUNCH:-vetro_arm64-bp1a-userdebug}"
mkdir -p "$work"
echo RUNNING > "$work/build.status"
echo $$ > "$work/build.pid"
{
  echo "=== $(date -u +%FT%TZ) build di $lunch_target"
  cd "$tree"
  # ccache per i C/C++ (50 GB su ~/.ccache): utile quando si ricomincia da
  # zero, per esempio dopo un cambio di BoardConfig che invalida out/.
  export USE_CCACHE=1 CCACHE_EXEC=/usr/bin/ccache CCACHE_DIR="$HOME/.ccache"
  ccache -M 50G >/dev/null
  source build/envsetup.sh
  lunch "$lunch_target"
  # droid = tutte le immagini (boot, vendor_boot, init_boot, super, vbmeta,
  # userdata); gli strumenti host servono a tools/aosp/fetch.sh.
  m droid avbtool simg2img lpunpack
} >> "$work/build.log" 2>&1
code=$?
if [ "$code" -eq 0 ]; then echo OK > "$work/build.status"; else echo "FAIL $code" > "$work/build.status"; fi
echo "=== $(date -u +%FT%TZ) fine, codice $code" >> "$work/build.log"
ccache -s >> "$work/build.log" 2>&1 || true
exit "$code"
