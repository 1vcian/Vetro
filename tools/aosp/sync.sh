#!/bin/sh
# Porta guest/aosp sulla VM di build e prepara il tree AOSP (idempotente):
#   guest/aosp/device/vetro  -> $TREE/device/vetro   (copia esatta)
#   guest/kernel/initramfs/vetro-files.c -> $TREE/device/vetro/vetro_arm64/vetro-files/
#   guest/aosp/vendor/vetro  -> $TREE/vendor/vetro   (copia esatta, tranne i prebuilt scaricati)
#   guest/aosp/patches, tools/aosp/remote -> ~/$WORK
# poi, sulla VM, applica le patch (salta quelle già applicate) e scarica le
# release di microG con lo sha256 di microg.lock (tools/aosp/remote/prepare.sh).
# Prima controlla la CA di sviluppo (tools/aosp/dev-ca.sh: certificato e patch
# coerenti) e scrive in ~/$WORK/sync.rev il commit di Vetro sincronizzato
# (ultimo commit di guest/aosp, tools/aosp e vetro-files.c, con "-dirty" se
# ci sono modifiche non committate): la build lo registra in build-info.txt e
# upload.sh ne ricava la versione.
# Uso: tools/aosp/sync.sh      (vedi common.sh per le variabili)
set -eu
. "$(cd "$(dirname "$0")" && pwd)/common.sh"
g="$root/guest/aosp"
"$here/dev-ca.sh" check
paths="guest/aosp tools/aosp guest/kernel/initramfs/vetro-files.c"
# shellcheck disable=SC2086
rev="$(git -C "$root" log -1 --format=%h -- $paths)"
# shellcheck disable=SC2086
if [ -n "$(git -C "$root" status --porcelain -- $paths)" ]; then rev="$rev-dirty"; fi
vm "mkdir -p $VETRO_AOSP_TREE/device/vetro $VETRO_AOSP_TREE/vendor/vetro $VETRO_AOSP_WORK"
vm_rsync -a --delete --exclude '/vetro_arm64/vetro-files/vetro-files.c' \
  "$g/device/vetro/" "$VETRO_AOSP_HOST:$VETRO_AOSP_TREE/device/vetro/"
# Il demone del gestore dei file (ADR 0020) ha un solo sorgente, in guest/kernel.
vm_rsync -a "$root/guest/kernel/initramfs/vetro-files.c" \
  "$VETRO_AOSP_HOST:$VETRO_AOSP_TREE/device/vetro/vetro_arm64/vetro-files/vetro-files.c"
vm_rsync -a --delete --exclude '/microg/prebuilt/' "$g/vendor/vetro/" "$VETRO_AOSP_HOST:$VETRO_AOSP_TREE/vendor/vetro/"
vm_rsync -a --delete "$g/patches/" "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/patches/"
vm_rsync -a --delete "$here/remote/" "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/remote/"
vm "VETRO_AOSP_TREE=$VETRO_AOSP_TREE VETRO_AOSP_WORK=$VETRO_AOSP_WORK sh $VETRO_AOSP_WORK/remote/prepare.sh"
vm "echo $rev > $VETRO_AOSP_WORK/sync.rev"
echo "sincronizzato: Vetro $rev -> $VETRO_AOSP_HOST"
