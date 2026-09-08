# Changelog

## 0.2.0 (2026-09-08)

First release on crates.io: `rattery` (host library), `rattery-app` (app crate),
`rattery-macros`, `rattery-build`.

- Apps are ordinary binary crates built for `wasm32-wasip2`; `rattery_app::app!`
  exports one async `run` and the host drives it with the component model's async
  ABI. HTTP uses `wasi:http@0.3`, timers the 0.3 clock; stdio stays on WASI 0.2.
- Server functions: request/response, streaming, websocket (host-provided), and
  multipart (rattery's own encoding).
- Host library: origin policy with allow lists and CORS, cookie jar with session
  persistence, watch-and-reload, headless mode with scripted input and snapshots,
  kill switch and timeouts, timings and counters.
- `rattery-build` compiles an app from `build.rs`; `rattery::embed!()` embeds it.
- The general-purpose command moved to `examples/rattery-cli` and is not published.

## 0.1.0 (2026-09-07)

Initial vertical slice: rendering, input, request/response server functions, the
origin policy, a URL loader, the standalone `rattery` command.
