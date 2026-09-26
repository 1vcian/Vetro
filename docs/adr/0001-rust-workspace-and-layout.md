# ADR 0001 — Rust workspace and repository layout

- Status: accepted (M0, 2026-09-24)

## Context
The core of the emulator must compile both for `wasm32-unknown-unknown` (browser)
and natively (tests with `cargo test`, comparison with QEMU, CI without a browser).

## Decision
A single Cargo workspace. One crate per component in `crates/`
(`vetro-cpu`, `vetro-mmu`, `vetro-jit`, `vetro-platform`, `vetro-net`,
`vetro-analysis`, `vetro-snapshot`, `vetro-wasm`, `vetro-cli`). The differential
harness is the `vetro-diff` crate in `tests/diff`. Edition 2024,
license inherited from the workspace (PolyForm Noncommercial 1.0.0, see ADR 0004).

The core crates do not use `std::process`, `std::fs` or host threads
directly: they must compile to WASM (CI checks this). `vetro-cli` and
`vetro-diff` are native only.

## Consequences
The same code base for native and WASM, so no drift between the two.
Dependencies between crates are declared in the specs in `docs/specs/`.
