#!/bin/sh
# M4: la macchina in WebAssembly. Costruisce vetro-wasm in release per wasm32,
# prova il ponte JIT (web/node/jit-selftest.mjs), poi avvia il kernel guest
# con lo stesso copione di tests/boot/tests/vetro.rs due volte:
#   1. interprete nativo (cargo test --release -p vetro-boot-tests --test vetro);
#   2. interprete compilato a wasm, in Node/V8 (web/node/boot.mjs).
# Le istruzioni contate e il log della console devono essere identici (la
# macchina è deterministica). Stampa i due tempi.
#
# Serve Node >= 22 e target/guest-kernel (tools/guest-kernel/build.sh).
set -eu
cd "$(dirname "$0")/.."

command -v node >/dev/null || { echo "ERRORE: node non trovato (serve Node >= 22)" >&2; exit 1; }
major=$(node -p 'process.versions.node.split(".")[0]')
[ "$major" -ge 22 ] || { echo "ERRORE: Node $(node --version), serve >= 22" >&2; exit 1; }
[ -f target/guest-kernel/Image ] && [ -f target/guest-kernel/initramfs.cpio.gz ] \
  || { echo "ERRORE: target/guest-kernel mancante: esegui tools/guest-kernel/build.sh" >&2; exit 1; }

echo "==> vetro-wasm (release, wasm32-unknown-unknown)"
cargo build --release --target wasm32-unknown-unknown -p vetro-wasm
wasm=target/wasm32-unknown-unknown/release/vetro_wasm.wasm
echo "    $(wc -c < "$wasm" | tr -d ' ') byte"

echo "==> ponte JIT in Node"
node web/node/jit-selftest.mjs --wasm "$wasm"

echo "==> avvio con l'interprete nativo"
native_log=target/guest-kernel/native-boot-test.out
# Senza pipe: sh non ha pipefail, e l'esito del test deve contare.
status=0
cargo test --release -p vetro-boot-tests --test vetro -- --nocapture > "$native_log" 2>&1 || status=$?
cat "$native_log"
[ "$status" -eq 0 ] || exit "$status"
steps=$(sed -n 's/.*(\([0-9]*\) istruzioni).*/\1/p' "$native_log" | tail -n 1)
native_s=$(sed -n 's/^test result: ok\..*finished in \([0-9.]*\)s.*/\1/p' "$native_log" | tail -n 1)
[ -n "$steps" ] && [ -n "$native_s" ] || { echo "ERRORE: avvio nativo senza esito" >&2; exit 1; }

echo "==> avvio con l'interprete in Node $(node --version) (V8)"
status=0
node web/node/boot.mjs --wasm "$wasm" --expect-steps "$steps" > target/guest-kernel/node-boot.out || status=$?
cat target/guest-kernel/node-boot.out
[ "$status" -eq 0 ] || exit "$status"
node_ms=$(sed -n 's/^VETRO-NODE-BOOT .*ms=\([0-9]*\).*/\1/p' target/guest-kernel/node-boot.out)

cmp target/guest-kernel/vetro-boot.log target/guest-kernel/node-boot.log \
  || { echo "ERRORE: il log della console in Node differisce da quello nativo" >&2; exit 1; }

echo "==> tempi (stesse $steps istruzioni, stesso log)"
echo "    interprete nativo:   ${native_s} s (test intero: lettura dei file, caricamento, avvio)"
echo "    interprete in V8:    $(node -p "($node_ms / 1000).toFixed(2)") s (dal caricamento allo spegnimento)"
