//! Compile, sandbox, and run one app: the core of the library.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Cache, Config, Engine, Store, UpdateDeadline};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{I32Exit, WasiCtx, WasiCtxBuilder};

use crate::bindings::App as GuestApp;
use crate::http::{CookieJar, OriginPolicy};
use crate::loader::{self, Loaded};
use crate::state::{HostState, HostStateConfig};
use crate::terminal::{Interrupt, Interrupter, Screen, Session, Tasks, TerminalHost};
use crate::{App, AppStatus, CookiePolicy, Phase, Report, Source, bindings, headless};

const WATCH_INTERVAL: Duration = Duration::from_millis(750);

/// How often the engine's epoch advances while an app runs. Each tick the
/// guest yields to the host, so input, timeouts, and the kill switch stay
/// responsive, and the CPU budget is charged.
pub const EPOCH_TICK: Duration = Duration::from_millis(10);

pub async fn run(app: App) -> Result<Report> {
    // If anything inside panics, the terminal is restored by the session's
    // drop and the panic hook is put back here, before the panic continues.
    let hook_slot: crate::terminal::HookSlot = Arc::default();
    let outcome = {
        use futures::FutureExt;
        std::panic::AssertUnwindSafe(run_inner(app, hook_slot.clone()))
            .catch_unwind()
            .await
    };
    match outcome {
        Ok(result) => result,
        Err(payload) => {
            if let Some(previous) = hook_slot.lock().ok().and_then(|mut g| g.take()) {
                std::panic::set_hook(previous);
            }
            std::panic::resume_unwind(payload)
        }
    }
}

