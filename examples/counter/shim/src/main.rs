//! The whole shim. `counter-shim [ORIGIN]` runs the embedded counter app
//! against the given server (default: a local dev server).

use std::process::exit;

fn main() {
    let origin = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:3000".to_owned());
    let report = rattery::App::from_bytes(rattery::embed!().to_vec())
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
