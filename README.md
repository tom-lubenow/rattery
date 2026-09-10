# rattery

**Ship a [ratatui](https://ratatui.rs) app as a thin binary that runs it sandboxed and talks
to your server.**

Write the app with ratatui. Declare its backend calls as `#[rattery_app::server]`
functions, Leptos / Dioxus fullstack style. The app compiles to a WASI component; a
few lines of `build.rs` build it and a few lines of `main.rs` embed it, so `cargo
build` of your CLI produces one binary with the app inside, pointed at your API:

```rust
// build.rs
rattery_build::App::new("../app").build();

// main.rs
let report = rattery::App::from_bytes(rattery::embed!().to_vec())
    .origin("https://api.example.com")
    .run_blocking()?;
std::process::exit(report.exit_code());
```

The host inside that binary is to the app what a browser is to a web page: it runs
the component in a [wasmtime](https://wasmtime.dev) sandbox, hands it the terminal
through a small WIT interface, keeps its cookies, and lets it make HTTP requests to
**its own origin only** unless you or the other server say otherwise. The same host can
just as well fetch the component from a URL at startup, so an app can be deployed by
replacing one file on the server. A *rattery* is an enclosure for rats. This one keeps a
ratatui app where it can't touch your filesystem, your network, or your other
terminals.

## Why

- **One fullstack dev model.** `#[rattery_app::server]` is `server_fn`'s `#[server]`
  with the client filled in. The same shared crate compiles into the app (calls become
  HTTP) and into the server (bodies run). It is the crate Leptos and Dioxus use, so
  request/response, streaming responses, websockets, multipart uploads, and cookie
  sessions all work as they do there.
- **A real sandbox.** The component gets the terminal, a clock, randomness, and HTTP
  to its origin. Nothing else is linked in. Embedding someone else's TUI, or loading
  one from a URL, is as safe as opening a web page.
- **Embeddable first.** `rattery::App` is the product: a builder your CLI calls. There
  is no daemon and no required command; `examples/rattery-cli` shows a general-purpose
  runner in 150 lines if you want one.
- **Deploy by URL, optionally.** `App::from_url` fetches the component like a browser
  would; with `.watch(true)` a running app reloads when the server publishes a new one.
- **Thick client.** UI state stays local, the server only answers RPC. Compare with
  SSH-app frameworks, which run the whole UI server-side and stream frames.

## How it works

```
 ┌────────── your terminal ──────────┐
 │ your CLI (rattery::App inside)    │      HTTP        ┌────────────────────┐
 │  crossterm ⇄ terminal (WIT) ⇄ app │ ───────────────▶ │ axum server        │
 │  wasi:http ── origin policy ──────┼────────────────▶ │  /api/* server fns │
 │               cookies             │  POST /api/...   │  (/app.wasm, opt.) │
 └───────────────────────────────────┘                  └────────────────────┘
```

- **`crates/rattery-app/wit/rattery.wit`** is the entire contract. It mirrors ratatui's `Backend` trait
  (draw a list of changed cells, cursor, size, flush) plus a crossterm-shaped event
  stream and a `wasi:io` pollable so an app can `await` key presses and server
  responses at the same time. A second small interface provides websockets, which
  WASI 0.2 lacks; the host applies the same origin policy and cookie jar to them.
- **`crates/rattery`** is the host library: wasmtime + `wasmtime-wasi` +
  `wasmtime-wasi-http`, crossterm behind the terminal interface, a loader, the origin
  policy, the cookie jar, and a headless mode. `rattery::App` is the entry point.
- **`crates/rattery-build`** builds an app to a component from `build.rs` so a shim can
  embed it with `rattery::embed!()`.
- **`crates/rattery-app`** is what apps depend on: a ratatui `Backend` over the WIT
  interface, the event API, background tasks, timers, websockets, and a `server_fn`
  client that speaks `wasi:http@0.3`. On native targets it provides only what the
  server build of a shared crate needs.
- **`crates/rattery-macros`** provides `#[rattery_app::server]`.
- **`examples/rattery-cli`** is a general-purpose runner built on the library, used by
  the dev loop and the benchmark, and the reference for a shim that takes everything
  as flags. It is not published.

Diffing happens inside ratatui's `Terminal` in the guest, so a frame is one `draw` call
carrying only the cells that changed, and one `flush`.

**The app is a real async program.** It exports one `async func run` and the host
drives it with the component model's async ABI, so waiting for a key press, a server
response, a websocket message, or a timer is a plain `.await` and other tasks in the
app keep running meanwhile. Input is an `async func` on the terminal interface; HTTP
and timers use WASI 0.3, while the standard library keeps using WASI 0.2 for stdio.
All of this builds on stable Rust for `wasm32-wasip2`: the async ABI does not need
the `wasm32-wasip3` target, which has no prebuilt standard library yet.

## Quick start

Everything is in the nix dev shell (`direnv allow` or `nix develop`): stable Rust
with the `wasm32-wasip2` target, `wasm-tools`, and the `wasmtime` CLI.

```sh
# the dev loop: build the app and the server, serve, rebuild on change
cargo xtask dev

# in another terminal, run the app like a browser would; --watch reloads it
# in place every time the component is rebuilt
cargo run -p rattery -- --watch http://127.0.0.1:3000/app.wasm
```

Or by hand:

```sh
cargo build -p counter-app --target wasm32-wasip2
cargo run -p counter-server
cargo run -p rattery -- http://127.0.0.1:3000/app.wasm
```

## Writing an app

A shared crate holds the server functions and any types they exchange:

```rust
// counter-shared/src/lib.rs
use rattery_app::server_fn::codec::{StreamingText, TextStream};
use rattery_app::{server, ServerFnError};

#[server]
pub async fn adjust_count(delta: i64) -> Result<i64, ServerFnError> {
    Ok(state::adjust(delta)) // only compiled with the `ssr` feature
}

/// A streaming response: the server pushes lines for as long as the app reads.
#[server(output = StreamingText)]
pub async fn live_feed() -> Result<TextStream, ServerFnError> {
    Ok(TextStream::from(state::ticks()))
}

/// A websocket: a stream in, a stream out, for as long as the connection lives.
#[server(protocol = Websocket<JsonEncoding, JsonEncoding>)]
pub async fn chat(
    input: BoxedStream<String, ServerFnError>,
) -> Result<BoxedStream<String, ServerFnError>, ServerFnError> {
    Ok(input.map(|m| m.map(|text| format!("echo: {text}"))).into())
}

/// A file upload. The app builds a `rattery_app::multipart::FormData`; the server
/// gets the parsed parts as a `multer` stream.
#[server(input = MultipartFormData)]
pub async fn upload(data: MultipartData) -> Result<String, ServerFnError> {
    let mut parts = data.into_inner().expect("server side");
    while let Some(field) = parts.next_field().await? { /* ... */ }
    Ok("thanks".into())
}
```

```toml
[features]
ssr = ["rattery/ssr"]
axum = ["ssr", "rattery/axum"]
```

The app is an ordinary binary crate built for `wasm32-wasip2`; `rattery_app::app!`
exports the component's entry point (and supplies the placeholder `main` a binary
needs; the host never calls it, because a synchronous `main` could not await). Server
calls run as background tasks so the UI never blocks; a finished task surfaces as
`Event::Wake`:

```rust
use rattery_app::prelude::*;
use rattery_app::{event, task};

rattery_app::app!(app);

async fn app(mut terminal: Terminal) -> Result<(), Box<dyn std::error::Error>> {
    let mut count = 0;
    let mut pending: Option<Task<Result<i64, ServerFnError>>> = Some(task::spawn(adjust_count(0)));
    loop {
        terminal.draw(|frame| frame.render_widget(count.to_string(), frame.area()))?;
        match event::next().await {
            Event::Wake => {
                if let Some(result) = pending.as_mut().and_then(Task::try_take) {
                    pending = None;
                    count = result?;
                }
            }
            Event::Key(key) if key.code == KeyCode::Char('q') => break Ok(()),
            Event::Key(key) if key.code == KeyCode::Up => pending = Some(task::spawn(adjust_count(1))),
            _ => {}
        }
    }
}
```

The server depends on the shared crate with the `axum` feature and mounts two routes:

```rust
Router::new()
    .route("/app.wasm", get(serve_component))
    .route("/api/{*rest}", any(rattery_app::server_fn::axum::handle_server_fn))
```

Updates work as they do on the web: when a new version is pending the host sends
`Event::UpdateChanged`, `rattery_app::update::pending()` says what and by when, and the
app calls `rattery_app::update::reload()` at a good moment (after saving a draft to
storage, say); under the default policy the host reloads it anyway once a grace period
passes and the app is idle. `rattery_app::update::check()` asks the host to look now.

Two more things a browser gives a page, the host gives an app. `rattery_app::storage`
is `localStorage`: a small key-value store scoped to the app's origin that survives
runs, with a quota (`get`, `set`, `remove`, `keys`, `clear`, `usage`, plus
`get_string`/`set_string`). The `log` facade is wired to the host, so `log::info!`
and friends leave the sandbox as records the embedder can write wherever it likes,
instead of scribbling on the terminal.

`examples/counter` is the complete version: background calls with a spinner, a
streaming live feed, a websocket echo, a multipart upload, cookie sessions with
per-session state, a launch counter in storage, and ETags for `--watch`. `rattery_app::websocket::WebSocket` is also usable
directly, outside server functions.

`rattery_app::location()` returns the URL the app was loaded from, query string included,
so `rattery https://host/app.wasm?team=infra` passes parameters the way a web page
gets them (the example reads `?title=`). `rattery_app::origin()` is where server calls go.

Event types mirror crossterm's (`KeyCode::Char('q')`, `KeyModifiers::CONTROL`, ...)
so existing ratatui code ports by changing an import. `event::next_timeout` and
`rattery_app::time::sleep` drive animations; `task::wake` lets a long-running task ask for
a redraw, which is how the example renders a streaming response line by line.

## The host

Everything below is a method on `rattery::App`; `examples/rattery-cli` exposes each as
a flag, shown here because it reads well:

```
rattery <URL or path>
        [--origin URL] [--allow-origin URL]... [--allow-all-origins]
        [--incognito | --no-cookies | --cookie-jar FILE]
        [--storage-dir DIR | --no-storage] [--log-file FILE]
        [--watch [--reload-grace SECS|none]] [--location URL] [--env KEY=VALUE]... [--no-mouse] [--no-cache]
        [--headless COLSxROWS [--script FILE] [--timeout SECS]]
```

**Origin policy.** An app may reach its own origin: where it was loaded from (after
same-origin redirects; cross-origin redirects are refused), or `--origin` for an app
loaded from a file. `--allow-origin` adds more and `--allow-all-origins` disables the
check. Every request carries an `Origin` header. Everything else is refused before a
connection is opened. There is no CORS mode: cross-origin access is allow-list only
until proper preflight and credential semantics exist. A `RequestPolicy` on the
builder sees every allowed request and can refuse or edit it.

**Cookies.** The host keeps a jar the way a browser does: the app never sees `Cookie`
or `Set-Cookie`, so ordinary cookie sessions on the server work unchanged and
`HttpOnly` means what it says. The jar persists under the user's local data directory;
`--incognito` keeps it in memory, `--no-cookies` drops everything, `--cookie-jar` picks
a file.

**Storage.** Each origin gets a private key-value file under the local data directory
(`StoragePolicy::Persistent`), bounded by `Limits::storage_bytes` and
`storage_entries`; `--incognito` keeps it in memory, `--no-storage` refuses writes,
`--storage-dir` picks the directory. An app loaded from a file without `--origin`
has no origin and so gets ephemeral storage.

**Logs.** Records from the app's `log` macros arrive as `Phase::Log` on the
`on_phase` hook, sanitised, size-capped, and rate-limited (`Limits::logs_per_second`).
`--log-file` appends them to a file you can `tail -f` in another terminal while the
app has the screen.

**Updates.** Three things are kept apart, the way a browser does: discovery, state,
and the reload. Discovery is `--watch` polling the URL with `If-None-Match`, a
`rattery-app-version` header on any server function reply naming a version the host
does not know (so a deploy is noticed on the next call, not the next poll), the app
calling `rattery_app::update::check()`, or the embedder offering bytes through
`AppHandle`. A new component is compiled and linked before anyone hears of it. The
state is `rattery_app::update::pending()` (version and deadlines as of now) for the
app and `AppHandle::pending_update` for the embedder; `Event::UpdateChanged` says it
changed, including when a rollback withdraws it. The reload is the app's
`rattery_app::update::reload()` or the embedder's `AppHandle::reload`, and a
`ReloadPolicy` says whether the host ever forces it: `Immediate` (the dev loop;
`--reload-grace 0`), `AppControlled` (`none`), or `Deferred` (the default): after five
minutes of grace, at the first thirty seconds of idleness, and within an hour
regardless. An app can save its state to storage first, and the server should keep
the old routes working for the grace period, since the old app keeps calling them.

**ABI transitions.** Every component fetch carries a `rattery-abi` header with the
host's `rattery::ABI`, so a server can serve the build that matches each client during
a transition, or answer `426 Upgrade Required` with the ABI it needs in the same
header. A component that needs another ABI, or such an answer, is never applied to a
running app: it surfaces as `Phase::UpdateRejected` with `requires_abi` set, and at
startup as an error naming both versions. The counter shim prints an upgrade hint on
exit; `--require-abi-file` on the example server demonstrates the server side.

**Safety.** Everything the app sends toward the terminal is validated: control
characters and malformed symbols never reach the screen or the title, cells outside
the screen are dropped, and guest output is rendered with escapes shown rather than
interpreted. Resource use is bounded by `Limits` (memory, CPU time on a continuous
10 ms epoch tick, queues, message and body sizes, concurrency). Ctrl-C three times
within 1.5 seconds interrupts an unresponsive app. Raw mode and the alternate screen
are always restored, including on panic, and every background task is stopped before
the terminal is handed back. See `docs/security.md`.

**Input.** Pointer movements and resizes the app has not read yet are merged into
the newest one, so a frame slower than the pointer never turns a burst of movement
into a backlog the app spends seconds draining; clicks, drags, scrolls, and keys are
delivered in full. `docs/perf.md` has the hover benchmark (`cargo xtask hover`), and
one finding worth repeating: build apps in release. At opt-level 0 the same widgets
run about nine times slower inside the component.

**Stats.** `Report::timings` and `Report::stats` (the `--stats` flag prints them) carry
phase timings (load, compile, instantiate, first frame) and terminal counters. `cargo
xtask bench` runs a rendering and request latency benchmark; see `docs/perf.md`.

**Headless.** `--headless 80x24 --script keys.txt` runs the app on an in-memory screen,
feeds it a script (`key k`, `key ctrl-c`, `type hello`, `paste`, `resize`, `sleep`,
`snapshot`; see `--help-script`), and prints the snapshots. This is how the repository's
end-to-end tests work, and it is a ready-made test harness for your own app.

## Embedding the host

```rust
use rattery::{App, CookiePolicy};

// One specific app against one specific backend, embedded in the binary
// (rattery-build compiled it in build.rs).
let report = App::from_bytes(rattery::embed!().to_vec())
    .origin("https://api.example.com")
    .cookies(CookiePolicy::File(config_dir.join("cookies.json")))
    .run_blocking()?;
std::process::exit(report.exit_code());

// A subcommand of an existing async CLI that opens a remote TUI.
let report = App::from_url("https://apps.example.com/dashboard/app.wasm")?
    .allow_origin("https://api.example.com")
    .run()
    .await?;
```

`rattery_build::App` takes the app crate's path (and optionally a package name,
features, or the dev profile), compiles it for `wasm32-wasip2` into a target directory
under `OUT_DIR`, and exports the component's path as `RATTERY_APP_WASM`; changes under
the app's `src` rebuild it. The nested build needs the target installed
(`rustup target add wasm32-wasip2`).

