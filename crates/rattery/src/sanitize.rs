//! Containment of guest-controlled text before it reaches a terminal.
//!
//! A terminal interprets escape sequences, and a sandboxed app must not be
//! able to emit them: not through a cell, not through the window title, and
//! not through its stdout or a panic message an embedder prints afterwards.
//! Everything here is total: bad input is replaced, never passed through.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Longest symbol accepted for one cell, in bytes. Real grapheme clusters
/// (flags, emoji with modifiers and joiners) fit comfortably.
pub const MAX_SYMBOL_BYTES: usize = 32;

/// Longest window title, in characters.
pub const MAX_TITLE_CHARS: usize = 256;

/// True for characters a terminal may interpret: C0 controls, DEL, C1
/// controls, and the Unicode line and paragraph separators.
pub fn is_control(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
}

/// A cell symbol the terminal backend may draw: no control characters, at
/// most one grapheme cluster, at most two columns wide, bounded in size.
/// Anything else becomes U+FFFD. The empty symbol is allowed: ratatui uses it
/// for the trailing half of a wide character.
pub fn symbol(s: &str) -> Cow<'_, str> {
    if s.is_empty() {
        return Cow::Borrowed(s);
    }
    // Fast path: the printable ASCII most cells hold.
    if let [b] = s.as_bytes()
        && (0x20..0x7f).contains(b)
    {
        return Cow::Borrowed(s);
    }
    let ok = s.len() <= MAX_SYMBOL_BYTES
        && !s.chars().any(is_control)
        && s.width() <= 2
        && s.graphemes(true).nth(1).is_none();
    if ok {
        Cow::Borrowed(s)
    } else {
        Cow::Borrowed("\u{FFFD}")
    }
}

/// A window title: control characters removed, length bounded.
pub fn title(s: &str) -> String {
    s.chars()
        .filter(|c| !is_control(*c))
        .take(MAX_TITLE_CHARS)
        .collect()
}

/// Text for printing to a terminal that came from a guest (its stdout, a
/// panic message, a trap backtrace with guest function names): newlines and
/// tabs survive, every other control character is shown as an escape like
/// `\u{1b}`, so nothing can start a sequence.
pub fn text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            c if is_control(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols() {
        assert_eq!(symbol("a"), "a");
        assert_eq!(symbol(""), "");
        assert_eq!(symbol("é"), "é");
        assert_eq!(symbol("日"), "日");
        assert_eq!(symbol("👨‍👩‍👧"), "👨‍👩‍👧");
        assert_eq!(symbol("\u{1b}"), "\u{FFFD}");
        assert_eq!(symbol("\u{1b}[31m"), "\u{FFFD}");
        assert_eq!(symbol("\u{7}"), "\u{FFFD}");
        assert_eq!(symbol("\u{9b}"), "\u{FFFD}");
        assert_eq!(symbol("ab"), "\u{FFFD}");
        assert_eq!(symbol("\u{2028}"), "\u{FFFD}");
        assert_eq!(symbol(&"x".repeat(MAX_SYMBOL_BYTES + 1)), "\u{FFFD}");
    }

    #[test]
    fn titles_and_text() {
        assert_eq!(title("hi\u{1b}]0;evil\u{7}there"), "hi]0;evilthere");
        assert_eq!(title(&"t".repeat(1000)).chars().count(), MAX_TITLE_CHARS);
        assert_eq!(text("ok\n\tfine"), "ok\n\tfine");
        assert_eq!(text("\u{1b}[2J\u{7}"), "\\u{1b}[2J\\u{7}");
        assert_eq!(text("c1 \u{9b}31m"), "c1 \\u{9b}31m");
    }
}
