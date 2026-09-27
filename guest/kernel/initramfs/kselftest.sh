# Runs the kselftests in /kselftest (kselftest initramfs, M3) with the BusyBox
# shell: the kernel runner (kselftest/runner.sh) uses constructs that ash
# does not understand. For each test in kselftest-list.txt prints the output
# prefixed with "# " and the result in the kernel TAP format:
#   ok N group:test | ok N group:test # SKIP | not ok N group:test # exit=R
# The time limit is in guest time (BusyBox timeout).
n=0
while IFS=: read -r dir test; do
  [ -n "$dir" ] || continue
  n=$((n + 1))
  echo "# selftests: $dir:$test"
  cd "/kselftest/$dir" || { echo "not ok $n $dir:$test # no directory"; continue; }
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
