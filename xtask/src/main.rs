//! `cargo xtask dev`: the rattery dev loop.
//!
//! Builds the app component and the server, runs the server, then watches the
//! sources. On a change it rebuilds the component (the server serves the new
//! file immediately and `rattery --watch` reloads it in place) and rebuilds
//! and restarts the server if its binary changed.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cargo xtask")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Build, serve, watch, rebuild. Run `rattery --watch <url>` next to it.
    Dev {
        /// The app package to build for wasm32-wasip2.
        #[arg(long, default_value = "counter-app")]
        app: String,
        /// The server package to build and run.
        #[arg(long, default_value = "counter-server")]
        server: String,
        /// Address the server listens on.
        #[arg(long, default_value = "127.0.0.1:3000")]
        bind: String,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Task::Dev { app, server, bind } => dev(&app, &server, &bind),
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .unwrap()
}

fn cargo(args: &[&str]) -> Result<bool> {
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(args)
        .current_dir(root())
        .status()
        .context("failed to run cargo")?;
    Ok(status.success())
}

fn dev(app: &str, server: &str, bind: &str) -> Result<()> {
    let root = root();
    let app_wasm = root.join(format!(
        "target/wasm32-wasip2/debug/{}.wasm",
        app.replace('-', "_")
    ));
    let server_bin = root.join(format!("target/debug/{server}"));
    let watched = [root.join("crates"), root.join("examples"), root.join("wit")];

    println!("xtask: building {app} (wasm32-wasip2) and {server}");
    if !cargo(&["build", "-p", app, "--target", "wasm32-wasip2"])? {
        bail!("initial build of {app} failed");
    }
    if !cargo(&["build", "-p", server])? {
        bail!("initial build of {server} failed");
    }

    let mut child = start_server(&server_bin, bind, &app_wasm)?;
    let mut server_stamp = mtime(&server_bin);
    let mut sources_stamp = newest_mtime(&watched);
    println!();
    println!("xtask: watching for changes. In another terminal:");
    println!("    cargo run -p rattery-host -- --watch http://{bind}/app.wasm");
    println!();

    loop {
        thread::sleep(Duration::from_millis(500));
        let now = newest_mtime(&watched);
        if now <= sources_stamp {
            continue;
        }
        sources_stamp = now;
        // Let the editor finish writing before we build.
        thread::sleep(Duration::from_millis(150));
        println!("xtask: change detected, rebuilding {app}");
        if cargo(&["build", "-p", app, "--target", "wasm32-wasip2"])? {
            println!("xtask: {app} rebuilt; a watching rattery reloads it now");
        } else {
            println!("xtask: {app} failed to build; serving the previous component");
        }
        if cargo(&["build", "-p", server])? {
            let stamp = mtime(&server_bin);
            if stamp != server_stamp {
                server_stamp = stamp;
                println!("xtask: restarting {server}");
                let _ = child.kill();
                let _ = child.wait();
                child = start_server(&server_bin, bind, &app_wasm)?;
            }
        } else {
            println!("xtask: {server} failed to build; keeping the running one");
        }
    }
}

fn start_server(bin: &Path, bind: &str, app_wasm: &Path) -> Result<Child> {
    Command::new(bin)
        .arg("--bind")
        .arg(bind)
        .arg("--app")
        .arg(app_wasm)
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start {}", bin.display()))
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn newest_mtime(roots: &[PathBuf]) -> Option<SystemTime> {
    let mut newest = None;
    for root in roots {
        walk(root, &mut newest);
    }
    newest
}

fn walk(dir: &Path, newest: &mut Option<SystemTime>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|n| n == "target" || n == ".direnv")
            {
                continue;
            }
            walk(&path, newest);
        } else if path
            .extension()
            .is_some_and(|ext| ext == "rs" || ext == "toml" || ext == "wit")
            && let Some(stamp) = mtime(&path)
            && newest.is_none_or(|current| stamp > current)
        {
            *newest = Some(stamp);
        }
    }
}
