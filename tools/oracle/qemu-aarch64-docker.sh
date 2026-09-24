#!/bin/sh
# Wrapper compatibile con qemu-aarch64: esegue QEMU user mode in un
# container Linux. Uso: export VETRO_QEMU_AARCH64=$PWD/tools/oracle/qemu-aarch64-docker.sh
set -eu
IMAGE="${VETRO_ORACLE_IMAGE:-vetro-oracle:latest}"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" "$(dirname "$0")" >&2
fi
elf="$1"; shift
dir="$(cd "$(dirname "$elf")" && pwd)"
exec docker run --rm -i -v "$dir:$dir:ro" "$IMAGE" qemu-aarch64 "$dir/$(basename "$elf")" "$@"
