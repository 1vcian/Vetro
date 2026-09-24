#!/bin/sh
# RISU per Vetro (ADR 0006): compila risu, genera immagini di istruzioni
# ARMv8.0 (Cortex-A53) e registra le tracce di riferimento sotto QEMU.
# Risultato in target/risu/: risu, <nome>.bin, <nome>.trace.
# Il test tests/linux/tests/risu.rs esegue poi risu su Vetro come apprendista.
#
# Variabili: RISU_INSNS (istruzioni per immagine, default 3000).
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$ROOT/target/risu"
REPO="https://github.com/rth7680/risu.git"
COMMIT="eed224965e7bde899dda788c65020adbdab20e5e"
N="${RISU_INSNS:-3000}"
mkdir -p "$OUT"

if [ ! -d "$OUT/src/.git" ]; then
  git clone -q "$REPO" "$OUT/src"
fi
git -C "$OUT/src" checkout -q "$COMMIT"

# 1. risu statico (musl), come gli altri binari guest.
docker build -q -t vetro-guest-bins:latest "$ROOT/tools/guest-bins" >/dev/null
docker run --rm --platform linux/arm64 -v "$OUT:/out" -w /out/src vetro-guest-bins:latest sh -euc '
  rm -rf /tmp/b && mkdir /tmp/b && cd /tmp/b
  /out/src/configure --static >/dev/null
  make -s >/dev/null 2>&1 || make
  cp risu /out/risu
'

# 2. Immagini con risugen: solo istruzioni della Cortex-A53 (ARMv8.0 con
#    CRC32, AES, SHA1, SHA256), niente estensioni successive.
EXCLUDE="A64_V8[1-9],LDAPR.*,LDAPUR.*,STLUR,SHA512.*,RAX1,SM3.*,SM4.*,EOR3,BCAX,XAR"
docker build -q -t vetro-oracle:latest "$ROOT/tools/oracle" >/dev/null
docker run --rm -v "$OUT:/out" -w /out/src vetro-oracle:latest sh -euc "
  gen() { ./risugen --numinsns $N \$2 --not-pattern '$EXCLUDE' aarch64.risu /out/\$1.bin >/dev/null; }
  gen int '--group DataProcessingImmediate'
  gen intreg '--group DataProcessingRegister'
  gen load '--group Load'
  gen store '--group Store'
  gen fp '--group DataProcessingScalarFP'
  gen simd '--group DataProcessingAdvSIMD'
  gen misto ''
  # 3. Tracce di riferimento: QEMU come maestro.
  for img in /out/*.bin; do
    qemu-aarch64 -cpu cortex-a53 /out/risu --master -t \${img%.bin}.trace \$img >/dev/null
  done
"
ls -la "$OUT" | grep -E "bin|trace|risu$"
