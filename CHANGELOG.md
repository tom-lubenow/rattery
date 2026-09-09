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
- Hardening (see `docs/security.md`): terminal output containment for cells,
  titles, and guest output; allow-list-only cross-origin (the CORS mode was
  removed) and same-origin-only redirects; `Limits` for memory, CPU time on a
  continuous epoch tick, sizes, queues, and concurrency; private, locked,
  atomically replaced cookie jars; all background tasks cancelled and awaited on
  exit, the panic hook restored, transactional terminal setup.
- Embedding controls: `RequestPolicy`, `on_phase`, `extension` and `state`,
  `from_resolver`, `inspect`, `ABI`. Minimum supported Rust 1.95.

## 0.1.0 (2026-09-07)

Initial vertical slice: rendering, input, request/response server functions, the
origin policy, a URL loader, the standalone `rattery` command.
