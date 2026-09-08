//! Compile, sandbox, and run one app: the core of both the CLI and the library.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use url::Url;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Cache, Config, Engine, Store};
use wasmtime_wasi::p2::bindings::Command;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{I32Exit, WasiCtx, WasiCtxBuilder};

use crate::http::{CookieJar, OriginPolicy};
use crate::loader::{self, Loaded};
use crate::state::HostState;
use crate::terminal::{Interrupt, Interrupter, Screen, Session, TerminalHost};
use crate::{App, AppStatus, CookiePolicy, Report, Source, bindings, headless};

const WATCH_INTERVAL: Duration = Duration::from_millis(750);

pub async fn run(app: App) -> Result<Report> {
    let loaded = loader::load(&app.source).await?;

    // The app's origin is where it came from; for bytes or a file, whatever
    // the embedder says it is.
    let app_origin = match (&app.source, &app.origin) {
        (Source::Url(_), _) => loaded.origin.clone(),
        (_, override_origin) => override_origin.clone(),
    };
    let server_origin = app.origin.clone().or_else(|| app_origin.clone());
    let policy = OriginPolicy::new(
        app_origin.as_deref(),
        &app.allow_origins,
        app.allow_all_origins,
        app.cors,
    )?;

    let cookies = match &app.cookies {
        CookiePolicy::Persistent => Some(match CookieJar::default_path() {
            Some(path) => CookieJar::at(path),
            None => CookieJar::ephemeral(),
        }),
        CookiePolicy::File(path) => Some(CookieJar::at(path.clone())),
        CookiePolicy::Ephemeral => Some(CookieJar::ephemeral()),
        CookiePolicy::Disabled => None,
    };

    let mut config = Config::new();
    config.epoch_interruption(true);
    if app.cache {
        let cache = Cache::from_file(None)
            .map_err(anyhow::Error::from)
            .context("failed to configure the compile cache")?;
        config.cache(Some(cache));
    }
    let engine = Engine::new(&config)?;

    // Compile before touching the terminal so errors print normally.
    let mut component = compile(&engine, &loaded)?;

    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    bindings::terminal::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
    bindings::websocket::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;

    let interrupter = Arc::new(Interrupter::new(engine.clone()));
    let location = app.location.clone().or_else(|| match &app.source {
        Source::Url(url) => Some(url.to_string()),
        _ => None,
    });

    let (session, mut term) = match &app.headless {
        None => {
            let session = Session::enter(app.mouse)?;
            let term =
                TerminalHost::interactive(server_origin.clone(), location, interrupter.clone());
            (Some(session), term)
        }
        Some(options) => {
            let term = TerminalHost::headless(
                server_origin.clone(),
                location,
                interrupter.clone(),
                options.width,
                options.height,
            );
            let backend = term
                .test_backend()
                .expect("headless terminal has a test backend");
            tokio::spawn(headless::run_script(
                options.script.clone(),
                term.queue(),
                backend,
                term.snapshots(),
            ));
            if let Some(timeout) = options.timeout {
                let interrupter = interrupter.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(timeout).await;
                    interrupter.fire(Interrupt::Timeout);
                });
            }
            (None, term)
        }
    };
    let snapshots = term.snapshots();
    let screen_source = term.test_backend();

    // In watch mode, poll the server for a new component and reload in place.
    let updates: Arc<Mutex<Option<Loaded>>> = Arc::default();
    if let (true, Source::Url(url)) = (app.watch, &app.source) {
        tokio::spawn(watch_for_updates(
            url.clone(),
            loaded.bytes.clone(),
            loaded.etag.clone(),
            loaded.last_modified.clone(),
            updates.clone(),
            interrupter.clone(),
        ));
    }

    let mut stdout_all = String::new();
    let mut stderr_all = String::new();

    let status = loop {
        let stdout = MemoryOutputPipe::new(1 << 20);
        let stderr = MemoryOutputPipe::new(1 << 20);
        let wasi = wasi_ctx(
            &app,
            server_origin.as_deref(),
            stdout.clone(),
            stderr.clone(),
        );
        let state = HostState::new(wasi, policy.clone(), cookies.clone(), term);
        let mut store = Store::new(&engine, state);
        store.set_epoch_deadline(1);

        let outcome = {
            let run = async {
                let command = Command::instantiate_async(&mut store, &component, &linker).await?;
                command.wasi_cli_run().call_run(&mut store).await
            };
            tokio::select! {
                result = run => Some(result),
                _ = interrupter.notified() => None,
            }
        };
        term = store.into_data().into_terminal();

        stdout_all.push_str(&String::from_utf8_lossy(&stdout.contents()));
        stderr_all.push_str(&String::from_utf8_lossy(&stderr.contents()));

        let reason = interrupter.take_reason();
        if reason == Some(Interrupt::Reload) {
            if let Some(next) = updates.lock().unwrap().take() {
                match compile(&engine, &next) {
                    Ok(compiled) => component = compiled,
                    Err(err) => stderr_all.push_str(&format!("rattery: reload skipped: {err:#}\n")),
                }
            }
            let _ = term.reset();
            continue;
        }

        break match (outcome, reason) {
            (_, Some(Interrupt::Kill)) => AppStatus::Killed,
            (_, Some(Interrupt::Timeout)) => AppStatus::TimedOut,
            (_, Some(Interrupt::Reload)) | (None, None) => AppStatus::Exited(0),
            (Some(Ok(Ok(()))), None) => AppStatus::Exited(0),
            (Some(Ok(Err(()))), None) => AppStatus::Exited(1),
            (Some(Err(err)), None) => match err.downcast_ref::<I32Exit>() {
                Some(exit) => AppStatus::Exited(exit.0),
                None => AppStatus::Trapped(format!("{err:?}")),
            },
        };
    };

    drop(term);
    drop(session);

    let final_screen = screen_source.map(|b| Screen::from_backend(&b.lock().unwrap()));
    let snapshots = std::mem::take(&mut *snapshots.lock().unwrap());

    Ok(Report {
        status,
        stdout: stdout_all,
        stderr: stderr_all,
        snapshots,
        final_screen,
    })
}

