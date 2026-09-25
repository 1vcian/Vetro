#!/bin/sh
# M5: test dell'app web (web/app) e dell'API dei dispositivi di vetro-wasm,
# senza dipendenze npm:
#   1. controllo della sintassi dei moduli JS (node --check);
#   2. test unitari JS (tests/web/unit.mjs): server con Range, DiskFeeder,
#      mappa dei tasti, terminale;
#   3. riferimento nativo (tests/boot/tests/web.rs): gli stessi copioni con
#      l'API di vetro-wasm compilata per l'host, interprete, disco locale;
#   4. disco via HTTP Range (tests/web/boot-disk.mjs): il kernel M3 legge e
#      scrive un disco servito da un server locale; istruzioni e log uguali
#      a quelli con un disco locale e al riferimento nativo, anche col
#      riavvio dalla cache;
#   5. display e ingressi via API (tests/web/devices.mjs): framebuffer RGBA
#      uguale al motivo del guest, cursore, tastiera, tablet, LED; istruzioni
#      e log uguali al riferimento nativo;
#   6. l'app in Chrome headless (tests/web/browser.mjs), se Chrome c'è
#      (VETRO_CHROME; altrimenti SKIP, che non è un test passato;
#      VETRO_REQUIRE_BROWSER=1 lo rende un errore).
#
#   tools/web-test.sh [--no-jit]
#
# Serve Node >= 22 e target/guest-kernel (tools/guest-kernel/build.sh).
set -eu
cd "$(dirname "$0")/.."

for a in "$@"; do
  case "$a" in
    --no-jit) ;;
    *) echo "uso: tools/web-test.sh [--no-jit]" >&2; exit 2 ;;
  esac
done

command -v node >/dev/null || { echo "ERRORE: node non trovato (serve Node >= 22)" >&2; exit 1; }
major=$(node -p 'process.versions.node.split(".")[0]')
[ "$major" -ge 22 ] || { echo "ERRORE: Node $(node --version), serve >= 22" >&2; exit 1; }
[ -f target/guest-kernel/Image ] && [ -f target/guest-kernel/initramfs.cpio.gz ] \
  || { echo "ERRORE: target/guest-kernel mancante: esegui tools/guest-kernel/build.sh" >&2; exit 1; }

echo "==> vetro-wasm (release, wasm32-unknown-unknown)"
cargo build --release --target wasm32-unknown-unknown -p vetro-wasm

echo "==> sintassi dei moduli JS"
for f in web/app/*.mjs web/node/*.mjs tools/web-serve.mjs tests/web/*.mjs; do
  node --check "$f"
done

echo "==> test unitari JS"
node tests/web/unit.mjs

echo "==> riferimento nativo (stessi copioni, interprete, disco locale)"
cargo test --release -p vetro-boot-tests --test web -- --nocapture
# I test in Node devono dare le stesse istruzioni e lo stesso log.
export VETRO_WEB_NATIVE=1

echo "==> disco via HTTP Range"
node tests/web/boot-disk.mjs "$@"

echo "==> display e ingressi via API"
node tests/web/devices.mjs "$@"

echo "==> app in Chrome headless"
node tests/web/browser.mjs
