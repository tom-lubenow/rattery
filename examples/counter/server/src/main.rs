//! The backend for the counter example.
//!
//! It does two things a web server would do for a Leptos app: serve the
//! compiled client (`/app.wasm`) and answer server-function calls (`/api/*`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
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

    /// ABI negotiation demo: while this file exists, its contents name the
    /// rattery ABI a client must have (a prefix such as `rattery:tui@0.4`);
    /// other clients get `426 Upgrade Required`. A real deployment would
    /// serve the build matching the client's `rattery-abi` header instead.
    #[arg(long)]
    require_abi_file: Option<PathBuf>,
}

#[derive(Clone)]
struct AppState {
    app_path: Arc<PathBuf>,
    require_abi_file: Option<Arc<PathBuf>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    counter_shared::init();

    let state = AppState {
        app_path: Arc::new(args.app.clone()),
        require_abi_file: args.require_abi_file.clone().map(Arc::new),
    };

    let router = Router::new()
        .route("/", get(index))
        .route("/app.wasm", get(serve_app))
        .route(
            "/api/{*rest}",
            any(rattery_app::server_fn::axum::handle_server_fn),
        )
        .layer(middleware::from_fn(session))
        .with_state(state);

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

/// Serve the component with an ETag so `rattery --watch` can poll cheaply.
async fn serve_app(State(state): State<AppState>, request: Request) -> Response {
    if let Some(required) = state
        .require_abi_file
        .as_deref()
        .and_then(|path| std::fs::read_to_string(path).ok())
    {
        let required = required.trim().to_owned();
        let client = request
            .headers()
            .get("rattery-abi")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !client.starts_with(&required) {
            return (
                StatusCode::UPGRADE_REQUIRED,
                [("rattery-abi", required.clone())],
                format!("this app needs a client with rattery ABI {required}; yours is {client:?}"),
            )
                .into_response();
        }
    }
    let metadata = match tokio::fs::metadata(&*state.app_path).await {
        Ok(metadata) => metadata,
        Err(err) => return app_missing(&state, err),
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let etag = format!("\"{}-{modified}\"", metadata.len());
    if request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|candidate| candidate.trim() == etag))
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    match tokio::fs::read(&*state.app_path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/wasm".to_owned()),
                (header::CACHE_CONTROL, "no-cache".to_owned()),
                (header::ETAG, etag),
            ],
            bytes,
        )
            .into_response(),
        Err(err) => app_missing(&state, err),
    }
}

fn app_missing(state: &AppState, err: std::io::Error) -> Response {
    (
            StatusCode::NOT_FOUND,
            format!(
                "app component not found at {}: {err}\nbuild it with:\n    cargo build -p counter-app --target wasm32-wasip2\n",
                state.app_path.display()
            ),
        )
            .into_response()
}

/// Give every client a session cookie and expose it to server functions.
/// The rattery host keeps the cookie like a browser, so the same session
/// comes back on the next run.
async fn session(request: Request, next: Next) -> Response {
    let existing = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .find_map(|pair| pair.trim().strip_prefix("session=").map(str::to_owned))
        });
    let (id, is_new) = match existing {
        Some(id) if !id.is_empty() => (id, false),
        _ => (format!("{:032x}", rand::random::<u128>()), true),
    };
    let mut response = counter_shared::SESSION
        .scope(id.clone(), next.run(request))
        .await;
    if is_new && let Ok(value) = HeaderValue::from_str(&format!("session={id}; Path=/; HttpOnly")) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}
