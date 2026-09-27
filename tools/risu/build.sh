#!/bin/sh
# RISU for Vetro (ADR 0006): builds risu, generates images of ARMv8.0
# (Cortex-A53) instructions and records the reference traces under QEMU.
# Result in target/risu/: risu, <name>.bin, <name>.trace.
# The test tests/linux/tests/risu.rs then runs risu on Vetro as the apprentice.
#
# Variables: RISU_INSNS (instructions per image, default 3000).
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$ROOT/target/risu"
REPO="https://github.com/rth7680/risu.git"
COMMIT="eed224965e7bde899dda788c65020adbdab20e5e"
N="${RISU_INSNS:-3000}"
mkdir -p "$OUT"

# The CI cache may return a half-finished target/: if the clone is not healthy or
# doesn't have the commit, it is redone from scratch.
if ! { git -C "$OUT/src" fsck --no-progress --no-dangling >/dev/null 2>&1 &&
       git -C "$OUT/src" checkout -q "$COMMIT" 2>/dev/null; }; then
  rm -rf "$OUT/src"
  git clone -q "$REPO" "$OUT/src"
  git -C "$OUT/src" checkout -q "$COMMIT"
fi

# 1. static risu (musl), like the other guest binaries.
docker build -q -t vetro-guest-bins:latest "$ROOT/tools/guest-bins" >/dev/null
docker run --rm --platform linux/arm64 -v "$OUT:/out" -w /out/src vetro-guest-bins:latest sh -euc '
  rm -rf /tmp/b && mkdir /tmp/b && cd /tmp/b
  /out/src/configure --static >/dev/null
  make -s >/dev/null 2>&1 || make
  cp risu /out/risu
'

# 2. Images with risugen: only Cortex-A53 instructions (ARMv8.0 with
#    CRC32, AES, SHA1, SHA256), no later extensions. risugen has no
#    option for the seed: it is fixed with srand before running it, so the
#    images (and hence the test) are the same at every run.
EXCLUDE="A64_V8[1-9],LDAPR.*,LDAPUR.*,STLUR,SHA512.*,RAX1,SM3.*,SM4.*,EOR3,BCAX,XAR"
docker build -q -t vetro-oracle:latest "$ROOT/tools/oracle" >/dev/null
docker run --rm -v "$OUT:/out" -w /out/src vetro-oracle:latest sh -euc "
  gen() {
    perl -e 'srand(shift @ARGV); do \"./risugen\"; die \$@ if \$@;' \$2 --numinsns $N \$3 \\
      --not-pattern '$EXCLUDE' aarch64.risu /out/\$1.bin >/dev/null
  }
  gen int 1 '--group DataProcessingImmediate'
  gen intreg 2 '--group DataProcessingRegister'
  gen load 3 '--group Load'
  gen store 4 '--group Store'
  gen fp 5 '--group DataProcessingScalarFP'
  gen simd 6 '--group DataProcessingAdvSIMD'
  gen misto 7 ''
  # 3. Reference traces: QEMU as the master.
  for img in /out/*.bin; do
    qemu-aarch64 -cpu cortex-a53 /out/risu --master -t \${img%.bin}.trace \$img >/dev/null
  done
"
ls -la "$OUT" | grep -E "bin|trace|risu$"
