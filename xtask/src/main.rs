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
    /// Build the bench app and the host in release mode and run the
    /// rendering benchmark matrix headless.
    Bench {
        /// Frames per run.
        #[arg(long, default_value_t = 200)]
        frames: usize,
    },
    /// The hover benchmark: a tiled layout redrawn on every pointer movement,
    /// natively and as a component (release, and a true opt-level 0 build),
    /// with and without input coalescing. See docs/perf.md.
    Hover {
        /// Pointer movements per run.
        #[arg(long, default_value_t = 400)]
        steps: usize,
        /// Milliseconds between movements.
        #[arg(long, default_value_t = 5)]
        interval: u64,
    },
    /// Native ratatui against rattery on the interactive workloads
    /// (animation, clicks, drags, hover): frame time, input-to-frame
    /// latency, animation cadence jitter; on an in-memory backend and on a
    /// real pseudo-terminal. See docs/perf.md.
    Perf {
        /// Input events per interactive run.
        #[arg(long, default_value_t = 200)]
        events: usize,
        /// Milliseconds between input events.
        #[arg(long, default_value_t = 5)]
        interval: u64,
        /// Frames per animation run.
        #[arg(long, default_value_t = 300)]
        frames: usize,
        /// Layout renders per frame, to stand in for a heavier UI.
        #[arg(long, default_value_t = 1)]
        work: usize,
        /// Skip the pseudo-terminal runs (they need `script` from util-linux).
        #[arg(long)]
        no_pty: bool,
    },
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
        Task::Bench { frames } => bench(frames),
        Task::Hover { steps, interval } => hover(steps, interval),
        Task::Perf {
            events,
            interval,
            frames,
            work,
            no_pty,
        } => perf(events, interval, frames, work, !no_pty),
    }
}

