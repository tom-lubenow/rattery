//! Reading input events from the host.
//!
//! `terminal.next-event` is an async host function, so waiting for a key is a
//! plain `.await`. Spawned tasks that finish ask for attention through
//! [`Event::Wake`], which [`next`] also delivers.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures::future::Either;

use crate::bindings::terminal as t;
use crate::event::*;
use crate::wasi::task;

type PendingEvent = Pin<Box<dyn Future<Output = t::Event>>>;

thread_local! {
    /// Events drained from the host but not yet handed to the app.
    static BUFFER: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
    /// A host call in flight. Kept across wakes so it is not cancelled and
    /// restarted every time a task finishes.
    static PENDING: RefCell<Option<PendingEvent>> = const { RefCell::new(None) };
}

fn refill() {
    BUFFER.with(|b| {
        b.borrow_mut()
            .extend(t::read_events().into_iter().map(Event::from))
    });
}

fn pop() -> Option<Event> {
    BUFFER.with(|b| b.borrow_mut().pop_front())
}

fn take_pending() -> PendingEvent {
    PENDING
        .with(|p| p.borrow_mut().take())
        .unwrap_or_else(|| Box::pin(t::next_event()))
}

/// Wait for the next event: terminal input, or [`Event::Wake`] when a spawned
/// [`Task`](crate::task::Task) finishes or [`task::wake`] is called.
pub async fn next() -> Event {
    if let Some(event) = pop() {
        return event;
    }
    if task::take_wake() {
        return Event::Wake;
    }
    let host = take_pending();
    let woken = task::woken();
    futures::pin_mut!(woken);
    match futures::future::select(host, woken).await {
        Either::Left((event, _)) => event.into(),
        Either::Right(((), host)) => {
            PENDING.with(|p| *p.borrow_mut() = Some(host));
            Event::Wake
        }
    }
}

/// Like [`next`], but gives up after `timeout` and returns `None`. Useful for
/// animations and periodic refreshes.
pub async fn next_timeout(timeout: Duration) -> Option<Event> {
    if let Some(event) = pop() {
        return Some(event);
    }
    if task::take_wake() {
        return Some(Event::Wake);
    }
    let host = take_pending();
    let woken = task::woken();
    let deadline = crate::wasi::time::sleep(timeout);
    futures::pin_mut!(woken);
    futures::pin_mut!(deadline);
    let wake_or_deadline = futures::future::select(woken, deadline);
    match futures::future::select(host, wake_or_deadline).await {
        Either::Left((event, _)) => Some(event.into()),
        Either::Right((Either::Left(_), host)) => {
            PENDING.with(|p| *p.borrow_mut() = Some(host));
            Some(Event::Wake)
        }
        Either::Right((Either::Right(_), host)) => {
            PENDING.with(|p| *p.borrow_mut() = Some(host));
            None
        }
    }
}

/// Return every event received so far without waiting.
pub fn poll() -> Vec<Event> {
    refill();
    BUFFER.with(|b| b.borrow_mut().drain(..).collect())
}

/// An endless stream of events.
pub fn stream() -> impl futures::Stream<Item = Event> + Unpin {
    Box::pin(futures::stream::unfold((), |()| async {
        Some((next().await, ()))
    }))
}

impl From<t::Event> for Event {
    fn from(event: t::Event) -> Self {
        match event {
            t::Event::FocusGained => Event::FocusGained,
            t::Event::FocusLost => Event::FocusLost,
            t::Event::Key(k) => Event::Key(k.into()),
            t::Event::Mouse(m) => Event::Mouse(m.into()),
            t::Event::Paste(s) => Event::Paste(s),
            t::Event::Resize(s) => Event::Resize(s.width, s.height),
            t::Event::UpdateChanged => Event::UpdateChanged,
        }
    }
}

impl From<t::KeyEvent> for KeyEvent {
    fn from(k: t::KeyEvent) -> Self {
        KeyEvent {
            code: k.code.into(),
            modifiers: KeyModifiers::from_bits_truncate(k.modifiers.bits()),
            kind: match k.kind {
                t::KeyEventKind::Press => KeyEventKind::Press,
                t::KeyEventKind::Repeat => KeyEventKind::Repeat,
                t::KeyEventKind::Release => KeyEventKind::Release,
            },
            state: KeyEventState::from_bits_truncate(k.state.bits()),
        }
    }
}

