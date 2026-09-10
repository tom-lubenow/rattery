//! End-to-end: build the example guests and server, run them headless.
//!
//! Needs the `wasm32-wasip2` target; skips with a message if it is missing.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use rattery::{
    App, AppStatus, CookiePolicy, HeadlessOptions, LogLevel, Phase, ReloadPolicy, Report, Script,
    StoragePolicy,
};

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
            "-p",
            "bench-app",
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
        let mut lines = BufReader::new(stdout).lines();
        let mut url = None;
        for line in lines.by_ref() {
            let line = line.unwrap();
            if let Some(rest) = line.strip_prefix("counter-server listening on ") {
                url = Some(rest.trim().to_owned());
                break;
            }
        }
        // Keep draining stdout: dropping the pipe would make the server's next
        // println! fail with a broken pipe and kill it.
        std::thread::spawn(move || for _ in lines.by_ref() {});
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

/// The big number in the counter box: the first box segment that is only digits.
fn count_on(screen: &rattery::Screen) -> Option<String> {
    screen
        .lines
        .iter()
        .flat_map(|l| l.split('│'))
        .map(str::trim)
        .find(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit() || c == '-'))
        .map(str::to_owned)
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
    let jar = std::env::temp_dir().join(format!("rattery-e2e-jar-{}.json", std::process::id()));

    let report = App::from_url(format!("{}/app.wasm", server.url))
        .unwrap()
        .cookies(CookiePolicy::File(jar.clone()))
        .headless(headless(
            "sleep 2500\nkey k\nsleep 700\nkey k\nkey w\nkey u\nsleep 1000\nsnapshot\nkey q",
            20,
        ))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(0), "{text}");
    let snap = &report.snapshots[0];
    assert!(snap.contains("served from http://127.0.0.1:"), "{text}");
    assert_eq!(count_on(snap).as_deref(), Some("2"), "{text}");
    assert!(snap.contains("3 calls"), "{text}");
    assert!(snap.contains("session "), "{text}");
    // The streaming server function has been pushing a line every 500ms.
    let ticks = snap.lines.iter().filter(|l| l.contains("tick ")).count();
    assert!(
        ticks >= 3,
        "expected streamed feed lines, got {ticks}\n{text}"
    );
    assert!(
        snap.contains("count 2  up"),
        "feed should reflect the increments\n{text}"
    );
    assert!(
        snap.contains("ws: pong 1"),
        "websocket reply should arrive\n{text}"
    );
    assert!(
        snap.contains("upload: note: count was") && snap.contains("upload: report: report.txt 24B"),
        "multipart upload should round-trip\n{text}"
    );

    // A second run with the same cookie jar is the same session: the count
    // it sees is the one the first run left behind.
    let report = App::from_url(format!("{}/app.wasm", server.url))
        .unwrap()
        .cookies(CookiePolicy::File(jar.clone()))
        .headless(headless("sleep 2500\nsnapshot\nkey q", 20))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(0), "{text}");
    assert_eq!(
        count_on(&report.snapshots[0]).as_deref(),
        Some("2"),
        "{text}"
    );

    // A private-window run gets a fresh session and starts from zero.
    let report = App::from_url(format!("{}/app.wasm", server.url))
        .unwrap()
        .cookies(CookiePolicy::Ephemeral)
        .headless(headless("sleep 2500\nsnapshot\nkey q", 20))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(
        count_on(&report.snapshots[0]).as_deref(),
        Some("0"),
        "{text}"
    );
    let _ = std::fs::remove_file(&jar);
}

