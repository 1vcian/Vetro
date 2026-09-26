#!/bin/bash
# Sulla VM di build: costruisce l'immagine di Vetro (lanciato staccato da
# tools/aosp/build.sh). La build di AOSP è incrementale: se la VM
# viene fermata, rilanciare riprende da dove era.
# Stato in ~/$WORK/build.status: RUNNING, OK o FAIL <codice>; log in
# ~/$WORK/build.log (in coda a ogni tentativo). ~/$WORK/build.rev = il commit
# di Vetro sincronizzato (sync.rev) di cui questa build è il risultato.
# Niente set -u: build/envsetup.sh usa variabili non definite.
set -o pipefail
cd || exit 1
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
lunch_target="${VETRO_AOSP_LUNCH:-vetro_arm64-bp1a-userdebug}"
mkdir -p "$work"
echo RUNNING > "$work/build.status"
echo $$ > "$work/build.pid"
rm -f "$work/build.rev"
rev="$(cat "$work/sync.rev" 2>/dev/null || echo sconosciuta)"
start=$(date +%s)
(
  echo "=== $(date -u +%FT%TZ) build di $lunch_target (Vetro $rev), $(nproc) CPU, $(free -g | awk '/^Mem:/ {print $2}') GiB"
  cd "$tree" || exit 1
  # ccache: spento di default. La sandbox della build (nsjail) monta tutto in
  # sola lettura tranne il tree e out/, quindi la cache sta in out/.ccache.
  # Accenderlo (VETRO_AOSP_CCACHE=1, serve /usr/bin/ccache) cambia la riga di
  # comando di ogni compilazione C/C++ (CC_WRAPPER e, con USE_CCACHE,
  # -Wno-unused-command-line-argument): la prima build ricompila tutto il
  # C/C++, poi conviene solo dopo un `m clean` o un cambio di tag AOSP. Le
  # immagini non cambiano (il wrapper e l'avviso in più non toccano il codice
  # generato; CCACHE_COMPILERCHECK=content).
  if [ "${VETRO_AOSP_CCACHE:-0}" = 1 ] && command -v ccache >/dev/null; then
    cc="$(command -v ccache)"
    export USE_CCACHE=1 CCACHE_EXEC="$cc" CC_WRAPPER="$cc"
    export CCACHE_DIR="$tree/out/.ccache" CCACHE_COMPILERCHECK=content
    mkdir -p "$CCACHE_DIR"
    ccache -M 50G >/dev/null
    echo "ccache: $CCACHE_DIR"
  else
    [ "${VETRO_AOSP_CCACHE:-0}" = 1 ] && echo "ccache chiesto ma non installato (sudo apt install ccache): build senza"
    unset USE_CCACHE CCACHE_EXEC CC_WRAPPER CCACHE_DIR
  fi
  source build/envsetup.sh
  lunch "$lunch_target" || exit 1
  # droid = tutte le immagini (boot, vendor_boot, init_boot, super,
  # userdata, vbmeta). Il disco GPT si compone sul Mac (tools/aosp/mkdisk.sh).
  # -k: non fermarsi al primo errore, per vederli tutti in un giro.
  m -k droid
) >> "$work/build.log" 2>&1
code=$?
end=$(date +%s)
{
  # Misure (le prestazioni si misurano): durata e peso degli strumenti host.
  echo "=== durata $(( (end - start) / 60 )) min"
  du -sh "$tree"/out/host/* 2>/dev/null | sed 's/^/=== host: /'
  [ "${VETRO_AOSP_CCACHE:-0}" = 1 ] && CCACHE_DIR="$tree/out/.ccache" ccache -s 2>/dev/null | sed 's/^/=== ccache: /'
} >> "$work/build.log" 2>&1
if [ "$code" -eq 0 ]; then
  echo "$rev" > "$work/build.rev"
  echo OK > "$work/build.status"
else
  echo "FAIL $code" > "$work/build.status"
fi
echo "=== $(date -u +%FT%TZ) fine, codice $code" >> "$work/build.log"
exit "$code"
