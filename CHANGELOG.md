# Changelog

## 0.2.0 (2026-09-08)

The first release intended for crates.io (not yet published): `rattery` (host
library), `rattery-app` (app crate), `rattery-macros`, `rattery-build`.

- Apps are ordinary binary crates built for `wasm32-wasip2`; `rattery_app::app!`
  exports one async `run` and the host drives it with the component model's async
  ABI. HTTP uses `wasi:http@0.3`, timers the 0.3 clock; stdio stays on WASI 0.2.
- Server functions: request/response, streaming, websocket (host-provided), and
  multipart (rattery's own encoding).
- Host library: origin policy with allow lists, cookie jar with session
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
- Second hardening pass: aggregate memory limit across memories and a host
  resource-count limit; websocket `send` is async with bounded, byte-quota'd
  queues in both directions and socket slots reserved before the handshake;
  tasks cancelled on drop and the panic hook restored across a panic;
  `Phase::Ready` only after a successful frame plus `rattery_app::ready()` /
  `Phase::AppReady`; paste, append, output, and message size caps; one cookie
  jar lock for load-modify-save and per-domain quotas. WIT package 0.2.0.
- Third pass: `Limits::validate` (queues must hold one maximum message;
  defaults are 4 MiB messages, 8 MiB queues) and oversized messages refused in
  both directions instead of waiting or overfilling; websocket slots are
  semaphore permits held by the resource; `Phase::Ready` after the first
  successful draw and flush; websocket tasks awaited at shutdown; reload
  errors bounded; cookie quota keyed on domain, path, and name.
- Fourth pass: websocket tasks tracked by a `TaskTracker` that forgets them as
  they finish; the reload reset bypasses readiness accounting; tungstenite's
  write buffer sized for a maximum frame independently of the app queue;
  `message_bytes` is a strict final size.

## 0.1.0 (2026-09-07)

Initial vertical slice: rendering, input, request/response server functions, the
origin policy, a URL loader, the standalone `rattery` command.
