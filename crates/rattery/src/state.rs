//! The `Store` data: WASI contexts, limits, the terminal, extension state,
//! and the host implementation of `rattery:tui/terminal` and
//! `rattery:tui/websocket`.
//!
//! Synchronous WIT functions land in the `Host` traits and take `&mut self`.
//! The `async func`s (`next-event`, websocket `connect` and `receive`) land in
//! the `HostWithStore` traits: they run concurrently with the guest and reach
//! the store through an `Accessor` only when they need it.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use wasmtime::ResourceLimiter;
use wasmtime::component::{Accessor, HasSelf, Resource, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use crate::bindings::storage::{self, StorageError};
use crate::bindings::terminal::{self, CellUpdate, ClearType, Event, Position, Size, WindowSize};
use crate::bindings::websocket;
use crate::http::{CookieJar, OriginHooks, OriginPolicy, RequestPolicy};
use crate::storage::Storage;
use crate::terminal::{Interrupt, Interrupter, PhaseHook, TerminalHost};
use crate::update::{self, Updates};
use crate::websocket::WsSocket;
use crate::{Limits, Phase};

/// What survives a run of the store.
pub struct RunParts {
    pub term: TerminalHost,
    pub storage: Storage,
    pub ext: HashMap<TypeId, Box<dyn Any + Send>>,
    pub memory_peak: usize,
    /// Reaps finished connection tasks continuously; `close` + `wait` it.
    pub websocket_tasks: tokio_util::task::TaskTracker,
}

/// The CPU budget, counted in epoch ticks while the guest executes.
pub struct CpuBudget {
    pub ticks: u64,
    pub budget_ticks: Option<u64>,
}

/// Bounds memory and tables *in aggregate* across every memory and table
/// the instance has, unlike wasmtime's `StoreLimits`, whose limits apply to
/// each memory separately.
pub struct AggregateLimiter {
    memory_limit: usize,
    memory_used: usize,
    table_limit: usize,
    table_used: usize,
    memories: usize,
    tables: usize,
    instances: usize,
    peak: usize,
}

impl AggregateLimiter {
    pub fn new(limits: &Limits) -> Self {
        Self {
            memory_limit: limits.memory_bytes,
            memory_used: 0,
            table_limit: limits.table_elements,
            table_used: 0,
            memories: limits.memories,
            tables: limits.tables,
            instances: limits.instances,
            peak: 0,
        }
    }

    /// The most linear memory in use at once across all memories.
    pub fn memory_peak(&self) -> usize {
        self.peak
    }
}

impl ResourceLimiter for AggregateLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let others = self.memory_used.saturating_sub(current);
        if others.saturating_add(desired) > self.memory_limit {
            return Err(wasmtime::Error::msg(format!(
                "memory limit of {} bytes exceeded (growing to {desired} bytes with {others} bytes in other memories)",
                self.memory_limit
            )));
        }
        self.memory_used = others + desired;
        self.peak = self.peak.max(self.memory_used);
        Ok(true)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let others = self.table_used.saturating_sub(current);
        if others.saturating_add(desired) > self.table_limit {
            return Err(wasmtime::Error::msg(format!(
                "table limit of {} elements exceeded",
                self.table_limit
            )));
        }
        self.table_used = others + desired;
        Ok(true)
    }

    fn instances(&self) -> usize {
        self.instances
    }

    fn tables(&self) -> usize {
        self.tables
    }

    fn memories(&self) -> usize {
        self.memories
    }
}

/// The data behind the wasmtime `Store`. Host extensions registered with
/// [`App::extension`](crate::App::extension) receive a
/// `Linker<HostState>` and can keep their own state in it through
/// [`HostState::ext`] and [`HostState::ext_mut`].
pub struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: OriginHooks,
    policy: OriginPolicy,
    cookies: Option<CookieJar>,
    request_policy: Option<Arc<dyn RequestPolicy>>,
    term: TerminalHost,
    storage: Storage,
    updates: Arc<Updates>,
    limits: Limits,
    limiter: AggregateLimiter,
    cpu: CpuBudget,
    interrupter: Arc<Interrupter>,
    on_phase: Option<PhaseHook>,
    /// Websocket slots; a permit lives in each socket resource.
    websocket_slots: Arc<tokio::sync::Semaphore>,
    /// Connection tasks. A tracker forgets tasks as they finish, so an app
    /// that connects and drops sockets forever costs nothing here.
    websocket_tasks: tokio_util::task::TaskTracker,
    ext: HashMap<TypeId, Box<dyn Any + Send>>,
}

