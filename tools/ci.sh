#!/bin/sh
# Runs locally the same checks the CI runs on every push.
# On macOS, for the oracle: export VETRO_QEMU_AARCH64="$PWD/tools/oracle/qemu-aarch64-docker.sh"
set -eu
cd "$(dirname "$0")/.."
echo "==> fmt";    cargo fmt --all --check
echo "==> clippy"; cargo clippy --workspace --all-targets -- -D warnings
echo "==> test";   cargo test --workspace
echo "==> wasm";   cargo build --target wasm32-unknown-unknown --workspace --exclude vetro-cli --exclude vetro-diff --exclude vetro-isa-tests --exclude vetro-linux-tests --exclude vetro-jit-native
echo "OK"
