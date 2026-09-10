//! # rattery
//!
//! Run a [rattery](https://github.com/tom-lubenow/rattery) app, a ratatui app
//! compiled to a WASI 0.2 component, inside the current terminal with the
//! isolation a browser gives a web page.
//!
//! This is the host library. An existing CLI embeds a remote TUI with a few
//! lines, and a thin shim ships one app against one endpoint with the
//! component inside its binary (see `rattery-build` for producing it). Apps
//! themselves depend on the `rattery-app` crate.
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use rattery::App;
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
//! embedded in your binary (`rattery-build` compiles it in `build.rs`):
//!
//! ```ignore
//! use rattery::App;
//!
//! let report = App::from_bytes(rattery::embed!().to_vec())
//!     .origin("https://api.example.com")
//!     .run_blocking()?;
//! ```
//!
//! ## What the sandbox contains
//!
//! The app gets the terminal interface, a clock, randomness, and HTTP to
//! allowed origins; no filesystem, no sockets, no inherited stdio. Everything
//! it sends toward the terminal is validated ([`sanitize`]): no control
//! characters reach the screen, the title, or the embedder's output. Its
//! resource use is bounded ([`Limits`]), including CPU time charged on a
//! continuous 10 ms epoch tick.
//!
//! ## Origin policy
//!
//! An app may make HTTP and websocket requests to its own origin: where it was
//! loaded from (the final URL after same-origin redirects; cross-origin
//! redirects are refused), or the [`origin`](App::origin) you give an app
//! loaded from bytes or a file. [`allow_origin`](App::allow_origin) adds
//! more. There is no browser-style CORS mode: cross-origin access is
//! allow-list only until preflight and credential semantics are implemented.
//! A [`RequestPolicy`] sees every request after the origin check and can
//! refuse it, edit it (inject or refresh credentials), or hold a guard for
//! its duration.
//!
//! ## Cookies and sessions
//!
//! The host keeps a cookie jar the way a browser does. The app never sees
//! `Cookie` or `Set-Cookie` headers, so ordinary cookie-based sessions on the
//! server work unchanged, and `HttpOnly` means what it says. A persistent jar
//! is a private file: owner-only permissions, no symbolic links, locked
//! between processes, replaced atomically. See [`CookiePolicy`].
//!
//! ## Headless mode
//!
//! [`App::headless`] swaps the real terminal for an in-memory one driven by a
//! [`Script`]. The [`Report`] then carries every [`Screen`] the script
//! snapshotted, which makes end-to-end tests of an app a few lines long.
//!
//! ## Compatibility
//!
//! The async component ABI and `wasi:http@0.3` are still experimental
//! upstream. [`ABI`] names the contract this version of the host speaks;
//! record it in release metadata and check components with [`inspect`]
//! before shipping them. The minimum supported Rust version is 1.95.

mod bindings;
mod convert;
mod headless;
mod http;
mod loader;
mod runner;
pub mod sanitize;
mod state;
mod storage;
mod terminal;
mod websocket;

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use url::Url;
pub use wasmtime;

pub use headless::{Script, ScriptCommand};
pub use http::{
    CookieJar, OriginPolicy, PolicyError, PolicyGuard, RequestInfo, RequestKind, RequestPolicy,
};
pub use runner::precompile;
pub use state::HostState;
pub use terminal::{Screen, Stats};

/// The contract this host speaks, for release metadata and [`inspect`].
///
/// It names the WIT package version of the terminal and websocket
/// interfaces, the component-model async ABI, and the WASI HTTP version the
/// guest runtime uses. Any change to these is a new identifier.
pub const ABI: &str = "rattery:tui@0.3.0;cm-async;wasi:http@0.3.0";

/// The request header sent with every component fetch, carrying [`ABI`], so
/// a server can serve the build that matches the host or answer
/// `426 Upgrade Required` (with the ABI it needs in the same header of the
/// response) during a transition.
pub const ABI_HEADER: &str = "rattery-abi";

