# Performance

What a rattery app costs, measured on 2026-09-08 with `cargo xtask bench` on a
Linux x86_64 workstation, release builds, wasmtime 48. Numbers are per frame or
per request, medians of 200 iterations; treat them as orders of magnitude.

## What the benchmark measures

`examples/bench` is an app that redraws in one of three patterns, times each
`terminal.draw` from inside the guest, and prints the distribution:

- **full**: every cell changes every frame. The worst case for the cell diff:
  the whole screen crosses the component boundary each frame.
- **sparse**: one row changes per frame. Typical of a live-updating UI.
- **text**: a wrapped paragraph re-rendered each frame. Typical widget cost;
  few cells actually change after ratatui's diff.
- **http**: sequential GETs of the app's origin through `wasi:http`, compared
  with a plain socket from the host process.

The host adds its own counters (`--stats`): time inside `draw` and `flush`,
cells transferred, phase timings from load to first frame.

## Results

Headless (in-memory screen, so this is guest work plus the boundary):

| mode   | screen  | cells/frame | per frame | fps   |
|--------|---------|-------------|-----------|-------|
| full   | 80×24   | 1,920       | 0.20 ms   | 5,000 |
| full   | 200×50  | 10,000      | 0.68 ms   | 1,470 |
| sparse | 80×24   | 80          | 0.03 ms   | 36,000 |
| sparse | 200×50  | 200         | 0.14 ms   | 7,000 |
| text   | 80×24   | ~9          | 0.12 ms   | 8,000 |
| text   | 200×50  | ~11         | 0.24 ms   | 4,000 |

A full 200×50 redraw costs about 70 ns per cell all in: ratatui's diff in the
guest, lowering 10,000 records through the canonical ABI, and the host's
conversion into ratatui cells.

Real terminal (crossterm writing escape sequences to a pty), full mode:

| screen  | per frame | of which host output |
|---------|-----------|----------------------|
| 80×24   | 0.65 ms   | 0.4 ms               |
| 200×50  | 2.6 ms    | 1.7 ms               |

Once a real terminal is attached, generating and writing escape sequences is
the largest cost, as it is for any native ratatui app. The sandbox and the
component boundary are not the bottleneck.

Requests: a GET through `wasi:http@0.3` takes 0.056 ms end to end against a
local axum server; the same request from a plain `std::net::TcpStream` in the
host process takes 0.044 ms. The host's policy check, cookie jar, and body
plumbing add about 12 µs.

Startup, warm compile cache: load 0.2 ms, compile 2 ms, instantiate 0.1 ms,
first frame 0.4 ms. Cold cache: compiling the 400 KB release component takes
35 ms; compiling a 19 MB debug component takes 60 to 120 ms (wasmtime compiles
in parallel). Host resident memory while running the benchmark: 16.5 MB.

## Native against rattery: interactive workloads

`cargo xtask perf` answers the question "does it feel like a native app":
the same ratatui widgets, driven by the same synthetic input, rendered by
a native binary and by the component, on an in-memory backend (pure
render cost) and on a real pseudo-terminal (raw mode, alternate screen,
every changed cell written out and consumed). Four workloads stand in for
real apps: an animated dashboard (sparkline, gauge, scrolling log,
spinner), a grid of buttons clicked, a divider dragged across two panes of
wrapped text, and a hover grid. 200×50, 200 events 5 ms apart, 300
animation frames; measured on the machine above:

| workload | runner | frame avg | frame p95 | input avg | input p95 |
|---|---|---|---|---|---|
| animation, back to back | native, in memory | 0.23 ms | 0.28 ms | | |
| | native, terminal | 0.21 ms | 0.22 ms | | |
| | rattery, in memory | 0.31 ms | 0.31 ms | | |
| | rattery, terminal | 0.33 ms | 0.35 ms | | |
| animation at 60 Hz | native, in memory | 0.71 ms | 0.83 ms | interval p95 16.4 ms | 0 late |
| | native, terminal | 0.78 ms | 0.93 ms | interval p95 16.4 ms | 0 late |
| | rattery, in memory | 1.04 ms | 1.27 ms | interval p95 16.8 ms | 0 late |
| | rattery, terminal | 1.15 ms | 1.39 ms | interval p95 16.8 ms | 0 late |
| click | native, in memory | 3.02 ms | 4.51 ms | 3.0 ms | 4.5 ms |
| | native, terminal | 2.77 ms | 4.30 ms | 2.8 ms | 4.3 ms |
| | rattery, in memory | 3.97 ms | 6.10 ms | 4.1 ms | 6.2 ms |
| | rattery, terminal | 3.86 ms | 5.98 ms | 4.0 ms | 6.1 ms |
| drag | native, in memory | 0.76 ms | 0.92 ms | 0.8 ms | 0.9 ms |
| | native, terminal | 0.88 ms | 1.01 ms | 0.9 ms | 1.0 ms |
| | rattery, in memory | 1.34 ms | 1.63 ms | 1.5 ms | 1.9 ms |
| | rattery, terminal | 1.44 ms | 1.73 ms | 1.5 ms | 1.9 ms |
| hover | native, in memory | 2.04 ms | 2.21 ms | 2.0 ms | 2.2 ms |
| | native, terminal | 1.97 ms | 2.15 ms | 2.0 ms | 2.1 ms |
| | rattery, in memory | 2.47 ms | 2.70 ms | 2.6 ms | 2.9 ms |
| | rattery, terminal | 2.51 ms | 2.78 ms | 2.9 ms | 3.2 ms |

"Input" is input-to-frame latency: from an input event arriving to the end
of the frame that shows it. The host measures it from the event being
queued to the end of the first flush after the app read it; the native
harness from the event's arrival to `draw` returning; the same thing, so
the columns compare. (The host merges pointer bursts keeping the oldest
timestamp, so a hover backlog is measured from the first movement.)

