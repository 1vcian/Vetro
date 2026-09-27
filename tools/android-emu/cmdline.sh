#!/bin/sh
# Kernel command line for the Android 15 emulator image on the
# Vetro machine (see README.md). Printed on stdout.
# Disks, in the order of the QEMU command line (the first goes into the highest
# virtio-mmio slot; Linux numbers them by increasing address):
#   slot 31 (a003e00) userdata.img     -> vdc, /data of the fstab (empty: vold formats it)
#   slot 30 (a003c00) encryptionkey.img -> vdb, GPT with "metadata" (the fstab looks for it here)
#   slot 29 (a003a00) system.img        -> vda, GPT with "super" (logical partitions)
# VETRO_ANDROID_APPEND appends parameters at the end.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
img="${VETRO_ANDROID_EMU:-$here/../../target/android-emu}/arm64-v8a"
vb="$(sed -n 's/^param: "\(.*\)"$/\1/p' "$img/VerifiedBootParams.textproto" | tr '\n' ' ')"
printf '%s' "console=ttyAMA0 nokaslr $(cat "$img/kernel_cmdline.txt") printk.devkmsg=on loop.max_part=7 \
androidboot.hardware=ranchu androidboot.qemu=1 androidboot.selinux=permissive \
androidboot.boot_devices=a003a00.virtio_mmio androidboot.console=ttyAMA0 ${vb}${VETRO_ANDROID_APPEND:-}"
