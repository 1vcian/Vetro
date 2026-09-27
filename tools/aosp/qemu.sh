#!/bin/sh
# Boots the Vetro AOSP image (target/aosp, tools/aosp/fetch.sh) under
# qemu-system-aarch64, the oracle: same machine as `vetro boot`
# (tools/aosp/vetro.sh) and same devices in the same order, hence
# in the same virtio-mmio slots (the first -device goes into slot 31):
#   31 virtio-gpu (2D, 1280x800)   30 keyboard   29 tablet
#   28 virtio-net (user, adb forwarded on 127.0.0.1:5555)   27 GPT disk
# The bootloader is Vetro's (ADR 0018): `vetro boot --android-dump`
# writes Image, initrd (vendor + generic ramdisk + bootconfig) and cmdline,
# which QEMU receives with -kernel/-initrd/-append.
# The disk is copy-on-write (snapshot=on): every boot is a first boot.
# QEMU runs in Docker (tools/guest-kernel/Dockerfile.qemu) with port 5555
# published on localhost: `adb connect 127.0.0.1:5555`.
# Usage: tools/aosp/qemu.sh > qemu.log   (serial on stdout; stops on SIGTERM)
# VETRO_QEMU_EXTRA adds options to QEMU, VETRO_AOSP_APPEND to the command line.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
a="$root/target/aosp"
# Images and disk (VETRO_AOSP_IMAGES, VETRO_AOSP_DISK: to try a copy).
o="${VETRO_AOSP_IMAGES:-$a/out}"
disk="${VETRO_AOSP_DISK:-$a/disk.img}"
vetro="${VETRO_BIN:-$root/target/release/vetro}"
dump="$a/boot"
rm -rf "$dump"
# --guest-secs=0: writes the files and stops at once (code 124, time limit).
"$vetro" boot --boot-img="$o/boot.img" --vendor-boot="$o/vendor_boot.img" \
  --init-boot="$o/init_boot.img" --append="nokaslr ${VETRO_AOSP_APPEND:-}" \
  --android-dump="$dump" --no-devices --mem=512 --guest-secs=0 >/dev/null || true
[ -s "$dump/Image" ] && [ -s "$dump/initrd" ] || { echo "vetro boot --android-dump did not write $dump" >&2; exit 1; }
image="${VETRO_QEMU_SYSTEM_IMAGE:-vetro-qemu-system:latest}"
docker image inspect "$image" >/dev/null 2>&1 ||
  docker build -q -t "$image" -f "$root/tools/guest-kernel/Dockerfile.qemu" "$root/tools/guest-kernel" >&2
# shellcheck disable=SC2086
exec docker run --rm -i --init --name "${VETRO_ORACLE_NAME:-vetro-aosp-qemu}" \
  -p 127.0.0.1:${VETRO_ADB_PORT:-5555}:5555 -v "$a:$a" "$image" \
  qemu-system-aarch64 -M virt,gic-version=3,its=off -cpu cortex-a53 -smp 1 -m 3G \
  -nographic -no-reboot -global virtio-mmio.force-legacy=false -nic none \
  -kernel "$dump/Image" -initrd "$dump/initrd" -append "$(cat "$dump/cmdline")" \
  -device virtio-gpu-device -device virtio-keyboard-device -device virtio-tablet-device \
  -netdev user,id=net0,hostfwd=tcp::5555-:5555 -device virtio-net-device,netdev=net0 \
  -drive "file=$disk,if=none,id=disk,format=raw,snapshot=on" -device virtio-blk-device,drive=disk \
  ${VETRO_QEMU_EXTRA:-}
