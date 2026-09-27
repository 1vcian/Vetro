#!/bin/sh
# Assembles AArch64 instructions (one per line, from stdin) and prints the encodings
# as Rust lines: `0x91000420, // add x0, x1, #1`.
# Used to write the tests in tests/isa with encodings from a real assembler.
# Requires clang with the AArch64 backend (Apple clang is fine) and llvm-objdump.
set -eu
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cat > "$tmp/in.s"
clang --target=aarch64-linux-gnu -march=armv8-a+crc+crypto -c "$tmp/in.s" -o "$tmp/in.o"
OBJDUMP="$(command -v llvm-objdump || xcrun --find llvm-objdump)"
# A relocation would leave a zero field in the encoding: better to fail.
if "$OBJDUMP" -r "$tmp/in.o" | grep -q "R_AARCH64"; then
  echo "a64asm: the assembly produces relocations (use numeric offsets):" >&2
  "$OBJDUMP" -r "$tmp/in.o" | grep "R_AARCH64" >&2
  exit 1
fi
"$OBJDUMP" -d "$tmp/in.o" | awk -F'\t' '/^ *[0-9a-f]+:/ {
  split($1, a, ":"); raw = a[2]; gsub(/^ +| +$/, "", raw);
  # llvm-objdump prints instructions as a 32-bit word ("f8200020") and
  # data (.word) as bytes in memory order ("ef cd ab 89").
  n = split(raw, b, " ");
  if (n == 4) w = b[4] b[3] b[2] b[1]; else w = raw;
  asm = $2; for (i = 3; i <= NF; i++) asm = asm " " $i;
  printf "0x%s, // %s\n", w, asm }'
