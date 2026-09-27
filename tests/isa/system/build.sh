#!/bin/sh
# Regenerates the system-mode probe:
#   probe.bin       flat image loaded at 0x40080000 (from probe.S)
#   probe.expected  serial output of qemu-system-aarch64 -M virt,gic-version=3
#                   -cpu cortex-a53 with that image
# The test tests/system_probe.rs runs probe.bin on Vetro and demands the
# same output. Needs Docker; the QEMU version ends up in probe.qemu.
set -eu
d="$(cd "$(dirname "$0")" && pwd -P)"
IMAGE="${VETRO_QSYS_IMAGE:-vetro-qemu-probe:latest}"
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
tail -n 1 "$d/probe.expected" | grep -q '^T:fine' || { echo "build.sh: the probe did not reach the end" >&2; exit 1; }
echo "ok: $(wc -l < "$d/probe.expected") righe, $(cat "$d/probe.qemu")"
