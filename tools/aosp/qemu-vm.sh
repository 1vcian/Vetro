#!/bin/sh
# Boots Vetro's AOSP image under QEMU on the build VM (native, no Docker: the
# Mac only runs light checks), from the Mac. Same machine and devices as
# tools/aosp/qemu.sh; the VM side is tools/aosp/remote/qemu.sh.
#   tools/aosp/qemu-vm.sh start           Vetro's bootloader writes Image, initrd
#                                         and cmdline here (`vetro boot
#                                         --android-dump`, from target/aosp/out or
#                                         VETRO_AOSP_IMAGES), they go to the VM,
#                                         QEMU starts there with the VM's images
#                                         (~/$WORK/out, i.e. the last fetch.sh)
#   tools/aosp/qemu-vm.sh status|stop
#   tools/aosp/qemu-vm.sh log [N]         last N lines of the serial console
#   tools/aosp/qemu-vm.sh adb ARGS...     the VM's adb (AOSP host build) on the guest
#   tools/aosp/qemu-vm.sh screendump F.png  the scanout as QEMU shows it (monitor)
#   tools/aosp/qemu-vm.sh screencap F.png   SurfaceFlinger's picture (adb screencap)
# VETRO_QEMU_NAME (default aosp), VETRO_ADB_PORT (5565), VETRO_QEMU_MONITOR
# (4454), VETRO_VM_IMAGES (images on the VM, default ~/$WORK/out; the local
# VETRO_AOSP_IMAGES must be the same image, for the bootloader's files), VETRO_AOSP_APPEND (more kernel/bootconfig parameters, e.g.
# androidboot.* for Vetro's bootloader), VETRO_QEMU_EXTRA: see remote/qemu.sh.
set -eu
. "$(cd "$(dirname "$0")" && pwd)/common.sh"
name="${VETRO_QEMU_NAME:-aosp}"
port="${VETRO_ADB_PORT:-5565}"
mon="${VETRO_QEMU_MONITOR:-4454}"
rd="$VETRO_AOSP_WORK/qemu/$name"
env="VETRO_AOSP_WORK=$VETRO_AOSP_WORK VETRO_AOSP_TREE=$VETRO_AOSP_TREE VETRO_QEMU_NAME=$name VETRO_ADB_PORT=$port VETRO_QEMU_MONITOR=$mon"
# Another image already on the VM (a directory with super.img and
# userdata.img, relative to the home), e.g. a previous version from R2.
[ -n "${VETRO_VM_IMAGES:-}" ] && env="$env VETRO_AOSP_IMAGES=\$HOME/$VETRO_VM_IMAGES"
adb_vm="$VETRO_AOSP_TREE/out/host/linux-x86/bin/adb"
case "${1:-status}" in
  start)
    o="${VETRO_AOSP_IMAGES:-$out/out}"
    vetro="${VETRO_BIN:-$root/target/release/vetro}"
    dump="$out/boot-vm"
    rm -rf "$dump"
    # --guest-secs=0: writes the files and stops at once (exit code 124).
    "$vetro" boot --boot-img="$o/boot.img" --vendor-boot="$o/vendor_boot.img" \
      --init-boot="$o/init_boot.img" --append="nokaslr ${VETRO_AOSP_APPEND:-}" \
      --android-dump="$dump" --no-devices --mem=512 --guest-secs=0 >/dev/null || true
    [ -s "$dump/Image" ] && [ -s "$dump/initrd" ] || { echo "vetro boot --android-dump did not write $dump" >&2; exit 1; }
    vm "mkdir -p $rd/boot"
    vm_rsync -a --delete "$here/remote/" "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/remote/"
    vm_rsync -a --delete "$dump/" "$VETRO_AOSP_HOST:$rd/boot/"
    vm "$env VETRO_QEMU_EXTRA='${VETRO_QEMU_EXTRA:-}' bash $VETRO_AOSP_WORK/remote/qemu.sh start" ;;
  stop|status)
    vm "$env bash $VETRO_AOSP_WORK/remote/qemu.sh $1" ;;
  log)
    vm "tail -n ${2:-40} $rd/serial.log" ;;
  adb)
    shift
    vm "$adb_vm connect 127.0.0.1:$port >/dev/null 2>&1; $adb_vm -s 127.0.0.1:$port $*" ;;
  screendump)
    [ -n "${2:-}" ] || { echo "usage: $0 screendump F.png" >&2; exit 2; }
    vm "rm -f $rd/screen.ppm; exec 3<>/dev/tcp/127.0.0.1/$mon; printf 'screendump $rd/screen.ppm\n' >&3; for i in 1 2 3 4 5 6 7 8 9 10; do sleep 1; [ -s $rd/screen.ppm ] && break; done; sleep 1; exec 3>&-"
    vm_rsync -a "$VETRO_AOSP_HOST:$rd/screen.ppm" "$2.ppm"
    sips -s format png "$2.ppm" --out "$2" >/dev/null && rm -f "$2.ppm"
    echo "$2" ;;
  screencap)
    [ -n "${2:-}" ] || { echo "usage: $0 screencap F.png" >&2; exit 2; }
    vm "$adb_vm connect 127.0.0.1:$port >/dev/null 2>&1; $adb_vm -s 127.0.0.1:$port exec-out screencap -p" > "$2"
    echo "$2" ;;
  *) echo "usage: $0 start|status|stop|log [N]|adb ARGS|screendump F.png|screencap F.png" >&2; exit 2 ;;
esac
