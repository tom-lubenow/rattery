//! The update model: discovery (is there a newer component), state (what
//! is pending and when the host will act), and application (the reload).
//!
//! A browser never reloads a page under the user; the page learns of an
//! update and reloads itself. The same shape here: the app (or the
//! embedder) asks the host to check, reads the pending state, and calls
//! `reload`; a policy decides whether the host ever forces it.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::task::JoinHandle;
use wasmtime::Engine;
use wasmtime::component::Linker;

use crate::bindings::AppPre;
use crate::bindings::terminal as t;
use crate::loader::{self, Loaded, UpgradeRequired};
use crate::runner::{compile, link, requires_abi, truncate};
use crate::state::HostState;
use crate::terminal::{EventQueue, Interrupt, Interrupter, PhaseHook};
use crate::{Limits, Phase, ReloadPolicy, Source, UpdateInfo};

/// A response header on server function replies naming the component
/// version the server currently serves (its ETag). A value the host does
/// not know triggers a check at once, so staleness is noticed on the first
/// call after a deploy instead of at the next poll.
pub const VERSION_HEADER: &str = "rattery-app-version";

/// How often `watch` checks.
pub const WATCH_INTERVAL: Duration = Duration::from_millis(750);

/// The component the app is running on.
pub struct Running {
    pub bytes: Arc<Vec<u8>>,
    pub instance: AppPre<HostState>,
    /// The server's validator for it, raw.
    pub version: Option<String>,
}

struct Pending {
    bytes: Arc<Vec<u8>>,
    instance: AppPre<HostState>,
    /// Raw validator, compared against version hints.
    version: Option<String>,
    /// Sanitised and bounded, shown to the app and the embedder.
    shown: Option<String>,
}

struct Deadline {
    /// After this the host reloads at the first idle moment.
    after: Instant,
    /// At this the host reloads regardless.
    by: Instant,
    task: JoinHandle<()>,
}

struct Inner {
    running: Running,
    /// Validators of the newest component fetched (pending if any, else
    /// running), so polls stay conditional.
    etag: Option<String>,
    last_modified: Option<String>,
    pending: Option<Pending>,
    deadline: Option<Deadline>,
    /// The last rejection reported, so a server that keeps answering the
    /// same way is reported once, not every check.
    last_rejection: Option<String>,
}

pub struct UpdatesConfig {
    pub engine: Engine,
    pub linker: Linker<HostState>,
    pub policy: ReloadPolicy,
    pub watched: bool,
    pub source: Source,
    pub limits: Limits,
    pub queue: Arc<EventQueue>,
    pub interrupter: Arc<Interrupter>,
    pub on_phase: Option<PhaseHook>,
    pub running: Running,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub struct Updates {
    engine: Engine,
    linker: Linker<HostState>,
    policy: ReloadPolicy,
    watched: bool,
    source: Source,
    limits: Limits,
    queue: Arc<EventQueue>,
    interrupter: Arc<Interrupter>,
    on_phase: Option<PhaseHook>,
    client: OnceLock<reqwest::Client>,
    /// One check at a time; a second caller waits for the first.
    checking: tokio::sync::Mutex<()>,
    inner: Mutex<Inner>,
}

impl Updates {
    pub fn new(config: UpdatesConfig) -> Self {
        Self {
            engine: config.engine,
            linker: config.linker,
            policy: config.policy,
            watched: config.watched,
            source: config.source,
            limits: config.limits,
            queue: config.queue,
            interrupter: config.interrupter,
            on_phase: config.on_phase,
            client: OnceLock::new(),
            checking: tokio::sync::Mutex::new(()),
            inner: Mutex::new(Inner {
                running: config.running,
                etag: config.etag,
                last_modified: config.last_modified,
                pending: None,
                deadline: None,
                last_rejection: None,
            }),
        }
    }

    fn phase(&self, phase: Phase) {
        if let Some(hook) = &self.on_phase {
            hook(phase);
        }
    }

    /// Whether this app can be updated at all, and by whom.
    pub fn availability(&self) -> t::Availability {
        match (&self.source, self.watched) {
            (Source::Bytes(_) | Source::Precompiled(_), _) => t::Availability::Unavailable,
            (_, true) => t::Availability::Watched,
            (_, false) => t::Availability::OnRequest,
        }
    }