/// The rattery ABI a component targets, from its imports: `rattery:tui@X`.
pub(crate) fn abi_of<'a>(imports: impl IntoIterator<Item = &'a str>) -> Option<String> {
    imports
        .into_iter()
        .find_map(|i| i.strip_prefix("rattery:tui/terminal@"))
        .map(|version| format!("rattery:tui@{version}"))
}

/// The ABI a component or server needs when it is not one this host
/// provides. Versions match on major.minor; patch levels resolve.
pub(crate) fn abi_mismatch(required: &str) -> Option<String> {
    let minor = |abi: &str| -> Option<String> {
        let version = abi.split(';').next()?.strip_prefix("rattery:tui@")?;
        let mut parts = version.split('.');
        Some(format!("{}.{}", parts.next()?, parts.next()?))
    };
    (minor(required) != minor(ABI))
        .then(|| required.split(';').next().unwrap_or(required).to_owned())
}

/// The bytes of the app that `rattery-build` compiled in `build.rs`:
/// `include_bytes!(env!("RATTERY_APP_WASM"))`. Pass a variable name for an
/// app built with a custom [`env`](https://docs.rs/rattery-build).
#[macro_export]
macro_rules! embed {
    () => {
        include_bytes!(env!("RATTERY_APP_WASM"))
    };
    ($env:literal) => {
        include_bytes!(env!($env))
    };
}

/// The precompiled app `rattery_build::App::precompile(true)` produced, for
/// [`App::from_precompiled`]: `include_bytes!(env!("RATTERY_APP_CWASM"))`.
#[macro_export]
macro_rules! embed_precompiled {
    () => {
        include_bytes!(env!("RATTERY_APP_CWASM"))
    };
    ($env:literal) => {
        include_bytes!(env!($env))
    };
}

/// What [`inspect`] found in a component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentInfo {
    /// Every imported interface or function, by name.
    pub imports: Vec<String>,
    /// Every export, by name.
    pub exports: Vec<String>,
    /// True if the component targets this host's [`ABI`]: it imports the
    /// terminal interface at a compatible version and exports `run`.
    pub compatible: bool,
    /// Imports outside the WASI and rattery namespaces: extensions the host
    /// must provide for the component to instantiate.
    pub extension_imports: Vec<String>,
    /// The rattery ABI the component targets (`rattery:tui@X`), if it
    /// imports the terminal interface at all.
    pub abi: Option<String>,
}

/// Validate a component against this host without running it: it must be a
/// valid component, within [`Limits::component_bytes`], and target [`ABI`].
/// Use it on externally resolved bytes before [`App::from_bytes`].
pub fn inspect(bytes: &[u8]) -> Result<ComponentInfo> {
    inspect_with(bytes, &Limits::default())
}

/// [`inspect`] with explicit limits.
pub fn inspect_with(bytes: &[u8], limits: &Limits) -> Result<ComponentInfo> {
    anyhow::ensure!(
        bytes.len() <= limits.component_bytes,
        "component is {} bytes, over the limit of {} bytes",
        bytes.len(),
        limits.component_bytes
    );
    let mut config = wasmtime::Config::new();
    config.wasm_component_model_async(true);
    let engine = wasmtime::Engine::new(&config)?;
    let component = wasmtime::component::Component::new(&engine, bytes)
        .map_err(anyhow::Error::from)
        .context("not a valid component")?;
    let ty = component.component_type();
    let imports: Vec<String> = ty
        .imports(&engine)
        .map(|(name, _)| name.to_owned())
        .collect();
    let exports: Vec<String> = ty
        .exports(&engine)
        .map(|(name, _)| name.to_owned())
        .collect();
    let abi = abi_of(imports.iter().map(String::as_str));
    let terminal_ok = abi
        .as_deref()
        .is_some_and(|abi| abi_mismatch(abi).is_none());
    let compatible = terminal_ok && exports.iter().any(|e| e == "run");
    let extension_imports = imports
        .iter()
        .filter(|i| !(i.starts_with("wasi:") || i.starts_with("rattery:")))
        .cloned()
        .collect();
    Ok(ComponentInfo {
        imports,
        exports,
        compatible,
        extension_imports,
        abi,
    })
}

