#!/bin/sh
# Stand-in for qemu-aarch64 on macOS: runs QEMU user mode in a Linux
# container with the same syntax:
#   qemu-aarch64-docker.sh [qemu options] program [guest arguments]
# Mounted (at the same path) are the directory of the program, the
# current directory (which stays the working directory) and those listed in
# VETRO_ORACLE_MOUNTS (separated by ':').
# --init: QEMU must not be PID 1, otherwise when the guest dies of
# a signal QEMU cannot terminate with that signal and hangs.
# Usage: export VETRO_QEMU_AARCH64=$PWD/tools/oracle/qemu-aarch64-docker.sh
set -eu
IMAGE="${VETRO_ORACLE_IMAGE:-vetro-oracle:latest}"
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker build -q -t "$IMAGE" "$(dirname "$0")" >&2
fi
# Separates the QEMU options (some take a value) from the program.
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
# env -i: the guest sees only the variables passed with -E, as natively
# with Command::env_clear().
# --user: the same user as the host, as natively (permissions and getuid).
exec docker run --rm -i --init --ulimit core=0 --user "$(id -u):$(id -g)" $mounts -w "$cwd" "$IMAGE" env -i qemu-aarch64 $opts "$prog" "$@"
