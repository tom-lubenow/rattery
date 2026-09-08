//! Server functions for the counter example.
//!
//! This crate is compiled twice: for `wasm32-wasip2` inside the app, where
//! each function becomes an HTTP call, and natively inside the server with the
//! `axum` feature, where the bodies run.

use rattery::{ServerFnError, server};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// This session's count. Sessions are cookies the server sets; the host
    /// keeps them like a browser would, so a session survives restarts.
    pub count: i64,
    pub session: String,
    pub server_pid: u32,
    pub uptime_secs: u64,
}

/// Read the current counter.
#[server]
pub async fn fetch_snapshot() -> Result<Snapshot, ServerFnError> {
    Ok(state::snapshot())
}

/// Move the counter by `delta` and return the new state.
#[server]
pub async fn adjust_count(delta: i64) -> Result<Snapshot, ServerFnError> {
    state::adjust(delta);
    Ok(state::snapshot())
}

/// Like [`fetch_snapshot`], but the server takes `delay_ms` to answer. Shows
/// that the UI stays responsive while a call is in flight.
#[server]
pub async fn slow_snapshot(delay_ms: u64) -> Result<Snapshot, ServerFnError> {
    tokio::time::sleep(std::time::Duration::from_millis(delay_ms.min(30_000))).await;
    Ok(state::snapshot())
}

#[cfg(feature = "ssr")]
mod state {
    use super::Snapshot;
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};
    use std::time::Instant;

    tokio::task_local! {
        /// The session id of the request being handled, set by the server's
        /// session middleware before a server function body runs.
        pub static SESSION: String;
    }

    static COUNTS: LazyLock<Mutex<HashMap<String, i64>>> = LazyLock::new(Mutex::default);
    static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

    fn session() -> String {
        SESSION
            .try_with(Clone::clone)
            .unwrap_or_else(|_| "anonymous".to_owned())
    }

    pub fn adjust(delta: i64) {
        *COUNTS.lock().unwrap().entry(session()).or_default() += delta;
    }

    pub fn snapshot() -> Snapshot {
        let session = session();
        Snapshot {
            count: COUNTS.lock().unwrap().get(&session).copied().unwrap_or(0),
            session,
            server_pid: std::process::id(),
            uptime_secs: STARTED.elapsed().as_secs(),
        }
    }

    /// Start the uptime clock when the server boots.
    pub fn init() {
        LazyLock::force(&STARTED);
    }
}

#[cfg(feature = "ssr")]
pub use state::{SESSION, init};