pub struct HostStateConfig {
    pub wasi: WasiCtx,
    pub policy: OriginPolicy,
    pub cookies: Option<CookieJar>,
    pub request_policy: Option<Arc<dyn RequestPolicy>>,
    pub term: TerminalHost,
    pub storage: Storage,
    pub updates: Arc<Updates>,
    pub limits: Limits,
    pub interrupter: Arc<Interrupter>,
    pub on_phase: Option<PhaseHook>,
    pub ext: HashMap<TypeId, Box<dyn Any + Send>>,
}

impl HostState {
    pub fn new(config: HostStateConfig) -> Self {
        let HostStateConfig {
            wasi,
            policy,
            cookies,
            request_policy,
            term,
            storage,
            updates,
            limits,
            interrupter,
            on_phase,
            ext,
        } = config;
        let limiter = AggregateLimiter::new(&limits);
        let websocket_slots = Arc::new(tokio::sync::Semaphore::new(limits.websockets));
        let mut table = ResourceTable::new();
        table.set_max_capacity(limits.resources);
        let budget_ticks = limits
            .cpu_time
            .map(|d| (d.as_nanos() / crate::runner::EPOCH_TICK.as_nanos()).max(1) as u64);
        Self {
            table,
            wasi,
            http: WasiHttpCtx::new(),
            hooks: {
                let mut hooks = OriginHooks::new(
                    policy.clone(),
                    cookies.clone(),
                    request_policy.clone(),
                    &limits,
                    on_phase.clone(),
                );
                hooks.set_updates(updates.clone());
                hooks
            },
            policy,
            cookies,
            request_policy,
            term,
            storage,
            updates,
            limits,
            limiter,
            cpu: CpuBudget {
                ticks: 0,
                budget_ticks,
            },
            interrupter,
            on_phase,
            websocket_slots,
            websocket_tasks: tokio_util::task::TaskTracker::new(),
            ext,
        }
    }

    pub fn into_terminal(self) -> TerminalHost {
        self.term
    }

    /// The terminal, the extension state (carried across a reload), the peak
    /// memory the app used, and the websocket tasks still to be awaited.
    pub(crate) fn into_parts(self) -> RunParts {
        let websocket_tasks = self.websocket_tasks;
        RunParts {
            term: self.term,
            storage: self.storage,
            ext: self.ext,
            memory_peak: self.limiter.memory_peak(),
            websocket_tasks,
        }
    }

    pub(crate) fn limiter(&mut self) -> &mut AggregateLimiter {
        &mut self.limiter
    }

