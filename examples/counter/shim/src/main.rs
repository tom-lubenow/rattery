//! The whole shim. `counter-shim [ORIGIN]` runs the embedded counter app
//! against the given server (default: a local dev server).

use std::process::exit;
use std::sync::{Arc, Mutex};

fn main() {
    let origin = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:3000".to_owned());
    // SAFETY: the native code was produced by this shim's own build script
    // from the app crate next door, for this target; nothing else wrote it.
    let app = unsafe { rattery::App::from_precompiled(rattery::embed_precompiled!().to_vec()) };
    // An update this build cannot run (a newer ABI) is the one thing worth
    // telling the user about after the app has the screen back.
    let needs_abi: Arc<Mutex<Option<String>>> = Arc::default();
    let seen = needs_abi.clone();
    let report = app
        .on_phase(move |phase| {
            if let rattery::Phase::UpdateRejected {
                requires_abi: Some(abi),
                ..
            } = phase
            {
                *seen.lock().unwrap() = Some(abi);
            }
        })
        .origin(&origin)
        .run_blocking()
        .unwrap_or_else(|err| {
            eprintln!("counter: {err:#}");
            exit(1)
        });
    if !report.stderr.is_empty() {
        eprint!("{}", report.stderr);
    }
    if let Some(abi) = needs_abi.lock().unwrap().take() {
        eprintln!(
            "counter: a newer version of this app needs rattery ABI {abi}; this build supports {}. Please upgrade counter-shim.",
            rattery::ABI
        );
    }
    exit(report.exit_code());
}
