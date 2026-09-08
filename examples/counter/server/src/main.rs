//! The backend for the counter example.
//!
//! It does two things a web server would do for a Leptos app: serve the
//! compiled client (`/app.wasm`) and answer server-function calls (`/api/*`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use clap::Parser;
use tokio::net::TcpListener;

#[derive(Debug, Parser)]
#[command(name = "counter-server")]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:3000")]
    bind: String,

    /// Path of the compiled app component to serve at /app.wasm.
    #[arg(
        long,
        env = "COUNTER_APP_WASM",
        default_value = "target/wasm32-wasip2/debug/counter-app.wasm"
    )]
    app: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    counter_shared::init();

    let router = Router::new()
        .route("/", get(index))
        .route("/app.wasm", get(serve_app))
        .route(
            "/api/{*rest}",
            any(rattery::server_fn::axum::handle_server_fn),
        )
        .with_state(Arc::new(args.app.clone()));

    let listener = TcpListener::bind(&args.bind).await?;
    let addr = listener.local_addr()?;
    println!("counter-server listening on http://{addr}");
    println!("  serving {} at /app.wasm", args.app.display());
    println!("  run the app with: rattery http://{addr}/app.wasm");
    axum::serve(listener, router).await?;
    Ok(())
}

async fn index() -> &'static str {
    "This server hosts a rattery app.\n\nRun it in your terminal with:\n    rattery http://<this host>/app.wasm\n"
}

async fn serve_app(State(path): State<Arc<PathBuf>>) -> Response {
    match tokio::fs::read(&*path).await {
        Ok(bytes) => (
            [(header::CONTENT_TYPE, "application/wasm"), (header::CACHE_CONTROL, "no-cache")],
            bytes,
        )
            .into_response(),
        Err(err) => (
            StatusCode::NOT_FOUND,
            format!(
                "app component not found at {}: {err}\nbuild it with:\n    cargo build -p counter-app --target wasm32-wasip2\n",
                path.display()
            ),
        )
            .into_response(),
    }
}
