# Security model

What a rattery host promises about the app it runs, and what it does not.

## The app is untrusted

Everything an app does reaches the outside world through the host. The host
treats the app the way a browser treats a page: it may draw, read input, keep
state on the server it came from, and nothing else.

**No ambient capabilities.** The component is linked against the terminal
interface, a clock, randomness, and `wasi:http`. No filesystem preopens, no
sockets, no inherited stdio, no environment beyond what the embedder sets with
`App::env`.

**Terminal output is contained** (`rattery::sanitize`). Every cell the app
draws is validated before it reaches the terminal backend: control characters
(C0, DEL, C1, the Unicode line separators), more than one grapheme cluster, a
width over two columns, or an oversized symbol are replaced with U+FFFD; cells
outside the screen are dropped. The window title has control characters
stripped and its length bounded. The app's stdout, stderr, and any trap
message are kept raw in the `Report`, and `sanitize::text` renders them with
every control character shown as an escape; embedders must print them through
it, as `examples/rattery-cli` does. The counters in `Stats::cells_rejected`
show when an app tried.

**Origin policy.** HTTP requests and websocket handshakes may only go to the
app's own origin and to origins the embedder allow-lists. The app's origin is
where the component was fetched from, after redirects; the loader follows only
same-origin redirects and refuses cross-origin ones, so privileges derive from
the final URL. The origin of an app loaded from bytes or a file is whatever
the embedder says with `App::origin`. There is no CORS mode: browser-style
cross-origin access needs preflight and credential semantics this host does
not implement, so cross-origin is allow-list only. Every request carries an
`Origin` header.

**Request policy.** `App::request_policy` installs an async hook that sees
every allowed request head and websocket handshake and can refuse it, edit
it (inject or refresh credentials), or hold a guard for its duration. Denials
are reported through `Phase::RequestDenied` with the reason; the app sees a
generic failure.

**Cookies.** The app never sees `Cookie` or `Set-Cookie`; the host attaches
and records them, so `HttpOnly` holds and the app cannot forge or exfiltrate
credentials through headers. Quotas: 64 cookies per domain whatever their
paths, 4 KiB each. A persistent jar is a private file: owner-only permissions
in an owner-only directory, never followed through a symbolic link, and
every load and every load-modify-save runs under one exclusive lock shared by
readers and writers, so concurrent processes never overwrite each other's
cookies. Writes go to a temporary file that is synced, renamed into place,
and followed by a directory sync.

## Resource limits

`Limits` bounds what an app may use; every field has a default for an
ordinary TUI and can be tightened for untrusted apps.

| limit | default | enforced by |
|---|---|---|
| linear memory | 256 MiB in aggregate across all memories | a custom `ResourceLimiter`; growth beyond it traps |
| host resources held (streams, bodies, sockets) | 4096 | `ResourceTable` capacity |
| CPU time | unlimited | epoch ticks every 10 ms while the guest executes; over budget stops the app with `AppStatus::LimitExceeded` |
| component size | 64 MiB | checked before download completes and before compile |
| download time | 60 s | HTTP client timeout |
| input event queue | 1024 | oldest events dropped |
| paste event | 1 MiB | longer pastes are cut at a character boundary |
| cells per frame | 1 M | frame refused, app stopped |
| open websockets | 16 | a semaphore permit taken before the handshake and held by the socket resource, so cancelled attempts, failures, and drops all release it |
| websocket queues | 64 messages and 8 MiB per direction | incoming: oldest dropped; outgoing: `send` is async and waits (backpressure); a message larger than the queue is refused |
| websocket message | 4 MiB | tungstenite frame and message caps, send refused; `Limits::validate` requires the queue to hold one |
| concurrent HTTP requests | 16 | semaphore held for the request's lifetime |
| request body | 64 MiB | `Limited` body |
| response body | 64 MiB | `Limited` body |
| guest stdout / stderr | 1 MiB each, in total across reloads | bounded pipe, oldest output dropped |
| storage per origin | 5 MiB, 1024 entries, 256 B keys, 1 MiB values | `set` returns `quota-exceeded` / `too-large` |
| log records | 1000 per second | the rest are dropped and counted in `Stats::logs_dropped`; target and message are sanitised and cut at `message_bytes` |
| error and trap messages | 16 KiB | truncated |
| `append-lines` | one screen | clamped to the screen height |
| tables, memories, instances | 32, 8, 16 | wasmtime store limiter |

