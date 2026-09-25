#!/bin/sh
# Avvia l'immagine dell'emulatore Android 15 (target/android-emu, vedi
# README.md) sotto `vetro boot`, con gli stessi dischi, la stessa RAM e la
# stessa riga di comando di qemu.sh. La seriale va su stdout; le statistiche
# su stderr. I dischi restano intatti (copy-on-write in memoria).
# Uso: tools/android-emu/vetro.sh [secondi di guest] > log
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
dir="$(cd "${VETRO_ANDROID_EMU:-$root/target/android-emu}" && pwd -P)"
img="$dir/arm64-v8a"
vetro="${VETRO_BIN:-$root/target/release/vetro}"
exec "$vetro" boot --kernel="$dir/Image" --initrd="$img/ramdisk.img" --append="$("$here/cmdline.sh")" \
  --mem=2048 --no-devices --disk="$dir/userdata.img" --disk="$img/encryptionkey.img" --disk="$img/system.img" \
  --guest-secs="${1:-120}" --stats
