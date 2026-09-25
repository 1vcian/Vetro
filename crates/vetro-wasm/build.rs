//! Solo per wasm32: esporta la tabella delle funzioni del modulo e la rende
//! estendibile. Il motore JIT in JS vi mette il dispatcher dei blocchi, che
//! Rust chiama poi come un puntatore a funzione, senza passare da JS
//! (`src/jit.rs`, ADR 0013).

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo:rustc-link-arg-cdylib=--export-table");
        println!("cargo:rustc-link-arg-cdylib=--growable-table");
    }
}
