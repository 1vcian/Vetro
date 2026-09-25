#!/bin/sh
# Sostituto di qemu-system-aarch64 per macOS: esegue QEMU (sistema completo)
# in un container Debian con la stessa sintassi:
#   qemu-system-aarch64-docker.sh -M virt ... -kernel Image -initrd initrd.gz
# Le directory dei file passati con -kernel, -initrd e -dtb sono montate in
# sola lettura con lo stesso percorso; le altre si aggiungono con
# VETRO_ORACLE_MOUNTS (separate da ':'). Stdin e stdout sono quelli del
# container (-i senza tty: con -nographic la seriale va su stdio).
# --init e il proxy dei segnali di `docker run`: un SIGTERM a questo processo
# arriva a QEMU e il container si chiude.
# VETRO_ORACLE_NAME dà un nome al container: serve ai test dell'inoltro di
# porte (tests/boot/tests/hostfwd.rs), che si collegano a `hostfwd` da
# localhost dentro il container con `docker exec` (le connessioni inoltrate
# da Docker arriverebbero dal suo gateway, non da localhost).
# Uso: export VETRO_QEMU_SYSTEM_AARCH64=$PWD/tools/guest-kernel/qemu-system-aarch64-docker.sh
set -eu
IMAGE="${VETRO_QEMU_SYSTEM_IMAGE:-vetro-qemu-system:latest}"
here="$(cd "$(dirname "$0")" && pwd)"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" -f "$here/Dockerfile.qemu" "$here" >&2
fi
mounts=""
prev=""
for a in "$@"; do
  case "$prev" in
    -kernel|-initrd|-dtb)
      d="$(cd "$(dirname "$a")" && pwd -P)"
      mounts="$mounts -v $d:$d:ro" ;;
  esac
  prev="$a"
done
IFS=':'
for m in ${VETRO_ORACLE_MOUNTS:-}; do
  [ -n "$m" ] && mounts="$mounts -v $m:$m"
done
unset IFS
# I percorsi relativi funzionano solo se la directory corrente esiste anche
# nel container: la montiamo in sola lettura.
cwd="$(pwd -P)"
name=""
[ -n "${VETRO_ORACLE_NAME:-}" ] && name="--name $VETRO_ORACLE_NAME"
# shellcheck disable=SC2086
exec docker run --rm -i --init $name $mounts -v "$cwd:$cwd:ro" -w "$cwd" "$IMAGE" qemu-system-aarch64 "$@"
