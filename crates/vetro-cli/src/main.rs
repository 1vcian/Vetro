//! Runner nativo headless. In M1 eseguirà ELF statici AArch64.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => println!("vetro {}", env!("CARGO_PKG_VERSION")),
        _ => {
            eprintln!("uso: vetro --version");
            eprintln!("(l'esecuzione di ELF arriva con M1)");
            std::process::exit(2);
        }
    }
}
