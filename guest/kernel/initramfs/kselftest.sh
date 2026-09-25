# Esegue i kselftest di /kselftest (initramfs dei kselftest, M3) con la shell
# di BusyBox: il runner del kernel (kselftest/runner.sh) usa costrutti che ash
# non capisce. Per ogni test di kselftest-list.txt stampa l'uscita con "# "
# davanti e l'esito nel formato TAP del kernel:
#   ok N gruppo:test | ok N gruppo:test # SKIP | not ok N gruppo:test # exit=R
# Il limite di tempo è in tempo del guest (timeout di BusyBox).
n=0
while IFS=: read -r dir test; do
  [ -n "$dir" ] || continue
  n=$((n + 1))
  echo "# selftests: $dir:$test"
  cd "/kselftest/$dir" || { echo "not ok $n $dir:$test # nessuna directory"; continue; }
  timeout -s KILL 300 "./$test" </dev/null >/tmp/kst.out 2>&1
  rc=$?
  sed 's/^/# /' /tmp/kst.out
  case $rc in
    0) echo "ok $n $dir:$test" ;;
    4) echo "ok $n $dir:$test # SKIP" ;;
    137) echo "not ok $n $dir:$test # TIMEOUT" ;;
    *) echo "not ok $n $dir:$test # exit=$rc" ;;
  esac
done < /kselftest/kselftest-list.txt
echo "VETRO-KSELFTEST-TOTALE: $n"
