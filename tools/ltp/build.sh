#!/bin/sh
# LTP for Vetro: builds (musl, static) a selection of the syscall tests
# of the Linux Test Project in target/ltp/bin. The test
# tests/linux/tests/ltp.rs runs them on Vetro and on QEMU and compares the outcome.
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$ROOT/target/ltp"
TAG="20260529"
mkdir -p "$OUT/bin"
# The CI cache may return a half-finished target/: if the clone is not healthy or
# not at the right tag, it is redone from scratch.
if ! { git -C "$OUT/src" fsck --no-progress --no-dangling >/dev/null 2>&1 &&
       [ "$(git -C "$OUT/src" describe --tags --exact-match 2>/dev/null)" = "$TAG" ]; }; then
  rm -rf "$OUT/src"
  git clone -q --depth 1 -b "$TAG" https://github.com/linux-test-project/ltp.git "$OUT/src"
fi
# Directories of testcases/kernel/syscalls to build.
DIRS="$(tr '\n' ' ' < "$ROOT/tools/ltp/syscalls.txt")"
docker build -q -t vetro-guest-bins:latest "$ROOT/tools/guest-bins" >/dev/null
docker run --rm --platform linux/arm64 -v "$OUT:/ltp" -w /ltp/src vetro-guest-bins:latest sh -euc "
  if [ ! -f configure ]; then make autotools >/dev/null 2>&1; fi
  if [ ! -f include/config.h ]; then ./configure LDFLAGS=-static >/dev/null; fi
  make -s -C lib >/dev/null 2>&1 || make -C lib
  for d in $DIRS; do
    if ! make -s -C testcases/kernel/syscalls/\$d >/tmp/log 2>&1; then
      echo \"compilazione fallita: \$d\"; tail -5 /tmp/log
      continue
    fi
    find testcases/kernel/syscalls/\$d -maxdepth 1 -type f -perm -u+x ! -name '*.sh' -exec cp {} /ltp/bin/ \;
  done
"
ls "$OUT/bin" | wc -l
