#!/bin/sh
# Sostituto di qemu-aarch64 per macOS: esegue QEMU user mode in un container
# Linux con la stessa sintassi:
#   qemu-aarch64-docker.sh [opzioni qemu] programma [argomenti del guest]
# Sono montati (con lo stesso percorso) la directory del programma, la
# directory corrente (che resta quella di lavoro) e quelle elencate in
# VETRO_ORACLE_MOUNTS (separate da ':').
# --init: QEMU non deve essere PID 1, altrimenti quando il guest muore per
# un segnale QEMU non riesce a terminare con quel segnale e resta appeso.
# Uso: export VETRO_QEMU_AARCH64=$PWD/tools/oracle/qemu-aarch64-docker.sh
set -eu
IMAGE="${VETRO_ORACLE_IMAGE:-vetro-oracle:latest}"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" "$(dirname "$0")" >&2
fi
# Separa le opzioni di QEMU (alcune hanno un valore) dal programma.
opts=""
while [ $# -gt 0 ]; do
  case "$1" in
    -cpu|-L|-E|-U|-d|-D|-r|-s|-B|-R|-seed|-trace)
      opts="$opts $1 $2"; shift 2 ;;
    -*) opts="$opts $1"; shift ;;
    *) break ;;
  esac
done
prog="$1"; shift
pdir="$(cd "$(dirname "$prog")" && pwd -P)"
prog="$pdir/$(basename "$prog")"
cwd="$(pwd -P)"
mounts="-v $pdir:$pdir:ro"
[ "$cwd" != "$pdir" ] && mounts="$mounts -v $cwd:$cwd"
IFS=':'
for m in ${VETRO_ORACLE_MOUNTS:-}; do
  [ -n "$m" ] && mounts="$mounts -v $m:$m"
done
unset IFS
# shellcheck disable=SC2086
# env -i: il guest vede solo le variabili passate con -E, come in nativo
# con Command::env_clear().
# --user: lo stesso utente dell'host, come in nativo (permessi e getuid).
exec docker run --rm -i --init --ulimit core=0 --user "$(id -u):$(id -g)" $mounts -w "$cwd" "$IMAGE" env -i qemu-aarch64 $opts "$prog" "$@"
