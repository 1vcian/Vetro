# ADR 0002 — Pinned Rust nightly toolchain

- Status: accepted (M0, 2026-09-24)

## Context
WASM threads (shared memory, atomics, one Worker per virtual core)
require rebuilding the standard library with `-Z build-std` and the flags
`+atomics,+bulk-memory`, available only on nightly.

## Decision
`rust-toolchain.toml` pins `nightly-2026-09-15` with `rust-src`, `clippy`,
`rustfmt` and the `wasm32-unknown-unknown` target. In M0 the WASM build is the
standard one, without atomics; `build-std` with atomics comes in when the CPU
worker is born (M4/M5), with a dedicated ADR for the flags.

## Consequences
Reproducible builds. Updating the nightly requires an ADR (even a short one) and
green CI, so we don't discover compiler regressions in the middle of a milestone.
