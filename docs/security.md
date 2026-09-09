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
credentials through headers. Quotas: 64 cookies per host, 4 KiB each. A
persistent jar is a private file: owner-only permissions in an owner-only
directory, never followed through a symbolic link, shared between processes
under a file lock, replaced by writing a temporary file, syncing it, renaming
it into place, and syncing the directory.

## Resource limits

`Limits` bounds what an app may use; every field has a default for an
ordinary TUI and can be tightened for untrusted apps.

| limit | default | enforced by |
|---|---|---|
| linear memory | 256 MiB | wasmtime store limiter, growth traps |
| CPU time | unlimited | epoch ticks every 10 ms while the guest executes; over budget stops the app with `AppStatus::LimitExceeded` |
| component size | 64 MiB | checked before download completes and before compile |
| download time | 60 s | HTTP client timeout |
| input event queue | 1024 | oldest events dropped |
| cells per frame | 1 M | frame refused, app stopped |
| open websockets | 16 | connect refused |
| websocket queue | 1024 messages | oldest dropped |
| websocket message | 16 MiB | tungstenite frame and message caps, send refused |
| concurrent HTTP requests | 16 | semaphore held for the request's lifetime |
| request body | 64 MiB | `Limited` body |
| response body | 64 MiB | `Limited` body |
| guest stdout / stderr | 1 MiB each | bounded pipe |
| tables, memories, instances | 32, 8, 16 | wasmtime store limiter |

The epoch ticks continuously from a host task, not only when the host has
something to say, so a tight loop in the guest is interrupted within a tick,
and every tick yields to the host so input, timeouts, the kill switch
(Ctrl-C three times), and reloads stay responsive.

## Process hygiene

Every task started for a run (input reader, epoch ticker, watcher, script
runner, timeout, websocket connections) is cancelled and awaited before the
terminal is handed back. Terminal setup is transactional: if enabling a later
mode fails, the earlier ones are undone, and on exit or panic every mode is
restored in reverse order. The panic hook installed for that wraps the
previous hook and is put back on exit.

## Compatibility

`rattery::ABI` and `rattery_app::ABI` name the contract: the WIT package
version, the component-model async ABI, and the WASI HTTP version. Record it
in release metadata, and run `rattery::inspect` on externally obtained bytes
before shipping them; it reports imports, exports, extension imports the host
must provide, and whether the component targets this ABI. The async component
ABI and `wasi:http@0.3` are still experimental in wasmtime; rattery pins the
wasmtime major version and tracks it. Minimum supported Rust is 1.95.

## Not covered

- A hostile app can still exhaust the terminal's own resources within the
  limits (fill the screen, spin for its CPU budget). Tighten `Limits` for
  untrusted apps.
- The wasmtime compile cache is shared by everything the user runs.
- The default persistent cookie jar is shared by every rattery app the user
  runs; give a shim its own jar with `CookiePolicy::File`.
