//! The terminal the app renders into: crossterm and stdout when interactive,
//! an in-memory `TestBackend` when headless. Also the event queue, the
//! pollable that lets the guest wait for input, and the interrupt machinery.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Stdout, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, EventStream,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use futures::StreamExt;
use ratatui::backend::{Backend, CrosstermBackend, TestBackend};
use tokio::sync::Notify;
use wasmtime::Engine;
use wasmtime::component::{Resource, ResourceTable};
use wasmtime_wasi::p2::{DynPollable, Pollable, subscribe};

use crate::bindings::terminal::{
    CellUpdate, ClearType, Event, KeyCode, KeyEventKind, KeyModifiers, Position, Size, WindowSize,
};
use crate::convert;

const KILL_PRESSES: usize = 3;
const KILL_WINDOW: Duration = Duration::from_millis(1500);

/// Raw mode, alternate screen, and input reporting. Restored on drop, even on
/// panic, so a misbehaving app never leaves the shell unusable.
pub struct Session {
    mouse: bool,
}

impl Session {
    pub fn enter(mouse: bool) -> Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        execute!(
            out,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableFocusChange
        )?;
        if mouse {
            execute!(out, EnableMouseCapture)?;
        }
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore(mouse);
            previous(info);
        }));
        Ok(Self { mouse })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        restore(self.mouse);
    }
}

fn restore(mouse: bool) {
    let mut out = io::stdout();
    if mouse {
        let _ = execute!(out, DisableMouseCapture);
    }
    let _ = execute!(
        out,
        DisableFocusChange,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
}

/// Why the host stopped the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    /// Ctrl-C three times in a row.
    Kill,
    /// The headless timeout elapsed.
    Timeout,
    /// A new version of the component is available.
    Reload,
}

/// Stops a running app from anywhere: an epoch bump traps an app busy in
/// wasm, and a notification unblocks the host if the app is waiting inside a
/// host call.
pub struct Interrupter {
    engine: Engine,
    reason: Mutex<Option<Interrupt>>,
    notify: Notify,
}

impl Interrupter {
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            reason: Mutex::new(None),
            notify: Notify::new(),
        }
    }

    pub fn fire(&self, reason: Interrupt) {
        let mut slot = self.reason.lock().unwrap();
        if slot.is_none() {
            *slot = Some(reason);
        }
        drop(slot);
        self.engine.increment_epoch();
        self.notify.notify_one();
    }

    /// The pending reason, cleared so the next run starts fresh.
    pub fn take_reason(&self) -> Option<Interrupt> {
        self.reason.lock().unwrap().take()
    }

    pub async fn notified(&self) {
        self.notify.notified().await
    }
}

/// Detects the kill chord in the input stream.
struct KillDetector {
    presses: VecDeque<Instant>,
    interrupter: Arc<Interrupter>,
}

impl KillDetector {
    fn observe(&mut self, event: &Event) {
        let is_ctrl_c = matches!(
            event,
            Event::Key(key)
                if key.code == KeyCode::Character('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.kind != KeyEventKind::Release
        );
        if !is_ctrl_c {
            return;
        }
        let now = Instant::now();
        self.presses.push_back(now);
        while self
            .presses
            .front()
            .is_some_and(|t| now.duration_since(*t) > KILL_WINDOW)
        {
            self.presses.pop_front();
        }
        if self.presses.len() >= KILL_PRESSES {
            self.interrupter.fire(Interrupt::Kill);
        }
    }
}

/// Events queued for the guest, plus a notifier so a pollable can wait on it.
pub struct EventQueue {
    events: Mutex<VecDeque<Event>>,
    notify: Notify,
    kill: Mutex<KillDetector>,
}

impl EventQueue {
    fn new(interrupter: Arc<Interrupter>) -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            kill: Mutex::new(KillDetector {
                presses: VecDeque::new(),
                interrupter,
            }),
        }
    }

    pub fn push(&self, event: Event) {
        self.kill.lock().unwrap().observe(&event);
        self.events.lock().unwrap().push_back(event);
        self.notify.notify_one();
    }

    fn is_empty(&self) -> bool {
        self.events.lock().unwrap().is_empty()
    }

    fn drain(&self) -> Vec<Event> {
        self.events.lock().unwrap().drain(..).collect()
    }
}

/// The `wasi:io/poll.pollable` behind `terminal.subscribe-events`.
struct EventsReady(Arc<EventQueue>);

#[async_trait::async_trait]
impl Pollable for EventsReady {
    async fn ready(&mut self) {
        loop {
            let notified = self.0.notify.notified();
            if !self.0.is_empty() {
                return;
            }
            notified.await;
        }
    }
}

/// The text of a headless screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    pub width: u16,
    pub height: u16,
    /// One string per row, trailing spaces trimmed.
    pub lines: Vec<String>,
}

impl Screen {
    pub fn from_backend(backend: &TestBackend) -> Self {
        let buffer = backend.buffer();
        let area = buffer.area;
        let lines = (0..area.height)
            .map(|y| {
                let mut line = String::new();
                for x in 0..area.width {
                    if let Some(cell) = buffer.cell((x, y)) {
                        line.push_str(cell.symbol());
                    }
                }
                line.trim_end().to_owned()
            })
            .collect();
        Self {
            width: area.width,
            height: area.height,
            lines,
        }
    }

    /// True if any row contains `needle`.
    pub fn contains(&self, needle: &str) -> bool {
        self.lines.iter().any(|line| line.contains(needle))
    }