async fn run_inner(app: App, hook_slot: crate::terminal::HookSlot) -> Result<Report> {
    let run_started = std::time::Instant::now();
    let limits = app.limits.clone();
    limits.validate().context("invalid limits")?;
    let on_phase = app.on_phase.clone();
    let phase = |phase: Phase| {
        if let Some(hook) = &on_phase {
            hook(phase);
        }
    };

    let loaded = loader::load(&app.source, &limits).await?;
    let mut timings = crate::Timings {
        load: run_started.elapsed(),
        ..Default::default()
    };
    phase(Phase::Loaded {
        bytes: loaded.bytes.len(),
    });

    // The app's origin is where it came from; for bytes, a file, or a
    // resolver that did not say, whatever the embedder says it is.
    let app_origin = match &app.source {
        Source::Url(_) => loaded.origin.clone(),
        _ => app.origin.clone().or_else(|| loaded.origin.clone()),
    };
    let server_origin = app.origin.clone().or_else(|| app_origin.clone());
    let policy = OriginPolicy::new(
        app_origin.as_deref(),
        &app.allow_origins,
        app.allow_all_origins,
    )?;

    let cookies = match &app.cookies {
        CookiePolicy::Persistent => Some(match CookieJar::default_path() {
            Some(path) => CookieJar::at(path)?,
            None => CookieJar::ephemeral(),
        }),
        CookiePolicy::File(path) => Some(CookieJar::at(path.clone())?),
        CookiePolicy::Ephemeral => Some(CookieJar::ephemeral()),
        CookiePolicy::Disabled => None,
    };

    let mut config = Config::new();
    config
        .epoch_interruption(true)
        .wasm_component_model_async(true);
    if app.cache {
        let cache = Cache::from_file(None)
            .map_err(anyhow::Error::from)
            .context("failed to configure the compile cache")?;
        config.cache(Some(cache));
    }
    let engine = Engine::new(&config)?;

    // Compile before touching the terminal so errors print normally.
    let t = std::time::Instant::now();
    let mut component = compile(&engine, &loaded)?;
    timings.compile = t.elapsed();
    phase(Phase::Compiled);

    // The standard library links WASI 0.2 (stdio, clocks); HTTP and timers
    // in the guest use WASI 0.3, which is what makes the app fully async.
    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    bindings::terminal::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
    bindings::websocket::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
    for extension in app.extensions {
        extension(&mut linker).context("a host extension failed to register")?;
    }

    let interrupter = Arc::new(Interrupter::new());
    let mut tasks = Tasks::default();

    // The epoch ticker: the one clock every deadline is measured against.
    tasks.spawn({
        let engine = engine.clone();
        async move {
            let mut interval = tokio::time::interval(EPOCH_TICK);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                engine.increment_epoch();
            }
        }
    });

    let location = app.location.clone().or_else(|| loaded.location.clone());
    let (session, mut term) = match &app.headless {
        None => {
            let session = Session::enter(app.mouse, hook_slot.clone())?;
            let term = TerminalHost::interactive(
                server_origin.clone(),
                location,
                interrupter.clone(),
                limits.event_queue,
                limits.paste_bytes,
                &mut tasks,
            );
            (Some(session), term)
        }
        Some(options) => {
            let term = TerminalHost::headless(
                server_origin.clone(),
                location,
                interrupter.clone(),
                limits.event_queue,
                limits.paste_bytes,
                options.width,
                options.height,
            );
            let backend = term
                .test_backend()
                .expect("headless terminal has a test backend");
            tasks.spawn(headless::run_script(
                options.script.clone(),
                term.queue(),
                backend,
                term.snapshots(),
            ));
            if let Some(timeout) = options.timeout {
                let interrupter = interrupter.clone();
                tasks.spawn(async move {
                    tokio::time::sleep(timeout).await;
                    interrupter.fire(Interrupt::Timeout);
                });
            }
            (None, term)
        }
    };
    term.set_phase_hook(on_phase.clone());
    let snapshots = term.snapshots();
    let screen_source = term.test_backend();

    // In watch mode, poll for a new component and reload in place.
    let updates: Arc<Mutex<Option<Loaded>>> = Arc::default();
    if app.watch {
        match &app.source {
            Source::Url(url) => tasks.spawn(watch_url(
                url.clone(),
                loaded.bytes.clone(),
                loaded.etag.clone(),
                loaded.last_modified.clone(),
                limits.clone(),
                updates.clone(),
                interrupter.clone(),
            )),
            Source::Resolver(resolver) => tasks.spawn(watch_resolver(
                resolver.clone(),
                loaded.bytes.clone(),
                loaded.etag.clone(),
                limits.clone(),
                updates.clone(),
                interrupter.clone(),
            )),
            _ => {}
        }
    }

    let mut stdout_all = String::new();
    let mut stderr_all = String::new();
    let mut stats;
    let mut ext = app.ext;

    let status = loop {
        term.mark_started();
        let stdout = MemoryOutputPipe::new(limits.guest_output_bytes);
        let stderr = MemoryOutputPipe::new(limits.guest_output_bytes);
        let wasi = wasi_ctx(
            &app.env,
            server_origin.as_deref(),
            stdout.clone(),
            stderr.clone(),
        );
        let state = HostState::new(HostStateConfig {
            wasi,
            policy: policy.clone(),
            cookies: cookies.clone(),
            request_policy: app.request_policy.clone(),
            term,
            limits: limits.clone(),
            interrupter: interrupter.clone(),
            on_phase: on_phase.clone(),
            ext: std::mem::take(&mut ext),
        });
        let mut store = Store::new(&engine, state);
        store.limiter(|state| state.limiter());
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|mut store| match store.data_mut().tick() {
            None => Ok(UpdateDeadline::Yield(1)),
            Some(reason) => Err(wasmtime::Error::msg(format!("interrupted: {reason:?}"))),
        });

        let outcome = {
            let run = async {
                let t = std::time::Instant::now();
                let guest = GuestApp::instantiate_async(&mut store, &component, &linker).await?;
                timings.instantiate = t.elapsed();
                phase(Phase::Instantiated);
                store
                    .run_concurrent(async move |store| guest.call_run(store).await)
                    .await?
            };
            tokio::select! {
                result = run => Some(result),
                _ = interrupter.notified() => None,
            }
        };
        let state = store.into_data();
        // Nothing else keeps extension state; it stays with us across reloads.
        let parts = state.into_parts();
        term = parts.term;
        ext = parts.ext;
        stats = term.stats();
        stats.memory_peak = parts.memory_peak;
        // Sockets the app still held: their tasks were aborted with the
        // store; wait for them to be gone.
        for handle in parts.websocket_tasks {
            handle.abort();
            let _ = handle.await;
        }

        append_bounded(
            &mut stdout_all,
            &String::from_utf8_lossy(&stdout.contents()),
            limits.guest_output_bytes,
        );
        append_bounded(
            &mut stderr_all,
            &String::from_utf8_lossy(&stderr.contents()),
            limits.guest_output_bytes,
        );

        let reason = interrupter.take_reason();
        if reason == Some(Interrupt::Reload) {
            if let Some(next) = updates.lock().unwrap().take() {
                match compile(&engine, &next) {
                    Ok(compiled) => component = compiled,
                    Err(err) => append_bounded(
                        &mut stderr_all,
                        &format!("rattery: reload skipped: {err:#}\n"),
                        limits.guest_output_bytes,
                    ),
                }
            }
            let _ = term.reset();
            phase(Phase::Reloading);
            continue;
        }

        break match (outcome, reason) {
            (_, Some(Interrupt::Kill)) => AppStatus::Killed,
            (_, Some(Interrupt::Timeout)) => AppStatus::TimedOut,
            (_, Some(Interrupt::Limit(what))) => AppStatus::LimitExceeded(what),
            (_, Some(Interrupt::Reload)) | (None, None) => AppStatus::Exited(0),
            (Some(Ok(Ok(()))), None) => AppStatus::Exited(0),
            (Some(Ok(Err(message))), None) => {
                let message = truncate(message, limits.message_bytes);
                append_bounded(
                    &mut stderr_all,
                    &format!("{message}\n"),
                    limits.guest_output_bytes,
                );
                AppStatus::Exited(1)
            }
            (Some(Err(err)), None) => match err.downcast_ref::<I32Exit>() {
                Some(exit) => AppStatus::Exited(exit.0),
                None => AppStatus::Trapped(truncate(format!("{err:?}"), limits.message_bytes)),
            },
        };
    };

    // Everything that ran alongside the app stops before the terminal is
    // handed back.
    tasks.shutdown().await;
    drop(term);
    drop(session);
    phase(Phase::Exited(status.clone()));

    let final_screen = screen_source.map(|b| Screen::from_backend(&b.lock().unwrap()));
    let snapshots = std::mem::take(&mut *snapshots.lock().unwrap());
    timings.first_draw = stats.first_draw;
    timings.total = run_started.elapsed();

    Ok(Report {
        status,
        stdout: stdout_all,
        stderr: stderr_all,
        snapshots,
        final_screen,
        timings,
        stats,
    })
}