    /// State registered with [`App::state`](crate::App::state).
    pub fn ext<T: Any + Send>(&self) -> Option<&T> {
        self.ext
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref())
    }

    /// Mutable access to state registered with [`App::state`](crate::App::state).
    pub fn ext_mut<T: Any + Send>(&mut self) -> Option<&mut T> {
        self.ext
            .get_mut(&TypeId::of::<T>())
            .and_then(|b| b.downcast_mut())
    }

    /// The resource table, for host extensions that own resources.
    pub fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    /// One epoch tick elapsed while the guest was executing. Returns the
    /// reason to stop, if any.
    pub(crate) fn tick(&mut self) -> Option<Interrupt> {
        if self.interrupter.is_fired() {
            // Keep the reason visible to the runner.
            let reason = self.interrupter.take_reason().unwrap_or(Interrupt::Kill);
            self.interrupter.fire(reason.clone());
            return Some(reason);
        }
        self.cpu.ticks += 1;
        if let Some(budget) = self.cpu.budget_ticks
            && self.cpu.ticks > budget
        {
            let reason = Interrupt::Limit(format!(
                "CPU budget of {:?} exhausted",
                self.limits.cpu_time.unwrap_or_default()
            ));
            self.interrupter.fire(reason.clone());
            return Some(reason);
        }
        None
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HostState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

impl terminal::Host for HostState {
    async fn draw(&mut self, updates: Vec<CellUpdate>) -> wasmtime::Result<()> {
        if updates.len() > self.limits.frame_cells {
            let reason = Interrupt::Limit(format!(
                "a frame carried {} cells, over the limit of {}",
                updates.len(),
                self.limits.frame_cells
            ));
            self.interrupter.fire(reason.clone());
            return Err(wasmtime::Error::msg(format!("{reason:?}")));
        }
        self.term.draw(&updates)?;
        Ok(())
    }

    async fn append_lines(&mut self, n: u16) -> wasmtime::Result<()> {
        self.term.append_lines(n)?;
        Ok(())
    }

    async fn hide_cursor(&mut self) -> wasmtime::Result<()> {
        self.term.hide_cursor()?;
        Ok(())
    }

    async fn show_cursor(&mut self) -> wasmtime::Result<()> {
        self.term.show_cursor()?;
        Ok(())
    }

    async fn get_cursor_position(&mut self) -> wasmtime::Result<Position> {
        Ok(self.term.cursor_position()?)
    }

    async fn set_cursor_position(&mut self, pos: Position) -> wasmtime::Result<()> {
        self.term.set_cursor_position(pos)?;
        Ok(())
    }

    async fn clear(&mut self, kind: ClearType) -> wasmtime::Result<()> {
        self.term.clear(kind)?;
        Ok(())
    }

    async fn get_size(&mut self) -> wasmtime::Result<Size> {
        Ok(self.term.size()?)
    }

    async fn get_window_size(&mut self) -> wasmtime::Result<WindowSize> {
        Ok(self.term.window_size()?)
    }

    async fn flush(&mut self) -> wasmtime::Result<()> {
        self.term.flush()?;
        Ok(())
    }

    async fn read_events(&mut self) -> wasmtime::Result<Vec<Event>> {
        Ok(self.term.drain_events())
    }

    async fn origin(&mut self) -> wasmtime::Result<Option<String>> {
        Ok(self.term.origin().map(str::to_owned))
    }

    async fn location(&mut self) -> wasmtime::Result<Option<String>> {
        Ok(self.term.location().map(str::to_owned))
    }

    async fn set_title(&mut self, title: String) -> wasmtime::Result<()> {
        self.term.set_title(&title)?;
        Ok(())
    }

    async fn ready(&mut self) -> wasmtime::Result<()> {
        self.term.app_ready();
        Ok(())
    }

    async fn reload(&mut self) -> wasmtime::Result<()> {
        // The runner sees the interrupt reason and starts a new instance;
        // the trap just ends this one without running any more guest code.
        self.interrupter.fire(Interrupt::Reload);
        Err(wasmtime::Error::msg("the app asked to be reloaded"))
    }

    async fn pending_update(&mut self) -> wasmtime::Result<Option<terminal::Update>> {
        Ok(self.updates.pending().map(update::to_wit))
    }

    async fn update_availability(&mut self) -> wasmtime::Result<terminal::Availability> {
        Ok(self.updates.availability())
    }

    async fn log(
        &mut self,
        level: terminal::LogLevel,
        target: String,
        message: String,
    ) -> wasmtime::Result<()> {
        let level = match level {
            terminal::LogLevel::Trace => crate::LogLevel::Trace,
            terminal::LogLevel::Debug => crate::LogLevel::Debug,
            terminal::LogLevel::Info => crate::LogLevel::Info,
            terminal::LogLevel::Warn => crate::LogLevel::Warn,
            terminal::LogLevel::Error => crate::LogLevel::Error,
        };
        self.term.log(level, &target, &message, &self.limits);
        Ok(())
    }
}

impl storage::Host for HostState {
    async fn get(&mut self, key: String) -> wasmtime::Result<Option<Vec<u8>>> {
        Ok(self.storage.get(&key))
    }

    async fn set(
        &mut self,
        key: String,
        value: Vec<u8>,
    ) -> wasmtime::Result<Result<(), StorageError>> {
        Ok(self.storage.set(key, value))
    }

    async fn remove(&mut self, key: String) -> wasmtime::Result<()> {
        self.storage.remove(&key);
        Ok(())
    }

    async fn keys(&mut self) -> wasmtime::Result<Vec<String>> {
        Ok(self.storage.keys())
    }

    async fn clear(&mut self) -> wasmtime::Result<()> {
        self.storage.clear();
        Ok(())
    }

    async fn usage(&mut self) -> wasmtime::Result<(u64, u64)> {
        Ok(self.storage.usage())
    }
}

impl<U> terminal::HostWithStore<U> for HasSelf<HostState> {
    async fn next_event(store: &Accessor<U, Self>) -> wasmtime::Result<Event> {
        let queue = store.with(|mut view| view.get().term.queue());
        let event = queue.next().await;
        store.with(|mut view| view.get().term.note_event());
        Ok(event)
    }

    /// A fetch can take seconds: it runs without the store held, so the app
    /// keeps drawing and reading input meanwhile.
    async fn check_update(
        store: &Accessor<U, Self>,
    ) -> wasmtime::Result<Result<Option<terminal::Update>, String>> {
        let (updates, message_bytes) = store.with(|mut view| {
            let state = view.get();
            (state.updates.clone(), state.limits.message_bytes)
        });
        Ok(match updates.check().await {
            Ok(info) => Ok(info.map(update::to_wit)),
            Err(err) => Err(crate::runner::truncate(
                crate::sanitize::text(&format!("{err:#}")),
                message_bytes,
            )),
        })
    }
}

impl websocket::Host for HostState {}

impl websocket::HostSocket for HostState {
    async fn close(&mut self, this: Resource<WsSocket>) -> wasmtime::Result<()> {
        self.table.get(&this)?.close();
        Ok(())
    }

    async fn drop(&mut self, this: Resource<WsSocket>) -> wasmtime::Result<()> {
        // The slot permit and the task's abort handle go with the resource.
        self.table.delete(this)?;
        Ok(())
    }
}

impl<U> websocket::HostSocketWithStore<U> for HasSelf<HostState> {
    async fn connect(
        store: &Accessor<U, Self>,
        url: String,
    ) -> wasmtime::Result<Result<Resource<WsSocket>, websocket::Error>> {
        // A slot is a semaphore permit held by the socket resource: it is
        // released whether the attempt is cancelled, fails, cannot be stored,
        // or the resource is dropped normally.
        let (slot, policy, cookies, request_policy, on_phase, limits, tasks) =
            store.with(|mut view| {
                let state = view.get();
                (
                    state.websocket_slots.clone().try_acquire_owned().ok(),
                    state.policy.clone(),
                    state.cookies.clone(),
                    state.request_policy.clone(),
                    state.on_phase.clone(),
                    state.limits.clone(),
                    state.websocket_tasks.clone(),
                )
            });
        let Some(slot) = slot else {
            if let Some(hook) = &on_phase {
                hook(Phase::RequestDenied {
                    url: url.clone(),
                    reason: format!("websocket limit of {} reached", limits.websockets),
                });
            }
            return Ok(Err(websocket::Error::Denied));
        };
        let connected = WsSocket::connect(
            &url,
            &policy,
            cookies.as_ref(),
            request_policy.as_ref(),
            on_phase.as_ref(),
            &limits,
            slot,
            &tasks,
        )
        .await;
        match connected {
            Ok(socket) => Ok(Ok(store.with(|mut view| view.get().table.push(socket))?)),
            Err(err) => Ok(Err(err)),
        }
    }

    async fn send(
        store: &Accessor<U, Self>,
        this: Resource<WsSocket>,
        message: websocket::Message,
    ) -> wasmtime::Result<Result<(), websocket::Error>> {
        let shared = store.with(|mut view| {
            let socket = view.get().table.get(&this)?;
            Ok::<_, wasmtime::Error>(socket.check_size(&message).map(|()| socket.shared()))
        })?;
        match shared {
            Ok(shared) => Ok(shared.send(message).await),
            Err(err) => Ok(Err(err)),
        }
    }

    async fn receive(
        store: &Accessor<U, Self>,
        this: Resource<WsSocket>,
    ) -> wasmtime::Result<Result<websocket::Message, websocket::Error>> {
        let shared = store.with(|mut view| view.get().table.get(&this).map(WsSocket::shared))?;
        Ok(shared.next_message().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_limit_is_aggregate_across_memories() {
        let limits = Limits {
            memory_bytes: 100,
            ..Limits::default()
        };
        let mut limiter = AggregateLimiter::new(&limits);
        assert!(limiter.memory_growing(0, 60, None).unwrap());
        // A second memory: 60 in use elsewhere, 50 more would exceed 100.
        assert!(limiter.memory_growing(0, 50, None).is_err());
        assert!(limiter.memory_growing(0, 40, None).unwrap());
        // The first memory growing from 60 to 70: 40 + 70 > 100.
        assert!(limiter.memory_growing(60, 70, None).is_err());
        assert_eq!(limiter.memory_peak(), 100);
    }
}
