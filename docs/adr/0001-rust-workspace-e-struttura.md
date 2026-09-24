# ADR 0001 — Workspace Rust e struttura del repository

- Stato: accettata (M0, 2026-09-24)

## Contesto
Il cuore dell'emulatore deve compilare sia in `wasm32-unknown-unknown` (browser)
sia in nativo (test con `cargo test`, confronto con QEMU, CI senza browser).

## Decisione
Un unico workspace Cargo. Un crate per componente in `crates/`
(`vetro-cpu`, `vetro-mmu`, `vetro-jit`, `vetro-platform`, `vetro-net`,
`vetro-analysis`, `vetro-snapshot`, `vetro-wasm`, `vetro-cli`). L'harness
differenziale è il crate `vetro-diff` in `tests/diff`. Edizione 2024,
licenza ereditata dal workspace (PolyForm Noncommercial 1.0.0, vedi ADR 0004).

I crate del core non usano `std::process`, `std::fs` o thread dell'host
direttamente: devono compilare in WASM (la CI lo verifica). `vetro-cli` e
`vetro-diff` sono solo nativi.

## Conseguenze
Stessa base di codice per nativo e WASM, quindi niente deriva tra le due.
Le dipendenze tra crate si dichiarano nelle spec in `docs/specs/`.
