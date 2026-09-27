#!/bin/sh
# Boots the Android 15 emulator image (target/android-emu, see
# README.md) under `vetro boot`, with the same disks, the same RAM and the
# same command line as qemu.sh. The serial goes to stdout; the statistics
# to stderr. The disks stay intact (in-memory copy-on-write).
# Usage: tools/android-emu/vetro.sh [guest seconds] > log
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
dir="$(cd "${VETRO_ANDROID_EMU:-$root/target/android-emu}" && pwd -P)"
img="$dir/arm64-v8a"
vetro="${VETRO_BIN:-$root/target/release/vetro}"
exec "$vetro" boot --kernel="$dir/Image" --initrd="$img/ramdisk.img" --append="$("$here/cmdline.sh")" \
  --mem=2048 --no-devices --disk="$dir/userdata.img" --disk="$img/encryptionkey.img" --disk="$img/system.img" \
  --guest-secs="${1:-120}" --stats
