//! The `Store` data: WASI contexts plus the terminal, and the host
//! implementation of `rattery:tui/terminal` and `rattery:tui/websocket`.
//!
//! Synchronous WIT functions land in the `Host` traits and take `&mut self`.
//! The `async func`s (`next-event`, websocket `connect` and `receive`) land in
//! the `HostWithStore` traits: they run concurrently with the guest and reach
//! the store through an `Accessor` only when they need it.

use wasmtime::component::{Accessor, HasSelf, Resource, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use crate::bindings::terminal::{self, CellUpdate, ClearType, Event, Position, Size, WindowSize};
use crate::bindings::websocket;
use crate::http::{CookieJar, OriginHooks, OriginPolicy};
use crate::terminal::TerminalHost;
use crate::websocket::WsSocket;

pub struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: OriginHooks,
    policy: OriginPolicy,
    cookies: Option<CookieJar>,
    term: TerminalHost,
}

impl HostState {
    pub fn new(
        wasi: WasiCtx,
        policy: OriginPolicy,
        cookies: Option<CookieJar>,
        term: TerminalHost,
    ) -> Self {
        Self {
            table: ResourceTable::new(),
            wasi,
            http: WasiHttpCtx::new(),
            hooks: OriginHooks::new(policy.clone(), cookies.clone()),
            policy,
            cookies,
            term,
        }
    }

    pub fn into_terminal(self) -> TerminalHost {
        self.term
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
        Ok(())
    }
}

impl<U> websocket::HostSocketWithStore<U> for HasSelf<HostState> {
    async fn connect(
        store: &Accessor<U, Self>,
        url: String,
    ) -> wasmtime::Result<Result<Resource<WsSocket>, websocket::Error>> {
        let (policy, cookies) =
            store.with(|mut view| (view.get().policy.clone(), view.get().cookies.clone()));
        match WsSocket::connect(&url, &policy, cookies.as_ref()).await {
            Ok(socket) => Ok(Ok(store.with(|mut view| view.get().table.push(socket))?)),
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
