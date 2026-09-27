#!/bin/sh
# Boots the Android 15 emulator image (target/android-emu, see
# README.md) under qemu-system-aarch64 with the Vetro machine. The serial goes
# to stdout. The disks are copy-on-write (snapshot=on): the files don't change
# (the image folder is mounted read-only by the Docker wrapper).
# Usage: tools/android-emu/qemu.sh > log (stops with SIGTERM); VETRO_QEMU_EXTRA
# adds options (e.g. "-device virtio-gpu-device").
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
dir="$(cd "${VETRO_ANDROID_EMU:-$here/../../target/android-emu}" && pwd -P)"
img="$dir/arm64-v8a"
qemu="${VETRO_QEMU_SYSTEM_AARCH64:-}"
if [ -z "$qemu" ]; then
  if command -v qemu-system-aarch64 >/dev/null 2>&1; then qemu=qemu-system-aarch64
  else qemu="$here/../guest-kernel/qemu-system-aarch64-docker.sh"; fi
fi
exec "$qemu" -M virt,gic-version=3,its=off -cpu cortex-a53 -smp 1 -m 2G \
  -nic none -global virtio-mmio.force-legacy=false -nographic -no-reboot \
  -kernel "$dir/Image" -initrd "$img/ramdisk.img" -append "$("$here/cmdline.sh")" \
  -drive "file=$dir/userdata.img,if=none,id=data,format=raw,snapshot=on" -device virtio-blk-device,drive=data \
  -drive "file=$img/encryptionkey.img,if=none,id=meta,format=raw,snapshot=on" -device virtio-blk-device,drive=meta \
  -drive "file=$img/system.img,if=none,id=system,format=raw,snapshot=on" -device virtio-blk-device,drive=system ${VETRO_QEMU_EXTRA:-}
