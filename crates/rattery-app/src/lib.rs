//! # rattery-app
//!
//! Write a [ratatui] app, compile it to a WASI 0.2 component, and let the
//! `rattery` host run it inside a real terminal, sandboxed like a web page.
//! Backend calls go through [`server_fn`] exactly as they do in Leptos or
//! Dioxus fullstack: declare `#[rattery_app::server]` functions in a crate shared
//! by the app and the server, call them as plain `async fn`s from the app.
//!
//! ```ignore
//! use rattery_app::prelude::*;
//!
//! #[rattery_app::server]
//! async fn hello(name: String) -> Result<String, ServerFnError> {
//!     Ok(format!("hello, {name}"))
//! }
//!
//! rattery_app::app!(run);
//!
//! async fn run(mut terminal: Terminal) -> Result<(), Box<dyn std::error::Error>> {
//!     let greeting = hello("rattery".into()).await?;
//!     loop {
//!         terminal.draw(|frame| frame.render_widget(greeting.as_str(), frame.area()))?;
//!         if let Event::Key(key) = rattery_app::event::next().await {
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

pub use log;
pub use ratatui;
pub use rattery_macros::server;
pub use server_fn;
pub use server_fn::ServerFnError;

/// The host contract this crate targets; must equal `rattery::ABI` of the
/// host that runs the app. Record it in release metadata.
pub const ABI: &str = "rattery:tui@0.4.0;cm-async;wasi:http@0.3.0";

pub mod event;
pub mod multipart;

#[cfg(target_os = "wasi")]
mod wasi;
#[cfg(target_os = "wasi")]
#[doc(hidden)]
pub use wasi::__run_app;
#[cfg(target_os = "wasi")]
pub use wasi::{
    Terminal, backend::RatteryBackend, bindings, client::ServerFnClient, location, origin, ready,
    runtime, set_title, storage, task, time, update, update::reload, websocket,
};

/// Declare the app's entry point: an `async fn(Terminal) -> Result<(), E>`.
///
/// Put this at the root of an ordinary binary crate built for
/// `wasm32-wasip2`. It exports the component's async `run`, wired to your
/// function, and supplies the `main` a binary crate needs. The host never
/// calls that `main`: the app is a component-model async task, and a
/// synchronous `main` could not await anything.
///
/// ```ignore
/// rattery_app::app!(run);
///
/// async fn run(mut terminal: rattery_app::Terminal) -> Result<(), Box<dyn std::error::Error>> {
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

        #[allow(dead_code)]
        fn main() {
            eprintln!("this is a rattery app; it runs inside a rattery host");
            std::process::exit(2);
        }
    };
}

/// On native targets there is nothing to export; the macro only supplies a
/// `main` that says how to build the app, so a native `cargo build` of the
/// workspace still succeeds.
#[cfg(not(target_os = "wasi"))]
#[macro_export]
macro_rules! app {
    ($run:path) => {
        #[allow(dead_code)]
        fn main() {
            eprintln!("this is a rattery app; build it with `cargo build --target wasm32-wasip2`");
            std::process::exit(2);
        }
    };
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