/// Append to a text kept within `max` bytes in total: the oldest output goes.
fn append_bounded(kept: &mut String, more: &str, max: usize) {
    kept.push_str(more);
    if kept.len() > max {
        let mut cut = kept.len() - max;
        while !kept.is_char_boundary(cut) {
            cut += 1;
        }
        kept.drain(..cut);
    }
}

fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("...");
    }
    text
}

fn compile(engine: &Engine, loaded: &Loaded) -> Result<Component> {
    Component::new(engine, &loaded.bytes)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("{} is not a valid component", loaded.description))
}

/// The guest gets no filesystem, no sockets, no inherited stdio: only the
/// terminal interface, the clock, randomness, and HTTP to allowed origins.
fn wasi_ctx(
    env: &[(String, String)],
    server_origin: Option<&str>,
    stdout: MemoryOutputPipe,
    stderr: MemoryOutputPipe,
) -> WasiCtx {
    let mut wasi = WasiCtxBuilder::new();
    wasi.stdout(stdout).stderr(stderr).arg("app");
    if let Some(origin) = server_origin {
        wasi.env("RATTERY_ORIGIN", origin);
    }
    for (key, value) in env {
        wasi.env(key, value);
    }
    wasi.build()
}

#[allow(clippy::too_many_arguments)]
async fn watch_url(
    url: url::Url,
    mut current: Vec<u8>,
    mut etag: Option<String>,
    mut last_modified: Option<String>,
    limits: crate::Limits,
    updates: Arc<Mutex<Option<Loaded>>>,
    interrupter: Arc<Interrupter>,
) {
    let Ok(client) = loader::client(&limits) else {
        return;
    };
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        let fetched = loader::fetch_if_changed(
            &client,
            &url,
            etag.as_deref(),
            last_modified.as_deref(),
            Some(&current),
            &limits,
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

async fn watch_resolver(
    resolver: Arc<dyn crate::Resolver>,
    mut current: Vec<u8>,
    mut version: Option<String>,
    limits: crate::Limits,
    updates: Arc<Mutex<Option<Loaded>>>,
    interrupter: Arc<Interrupter>,
) {
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        if let Ok(Some(next)) =
            loader::resolve_if_changed(&resolver, version.as_deref(), &current, &limits).await
        {
            current = next.bytes.clone();
            version = next.etag.clone();
            *updates.lock().unwrap() = Some(next);
            interrupter.fire(Interrupt::Reload);
        }
    }
}
