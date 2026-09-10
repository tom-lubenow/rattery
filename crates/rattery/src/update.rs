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

/// What a conditional fetch sends back to the server.
#[derive(Clone, Default, PartialEq, Eq)]
struct Validators {
    etag: Option<String>,
    last_modified: Option<String>,
}

impl Validators {
    fn of(loaded: &Loaded) -> Self {
        Self {
            etag: loaded.etag.clone(),
            last_modified: loaded.last_modified.clone(),
        }
    }
}

/// A fetched candidate that failed validation. Remembered so polls stay
/// conditional on it (it is not downloaded again while the server keeps
/// serving it) and so a version hint naming it does not trigger a check.
struct Rejected {
    validators: Validators,
    version: Option<String>,
}

struct Inner {
    running: Running,
    pending: Option<Pending>,
    /// Validators of the newest accepted fetch from the source: the pending
    /// update if there is one, else the running version.
    newest: Validators,
    rejected: Option<Rejected>,
    deadline: Option<Deadline>,
    /// The last rejection reported, so a server that keeps answering the
    /// same way is reported once, not every check.
    last_rejection: Option<String>,
}

/// What became of an offered component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    /// It is now the pending update.
    Pending(UpdateInfo),
    /// It already was the pending update.
    Unchanged(UpdateInfo),
    /// It is the running version. A pending update, if there was one, is
    /// withdrawn.
    Current,
}

/// A candidate did not compile or does not link against this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Sanitised and bounded.
    pub reason: String,
    /// Set when the candidate needs a rattery ABI this host does not
    /// provide.
    pub requires_abi: Option<String>,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for Rejection {}

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
                pending: None,
                newest: Validators {
                    etag: config.etag,
                    last_modified: config.last_modified,
                },
                rejected: None,
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
    /// and returns the pending state afterwards; a candidate that fails
    /// validation is the error.
    pub async fn check(self: &Arc<Self>) -> Result<Option<UpdateInfo>> {
        let _one_at_a_time = self.checking.lock().await;
        let (validators, newest_bytes) = {
            let inner = self.inner.lock().unwrap();
            let newest_bytes = inner
                .pending
                .as_ref()
                .map_or_else(|| inner.running.bytes.clone(), |p| p.bytes.clone());
            // While a rejected candidate stands, ask conditionally on it
            // (no re-download while the server keeps serving it) and take
            // whatever else comes back in full, so the bookkeeping can
            // clear the rejection once the server moves on.
            let validators = inner
                .rejected
                .as_ref()
                .map_or_else(|| inner.newest.clone(), |r| r.validators.clone());
            (validators, inner.rejected.is_none().then_some(newest_bytes))
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
                    validators.etag.as_deref(),
                    validators.last_modified.as_deref(),
                    newest_bytes.as_deref().map(Vec::as_slice),
                    &self.limits,
                )
                .await
            }
            Source::Path(path) => {
                loader::read_if_changed(
                    path,
                    newest_bytes.as_deref().map_or(&[][..], Vec::as_slice),
                    &self.limits,
                )
                .await
            }
            Source::Resolver(resolver) => {
                loader::resolve_if_changed(
                    resolver,
                    validators.etag.as_deref(),
                    newest_bytes.as_deref().map_or(&[][..], Vec::as_slice),
                    &self.limits,
                )
                .await
            }
            Source::Bytes(_) | Source::Precompiled(_) => Ok(None),
        };
        match fetched {
            Ok(Some(loaded)) => match self.offer_from(loaded, true).await {
                Ok(Offer::Pending(info) | Offer::Unchanged(info)) => Ok(Some(info)),
                Ok(Offer::Current) => Ok(None),
                Err(rejection) => Err(anyhow::Error::new(rejection)),
            },
            Ok(None) => Ok(self.pending()),
            Err(err) => {
                if let Some(upgrade) = err.downcast_ref::<UpgradeRequired>() {
                    self.report_rejection(upgrade.to_string(), upgrade.required.clone());
                }
                Err(err)
            }
        }
    }

    /// A component from the embedder. See [`Updates::offer_from`].
    pub async fn offer(self: &Arc<Self>, loaded: Loaded) -> Result<Offer, Rejection> {
        self.offer_from(loaded, false).await
    }

    /// A candidate component. The running version's bytes withdraw a
    /// pending update; the pending update's bytes change nothing; anything
    /// else is validated off the runtime and becomes the pending update or
    /// a rejection. `from_source` says the validators are the source's and
    /// may steer the next conditional fetch. Every state change happens in
    /// one step once the outcome is known, so a failed candidate leaves the
    /// running and pending state, and the validators, exactly as they were.
    async fn offer_from(
        self: &Arc<Self>,
        loaded: Loaded,
        from_source: bool,
    ) -> Result<Offer, Rejection> {
        let validators = from_source.then(|| Validators::of(&loaded));
        {
            let mut inner = self.inner.lock().unwrap();
            if *inner.running.bytes == loaded.bytes {
                if let Some(validators) = validators {
                    inner.newest = validators;
                }
                inner.rejected = None;
                let withdrawn = inner.pending.take().is_some();
                if let Some(deadline) = inner.deadline.take() {
                    deadline.task.abort();
                }
                drop(inner);
                if withdrawn {
                    self.phase(Phase::UpdateWithdrawn);
                    self.queue.push(t::Event::UpdateChanged);
                }
                return Ok(Offer::Current);
            }
            if inner
                .pending
                .as_ref()
                .is_some_and(|p| *p.bytes == loaded.bytes)
            {
                if let Some(validators) = validators {
                    inner.newest = validators;
                }
                inner.rejected = None;
                return Ok(Offer::Unchanged(info_of(&inner).expect("pending")));
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
        let (err, requires_abi) = match validated {
            Ok(Ok((bytes, instance))) => {
                let info = self.set_pending(
                    Pending {
                        bytes: Arc::new(bytes),
                        instance,
                        version,
                        shown,
                    },
                    validators,
                );
                return Ok(Offer::Pending(info));
            }
            Ok(Err((err, requires))) => (format!("{err:#}"), requires),
            Err(err) => (format!("validation failed: {err}"), None),
        };
        let reason = truncate(crate::sanitize::text(&err), self.limits.message_bytes);
        if let Some(validators) = validators {
            self.inner.lock().unwrap().rejected = Some(Rejected {
                validators,
                version,
            });
        }
        self.report_rejection(reason.clone(), requires_abi.clone());
        Err(Rejection {
            reason,
            requires_abi,
        })
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
        self.set_pending(pending, None);
    }

    /// The candidate passed: it is the pending update, the validators (if
    /// from the source) describe it, and no rejection stands.
    fn set_pending(&self, pending: Pending, validators: Option<Validators>) -> UpdateInfo {
        let shown = pending.shown.clone();
        let mut inner = self.inner.lock().unwrap();
        inner.pending = Some(pending);
        if let Some(validators) = validators {
            inner.newest = validators;
        }
        inner.rejected = None;
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
                    .is_some_and(|p| p.version.as_deref() == Some(version))
                || inner
                    .rejected
                    .as_ref()
                    .is_some_and(|r| r.version.as_deref() == Some(version));
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

    /// Tell the embedder, once per distinct reason.
    fn report_rejection(&self, reason: String, requires_abi: Option<String>) {
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
