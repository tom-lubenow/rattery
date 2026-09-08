//! A ratatui [`Backend`] whose "terminal" is the rattery host.

use super::bindings::terminal as t;
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};
use std::io;

/// Renders cell diffs to the host terminal and reports its size and cursor.
///
/// Diffing happens inside ratatui's `Terminal`; only changed cells cross the
/// component boundary, so a frame costs one host call plus one for `flush`.
#[derive(Debug, Default, Clone)]
pub struct RatteryBackend {
    _private: (),
}

impl RatteryBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Backend for RatteryBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let updates: Vec<t::CellUpdate<'a>> = content
            .map(|(x, y, cell)| t::CellUpdate {
                x,
                y,
                cell: to_wit_cell(cell),
            })
            .collect();
        if !updates.is_empty() {
            t::draw(&updates);
        }
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        t::append_lines(n);
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        t::hide_cursor();
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        t::show_cursor();
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        let p = t::get_cursor_position();
        Ok(Position::new(p.x, p.y))
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let p = position.into();
        t::set_cursor_position(t::Position { x: p.x, y: p.y });
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        t::clear(t::ClearType::All);
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        t::clear(match clear_type {
            ClearType::All => t::ClearType::All,
            ClearType::AfterCursor => t::ClearType::AfterCursor,
            ClearType::BeforeCursor => t::ClearType::BeforeCursor,
            ClearType::CurrentLine => t::ClearType::CurrentLine,
            ClearType::UntilNewLine => t::ClearType::UntilNewLine,
        });
        Ok(())
    }

    fn size(&self) -> io::Result<Size> {
        let s = t::get_size();
        Ok(Size::new(s.width, s.height))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let w = t::get_window_size();
        Ok(WindowSize {
            columns_rows: Size::new(w.columns_rows.width, w.columns_rows.height),
            pixels: Size::new(w.pixels.width, w.pixels.height),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        t::flush();
        Ok(())
    }
}

fn to_wit_cell(cell: &Cell) -> t::Cell<'_> {
    t::Cell {
        symbol: cell.symbol(),
        fg: to_wit_color(cell.fg),
        bg: to_wit_color(cell.bg),
        underline_color: to_wit_color(cell.underline_color),
        modifier: to_wit_modifier(cell.modifier),
    }
}

fn to_wit_color(color: Color) -> t::Color {
    match color {
        Color::Reset => t::Color::Reset,
        Color::Black => t::Color::Black,
        Color::Red => t::Color::Red,
        Color::Green => t::Color::Green,
        Color::Yellow => t::Color::Yellow,
        Color::Blue => t::Color::Blue,
        Color::Magenta => t::Color::Magenta,
        Color::Cyan => t::Color::Cyan,
        Color::Gray => t::Color::Gray,
        Color::DarkGray => t::Color::DarkGray,
        Color::LightRed => t::Color::LightRed,
        Color::LightGreen => t::Color::LightGreen,
        Color::LightYellow => t::Color::LightYellow,
        Color::LightBlue => t::Color::LightBlue,
        Color::LightMagenta => t::Color::LightMagenta,
        Color::LightCyan => t::Color::LightCyan,
        Color::White => t::Color::White,
        Color::Rgb(r, g, b) => t::Color::Rgb((r, g, b)),
        Color::Indexed(i) => t::Color::Indexed(i),
    }
}

fn to_wit_modifier(modifier: Modifier) -> t::Modifier {
    const TABLE: &[(Modifier, t::Modifier)] = &[
        (Modifier::BOLD, t::Modifier::BOLD),
        (Modifier::DIM, t::Modifier::DIM),
        (Modifier::ITALIC, t::Modifier::ITALIC),
        (Modifier::UNDERLINED, t::Modifier::UNDERLINED),
        (Modifier::SLOW_BLINK, t::Modifier::SLOW_BLINK),
        (Modifier::RAPID_BLINK, t::Modifier::RAPID_BLINK),
        (Modifier::REVERSED, t::Modifier::REVERSED),
        (Modifier::HIDDEN, t::Modifier::HIDDEN),
        (Modifier::CROSSED_OUT, t::Modifier::CROSSED_OUT),
    ];
    let mut out = t::Modifier::empty();
    for (from, to) in TABLE {
        if modifier.contains(*from) {
            out |= *to;
        }
    }
    out
}
