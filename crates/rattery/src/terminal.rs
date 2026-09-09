//! The terminal the app renders into: crossterm and stdout when interactive,
//! an in-memory `TestBackend` when headless. Also the event queue, the
//! interrupt machinery, and the containment of everything the guest sends
//! toward the terminal.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Stdout, Write};
use std::panic::PanicHookInfo;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
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
use tokio::task::JoinHandle;

use crate::bindings::terminal::{
    CellUpdate, ClearType, Event, KeyCode, KeyEventKind, KeyModifiers, Position, Size, WindowSize,
};
use crate::{Phase, convert, sanitize};

const KILL_PRESSES: usize = 3;
const KILL_WINDOW: Duration = Duration::from_millis(1500);

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

/// Where a session parks the panic hook it replaced. Shared with the runner
/// so the hook can be put back even if the session was destroyed by a panic.
pub type HookSlot = Arc<Mutex<Option<PanicHook>>>;

/// Raw mode, alternate screen, and input reporting. Every step is undone if a
/// later one fails, and everything is restored on drop, even on panic, so a
/// misbehaving app never leaves the shell unusable. The panic hook installed
/// for that is the previous hook wrapped, and is put back on drop.
pub struct Session {
    steps: Vec<Step>,
    previous_hook: HookSlot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    RawMode,
    AlternateScreen,
    BracketedPaste,
    FocusChange,
    MouseCapture,
}

impl Session {
    pub fn enter(mouse: bool, previous_hook: HookSlot) -> Result<Self> {
        let mut steps = Vec::new();
        let mut out = io::stdout();
        let attempt = (|| -> Result<()> {
            enable_raw_mode().context("enabling raw mode")?;
            steps.push(Step::RawMode);
            execute!(out, EnterAlternateScreen).context("entering the alternate screen")?;
            steps.push(Step::AlternateScreen);
            execute!(out, EnableBracketedPaste).context("enabling bracketed paste")?;
            steps.push(Step::BracketedPaste);
            execute!(out, EnableFocusChange).context("enabling focus reporting")?;
            steps.push(Step::FocusChange);
            if mouse {
                execute!(out, EnableMouseCapture).context("enabling mouse capture")?;
                steps.push(Step::MouseCapture);
            }
            Ok(())
        })();
        if let Err(err) = attempt {
            undo(&steps);
            return Err(err.context("failed to set up the terminal"));
        }

        // Wrap the current panic hook so a panic restores the terminal first.
        *previous_hook.lock().unwrap() = Some(std::panic::take_hook());
        let hook_steps = steps.clone();
        let hook_previous = previous_hook.clone();
        std::panic::set_hook(Box::new(move |info| {
            undo(&hook_steps);
            if let Ok(guard) = hook_previous.lock()
                && let Some(previous) = guard.as_ref()
            {
                previous(info);
            }
        }));
        Ok(Self {
            steps,
            previous_hook,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        undo(&self.steps);
        // Put the previous hook back; skip if a panic is in flight, since
        // set_hook would itself panic then.
        if !std::thread::panicking()
            && let Some(previous) = self.previous_hook.lock().ok().and_then(|mut g| g.take())
        {
            std::panic::set_hook(previous);
        }
    }
}

fn undo(steps: &[Step]) {
    let mut out = io::stdout();
    for step in steps.iter().rev() {
        match step {
            Step::MouseCapture => {
                let _ = execute!(out, DisableMouseCapture);
            }
            Step::FocusChange => {
                let _ = execute!(out, DisableFocusChange);
            }
            Step::BracketedPaste => {
                let _ = execute!(out, DisableBracketedPaste);
            }
            Step::AlternateScreen => {
                let _ = execute!(out, LeaveAlternateScreen);
            }
            Step::RawMode => {
                let _ = disable_raw_mode();
            }
        }
    }
}

/// Why the host stopped the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Interrupt {
    /// Ctrl-C three times in a row.
    Kill,
    /// The headless timeout elapsed.
    Timeout,
    /// A new version of the component is available.
    Reload,
    /// A resource limit was exceeded.
    Limit(String),
}

/// Stops a running app from anywhere. The reason is checked by the epoch
/// callback on the next tick, which traps an app busy in wasm; the
/// notification unblocks the host if the app is waiting inside a host call.
pub struct Interrupter {
    reason: Mutex<Option<Interrupt>>,
    notify: Notify,
}

impl Interrupter {
    pub fn new() -> Self {
        Self {
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
        self.notify.notify_one();
    }

    pub fn is_fired(&self) -> bool {
        self.reason.lock().unwrap().is_some()
    }

    /// The pending reason, cleared so the next run starts fresh.
    pub fn take_reason(&self) -> Option<Interrupt> {
        self.reason.lock().unwrap().take()
    }

    pub async fn notified(&self) {
        self.notify.notified().await
    }
}

/// Background tasks that belong to one run, cancelled and awaited on exit.
#[derive(Default)]
pub struct Tasks {
    handles: Vec<JoinHandle<()>>,
}

impl Tasks {
    pub fn spawn(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        self.handles.push(tokio::spawn(future));
    }