impl From<t::KeyCode> for KeyCode {
    fn from(code: t::KeyCode) -> Self {
        use t::KeyCode as W;
        match code {
            W::Backspace => KeyCode::Backspace,
            W::Enter => KeyCode::Enter,
            W::Left => KeyCode::Left,
            W::Right => KeyCode::Right,
            W::Up => KeyCode::Up,
            W::Down => KeyCode::Down,
            W::Home => KeyCode::Home,
            W::End => KeyCode::End,
            W::PageUp => KeyCode::PageUp,
            W::PageDown => KeyCode::PageDown,
            W::Tab => KeyCode::Tab,
            W::BackTab => KeyCode::BackTab,
            W::Delete => KeyCode::Delete,
            W::Insert => KeyCode::Insert,
            W::F(n) => KeyCode::F(n),
            W::Character(c) => KeyCode::Char(c),
            W::Null => KeyCode::Null,
            W::Esc => KeyCode::Esc,
            W::CapsLock => KeyCode::CapsLock,
            W::ScrollLock => KeyCode::ScrollLock,
            W::NumLock => KeyCode::NumLock,
            W::PrintScreen => KeyCode::PrintScreen,
            W::Pause => KeyCode::Pause,
            W::Menu => KeyCode::Menu,
            W::KeypadBegin => KeyCode::KeypadBegin,
            W::Media(m) => KeyCode::Media(m.into()),
            W::Modifier(m) => KeyCode::Modifier(m.into()),
        }
    }
}

impl From<t::MediaKey> for MediaKeyCode {
    fn from(m: t::MediaKey) -> Self {
        use t::MediaKey as W;
        match m {
            W::Play => MediaKeyCode::Play,
            W::Pause => MediaKeyCode::Pause,
            W::PlayPause => MediaKeyCode::PlayPause,
            W::Reverse => MediaKeyCode::Reverse,
            W::Stop => MediaKeyCode::Stop,
            W::FastForward => MediaKeyCode::FastForward,
            W::Rewind => MediaKeyCode::Rewind,
            W::TrackNext => MediaKeyCode::TrackNext,
            W::TrackPrevious => MediaKeyCode::TrackPrevious,
            W::Record => MediaKeyCode::Record,
            W::LowerVolume => MediaKeyCode::LowerVolume,
            W::RaiseVolume => MediaKeyCode::RaiseVolume,
            W::MuteVolume => MediaKeyCode::MuteVolume,
        }
    }
}

impl From<t::ModifierKey> for ModifierKeyCode {
    fn from(m: t::ModifierKey) -> Self {
        use t::ModifierKey as W;
        match m {
            W::LeftShift => ModifierKeyCode::LeftShift,
            W::LeftControl => ModifierKeyCode::LeftControl,
            W::LeftAlt => ModifierKeyCode::LeftAlt,
            W::LeftSuper => ModifierKeyCode::LeftSuper,
            W::LeftHyper => ModifierKeyCode::LeftHyper,
            W::LeftMeta => ModifierKeyCode::LeftMeta,
            W::RightShift => ModifierKeyCode::RightShift,
            W::RightControl => ModifierKeyCode::RightControl,
            W::RightAlt => ModifierKeyCode::RightAlt,
            W::RightSuper => ModifierKeyCode::RightSuper,
            W::RightHyper => ModifierKeyCode::RightHyper,
            W::RightMeta => ModifierKeyCode::RightMeta,
            W::IsoLevel3Shift => ModifierKeyCode::IsoLevel3Shift,
            W::IsoLevel5Shift => ModifierKeyCode::IsoLevel5Shift,
        }
    }
}

impl From<t::MouseEvent> for MouseEvent {
    fn from(m: t::MouseEvent) -> Self {
        MouseEvent {
            kind: m.kind.into(),
            column: m.column,
            row: m.row,
            modifiers: KeyModifiers::from_bits_truncate(m.modifiers.bits()),
        }
    }
}

impl From<t::MouseEventKind> for MouseEventKind {
    fn from(k: t::MouseEventKind) -> Self {
        use t::MouseEventKind as W;
        match k {
            W::Down(b) => MouseEventKind::Down(b.into()),
            W::Up(b) => MouseEventKind::Up(b.into()),
            W::Drag(b) => MouseEventKind::Drag(b.into()),
            W::Moved => MouseEventKind::Moved,
            W::ScrollDown => MouseEventKind::ScrollDown,
            W::ScrollUp => MouseEventKind::ScrollUp,
            W::ScrollLeft => MouseEventKind::ScrollLeft,
            W::ScrollRight => MouseEventKind::ScrollRight,
        }
    }
}

impl From<t::MouseButton> for MouseButton {
    fn from(b: t::MouseButton) -> Self {
        match b {
            t::MouseButton::Left => MouseButton::Left,
            t::MouseButton::Right => MouseButton::Right,
            t::MouseButton::Middle => MouseButton::Middle,
        }
    }
}