fn bench(frames: usize) -> Result<()> {
    let root = root();
    if !cargo(&[
        "build",
        "-p",
        "bench-app",
        "--target",
        "wasm32-wasip2",
        "--release",
    ])? {
        bail!("bench-app failed to build");
    }
    if !cargo(&["build", "-p", "rattery-cli", "--release"])? {
        bail!("rattery-cli failed to build");
    }
    let host = root.join("target/release/rattery");
    let app = root.join("target/wasm32-wasip2/release/bench-app.wasm");
    println!();
    println!(
        "{:<8} {:<8} {:>10} {:>9} {:>9} {:>9} {:>7}   host draw avg / flush avg",
        "mode", "size", "cells", "avg ms", "p50 ms", "p95 ms", "fps"
    );
    for mode in ["full", "sparse", "text"] {
        for size in ["80x24", "200x50"] {
            let output = Command::new(&host)
                .arg("--headless")
                .arg(size)
                .arg("--timeout")
                .arg("120")
                .arg("--stats")
                .arg("--no-cookies")
                .arg("--location")
                .arg(format!(
                    "bench://local/bench_app.wasm?frames={frames}&mode={mode}"
                ))
                .arg(&app)
                .output()
                .context("failed to run rattery")?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let Some(line) = stdout.lines().find(|l| l.starts_with("bench ")) else {
                println!("{mode:<8} {size:<8} failed: {stderr}");
                continue;
            };
            let get = |key: &str| {
                line.split_whitespace()
                    .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
                    .unwrap_or("?")
                    .to_owned()
            };
            let host_line = stderr
                .lines()
                .find(|l| l.trim_start().starts_with("draws "))
                .map(|l| {
                    // "draws N (C cells, Xms avg on host), flushes M (Yms avg), events E"
                    let field = |marker: &str| {
                        l.split_once(marker)
                            .and_then(|(before, _)| before.rsplit([' ', '(']).next())
                            .unwrap_or("?")
                            .to_owned()
                    };
                    format!("{} / {}", field(" avg on host"), field(" avg)"))
                })
                .unwrap_or_else(|| "?".into());
            println!(
                "{mode:<8} {size:<8} {:>10} {:>9} {:>9} {:>9} {:>7}   {host_line}",
                get("screen_cells"),
                get("avg_ms"),
                get("p50_ms"),
                get("p95_ms"),
                get("fps")
            );
        }
    }
    // Request latency through wasi:http, against the example server.
    if cargo(&["build", "-p", "counter-server", "--release"])? {
        let mut server = Command::new(root.join("target/release/counter-server"))
            .args(["--bind", "127.0.0.1:0", "--app", "/dev/null"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start counter-server")?;
        let mut origin = None;
        if let Some(stdout) = server.stdout.take() {
            use std::io::BufRead;
            let mut lines = std::io::BufReader::new(stdout).lines();
            for line in lines.by_ref() {
                let line = line?;
                if let Some(rest) = line.strip_prefix("counter-server listening on ") {
                    origin = Some(rest.trim().to_owned());
                    break;
                }
            }
            thread::spawn(move || for _ in lines.by_ref() {});
        }
        if let Some(origin) = origin {
            let native = native_http_latency(&origin, frames);
            let output = Command::new(&host)
                .args([
                    "--headless",
                    "80x24",
                    "--timeout",
                    "120",
                    "--stats",
                    "--no-cookies",
                ])
                .arg("--origin")
                .arg(&origin)
                .arg("--location")
                .arg(format!(
                    "bench://local/bench_app.wasm?frames={frames}&mode=http"
                ))
                .arg(&app)
                .output()
                .context("failed to run rattery")?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = stdout.lines().find(|l| l.starts_with("bench mode=http")) {
                let get = |key: &str| {
                    line.split_whitespace()
                        .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
                        .unwrap_or("?")
                        .to_owned()
                };
                println!();
                println!(
                    "http GET via wasi:http: avg {} ms, p50 {} ms, p95 {} ms  (native std TcpStream: avg {:.3} ms)",
                    get("avg_ms"),
                    get("p50_ms"),
                    get("p95_ms"),
                    native
                );
            } else {
                println!(
                    "http bench failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        let _ = server.kill();
        let _ = server.wait();
    }

    let timings = Command::new(&host)
        .args([
            "--headless",
            "80x24",
            "--timeout",
            "60",
            "--stats",
            "--no-cookies",
        ])
        .arg("--location")
        .arg("bench://local/bench_app.wasm?frames=1&mode=text")
        .arg(&app)
        .output()
        .context("failed to run rattery")?;
    if let Some(line) = String::from_utf8_lossy(&timings.stderr)
        .lines()
        .find(|l| l.starts_with("rattery stats:"))
    {
        println!();
        println!(
            "startup (warm cache): {}",
            line.trim_start_matches("rattery stats: ")
        );
    }
    let size = std::fs::metadata(&app).map(|m| m.len()).unwrap_or(0);
    println!("component size (release): {} KB", size / 1024);
    Ok(())
}

fn hover(steps: usize, interval: u64) -> Result<()> {
    let root = root();
    for args in [
        &[
            "build",
            "-p",
            "bench-app",
            "--target",
            "wasm32-wasip2",
            "--release",
        ][..],
        &[
            "build",
            "-p",
            "bench-app",
            "--target",
            "wasm32-wasip2",
            "--target-dir",
            "target/opt0",
            "--config",
            "profile.dev.package.\"*\".opt-level=0",
        ],
        &["build", "-p", "rattery-cli", "-p", "bench-app", "--release"],
    ] {
        if !cargo(args)? {
            bail!("build failed: cargo {}", args.join(" "));
        }
    }
    let script = root.join("target/hover-script.txt");
    std::fs::write(
        &script,
        format!("sleep 1500\nsweep {steps} {interval}\nkey q\n"),
    )?;
    println!();
    println!(
        "{:<34} {:>7} {:>7} {:>9} {:>9} {:>10}",
        "run (200x50)", "events", "frames", "frame ms", "host µs", "lag at end"
    );
    for work in [1, 10] {
        let output = Command::new(root.join("target/release/hover-native"))
            .args([steps.to_string(), "200x50".into(), work.to_string()])
            .output()
            .context("failed to run hover-native")?;
        let line = String::from_utf8_lossy(&output.stdout);
        println!(
            "{:<34} {:>7} {:>7} {:>9} {:>9} {:>10}",
            format!("native work={work}"),
            "-",
            steps,
            field(&line, "avg_ms"),
            "-",
            "-"
        );
    }
    let host = root.join("target/release/rattery");
    let builds = [
        (
            "release",
            root.join("target/wasm32-wasip2/release/bench-app.wasm"),
        ),
        (
            "opt-level 0",
            root.join("target/opt0/wasm32-wasip2/debug/bench-app.wasm"),
        ),
    ];
    for (label, app) in &builds {
        for work in [1, 10] {
            for coalesce in [true, false] {
                let mut cmd = Command::new(&host);
                cmd.args([
                    "--headless",
                    "200x50",
                    "--timeout",
                    "300",
                    "--stats",
                    "--no-cookies",
                ])
                .arg("--script")
                .arg(&script)
                .arg("--location")
                .arg(format!("bench://local/app.wasm?mode=hover&work={work}"));
                if !coalesce {
                    cmd.arg("--no-coalesce");
                }
                let output = cmd.arg(app).output().context("failed to run rattery")?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let name = format!(
                    "{label} work={work} {}",
                    if coalesce { "coalesced" } else { "every event" }
                );
                let Some(line) = stdout.lines().find(|l| l.starts_with("bench ")) else {
                    println!("{name:<34} failed: {}", stderr.lines().last().unwrap_or(""));
                    continue;
                };
                let draws = stderr
                    .lines()
                    .find(|l| l.trim_start().starts_with("draws "))
                    .unwrap_or("");
                let host_avg = draws
                    .split_once(" avg on host")
                    .and_then(|(before, _)| before.rsplit([' ', '(']).next())
                    .unwrap_or("?");
                let lag = stderr
                    .lines()
                    .find_map(|l| l.trim_start().strip_prefix("lag at end: "))
                    .and_then(|l| l.split_whitespace().next())
                    .unwrap_or("?");
                println!(
                    "{name:<34} {:>7} {:>7} {:>9} {:>9} {:>10}",
                    field(line, "mouse_events"),
                    field(line, "frames"),
                    field(line, "avg_ms"),
                    host_avg,
                    lag
                );
            }
        }
    }
    Ok(())
}

/// One row of the perf table.
struct Row {
    name: String,
    frames: String,
    frame_avg: String,
    frame_p95: String,
    latency_avg: String,
    latency_p95: String,
    extra: String,
}

impl Row {
    fn print(&self) {
        println!(
            "{:<32} {:>6} {:>9} {:>9} {:>9} {:>9}   {}",
            self.name,
            self.frames,
            self.frame_avg,
            self.frame_p95,
            self.latency_avg,
            self.latency_p95,
            self.extra
        );
    }
}

fn perf(events: usize, interval: u64, frames: usize, work: usize, pty: bool) -> Result<()> {
    let root = root();
    for args in [
        &[
            "build",
            "-p",
            "bench-app",
            "--target",
            "wasm32-wasip2",
            "--release",
        ][..],
        &["build", "-p", "rattery-cli", "-p", "bench-app", "--release"],
    ] {
        if !cargo(args)? {
            bail!("build failed: cargo {}", args.join(" "));
        }
    }
    let pty = pty
        && Command::new("script")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    let native = root.join("target/release/bench-native");
    let host = root.join("target/release/rattery");
    let app = root.join("target/wasm32-wasip2/release/bench-app.wasm");
    let scratch = root.join("target/perf");
    std::fs::create_dir_all(&scratch)?;
    let size = "200x50";

    let heading = format!("workload / runner ({size})");
    println!();
    println!(
        "{heading:<32} {:>6} {:>9} {:>9} {:>9} {:>9}   animation: interval p95, late frames",
        "frames", "frame avg", "frame p95", "input avg", "input p95"
    );
    let runners: Vec<(&str, bool, bool)> = if pty {
        vec![
            ("native, in memory", false, false),
            ("native, terminal", false, true),
            ("rattery, in memory", true, false),
            ("rattery, terminal", true, true),
        ]
    } else {
        vec![
            ("native, in memory", false, false),
            ("rattery, in memory", true, false),
        ]
    };
    let workloads: Vec<(&str, Option<u64>)> = vec![
        ("anim", None),
        ("anim", Some(16)),
        ("click", None),
        ("drag", None),
        ("hover", None),
    ];
    for (mode, cadence) in &workloads {
        let label = match cadence {
            Some(ms) => format!("{mode} @ {ms} ms"),
            None => mode.to_string(),
        };
        println!("{label}");
        let script = scratch.join(format!("{mode}.txt"));
        if *mode != "anim" {
            let status = Command::new(&native)
                .args(["--mode", mode, "--size", size, "--events"])
                .arg(events.to_string())
                .arg("--interval")
                .arg(interval.to_string())
                .arg("--emit-script")
                .arg(&script)
                .status()?;
            if !status.success() {
                bail!("bench-native could not write the script");
            }
        }
        for (runner, rattery, terminal) in &runners {
            let out = scratch.join("out.txt");
            let _ = std::fs::remove_file(&out);
            let mut cmd: Vec<String> = Vec::new();
            if *rattery {
                cmd.push(host.display().to_string());
                if !*terminal {
                    cmd.extend(["--headless".into(), size.into()]);
                }
                cmd.extend([
                    "--stats".into(),
                    "--stats-file".into(),
                    out.display().to_string(),
                    "--timeout".into(),
                    "120".into(),
                    "--no-cookies".into(),
                ]);
                if *mode != "anim" {
                    cmd.extend(["--script".into(), script.display().to_string()]);
                }
                let query = match cadence {
                    Some(ms) => format!("mode=anim&frames={frames}&cadence={ms}&work={work}"),
                    None if *mode == "anim" => format!("mode=anim&frames={frames}&work={work}"),
                    None => format!("mode={mode}&work={work}"),
                };
                cmd.extend([
                    "--location".into(),
                    format!("bench://local/app.wasm?{query}"),
                    app.display().to_string(),
                ]);
            } else {
                cmd.push(native.display().to_string());
                cmd.extend([
                    "--mode".into(),
                    mode.to_string(),
                    "--size".into(),
                    size.into(),
                ]);
                cmd.extend(["--events".into(), events.to_string()]);
                cmd.extend(["--interval".into(), interval.to_string()]);
                cmd.extend(["--frames".into(), frames.to_string()]);
                cmd.extend(["--work".into(), work.to_string()]);
                if let Some(ms) = cadence {
                    cmd.extend(["--cadence".into(), ms.to_string()]);
                }
                cmd.extend(["--out".into(), out.display().to_string()]);
                if *terminal {
                    cmd.extend(["--backend".into(), "terminal".into()]);
                }
            }
            let status = if *terminal {
                // A pseudo-terminal of the benchmark size; its output is
                // consumed and discarded, as a terminal that keeps up would.
                let (cols, rows) = size.split_once('x').unwrap();
                let shell = format!(
                    "stty cols {cols} rows {rows}; {}",
                    cmd.iter()
                        .map(|a| shell_quote(a))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                // stdin stays open: at end-of-file `script` would send a
                // Ctrl-D into the pty, an input event the app never draws
                // for, which would be charged to the first real frame.
                let mut child = Command::new("script")
                    .args(["-qec", &shell, "/dev/null"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?;
                let stdin = child.stdin.take();
                let status = child.wait()?;
                drop(stdin);
                status
            } else {
                Command::new(&cmd[0])
                    .args(&cmd[1..])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()?
            };
            let text = std::fs::read_to_string(&out).unwrap_or_default();
            let line = text
                .lines()
                .find(|l| l.starts_with("bench ") || l.starts_with("native "))
                .unwrap_or("");
            if !status.success() || line.is_empty() {
                println!("  {runner:<30} failed");
                continue;
            }
            let (latency_avg, latency_p95) = if *rattery {
                let host_line = text
                    .lines()
                    .find_map(|l| l.trim_start().strip_prefix("input to frame: "))
                    .unwrap_or("");
                let after = |key: &str| {
                    host_line
                        .split_once(&format!("{key} "))
                        .and_then(|(_, rest)| rest.split_whitespace().next())
                        .map(|v| v.trim_end_matches("ms").to_owned())
                        .unwrap_or_else(|| "-".into())
                };
                (after("avg"), after("p95"))
            } else {
                (field(line, "latency_avg_ms"), field(line, "latency_p95_ms"))
            };
            let extra = if *mode == "anim" && cadence.is_some() {
                format!(
                    "interval p95 {} ms, late {}",
                    field(line, "interval_p95_ms"),
                    field(line, "late")
                )
            } else {
                String::new()
            };
            let anim = *mode == "anim";
            Row {
                name: format!("  {runner}"),
                frames: field(line, "frames"),
                frame_avg: field(line, "frame_avg_ms"),
                frame_p95: field(line, "frame_p95_ms"),
                latency_avg: if anim { "-".into() } else { latency_avg },
                latency_p95: if anim { "-".into() } else { latency_p95 },
                extra,
            }
            .print();
        }
    }
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn field(line: &str, key: &str) -> String {
    line.split_whitespace()
        .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
        .unwrap_or("?")
        .to_owned()
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
    let app_wasm = root.join(format!("target/wasm32-wasip2/debug/{app}.wasm"));
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
    println!("    cargo rattery --watch http://{bind}/app.wasm");
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

/// Sequential HTTP/1.1 GETs with a fresh connection each, like the guest does.
fn native_http_latency(origin: &str, count: usize) -> f64 {
    use std::io::{Read, Write};
    let addr = origin.trim_start_matches("http://");
    let started = std::time::Instant::now();
    for _ in 0..count {
        let Ok(mut stream) = std::net::TcpStream::connect(addr) else {
            return f64::NAN;
        };
        let _ = stream.write_all(
            format!("GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        );
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf);
    }
    started.elapsed().as_secs_f64() * 1000.0 / count.max(1) as f64
}
