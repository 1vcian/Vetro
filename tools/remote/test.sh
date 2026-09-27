#!/bin/sh
# Run heavy tests on the build VM instead of the Mac (native QEMU, no Docker,
# no false oracle timeouts, the Mac stays cool).
#
#   tools/remote/test.sh [--dir DIR] -- <command...>
#   tools/remote/test.sh -- cargo test --release -p vetro-boot-tests
#   tools/remote/test.sh -- tools/web-test.sh
#
# The working tree (default: the current directory, so it works from agent
# worktrees too) is synced with rsync to ~/vetro-test/<name> on the VM, without
# target/ except the guest artifacts the tests need (target/guest-kernel,
# target/guest-bins, target/aosp/out), then the command runs there with the
# oracle variables set for native QEMU. Output streams back; the exit code is
# the command's.
#
# The VM address is in target/aosp/vm-host of the main repository (vetro@IP).
# The VM is started and stopped by the owner: if it's off, this script says so
# and exits with 3; run the light checks locally and leave the heavy ones to CI.
set -eu

dir=$(pwd)
while [ $# -gt 0 ]; do
  case $1 in
    --dir) dir=$2; shift 2 ;;
    --) shift; break ;;
    *) echo "usage: $0 [--dir DIR] -- <command...>" >&2; exit 2 ;;
  esac
done
[ $# -gt 0 ] || { echo "usage: $0 [--dir DIR] -- <command...>" >&2; exit 2; }

main=$(git -C "$dir" rev-parse --path-format=absolute --git-common-dir | sed 's|/\.git$||')
host=$(cat "$main/target/aosp/vm-host" 2>/dev/null || true)
[ -n "$host" ] || { echo "no VM address in $main/target/aosp/vm-host" >&2; exit 3; }
key=$HOME/.ssh/vetro_aosp
ssh_="ssh -i $key -o ConnectTimeout=10 -o BatchMode=yes"
if ! $ssh_ "$host" true 2>/dev/null; then
  echo "build VM $host is not reachable (off?): run light checks locally, heavy ones in CI" >&2
  exit 3
fi

name=$(basename "$dir")
remote=vetro-test/$name
$ssh_ "$host" "mkdir -p $remote"
rsync -a --delete -e "$ssh_" \
  --exclude /target/ --exclude .git/ \
  "$dir/" "$host:$remote/"
for t in guest-kernel guest-bins aosp/out; do
  src=$dir/target/$t
  [ -d "$src" ] || src=$main/target/$t
  [ -d "$src" ] || continue
  $ssh_ "$host" "mkdir -p $remote/target/$t"
  rsync -a --delete -e "$ssh_" "$src/" "$host:$remote/target/$t/"
done

# Arguments are quoted for the remote shell.
cmd=
for a in "$@"; do cmd="$cmd '$(printf %s "$a" | sed "s/'/'\\\\''/g")'"; done
exec $ssh_ "$host" "cd $remote && . \$HOME/.cargo/env 2>/dev/null; \
  export PATH=\$HOME/qemu/bin:\$PATH; export VETRO_QEMU_AARCH64=\$(command -v qemu-aarch64) VETRO_QEMU_SYSTEM_AARCH64=\$(command -v qemu-system-aarch64) \
  VETRO_CHROME=\$(command -v google-chrome); $cmd"