#[tokio::test(flavor = "multi_thread")]
async fn file_without_origin_fails_cleanly() {
    require_wasip2!();
    let report = App::from_path(guest("counter-app"))
        .headless(headless("sleep 2500\nsnapshot\nkey q", 20))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(0), "{text}");
    assert!(report.snapshots[0].contains("loaded from a file"), "{text}");
    assert!(report.snapshots[0].contains("error"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn cross_origin_is_denied_unless_allow_listed() {
    require_wasip2!();
    let host = Server::start(&[]);
    let api = Server::start(&[]);

    // The app comes from `host` but is pointed at `api`: cross-origin, denied.
    let denied = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let report = App::from_url(format!("{}/app.wasm", host.url))
        .unwrap()
        .origin(&api.url)
        .cookies(CookiePolicy::Ephemeral)
        .on_phase({
            let denied = denied.clone();
            move |phase| {
                if let rattery::Phase::RequestDenied { url, reason } = phase {
                    denied.lock().unwrap().push(format!("{url}: {reason}"));
                }
            }
        })
        .headless(headless("sleep 2500\nsnapshot\nkey q", 20))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(0), "{text}");
    assert!(report.snapshots[0].contains("error"), "{text}");
    assert!(
        !denied.lock().unwrap().is_empty(),
        "the embedder is told about denials"
    );

    // Explicitly allowing the origin works.
    let report = App::from_url(format!("{}/app.wasm", host.url))
        .unwrap()
        .origin(&api.url)
        .allow_origin(&api.url)
        .cookies(CookiePolicy::Ephemeral)
        .headless(headless(
            "sleep 2500\nkey k\nsleep 700\nsnapshot\nkey q",
            20,
        ))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert!(!report.snapshots[0].contains("error"), "{text}");
    assert!(report.snapshots[0].contains("2 calls"), "{text}");
}

/// A policy that refuses one route and stamps every other request.
struct RoutePolicy;

impl rattery::RequestPolicy for RoutePolicy {
    fn on_request<'a>(
        &'a self,
        request: &'a mut http::request::Parts,
        info: &'a rattery::RequestInfo,
    ) -> futures::future::BoxFuture<'a, Result<rattery::PolicyGuard, rattery::PolicyError>> {
        Box::pin(async move {
            assert!(!info.cross_origin);
            if request.uri.path().contains("adjust_count") {
                return Err(rattery::PolicyError::new("adjusting is not allowed here"));
            }
            request
                .headers
                .insert("x-shim", http::HeaderValue::from_static("1"));
            Ok(None)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn request_policy_can_refuse_routes() {
    require_wasip2!();
    let server = Server::start(&[]);
    let denied = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let report = App::from_url(format!("{}/app.wasm", server.url))
        .unwrap()
        .cookies(CookiePolicy::Ephemeral)
        .request_policy(std::sync::Arc::new(RoutePolicy))
        .on_phase({
            let denied = denied.clone();
            move |phase| {
                if let rattery::Phase::RequestDenied { reason, .. } = phase {
                    denied.lock().unwrap().push(reason);
                }
            }
        })
        .headless(headless(
            "sleep 2500\nkey k\nsleep 700\nsnapshot\nkey q",
            20,
        ))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(0), "{text}");
    // The first snapshot succeeded (policy allowed it), the increment did not.
    assert_eq!(
        count_on(&report.snapshots[0]).as_deref(),
        Some("0"),
        "{text}"
    );
    assert!(report.snapshots[0].contains("error"), "{text}");
    assert_eq!(
        denied.lock().unwrap().as_slice(),
        ["adjusting is not allowed here"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn escape_sequences_from_the_guest_are_contained() {
    require_wasip2!();
    let report = App::from_path(guest("bench-app"))
        .location("bench://local/app.wasm?mode=evil")
        .cookies(CookiePolicy::Ephemeral)
        .headless(headless("sleep 1500\nsnapshot", 10))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(1), "{text}");
    // The app exits at once, so the screen it left behind is the evidence.
    let screen = report.final_screen.as_ref().unwrap();
    for (y, expected) in ["\u{FFFD}", "\u{FFFD}", "\u{FFFD}", "\u{FFFD}", "é"]
        .iter()
        .enumerate()
    {
        let row = &screen.lines[y];
        assert!(row.starts_with(expected), "row {y} was {row:?}\n{text}");
        assert!(
            !row.contains('\u{1b}') && !row.contains('\u{7}') && !row.contains('\u{9b}'),
            "{row:?}"
        );
    }
    assert_eq!(
        report.stats.cells_rejected, 5,
        "four bad symbols and one off-screen cell\n{text}"
    );
    // Raw output still carries the bytes; the sanitiser is for printing.
    assert!(report.stdout.contains('\u{1b}'));
    assert!(!rattery::sanitize::text(&report.stdout).contains('\u{1b}'));
    assert!(
        rattery::sanitize::text(&report.stderr).contains("\\u{1b}[31mred"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cpu_budget_stops_a_spinning_app() {
    require_wasip2!();
    let report = App::from_path(guest("spin-app"))
        .cookies(CookiePolicy::Ephemeral)
        .limits(rattery::Limits {
            cpu_time: Some(Duration::from_millis(500)),
            ..Default::default()
        })
        .headless(headless("", 10))
        .run()
        .await
        .unwrap();
    assert!(
        matches!(report.status, AppStatus::LimitExceeded(ref what) if what.contains("CPU")),
        "{}",
        dump(&report)
    );
    assert!(
        report.timings.total < Duration::from_secs(5),
        "{:?}",
        report.timings
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_limit_is_enforced() {
    require_wasip2!();
    let result = App::from_path(guest("counter-app"))
        .cookies(CookiePolicy::Ephemeral)
        .limits(rattery::Limits {
            memory_bytes: 1 << 16,
            ..Default::default()
        })
        .headless(headless("sleep 500", 5))
        .run()
        .await;
    match result {
        Ok(report) => assert!(
            !matches!(report.status, AppStatus::Exited(0) | AppStatus::TimedOut),
            "{}",
            dump(&report)
        ),
        Err(err) => {
            let text = format!("{err:#}");
            assert!(
                text.to_lowercase().contains("memory") || text.contains("limit"),
                "{text}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_reloads_when_the_served_component_changes() {
    require_wasip2!();
    let dir = std::env::temp_dir().join(format!("rattery-watch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let served = dir.join("app.wasm");
    std::fs::copy(guest("counter-app"), &served).unwrap();
    let server = Server::start_serving(&served);

    // The swap below is on a wall-clock timer, so make sure neither component
    // needs a cold compile inside the timed window: run each once (cheap when
    // wasmtime's cache is warm, a few seconds when it is not).
    for package in ["counter-app", "spin-app"] {
        App::from_path(guest(package))
            .cookies(CookiePolicy::Ephemeral)
            .headless(headless("", 1))
            .run()
            .await
            .unwrap();
    }

    // Deferred policy: the app hears about the update and gets three seconds
    // to act on it; the counter app only shows a banner, so the host reloads
    // it when the deadline passes.
    let phases = std::sync::Arc::new(Mutex::new(Vec::new()));
    let seen = phases.clone();
    let run = tokio::spawn(
        App::from_url(format!("{}/app.wasm", server.url))
            .unwrap()
            .watch(true)
            .reload_policy(ReloadPolicy::Deferred {
                grace: Duration::from_secs(3),
            })
            .on_phase(move |phase| {
                if matches!(phase, Phase::UpdateAvailable { .. } | Phase::Reloading) {
                    seen.lock().unwrap().push(phase);
                }
            })
            .cookies(CookiePolicy::Ephemeral)
            .headless(headless("sleep 2500\nsnapshot\nsleep 4000\nsnapshot", 16))
            .run(),
    );
    tokio::time::sleep(Duration::from_secs(4)).await;
    // Publish a different app; the running one is told, then replaced.
    std::fs::copy(guest("spin-app"), &served).unwrap();

    let report = run.await.unwrap().unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::TimedOut, "{text}");
    assert!(report.snapshots[0].contains("rattery counter"), "{text}");
    let banner = &report.snapshots[1];
    assert!(
        banner.contains("rattery counter") && banner.contains("available: R reloads, forced in"),
        "the app should still be running with the banner up\n{text}"
    );
    let phases = phases.lock().unwrap();
    assert!(
        matches!(phases.first(), Some(Phase::UpdateAvailable { version: Some(v) }) if !v.is_empty()),
        "{phases:?}"
    );
    assert!(phases.contains(&Phase::Reloading), "{phases:?}");
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
async fn location_carries_query_parameters() {
    require_wasip2!();
    let server = Server::start(&[]);
    let report = App::from_url(format!("{}/app.wasm?title=Hello+from+the+URL", server.url))
        .unwrap()
        .cookies(CookiePolicy::Ephemeral)
        .headless(headless("sleep 2500\nsnapshot\nkey q", 20))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert!(report.snapshots[0].contains("Hello+from+the+URL"), "{text}");
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

type Logs = std::sync::Arc<Mutex<Vec<(LogLevel, String, String)>>>;

/// Collect the app's log records through the phase hook.
fn log_sink() -> (Logs, impl Fn(Phase) + Send + Sync + 'static) {
    let logs = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = logs.clone();
    (logs, move |phase| {
        if let Phase::Log {
            level,
            target,
            message,
        } = phase
        {
            sink.lock().unwrap().push((level, target, message));
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_is_per_origin_and_persists_across_runs() {
    require_wasip2!();
    let server = Server::start(&[]);
    let dir = std::env::temp_dir().join(format!("rattery-e2e-storage-{}", std::process::id()));
    let launch = |snap: &rattery::Screen| -> Option<u64> {
        let line = snap.lines.iter().find(|l| l.contains("launch "))?;
        let rest = line.split("launch ").nth(1)?;
        rest.split_whitespace().next()?.parse().ok()
    };

    for expected in [1, 2] {
        let (logs, sink) = log_sink();
        let report = App::from_url(format!("{}/app.wasm", server.url))
            .unwrap()
            .cookies(CookiePolicy::Ephemeral)
            .storage(StoragePolicy::Dir(dir.clone()))
            .on_phase(sink)
            .headless(headless("sleep 2500\nsnapshot\nkey q", 20))
            .run()
            .await
            .unwrap();
        let text = dump(&report);
        assert_eq!(report.status, AppStatus::Exited(0), "{text}");
        assert_eq!(launch(&report.snapshots[0]), Some(expected), "{text}");
        let logs = logs.lock().unwrap();
        assert!(
            logs.iter()
                .any(|(level, target, message)| *level == LogLevel::Info
                    && target == "counter_app::app"
                    && *message == format!("launch {expected} from Some({:?})", server.url)),
            "{logs:?}\n{text}"
        );
        assert!(
            logs.iter().any(|(_, _, m)| m.starts_with("count is ")),
            "{logs:?}"
        );
        assert_eq!(report.stats.logs, logs.len() as u64, "{text}");
        assert_eq!(report.stats.logs_dropped, 0, "{text}");
    }

    // Ephemeral storage starts empty every run; a different origin is a
    // different store even under the same directory.
    let report = App::from_url(format!("{}/app.wasm", server.url))
        .unwrap()
        .cookies(CookiePolicy::Ephemeral)
        .storage(StoragePolicy::Ephemeral)
        .headless(headless("sleep 2500\nsnapshot\nkey q", 20))
        .run()
        .await
        .unwrap();
    assert_eq!(launch(&report.snapshots[0]), Some(1), "{}", dump(&report));
    let report = App::from_path(guest("counter-app"))
        .origin(&server.url)
        .cookies(CookiePolicy::Ephemeral)
        .storage(StoragePolicy::Dir(dir.clone()))
        .headless(headless("sleep 2500\nsnapshot\nkey q", 20))
        .run()
        .await
        .unwrap();
    assert_eq!(
        launch(&report.snapshots[0]),
        Some(3),
        "same origin, same store\n{}",
        dump(&report)
    );
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    let files: Vec<_> = files
        .into_iter()
        .filter(|f| f.to_string_lossy().ends_with(".json"))
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_quotas_and_log_limits_hold() {
    require_wasip2!();
    let (logs, sink) = log_sink();
    let limits = rattery::Limits {
        storage_bytes: 4096,
        storage_entries: 64,
        logs_per_second: 100,
        ..rattery::Limits::default()
    };
    let run = |storage: StoragePolicy| {
        App::from_path(guest("bench-app"))
            .origin("http://storage.test")
            .location("http://storage.test/app.wasm?mode=storage")
            .cookies(CookiePolicy::Ephemeral)
            .storage(storage)
            .limits(limits.clone())
    };
    let report = run(StoragePolicy::Ephemeral)
        .on_phase(sink)
        .headless(headless("sleep 3000", 10))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(0), "{text}");
    assert!(
        report.stdout.contains("storage runs=1 used=5 quota=4096 big=Err(QuotaExceeded) many=Some(Err(QuotaExceeded)) keys=64"),
        "{text}"
    );
    let logs = std::mem::take(&mut *logs.lock().unwrap());
    let warn = logs
        .iter()
        .find(|(level, _, _)| *level == LogLevel::Warn)
        .unwrap();
    assert_eq!(warn.1, "bench_app::bench");
    assert!(
        !warn.2.contains('\u{1b}') && warn.2.contains("\\u{1b}]0;pwned"),
        "{warn:?}"
    );
    assert_eq!(logs.len(), 100, "rate limit\n{text}");
    assert_eq!(report.stats.logs_dropped, 2901, "{text}");

    // Disabled storage refuses writes, so the app's `?` fails it.
    let report = run(StoragePolicy::Disabled)
        .headless(headless("sleep 3000", 10))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(1), "{text}");
    assert!(report.stderr.contains("storage is disabled"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn precompiled_components_skip_compilation() {
    require_wasip2!();
    let wasm = std::fs::read(guest("bench-app")).unwrap();
    let native = rattery::precompile(&wasm, None).unwrap();
    let run = |app: App| async move {
        app.location("bench://local/app.wasm?mode=sparse&frames=5")
            .cookies(CookiePolicy::Ephemeral)
            .cache(false)
            .headless(headless("sleep 2000", 10))
            .run()
            .await
            .unwrap()
    };
    // SAFETY: produced a few lines up by `precompile`.
    let fast = run(unsafe { App::from_precompiled(native.clone()) }).await;
    let slow = run(App::from_bytes(wasm.clone())).await;
    eprintln!(
        "compile: {:?} precompiled vs {:?} from source",
        fast.timings.compile, slow.timings.compile
    );
    assert_eq!(fast.status, AppStatus::Exited(0), "{}", dump(&fast));
    assert!(
        fast.stdout.contains("bench mode=sparse frames=5"),
        "{}",
        dump(&fast)
    );
    assert!(
        fast.timings.compile * 10 < slow.timings.compile,
        "deserialising should be much cheaper than compiling: {:?} vs {:?}",
        fast.timings.compile,
        slow.timings.compile
    );

    // Mixing the two up fails cleanly before anything runs.
    let err = run_err(App::from_bytes(native.clone())).await;
    assert!(err.contains("not a valid component"), "{err}");
    // SAFETY: a wasm binary is refused by the header check; nothing executes.
    let err = run_err(unsafe { App::from_precompiled(wasm) }).await;
    assert!(err.contains("not a component precompiled"), "{err}");
}

async fn run_err(app: App) -> String {
    let err = app
        .cookies(CookiePolicy::Ephemeral)
        .headless(headless("sleep 100", 5))
        .run()
        .await
        .expect_err("should fail");
    format!("{err:#}")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_app_reloads_itself_when_it_is_ready() {
    require_wasip2!();
    let server = Server::start(&[]);
    let dir = std::env::temp_dir().join(format!("rattery-e2e-reload-{}", std::process::id()));
    let phases = std::sync::Arc::new(Mutex::new(Vec::new()));
    let seen = phases.clone();
    // The headless `update` command plays the watcher; `R` is the app's
    // reload key. The launch counter in storage proves a new instance ran.
    let report = App::from_path(guest("counter-app"))
        .origin(&server.url)
        .cookies(CookiePolicy::Ephemeral)
        .storage(StoragePolicy::Dir(dir.clone()))
        .on_phase(move |phase| seen.lock().unwrap().push(format!("{phase:?}")))
        .headless(headless(
            "sleep 2500\nupdate v2\nsleep 400\nsnapshot\nkey R\nsleep 2500\nsnapshot\nkey q",
            20,
        ))
        .run()
        .await
        .unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::Exited(0), "{text}");
    let before = &report.snapshots[0];
    assert!(before.contains("update v2 available: R reloads"), "{text}");
    assert!(before.contains("launch 1"), "{text}");
    let after = &report.snapshots[1];
    assert!(!after.contains("available"), "{text}");
    assert!(after.contains("launch 2"), "{text}");
    let phases = phases.lock().unwrap().join("\n");
    assert!(phases.contains("Reloading"), "{phases}");
    assert_eq!(phases.matches("Ready").count(), 2, "{phases}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broken_update_never_interrupts_the_app() {
    require_wasip2!();
    let dir = std::env::temp_dir().join(format!("rattery-broken-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let served = dir.join("app.wasm");
    std::fs::copy(guest("counter-app"), &served).unwrap();
    let server = Server::start_serving(&served);
    for package in ["counter-app", "spin-app"] {
        App::from_path(guest(package))
            .cookies(CookiePolicy::Ephemeral)
            .headless(headless("", 1))
            .run()
            .await
            .unwrap();
    }

    let phases = std::sync::Arc::new(Mutex::new(Vec::new()));
    let seen = phases.clone();
    let run = tokio::spawn(
        App::from_url(format!("{}/app.wasm", server.url))
            .unwrap()
            .watch(true)
            .reload_policy(ReloadPolicy::Immediate)
            .on_phase(move |phase| seen.lock().unwrap().push(phase))
            .cookies(CookiePolicy::Ephemeral)
            .headless(headless("sleep 2500\nsnapshot\nsleep 5000\nsnapshot", 16))
            .run(),
    );
    tokio::time::sleep(Duration::from_secs(4)).await;
    // Garbage goes out first: the running app must not notice.
    std::fs::write(&served, b"this is not a component").unwrap();
    tokio::time::sleep(Duration::from_millis(4500)).await;
    // A real component afterwards still gets picked up.
    std::fs::copy(guest("spin-app"), &served).unwrap();

    let report = run.await.unwrap().unwrap();
    let text = dump(&report);
    assert_eq!(report.status, AppStatus::TimedOut, "{text}");
    assert!(report.snapshots[0].contains("rattery counter"), "{text}");
    let later = &report.snapshots[1];
    assert!(
        later.contains("rattery counter") && later.contains("launch 1"),
        "the app should have run on undisturbed\n{text}"
    );
    assert!(
        report
            .final_screen
            .as_ref()
            .unwrap()
            .contains("spinning forever"),
        "{text}"
    );
    let phases = phases.lock().unwrap();
    let rejected = phases
        .iter()
        .position(|p| matches!(p, Phase::UpdateRejected { reason } if reason.contains("not a valid component")))
        .unwrap_or_else(|| panic!("{phases:?}"));
    let reloaded = phases.iter().position(|p| *p == Phase::Reloading).unwrap();
    assert!(rejected < reloaded, "{phases:?}");
    assert_eq!(
        phases.iter().filter(|p| **p == Phase::Reloading).count(),
        1,
        "{phases:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
