//! Draws one frame, then spins forever without reading input. Exists so the
//! host's kill switch (Ctrl-C three times) and headless timeout have
//! something to interrupt.

#[cfg(target_os = "wasi")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    rattery::run(app)
}

#[cfg(target_os = "wasi")]
async fn app(mut terminal: rattery::Terminal) -> Result<(), Box<dyn std::error::Error>> {
    terminal.draw(|frame| {
        frame.render_widget("spinning forever; press Ctrl-C three times", frame.area());
    })?;
    loop {
        std::hint::spin_loop();
    }
}

#[cfg(not(target_os = "wasi"))]
fn main() {
    eprintln!("spin-app is a rattery guest; build it with:");
    eprintln!("    cargo build -p spin-app --target wasm32-wasip2");
    std::process::exit(2);
}
