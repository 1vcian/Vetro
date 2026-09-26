#!/bin/sh
# Porta guest/aosp sulla VM di build e prepara il tree AOSP (idempotente):
#   guest/aosp/device/vetro  -> $TREE/device/vetro   (copia esatta)
#   guest/kernel/initramfs/vetro-files.c -> $TREE/device/vetro/vetro_arm64/vetro-files/
#   guest/aosp/vendor/vetro  -> $TREE/vendor/vetro   (copia esatta, tranne i prebuilt scaricati)
#   guest/aosp/patches, tools/aosp/remote -> ~/$WORK
# poi, sulla VM, applica le patch (salta quelle già applicate) e scarica le
# release di microG con lo sha256 di microg.lock (tools/aosp/remote/prepare.sh).
# Uso: tools/aosp/sync.sh      (vedi common.sh per le variabili)
set -eu
. "$(cd "$(dirname "$0")" && pwd)/common.sh"
g="$root/guest/aosp"
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
