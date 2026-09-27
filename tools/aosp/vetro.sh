#!/bin/sh
# Boots the Vetro AOSP image (target/aosp, tools/aosp/fetch.sh) under
# `vetro boot`, with the machine, devices (same virtio-mmio slots) and
# command line of tools/aosp/qemu.sh. The disk stays untouched
# (copy-on-write in memory): every boot is a first boot.
# adb: `adb connect 127.0.0.1:5555` (--hostfwd to adbd, TCP 5555).
# Usage: tools/aosp/vetro.sh [guest seconds] > vetro.log  (serial on stdout,
# statistics on stderr). VETRO_JIT=1 uses the system JIT (ADR 0013),
# VETRO_AOSP_APPEND adds parameters, VETRO_VETRO_EXTRA vetro options.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
a="$root/target/aosp"
# Images and disk (VETRO_AOSP_IMAGES, VETRO_AOSP_DISK: to try a copy).
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
