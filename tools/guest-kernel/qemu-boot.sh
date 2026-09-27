#!/bin/sh
# Boots the M3 guest kernel under qemu-system-aarch64 (the oracle) and checks
# markers, shell and power-off: it is the tests/boot test, here with the right
# values of the variables. On macOS it uses the container, on Linux native QEMU.
#   tools/guest-kernel/qemu-boot.sh            # check
#   tools/guest-kernel/qemu-boot.sh --update   # rewrites the reference log
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [ -z "${VETRO_QEMU_SYSTEM_AARCH64:-}" ] && ! command -v qemu-system-aarch64 >/dev/null 2>&1; then
  export VETRO_QEMU_SYSTEM_AARCH64="$ROOT/tools/guest-kernel/qemu-system-aarch64-docker.sh"
fi
[ "${1:-}" = "--update" ] && export VETRO_BOOT_UPDATE_REFERENCE=1
export VETRO_REQUIRE_ORACLE=1 VETRO_REQUIRE_GUEST_KERNEL=1
cd "$ROOT"
exec cargo test -p vetro-boot-tests --test qemu -- --nocapture
