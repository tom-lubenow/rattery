//! A rattery app. Build it with:
//!
//! ```sh
//! cargo build -p counter-app --target wasm32-wasip2
//! ```
//!
//! The crate is a `cdylib`: `rattery::app!` exports the component's async
//! `run` and hands `app::run` a terminal.

#[cfg(target_os = "wasi")]
mod app;

#[cfg(target_os = "wasi")]
rattery::app!(app::run);
