//! A rattery app: an ordinary binary crate. Build it with:
//!
//! ```sh
//! cargo build -p counter-app --target wasm32-wasip2
//! ```

#[cfg(target_os = "wasi")]
mod app;

#[cfg(target_os = "wasi")]
rattery_app::app!(app::run);

#[cfg(not(target_os = "wasi"))]
rattery_app::app!(unused);
