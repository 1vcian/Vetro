# ADR 0002 — Toolchain Rust nightly fissato

- Stato: accettata (M0, 2026-09-24)

## Contesto
I thread WASM (memoria condivisa, atomics, un Worker per core virtuale)
richiedono di ricompilare la libreria standard con `-Z build-std` e i flag
`+atomics,+bulk-memory`, disponibili solo su nightly.

## Decisione
`rust-toolchain.toml` fissa `nightly-2026-09-15` con `rust-src`, `clippy`,
`rustfmt` e il target `wasm32-unknown-unknown`. In M0 la build WASM è quella
standard, senza atomics; `build-std` con atomics entra quando nasce il worker
CPU (M4/M5), con un ADR dedicato ai flag.

## Conseguenze
Build riproducibili. Aggiornare il nightly richiede un ADR (anche breve) e la
CI verde, per non scoprire regressioni del compilatore a metà milestone.
