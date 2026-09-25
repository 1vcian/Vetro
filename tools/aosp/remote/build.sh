#!/bin/bash
# Sulla VM di build: costruisce l'immagine di Vetro (lanciato staccato da
# tools/aosp/build.sh). La build di AOSP è incrementale: se la VM
# viene fermata, rilanciare riprende da dove era.
# Stato in ~/$WORK/build.status: RUNNING, OK o FAIL <codice>; log in
# ~/$WORK/build.log (in coda a ogni tentativo).
# Niente set -u: build/envsetup.sh usa variabili non definite.
set -o pipefail
cd
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
lunch_target="${VETRO_AOSP_LUNCH:-vetro_arm64-bp1a-userdebug}"
mkdir -p "$work"
echo RUNNING > "$work/build.status"
echo $$ > "$work/build.pid"
(
  echo "=== $(date -u +%FT%TZ) build di $lunch_target"
  cd "$tree"
  # Niente ccache: la sandbox della build di AOSP 15 (nsjail) monta la home
  # in sola lettura e ogni compilazione fallirebbe ("Read-only file system").
  # La build resta incrementale in out/.
  unset USE_CCACHE CCACHE_EXEC
  source build/envsetup.sh
  lunch "$lunch_target" || exit 1
  # droid = tutte le immagini (boot, vendor_boot, init_boot, super,
  # userdata, vbmeta). Il disco GPT si compone sul Mac (tools/aosp/mkdisk.sh).
  m droid
) >> "$work/build.log" 2>&1
code=$?
if [ "$code" -eq 0 ]; then echo OK > "$work/build.status"; else echo "FAIL $code" > "$work/build.status"; fi
echo "=== $(date -u +%FT%TZ) fine, codice $code" >> "$work/build.log"
exit "$code"
