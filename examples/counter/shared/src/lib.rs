//! Server functions for the counter example.
//!
//! This crate is compiled twice: for `wasm32-wasip2` inside the app, where
//! each function becomes an HTTP call, and natively inside the server with the
//! `axum` feature, where the bodies run.

use rattery::{ServerFnError, server};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub count: i64,
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
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::time::Instant;

    static COUNT: AtomicI64 = AtomicI64::new(0);
    static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

    pub fn adjust(delta: i64) {
        COUNT.fetch_add(delta, Ordering::SeqCst);
    }

    pub fn snapshot() -> Snapshot {
        Snapshot {
            count: COUNT.load(Ordering::SeqCst),
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
pub use state::init;