/// A component the embedder fetches and validates itself.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub bytes: Vec<u8>,
    /// An opaque version (an ETag, a digest, a build id). Reported back on
    /// the next poll so the resolver can answer "unchanged".
    pub version: Option<String>,
    /// The app's origin, if the resolver knows it; else [`App::origin`].
    pub origin: Option<String>,
    /// What `rattery_app::location()` should return.
    pub location: Option<String>,
}

/// Retrieval of the component by the embedder: from a registry, an
/// artifact store, a signed bundle, anywhere. Return `Ok(None)` from a poll
/// (`current` is `Some`) when nothing changed. The bytes are still checked
/// against [`Limits::component_bytes`]; run [`inspect`] yourself for more.
pub trait Resolver: Send + Sync + 'static {
    fn resolve<'a>(&'a self, current: Option<&'a str>) -> BoxFuture<'a, Result<Option<Resolved>>>;
}

/// Where the app component comes from.
#[derive(Clone)]
pub enum Source {
    /// Fetched over HTTP; the final URL's origin becomes the app's origin.
    Url(Url),
    /// Read from disk. The app has no origin unless [`App::origin`] is set.
    Path(PathBuf),
    /// Already in memory, for example via `include_bytes!`.
    Bytes(Vec<u8>),
    /// Native code from [`precompile`], already in memory. Trusted: see
    /// [`App::from_precompiled`].
    Precompiled(Vec<u8>),
    /// Produced by the embedder's [`Resolver`].
    Resolver(Arc<dyn Resolver>),
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Url(url) => f.debug_tuple("Url").field(url).finish(),
            Source::Path(path) => f.debug_tuple("Path").field(path).finish(),
            Source::Bytes(bytes) => f.debug_tuple("Bytes").field(&bytes.len()).finish(),
            Source::Precompiled(bytes) => f.debug_tuple("Precompiled").field(&bytes.len()).finish(),
            Source::Resolver(_) => f.debug_tuple("Resolver").finish(),
        }
    }
}

/// Bounds on what an app may use. Every field has a default meant for an
/// ordinary TUI; tighten them for untrusted apps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Linear memory the app may grow to, in bytes, across all of its
    /// memories together.
    pub memory_bytes: usize,
    /// Total time the app may spend executing (not waiting), charged in
    /// 10 ms ticks. `None` is unlimited.
    pub cpu_time: Option<Duration>,
    /// Largest component accepted, in bytes.
    pub component_bytes: usize,
    /// How long fetching the component may take.
    pub download_timeout: Duration,
    /// Input events queued while the app is not reading; older ones drop.
    pub event_queue: usize,
    /// Cells one `draw` call may carry.
    pub frame_cells: usize,
    /// Open websockets at once.
    pub websockets: usize,
    /// Messages queued per websocket per direction: incoming while the app
    /// is not reading (older ones drop), outgoing while the peer is slow
    /// (`send` waits).
    pub websocket_queue: usize,
    /// Bytes queued per websocket per direction, across all queued messages.
    pub websocket_queue_bytes: usize,
    /// Largest websocket message, in either direction.
    pub websocket_message_bytes: usize,
    /// HTTP requests in flight at once.
    pub http_concurrency: usize,
    /// Largest request body.
    pub request_body_bytes: usize,
    /// Largest response body.
    pub response_body_bytes: usize,
    /// Bytes of stdout and of stderr kept from the app, in total across
    /// reloads; older output gives way.
    pub guest_output_bytes: usize,
    /// Longest paste event delivered to the app; longer ones are cut.
    pub paste_bytes: usize,
    /// Longest error or trap message kept from the app.
    pub message_bytes: usize,
    /// Host resources (streams, requests, sockets, bodies) the app may hold
    /// at once.
    pub resources: usize,
    /// Origin-scoped storage: total bytes of keys and values per origin.
    pub storage_bytes: usize,
    /// Entries per origin.
    pub storage_entries: usize,
    /// Longest storage key.
    pub storage_key_bytes: usize,
    /// Largest storage value.
    pub storage_value_bytes: usize,
    /// Log records accepted per second; the rest are counted and dropped.
    pub logs_per_second: usize,
    /// wasmtime store limits: tables, table elements, memories, instances.
    pub tables: usize,
    pub table_elements: usize,
    pub memories: usize,
    pub instances: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 256 << 20,
            cpu_time: None,
            component_bytes: 64 << 20,
            download_timeout: Duration::from_secs(60),
            event_queue: 1024,
            frame_cells: 1 << 20,
            websockets: 16,
            websocket_queue: 64,
            websocket_queue_bytes: 8 << 20,
            websocket_message_bytes: 4 << 20,
            http_concurrency: 16,
            request_body_bytes: 64 << 20,
            response_body_bytes: 64 << 20,
            guest_output_bytes: 1 << 20,
            paste_bytes: 1 << 20,
            message_bytes: 16 << 10,
            resources: 4096,
            storage_bytes: 5 << 20,
            storage_entries: 1024,
            storage_key_bytes: 256,
            storage_value_bytes: 1 << 20,
            logs_per_second: 1000,
            tables: 32,
            table_elements: 1 << 20,
            memories: 8,
            instances: 16,
        }
    }
}

