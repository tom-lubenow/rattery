//! `rattery`: run a ratatui app packaged as a WASI 0.2 component inside the
//! current terminal, with the same isolation a browser gives a web page.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use rattery::{App, AppStatus, CookiePolicy, HeadlessOptions, Script};

/// Run a ratatui app delivered as a WASI component, sandboxed like a web page.
#[derive(Debug, Parser)]
#[command(name = "rattery", version)]
struct Cli {
    /// URL (http/https) or local path of the app component (.wasm).
    source: String,

    /// Origin the app's server functions are sent to. Defaults to the origin
    /// of SOURCE when it is a URL; an app loaded from a file has no origin
    /// without this.
    #[arg(long, value_name = "URL")]
    origin: Option<String>,

    /// Extra origins the app may reach over HTTP (repeatable).
    #[arg(long = "allow-origin", value_name = "URL")]
    allow_origins: Vec<String>,

    /// Let the app reach any origin over HTTP.
    #[arg(long)]
    allow_all_origins: bool,

    /// Browser-style CORS: other origins may opt in per response with
    /// Access-Control-Allow-Origin.
    #[arg(long)]
    cors: bool,

    /// URL reported to the app as its location, query string included
    /// (defaults to SOURCE when it is a URL).
    #[arg(long, value_name = "URL")]
    location: Option<String>,

    /// Environment variable to expose to the app (repeatable).
    #[arg(long, value_name = "KEY=VALUE", value_parser = parse_env)]
    env: Vec<(String, String)>,

    /// Keep cookies for this run only, like a private browser window.
    #[arg(long, conflicts_with_all = ["no_cookies", "cookie_jar"])]
    incognito: bool,

    /// Never send or store cookies.
    #[arg(long, conflicts_with = "cookie_jar")]
    no_cookies: bool,

    /// Store cookies in this file instead of the default jar.
    #[arg(long, value_name = "FILE")]
    cookie_jar: Option<PathBuf>,

    /// Poll the server for a new component and restart the app in place
    /// when one is published (URL sources only).
    #[arg(long)]
    watch: bool,

    /// Do not report mouse events to the app.
    #[arg(long)]
    no_mouse: bool,

    /// Skip the on-disk cache of compiled components.
    #[arg(long)]
    no_cache: bool,

    /// Run without a terminal, on an in-memory screen of this size, and print
    /// the snapshots the script takes plus the final screen.
    #[arg(long, value_name = "COLSxROWS", value_parser = parse_size)]
    headless: Option<(u16, u16)>,

    /// Script of input to feed a headless run (see `rattery --help-script`).
    #[arg(long, value_name = "FILE", requires = "headless")]
    script: Option<PathBuf>,

    /// Stop a headless run after this many seconds.
    #[arg(long, value_name = "SECS", requires = "headless")]
    timeout: Option<f64>,

    /// Print the headless script format and exit.
    #[arg(long)]
    help_script: bool,

    /// After the app exits, print timings and terminal counters to stderr.
    #[arg(long)]
    stats: bool,
}

fn parse_env(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| format!("expected KEY=VALUE, got {s:?}"))
}

fn parse_size(s: &str) -> Result<(u16, u16), String> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| format!("expected COLSxROWS, got {s:?}"))?;
    Ok((
        w.parse().map_err(|e| format!("bad width: {e}"))?,
        h.parse().map_err(|e| format!("bad height: {e}"))?,
    ))
}

const SCRIPT_HELP: &str = "\
Headless scripts are one command per line; blank lines and # comments are ignored.

  sleep 500          milliseconds
  key k              a single character
  key ctrl-c         modifiers: ctrl, alt, shift, super, meta
  key enter          enter esc up down left right tab backtab backspace delete
                     insert home end pageup pagedown space f1..f24
  type hello world   one key event per character
  paste some text    a bracketed paste
  resize 100 30      columns rows; the app receives a resize event
  snapshot           capture the screen; printed when the app ends
";

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.help_script {
        print!("{SCRIPT_HELP}");
        return Ok(());
    }

    let mut app = App::from_source(&cli.source)?
        .allow_all_origins(cli.allow_all_origins)
        .cors(cli.cors)
        .mouse(!cli.no_mouse)
        .cache(!cli.no_cache)
        .cookies(if cli.no_cookies {
            CookiePolicy::Disabled
        } else if cli.incognito {
            CookiePolicy::Ephemeral
        } else if let Some(path) = cli.cookie_jar {
            CookiePolicy::File(path)
        } else {
            CookiePolicy::Persistent
        })
        .watch(cli.watch);
    if let Some(origin) = cli.origin {
        app = app.origin(origin);
    }
    if let Some(location) = cli.location {
        app = app.location(location);
    }
    for origin in cli.allow_origins {
        app = app.allow_origin(origin);
    }
    for (key, value) in cli.env {
        app = app.env(key, value);
    }
    if let Some((width, height)) = cli.headless {
        let script = match &cli.script {
            Some(path) => Script::parse(
                &std::fs::read_to_string(path)
                    .with_context(|| format!("failed to read {}", path.display()))?,
            )?,
            None => Script::default(),
        };
        app = app.headless(HeadlessOptions {
            width,
            height,
            script,
            timeout: cli.timeout.map(Duration::from_secs_f64),
        });
    }

    let report = app.run().await?;

    for (index, screen) in report.snapshots.iter().enumerate() {
        println!(
            "--- snapshot {} ({}x{}) ---",
            index + 1,
            screen.width,
            screen.height
        );
        print!("{screen}");
    }
    if let Some(screen) = &report.final_screen {
        println!("--- final screen ({}x{}) ---", screen.width, screen.height);
        print!("{screen}");
    }
    if !report.stdout.is_empty() {
        print!("{}", report.stdout);
    }
    if !report.stderr.is_empty() {
        eprint!("{}", report.stderr);
    }
    if cli.stats {
        let t = &report.timings;
        let s = &report.stats;
        let ms = |d: Duration| {
            if d < Duration::from_millis(1) {
                format!("{}µs", d.as_micros())
            } else {
                format!("{:.1}ms", d.as_secs_f64() * 1000.0)
            }
        };
        eprintln!(
            "rattery stats: load {}, compile {}, instantiate {}, first frame {}, total {}",
            ms(t.load),
            ms(t.compile),
            ms(t.instantiate),
            t.first_draw.map(ms).unwrap_or_else(|| "-".into()),
            ms(t.total)
        );
        let per =
            |total: Duration, n: u64| ms(total.checked_div(n.max(1) as u32).unwrap_or_default());
        eprintln!(
            "  draws {} ({} cells, {} avg on host), flushes {} ({} avg), events {}",
            s.draws,
            s.cells,
            per(s.draw_time, s.draws),
            s.flushes,
            per(s.flush_time, s.flushes),
            s.events
        );
    }
    match &report.status {
        AppStatus::Exited(0) => {}
        AppStatus::Exited(code) => eprintln!("app exited with status {code}"),
        AppStatus::Trapped(message) => eprintln!("app trapped: {message}"),
        AppStatus::Killed => eprintln!("app terminated by rattery (Ctrl-C pressed three times)"),
        AppStatus::TimedOut => eprintln!("app stopped: headless timeout elapsed"),
    }
    std::process::exit(report.exit_code());
}
