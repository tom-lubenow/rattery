//! Guest-side runtime, compiled only for `wasm32-wasip2`.

pub mod backend;
pub mod bindings;
pub mod client;
pub mod events;
pub mod task;

pub use wstd;

/// The single-threaded async runtime the app runs on (re-exported from `wstd`).
pub mod runtime {
    pub use wstd::runtime::{AsyncPollable, Reactor, Task, block_on, spawn};
}

/// Timers and sleeps that cooperate with the runtime (re-exported from `wstd`).
pub mod time {
    pub use wstd::time::*;
}

/// A ratatui terminal that renders through the rattery host.
pub type Terminal = ratatui::Terminal<backend::RatteryBackend>;

/// The origin (`scheme://host[:port]`) this app was loaded from, if the host
/// knows it. Server functions are sent to this origin.
pub fn origin() -> Option<String> {
    bindings::terminal::origin()
}

/// Set the terminal window title.
pub fn set_title(title: &str) {
    bindings::terminal::set_title(title)
}

/// Run an app: configure server functions to talk to the app's origin, open
/// the terminal, and drive `app` to completion on the async runtime.
///
/// This is the whole `main` of a rattery app:
///
/// ```ignore
/// fn main() -> Result<(), Box<dyn std::error::Error>> {
///     rattery::run(my_app)
/// }
///
/// async fn my_app(mut terminal: rattery::Terminal) -> Result<(), Box<dyn std::error::Error>> {
///     // ...
/// }
/// ```
pub fn run<F, Fut, T>(app: F) -> T
where
    F: FnOnce(Terminal) -> Fut,
    Fut: Future<Output = T>,
{
    if let Some(origin) = origin() {
        let origin: &'static str = Box::leak(origin.into_boxed_str());
        let _ = server_fn::client::try_set_server_url(origin);
    }
    let terminal = Terminal::new(backend::RatteryBackend::new())
        .expect("rattery: the host refused to open a terminal");
    wstd::runtime::block_on(app(terminal))
}
