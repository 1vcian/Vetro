#!/usr/bin/env bash
# Quali job della CI servono (ADR 0025). Stampa righe `nome=true|false` per
# $GITHUB_OUTPUT. Variabili (le imposta il job `changes` di ci.yml):
#   EVENT     push | pull_request | schedule | workflow_dispatch
#   BEFORE    commit precedente del push (tutti zeri se il branch è nuovo)
#   PR_BASE   base della pull request
#   MESSAGE   messaggio del commit: "[ci full]" forza tutto
# Localmente: EVENT=push BEFORE=<commit> tools/ci-changes.sh
#
# Categorie (un file può farne scattare più d'una):
#   rust    codice Rust, test, toolchain         -> native, wasm
#   linux   CPU, JIT, syscall Linux, LTP/RISU     -> linux (LTP completo, due volte)
#   boot    macchina, dispositivi, kernel guest   -> boot (avvio sotto QEMU e Vetro)
#   kernel  configurazione/initramfs del guest    -> kselftest dentro boot
#   web     app web, vetro-wasm, test web         -> boot ridotto (solo test web)
#   site    ciò che finisce su GitHub Pages       -> pages, deploy
# Solo documenti (docs/, *.md): nessun job pesante.
set -euo pipefail

all() {
  for k in rust linux boot kernel web site; do echo "$k=true"; done
  exit 0
}

case "${EVENT:-push}" in
  schedule|workflow_dispatch) all ;;
esac
case "${MESSAGE:-}" in
  *"[ci full]"*) all ;;
esac

base=${PR_BASE:-}
[ "${EVENT:-push}" = pull_request ] || base=${BEFORE:-}
if [ -z "$base" ] || [ "$base" = 0000000000000000000000000000000000000000 ] \
  || ! git cat-file -e "$base^{commit}" 2>/dev/null; then
  all
fi

files=$(git diff --name-only "$base" HEAD)
match() { grep -Eq "$1" <<<"$files" && echo true || echo false; }

# La CI stessa e questo script: tutto, per provarli davvero.
if grep -Eq '^(\.github/|tools/ci-changes\.sh$)' <<<"$files"; then all; fi

echo "rust=$(match '^(crates/|tests/[^/]+/(Cargo\.toml|src/|tests/|c/|[^/]+\.rs$)|Cargo\.(toml|lock)$|rust-toolchain\.toml$|\.cargo/)')"
echo "linux=$(match '^(crates/vetro-(cpu|mmu|jit|jit-native|cli)/|tests/(linux|diff|isa)/|tools/(ltp|risu|guest-bins|oracle)/|Cargo\.lock$|rust-toolchain\.toml$)')"
echo "boot=$(match '^(crates/|tests/boot/|guest/kernel/|tools/(guest-kernel|guest-bins|analysis)/|tools/wasm-boot\.sh$|Cargo\.lock$|rust-toolchain\.toml$)')"
echo "kernel=$(match '^(guest/kernel/(config|initramfs|kselftest)/|tools/guest-kernel/|tools/guest-bins/)')"
echo "web=$(match '^(web/|tests/web/|crates/vetro-(wasm|analysis|machine)/|tools/(web-test\.sh|web-serve\.mjs|pages/))')"
echo "site=$(match '^(web/|crates/|guest/kernel/|tools/(pages|guest-kernel)/|tests/web/pages\.mjs$|Cargo\.lock$)')"
