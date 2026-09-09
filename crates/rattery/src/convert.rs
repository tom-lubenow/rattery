//! Conversions between the WIT types and ratatui / crossterm types.

use crossterm::event as ct;
use ratatui::buffer::Cell;
use ratatui::style::{Color, Modifier};

use crate::bindings::terminal as t;

/// A ratatui cell from a WIT cell, with the symbol already sanitised.
pub fn cell(c: &t::Cell, symbol: &str) -> Cell {
    let mut cell = Cell::default();
    cell.set_symbol(symbol);
    cell.fg = color(c.fg);
    cell.bg = color(c.bg);
    cell.underline_color = color(c.underline_color);
    cell.modifier = modifier(c.modifier);
    cell
}

pub fn color(c: t::Color) -> Color {
    match c {
        t::Color::Reset => Color::Reset,
        t::Color::Black => Color::Black,
        t::Color::Red => Color::Red,
        t::Color::Green => Color::Green,
        t::Color::Yellow => Color::Yellow,
        t::Color::Blue => Color::Blue,
        t::Color::Magenta => Color::Magenta,
        t::Color::Cyan => Color::Cyan,
        t::Color::Gray => Color::Gray,
        t::Color::DarkGray => Color::DarkGray,
        t::Color::LightRed => Color::LightRed,
        t::Color::LightGreen => Color::LightGreen,
        t::Color::LightYellow => Color::LightYellow,
        t::Color::LightBlue => Color::LightBlue,
        t::Color::LightMagenta => Color::LightMagenta,
        t::Color::LightCyan => Color::LightCyan,
        t::Color::White => Color::White,
        t::Color::Rgb((r, g, b)) => Color::Rgb(r, g, b),
        t::Color::Indexed(i) => Color::Indexed(i),
    }
}

pub fn modifier(m: t::Modifier) -> Modifier {
    const TABLE: &[(t::Modifier, Modifier)] = &[
        (t::Modifier::BOLD, Modifier::BOLD),
        (t::Modifier::DIM, Modifier::DIM),
        (t::Modifier::ITALIC, Modifier::ITALIC),
        (t::Modifier::UNDERLINED, Modifier::UNDERLINED),
        (t::Modifier::SLOW_BLINK, Modifier::SLOW_BLINK),
        (t::Modifier::RAPID_BLINK, Modifier::RAPID_BLINK),
        (t::Modifier::REVERSED, Modifier::REVERSED),
        (t::Modifier::HIDDEN, Modifier::HIDDEN),
        (t::Modifier::CROSSED_OUT, Modifier::CROSSED_OUT),
    ];
    let mut out = Modifier::empty();
    for (from, to) in TABLE {
        if m.contains(*from) {
            out |= *to;
        }
    }
    out
}

pub fn clear_type(kind: t::ClearType) -> ratatui::backend::ClearType {
    use ratatui::backend::ClearType as R;
    match kind {
        t::ClearType::All => R::All,
        t::ClearType::AfterCursor => R::AfterCursor,
        t::ClearType::BeforeCursor => R::BeforeCursor,
        t::ClearType::CurrentLine => R::CurrentLine,
        t::ClearType::UntilNewLine => R::UntilNewLine,
    }
}

/// Convert a crossterm event. Returns `None` for events the contract does not
/// carry (currently none, but the option keeps the call site honest).
pub fn event(event: ct::Event) -> Option<t::Event> {
    Some(match event {
        ct::Event::FocusGained => t::Event::FocusGained,
        ct::Event::FocusLost => t::Event::FocusLost,
        ct::Event::Key(k) => t::Event::Key(key_event(k)),
        ct::Event::Mouse(m) => t::Event::Mouse(mouse_event(m)),
        ct::Event::Paste(s) => t::Event::Paste(s),
        ct::Event::Resize(width, height) => t::Event::Resize(t::Size { width, height }),
    })
}

fn key_event(k: ct::KeyEvent) -> t::KeyEvent {
    t::KeyEvent {
        code: key_code(k.code),
        modifiers: key_modifiers(k.modifiers),
        kind: match k.kind {
            ct::KeyEventKind::Press => t::KeyEventKind::Press,
            ct::KeyEventKind::Repeat => t::KeyEventKind::Repeat,
            ct::KeyEventKind::Release => t::KeyEventKind::Release,
        },
        state: key_state(k.state),
    }
}

fn key_modifiers(m: ct::KeyModifiers) -> t::KeyModifiers {
    const TABLE: &[(ct::KeyModifiers, t::KeyModifiers)] = &[
        (ct::KeyModifiers::SHIFT, t::KeyModifiers::SHIFT),
        (ct::KeyModifiers::CONTROL, t::KeyModifiers::CONTROL),
        (ct::KeyModifiers::ALT, t::KeyModifiers::ALT),
        (ct::KeyModifiers::SUPER, t::KeyModifiers::SUPER),
        (ct::KeyModifiers::HYPER, t::KeyModifiers::HYPER),
        (ct::KeyModifiers::META, t::KeyModifiers::META),
    ];
    let mut out = t::KeyModifiers::empty();
    for (from, to) in TABLE {
        if m.contains(*from) {
            out |= *to;
        }
    }
    out
}

