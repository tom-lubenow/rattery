//! A tiled layout with a highlighted tile under the pointer, standing in for
//! a tiling widget library with hover support. Pure ratatui, so it renders
//! the same natively and inside the component.

use rattery_app::ratatui::buffer::Buffer;
use rattery_app::ratatui::layout::{Constraint, Layout, Rect};
use rattery_app::ratatui::style::{Color, Style, Stylize};
use rattery_app::ratatui::text::Line;
use rattery_app::ratatui::widgets::{Block, Paragraph, Widget, Wrap};

pub const COLS: u16 = 6;
pub const ROWS: u16 = 4;

/// The grid of tiles, with the pointer at `hover` (column, row) if known.
pub struct Tiles {
    pub hover: Option<(u16, u16)>,
    pub frame: usize,
}

impl Tiles {
    /// The tile areas, row-major.
    pub fn areas(area: Rect) -> Vec<Rect> {
        let rows = Layout::vertical(vec![Constraint::Fill(1); ROWS as usize]).split(area);
        rows.iter()
            .flat_map(|row| {
                Layout::horizontal(vec![Constraint::Fill(1); COLS as usize])
                    .split(*row)
                    .to_vec()
            })
            .collect()
    }
}

impl Widget for Tiles {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for (index, tile) in Tiles::areas(area).into_iter().enumerate() {
            let hovered = self
                .hover
                .is_some_and(|(x, y)| tile.contains((x, y).into()));
            let block = if hovered {
                Block::bordered()
                    .border_style(Style::new().fg(Color::Yellow))
                    .title(format!(" tile {index} (hover) ").bold().yellow())
            } else {
                Block::bordered().title(format!(" tile {index} ").dim())
            };
            let body = format!(
                "frame {} · a few lines of text so each tile costs something to lay out and wrap, \
                 the way a real pane does; pointer {:?}",
                self.frame, self.hover
            );
            let paragraph = Paragraph::new(vec![Line::from(body)])
                .wrap(Wrap { trim: true })
                .block(block);
            let paragraph = if hovered {
                paragraph.style(Style::new().fg(Color::Black).bg(Color::Yellow))
            } else {
                paragraph
            };
            paragraph.render(tile, buf);
        }
    }
}
