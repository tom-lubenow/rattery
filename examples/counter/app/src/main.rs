//! A rattery app. Build it with:
//!
//! ```sh
//! cargo build -p counter-app --target wasm32-wasip2
//! ```

#[cfg(target_os = "wasi")]
mod app;

#[cfg(target_os = "wasi")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    rattery::run(app::run)
}

#[cfg(not(target_os = "wasi"))]
fn main() {
    eprintln!("counter-app is a rattery guest; build it with:");
    eprintln!("    cargo build -p counter-app --target wasm32-wasip2");
    std::process::exit(2);
}
