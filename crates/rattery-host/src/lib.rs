//! # rattery-host
//!
//! Run a [rattery](https://github.com/tom-lubenow/rattery) app, a ratatui app
//! compiled to a WASI 0.2 component, inside the current terminal with the
//! isolation a browser gives a web page. This crate is both the `rattery`
//! command and a library, so an existing CLI can embed a remote TUI:
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use rattery_host::App;
//!
//! let report = App::from_url("https://apps.example.com/dashboard/app.wasm")?
//!     .allow_origin("https://api.example.com")
//!     .run()
//!     .await?;
//! std::process::exit(report.exit_code());
//! # }
//! ```
//!
//! Or ship one specific app against one specific backend, with the component
//! embedded in your binary:
//!
//! ```ignore
//! use rattery_host::App;
//!
//! let report = App::from_bytes(include_bytes!("../app.wasm").to_vec())
//!     .origin("https://api.example.com")
//!     .run_blocking()?;
//! ```
//!
//! ## Origin policy
//!
//! An app may make HTTP requests to its own origin: where it was loaded from,
//! or the [`origin`](App::origin) you give an app loaded from bytes or a file.
//! [`allow_origin`](App::allow_origin) adds more, [`allow_all_origins`](App::allow_all_origins)
//! removes the check, and [`cors`](App::cors) lets other origins opt in
//! themselves with an `Access-Control-Allow-Origin` header, the way browsers
//! do. Everything else is refused before a connection is opened.
//!
//! ## Headless mode
//!
//! [`App::headless`] swaps the real terminal for an in-memory one driven by a
//! [`Script`]. The [`Report`] then carries every [`Screen`] the script
//! snapshotted, which makes end-to-end tests of an app a few lines long.

mod bindings;
mod convert;
mod headless;
mod http;
mod loader;
mod runner;
mod state;
mod terminal;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use url::Url;

pub use headless::{Script, ScriptCommand};
pub use http::OriginPolicy;
pub use terminal::Screen;

/// Where the app component comes from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Fetched over HTTP; the URL's origin becomes the app's origin.
    Url(Url),
    /// Read from disk. The app has no origin unless [`App::origin`] is set.
    Path(PathBuf),
    /// Already in memory, for example via `include_bytes!`.
    Bytes(Vec<u8>),
}

/// Options for running without a real terminal.
#[derive(Debug, Clone)]
pub struct HeadlessOptions {
    /// Screen size in columns and rows.
    pub width: u16,
    pub height: u16,
    /// Input to feed the app and when to take snapshots.
    pub script: Script,
    /// Stop the app after this long, reporting [`AppStatus::TimedOut`].
    pub timeout: Option<Duration>,
}

impl Default for HeadlessOptions {
    fn default() -> Self {
        Self {
            width: 80,
            height: 24,
            script: Script::default(),
            timeout: None,
        }
    }
}

/// How the app ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppStatus {
    /// The app returned or called `exit` with this code.
    Exited(i32),
    /// The app trapped (a panic, an out-of-bounds access, ...).
    Trapped(String),
    /// The user pressed Ctrl-C three times in a row.
    Killed,
    /// The headless timeout elapsed.
    TimedOut,
}

/// What happened while the app ran.
#[derive(Debug, Clone)]
pub struct Report {
    pub status: AppStatus,
    /// Whatever the app wrote to its stdout.
    pub stdout: String,
    /// Whatever the app wrote to its stderr, panics included.
    pub stderr: String,
    /// Screens captured by `snapshot` script commands (headless only).
    pub snapshots: Vec<Screen>,
    /// The screen when the app ended (headless only).
    pub final_screen: Option<Screen>,
}

impl Report {
    /// A process exit code that reflects [`Report::status`].
    pub fn exit_code(&self) -> i32 {
        match &self.status {
            AppStatus::Exited(code) => *code,
            AppStatus::Trapped(_) => 101,
            AppStatus::Killed => 130,
            AppStatus::TimedOut => 124,
        }
    }
}

