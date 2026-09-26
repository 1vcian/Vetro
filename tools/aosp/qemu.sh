#!/bin/sh
# Avvia l'immagine AOSP di Vetro (target/aosp, tools/aosp/fetch.sh) sotto
# qemu-system-aarch64, l'oracolo: stessa macchina di `vetro boot`
# (tools/aosp/vetro.sh) e stessi dispositivi nello stesso ordine, quindi
# negli stessi slot virtio-mmio (il primo -device va nello slot 31):
#   31 virtio-gpu (2D, 1280x800)   30 tastiera   29 tablet
#   28 virtio-net (user, adb inoltrato su 127.0.0.1:5555)   27 disco GPT
# Il bootloader è quello di Vetro (ADR 0018): `vetro boot --android-dump`
# scrive Image, initrd (ramdisk del vendor + generico + bootconfig) e cmdline,
# che QEMU riceve con -kernel/-initrd/-append.
# Il disco è in copy-on-write (snapshot=on): ogni avvio è un primo avvio.
# QEMU gira in Docker (tools/guest-kernel/Dockerfile.qemu) con la porta 5555
# pubblicata su localhost: `adb connect 127.0.0.1:5555`.
# Uso: tools/aosp/qemu.sh > qemu.log   (seriale su stdout; si ferma con SIGTERM)
# VETRO_QEMU_EXTRA aggiunge opzioni a QEMU, VETRO_AOSP_APPEND alla riga di comando.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
a="$root/target/aosp"
# Immagini e disco (VETRO_AOSP_IMAGES, VETRO_AOSP_DISK: per provare una copia).
o="${VETRO_AOSP_IMAGES:-$a/out}"
disk="${VETRO_AOSP_DISK:-$a/disk.img}"
vetro="${VETRO_BIN:-$root/target/release/vetro}"
dump="$a/boot"
rm -rf "$dump"
# --guest-secs=0: scrive i file e si ferma subito (codice 124, limite di tempo).
"$vetro" boot --boot-img="$o/boot.img" --vendor-boot="$o/vendor_boot.img" \
  --init-boot="$o/init_boot.img" --append="nokaslr ${VETRO_AOSP_APPEND:-}" \
  --android-dump="$dump" --no-devices --mem=512 --guest-secs=0 >/dev/null || true
[ -s "$dump/Image" ] && [ -s "$dump/initrd" ] || { echo "vetro boot --android-dump non ha scritto $dump" >&2; exit 1; }
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
