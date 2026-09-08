//! The `Store` data: WASI contexts plus the terminal, and the host
//! implementation of `rattery:tui/terminal`.

use wasmtime::component::{Resource, ResourceTable};
use wasmtime_wasi::p2::bindings::io::poll::Pollable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use crate::bindings::terminal::{CellUpdate, ClearType, Event, Host, Position, Size, WindowSize};
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
}

impl HostState {
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

impl Host for HostState {
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

    async fn subscribe_events(&mut self) -> wasmtime::Result<Resource<Pollable>> {
        self.term.subscribe(&mut self.table)
    }

    async fn read_events(&mut self) -> wasmtime::Result<Vec<Event>> {
        Ok(self.term.drain_events())
    }

    async fn origin(&mut self) -> wasmtime::Result<Option<String>> {
        Ok(self.term.origin().map(str::to_owned))
    }

    async fn set_title(&mut self, title: String) -> wasmtime::Result<()> {
        self.term.set_title(&title)?;
        Ok(())
    }
}

impl websocket::Host for HostState {}

impl websocket::HostSocket for HostState {
    async fn connect(&mut self, url: String) -> wasmtime::Result<Resource<WsSocket>> {
        let socket = WsSocket::connect(&url, &self.policy, self.cookies.as_ref());
        Ok(self.table.push(socket)?)
    }

    async fn subscribe(
        &mut self,
        this: Resource<WsSocket>,
    ) -> wasmtime::Result<Resource<Pollable>> {
        WsSocket::subscribe(&mut self.table, &this)
    }

    async fn is_open(&mut self, this: Resource<WsSocket>) -> wasmtime::Result<bool> {
        Ok(self.table.get(&this)?.is_open())
    }

    async fn receive(
        &mut self,
        this: Resource<WsSocket>,
    ) -> wasmtime::Result<Result<Option<websocket::Message>, websocket::Error>> {
        Ok(self.table.get(&this)?.receive())
    }

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
