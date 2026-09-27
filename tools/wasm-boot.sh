#!/bin/sh
# M4: the machine in WebAssembly. Builds vetro-wasm in release for wasm32,
# tests the JIT bridge (web/node/jit-selftest.mjs), then boots the guest kernel
# with the same script as tests/boot/tests/vetro.rs:
#   1. native interpreter (cargo test --release -p vetro-boot-tests --test vetro);
#   2. interpreter compiled to wasm, in Node/V8 (web/node/boot.mjs);
#   3. with --jit, also the system-mode JIT in Node/V8
#      (web/node/boot.mjs --jit, ADR 0013).
# The counted instructions and the console log must be identical (the
# machine is deterministic, with or without the JIT). Prints the timings.
#
# With --jit it fails if the boot with the JIT in V8 takes longer than the boot with
# the native interpreter measured here (the M4 threshold, ADR 0012). The timings
# compared are those from kernel load to power-off.
#
#   tools/wasm-boot.sh [--jit]
#
# Needs Node >= 22 and target/guest-kernel (tools/guest-kernel/build.sh).
set -eu
cd "$(dirname "$0")/.."

jit=0
for a in "$@"; do
  case "$a" in
    --jit) jit=1 ;;
    *) echo "usage: tools/wasm-boot.sh [--jit]" >&2; exit 2 ;;
  esac
done

command -v node >/dev/null || { echo "ERROR: node not found (Node >= 22 needed)" >&2; exit 1; }
major=$(node -p 'process.versions.node.split(".")[0]')
[ "$major" -ge 22 ] || { echo "ERROR: Node $(node --version), >= 22 needed" >&2; exit 1; }
[ -f target/guest-kernel/Image ] && [ -f target/guest-kernel/initramfs.cpio.gz ] \
  || { echo "ERROR: target/guest-kernel missing: run tools/guest-kernel/build.sh" >&2; exit 1; }

echo "==> vetro-wasm (release, wasm32-unknown-unknown)"
cargo build --release --target wasm32-unknown-unknown -p vetro-wasm
wasm=target/wasm32-unknown-unknown/release/vetro_wasm.wasm
echo "    $(wc -c < "$wasm" | tr -d ' ') bytes"

echo "==> JIT bridge in Node"
node web/node/jit-selftest.mjs --wasm "$wasm"

echo "==> boot with the native interpreter"
native_log=target/guest-kernel/native-boot-test.out
# No pipe: sh has no pipefail, and the test's outcome must count. Only
# the interpreter: VETRO_JIT is not needed here.
status=0
env -u VETRO_JIT cargo test --release -p vetro-boot-tests --test vetro -- --nocapture > "$native_log" 2>&1 || status=$?
cat "$native_log"
[ "$status" -eq 0 ] || exit "$status"
steps=$(sed -n 's/.*(\([0-9]*\) instructions).*/\1/p' "$native_log" | tail -n 1)
native_s=$(sed -n 's/^interpreter: \([0-9.]*\) s$/\1/p' "$native_log" | tail -n 1)
[ -n "$steps" ] && [ -n "$native_s" ] || { echo "ERROR: native boot without an outcome" >&2; exit 1; }

echo "==> boot with the interpreter in Node $(node --version) (V8)"
status=0
node web/node/boot.mjs --wasm "$wasm" --expect-steps "$steps" > target/guest-kernel/node-boot.out || status=$?
cat target/guest-kernel/node-boot.out
[ "$status" -eq 0 ] || exit "$status"
node_ms=$(sed -n 's/^VETRO-NODE-BOOT .*ms=\([0-9]*\).*/\1/p' target/guest-kernel/node-boot.out)

cmp target/guest-kernel/vetro-boot.log target/guest-kernel/node-boot.log \
  || { echo "ERROR: the console log in Node differs from the native one" >&2; exit 1; }

if [ "$jit" -eq 1 ]; then
  echo "==> boot with the JIT in Node $(node --version) (V8)"
  status=0
  node web/node/boot.mjs --wasm "$wasm" --jit --expect-steps "$steps" > target/guest-kernel/node-boot-jit.out \
    || status=$?
  cat target/guest-kernel/node-boot-jit.out
  [ "$status" -eq 0 ] || exit "$status"
  jit_ms=$(sed -n 's/^VETRO-NODE-BOOT .*ms=\([0-9]*\).*/\1/p' target/guest-kernel/node-boot-jit.out)
  cmp target/guest-kernel/vetro-boot.log target/guest-kernel/node-boot-jit.log \
    || { echo "ERROR: the console log with the JIT in Node differs from the native one" >&2; exit 1; }
fi

echo "==> timings (same $steps instructions, same log; from load to power-off)"
echo "    native interpreter:  ${native_s} s"
echo "    interpreter in V8:   $(node -p "($node_ms / 1000).toFixed(2)") s"
if [ "$jit" -eq 1 ]; then
  echo "    JIT in V8:           $(node -p "($jit_ms / 1000).toFixed(2)") s"
  # M4 threshold: the JIT in V8 no slower than the native interpreter.
  if [ "$(node -p "$jit_ms / 1000 <= $native_s")" != true ]; then
    echo "ERROR: the JIT in V8 ($(node -p "($jit_ms / 1000).toFixed(2)") s) is slower than the native interpreter (${native_s} s)" >&2
    exit 1
  fi
fi
