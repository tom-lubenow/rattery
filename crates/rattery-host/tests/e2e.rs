//! End-to-end: build the example guests and server, run them headless.
//!
//! Needs the `wasm32-wasip2` target; skips with a message if it is missing.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use rattery_host::{App, AppStatus, HeadlessOptions, Report, Script};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn cargo() -> Command {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(workspace_root());
    cmd
}

fn wasip2_available() -> bool {
    let out = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
        .args(["--print", "target-libdir", "--target", "wasm32-wasip2"])
        .output();
    match out {
        Ok(out) if out.status.success() => {
            let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
            dir.read_dir()
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
        }
        _ => false,
    }
}

/// Builds happen once per process; cargo serialises concurrent ones anyway.
fn build(args: &[&str]) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap();
    let status = cargo().args(args).status().expect("failed to run cargo");
    assert!(status.success(), "cargo {} failed", args.join(" "));
}

fn guest(package: &str) -> PathBuf {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        build(&[
            "build",
            "-p",
            "counter-app",
            "-p",
            "spin-app",
            "--target",
            "wasm32-wasip2",
        ])
    });
    workspace_root().join(format!("target/wasm32-wasip2/debug/{package}.wasm"))
}

struct Server {
    child: Child,
    url: String,
}

impl Server {
    fn start(extra_args: &[&str]) -> Self {
        Self::start_with(&guest("counter-app"), extra_args)
    }

    fn start_serving(app: &Path) -> Self {
        Self::start_with(app, &[])
    }

