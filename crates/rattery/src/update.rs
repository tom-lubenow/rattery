//! The update model: discovery (is there a newer component), state (what
//! is pending and when the host will act), and application (the reload).
//!
//! A browser never reloads a page under the user; the page learns of an
//! update and reloads itself. The same shape here: the app (or the
//! embedder) asks the host to check, reads the pending state, and calls
//! `reload`; a policy decides whether the host ever forces it.
//!
//! Every state change is committed in one step under `inner`, once a
//! candidate's outcome is known; `checking` serialises discovery so two
//! candidates cannot interleave their validation and commits.

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::task::JoinHandle;
use wasmtime::Engine;
use wasmtime::component::Linker;

use crate::bindings::AppPre;
use crate::bindings::terminal as t;
use crate::loader::{self, Fetch, Loaded, UpgradeRequired};
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

/// The least time between two checks the app itself asks for; closer
/// requests get the current state without a fetch.
pub const GUEST_CHECK_INTERVAL: Duration = Duration::from_secs(1);

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
    id: u64,
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

    fn version(&self) -> Option<String> {
        self.etag.clone().or_else(|| self.last_modified.clone())
    }
}

/// A fetched candidate that failed validation. Remembered so polls stay
/// conditional on it and compare against its bytes (it is neither
/// downloaded nor compiled again while the source keeps serving it) and so
/// a version hint naming it does not trigger a check.
struct Rejected {
    validators: Validators,
    version: Option<String>,
    bytes: Arc<Vec<u8>>,
}