    /// Ask the source for a newer component now. Validates what it finds
    /// and returns the pending state afterwards.
    pub async fn check(self: &Arc<Self>) -> Result<Option<UpdateInfo>> {
        let _one_at_a_time = self.checking.lock().await;
        let (etag, last_modified, newest) = {
            let inner = self.inner.lock().unwrap();
            let newest = inner
                .pending
                .as_ref()
                .map_or_else(|| inner.running.bytes.clone(), |p| p.bytes.clone());
            (inner.etag.clone(), inner.last_modified.clone(), newest)
        };
        let fetched = match &self.source {
            Source::Url(url) => {
                let client = match self.client.get() {
                    Some(client) => client,
                    None => {
                        let _ = self.client.set(loader::client(&self.limits)?);
                        self.client.get().expect("just set")
                    }
                };
                loader::fetch_if_changed(
                    client,
                    url,
                    etag.as_deref(),
                    last_modified.as_deref(),
                    Some(&newest),
                    &self.limits,
                )
                .await
            }
            Source::Path(path) => loader::read_if_changed(path, &newest, &self.limits).await,
            Source::Resolver(resolver) => {
                loader::resolve_if_changed(resolver, etag.as_deref(), &newest, &self.limits).await
            }
            Source::Bytes(_) | Source::Precompiled(_) => Ok(None),
        };
        match fetched {
            Ok(Some(loaded)) => {
                {
                    let mut inner = self.inner.lock().unwrap();
                    inner.etag = loaded.etag.clone();
                    inner.last_modified = loaded.last_modified.clone();
                }
                Ok(self.offer(loaded).await)
            }
            Ok(None) => Ok(self.pending()),
            Err(err) => {
                if let Some(upgrade) = err.downcast_ref::<UpgradeRequired>() {
                    self.reject(upgrade.to_string(), upgrade.required.clone());
                }
                Err(err)
            }
        }
    }

    /// A component from anywhere (the source, or the embedder). The same
    /// bytes as the running app withdraw a pending update; the same bytes
    /// as the pending one change nothing; anything else is validated off
    /// the runtime and, if it passes, becomes the pending update.
    pub async fn offer(self: &Arc<Self>, loaded: Loaded) -> Option<UpdateInfo> {
        {
            let inner = self.inner.lock().unwrap();
            if *inner.running.bytes == loaded.bytes {
                drop(inner);
                self.withdraw();
                return None;
            }
            if inner
                .pending
                .as_ref()
                .is_some_and(|p| *p.bytes == loaded.bytes)
            {
                return info_of(&inner);
            }
        }
        let version = loaded.etag.clone().or_else(|| loaded.last_modified.clone());
        // The validator comes from the server: keep it printable and short.
        let shown = version
            .as_deref()
            .map(|v| truncate(crate::sanitize::text(v), self.limits.message_bytes.min(256)));
        let updates = self.clone();
        let validated = tokio::task::spawn_blocking(move || {
            let component = compile(&updates.engine, &loaded).map_err(|err| (err, None))?;
            let requires = requires_abi(&updates.engine, &component);
            match link(
                &updates.engine,
                &updates.linker,
                &component,
                &loaded.description,
            ) {
                Ok(instance) => Ok((loaded.bytes, instance)),
                Err(err) => Err((err, requires)),
            }
        })
        .await;
        match validated {
            Ok(Ok((bytes, instance))) => Some(self.set_pending(Pending {
                bytes: Arc::new(bytes),
                instance,
                version,
                shown,
            })),
            Ok(Err((err, requires))) => {
                self.reject(format!("{err:#}"), requires);
                None
            }
            Err(err) => {
                self.reject(format!("validation failed: {err}"), None);
                None
            }
        }
    }

    /// A pretend update on the running component itself, so the handling
    /// can be exercised without a server (the headless `update` command).
    pub fn offer_same(&self, shown: Option<String>) {
        let pending = {
            let inner = self.inner.lock().unwrap();
            Pending {
                bytes: inner.running.bytes.clone(),
                instance: inner.running.instance.clone(),
                version: None,
                shown,
            }
        };
        self.set_pending(pending);
    }

