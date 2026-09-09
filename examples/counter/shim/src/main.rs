//! The whole shim. `counter-shim [ORIGIN]` runs the embedded counter app
//! against the given server (default: a local dev server).

use std::process::exit;

fn main() {
    let origin = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:3000".to_owned());
    // SAFETY: the native code was produced by this shim's own build script
    // from the app crate next door, for this target; nothing else wrote it.
    let app = unsafe { rattery::App::from_precompiled(rattery::embed_precompiled!().to_vec()) };
    let report = app
        .origin(&origin)
        .run_blocking()
        .unwrap_or_else(|err| {
            eprintln!("counter: {err:#}");
            exit(1)
        });
    if !report.stderr.is_empty() {
        eprint!("{}", report.stderr);
    }
    exit(report.exit_code());
}
