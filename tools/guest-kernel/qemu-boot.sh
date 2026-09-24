#!/bin/sh
# Avvia il kernel guest di M3 sotto qemu-system-aarch64 (l'oracolo) e controlla
# marcatori, shell e spegnimento: è il test di tests/boot, qui con i valori
# giusti delle variabili. Su macOS usa il container, su Linux QEMU nativo.
#   tools/guest-kernel/qemu-boot.sh            # verifica
#   tools/guest-kernel/qemu-boot.sh --update   # riscrive il log di riferimento
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [ -z "${VETRO_QEMU_SYSTEM_AARCH64:-}" ] && ! command -v qemu-system-aarch64 >/dev/null 2>&1; then
  export VETRO_QEMU_SYSTEM_AARCH64="$ROOT/tools/guest-kernel/qemu-system-aarch64-docker.sh"
fi
[ "${1:-}" = "--update" ] && export VETRO_BOOT_UPDATE_REFERENCE=1
export VETRO_REQUIRE_ORACLE=1 VETRO_REQUIRE_GUEST_KERNEL=1
cd "$ROOT"
exec cargo test -p vetro-boot-tests --test qemu -- --nocapture
