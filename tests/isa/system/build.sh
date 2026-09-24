#!/bin/sh
# Rigenera la sonda della modalità sistema:
#   probe.bin       immagine piatta caricata a 0x40080000 (da probe.S)
#   probe.expected  uscita seriale di qemu-system-aarch64 -M virt,gic-version=3
#                   -cpu cortex-a53 con quell'immagine
# Il test tests/system_probe.rs esegue probe.bin su Vetro e pretende la
# stessa uscita. Serve Docker; la versione di QEMU finisce in probe.qemu.
set -eu
d="$(cd "$(dirname "$0")" && pwd -P)"
IMAGE="${VETRO_QSYS_IMAGE:-vetro-qemu-system:latest}"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" "$d" >&2
fi
docker run --rm --user "$(id -u):$(id -g)" -v "$d:/w" -w /w "$IMAGE" sh -euc '
tmp="$(mktemp -d)"
aarch64-linux-gnu-as -o "$tmp/probe.o" probe.S
aarch64-linux-gnu-ld -Ttext=0x40080000 -e _start -o "$tmp/probe.elf" "$tmp/probe.o"
aarch64-linux-gnu-objcopy -O binary "$tmp/probe.elf" probe.bin
timeout 60 qemu-system-aarch64 -M virt,gic-version=3 -cpu cortex-a53 -m 128M \
  -nographic -monitor none -nic none -serial stdio -kernel "$tmp/probe.elf" > probe.expected
qemu-system-aarch64 --version | head -1 > probe.qemu
'
tail -n 1 "$d/probe.expected" | grep -q '^T:fine' || { echo "build.sh: la sonda non è arrivata alla fine" >&2; exit 1; }
echo "ok: $(wc -l < "$d/probe.expected") righe, $(cat "$d/probe.qemu")"
