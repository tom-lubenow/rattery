//! The real terminal: crossterm for input and ratatui's crossterm backend
//! for output, plus the pollable that lets the guest wait for input.

use std::collections::VecDeque;
use std::io::{self, Stdout, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, EventStream, KeyCode, KeyModifiers,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use futures::StreamExt;
use ratatui::backend::{Backend, CrosstermBackend};
use tokio::sync::{Notify, oneshot};
use wasmtime::component::{Resource, ResourceTable};
use wasmtime_wasi::p2::{DynPollable, Pollable, subscribe};

use crate::bindings::terminal::{CellUpdate, ClearType, Event, Position, Size, WindowSize};
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
        let restore_mouse = mouse;
        std::panic::set_hook(Box::new(move |info| {
            restore(restore_mouse);
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

/// Events queued for the guest, plus a notifier so a pollable can wait on it.
#[derive(Default)]
pub struct EventQueue {
    events: Mutex<VecDeque<Event>>,
    notify: Notify,
}

impl EventQueue {
    fn push(&self, event: Event) {
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

pub struct TerminalHost {
    backend: CrosstermBackend<Stdout>,
    queue: Arc<EventQueue>,
    origin: Option<String>,
}

impl TerminalHost {
    /// Start reading terminal input. Returns the host and a receiver that
    /// fires when the user hits the kill chord (Ctrl-C three times).
    pub fn start(origin: Option<String>) -> (Self, oneshot::Receiver<()>) {
        let queue = Arc::new(EventQueue::default());
        let (kill_tx, kill_rx) = oneshot::channel();
        tokio::spawn(read_input(queue.clone(), kill_tx));
        let host = Self {
            backend: CrosstermBackend::new(io::stdout()),
            queue,
            origin,
        };
        (host, kill_rx)
    }

    pub fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    pub fn draw(&mut self, updates: &[CellUpdate]) -> io::Result<()> {
        let cells: Vec<(u16, u16, ratatui::buffer::Cell)> = updates
            .iter()
            .map(|u| (u.x, u.y, convert::cell(&u.cell)))
            .collect();
        self.backend.draw(cells.iter().map(|(x, y, c)| (*x, *y, c)))
    }

    pub fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.backend.append_lines(n)
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()
    }

    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.backend.show_cursor()
    }

    pub fn cursor_position(&mut self) -> io::Result<Position> {
        let p = self.backend.get_cursor_position()?;
        Ok(Position { x: p.x, y: p.y })
    }

    pub fn set_cursor_position(&mut self, pos: Position) -> io::Result<()> {
        self.backend
            .set_cursor_position(ratatui::layout::Position::new(pos.x, pos.y))
    }

    pub fn clear(&mut self, kind: ClearType) -> io::Result<()> {
        self.backend.clear_region(convert::clear_type(kind))
    }

    pub fn size(&self) -> io::Result<Size> {
        let s = self.backend.size()?;
        Ok(Size {
            width: s.width,
            height: s.height,
        })
    }

    pub fn window_size(&mut self) -> io::Result<WindowSize> {
        let w = self.backend.window_size()?;
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
        Backend::flush(&mut self.backend)
    }

    pub fn set_title(&mut self, title: &str) -> io::Result<()> {
        let mut out = io::stdout();
        queue!(out, SetTitle(title))?;
        out.flush()
    }

    pub fn subscribe(&self, table: &mut ResourceTable) -> wasmtime::Result<Resource<DynPollable>> {
        let ready = table.push(EventsReady(self.queue.clone()))?;
        subscribe(table, ready)
    }

    pub fn drain_events(&self) -> Vec<Event> {
        self.queue.drain()
    }
}

async fn read_input(queue: Arc<EventQueue>, kill: oneshot::Sender<()>) {
    let mut stream = EventStream::new();
    let mut kill = Some(kill);
    let mut ctrl_c_presses: VecDeque<Instant> = VecDeque::new();

    while let Some(item) = stream.next().await {
        let Ok(event) = item else { break };

        if is_ctrl_c(&event) {
            let now = Instant::now();
            ctrl_c_presses.push_back(now);
            while ctrl_c_presses
                .front()
                .is_some_and(|t| now.duration_since(*t) > KILL_WINDOW)
            {
                ctrl_c_presses.pop_front();
            }
            if ctrl_c_presses.len() >= KILL_PRESSES
                && let Some(kill) = kill.take()
            {
                let _ = kill.send(());
            }
        }

        if let Some(event) = convert::event(event) {
            queue.push(event);
        }
    }
}

fn is_ctrl_c(event: &crossterm::event::Event) -> bool {
    matches!(
        event,
        crossterm::event::Event::Key(key)
            if key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && key.kind != crossterm::event::KeyEventKind::Release
    )
}