/// A rattery app, ready to run. Build one with [`App::from_url`],
/// [`App::from_path`], or [`App::from_bytes`], adjust the policy, then
/// [`run`](App::run) it.
#[derive(Debug, Clone)]
pub struct App {
    pub(crate) source: Source,
    pub(crate) origin: Option<String>,
    pub(crate) allow_origins: Vec<String>,
    pub(crate) allow_all_origins: bool,
    pub(crate) cors: bool,
    pub(crate) mouse: bool,
    pub(crate) cache: bool,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) watch: bool,
    pub(crate) headless: Option<HeadlessOptions>,
}

impl App {
    fn new(source: Source) -> Self {
        Self {
            source,
            origin: None,
            allow_origins: Vec::new(),
            allow_all_origins: false,
            cors: false,
            mouse: true,
            cache: true,
            env: Vec::new(),
            watch: false,
            headless: None,
        }
    }

    /// An app served over HTTP. Its origin is the URL's origin.
    pub fn from_url(url: impl AsRef<str>) -> Result<Self> {
        let url =
            Url::parse(url.as_ref()).with_context(|| format!("invalid URL {:?}", url.as_ref()))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https"),
            "unsupported URL scheme {:?}, expected http or https",
            url.scheme()
        );
        Ok(Self::new(Source::Url(url)))
    }

    /// An app component on disk.
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self::new(Source::Path(path.into()))
    }

    /// An app component already in memory.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(Source::Bytes(bytes.into()))
    }

    /// A URL if `source` parses as an http(s) URL, otherwise a path. This is
    /// what the `rattery` command does with its argument.
    pub fn from_source(source: &str) -> Result<Self> {
        match Url::parse(source) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => Self::from_url(source),
            _ => Ok(Self::from_path(source)),
        }
    }

    /// Where server functions are sent, as `scheme://host[:port]`.
    ///
    /// For an app loaded from a URL this overrides the URL's origin, and the
    /// calls become cross-origin requests subject to the policy. For an app
    /// loaded from bytes or a file it *is* the app's origin.
    pub fn origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }

    /// Let the app reach one more origin over HTTP.
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        self.allow_origins.push(origin.into());
        self
    }

    /// Let the app reach any origin over HTTP.
    pub fn allow_all_origins(mut self, yes: bool) -> Self {
        self.allow_all_origins = yes;
        self
    }

    /// Browser-style CORS: a request to an origin that is not allowed is still
    /// sent, carrying an `Origin` header, and the response is delivered only if
    /// it answers with a matching `Access-Control-Allow-Origin`.
    pub fn cors(mut self, yes: bool) -> Self {
        self.cors = yes;
        self
    }

    /// Report mouse events to the app (default: yes).
    pub fn mouse(mut self, yes: bool) -> Self {
        self.mouse = yes;
        self
    }

    /// Cache compiled components on disk (default: yes).
    pub fn cache(mut self, yes: bool) -> Self {
        self.cache = yes;
        self
    }

    /// Set an environment variable the app can read. Apps see nothing else
    /// from your environment.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// For an app loaded from a URL: poll the server and restart the app in
    /// place whenever a new component is published. This is the dev loop.
    pub fn watch(mut self, yes: bool) -> Self {
        self.watch = yes;
        self
    }

    /// Run without touching the real terminal; see [`HeadlessOptions`].
    pub fn headless(mut self, options: HeadlessOptions) -> Self {
        self.headless = Some(options);
        self
    }

    /// Fetch, compile, and run the app to completion.
    ///
    /// Needs a multi-threaded tokio runtime: input is read on a separate task
    /// so an unresponsive app can still be interrupted.
    pub async fn run(self) -> Result<Report> {
        runner::run(self).await
    }

    /// [`run`](App::run) on a runtime of its own, for programs without one.
    pub fn run_blocking(self) -> Result<Report> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to start a tokio runtime")?
            .block_on(self.run())
    }
}
