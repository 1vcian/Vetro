#!/bin/bash
# On the build VM (launched by tools/aosp/qemu-vm.sh): boots an AOSP image of
# Vetro under the VM's native qemu-system-aarch64 (the oracle), with the same
# machine and devices as tools/aosp/qemu.sh (Mac, Docker) and `vetro boot`.
#   remote/qemu.sh start   compose the disk if needed, start QEMU detached
#   remote/qemu.sh stop    stop it
#   remote/qemu.sh status  running or not, last serial lines
# Instance directory ~/$WORK/qemu/$VETRO_QEMU_NAME (default: aosp):
#   boot/{Image,initrd,cmdline}  from `vetro boot --android-dump` (the Mac
#                                sends them: Vetro's bootloader, ADR 0018)
#   out -> images (default ~/$WORK/out, the last tools/aosp/fetch.sh pack)
#   disk.img (+ disk.img.inputs)  tools/aosp/remote/disk-layout.sh
#   serial.log, qemu.pid
# adb on 127.0.0.1:$VETRO_ADB_PORT (default 5565), QEMU monitor (screendump)
# on 127.0.0.1:$VETRO_QEMU_MONITOR (default 4454), both on the VM only.
# The disk is copy-on-write (snapshot=on): every boot is a first boot.
set -euo pipefail
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
name="${VETRO_QEMU_NAME:-aosp}"
d="$work/qemu/$name"
images="${VETRO_AOSP_IMAGES:-$work/out}"
adb_port="${VETRO_ADB_PORT:-5565}"
mon_port="${VETRO_QEMU_MONITOR:-4454}"
qemu=qemu-system-aarch64
[ -x "$HOME/qemu/bin/qemu-system-aarch64" ] && qemu="$HOME/qemu/bin/qemu-system-aarch64"
mkdir -p "$d"
running() { [ -s "$d/qemu.pid" ] && kill -0 "$(cat "$d/qemu.pid")" 2>/dev/null; }
case "${1:-status}" in
  start)
    running && { echo "already running (pid $(cat "$d/qemu.pid"))" >&2; exit 1; }
    for f in Image initrd cmdline; do [ -s "$d/boot/$f" ] || { echo "missing $d/boot/$f" >&2; exit 1; }; done
    ln -sfn "$images" "$d/out"
    want="$(sha256sum "$images/super.img" "$images/userdata.img" "$work/remote/disk-layout.sh" | cut -d' ' -f1)"
    if [ ! -f "$d/disk.img" ] || [ "$(cat "$d/disk.img.inputs" 2>/dev/null)" != "$want" ]; then
      rm -f "$d/disk.img.inputs"
      (cd "$d" && PATH="$PATH:$tree/out/host/linux-x86/bin" sh "$work/remote/disk-layout.sh" >/dev/null)
      echo "$want" > "$d/disk.img.inputs"
    fi
    rm -f "$d/serial.log"
    # shellcheck disable=SC2086
    nohup setsid "$qemu" -M virt,gic-version=3,its=off -cpu cortex-a53 -smp 1 -m 3G \
      -display none -no-reboot -global virtio-mmio.force-legacy=false -nic none \
      -serial "file:$d/serial.log" -monitor "tcp:127.0.0.1:$mon_port,server,nowait" \
      -kernel "$d/boot/Image" -initrd "$d/boot/initrd" -append "$(cat "$d/boot/cmdline")" \
      -device virtio-gpu-device -device virtio-keyboard-device -device virtio-tablet-device \
      -netdev "user,id=net0,hostfwd=tcp:127.0.0.1:$adb_port-:5555" -device virtio-net-device,netdev=net0 \
      -drive "file=$d/disk.img,if=none,id=disk,format=raw,snapshot=on" -device virtio-blk-device,drive=disk \
      ${VETRO_QEMU_EXTRA:-} </dev/null >"$d/qemu.out" 2>&1 &
    echo $! > "$d/qemu.pid"
    date -u +%s > "$d/started"
    echo "started $name: $($qemu --version | head -n1), pid $(cat "$d/qemu.pid"), adb 127.0.0.1:$adb_port, monitor 127.0.0.1:$mon_port" ;;
  stop)
    if running; then kill "$(cat "$d/qemu.pid")"; echo "stopped $name"; else echo "$name not running"; fi
    rm -f "$d/qemu.pid" ;;
  status)
    if running; then echo "running (pid $(cat "$d/qemu.pid"), $(( $(date -u +%s) - $(cat "$d/started") )) s)"; else echo "not running"; cat "$d/qemu.out" 2>/dev/null | tail -n 3; fi
    tail -n 3 "$d/serial.log" 2>/dev/null | cut -c1-200 ;;
  *) echo "usage: $0 start|stop|status" >&2; exit 2 ;;
esac