The epoch ticks continuously from a host task, not only when the host has
something to say, so a tight loop in the guest is interrupted within a tick,
and every tick yields to the host so input, timeouts, the kill switch
(Ctrl-C three times), and reloads stay responsive.

## Storage

Origin-scoped storage is the terminal's `localStorage`. The host keys it on
the app's normalised origin, so two apps served from the same origin share a
store and nothing else can read it; an app without an origin (a file run
without `--origin`) gets memory that is forgotten when it exits. On disk it
is one JSON file per origin under `rattery/storage` in the user's local data
directory (or the directory `StoragePolicy::Dir` names), created with mode
0600, written atomically through a temporary file, serialised with the same
lock file discipline as the cookie jar, and never followed through a symlink.
Values are bytes; the app decides what they mean. `StoragePolicy::Disabled`
makes every write fail with `disabled` while reads return nothing.

## Logs

`rattery_app` installs a `log` backend that hands records to the host, which
sanitises the target and message (control characters are shown escaped),
bounds them by `message_bytes`, rate-limits them, and delivers them to the
embedder as `Phase::Log`. The host writes nothing itself: on a real terminal
the embedder should send them to a file, a socket, or a pane of its own, never
to the screen the app is drawing on.

## Process hygiene

Every task started for a run (input reader, epoch ticker, watcher, script
runner, timeout, websocket connections) is cancelled and awaited before the
terminal is handed back, and cancelled by `Drop` if the run is abandoned (the
future dropped, a timeout, a panic). Terminal setup is transactional: if
enabling a later mode fails, the earlier ones are undone, and on exit or panic
every mode is restored in reverse order. The panic hook installed for that
wraps the previous hook; it is put back on exit, and if the run panics, the
panic is caught, the hook restored, and the panic resumed.

A reload never happens behind the app's back unless the embedder asks for
that: `ReloadPolicy::Immediate` replaces it at once, `AppControlled` only
delivers `Event::UpdateAvailable`, and `Deferred` (the default) delivers the
event with its deadline and forces the reload when the deadline passes. The
first deadline stands across further updates, so a stream of deploys cannot
postpone it. The version string in the event is the server's validator,
sanitised and cut to 256 bytes. A new component is compiled and linked
against the host before the app is told about it; one that fails becomes
`Phase::UpdateRejected` and the running app is left alone.

`Phase::Ready` fires after the first frame has been validated, drawn, and
flushed successfully. An app that wants a stronger signal calls
`rattery_app::ready()` when its data is loaded and a real screen is up, which
arrives as `Phase::AppReady`.

## Compatibility

`rattery::ABI` and `rattery_app::ABI` name the contract: the WIT package
version, the component-model async ABI, and the WASI HTTP version. Record it
in release metadata, and run `rattery::inspect` on externally obtained bytes
before shipping them; it reports imports, exports, extension imports the host
must provide, the rattery ABI it targets, and whether that is this host's.
Versions match on major.minor. A component for another ABI is refused at
link time, before the terminal is touched at startup and before a running
app is told about an update (`Phase::UpdateRejected` with `requires_abi`).
Component fetches carry the `rattery-abi` request header; a server may
answer `426 Upgrade Required` naming the ABI it needs in the same header,
which is reported the same way, once per distinct answer. Header and body
of such an answer are sanitised and cut to 256 bytes. A precompiled
component (`rattery::precompile`, `App::from_precompiled`) is native code and
is trusted as such: wasmtime checks its header, engine settings, and target
triple, not its contents, so precompile at build time and embed the result;
never deserialise bytes fetched at run time. The async component
ABI and `wasi:http@0.3` are still experimental in wasmtime; rattery pins the
wasmtime major version and tracks it. Minimum supported Rust is 1.95.

## Not covered

- A hostile app can still exhaust the terminal's own resources within the
  limits (fill the screen, spin for its CPU budget). Tighten `Limits` for
  untrusted apps.
- The wasmtime compile cache is shared by everything the user runs.
- The default persistent cookie jar is shared by every rattery app the user
  runs; give a shim its own jar with `CookiePolicy::File`.