fn compile(engine: &Engine, loaded: &Loaded) -> Result<Component> {
    Component::new(engine, &loaded.bytes)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("{} is not a valid component", loaded.description))
}

/// The guest gets no filesystem, no sockets, no inherited stdio: only the
/// terminal interface, the clock, randomness, and HTTP to allowed origins.
fn wasi_ctx(
    app: &App,
    server_origin: Option<&str>,
    stdout: MemoryOutputPipe,
    stderr: MemoryOutputPipe,
) -> WasiCtx {
    let mut wasi = WasiCtxBuilder::new();
    wasi.stdout(stdout).stderr(stderr).arg("app");
    if let Some(origin) = server_origin {
        wasi.env("RATTERY_ORIGIN", origin);
    }
    for (key, value) in &app.env {
        wasi.env(key, value);
    }
    wasi.build()
}

async fn watch_for_updates(
    url: Url,
    mut current: Vec<u8>,
    mut etag: Option<String>,
    mut last_modified: Option<String>,
    updates: Arc<Mutex<Option<Loaded>>>,
    interrupter: Arc<Interrupter>,
) {
    let client = reqwest::Client::new();
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        let fetched = loader::fetch_if_changed(
            &client,
            &url,
            etag.as_deref(),
            last_modified.as_deref(),
            Some(&current),
        )
        .await;
        // A server that is down or mid-restart just gets polled again.
        if let Ok(Some(next)) = fetched {
            current = next.bytes.clone();
            etag = next.etag.clone();
            last_modified = next.last_modified.clone();
            *updates.lock().unwrap() = Some(next);
            interrupter.fire(Interrupt::Reload);
        }
    }
}
