#!/bin/sh
# Wrapper compatibile con qemu-aarch64: esegue QEMU user mode in un
# container Linux. L'ultimo argomento è l'ELF da eseguire; gli altri sono
# opzioni di QEMU (es. -cpu cortex-a53).
# --init: QEMU non deve essere PID 1, altrimenti quando il guest muore per
# un segnale QEMU non riesce a terminare con quel segnale e resta appeso.
# Uso: export VETRO_QEMU_AARCH64=$PWD/tools/oracle/qemu-aarch64-docker.sh
set -eu
IMAGE="${VETRO_ORACLE_IMAGE:-vetro-oracle:latest}"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" "$(dirname "$0")" >&2
fi
for last; do :; done
dir="$(cd "$(dirname "$last")" && pwd)"
n=$#
i=1
set -- "$@" --
while [ "$i" -lt "$n" ]; do
  set -- "$@" "$1"; shift; i=$((i + 1))
done
shift  # scarta l'ELF originale (relativo)
shift  # scarta il separatore
exec docker run --rm -i --init --ulimit core=0 -v "$dir:$dir:ro" "$IMAGE" qemu-aarch64 "$@" "$dir/$(basename "$last")"
