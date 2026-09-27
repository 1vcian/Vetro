#!/bin/sh
# Stand-in for qemu-system-aarch64 on macOS: runs QEMU (full system)
# in a Debian container with the same syntax:
#   qemu-system-aarch64-docker.sh -M virt ... -kernel Image -initrd initrd.gz
# The directories of the files passed with -kernel, -initrd and -dtb are mounted
# read-only at the same path; others are added with
# VETRO_ORACLE_MOUNTS (separated by ':'). Stdin and stdout are those of the
# container (-i without a tty: with -nographic the serial goes to stdio).
# --init and the signal proxy of `docker run`: a SIGTERM to this process
# reaches QEMU and the container shuts down.
# VETRO_ORACLE_NAME gives the container a name: needed by the port forwarding
# tests (tests/boot/tests/hostfwd.rs), which connect to `hostfwd` from
# localhost inside the container with `docker exec` (the connections forwarded
# by Docker would arrive from its gateway, not from localhost).
# Usage: export VETRO_QEMU_SYSTEM_AARCH64=$PWD/tools/guest-kernel/qemu-system-aarch64-docker.sh
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
# Relative paths work only if the current directory also exists
# in the container: we mount it read-only.
cwd="$(pwd -P)"
name=""
[ -n "${VETRO_ORACLE_NAME:-}" ] && name="--name $VETRO_ORACLE_NAME"
# shellcheck disable=SC2086
exec docker run --rm -i --init $name $mounts -v "$cwd:$cwd:ro" -w "$cwd" "$IMAGE" qemu-system-aarch64 "$@"