struct Inner {
    running: Running,
    pending: Option<Pending>,
    /// Validators of the newest accepted fetch from the source: the pending
    /// update if there is one, else the running version.
    newest: Validators,
    rejected: Option<Rejected>,
    deadline: Option<Deadline>,
    next_deadline_id: u64,
    /// The last rejection reported, so a source that keeps answering the
    /// same way is reported once; cleared when a candidate is accepted.
    last_rejection: Option<String>,
    /// The check a version hint started, so hints never pile up.
    hint_task: Option<JoinHandle<()>>,
    /// When the app itself last asked for a check.
    last_guest_check: Option<Instant>,
    /// The run ended: nothing is fetched, offered, or fired any more.
    closed: bool,
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

/// A candidate did not compile or does not link against this host, or the
/// run is over.
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
    /// One discovery at a time, validation included: a check from the
    /// watcher, the app, a hint, or an offer from the embedder.
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
                next_deadline_id: 1,
                last_rejection: None,
                hint_task: None,
                last_guest_check: None,
                closed: false,
            }),
        }
    }

    fn phase(&self, phase: Phase) {
        if let Some(hook) = &self.on_phase {
            hook(phase);
        }
    }

    fn closed_rejection() -> Rejection {
        Rejection {
            reason: "the app has exited".into(),
            requires_abi: None,
        }
    }

    /// The run is over: forget the pending update, stop the deadline and
    /// any hint check, and refuse everything from now on. An `AppHandle`
    /// outliving the run becomes inert.
    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.pending = None;
        if let Some(deadline) = inner.deadline.take() {
            deadline.task.abort();
        }
        if let Some(task) = inner.hint_task.take() {
            task.abort();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().unwrap().closed
    }

    /// Whether this app can be updated at all, and by whom. `Unavailable`
    /// means the host has nowhere to look; the embedder may still offer
    /// a component through its handle.
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
        self.check_locked().await
    }

    /// [`check`](Updates::check) for the app itself: at most one fetch per
    /// [`GUEST_CHECK_INTERVAL`]; closer calls get the state without one.
    pub async fn check_throttled(self: &Arc<Self>) -> Result<Option<UpdateInfo>> {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(anyhow::Error::new(Self::closed_rejection()));
            }
            let now = Instant::now();
            if inner
                .last_guest_check
                .is_some_and(|t| now.duration_since(t) < GUEST_CHECK_INTERVAL)
            {
                return Ok(info_of(&inner));
            }
            inner.last_guest_check = Some(now);
        }
        self.check().await
    }

    async fn check_locked(self: &Arc<Self>) -> Result<Option<UpdateInfo>> {
        let (validators, previous, rejected_standing) = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return Err(anyhow::Error::new(Self::closed_rejection()));
            }
            // While a rejected candidate stands, ask conditionally on it and
            // compare against its bytes: the source serving it again costs
            // nothing, and anything else comes back in full so the
            // bookkeeping can clear the rejection.
            match &inner.rejected {
                Some(rejected) => (rejected.validators.clone(), rejected.bytes.clone(), true),
                None => {
                    let newest_bytes = inner
                        .pending
                        .as_ref()
                        .map_or_else(|| inner.running.bytes.clone(), |p| p.bytes.clone());
                    (inner.newest.clone(), newest_bytes, false)
                }
            }
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
                    Some(&previous),
                    &self.limits,
                )
                .await
            }
            Source::Path(path) => loader::read_if_changed(path, &previous, &self.limits)
                .await
                .map(|loaded| loaded.map_or(Fetch::NotModified, Fetch::New)),
            Source::Resolver(resolver) => loader::resolve_if_changed(
                resolver,
                validators.etag.as_deref(),
                &previous,
                &self.limits,
            )
            .await
            .map(|loaded| loaded.map_or(Fetch::NotModified, Fetch::New)),
            Source::Bytes(_) | Source::Precompiled(_) => Ok(Fetch::NotModified),
        };
        match fetched {
            Ok(Fetch::New(loaded)) => match self.offer_locked(loaded, true).await {
                Ok(Offer::Pending(info) | Offer::Unchanged(info)) => Ok(Some(info)),
                Ok(Offer::Current) => Ok(None),
                Err(rejection) => Err(anyhow::Error::new(rejection)),
            },
            Ok(Fetch::Same {
                etag,
                last_modified,
            }) => {
                // The same bytes under new validators (a re-copy of the same
                // file): remember them, or every poll downloads it again and
                // every hint naming the new tag starts a check.
                let validators = Validators {
                    etag,
                    last_modified,
                };
                let version = validators.version();
                let mut inner = self.inner.lock().unwrap();
                if rejected_standing {
                    if let Some(rejected) = &mut inner.rejected {
                        rejected.validators = validators;
                        rejected.version = version;
                    }
                } else {
                    inner.newest = validators;
                    if version.is_some() {
                        match &mut inner.pending {
                            Some(pending) => pending.version = version,
                            None => inner.running.version = version,
                        }
                    }
                }
                Ok(info_of(&inner))
            }
            Ok(Fetch::NotModified) => Ok(self.pending()),
            Err(err) => {
                if let Some(upgrade) = err.downcast_ref::<UpgradeRequired>() {
                    self.report_rejection(upgrade.to_string(), upgrade.required.clone());
                }
                Err(err)
            }
        }
    }

    /// A component from the embedder. Validated like any candidate, one at
    /// a time with the checks.
    pub async fn offer(self: &Arc<Self>, loaded: Loaded) -> Result<Offer, Rejection> {
        let _one_at_a_time = self.checking.lock().await;
        self.offer_locked(loaded, false).await
    }

    /// A candidate component, with `checking` held. The running version's
    /// bytes withdraw a pending update; the pending update's bytes change
    /// nothing; anything else is validated off the runtime and becomes the
    /// pending update or a rejection. `from_source` says the validators are
    /// the source's and may steer the next conditional fetch. Every state
    /// change happens in one step once the outcome is known, so a failed
    /// candidate leaves the running and pending state, and the validators,
    /// exactly as they were.
    async fn offer_locked(
        self: &Arc<Self>,
        loaded: Loaded,
        from_source: bool,
    ) -> Result<Offer, Rejection> {
        let validators = from_source.then(|| Validators::of(&loaded));
        if let Some(outcome) = self.settle_known(&loaded.bytes, validators.as_ref()) {
            return outcome;
        }
        let version = Validators::of(&loaded).version();
        // The validator comes from the server: keep it printable and short.
        let shown = version
            .as_deref()
            .map(|v| truncate(crate::sanitize::text(v), self.limits.message_bytes.min(256)));
        let updates = self.clone();
        let validated = tokio::task::spawn_blocking(move || {
            let component = match compile(&updates.engine, &loaded) {
                Ok(component) => component,
                Err(err) => return Err((err, None, loaded.bytes)),
            };
            let requires = requires_abi(&updates.engine, &component);
            match link(
                &updates.engine,
                &updates.linker,
                &component,
                &loaded.description,
            ) {
                Ok(instance) => Ok((loaded.bytes, instance)),
                Err(err) => Err((err, requires, loaded.bytes)),
            }
        })
        .await;
        let (err, requires_abi, bytes) = match validated {
            Ok(Ok((bytes, instance))) => {
                let bytes = Arc::new(bytes);
                // The world may have moved while validation ran (a reload
                // took the pending update): decide again before committing.
                if let Some(outcome) = self.settle_known(&bytes, validators.as_ref()) {
                    return outcome;
                }
                let info = self.commit_pending(
                    Pending {
                        bytes,
                        instance,
                        version,
                        shown,
                    },
                    validators,
                );
                return Ok(Offer::Pending(info));
            }
            Ok(Err((err, requires, bytes))) => (format!("{err:#}"), requires, bytes),
            Err(err) => (format!("validation failed: {err}"), None, Vec::new()),
        };
        let reason = truncate(crate::sanitize::text(&err), self.limits.message_bytes);
        if let Some(validators) = validators {
            let mut inner = self.inner.lock().unwrap();
            if !inner.closed {
                inner.rejected = Some(Rejected {
                    validators,
                    version,
                    bytes: Arc::new(bytes),
                });
            }
        }
        self.report_rejection(reason.clone(), requires_abi.clone());
        Err(Rejection {
            reason,
            requires_abi,
        })
    }

    /// Bytes the state already knows settle without validation: the running
    /// version (withdrawing a pending update) or the pending one. Also the
    /// answer when the run is over.
    fn settle_known(
        &self,
        bytes: &[u8],
        validators: Option<&Validators>,
    ) -> Option<Result<Offer, Rejection>> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Some(Err(Self::closed_rejection()));
        }
        let version = validators.and_then(Validators::version);
        if *inner.running.bytes == *bytes {
            if let Some(validators) = validators {
                inner.newest = validators.clone();
            }
            if version.is_some() {
                inner.running.version = version;
            }
            inner.rejected = None;
            inner.last_rejection = None;
            let withdrawn = inner.pending.take().is_some();
            if let Some(deadline) = inner.deadline.take() {
                deadline.task.abort();
            }
            drop(inner);
            if withdrawn {
                self.phase(Phase::UpdateWithdrawn);
                self.queue.push_update_changed();
            }
            return Some(Ok(Offer::Current));
        }
        if inner.pending.as_ref().is_some_and(|p| *p.bytes == *bytes) {
            if let Some(validators) = validators {
                inner.newest = validators.clone();
            }
            if version.is_some()
                && let Some(pending) = &mut inner.pending
            {
                pending.version = version;
            }
            inner.rejected = None;
            inner.last_rejection = None;
            return Some(Ok(Offer::Unchanged(info_of(&inner).expect("pending"))));
        }
        None
    }

    /// A pretend update on the running component itself, so the handling
    /// can be exercised without a server (the headless `update` command).
    pub fn offer_same(self: &Arc<Self>, shown: Option<String>) {
        let pending = {
            let inner = self.inner.lock().unwrap();
            if inner.closed {
                return;
            }
            Pending {
                bytes: inner.running.bytes.clone(),
                instance: inner.running.instance.clone(),
                version: None,
                shown,
            }
        };
        self.commit_pending(pending, None);
    }

    /// The candidate passed: it is the pending update, the validators (if
    /// from the source) describe it, and no rejection stands.
    fn commit_pending(
        self: &Arc<Self>,
        pending: Pending,
        validators: Option<Validators>,
    ) -> UpdateInfo {
        let shown = pending.shown.clone();
        let mut inner = self.inner.lock().unwrap();
        inner.pending = Some(pending);
        if let Some(validators) = validators {
            inner.newest = validators;
        }
        inner.rejected = None;
        inner.last_rejection = None;
        if let ReloadPolicy::Deferred {
            grace,
            idle,
            hard_limit,
        } = self.policy
        {
            // The first update's deadline stands for later ones, so a
            // stream of deploys cannot postpone the reload forever.
            let stale = inner.deadline.as_ref().is_none_or(|d| d.task.is_finished());
            if stale {
                let id = inner.next_deadline_id;
                inner.next_deadline_id += 1;
                let now = Instant::now();
                let after = now + grace;
                let by = now + hard_limit.max(grace);
                let task = tokio::spawn(deadline_task(Arc::downgrade(self), id, after, by, idle));
                inner.deadline = Some(Deadline {
                    id,
                    after,
                    by,
                    task,
                });
            }
        }
        let info = info_of(&inner).expect("pending was just set");
        drop(inner);
        self.phase(Phase::UpdateAvailable { version: shown });
        if self.policy == ReloadPolicy::Immediate {
            self.interrupter.fire(Interrupt::Reload);
        } else {
            self.queue.push_update_changed();
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

    /// A reloaded component could not be instantiated and the previous one
    /// is back: the running slot must say so.
    pub fn restore_running(
        &self,
        instance: AppPre<HostState>,
        bytes: Arc<Vec<u8>>,
        version: Option<String>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        inner.running = Running {
            bytes,
            instance,
            version,
        };
    }

    /// The running component, for the runner to remember before a reload.
    pub fn running(&self) -> (Arc<Vec<u8>>, Option<String>) {
        let inner = self.inner.lock().unwrap();
        (inner.running.bytes.clone(), inner.running.version.clone())
    }

    fn knows(inner: &Inner, version: &str) -> bool {
        inner.running.version.as_deref() == Some(version)
            || inner
                .pending
                .as_ref()
                .is_some_and(|p| p.version.as_deref() == Some(version))
            || inner
                .rejected
                .as_ref()
                .is_some_and(|r| r.version.as_deref() == Some(version))
    }

    /// A server function reply from the app's own origin named the version
    /// it serves. One the host does not know starts a check, unless one
    /// started by a hint is still running.
    pub fn hint(self: &Arc<Self>, version: &str) {
        if matches!(self.source, Source::Bytes(_) | Source::Precompiled(_)) {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.closed
            || Self::knows(&inner, version)
            || inner.hint_task.as_ref().is_some_and(|t| !t.is_finished())
        {
            return;
        }
        let weak = Arc::downgrade(self);
        let version = version.to_owned();
        inner.hint_task = Some(tokio::spawn(async move {
            let Some(updates) = weak.upgrade() else {
                return;
            };
            let _one_at_a_time = updates.checking.lock().await;
            // A check that ran in between may have learned this version.
            if Self::knows(&updates.inner.lock().unwrap(), &version) {
                return;
            }
            let _ = updates.check_locked().await;
        }));
    }

    /// Tell the embedder, once per distinct reason while nothing is
    /// accepted in between.
    fn report_rejection(&self, reason: String, requires_abi: Option<String>) {
        let reason = truncate(crate::sanitize::text(&reason), self.limits.message_bytes);
        let mut inner = self.inner.lock().unwrap();
        if inner.closed || inner.last_rejection.as_deref() == Some(reason.as_str()) {
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

/// After `after`, reload at the first moment the user has been idle for
/// `idle`; at `by` regardless. Fires only while its update is still the
/// pending one, checked under the lock, so a withdrawn or applied update
/// never produces a reload from here.
async fn deadline_task(
    updates: Weak<Updates>,
    id: u64,
    after: Instant,
    by: Instant,
    idle: Duration,
) {
    tokio::time::sleep_until(after.into()).await;
    loop {
        let Some(updates) = updates.upgrade() else {
            return;
        };
        let now = Instant::now();
        let wait = {
            let inner = updates.inner.lock().unwrap();
            let mine = inner.deadline.as_ref().is_some_and(|d| d.id == id);
            if inner.closed || !mine || inner.pending.is_none() {
                return;
            }
            let idle_for = updates
                .queue
                .last_input()
                .map_or(Duration::MAX, |t| now.saturating_duration_since(t));
            if now >= by || idle_for >= idle {
                updates.interrupter.fire(Interrupt::Reload);
                return;
            }
            (idle - idle_for)
                .min(by - now)
                .clamp(Duration::from_millis(50), Duration::from_secs(1))
        };
        drop(updates);
        tokio::time::sleep(wait).await;
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
        let inner = self.inner.get_mut().unwrap();
        if let Some(deadline) = inner.deadline.take() {
            deadline.task.abort();
        }
        if let Some(task) = inner.hint_task.take() {
            task.abort();
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
