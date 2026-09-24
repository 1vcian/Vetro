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
if [ $fail = 0 ]; then
  echo "VETRO-AUTOTEST-FINE: ok"
else
  echo "VETRO-AUTOTEST-FINE: errori"
fi