impl Limits {
    /// Check the limits are coherent: every queue must hold at least one
    /// maximum-sized message, and no bound may be zero. [`App::run`] refuses
    /// limits that fail this.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.websocket_queue_bytes >= self.websocket_message_bytes,
            "websocket_queue_bytes ({}) must be at least websocket_message_bytes ({}) so a \
             maximum-sized message can be queued",
            self.websocket_queue_bytes,
            self.websocket_message_bytes
        );
        for (name, value) in [
            ("memory_bytes", self.memory_bytes),
            ("component_bytes", self.component_bytes),
            ("event_queue", self.event_queue),
            ("frame_cells", self.frame_cells),
            ("websocket_queue", self.websocket_queue),
            ("websocket_message_bytes", self.websocket_message_bytes),
            ("http_concurrency", self.http_concurrency),
            ("request_body_bytes", self.request_body_bytes),
            ("response_body_bytes", self.response_body_bytes),
            ("guest_output_bytes", self.guest_output_bytes),
            ("message_bytes", self.message_bytes),
            ("resources", self.resources),
            ("storage_bytes", self.storage_bytes),
            ("storage_entries", self.storage_entries),
            ("storage_key_bytes", self.storage_key_bytes),
            ("storage_value_bytes", self.storage_value_bytes),
            ("logs_per_second", self.logs_per_second),
            ("instances", self.instances),
        ] {
            anyhow::ensure!(value > 0, "Limits::{name} must be greater than zero");
        }
        Ok(())
    }
}

/// Lifecycle notifications for an embedder; see [`App::on_phase`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// The component was fetched or read.
    Loaded { bytes: usize },
    /// It compiled.
    Compiled,
    /// It was instantiated and is about to run.
    Instantiated,
    /// It drew its first frame successfully.
    Ready,
    /// The app called `rattery_app::ready()`: by its own account it is
    /// ready for the user (data loaded, a real screen showing).
    AppReady,
    /// A request was refused by the origin policy, the request policy, or a limit.
    RequestDenied { url: String, reason: String },
    /// The app logged a record. `target` and `message` are already sanitised
    /// (no control characters) and bounded by [`Limits::message_bytes`].
    Log {
        level: LogLevel,
        target: String,
        message: String,
    },
    /// The watcher found a newer component. Under
    /// [`ReloadPolicy::Immediate`] the reload follows at once; otherwise the
    /// app has been told and decides. `version` is the server's validator,
    /// sanitised.
    UpdateAvailable { version: Option<String> },
    /// The watcher found a newer component that does not compile or does not
    /// link against this host, so it was not offered; the app keeps running
    /// and the watcher keeps polling for the next version.
    UpdateRejected {
        reason: String,
        /// Set when the rejection is an ABI transition: the component (or,
        /// for a `426 Upgrade Required` answer, the server) needs a rattery
        /// ABI this host does not provide, e.g. `rattery:tui@0.4.0`. The
        /// embedder should tell the user to upgrade the host.
        requires_abi: Option<String>,
    },
    /// A new component is being loaded in place: the app asked with
    /// `rattery_app::reload()`, or the policy decided.
    Reloading,
    /// The app ended.
    Exited(AppStatus),
}

