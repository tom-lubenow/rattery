//! Guest-side runtime, compiled only for `wasm32-wasip2`.
//!
//! The app is a component-model async task: it exports one async `run`, the
//! host drives it with the async ABI, and every wait (input, HTTP, websockets,
//! timers) is a real `.await` that lets other tasks in the app make progress.

pub mod backend;
pub mod bindings;
pub mod client;
pub mod events;
pub mod task;
pub mod websocket;

use std::fmt::Display;
use std::future::Future;

/// A ratatui terminal that renders through the rattery host.
pub type Terminal = ratatui::Terminal<backend::RatteryBackend>;

/// The origin (`scheme://host[:port]`) server functions are sent to, if the
/// host knows it.
pub fn origin() -> Option<String> {
    bindings::terminal::origin()
}

/// The full URL this app was loaded from, query string included, if it was
/// loaded from one: the terminal's `window.location`. Parse it with the `url`
/// crate to read parameters.
pub fn location() -> Option<String> {
    bindings::terminal::location()
}

/// Set the terminal window title.
pub fn set_title(title: &str) {
    bindings::terminal::set_title(title)
}

/// Tell the host the app is ready for the user: data loaded, a real screen
/// showing. Optional; embedders may wait for it (`Phase::AppReady`) rather
/// than for the first frame.
pub fn ready() {
    bindings::terminal::ready()
}

/// Timers that cooperate with the runtime.
pub mod time {
    use std::time::Duration;

    /// Wait for `duration` without blocking other tasks.
    pub async fn sleep(duration: Duration) {
        let nanos = duration.as_nanos().min(u64::MAX as u128) as u64;
        wasip3::clocks::monotonic_clock::wait_for(nanos).await
    }
}

/// The runtime primitives, for code that wants them directly. Prefer
/// [`crate::task`] for spawning: it gives you a handle.
pub mod runtime {
    pub use wit_bindgen::spawn_local;
}

/// Implementation of [`crate::app!`]: configure server functions, open the
/// terminal, run the app, turn its error into a message for the host.
#[doc(hidden)]
pub async fn __run_app<F, Fut, E>(app: F) -> Result<(), String>
where
    F: FnOnce(Terminal) -> Fut,
    Fut: Future<Output = Result<(), E>>,
    E: Display,
{
    if let Some(origin) = origin() {
        let origin: &'static str = Box::leak(origin.into_boxed_str());
        let _ = server_fn::client::try_set_server_url(origin);
    }
    let terminal = Terminal::new(backend::RatteryBackend::new())
        .map_err(|e| format!("rattery: the host refused to open a terminal: {e}"))?;
    app(terminal).await.map_err(|e| e.to_string())
}
