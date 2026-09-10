//! Compile, sandbox, and run one app: the core of the library.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Cache, Config, Engine, Store, UpdateDeadline};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{I32Exit, WasiCtx, WasiCtxBuilder};

use crate::bindings::{App as GuestApp, AppPre};
use crate::http::{CookieJar, OriginPolicy};
use crate::loader::{self, Loaded};
use crate::state::{HostState, HostStateConfig};
use crate::storage::Storage;
use crate::terminal::{Interrupt, Interrupter, Screen, Session, Tasks, TerminalHost};
use crate::update::{Running, Updates, UpdatesConfig, WATCH_INTERVAL};
use crate::{
    App, AppStatus, CookiePolicy, Phase, Report, Source, StoragePolicy, bindings, headless,
};

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

    let mut config = engine_config();
    if app.cache && !loaded.precompiled {
        let cache = Cache::from_file(None)
            .map_err(anyhow::Error::from)
            .context("failed to configure the compile cache")?;
        config.cache(Some(cache));
    }
    let engine = Engine::new(&config)?;

    // Compile before touching the terminal so errors print normally.
    let t = std::time::Instant::now();
    let component = compile(&engine, &loaded)?;
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
    bindings::storage::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
    for extension in app.extensions {
        extension(&mut linker).context("a host extension failed to register")?;
    }
    // Resolve the component's imports now, so one built against another ABI
    // fails here with a readable error rather than after the terminal is up.
    let mut instance_pre = link(&engine, &linker, &component, &loaded.description)?;
    let mut previous_instance: Option<AppPre<HostState>> = None;

    let interrupter = Arc::new(Interrupter::new());
    let mut tasks = Tasks::default();
    // Filled once the update state exists; the headless script needs it.
    let updates_slot: Arc<std::sync::OnceLock<Arc<Updates>>> = Arc::default();

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
                updates_slot.clone(),
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
    term.queue().set_coalesce(app.coalesce_input);
    term.set_phase_hook(on_phase.clone());
    let snapshots = term.snapshots();
    let screen_source = term.test_backend();

    // The update state: what is running, what is pending, when the host
    // acts. Discovery is the watcher, a check from the app or the embedder,
    // or a version hint on a server reply.
    let updates = Arc::new(Updates::new(UpdatesConfig {
        engine: engine.clone(),
        linker: linker.clone(),
        policy: app.reload,
        watched: app.watch,
        source: app.source.clone(),
        limits: limits.clone(),
        queue: term.queue(),
        interrupter: interrupter.clone(),
        on_phase: on_phase.clone(),
        running: Running {
            bytes: Arc::new(loaded.bytes.clone()),
            instance: instance_pre.clone(),
            version: loaded.etag.clone().or_else(|| loaded.last_modified.clone()),
        },
        etag: loaded.etag.clone(),
        last_modified: loaded.last_modified.clone(),
    }));
    let _ = updates_slot.set(updates.clone());
    if let Some(handle) = &app.handle {
        handle.attach(updates.clone(), interrupter.clone());
    }
    if app.watch
        && matches!(
            app.source,
            Source::Url(_) | Source::Resolver(_) | Source::Path(_)
        )
    {
        let updates = updates.clone();
        tasks.spawn(async move {
            loop {
                tokio::time::sleep(WATCH_INTERVAL).await;
                // A source that is down or mid-restart just gets polled again.
                let _ = updates.check().await;
            }
        });
    }

    // Storage is keyed by the app's origin; without one it cannot persist.
    let mut storage = match (&app.storage, &app_origin) {
        (StoragePolicy::Disabled, _) => Storage::disabled(),
        (StoragePolicy::Ephemeral, _) | (_, None) => Storage::ephemeral(&limits),
        (StoragePolicy::Dir(dir), Some(origin)) => Storage::persistent(dir, origin, &limits)?,
        (StoragePolicy::Persistent, Some(origin)) => match storage_dir() {
            Some(dir) => Storage::persistent(&dir, origin, &limits)?,
            None => Storage::ephemeral(&limits),
        },
    };

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
            storage,
            updates: updates.clone(),
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

        let instantiated = std::sync::atomic::AtomicBool::new(false);
        let outcome = {
            let run = async {
                let t = std::time::Instant::now();
                let guest: GuestApp = instance_pre.instantiate_async(&mut store).await?;
                instantiated.store(true, std::sync::atomic::Ordering::Relaxed);
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
        storage = parts.storage;
        ext = parts.ext;
        stats = term.stats();
        stats.memory_peak = parts.memory_peak;
        // Sockets the app still held: their tasks were aborted when their
        // resources went with the store; wait for them to be gone.
        parts.websocket_tasks.close();
        parts.websocket_tasks.wait().await;

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
            // Without a pending update the same component restarts. A
            // pending one was compiled and linked when it was offered.
            if let Some(next) = updates.take() {
                previous_instance = Some(std::mem::replace(&mut instance_pre, next));
            }
            let _ = term.reset();
            phase(Phase::Reloading);
            continue;
        }
        // A freshly reloaded component that could not even be instantiated
        // (a resource limit, say) is not worth ending the run over: go back
        // to the one that worked, once.
        if let (false, Some(Err(err)), Some(previous)) = (
            instantiated.load(std::sync::atomic::Ordering::Relaxed),
            &outcome,
            previous_instance.take(),
        ) {
            append_bounded(
                &mut stderr_all,
                &format!(
                    "rattery: the new version failed to start, keeping the previous one: {err:#}\n"
                ),
                limits.guest_output_bytes,
            );
            instance_pre = previous;
            let _ = term.reset();
            phase(Phase::Reloading);
            continue;
        }

        break match (outcome, reason) {
            (_, Some(Interrupt::Kill)) => AppStatus::Killed,
            (_, Some(Interrupt::Shutdown)) => AppStatus::Stopped,
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

fn storage_dir() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("", "", "rattery").map(|d| d.data_local_dir().join("storage"))
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

/// Cut `text` to at most `max` bytes, ending in `...` when there is room
/// for the marker.
pub(crate) fn truncate(mut text: String, max: usize) -> String {
    const MARK: &str = "...";
    if text.len() <= max {
        return text;
    }
    // Room for the marker, or as much text as fits when there is none.
    let keep = if max >= MARK.len() {
        max - MARK.len()
    } else {
        max
    };
    let mut cut = keep;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    if max >= MARK.len() {
        text.push_str(MARK);
    }
    text
}

/// The engine settings every rattery host uses; precompiled components must
/// come from the same settings, so [`precompile`] shares them.
pub(crate) fn engine_config() -> Config {
    let mut config = Config::new();
    config
        .epoch_interruption(true)
        .wasm_component_model_async(true);
    config
}

/// Compile a component to native code for `target` (the host when `None`),
/// for [`crate::App::from_precompiled`]. When a target is named, the host's
/// CPU features are not assumed, so the output runs on any machine of that
/// triple.
pub fn precompile(component: &[u8], target: Option<&str>) -> Result<Vec<u8>> {
    let mut config = engine_config();
    if let Some(target) = target {
        config
            .target(target)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("cannot compile for target {target}"))?;
    }
    let engine = Engine::new(&config)?;
    engine
        .precompile_component(component)
        .map_err(anyhow::Error::from)
        .context("the bytes are not a valid component")
}

/// The rattery ABI `component` needs when it is not this host's.
pub(crate) fn requires_abi(engine: &Engine, component: &Component) -> Option<String> {
    let ty = component.component_type();
    crate::abi_of(ty.imports(engine).map(|(name, _)| name))
        .and_then(|abi| crate::abi_mismatch(&abi))
}

/// Resolve every import against the host's linker without running anything.
/// An ABI transition is named as such in the error.
pub(crate) fn link(
    engine: &Engine,
    linker: &Linker<HostState>,
    component: &Component,
    description: &str,
) -> Result<AppPre<HostState>> {
    linker
        .instantiate_pre(component)
        .and_then(AppPre::new)
        .map_err(anyhow::Error::from)
        .with_context(|| match requires_abi(engine, component) {
            Some(required) => format!(
                "{description} was built for {required}; this host provides {}; upgrade the host",
                crate::ABI
            ),
            None => format!("{description} cannot run on this host (ABI {})", crate::ABI),
        })
}

pub(crate) fn compile(engine: &Engine, loaded: &Loaded) -> Result<Component> {
    if loaded.precompiled {
        // SAFETY: the embedder vouched for these bytes by using
        // `App::from_precompiled`; wasmtime still checks the header, the
        // engine settings, and the target before trusting the code inside.
        return unsafe { Component::deserialize(engine, &loaded.bytes) }
            .map_err(anyhow::Error::from)
            .with_context(|| {
                format!(
                    "{} is not a component precompiled for this host and rattery version",
                    loaded.description
                )
            });
    }
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

#[cfg(test)]
mod tests {
    use super::{append_bounded, truncate};

    #[test]
    fn truncate_is_a_strict_limit() {
        assert_eq!(truncate("hello".into(), 10), "hello");
        assert_eq!(truncate("hello world".into(), 8), "hello...");
        assert_eq!(truncate("hello world".into(), 2), "he");
        assert!(truncate("héllo wörld".into(), 5).len() <= 5);
        for max in 0..12 {
            assert!(
                truncate("hello world!".into(), max).len() <= max,
                "max {max}"
            );
        }
    }

    #[test]
    fn append_bounded_keeps_the_newest() {
        let mut kept = String::new();
        append_bounded(&mut kept, "abcdef", 4);
        assert_eq!(kept, "cdef");
        append_bounded(&mut kept, "gh", 4);
        assert_eq!(kept, "efgh");
    }
}