/// Severity of a [`Phase::Log`] record, matching the `log` crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        })
    }
}

/// Where the app's origin-scoped key-value storage lives.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StoragePolicy {
    /// One private file per origin under `rattery/storage` in the user's
    /// local data directory (the default). An app without an origin gets
    /// ephemeral storage.
    #[default]
    Persistent,
    /// One private file per origin under this directory.
    Dir(PathBuf),
    /// Kept in memory for this run only.
    Ephemeral,
    /// Every write fails with `disabled`.
    Disabled,
}

/// What happens when [`App::watch`] finds a newer component. A browser never
/// reloads a page under the user; the page learns of the update and reloads
/// itself. The same choice is the embedder's here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadPolicy {
    /// Replace the running app at once. Right for the dev loop.
    Immediate,
    /// Tell the app (`Event::UpdateAvailable`) and let it call
    /// `rattery_app::reload()` when it is ready, however long that takes.
    AppControlled,
    /// Tell the app, with the deadline, and reload it anyway once `grace`
    /// has passed. The default, with five minutes.
    Deferred { grace: Duration },
}

impl Default for ReloadPolicy {
    fn default() -> Self {
        ReloadPolicy::Deferred {
            grace: Duration::from_secs(300),
        }
    }
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
    /// The app trapped (a panic, an out-of-bounds access, ...). The message
    /// may contain guest-chosen text; pass it through [`sanitize::text`]
    /// before printing.
    Trapped(String),
    /// The user pressed Ctrl-C three times in a row.
    Killed,
    /// The headless timeout elapsed.
    TimedOut,
    /// A [`Limits`] bound was exceeded; the text says which.
    LimitExceeded(String),
}

/// What happened while the app ran.
///
/// `stdout`, `stderr`, and a trap message are guest-controlled text: print
/// them through [`sanitize::text`], never verbatim.
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
    /// Phase timings.
    pub timings: Timings,
    /// Terminal counters from the last run of the app.
    pub stats: Stats,
}

impl Report {
    /// A process exit code that reflects [`Report::status`].
    pub fn exit_code(&self) -> i32 {
        match &self.status {
            AppStatus::Exited(code) => *code,
            AppStatus::Trapped(_) => 101,
            AppStatus::Killed => 130,
            AppStatus::TimedOut => 124,
            AppStatus::LimitExceeded(_) => 137,
        }
    }
}

/// How long the phases before the app ran took.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Timings {
    /// Fetching or reading the component.
    pub load: Duration,
    /// Compiling it (near zero on a cache hit).
    pub compile: Duration,
    /// Instantiating the component.
    pub instantiate: Duration,
    /// From the app starting to its first frame, if it drew one.
    pub first_draw: Option<Duration>,
    /// The whole run, load to exit.
    pub total: Duration,
}

/// What happens to cookies the app's servers set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CookiePolicy {
    /// Keep them in `rattery/cookies.json` under the user's local data
    /// directory, shared by every app the user runs (the default).
    #[default]
    Persistent,
    /// Keep them in a file of your choosing, for an embedding CLI that wants
    /// its own sessions.
    File(PathBuf),
    /// Keep them in memory for this run only, like a private window.
    Ephemeral,
    /// Drop every cookie; the app is never logged in to anything.
    Disabled,
}

