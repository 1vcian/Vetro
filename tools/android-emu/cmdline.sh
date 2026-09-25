#!/bin/sh
# Riga di comando del kernel per l'immagine dell'emulatore Android 15 sulla
# macchina di Vetro (vedi README.md). Stampata su stdout.
# Dischi, nell'ordine della riga di comando di QEMU (il primo va nello slot
# virtio-mmio più alto; Linux li numera per indirizzo crescente):
#   slot 31 (a003e00) userdata.img     -> vdc, /data del fstab (vuoto: lo formatta vold)
#   slot 30 (a003c00) encryptionkey.img -> vdb, GPT con "metadata" (il fstab lo cerca qui)
#   slot 29 (a003a00) system.img        -> vda, GPT con "super" (partizioni logiche)
# VETRO_ANDROID_APPEND aggiunge parametri in fondo.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
img="${VETRO_ANDROID_EMU:-$here/../../target/android-emu}/arm64-v8a"
vb="$(sed -n 's/^param: "\(.*\)"$/\1/p' "$img/VerifiedBootParams.textproto" | tr '\n' ' ')"
printf '%s' "console=ttyAMA0 nokaslr $(cat "$img/kernel_cmdline.txt") printk.devkmsg=on loop.max_part=7 \
androidboot.hardware=ranchu androidboot.qemu=1 androidboot.selinux=permissive \
androidboot.boot_devices=a003a00.virtio_mmio androidboot.console=ttyAMA0 ${vb}${VETRO_ANDROID_APPEND:-}"
