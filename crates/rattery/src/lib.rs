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
//! rattery::app!(run);
//!
//! async fn run(mut terminal: Terminal) -> Result<(), Box<dyn std::error::Error>> {
//!     let greeting = hello("rattery".into()).await?;
//!     loop {
//!         terminal.draw(|frame| frame.render_widget(greeting.as_str(), frame.area()))?;
//!         if let Event::Key(key) = rattery::event::next().await {
//!             if key.code == KeyCode::Char('q') { break Ok(()) }
//!         }
//!     }
//! }
//! ```
//!
//! On `wasm32-wasip2` this crate provides the terminal backend, the event
//! stream, background tasks, websockets, and the `wasi:http` server-function
//! client, all on the component model's async ABI. On native targets it
//! provides only what the server build of a shared crate needs: the macro,
//! `server_fn`, and a stub client.

pub use ratatui;
pub use rattery_macros::server;
pub use server_fn;
pub use server_fn::ServerFnError;

pub mod event;

#[cfg(target_os = "wasi")]
mod wasi;
#[cfg(target_os = "wasi")]
#[doc(hidden)]
pub use wasi::__run_app;
#[cfg(target_os = "wasi")]
pub use wasi::{
    Terminal, backend::RatteryBackend, bindings, client::ServerFnClient, location, origin, runtime,
    set_title, task, time, websocket,
};

/// Declare the app's entry point: an `async fn(Terminal) -> Result<(), E>`.
///
/// The crate must be a `cdylib` built for `wasm32-wasip2`; this macro exports
/// the component's async `run` and wires it to your function.
///
/// ```ignore
/// rattery::app!(run);
///
/// async fn run(mut terminal: rattery::Terminal) -> Result<(), Box<dyn std::error::Error>> {
///     // ...
/// }
/// ```
#[cfg(target_os = "wasi")]
#[macro_export]
macro_rules! app {
    ($run:path) => {
        struct __RatteryApp;

        impl $crate::bindings::Guest for __RatteryApp {
            async fn run() -> Result<(), String> {
                $crate::__run_app($run).await
            }
        }

        $crate::bindings::export!(__RatteryApp with_types_in $crate::bindings);
    };
}

/// On native targets there is nothing to export; the macro expands to nothing
/// so a shared crate can still compile the app module if it wants to.
#[cfg(not(target_os = "wasi"))]
#[macro_export]
macro_rules! app {
    ($run:path) => {};
}

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
