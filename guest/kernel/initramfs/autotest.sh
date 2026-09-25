# Autotest dell'initramfs di Vetro (M3): pochi controlli che toccano syscall,
# file system virtuali e dispositivi. Stampa VETRO-AUTOTEST-FINE con l'esito.
fail=0
check() {
  # check <descrizione> <comando...>: esegue il comando e ne segna l'esito.
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
# Layout di avvio visto dal guest: confronto con il caricatore di Vetro
# (crates/vetro-cli/src/boot.rs).
grep -i kernel /proc/iomem
echo "initrd-start: $(od -An -tx1 /proc/device-tree/chosen/linux,initrd-start)"
# M5: virtio-gpu, virtio-input e virtio-vsock, esercitati da vetro-dev
# (guest/kernel/initramfs/vetro-dev.c). Solo ciò che è uguale sotto QEMU:
# niente CID né trasporto vsock (QEMU in container non ha vhost-vsock).
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
if [ $fail = 0 ]; then
  echo "VETRO-AUTOTEST-FINE: ok"
else
  echo "VETRO-AUTOTEST-FINE: errori"
fi