type Extension = Box<dyn FnOnce(&mut wasmtime::component::Linker<HostState>) -> Result<()> + Send>;
type PhaseHook = Arc<dyn Fn(Phase) + Send + Sync>;

/// A rattery app, ready to run. Build one with [`App::from_url`],
/// [`App::from_path`], [`App::from_bytes`], or [`App::from_resolver`],
/// adjust the policy and limits, then [`run`](App::run) it.
pub struct App {
    pub(crate) source: Source,
    pub(crate) origin: Option<String>,
    pub(crate) allow_origins: Vec<String>,
    pub(crate) allow_all_origins: bool,
    pub(crate) mouse: bool,
    pub(crate) cache: bool,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) location: Option<String>,
    pub(crate) cookies: CookiePolicy,
    pub(crate) storage: StoragePolicy,
    pub(crate) watch: bool,
    pub(crate) reload: ReloadPolicy,
    pub(crate) headless: Option<HeadlessOptions>,
    pub(crate) limits: Limits,
    pub(crate) on_phase: Option<PhaseHook>,
    pub(crate) request_policy: Option<Arc<dyn RequestPolicy>>,
    pub(crate) extensions: Vec<Extension>,
    pub(crate) ext: HashMap<TypeId, Box<dyn Any + Send>>,
}

impl App {
    fn new(source: Source) -> Self {
        Self {
            source,
            origin: None,
            allow_origins: Vec::new(),
            allow_all_origins: false,
            mouse: true,
            cache: true,
            env: Vec::new(),
            location: None,
            cookies: CookiePolicy::Persistent,
            storage: StoragePolicy::Persistent,
            watch: false,
            reload: ReloadPolicy::default(),
            headless: None,
            limits: Limits::default(),
            on_phase: None,
            request_policy: None,
            extensions: Vec::new(),
            ext: HashMap::new(),
        }
    }

    /// An app served over HTTP. Its origin is the origin of the final URL
    /// after same-origin redirects; a cross-origin redirect is refused.
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

