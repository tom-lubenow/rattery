//! # rattery
//!
//! Write a [ratatui] app, compile it to a WASI 0.2 component, and let the
//! `rattery` host run it inside a real terminal, sandboxed like a web page.
//! Backend calls go through [`server_fn`] exactly as they do in Leptos or
//! Dioxus fullstack: declare `#[rattery::server]` functions in a crate shared
//! by the app and the server, call them as plain `async fn`s from the app.
//!
//! ```ignore
//! use rattery::prelude::*;
//!
//! #[rattery::server]
//! async fn hello(name: String) -> Result<String, ServerFnError> {
//!     Ok(format!("hello, {name}"))
//! }
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     rattery::run(|mut terminal| async move {
//!         let greeting = hello("rattery".into()).await?;
//!         loop {
//!             terminal.draw(|frame| frame.render_widget(greeting.as_str(), frame.area()))?;
//!             if let Event::Key(key) = rattery::event::next().await {
//!                 if key.code == KeyCode::Char('q') { break Ok(()) }
//!             }
//!         }
//!     })
//! }
//! ```
//!
//! On `wasm32-wasip2` this crate provides the terminal backend, the event
//! stream, the async runtime, and the `wasi:http` server-function client. On
//! native targets it provides only what the server build of a shared crate
//! needs: the macro, `server_fn`, and a stub client.

pub use ratatui;
pub use rattery_macros::server;
pub use server_fn;
pub use server_fn::ServerFnError;

pub mod event;

#[cfg(target_os = "wasi")]
mod wasi;
#[cfg(target_os = "wasi")]
pub use wasi::{
    Terminal, backend::RatteryBackend, client::ServerFnClient, origin, run, runtime, set_title,
    task, time, wstd,
};

#[cfg(not(target_os = "wasi"))]
mod native;
#[cfg(not(target_os = "wasi"))]
pub use native::client::ServerFnClient;

/// The usual imports for writing an app.
pub mod prelude {
    pub use crate::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    pub use crate::{ServerFnError, server};
    pub use ratatui::prelude::*;

    #[cfg(target_os = "wasi")]
    pub use crate::{Terminal, task::Task};
}
