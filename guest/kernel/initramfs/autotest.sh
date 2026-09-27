# Autotest of the Vetro initramfs (M3): a few checks that touch syscalls,
# virtual file systems and devices. Prints VETRO-AUTOTEST-FINE with the result.
# The check descriptions stay in Italian: they end up in the console log, which
# is compared with guest/kernel/reference/qemu-boot.log.
fail=0
check() {
  # check <description> <command...>: runs the command and records its result.
  desc=$1
  shift
  if "$@"; then
    echo "VETRO-AUTOTEST ok: $desc"
  else
    echo "VETRO-AUTOTEST ERRORE: $desc"
    fail=1
  fi
}
echo "VETRO-AUTOTEST-INIZIO"
uname -a
check "uname -m è aarch64" test "$(uname -m)" = aarch64
mount
check "proc montato" grep -q "^proc /proc proc" /proc/mounts
check "sysfs montato" grep -q "^sysfs /sys sysfs" /proc/mounts
check "devtmpfs montato" grep -q "^devtmpfs /dev devtmpfs" /proc/mounts
ls /sys
check "/sys/devices presente" test -d /sys/devices
check "console PL011" test -c /dev/ttyAMA0
check "RTC PL031" test -c /dev/rtc0
check "echo e pipe" test "$(echo vetro | tr a-z A-Z)" = VETRO
check "file su tmpfs" sh -c 'echo 42 > /tmp/x && test "$(cat /tmp/x)" = 42'
check "aritmetica della shell" test $((6 * 7)) = 42
# Boot layout as seen by the guest: comparison with the Vetro loader
# (crates/vetro-cli/src/boot.rs).
grep -i kernel /proc/iomem
echo "initrd-start: $(od -An -tx1 /proc/device-tree/chosen/linux,initrd-start)"
# M5: virtio-gpu, virtio-input and virtio-vsock, exercised by vetro-dev
# (guest/kernel/initramfs/vetro-dev.c). Only what is identical under QEMU:
# no CID and no vsock transport (QEMU in a container has no vhost-vsock).
check "DRM: /dev/dri/card0" test -c /dev/dri/card0
check "DRM: dumb buffer, modeset, dirtyfb e cursore" vetro-dev drm
conn=/sys/class/drm/card0-Virtual-1
echo "drm Virtual-1: $(cat $conn/status), $(cat $conn/enabled), modi: $(cat $conn/modes | tr '\n' ' ')"
check "DRM: modo preferito 1280x800" test "$(head -n 1 $conn/modes)" = 1280x800
echo "EDID:"
od -An -tx1 $conn/edid
check "input: tastiera e tablet" test -c /dev/input/event0 -a -c /dev/input/event1
cat /proc/bus/input/devices
check "input: capacità di evdev" vetro-dev input
check "vsock: /dev/vsock" test -c /dev/vsock
# Network (virtio-net): DHCP and ping to the gateway, with the same result under
# QEMU's user network (-netdev user) and under the Vetro stack (vetro-net).
# Only values that do not depend on timing: no counters and no milliseconds
# (udhcpc's messages, which count the attempts, go to /tmp).
check "rete: eth0" test -d /sys/class/net/eth0
echo "eth0: $(cat /sys/class/net/eth0/address)"
check "rete: DHCP" sh -c 'udhcpc -i eth0 -n -q -t 5 -T 2 2>/tmp/udhcpc.err'
route -n
cat /etc/resolv.conf
check "rete: ping al gateway" sh -c 'ping -c 2 -W 5 10.0.2.2 >/dev/null'
check "rete: ping al DNS" sh -c 'ping -c 1 -W 5 10.0.2.3 >/dev/null'
if [ $fail = 0 ]; then
  echo "VETRO-AUTOTEST-FINE: ok"
else
  echo "VETRO-AUTOTEST-FINE: errori"
fi
