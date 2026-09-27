//! Only for wasm32: exports the module's function table and makes it
//! growable. The JS JIT engine puts the block dispatcher in it, which
//! Rust then calls as a function pointer, without going through JS
//! (`src/jit.rs`, ADR 0013).

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo:rustc-link-arg-cdylib=--export-table");
        println!("cargo:rustc-link-arg-cdylib=--growable-table");
    }
}
