# rattery

**A sandboxed terminal host for [ratatui](https://ratatui.rs) apps delivered over HTTP.**

Write a ratatui app. Declare your backend calls as `#[rattery_app::server]` functions,
Leptos / Dioxus fullstack style. Compile the app to a WASI 0.2 component. Serve it
from your axum server next to the server functions. Users run it with:

```sh
rattery https://apps.example.com/app.wasm
```

The `rattery` binary is to terminal apps what a browser is to web apps: it fetches the
component, runs it in a [wasmtime](https://wasmtime.dev) sandbox, hands it the terminal
through a small WIT interface, keeps its cookies, and lets it make HTTP requests to
**its own origin only** unless you or the other server say otherwise. A *rattery* is an
enclosure for rats. This one keeps a ratatui app where it can't touch your filesystem,
your network, or your other terminals.

## Why

- **Deploy by URL.** Ship a new version by replacing one file on the server. No
  installers, no `curl | sh`, no stale clients. With `--watch`, running apps reload.
- **One fullstack dev model.** `#[rattery_app::server]` is `server_fn`'s `#[server]`
  with the client filled in. The same shared crate compiles into the app (calls become
  HTTP) and into the server (bodies run). It is the crate Leptos and Dioxus use, so
  request/response, streaming responses, websockets, and cookie sessions all work as
  they do there.
- **A real sandbox.** The component gets the terminal, a clock, randomness, and HTTP
  to its origin. Nothing else is linked in. Running someone's TUI from a URL is as
  safe as opening a web page.
- **Embeddable.** The `rattery` crate is a library too. Put a remote TUI behind a subcommand
  of an existing CLI, or ship one app against one endpoint with the component embedded
  in your binary.
- **Thick client.** UI state stays local, the server only answers RPC. Compare with
  SSH-app frameworks, which run the whole UI server-side and stream frames.

## How it works

```
 ┌───────────── your terminal ─────────────┐
 │ rattery (host)                          │      HTTP        ┌────────────────────┐
 │  crossterm ⇄ rattery:tui/terminal ⇄ app │ ───────────────▶ │ axum server        │
 │                (WIT)          (wasm)    │  GET /app.wasm   │  /app.wasm         │
 │  wasi:http ── origin policy, cookies ───┼────────────────▶ │  /api/* server fns │
 └─────────────────────────────────────────┘  POST /api/...   └────────────────────┘
```

- **`crates/rattery-app/wit/rattery.wit`** is the entire contract. It mirrors ratatui's `Backend` trait
  (draw a list of changed cells, cursor, size, flush) plus a crossterm-shaped event
  stream and a `wasi:io` pollable so an app can `await` key presses and server
  responses at the same time. A second small interface provides websockets, which
  WASI 0.2 lacks; the host applies the same origin policy and cookie jar to them.
- **`crates/rattery`** is the `rattery` library and command: wasmtime +
  `wasmtime-wasi` + `wasmtime-wasi-http`, crossterm behind the terminal interface, a
  loader, the origin policy, the cookie jar, and a headless mode. `cargo install
  rattery` gives you the command; `rattery::App` embeds it in your own CLI.
- **`crates/rattery-app`** is what apps depend on: a ratatui `Backend` over the WIT
  interface, the event API, background tasks, timers, websockets, and a `server_fn`
  client that speaks `wasi:http@0.3`. On native targets it provides only what the
  server build of a shared crate needs.
- **`crates/rattery-macros`** provides `#[rattery_app::server]`.

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

The app is a `cdylib` crate built for `wasm32-wasip2`; `rattery_app::app!` exports the
component's entry point. Server calls run as background tasks so the UI never blocks;
a finished task surfaces as `Event::Wake`:

```toml
[lib]
crate-type = ["cdylib"]
```

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

`examples/counter` is the complete version: background calls with a spinner, a
streaming live feed, a websocket echo, a multipart upload, cookie sessions with
per-session state, ETags for `--watch`, and a CORS opt-in flag. `rattery_app::websocket::WebSocket` is also usable
directly, outside server functions.

`rattery_app::location()` returns the URL the app was loaded from, query string included,
so `rattery https://host/app.wasm?team=infra` passes parameters the way a web page
gets them (the example reads `?title=`). `rattery_app::origin()` is where server calls go.

Event types mirror crossterm's (`KeyCode::Char('q')`, `KeyModifiers::CONTROL`, ...)
so existing ratatui code ports by changing an import. `event::next_timeout` and
`rattery_app::time::sleep` drive animations; `task::wake` lets a long-running task ask for
a redraw, which is how the example renders a streaming response line by line.

## The host

```
rattery <URL or path>
        [--origin URL] [--allow-origin URL]... [--allow-all-origins] [--cors]
        [--incognito | --no-cookies | --cookie-jar FILE]
        [--watch] [--location URL] [--env KEY=VALUE]... [--no-mouse] [--no-cache]
        [--headless COLSxROWS [--script FILE] [--timeout SECS]]
```

**Origin policy.** An app may reach its own origin: where it was loaded from, or
`--origin` for an app loaded from a file. `--allow-origin` adds more,
`--allow-all-origins` disables the check, and `--cors` lets other origins opt in
themselves with `Access-Control-Allow-Origin`, the way they do for browsers. Every
request carries an `Origin` header. Everything else is refused before a connection
is opened.

**Cookies.** The host keeps a jar the way a browser does: the app never sees `Cookie`
or `Set-Cookie`, so ordinary cookie sessions on the server work unchanged and
`HttpOnly` means what it says. The jar persists under the user's local data directory;
`--incognito` keeps it in memory, `--no-cookies` drops everything, `--cookie-jar` picks
a file.

**Reload.** `--watch` polls the URL with `If-None-Match` and restarts the app in place
when the server publishes a new component.

**Safety.** The guest's stdout and stderr are captured and printed after it exits, so
panics are readable and never corrupt the screen. Ctrl-C three times within 1.5 seconds
interrupts an unresponsive app, even one spinning in a tight loop. Raw mode and the
alternate screen are always restored, including on panic.

**Stats.** `--stats` prints phase timings (load, compile, instantiate, first frame) and
terminal counters after the app exits. `cargo xtask bench` runs a rendering and request
latency benchmark; see `docs/perf.md` for what it measures and current numbers.

**Headless.** `--headless 80x24 --script keys.txt` runs the app on an in-memory screen,
feeds it a script (`key k`, `key ctrl-c`, `type hello`, `paste`, `resize`, `sleep`,
`snapshot`; see `--help-script`), and prints the snapshots. This is how the repository's
end-to-end tests work, and it is a ready-made test harness for your own app.

## Embedding the host

```rust
use rattery::{App, CookiePolicy};

// A subcommand of an existing CLI that opens a remote TUI.
let report = App::from_url("https://apps.example.com/dashboard/app.wasm")?
    .allow_origin("https://api.example.com")
    .cookies(CookiePolicy::File(config_dir.join("cookies.json")))
    .run()
    .await?;

// One specific app against one specific backend, embedded in the binary.
let report = App::from_bytes(include_bytes!("app.wasm").to_vec())
    .origin("https://api.example.com")
    .run_blocking()?;
std::process::exit(report.exit_code());
```

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
model's async ABI with HTTP over WASI 0.3; the origin policy with allow lists and CORS;
a persistent cookie jar; hot reload; the library API; headless mode; a kill switch and
timeouts; end-to-end tests of all of it. Note that wasmtime's WASI 0.3 support is marked
experimental upstream; rattery pins wasmtime and tracks it.

## License

MIT
