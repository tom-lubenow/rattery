//! Server functions for the counter example.
//!
//! This crate is compiled twice: for `wasm32-wasip2` inside the app, where
//! each function becomes an HTTP call, and natively inside the server with the
//! `axum` feature, where the bodies run.

use rattery_app::multipart::{MultipartData, MultipartFormData};
use rattery_app::server_fn::codec::{JsonEncoding, StreamingText, TextStream};
use rattery_app::server_fn::{BoxedStream, Websocket};
use rattery_app::{ServerFnError, server};
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

/// A stream of lines from the server, one every half second, for as long as
/// the app keeps reading. Shows that a server function can push updates.
#[server(output = StreamingText)]
pub async fn live_feed() -> Result<TextStream, ServerFnError> {
    use futures::StreamExt;
    use std::time::Duration;

    // The task-local session is only set while this handler runs; capture it
    // before the stream outlives the request.
    let session = state::session_id();
    let ticks = futures::stream::unfold(0u64, move |tick| {
        let session = session.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let snapshot = state::snapshot_for(&session);
            let line = format!(
                "tick {}  count {}  up {}s\n",
                tick + 1,
                snapshot.count,
                snapshot.uptime_secs
            );
            Some((line, tick + 1))
        }
    });
    Ok(TextStream::from(ticks.boxed()))
}

/// A websocket server function: every message the app sends comes back
/// answered, for as long as the connection lives.
#[server(protocol = Websocket<JsonEncoding, JsonEncoding>)]
pub async fn chat(
    input: BoxedStream<String, ServerFnError>,
) -> Result<BoxedStream<String, ServerFnError>, ServerFnError> {
    use futures::StreamExt;

    let mut input = input;
    let replies = futures::stream::poll_fn(move |cx| input.poll_next_unpin(cx)).map(|message| {
        message.map(|text| match text.strip_prefix("ping ") {
            Some(n) => format!("pong {n}"),
            None => format!("echo: {text}"),
        })
    });
    Ok(replies.into())
}

/// A file upload: the app sends `multipart/form-data`, the server describes
/// what it received.
#[server(input = MultipartFormData)]
pub async fn upload(data: MultipartData) -> Result<String, ServerFnError> {
    let mut multipart = data
        .into_inner()
        .ok_or_else(|| ServerFnError::new("upload must run on the server"))?;
    let mut summary = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?
    {
        let name = field.name().unwrap_or("?").to_owned();
        let file_name = field.file_name().map(str::to_owned);
        let bytes = field
            .bytes()
            .await
            .map_err(|e| ServerFnError::new(e.to_string()))?;
        summary.push(match file_name {
            Some(file_name) => format!("{name}: {file_name} {}B", bytes.len()),
            None => format!("{name}: {}", String::from_utf8_lossy(&bytes)),
        });
    }
    Ok(summary.join("\n"))
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

    pub fn session_id() -> String {
        SESSION
            .try_with(Clone::clone)
            .unwrap_or_else(|_| "anonymous".to_owned())
    }

    pub fn adjust(delta: i64) {
        *COUNTS.lock().unwrap().entry(session_id()).or_default() += delta;
    }

    pub fn snapshot() -> Snapshot {
        snapshot_for(&session_id())
    }

    pub fn snapshot_for(session: &str) -> Snapshot {
        Snapshot {
            count: COUNTS.lock().unwrap().get(session).copied().unwrap_or(0),
            session: session.to_owned(),
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
