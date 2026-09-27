#!/usr/bin/env bash
# Which CI jobs are needed (ADR 0025). Prints `name=true|false` lines for
# $GITHUB_OUTPUT. Variables (set by the `changes` job in ci.yml):
#   EVENT     push | pull_request | schedule | workflow_dispatch
#   BEFORE    commit before the push (all zeros if the branch is new)
#   PR_BASE   base of the pull request
#   MESSAGE   commit message: "[ci full]" forces everything
# Locally: EVENT=push BEFORE=<commit> tools/ci-changes.sh
#
# Categories (one file can trigger more than one):
#   rust    Rust code, tests, toolchain           -> native, wasm
#   linux   CPU, JIT, Linux syscalls, LTP/RISU    -> linux (full LTP, twice)
#   boot    machine, devices, guest kernel        -> boot (boot under QEMU and Vetro)
#   kernel  guest configuration/initramfs         -> kselftest inside boot
#   web     web app, vetro-wasm, web tests        -> reduced boot (web tests only)
#   site    whatever ends up on GitHub Pages      -> pages, deploy
# Documents only (docs/, *.md): no heavy job.
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

# The CI itself and this script: everything, to really exercise them.
if grep -Eq '^(\.github/|tools/ci-changes\.sh$)' <<<"$files"; then all; fi

echo "rust=$(match '^(crates/|tests/[^/]+/(Cargo\.toml|src/|tests/|c/|[^/]+\.rs$)|Cargo\.(toml|lock)$|rust-toolchain\.toml$|\.cargo/)')"
echo "linux=$(match '^(crates/vetro-(cpu|mmu|jit|jit-native|cli)/|tests/(linux|diff|isa)/|tools/(ltp|risu|guest-bins|oracle)/|Cargo\.lock$|rust-toolchain\.toml$)')"
echo "boot=$(match '^(crates/|tests/boot/|guest/kernel/|tools/(guest-kernel|guest-bins|analysis)/|tools/wasm-boot\.sh$|Cargo\.lock$|rust-toolchain\.toml$)')"
echo "kernel=$(match '^(guest/kernel/(config|initramfs|kselftest)/|tools/guest-kernel/|tools/guest-bins/)')"
echo "web=$(match '^(web/|tests/web/|crates/vetro-(wasm|analysis|machine)/|tools/(web-test\.sh|web-serve\.mjs|pages/))')"
echo "site=$(match '^(web/|crates/|guest/kernel/|tools/(pages|guest-kernel)/|tests/web/(pages|chrome|lib)\.mjs$|Cargo\.lock$)')"
