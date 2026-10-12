//! Copy text to the terminal's system clipboard using OSC 52.
//!
//! The terminal emulator (and any multiplexer in between) must support and
//! allow OSC 52. There is no clipboard read operation; user pastes arrive as
//! [`crate::event::Event::Paste`].

use std::fmt;

use crate::bindings::terminal::{self, ClipboardError};

/// Why the host could not copy the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Clipboard writes are disabled by the host.
    Disabled,
    /// The UTF-8 text exceeds the host's byte limit (64 KiB by default).
    TooLarge,
    /// Writing to the terminal failed.
    Io(String),
}

impl From<ClipboardError> for Error {
    fn from(error: ClipboardError) -> Self {
        match error {
            ClipboardError::Disabled => Self::Disabled,
            ClipboardError::TooLarge => Self::TooLarge,
            ClipboardError::Io(message) => Self::Io(message),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("clipboard writes are disabled by the host"),
            Self::TooLarge => f.write_str("clipboard text exceeds the host's byte limit"),
            Self::Io(message) => write!(f, "clipboard write failed: {message}"),
        }
    }
}

impl std::error::Error for Error {}

/// Copy `text` to the system clipboard. Call in response to the user's copy
/// action. Text is preserved exactly, including newlines and Unicode; an
/// empty string clears the clipboard.
///
/// Success means the host wrote and flushed an OSC 52 command. Terminals
/// may silently ignore it, so success cannot confirm clipboard contents.
/// A headless host instead keeps the last copy in `rattery::Report::clipboard`.
pub fn copy(text: &str) -> Result<(), Error> {
    terminal::copy_to_clipboard(text).map_err(Error::from)
}