**Precompiled components.** With the `precompile` feature of `rattery-build`,
`.precompile(true)` also compiles the component to native code for the shim's target
and exports `RATTERY_APP_CWASM`; `unsafe { App::from_precompiled(embed_precompiled!().to_vec()) }`
then starts without a compile step or a compile cache (the `unsafe` is wasmtime's:
the bytes run as native code, so only embed what your own build produced).
`rattery::precompile` is the same step for your own pipeline. `examples/counter/shim`
does this.

Production controls on the builder: `limits` (see `docs/security.md`), `on_phase`
(loaded, compiled, ready, denied requests, reload, exit), `request_policy` (route
authorisation, credential injection), `extension` and `state` (extra WIT imports
backed by your own state), `from_resolver` (the embedder retrieves and validates the
bytes). `rattery::inspect` checks a component against `rattery::ABI` before it runs.

`App::headless` returns the snapshots in the `Report`, so an app's integration tests
can be a few lines:

```rust
let report = App::from_url(&url)?
    .headless(HeadlessOptions { script: Script::parse("sleep 1000\nkey k\nsnapshot\nkey q")?, ..Default::default() })
    .run()
    .await?;
assert!(report.snapshots[0].contains("1"));
```

## Status

Working: rendering, keyboard, mouse, paste, focus and resize events; request/response,
streaming, websocket, and multipart server functions; background tasks on the component
model's async ABI with HTTP over WASI 0.3; the origin policy with allow lists;
a persistent cookie jar; hot reload; the library API; headless mode; a kill switch and
timeouts; end-to-end tests of all of it. Note that wasmtime's WASI 0.3 support is marked
experimental upstream; rattery pins wasmtime and tracks it.

## License

MIT
