#!/bin/sh
# Builds the guest test binaries (arm64, static, musl) in target/guest-bins:
# - tests/linux/c/*.c compiled with -static;
# - busybox.static from the Alpine package.
# Runs in an arm64 Alpine container (native on macOS Apple Silicon and on arm64
# runners). Usage: tools/guest-bins/build.sh
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
IMAGE="${VETRO_GUEST_IMAGE:-vetro-guest-bins:latest}"
OUT="$ROOT/target/guest-bins"
mkdir -p "$OUT"
docker build -q -t "$IMAGE" "$ROOT/tools/guest-bins" >/dev/null
docker run --rm --platform linux/arm64 -v "$ROOT:/src" -w /src "$IMAGE" sh -euc '
  out=/src/target/guest-bins
  for c in tests/linux/c/*.c; do
    name=$(basename "$c" .c)
    gcc -static -O2 -Wall -Werror -o "$out/$name" "$c"
  done
  cp /bin/busybox.static "$out/busybox"
  apk list --installed 2>/dev/null | grep -E "^(musl|busybox-static|gcc)-[0-9]" > "$out/VERSIONS"
'
ls "$OUT"