    /// The rows joined with newlines.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

impl fmt::Display for Screen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for line in &self.lines {
            writeln!(f, "{line}")?;
        }
        Ok(())
    }
}

enum Output {
    Crossterm(CrosstermBackend<Stdout>),
    Test(Arc<Mutex<TestBackend>>),
}

macro_rules! with_backend {
    ($self:ident, |$b:ident| $body:expr) => {
        match &mut $self.output {
            Output::Crossterm($b) => $body,
            Output::Test(shared) => {
                let mut guard = shared.lock().unwrap();
                let $b = &mut *guard;
                $body.map_err(|e| io::Error::other(e.to_string()))
            }
        }
    };
}

pub struct TerminalHost {
    output: Output,
    queue: Arc<EventQueue>,
    origin: Option<String>,
    snapshots: Arc<Mutex<Vec<Screen>>>,
}

impl TerminalHost {
    /// Interactive: crossterm on stdout, input read from stdin on a task.
    pub fn interactive(origin: Option<String>, interrupter: Arc<Interrupter>) -> Self {
        let queue = Arc::new(EventQueue::new(interrupter));
        tokio::spawn(read_input(queue.clone()));
        Self {
            output: Output::Crossterm(CrosstermBackend::new(io::stdout())),
            queue,
            origin,
            snapshots: Arc::default(),
        }
    }

    /// Headless: an in-memory screen, input from a script.
    pub fn headless(
        origin: Option<String>,
        interrupter: Arc<Interrupter>,
        width: u16,
        height: u16,
    ) -> Self {
        Self {
            output: Output::Test(Arc::new(Mutex::new(TestBackend::new(width, height)))),
            queue: Arc::new(EventQueue::new(interrupter)),
            origin,
            snapshots: Arc::default(),
        }
    }

    pub fn queue(&self) -> Arc<EventQueue> {
        self.queue.clone()
    }

    /// The in-memory screen, when headless.
    pub fn test_backend(&self) -> Option<Arc<Mutex<TestBackend>>> {
        match &self.output {
            Output::Test(shared) => Some(shared.clone()),
            Output::Crossterm(_) => None,
        }
    }

    pub fn snapshots(&self) -> Arc<Mutex<Vec<Screen>>> {
        self.snapshots.clone()
    }

    pub fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    pub fn draw(&mut self, updates: &[CellUpdate]) -> io::Result<()> {
        let cells: Vec<(u16, u16, ratatui::buffer::Cell)> = updates
            .iter()
            .map(|u| (u.x, u.y, convert::cell(&u.cell)))
            .collect();
        with_backend!(self, |b| b.draw(cells.iter().map(|(x, y, c)| (*x, *y, c))))
    }

    pub fn append_lines(&mut self, n: u16) -> io::Result<()> {
        with_backend!(self, |b| b.append_lines(n))
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        with_backend!(self, |b| b.hide_cursor())
    }

    pub fn show_cursor(&mut self) -> io::Result<()> {
        with_backend!(self, |b| b.show_cursor())
    }

    pub fn cursor_position(&mut self) -> io::Result<Position> {
        let p = with_backend!(self, |b| b.get_cursor_position())?;
        Ok(Position { x: p.x, y: p.y })
    }

    pub fn set_cursor_position(&mut self, pos: Position) -> io::Result<()> {
        let p = ratatui::layout::Position::new(pos.x, pos.y);
        with_backend!(self, |b| b.set_cursor_position(p))
    }

    pub fn clear(&mut self, kind: ClearType) -> io::Result<()> {
        let kind = convert::clear_type(kind);
        with_backend!(self, |b| b.clear_region(kind))
    }

    pub fn size(&mut self) -> io::Result<Size> {
        let s = with_backend!(self, |b| b.size())?;
        Ok(Size {
            width: s.width,
            height: s.height,
        })
    }

    pub fn window_size(&mut self) -> io::Result<WindowSize> {
        let w = with_backend!(self, |b| b.window_size())?;
        Ok(WindowSize {
            columns_rows: Size {
                width: w.columns_rows.width,
                height: w.columns_rows.height,
            },
            pixels: Size {
                width: w.pixels.width,
                height: w.pixels.height,
            },
        })
    }

    pub fn flush(&mut self) -> io::Result<()> {
        with_backend!(self, |b| Backend::flush(b))
    }

    /// Wipe the screen between two apps sharing the terminal.
    pub fn reset(&mut self) -> io::Result<()> {
        with_backend!(self, |b| b.clear())?;
        self.flush()
    }

    pub fn set_title(&mut self, title: &str) -> io::Result<()> {
        if let Output::Crossterm(_) = self.output {
            let mut out = io::stdout();
            queue!(out, SetTitle(title))?;
            out.flush()?;
        }
        Ok(())
    }

    pub fn subscribe(&self, table: &mut ResourceTable) -> wasmtime::Result<Resource<DynPollable>> {
        let ready = table.push(EventsReady(self.queue.clone()))?;
        subscribe(table, ready)
    }

    pub fn drain_events(&self) -> Vec<Event> {
        self.queue.drain()
    }
}

async fn read_input(queue: Arc<EventQueue>) {
    let mut stream = EventStream::new();
    while let Some(item) = stream.next().await {
        let Ok(event) = item else { break };
        if let Some(event) = convert::event(event) {
            queue.push(event);
        }
    }
}
