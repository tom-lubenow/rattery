# Changelog

## 0.3.3 (2026-09-10)

- Input coalescing: a pointer movement or resize the app has not read yet
  is replaced by the newer one (`App::coalesce_input`, default on), so a
  frame slower than the pointer no longer builds a backlog. Drags, clicks,
  scrolls, and keys are delivered in full. `Stats::events_coalesced`,
  `Stats::last_draw`, `Stats::last_event` (their difference is the lag at
  the end of a run). Headless scripts gain `mouse move X Y` and
  `sweep STEPS MS`; the bench app gains a `hover` mode with a `work`
  multiplier and a native baseline binary; `cargo xtask hover` runs the
  matrix. See the mouse movement section of `docs/perf.md`.

## 0.3.2 (2026-09-10)

- ABI transitions: component fetches send a `rattery-abi` request header
  with the host's `ABI`; a `426 Upgrade Required` answer (with the required
  ABI in the same response header) is understood. `Phase::UpdateRejected`
  gains `requires_abi`, set when a rejected update or such an answer needs a
  rattery ABI this host does not provide; the same rejection is reported
  once, not every poll. Startup errors name both ABIs. `ComponentInfo::abi`.
  `App::take_phase_hook` for wrapping a hook. The counter shim and the
  example CLI print an upgrade hint on exit; the example server has
  `--require-abi-file` to demonstrate the server side.

## 0.3.1 (2026-09-10)

- A new component found by the watcher is compiled and linked before the app
  hears about it. One that fails is reported as `Phase::UpdateRejected` and
  never interrupts the running app; the watcher keeps polling. Before, the
  app was torn down first and a broken deploy could end the run.
- A component that compiles but does not link against this host (another
  ABI) now fails at startup with a readable error, before the terminal is
  touched. If a reloaded component fails to instantiate, the previous one is
  restored instead of ending the run.

## 0.3.0 (2026-09-10)

WIT package `rattery:tui@0.3.0`; `ABI` is now
`rattery:tui@0.3.0;cm-async;wasi:http@0.3.0`. Apps and hosts must move
together.

- App-controlled reload: the watcher's update reaches the app as
  `Event::UpdateAvailable` (version, deadline) and the app restarts itself
  with `rattery_app::reload()`. `App::reload_policy` picks `Immediate`,
  `AppControlled`, or `Deferred { grace }` (the default, five minutes);
  `Phase::UpdateAvailable`; headless scripts gain `update [VERSION]`; the
  example CLI gains `--reload-grace`.

- WIT `rattery:tui@0.2.0` gains `terminal.log` and a `storage` interface
  (origin-scoped key-value storage: `get`, `set`, `remove`, `keys`, `clear`,
  `usage`). `rattery_app::storage` wraps it; the `log` facade is routed to the
  host.
- Host: `StoragePolicy` (`Persistent` per origin under the local data
  directory, `Dir`, `Ephemeral`, `Disabled`), `Limits::storage_*` quotas,
  `Phase::Log` with `LogLevel`, `Limits::logs_per_second`, `Stats::logs` /
  `logs_dropped`. The example CLI adds `--storage-dir`, `--no-storage`,
  `--log-file`.
- Precompiled components: `rattery::precompile`, `App::from_precompiled`
  (unsafe: native code), `rattery::embed_precompiled!`, and
  `rattery_build::App::precompile(true)` behind the `precompile` feature of
  `rattery-build`, which exports `RATTERY_APP_CWASM`. The counter shim uses it.
  Publish order is now rattery-macros, rattery-app, rattery, rattery-build.

## 0.2.0 (2026-09-09)

The first release on crates.io: `rattery` (host library), `rattery-app` (app
crate), `rattery-macros`, `rattery-build`.

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