    pub async fn shutdown(mut self) {
        let handles = std::mem::take(&mut self.handles);
        for handle in &handles {
            handle.abort();
        }
        for handle in handles {
            let _ = handle.await;
        }
    }
}

/// If the run is abandoned (the future dropped, a timeout, a panic), the
/// tasks are cancelled rather than detached.
impl Drop for Tasks {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
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

/// Events queued for the guest, bounded: when the app does not read, the
/// oldest events are dropped rather than the queue growing.
pub struct EventQueue {
    events: Mutex<VecDeque<Event>>,
    capacity: usize,
    paste_bytes: usize,
    notify: Notify,
    kill: Mutex<KillDetector>,
}

impl EventQueue {
    fn new(interrupter: Arc<Interrupter>, capacity: usize, paste_bytes: usize) -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
            paste_bytes,
            notify: Notify::new(),
            kill: Mutex::new(KillDetector {
                presses: VecDeque::new(),
                interrupter,
            }),
        }
    }

    pub fn push(&self, mut event: Event) {
        if let Event::Paste(text) = &mut event
            && text.len() > self.paste_bytes
        {
            let mut cut = self.paste_bytes;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        self.kill.lock().unwrap().observe(&event);
        let mut events = self.events.lock().unwrap();
        if events.len() >= self.capacity {
            events.pop_front();
        }
        events.push_back(event);
        drop(events);
        self.notify.notify_one();
    }

    fn drain(&self) -> Vec<Event> {
        self.events.lock().unwrap().drain(..).collect()
    }

    /// Wait for the next event. Backs `terminal.next-event`.
    pub async fn next(&self) -> Event {
        loop {
            let notified = self.notify.notified();
            if let Some(event) = self.events.lock().unwrap().pop_front() {
                return event;
            }
            notified.await;
        }
    }
}

/// Counters the host keeps about the app's use of the terminal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// `draw` calls.
    pub draws: u64,
    /// Cells carried by all `draw` calls together.
    pub cells: u64,
    /// Cells refused: outside the screen, or replaced for containing
    /// control characters or malformed symbols.
    pub cells_rejected: u64,
    /// Time spent inside `draw` on the host, including terminal output.
    pub draw_time: Duration,
    /// `flush` calls.
    pub flushes: u64,
    /// Time spent inside `flush` on the host.
    pub flush_time: Duration,
    /// Events handed to the app.
    pub events: u64,
    /// When the first `draw` arrived, relative to the app starting.
    pub first_draw: Option<Duration>,
    /// The most linear memory the app had in use at once, in bytes.
    pub memory_peak: usize,
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

pub type PhaseHook = Arc<dyn Fn(Phase) + Send + Sync>;

pub struct TerminalHost {
    output: Output,
    queue: Arc<EventQueue>,
    origin: Option<String>,
    location: Option<String>,
    snapshots: Arc<Mutex<Vec<Screen>>>,
    stats: Stats,
    started: Instant,
    on_phase: Option<PhaseHook>,
    ready_reported: bool,
}

impl TerminalHost {
    /// Interactive: crossterm on stdout, input read from stdin on a task.
    pub fn interactive(
        origin: Option<String>,
        location: Option<String>,
        interrupter: Arc<Interrupter>,
        queue_capacity: usize,
        paste_bytes: usize,
        tasks: &mut Tasks,
    ) -> Self {
        let queue = Arc::new(EventQueue::new(interrupter, queue_capacity, paste_bytes));
        tasks.spawn(read_input(queue.clone()));
        Self {
            output: Output::Crossterm(CrosstermBackend::new(io::stdout())),
            queue,
            origin,
            location,
            snapshots: Arc::default(),
            stats: Stats::default(),
            started: Instant::now(),
            on_phase: None,
            ready_reported: false,
        }
    }

