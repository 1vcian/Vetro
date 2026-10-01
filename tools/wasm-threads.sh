#!/bin/sh
# The threads build of vetro-wasm (ADR 0041): atomics and bulk memory, the
# standard library rebuilt with them (-Z build-std, the pinned nightly of
# ADR 0002), the memory imported as a shared WebAssembly.Memory with a 4 GiB
# maximum. Same Rust sources and the same C API as the ordinary build: the JS
# loader (web/node/vetro.mjs) sees the memory import and creates the shared
# memory itself. Needs cross-origin isolation in the browser (COOP/COEP).
#
#   tools/wasm-threads.sh [--debug]
#
# Output: target/wasm32-unknown-unknown/release/vetro_wasm_threads.wasm (next to
# the ordinary vetro_wasm.wasm; --debug: the debug directory). Its own target
# directory (target/threads), so the ordinary build is never rebuilt with these
# flags.
set -eu
cd "$(dirname "$0")/.."

profile=release
flag=--release
for a in "$@"; do
  case "$a" in
    --debug) profile=debug; flag= ;;
    *) echo "usage: tools/wasm-threads.sh [--debug]" >&2; exit 2 ;;
  esac
done

# Initial memory: fixed, so the loader can create the shared memory before it
# instantiates the module (must cover the module's data and stack; the linker
# refuses a smaller value). Maximum: 4 GiB, the whole wasm32 address space (a
# shared memory cannot move, so its maximum is declared up front).
INITIAL=$((32 << 20))
MAX=4294967296
export VETRO_WASM_THREADS=1
export CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals \
-C link-arg=--shared-memory -C link-arg=--import-memory \
-C link-arg=--initial-memory=$INITIAL -C link-arg=--max-memory=$MAX \
-C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size \
-C link-arg=--export=__tls_align -C link-arg=--export=__tls_base"
# shellcheck disable=SC2086
cargo build $flag --target wasm32-unknown-unknown -p vetro-wasm \
  -Z build-std=std,panic_abort --target-dir target/threads
mkdir -p "target/wasm32-unknown-unknown/$profile"
cp "target/threads/wasm32-unknown-unknown/$profile/vetro_wasm.wasm" \
  "target/wasm32-unknown-unknown/$profile/vetro_wasm_threads.wasm"
echo "target/wasm32-unknown-unknown/$profile/vetro_wasm_threads.wasm"