What the table says:

**Equivalent as far as a user can tell.** Every frame on every workload is
under 7 ms, against a 16 ms frame budget, and the 60 Hz animation holds its
cadence on both sides with no late frames; the component's timer adds under
half a millisecond of jitter at the 95th percentile. Input reaches the
screen within 3 to 6 ms either way.

**The difference is the compiler, not the architecture.** rattery costs
0.1 ms more per animation frame and 0.5 to 1 ms more per input frame,
which is the same ratatui code running under Cranelift instead of LLVM
(1.3× to 1.8× on these workloads, as in the hover section). The parts
rattery adds around it are small and constant: the cell diff and terminal
write on the host are 10 to 70 µs, and the event hop through the host
queue is what separates the input column from the frame column, 0.1 to
0.3 ms.

**The terminal is not the bottleneck.** Writing the changed cells to a
pseudo-terminal costs nothing measurable at this size on either side; the
in-memory and terminal rows agree to within noise. A slow terminal
emulator would slow both equally, since rattery emits the same cells the
native app does.

To measure your own app: `rattery --stats` on a real terminal prints the
input-to-frame figures for your own mouse and keyboard when the app
exits; `--script` feeds it recorded input on a real terminal too, and
`--stats-file` keeps the report when the terminal is a pty. `cargo xtask
perf --work 10` renders each layout ten times per frame for a heavier
UI; `--no-pty` skips the terminal rows.

## Mouse movement and hover

`cargo xtask hover` measures the case that makes a TUI feel slow rather than
merely cost CPU: a tiled layout that highlights the tile under the pointer
and redraws on every movement, the way a tiling widget library does. The
pointer sweeps the screen 400 times, 5 ms apart (200 Hz, about what a
terminal emits). `work=10` renders the layout ten times per frame to stand
in for a heavier UI. Measured on the same machine as above, 200×50 headless:

| run | frame ms | lag at end |
|---|---|---|
| native ratatui, `TestBackend` | 0.96 | - |
| component, release | 2.4 | 0 |
| component, release, work=10, every event | 10.8 | 1894 ms |
| component, release, work=10, coalesced | 11.1 | 7 ms |
| component, opt-level 0, every event | 21.7 | 6252 ms |
| component, opt-level 0, coalesced | 22.1 | 26 ms |
| component, opt-level 0, work=10, every event | 218 | 84.6 s |
| component, opt-level 0, work=10, coalesced | 217 | 369 ms |
| native, work=10 | 9.0 | - |

Three things follow.

**The boundary is not the cost.** The host spends 1 to 5 µs per frame on
the cell diff (about 76 cells change per hover frame). Rendering inside the
component costs about 2.5× native for the same ratatui code (Cranelift
against LLVM), which is the whole difference between the first two rows.

**A debug build is the cost.** At opt-level 0 the same widget takes 22 ms
a frame, nine times the release figure and twenty-three times native. That
is the difference between hover that keeps up with a 200 Hz pointer and one
that cannot. The workspace here sets `[profile.dev.package."*"] opt-level =
2`, but that covers registry dependencies only: an app crate, or a widget
library vendored as a path dependency, is compiled at opt-level 0 in a dev
build. Build apps in release (`rattery-build` does by default), or add the
app and vendored crates to the profile override.

**Once a frame is slower than the pointer, the queue is the cost.** Every
movement queued behind a slow frame is another full frame later, so the lag
grows for as long as the pointer moves: two seconds at work=10, six at
opt-level 0, eighty-five with both, and the hover highlight trails the
pointer by that much. The
host therefore merges a pointer movement into an unread one (and a resize
into an unread resize) by default; only the latest position matters. The
backlog then never exceeds one frame, and the lag at the end of the sweep is
one frame's worth. Drags, clicks, scrolls, and keys are never merged.
`App::coalesce_input(false)` and `--no-coalesce` turn it off for measuring.

Precompiled (`rattery::precompile` at build time, `App::from_precompiled` at
run time): the compile step becomes a deserialisation, 1.1 ms instead of 49 ms
cold for the bench component, and needs no compile cache on the user's
machine. The counter shim ships this way.

Sizes: the release example component is 400 KB; the stripped host binary is
27 MB (wasmtime with Cranelift, rustls, tokio, tungstenite).

## What changed as a result

- **Dev builds optimise dependencies.** `[profile.dev.package."*"] opt-level = 2`
  in the workspace. App code stays unoptimised for fast iteration, but ratatui
  and the runtime glue are where frame time goes: a full 200×50 debug redraw
  went from 8.3 ms to 2.9 ms and the debug component from 24 MB to 19 MB.
- **Frames borrow their symbols.** The guest bindings lower import parameters
  in borrowing mode, so a frame references the buffer's strings instead of
  allocating one `String` per changed cell: 0.84 ms to 0.68 ms for a full
  200×50 redraw.
- **Stripped release binaries.**

## What was considered and left alone

- **A denser cell encoding** (a `char` fast path instead of `string`, or
  run-length rows). At 70 ns per cell the boundary is a few percent of a
  frame on a real terminal; not worth a contract change now.
- **Winch** (wasmtime's fast baseline compiler) for debug components. Cold
  compile of a debug component is already around 100 ms and cached
  afterwards.
- **Input latency.** Key press to frame is bounded by the same numbers above:
  one async host call to deliver the event and one draw. Nothing to optimise
  until something shows up in practice.

## Running it

```sh
cargo xtask bench            # 200 frames per cell of the matrix
cargo xtask bench --frames 1000
rattery --stats <app>        # timings and counters for any app
```
