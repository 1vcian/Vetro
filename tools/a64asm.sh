#!/bin/sh
# Assembla istruzioni AArch64 (una per riga, da stdin) e stampa le codifiche
# come righe Rust: `0x91000420, // add x0, x1, #1`.
# Serve a scrivere i test in tests/isa con codifiche da un assembler vero.
# Richiede clang con il backend AArch64 (Apple clang va bene) e llvm-objdump.
set -eu
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cat > "$tmp/in.s"
clang --target=aarch64-linux-gnu -march=armv8-a+crc -c "$tmp/in.s" -o "$tmp/in.o"
OBJDUMP="$(command -v llvm-objdump || xcrun --find llvm-objdump)"
# Una rilocazione lascerebbe un campo a zero nella codifica: meglio fallire.
if "$OBJDUMP" -r "$tmp/in.o" | grep -q "R_AARCH64"; then
  echo "a64asm: l'assembly produce rilocazioni (usa offset numerici):" >&2
  "$OBJDUMP" -r "$tmp/in.o" | grep "R_AARCH64" >&2
  exit 1
fi
"$OBJDUMP" -d "$tmp/in.o" | awk -F'\t' '/^ *[0-9a-f]+:/ {
  split($1, a, ":"); raw = a[2]; gsub(/^ +| +$/, "", raw);
  # llvm-objdump stampa le istruzioni come parola a 32 bit ("f8200020") e i
  # dati (.word) come byte in ordine di memoria ("ef cd ab 89").
  n = split(raw, b, " ");
  if (n == 4) w = b[4] b[3] b[2] b[1]; else w = raw;
  asm = $2; for (i = 3; i <= NF; i++) asm = asm " " $i;
  printf "0x%s, // %s\n", w, asm }'