    fn set_pending(&self, pending: Pending) -> UpdateInfo {
        let shown = pending.shown.clone();
        let mut inner = self.inner.lock().unwrap();
        inner.pending = Some(pending);
        match self.policy {
            ReloadPolicy::Immediate => {}
            ReloadPolicy::AppControlled => {}
            ReloadPolicy::Deferred {
                grace,
                idle,
                hard_limit,
            } => {
                // The first update's deadline stands for later ones, so a
                // stream of deploys cannot postpone the reload forever.
                let stale = inner.deadline.as_ref().is_none_or(|d| d.task.is_finished());
                if stale {
                    let now = Instant::now();
                    let after = now + grace;
                    let by = now + hard_limit.max(grace);
                    let queue = self.queue.clone();
                    let interrupter = self.interrupter.clone();
                    let task = tokio::spawn(async move {
                        tokio::time::sleep_until(after.into()).await;
                        loop {
                            let now = Instant::now();
                            let idle_for = queue
                                .last_push()
                                .map_or(Duration::MAX, |t| now.saturating_duration_since(t));
                            if now >= by || idle_for >= idle {
                                interrupter.fire(Interrupt::Reload);
                                return;
                            }
                            let wait = (idle - idle_for)
                                .min(by - now)
                                .clamp(Duration::from_millis(50), Duration::from_secs(1));
                            tokio::time::sleep(wait).await;
                        }
                    });
                    inner.deadline = Some(Deadline { after, by, task });
                }
            }
        }
        let info = info_of(&inner).expect("pending was just set");
        drop(inner);
        self.phase(Phase::UpdateAvailable { version: shown });
        if self.policy == ReloadPolicy::Immediate {
            self.interrupter.fire(Interrupt::Reload);
        } else {
            self.queue.push(t::Event::UpdateChanged);
        }
        info
    }

    /// The newest component is the running one again: forget the pending
    /// update and its deadline.
    pub fn withdraw(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.pending.take().is_none() {
            return;
        }
        if let Some(deadline) = inner.deadline.take() {
            deadline.task.abort();
        }
        drop(inner);
        self.phase(Phase::UpdateWithdrawn);
        self.queue.push(t::Event::UpdateChanged);
    }

    /// The pending update, with the deadlines as of now.
    pub fn pending(&self) -> Option<UpdateInfo> {
        info_of(&self.inner.lock().unwrap())
    }

    /// For the reload now happening: the pending instance becomes the
    /// running one. `None` means restart the current component.
    pub fn take(&self) -> Option<AppPre<HostState>> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(deadline) = inner.deadline.take() {
            deadline.task.abort();
        }
        let pending = inner.pending.take()?;
        inner.running = Running {
            bytes: pending.bytes,
            instance: pending.instance.clone(),
            version: pending.version,
        };
        Some(pending.instance)
    }

    /// A server function reply named the version it serves. One the host
    /// does not know triggers a check, unless one is already running.
    pub fn hint(self: &Arc<Self>, version: &str) {
        {
            let inner = self.inner.lock().unwrap();
            let known = inner.running.version.as_deref() == Some(version)
                || inner
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.version.as_deref() == Some(version));
            if known || matches!(self.source, Source::Bytes(_) | Source::Precompiled(_)) {
                return;
            }
        }
        if self.checking.try_lock().is_err() {
            return;
        }
        let updates = self.clone();
        tokio::spawn(async move {
            let _ = updates.check().await;
        });
    }

    fn reject(&self, reason: String, requires_abi: Option<String>) {
        let reason = truncate(crate::sanitize::text(&reason), self.limits.message_bytes);
        let mut inner = self.inner.lock().unwrap();
        if inner.last_rejection.as_deref() == Some(reason.as_str()) {
            return;
        }
        inner.last_rejection = Some(reason.clone());
        drop(inner);
        self.phase(Phase::UpdateRejected {
            reason,
            requires_abi,
        });
    }
}

fn info_of(inner: &Inner) -> Option<UpdateInfo> {
    let pending = inner.pending.as_ref()?;
    let now = Instant::now();
    let (reload_after, reload_by) = match &inner.deadline {
        Some(d) if !d.task.is_finished() => (
            Some(d.after.saturating_duration_since(now)),
            Some(d.by.saturating_duration_since(now)),
        ),
        _ => (None, None),
    };
    Some(UpdateInfo {
        version: pending.shown.clone(),
        reload_after,
        reload_by,
    })
}

impl Drop for Updates {
    fn drop(&mut self) {
        if let Some(deadline) = self.inner.get_mut().unwrap().deadline.take() {
            deadline.task.abort();
        }
    }
}

pub fn to_wit(info: UpdateInfo) -> t::Update {
    t::Update {
        version: info.version,
        reload_after_ms: info.reload_after.map(|d| d.as_millis() as u64),
        reload_by_ms: info.reload_by.map(|d| d.as_millis() as u64),
    }
}