    fn start_with(app: &Path, extra_args: &[&str]) -> Self {
        static BUILT: OnceLock<()> = OnceLock::new();
        BUILT.get_or_init(|| build(&["build", "-p", "counter-server"]));
        let _ = guest("counter-app");
        let mut child = Command::new(workspace_root().join("target/debug/counter-server"))
            .arg("--bind")
            .arg("127.0.0.1:0")
            .arg("--app")
            .arg(app)
            .args(extra_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to start counter-server");
        let stdout = child.stdout.take().unwrap();
        let mut url = None;
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if let Some(rest) = line.strip_prefix("counter-server listening on ") {
                url = Some(rest.trim().to_owned());
                break;
            }
        }
        // Keep draining stdout so the server never blocks on a full pipe.
        Self {
            child,
            url: url.expect("server did not report its address"),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn headless(script: &str, timeout_secs: u64) -> HeadlessOptions {
    HeadlessOptions {
        width: 80,
        height: 24,
        script: Script::parse(script).unwrap(),
        timeout: Some(Duration::from_secs(timeout_secs)),
    }
}

fn dump(report: &Report) -> String {
    let mut out = format!("status: {:?}\nstderr: {}\n", report.status, report.stderr);
    for (i, s) in report.snapshots.iter().enumerate() {
        out.push_str(&format!("--- snapshot {} ---\n{s}", i + 1));
    }
    if let Some(s) = &report.final_screen {
        out.push_str(&format!("--- final ---\n{s}"));
    }
    out
}

macro_rules! require_wasip2 {
    () => {
        if !wasip2_available() {
            eprintln!(
                "skipping: wasm32-wasip2 target not installed (rustup target add wasm32-wasip2)"
            );
            return;
        }
    };
}

#[tokio::test(flavor = "multi_thread")]
async fn counter_round_trip_and_state_on_server() {
    require_wasip2!();
    let server = Server::start(&[]);

    let report = App::from_url(format!("{}/app.wasm", server.url))
        .unwrap()
        .headless(headless(
            "sleep 1500\nkey k\nsleep 700\nkey k\nsleep 700\nsnapshot\nkey q",
            20,
        ))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(0), "{text}");
    let snap = &report.snapshots[0];
    assert!(snap.contains("served from http://127.0.0.1:"), "{text}");
    assert!(
        snap.lines
            .iter()
            .any(|l| l.trim_matches(|c| c == '│' || c == ' ') == "2"),
        "{text}"
    );
    assert!(snap.contains("3 calls"), "{text}");

    // A second session sees the count the first one left behind.
    let report = App::from_url(format!("{}/app.wasm", server.url))
        .unwrap()
        .headless(headless("sleep 1500\nsnapshot\nkey q", 20))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(0), "{text}");
    assert!(
        report.snapshots[0]
            .lines
            .iter()
            .any(|l| l.trim_matches(|c| c == '│' || c == ' ') == "2"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn file_without_origin_fails_cleanly() {
    require_wasip2!();
    let report = App::from_path(guest("counter-app"))
        .headless(headless("sleep 1500\nsnapshot\nkey q", 20))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(0), "{text}");
    assert!(report.snapshots[0].contains("loaded from a file"), "{text}");
    assert!(report.snapshots[0].contains("error"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn cross_origin_is_denied_unless_cors_permits() {
    require_wasip2!();
    let host = Server::start(&[]);
    let api_closed = Server::start(&[]);

    // The app comes from `host` but is pointed at `api_closed`: cross-origin, denied.
    let report = App::from_url(format!("{}/app.wasm", host.url))
        .unwrap()
        .origin(&api_closed.url)
        .cors(true)
        .headless(headless("sleep 1500\nsnapshot\nkey q", 20))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(0), "{text}");
    assert!(report.snapshots[0].contains("error"), "{text}");

    // An API that opts in with Access-Control-Allow-Origin is reachable.
    let api_open = Server::start(&["--cors-allow-origin", &host.url]);
    let report = App::from_url(format!("{}/app.wasm", host.url))
        .unwrap()
        .origin(&api_open.url)
        .cors(true)
        .headless(headless(
            "sleep 1500\nkey k\nsleep 700\nsnapshot\nkey q",
            20,
        ))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(0), "{text}");
    assert!(!report.snapshots[0].contains("error"), "{text}");
    assert!(report.snapshots[0].contains("2 calls"), "{text}");

    // Explicitly allowing the origin works without CORS.
    let report = App::from_url(format!("{}/app.wasm", host.url))
        .unwrap()
        .origin(&api_closed.url)
        .allow_origin(&api_closed.url)
        .headless(headless("sleep 1500\nsnapshot\nkey q", 20))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert!(!report.snapshots[0].contains("error"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_reloads_when_the_served_component_changes() {
    require_wasip2!();
    let dir = std::env::temp_dir().join(format!("rattery-watch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let served = dir.join("app.wasm");
    std::fs::copy(guest("counter-app"), &served).unwrap();
    let server = Server::start_serving(&served);

    let run = tokio::spawn(
        App::from_url(format!("{}/app.wasm", server.url))
            .unwrap()
            .watch(true)
            .headless(headless("sleep 1500\nsnapshot", 12))
            .run(),
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    // Publish a different app; the running one is replaced in place.
    std::fs::copy(guest("spin-app"), &served).unwrap();

    let report = run.await.unwrap().unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::TimedOut, "{text}");
    assert!(report.snapshots[0].contains("rattery counter"), "{text}");
    assert!(
        report
            .final_screen
            .as_ref()
            .unwrap()
            .contains("spinning forever"),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_switch_interrupts_a_spinning_app() {
    require_wasip2!();
    let report = App::from_path(guest("spin-app"))
        .headless(headless(
            "sleep 800\nkey ctrl-c\nkey ctrl-c\nkey ctrl-c",
            30,
        ))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Killed, "{text}");
    assert!(
        report.final_screen.unwrap().contains("spinning forever"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn headless_timeout_stops_a_spinning_app() {
    require_wasip2!();
    let report = App::from_path(guest("spin-app"))
        .headless(headless("", 2))
        .run()
        .await
        .unwrap();
    assert_eq!(report.status, AppStatus::TimedOut, "{}", dump(&report));
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_stops_an_app_waiting_for_input() {
    require_wasip2!();
    let report = App::from_path(guest("counter-app"))
        .headless(headless("", 2))
        .run()
        .await
        .unwrap();
    assert_eq!(report.status, AppStatus::TimedOut, "{}", dump(&report));
}