fn key_state(s: ct::KeyEventState) -> t::KeyEventState {
    const TABLE: &[(ct::KeyEventState, t::KeyEventState)] = &[
        (ct::KeyEventState::KEYPAD, t::KeyEventState::KEYPAD),
        (ct::KeyEventState::CAPS_LOCK, t::KeyEventState::CAPS_LOCK),
        (ct::KeyEventState::NUM_LOCK, t::KeyEventState::NUM_LOCK),
    ];
    let mut out = t::KeyEventState::empty();
    for (from, to) in TABLE {
        if s.contains(*from) {
            out |= *to;
        }
    }
    out
}

fn key_code(code: ct::KeyCode) -> t::KeyCode {
    use ct::KeyCode as C;
    match code {
        C::Backspace => t::KeyCode::Backspace,
        C::Enter => t::KeyCode::Enter,
        C::Left => t::KeyCode::Left,
        C::Right => t::KeyCode::Right,
        C::Up => t::KeyCode::Up,
        C::Down => t::KeyCode::Down,
        C::Home => t::KeyCode::Home,
        C::End => t::KeyCode::End,
        C::PageUp => t::KeyCode::PageUp,
        C::PageDown => t::KeyCode::PageDown,
        C::Tab => t::KeyCode::Tab,
        C::BackTab => t::KeyCode::BackTab,
        C::Delete => t::KeyCode::Delete,
        C::Insert => t::KeyCode::Insert,
        C::F(n) => t::KeyCode::F(n),
        C::Char(c) => t::KeyCode::Character(c),
        C::Null => t::KeyCode::Null,
        C::Esc => t::KeyCode::Esc,
        C::CapsLock => t::KeyCode::CapsLock,
        C::ScrollLock => t::KeyCode::ScrollLock,
        C::NumLock => t::KeyCode::NumLock,
        C::PrintScreen => t::KeyCode::PrintScreen,
        C::Pause => t::KeyCode::Pause,
        C::Menu => t::KeyCode::Menu,
        C::KeypadBegin => t::KeyCode::KeypadBegin,
        C::Media(m) => t::KeyCode::Media(media_key(m)),
        C::Modifier(m) => t::KeyCode::Modifier(modifier_key(m)),
    }
}

fn media_key(m: ct::MediaKeyCode) -> t::MediaKey {
    use ct::MediaKeyCode as C;
    match m {
        C::Play => t::MediaKey::Play,
        C::Pause => t::MediaKey::Pause,
        C::PlayPause => t::MediaKey::PlayPause,
        C::Reverse => t::MediaKey::Reverse,
        C::Stop => t::MediaKey::Stop,
        C::FastForward => t::MediaKey::FastForward,
        C::Rewind => t::MediaKey::Rewind,
        C::TrackNext => t::MediaKey::TrackNext,
        C::TrackPrevious => t::MediaKey::TrackPrevious,
        C::Record => t::MediaKey::Record,
        C::LowerVolume => t::MediaKey::LowerVolume,
        C::RaiseVolume => t::MediaKey::RaiseVolume,
        C::MuteVolume => t::MediaKey::MuteVolume,
    }
}

fn modifier_key(m: ct::ModifierKeyCode) -> t::ModifierKey {
    use ct::ModifierKeyCode as C;
    match m {
        C::LeftShift => t::ModifierKey::LeftShift,
        C::LeftControl => t::ModifierKey::LeftControl,
        C::LeftAlt => t::ModifierKey::LeftAlt,
        C::LeftSuper => t::ModifierKey::LeftSuper,
        C::LeftHyper => t::ModifierKey::LeftHyper,
        C::LeftMeta => t::ModifierKey::LeftMeta,
        C::RightShift => t::ModifierKey::RightShift,
        C::RightControl => t::ModifierKey::RightControl,
        C::RightAlt => t::ModifierKey::RightAlt,
        C::RightSuper => t::ModifierKey::RightSuper,
        C::RightHyper => t::ModifierKey::RightHyper,
        C::RightMeta => t::ModifierKey::RightMeta,
        C::IsoLevel3Shift => t::ModifierKey::IsoLevel3Shift,
        C::IsoLevel5Shift => t::ModifierKey::IsoLevel5Shift,
    }
}

fn mouse_event(m: ct::MouseEvent) -> t::MouseEvent {
    use ct::MouseEventKind as K;
    let kind = match m.kind {
        K::Down(b) => t::MouseEventKind::Down(mouse_button(b)),
        K::Up(b) => t::MouseEventKind::Up(mouse_button(b)),
        K::Drag(b) => t::MouseEventKind::Drag(mouse_button(b)),
        K::Moved => t::MouseEventKind::Moved,
        K::ScrollDown => t::MouseEventKind::ScrollDown,
        K::ScrollUp => t::MouseEventKind::ScrollUp,
        K::ScrollLeft => t::MouseEventKind::ScrollLeft,
        K::ScrollRight => t::MouseEventKind::ScrollRight,
    };
    t::MouseEvent {
        kind,
        column: m.column,
        row: m.row,
        modifiers: key_modifiers(m.modifiers),
    }
}

fn mouse_button(b: ct::MouseButton) -> t::MouseButton {
    match b {
        ct::MouseButton::Left => t::MouseButton::Left,
        ct::MouseButton::Right => t::MouseButton::Right,
        ct::MouseButton::Middle => t::MouseButton::Middle,
    }
}