    /// Headless: an in-memory screen, input from a script.
    pub fn headless(
        origin: Option<String>,
        location: Option<String>,
        interrupter: Arc<Interrupter>,
        queue_capacity: usize,
        paste_bytes: usize,
        width: u16,
        height: u16,
    ) -> Self {
        Self {
            output: Output::Test(Arc::new(Mutex::new(TestBackend::new(width, height)))),
            queue: Arc::new(EventQueue::new(interrupter, queue_capacity, paste_bytes)),
            origin,
            location,
            snapshots: Arc::default(),
            stats: Stats::default(),
            started: Instant::now(),
            on_phase: None,
            ready_reported: false,
        }
    }

    pub fn set_phase_hook(&mut self, hook: Option<PhaseHook>) {
        self.on_phase = hook;
    }

    /// Counters since the terminal was opened.
    pub fn stats(&self) -> Stats {
        self.stats.clone()
    }

    /// Restart the clock behind `Stats::first_draw` (used before each run).
    pub fn mark_started(&mut self) {
        self.started = Instant::now();
        self.stats.first_draw = None;
        self.ready_reported = false;
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

    pub fn location(&self) -> Option<&str> {
        self.location.as_deref()
    }

    /// Draw the cells the guest sent, after containment: cells outside the
    /// screen are dropped, symbols are validated (see [`sanitize::symbol`]).
    pub fn draw(&mut self, updates: &[CellUpdate]) -> io::Result<()> {
        let t = Instant::now();
        let size = with_backend!(self, |b| b.size())?;
        let mut rejected = 0u64;
        let mut cells: Vec<(u16, u16, ratatui::buffer::Cell)> = Vec::with_capacity(updates.len());
        for u in updates {
            if u.x >= size.width || u.y >= size.height {
                rejected += 1;
                continue;
            }
            let symbol = sanitize::symbol(&u.cell.symbol);
            if symbol.as_ref() != u.cell.symbol {
                rejected += 1;
            }
            cells.push((u.x, u.y, convert::cell(&u.cell, &symbol)));
        }
        let result = with_backend!(self, |b| b.draw(cells.iter().map(|(x, y, c)| (*x, *y, c))));
        if result.is_ok() && self.stats.first_draw.is_none() {
            self.stats.first_draw = Some(t.duration_since(self.started));
        }
        self.stats.draws += 1;
        self.stats.cells += updates.len() as u64;
        self.stats.cells_rejected += rejected;
        self.stats.draw_time += t.elapsed();
        result
    }

    /// Scroll by up to one screen; more is pointless and costs output.
    pub fn append_lines(&mut self, n: u16) -> io::Result<()> {
        let size = with_backend!(self, |b| b.size())?;
        let n = n.min(size.height);
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

    /// Move the cursor, clamped to the screen.
    pub fn set_cursor_position(&mut self, pos: Position) -> io::Result<()> {
        let size = with_backend!(self, |b| b.size())?;
        let p = ratatui::layout::Position::new(
            pos.x.min(size.width.saturating_sub(1)),
            pos.y.min(size.height.saturating_sub(1)),
        );
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

    /// Flush the frame. The first successful draw-and-flush is the moment
    /// the app has presented something, which is `Phase::Ready`.
    pub fn flush(&mut self) -> io::Result<()> {
        let t = Instant::now();
        let result = with_backend!(self, |b| Backend::flush(b));
        self.stats.flushes += 1;
        self.stats.flush_time += t.elapsed();
        if result.is_ok() && self.stats.first_draw.is_some() && !self.ready_reported {
            self.ready_reported = true;
            if let Some(hook) = &self.on_phase {
                hook(Phase::Ready);
            }
        }
        result
    }

    /// Wipe the screen between two apps sharing the terminal.
    ///
    /// This is host housekeeping, not a frame from the app, so it goes around
    /// the readiness accounting: a guest interrupted between its first draw
    /// and its flush must not be reported ready by our flush.
    pub fn reset(&mut self) -> io::Result<()> {
        self.stats.first_draw = None;
        self.ready_reported = false;
        with_backend!(self, |b| b.clear())?;
        with_backend!(self, |b| Backend::flush(b))
    }

    /// Set the window title, with control characters removed and the length
    /// bounded (see [`sanitize::title`]).
    pub fn set_title(&mut self, title: &str) -> io::Result<()> {
        if let Output::Crossterm(_) = self.output {
            let title = sanitize::title(title);
            let mut out = io::stdout();
            queue!(out, SetTitle(title))?;
            out.flush()?;
        }
        Ok(())
    }

    pub fn drain_events(&mut self) -> Vec<Event> {
        let events = self.queue.drain();
        self.stats.events += events.len() as u64;
        events
    }

    /// Count an event delivered through `next-event`.
    pub fn note_event(&mut self) {
        self.stats.events += 1;
    }

    /// The app declared itself ready.
    pub fn app_ready(&mut self) {
        if let Some(hook) = &self.on_phase {
            hook(Phase::AppReady);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::terminal::{Cell, Color, Modifier};

    fn host(phases: Arc<Mutex<Vec<Phase>>>) -> TerminalHost {
        let interrupter = Arc::new(Interrupter::new());
        let mut host = TerminalHost::headless(None, None, interrupter, 8, 1024, 10, 3);
        host.set_phase_hook(Some(Arc::new(move |phase| {
            phases.lock().unwrap().push(phase)
        })));
        host
    }

    fn update(x: u16, y: u16) -> CellUpdate {
        CellUpdate {
            x,
            y,
            cell: Cell {
                symbol: "x".into(),
                fg: Color::Reset,
                bg: Color::Reset,
                underline_color: Color::Reset,
                modifier: Modifier::empty(),
            },
        }
    }

    #[test]
    fn ready_needs_a_draw_and_a_flush_and_reset_does_not_count() {
        let phases = Arc::new(Mutex::new(Vec::new()));
        let mut host = host(phases.clone());
        host.draw(&[update(0, 0)]).unwrap();
        assert!(phases.lock().unwrap().is_empty(), "draw alone is not ready");
        // Interrupted before its flush: the host's reset must not count.
        host.reset().unwrap();
        assert!(
            phases.lock().unwrap().is_empty(),
            "reset is not the app's flush"
        );
        host.mark_started();
        host.draw(&[update(1, 1)]).unwrap();
        host.flush().unwrap();
        host.flush().unwrap();
        assert_eq!(
            phases.lock().unwrap().as_slice(),
            [Phase::Ready],
            "ready once, after the flush"
        );
    }

    #[test]
    fn off_screen_and_hostile_cells_are_counted() {
        let phases = Arc::new(Mutex::new(Vec::new()));
        let mut host = host(phases);
        let mut bad = update(0, 0);
        bad.cell.symbol = "\u{1b}[2J".into();
        host.draw(&[bad, update(50, 50), update(2, 2)]).unwrap();
        assert_eq!(host.stats().cells_rejected, 2);
        assert_eq!(host.stats().cells, 3);
    }
}
