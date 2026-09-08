//! Reading input events from the host.
//!
//! The host hands us a `wasi:io` pollable that is ready whenever events are
//! queued. Waiting on it through the `wstd` reactor lets an app `await` key
//! presses and server-function responses at the same time.

use super::bindings::terminal as t;
use crate::event::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use wstd::runtime::AsyncPollable;

struct Source {
    pollable: AsyncPollable,
    buffer: VecDeque<Event>,
}

thread_local! {
    static SOURCE: RefCell<Option<Source>> = const { RefCell::new(None) };
}

fn with_source<R>(f: impl FnOnce(&mut Source) -> R) -> R {
    SOURCE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let source = slot.get_or_insert_with(|| Source {
            pollable: AsyncPollable::new(t::subscribe_events()),
            buffer: VecDeque::new(),
        });
        f(source)
    })
}

fn refill(buffer: &mut VecDeque<Event>) {
    buffer.extend(t::read_events().into_iter().map(Event::from));
}

/// Wait for the next input event.
///
/// Must be called from inside [`crate::run`] (or `runtime::block_on`).
pub async fn next() -> Event {
    loop {
        if let Some(event) = with_source(|s| s.buffer.pop_front()) {
            return event;
        }
        let pollable = with_source(|s| s.pollable.clone());
        pollable.wait_for().await;
        with_source(|s| refill(&mut s.buffer));
    }
}

/// Return every event received so far without waiting.
pub fn poll() -> Vec<Event> {
    with_source(|s| {
        refill(&mut s.buffer);
        s.buffer.drain(..).collect()
    })
}

/// An endless stream of input events.
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
