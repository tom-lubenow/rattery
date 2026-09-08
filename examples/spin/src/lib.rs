//! Draws one frame, then spins forever without reading input. Exists so the
//! host's kill switch (Ctrl-C three times) and headless timeout have
//! something to interrupt.

#[cfg(target_os = "wasi")]
rattery::app!(run);

#[cfg(target_os = "wasi")]
async fn run(mut terminal: rattery::Terminal) -> Result<(), Box<dyn std::error::Error>> {
    terminal.draw(|frame| {
        frame.render_widget("spinning forever; press Ctrl-C three times", frame.area());
    })?;
    loop {
        std::hint::spin_loop();
    }
}
