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

use wasmtime::StoreLimits;
use wasmtime::component::{Accessor, HasSelf, Resource, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use crate::bindings::terminal::{self, CellUpdate, ClearType, Event, Position, Size, WindowSize};
use crate::bindings::websocket;
use crate::http::{CookieJar, OriginHooks, OriginPolicy, RequestPolicy};
use crate::terminal::{Interrupt, Interrupter, PhaseHook, TerminalHost};
use crate::websocket::WsSocket;
use crate::{Limits, Phase};

/// The CPU budget, counted in epoch ticks while the guest executes.
pub struct CpuBudget {
    pub ticks: u64,
    pub budget_ticks: Option<u64>,
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
    limits: Limits,
    store_limits: StoreLimits,
    cpu: CpuBudget,
    interrupter: Arc<Interrupter>,
    on_phase: Option<PhaseHook>,
    websockets: usize,
    ext: HashMap<TypeId, Box<dyn Any + Send>>,
}

pub struct HostStateConfig {
    pub wasi: WasiCtx,
    pub policy: OriginPolicy,
    pub cookies: Option<CookieJar>,
    pub request_policy: Option<Arc<dyn RequestPolicy>>,
    pub term: TerminalHost,
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
            limits,
            interrupter,
            on_phase,
            ext,
        } = config;
        let store_limits = wasmtime::StoreLimitsBuilder::new()
            .memory_size(limits.memory_bytes)
            .table_elements(limits.table_elements)
            .tables(limits.tables)
            .memories(limits.memories)
            .instances(limits.instances)
            .trap_on_grow_failure(true)
            .build();
        let budget_ticks = limits
            .cpu_time
            .map(|d| (d.as_nanos() / crate::runner::EPOCH_TICK.as_nanos()).max(1) as u64);
        Self {
            table: ResourceTable::new(),
            wasi,
            http: WasiHttpCtx::new(),
            hooks: OriginHooks::new(
                policy.clone(),
                cookies.clone(),
                request_policy.clone(),
                &limits,
                on_phase.clone(),
            ),
            policy,
            cookies,
            request_policy,
            term,
            limits,
            store_limits,
            cpu: CpuBudget {
                ticks: 0,
                budget_ticks,
            },
            interrupter,
            on_phase,
            websockets: 0,
            ext,
        }
    }

    pub fn into_terminal(self) -> TerminalHost {
        self.term
    }

    /// The terminal and the extension state, to carry across a reload.
    pub(crate) fn into_parts(self) -> (TerminalHost, HashMap<TypeId, Box<dyn Any + Send>>) {
        (self.term, self.ext)
    }

    pub(crate) fn store_limits(&mut self) -> &mut StoreLimits {
        &mut self.store_limits
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
}

impl<U> terminal::HostWithStore<U> for HasSelf<HostState> {
    async fn next_event(store: &Accessor<U, Self>) -> wasmtime::Result<Event> {
        let queue = store.with(|mut view| view.get().term.queue());
        let event = queue.next().await;
        store.with(|mut view| view.get().term.note_event());
        Ok(event)
    }
}

impl websocket::Host for HostState {}

impl websocket::HostSocket for HostState {
    async fn send(
        &mut self,
        this: Resource<WsSocket>,
        message: websocket::Message,
    ) -> wasmtime::Result<Result<(), websocket::Error>> {
        Ok(self.table.get(&this)?.send(message))
    }

    async fn close(&mut self, this: Resource<WsSocket>) -> wasmtime::Result<()> {
        self.table.get(&this)?.close();
        Ok(())
    }

    async fn drop(&mut self, this: Resource<WsSocket>) -> wasmtime::Result<()> {
        self.table.delete(this)?;
        self.websockets = self.websockets.saturating_sub(1);
        Ok(())
    }
}

impl<U> websocket::HostSocketWithStore<U> for HasSelf<HostState> {
    async fn connect(
        store: &Accessor<U, Self>,
        url: String,
    ) -> wasmtime::Result<Result<Resource<WsSocket>, websocket::Error>> {
        let (policy, cookies, request_policy, on_phase, limits, open) = store.with(|mut view| {
            let state = view.get();
            (
                state.policy.clone(),
                state.cookies.clone(),
                state.request_policy.clone(),
                state.on_phase.clone(),
                state.limits.clone(),
                state.websockets,
            )
        });
        if open >= limits.websockets {
            if let Some(hook) = &on_phase {
                hook(Phase::RequestDenied {
                    url: url.clone(),
                    reason: format!("websocket limit of {} reached", limits.websockets),
                });
            }
            return Ok(Err(websocket::Error::Denied));
        }
        match WsSocket::connect(
            &url,
            &policy,
            cookies.as_ref(),
            request_policy.as_ref(),
            on_phase.as_ref(),
            &limits,
        )
        .await
        {
            Ok(socket) => Ok(Ok(store.with(|mut view| {
                let state = view.get();
                state.websockets += 1;
                state.table.push(socket)
            })?)),
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
