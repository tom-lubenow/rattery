# rattery

**A sandboxed terminal host for [ratatui](https://ratatui.rs) apps delivered over HTTP.**

Write a ratatui app. Declare your backend calls as `#[rattery::server]` functions,
Leptos / Dioxus fullstack style. Compile the app to a WASI 0.2 component. Serve it
from your axum server next to the server functions. Users run it with:

```sh
rattery https://apps.example.com/app.wasm
```

The `rattery` binary is to terminal apps what a browser is to web apps: it fetches the
component, runs it in a [wasmtime](https://wasmtime.dev) sandbox, hands it the terminal
through a small WIT interface, and lets it make HTTP requests to **its own origin only**.
A *rattery* is an enclosure for rats. This one keeps a ratatui app where it can't
touch your filesystem, your network, or your other terminals.

## Why

- **Deploy by URL.** Ship a new version by replacing one file on the server. No
  installers, no `curl | sh`, no stale clients.
- **One fullstack dev model.** `#[rattery::server]` is `server_fn`'s `#[server]`
  with the client filled in. The same shared crate compiles into the app (calls become
  HTTP) and into the server (bodies run). It is the crate Leptos and Dioxus use.
- **A real sandbox.** The component gets the terminal, a clock, randomness, and HTTP
  to the origin it was loaded from. Nothing else is linked in. Running someone's TUI
  from a URL is as safe as opening a web page.
- **Thick client.** UI state stays local, the server only answers RPC. Compare with
  SSH-app frameworks, which run the whole UI server-side and stream frames.

## How it works

```
 ┌───────────── your terminal ─────────────┐
 │ rattery (host)                          │      HTTP        ┌────────────────────┐
 │  crossterm ⇄ rattery:tui/terminal ⇄ app │ ───────────────▶ │ axum server        │
 │                (WIT)          (wasm)    │  GET /app.wasm   │  /app.wasm         │
 │  wasi:http ──── same-origin policy ─────┼────────────────▶ │  /api/* server fns │
 └─────────────────────────────────────────┘  POST /api/...   └────────────────────┘
```

- **`wit/rattery.wit`** is the entire contract. It mirrors ratatui's `Backend` trait
  (draw a list of changed cells, cursor, size, flush) plus a crossterm-shaped event
  stream and a `wasi:io` pollable so an app can `await` key presses and server
  responses at the same time.
- **`crates/rattery`** is what apps depend on: a ratatui `Backend` over the WIT
  interface, an event API, the async runtime (`wstd`), and a `server_fn` client that
  speaks `wasi:http`. On native targets it provides only what the server build of a
  shared crate needs.
- **`crates/rattery-host`** is the `rattery` binary: wasmtime + `wasmtime-wasi` +
  `wasmtime-wasi-http`, crossterm behind the terminal interface, a loader, and the
  origin policy.
- **`crates/rattery-macros`** provides `#[rattery::server]`.

Diffing happens inside ratatui's `Terminal` in the guest, so a frame is one `draw` call
carrying only the cells that changed, and one `flush`.

## Quick start

Everything is in the nix dev shell (`direnv allow` or `nix develop`): stable Rust
with the `wasm32-wasip2` target, `wasm-tools`, and the `wasmtime` CLI.

```sh
# 1. build the app component
cargo build -p counter-app --target wasm32-wasip2

# 2. run the server (serves /app.wasm and /api/*)
cargo run -p counter-server

# 3. in another terminal, run the app like a browser would
cargo run -p rattery-host -- http://127.0.0.1:3000/app.wasm
```

Rebuild the component and restart `rattery`; the server picks up the new file on the
next request. Compiled components are cached by wasmtime, so a second start is
instant.

## Writing an app

A shared crate holds the server functions and any types they exchange:

```rust
// counter-shared/src/lib.rs
use rattery::{server, ServerFnError};

#[server]
pub async fn adjust_count(delta: i64) -> Result<i64, ServerFnError> {
    Ok(state::adjust(delta)) // only compiled with the `ssr` feature
}
```

```toml
[features]
ssr = ["rattery/ssr"]
axum = ["ssr", "rattery/axum"]
```

The app is an ordinary binary crate built for `wasm32-wasip2`:

```rust
use rattery::prelude::*;
use rattery::event;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    rattery::run(app)
}

async fn app(mut terminal: Terminal) -> Result<(), Box<dyn std::error::Error>> {
    let mut count = adjust_count(0).await?;
    loop {
        terminal.draw(|frame| frame.render_widget(count.to_string(), frame.area()))?;
        match event::next().await {
            Event::Key(key) if key.code == KeyCode::Char('q') => break Ok(()),
            Event::Key(key) if key.code == KeyCode::Up => count = adjust_count(1).await?,
            _ => {}
        }
    }
}
```

The server depends on the shared crate with the `axum` feature and mounts two routes:

```rust
Router::new()
    .route("/app.wasm", get(serve_component))
    .route("/api/{*rest}", any(rattery::server_fn::axum::handle_server_fn))
```

`examples/counter` is the complete version of this.

Event types mirror crossterm's (`KeyCode::Char('q')`, `KeyModifiers::CONTROL`, ...)
so existing ratatui code ports by changing an import. Because the app is single
threaded and async, long server calls can run in a spawned task
(`rattery::runtime::spawn`) while the UI keeps handling input.

## The host

```
rattery <URL or path> [--origin URL] [--allow-origin URL]... [--allow-all-origins]
                      [--no-mouse] [--no-cache]
```

- Loaded from a URL, the app may reach that URL's origin. `--allow-origin` adds
  more; `--allow-all-origins` disables the check.
- Loaded from a file, the app has no origin and server calls fail unless you pass
  `--origin`.
- The guest's stdout and stderr are captured and printed after it exits, so panics
  are readable and never corrupt the screen.
- Ctrl-C three times within 1.5 seconds interrupts an unresponsive app.
- Raw mode and the alternate screen are always restored, including on panic.

## Status

v0.1.0 is a working vertical slice: rendering, keyboard, mouse, paste, focus and
resize events, request/response server functions, streaming responses, the origin
policy, the loader and cache. Not yet supported: websocket server functions,
multipart bodies, WASI 0.3 async, and publishing the crates (the WIT lives at the
workspace root for now).

## License

MIT