    /// An app component already in memory. Validate externally obtained
    /// bytes with [`inspect`] first if you want a diagnosis before running.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(Source::Bytes(bytes.into()))
    }

    /// A component already compiled to native code by [`precompile`] (or
    /// `rattery_build::App::precompile`), so startup skips compilation.
    ///
    /// # Safety
    ///
    /// The bytes are executed as native code. They must come from
    /// [`precompile`] of the same rattery version and target, and must not
    /// have been tampered with since; wasmtime checks the header, engine
    /// settings, and target, but cannot verify the code itself. Embed them at
    /// build time (`embed_precompiled!`) rather than loading them from
    /// anywhere an attacker could write.
    pub unsafe fn from_precompiled(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(Source::Precompiled(bytes.into()))
    }

    /// An app component the embedder retrieves itself; with
    /// [`watch`](App::watch), the resolver is polled for new versions.
    pub fn from_resolver(resolver: Arc<dyn Resolver>) -> Self {
        Self::new(Source::Resolver(resolver))
    }

    /// A URL if `source` parses as an http(s) URL, otherwise a path.
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
    /// loaded from bytes, a file, or a resolver that gave no origin, it *is*
    /// the app's origin.
    pub fn origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }

    /// Let the app reach one more origin over HTTP and websockets.
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        self.allow_origins.push(origin.into());
        self
    }

    /// Let the app reach any origin. Only for apps you trust completely.
    pub fn allow_all_origins(mut self, yes: bool) -> Self {
        self.allow_all_origins = yes;
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

    /// The URL the app believes it was loaded from, query string included:
    /// what `rattery_app::location()` returns. Defaults to the source URL.
    /// Set it to pass parameters to an app loaded from bytes or a file.
    pub fn location(mut self, url: impl Into<String>) -> Self {
        self.location = Some(url.into());
        self
    }

    /// How cookies are stored between requests and runs (default: persistent).
    pub fn cookies(mut self, policy: CookiePolicy) -> Self {
        self.cookies = policy;
        self
    }

    /// Where the app's key-value storage lives (default: persistent, per origin).
    pub fn storage(mut self, policy: StoragePolicy) -> Self {
        self.storage = policy;
        self
    }

    /// For an app loaded from a URL or a resolver: poll for a new component
    /// and restart the app in place whenever one is published.
    pub fn watch(mut self, yes: bool) -> Self {
        self.watch = yes;
        self
    }

    /// What to do when [`watch`](App::watch) finds a new version; see
    /// [`ReloadPolicy`]. Whatever the policy, an app may reload itself at
    /// any time with `rattery_app::reload()`.
    pub fn reload_policy(mut self, policy: ReloadPolicy) -> Self {
        self.reload = policy;
        self
    }

    /// Run without touching the real terminal; see [`HeadlessOptions`].
    pub fn headless(mut self, options: HeadlessOptions) -> Self {
        self.headless = Some(options);
        self
    }

    /// Resource bounds; see [`Limits`].
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Be told about lifecycle phases: loaded, compiled, ready (first frame),
    /// denied requests, reloads, exit. The hook runs on the host's threads;
    /// keep it quick.
    pub fn on_phase(mut self, hook: impl Fn(Phase) + Send + Sync + 'static) -> Self {
        self.on_phase = Some(Arc::new(hook));
        self
    }

    /// Remove and return the phase hook set so far, to wrap it in another.
    pub fn take_phase_hook(&mut self) -> Option<Arc<dyn Fn(Phase) + Send + Sync>> {
        self.on_phase.take()
    }

    /// A [`RequestPolicy`] applied to every outbound HTTP request and
    /// websocket handshake after the origin check.
    pub fn request_policy(mut self, policy: Arc<dyn RequestPolicy>) -> Self {
        self.request_policy = Some(policy);
        self
    }

    /// Register a host extension: additional WIT imports the app may use,
    /// added to the wasmtime linker. Keep the extension's state with
    /// [`App::state`] and reach it through [`HostState::ext_mut`].
    pub fn extension(
        mut self,
        register: impl FnOnce(&mut wasmtime::component::Linker<HostState>) -> Result<()>
        + Send
        + 'static,
    ) -> Self {
        self.extensions.push(Box::new(register));
        self
    }

    /// State for host extensions, one value per type, reachable through
    /// [`HostState::ext`] and kept across reloads.
    pub fn state<T: Any + Send>(mut self, value: T) -> Self {
        self.ext.insert(TypeId::of::<T>(), Box::new(value));
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

#[cfg(test)]
mod wit_sync {
    /// `crates/rattery/wit` is a copy of `crates/rattery-app/wit` so both
    /// crates are publishable on their own; this keeps them identical.
    #[test]
    fn wit_matches_the_app_crate() {
        let ours = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("wit");
        let theirs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../rattery-app/wit");
        if !theirs.exists() {
            return; // published copy: nothing to compare against
        }
        for entry in walk(&theirs) {
            let rel = entry.strip_prefix(&theirs).unwrap();
            let a = std::fs::read(&entry).unwrap();
            let b = std::fs::read(ours.join(rel)).unwrap_or_default();
            assert!(
                a == b,
                "wit/{} differs from crates/rattery-app/wit; copy it over",
                rel.display()
            );
        }
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path))
            } else {
                out.push(path)
            }
        }
        out
    }
}

#[cfg(test)]
mod limit_tests {
    use super::Limits;

    #[test]
    fn defaults_are_coherent_and_incoherence_is_rejected() {
        Limits::default().validate().unwrap();
        let bad = Limits {
            websocket_queue_bytes: 1 << 20,
            websocket_message_bytes: 2 << 20,
            ..Limits::default()
        };
        assert!(bad.validate().is_err());
        let zero = Limits {
            http_concurrency: 0,
            ..Limits::default()
        };
        assert!(zero.validate().is_err());
    }
}
