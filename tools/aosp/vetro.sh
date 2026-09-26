#!/bin/sh
# Avvia l'immagine AOSP di Vetro (target/aosp, tools/aosp/fetch.sh) sotto
# `vetro boot`, con la macchina, i dispositivi (stessi slot virtio-mmio) e la
# riga di comando di tools/aosp/qemu.sh. Il disco resta intatto
# (copy-on-write in memoria): ogni avvio è un primo avvio.
# adb: `adb connect 127.0.0.1:5555` (--hostfwd verso adbd, TCP 5555).
# Uso: tools/aosp/vetro.sh [secondi di guest] > vetro.log  (seriale su stdout,
# statistiche su stderr). VETRO_JIT=1 usa il JIT di sistema (ADR 0013),
# VETRO_AOSP_APPEND aggiunge parametri, VETRO_VETRO_EXTRA opzioni di vetro.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
a="$root/target/aosp"
# Immagini e disco (VETRO_AOSP_IMAGES, VETRO_AOSP_DISK: per provare una copia).
o="${VETRO_AOSP_IMAGES:-$a/out}"
disk="${VETRO_AOSP_DISK:-$a/disk.img}"
vetro="${VETRO_BIN:-$root/target/release/vetro}"
jit=""
[ "${VETRO_JIT:-0}" = 1 ] && jit="--jit"
# shellcheck disable=SC2086
exec "$vetro" boot --boot-img="$o/boot.img" --vendor-boot="$o/vendor_boot.img" \
  --init-boot="$o/init_boot.img" --append="nokaslr ${VETRO_AOSP_APPEND:-}" \
  --mem=3072 --disk="$disk" --hostfwd="tcp:127.0.0.1:${VETRO_ADB_PORT:-5555}-:5555" \
  --guest-secs="${1:-1800}" --stats $jit ${VETRO_VETRO_EXTRA:-}
