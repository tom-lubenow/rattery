//! Interactive workloads that stand in for real apps: an animated
//! dashboard, a grid of buttons, a draggable divider, and a hover grid.
//! Each is a piece of state driven by [`Input`] and rendered with ratatui,
//! plus a deterministic input stream, so native ratatui and rattery can be
//! measured on exactly the same work.

use std::collections::VecDeque;

use rattery_app::ratatui::Frame;
use rattery_app::ratatui::layout::{Constraint, Layout, Rect};
use rattery_app::ratatui::style::{Color, Style, Stylize};
use rattery_app::ratatui::text::{Line, Span};
use rattery_app::ratatui::widgets::{
    Block, Borders, Gauge, List, ListItem, Paragraph, Sparkline, Wrap,
};

use crate::hover::Tiles;

/// Input as the workloads see it, independent of who delivers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Move(u16, u16),
    Down(u16, u16),
    Up(u16, u16),
    Drag(u16, u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A dashboard that changes every frame: sparkline, gauge, a scrolling
    /// log, a spinner. Driven by frames, not input.
    Anim,
    /// A grid of buttons; a press highlights one and the release toggles it.
    Click,
    /// Two panes of wrapped text with a divider the pointer drags.
    Drag,
    /// A grid of tiles highlighting the one under the pointer.
    Hover,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s {
            "anim" => Mode::Anim,
            "click" => Mode::Click,
            "drag" => Mode::Drag,
            "hover" => Mode::Hover,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Anim => "anim",
            Mode::Click => "click",
            Mode::Drag => "drag",
            Mode::Hover => "hover",
        }
    }
}

pub const BUTTON_COLS: u16 = 8;
pub const BUTTON_ROWS: u16 = 4;
const LOG_LINES: usize = 40;
const SAMPLES: usize = 120;
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const LOREM: &str = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod \
    tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud \
    exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. ";

/// One workload's state.
pub struct Workload {
    pub mode: Mode,
    pub frame: usize,
    /// How many times the layout is rendered per frame (heavier UIs).
    pub work: usize,
    // anim
    samples: VecDeque<u64>,
    log: VecDeque<String>,
    // click
    buttons: Vec<bool>,
    pressed: Option<usize>,
    pub toggles: usize,
    // drag
    split: u16,
    dragging: bool,
    // hover
    hover: Option<(u16, u16)>,
    /// Inputs applied.
    pub inputs: usize,
}

impl Workload {
    pub fn new(mode: Mode, work: usize, width: u16) -> Self {
        Self {
            mode,
            frame: 0,
            work: work.max(1),
            samples: (0..SAMPLES).map(|i| (i as u64 * 37) % 100).collect(),
            log: VecDeque::new(),
            buttons: vec![false; (BUTTON_COLS * BUTTON_ROWS) as usize],
            pressed: None,
            toggles: 0,
            split: width / 2,
            dragging: false,
            hover: None,
            inputs: 0,
        }
    }

    /// Advance the animation by one frame.
    pub fn tick(&mut self) {
        self.frame += 1;
        if self.mode == Mode::Anim {
            let next =
                (self.samples.back().copied().unwrap_or(50) * 7 + self.frame as u64 * 13) % 100;
            self.samples.pop_front();
            self.samples.push_back(next);
            if self.log.len() == LOG_LINES {
                self.log.pop_front();
            }
            self.log.push_back(format!(
                "frame {:>6}  sample {next:>3}  {}",
                self.frame,
                if self.frame.is_multiple_of(3) {
                    "ok"
                } else {
                    "tick"
                }
            ));
        }
    }

    pub fn apply(&mut self, input: Input, area: Rect) {
        self.inputs += 1;
        match self.mode {
            Mode::Anim => {}
            Mode::Click => match input {
                Input::Down(x, y) => self.pressed = button_at(area, x, y),
                Input::Up(..) => {
                    if let Some(i) = self.pressed.take() {
                        self.buttons[i] = !self.buttons[i];
                        self.toggles += 1;
                    }
                }
                _ => {}
            },
            Mode::Drag => match input {
                Input::Down(x, _) => self.dragging = x.abs_diff(self.split) <= 1,
                Input::Drag(x, _) if self.dragging => {
                    self.split = x.clamp(10, area.width.saturating_sub(10).max(10));
                }
                Input::Up(..) => self.dragging = false,
                _ => {}
            },
            Mode::Hover => {
                if let Input::Move(x, y) | Input::Drag(x, y) = input {
                    self.hover = Some((x, y));
                }
            }
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        for _ in 1..self.work {
            // Rendered and discarded: the layout cost without the cells.
            let mut scratch = frame.buffer_mut().clone();
            self.render_into(area, &mut scratch);
        }
        let buf = frame.buffer_mut();
        self.render_into(area, buf);
    }

    fn render_into(&self, area: Rect, buf: &mut rattery_app::ratatui::buffer::Buffer) {
        use rattery_app::ratatui::widgets::Widget;
        match self.mode {
            Mode::Anim => {
                let [top, middle, bottom] = Layout::vertical([
                    Constraint::Length(3),
                    Constraint::Length(8),
                    Constraint::Fill(1),
                ])
                .areas(area);
                let percent = (self.frame % 100) as u16;
                Gauge::default()
                    .block(Block::bordered().title(" progress "))
                    .gauge_style(Style::new().fg(Color::Cyan))
                    .percent(percent)
                    .render(top, buf);
                let samples: Vec<u64> = self.samples.iter().copied().collect();
                Sparkline::default()
                    .block(Block::bordered().title(" throughput "))
                    .data(&samples)
                    .style(Style::new().fg(Color::Green))
                    .render(middle, buf);
                let [log, status] =
                    Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(bottom);
                let items: Vec<ListItem> = self
                    .log
                    .iter()
                    .rev()
                    .take(log.height.saturating_sub(2) as usize)
                    .rev()
                    .map(|l| ListItem::new(l.as_str()))
                    .collect();
                List::new(items)
                    .block(Block::bordered().title(" log "))
                    .render(log, buf);
                Line::from(vec![
                    Span::from(SPINNER[self.frame % SPINNER.len()]).yellow(),
                    Span::from(format!(" frame {}  ", self.frame)),
                    Span::from(format!("t+{:.1}s", self.frame as f64 / 60.0)).dim(),
                ])
                .render(status, buf);
            }
            Mode::Click => {
                for (i, cell) in button_areas(area).into_iter().enumerate() {
                    let on = self.buttons[i];
                    let pressed = self.pressed == Some(i);
                    let style = match (on, pressed) {
                        (_, true) => Style::new().fg(Color::Black).bg(Color::White),
                        (true, _) => Style::new().fg(Color::Black).bg(Color::Green),
                        _ => Style::new(),
                    };
                    Paragraph::new(
                        Line::from(format!("button {i}: {}", if on { "on" } else { "off" }))
                            .centered(),
                    )
                    .style(style)
                    .block(Block::bordered())
                    .render(cell, buf);
                }
                Line::from(format!(" {} toggles ", self.toggles))
                    .render(Rect { height: 1, ..area }, buf);
            }
            Mode::Drag => {
                let split = self.split.min(area.width);
                let [left, right] =
                    Layout::horizontal([Constraint::Length(split), Constraint::Fill(1)])
                        .areas(area);
                let divider = if self.dragging {
                    Style::new().fg(Color::Yellow)
                } else {
                    Style::new().fg(Color::DarkGray)
                };
                Paragraph::new(LOREM.repeat(8))
                    .wrap(Wrap { trim: true })
                    .block(
                        Block::new()
                            .borders(Borders::ALL)
                            .title(format!(" left ({split}) "))
                            .border_style(divider),
                    )
                    .render(left, buf);
                Paragraph::new(LOREM.repeat(8))
                    .wrap(Wrap { trim: true })
                    .block(Block::bordered().title(" right "))
                    .render(right, buf);
            }
            Mode::Hover => Tiles {
                hover: self.hover,
                frame: self.frame,
            }
            .render(area, buf),
        }
    }
}

fn button_areas(area: Rect) -> Vec<Rect> {
    Layout::vertical(vec![Constraint::Fill(1); BUTTON_ROWS as usize])
        .split(area)
        .iter()
        .flat_map(|row| {
            Layout::horizontal(vec![Constraint::Fill(1); BUTTON_COLS as usize])
                .split(*row)
                .to_vec()
        })
        .collect()
}

fn button_at(area: Rect, x: u16, y: u16) -> Option<usize> {
    button_areas(area)
        .iter()
        .position(|r| r.contains((x, y).into()))
}

/// The synthetic input for a mode on a screen of this size: `n` steps.
/// Deterministic, so both sides see the same stream.
pub fn stream(mode: Mode, width: u16, height: u16, n: usize) -> Vec<Input> {
    let w = width.saturating_sub(1) as f64;
    let h = height.saturating_sub(1) as f64;
    match mode {
        Mode::Anim => Vec::new(),
        Mode::Hover => (0..n)
            .map(|i| {
                let t = i as f64 / n.max(1) as f64;
                Input::Move((t * w) as u16, (t * h) as u16)
            })
            .collect(),
        Mode::Click => {
            let area = Rect::new(0, 0, width, height);
            let areas = button_areas(area);
            (0..n)
                .flat_map(|i| {
                    let r = areas[(i * 7) % areas.len()];
                    let (x, y) = (r.x + r.width / 2, r.y + r.height / 2);
                    [Input::Down(x, y), Input::Up(x, y)]
                })
                .collect()
        }
        Mode::Drag => {
            let y = height / 2;
            let mut inputs = vec![Input::Down(width / 2, y)];
            let (lo, hi) = (width / 4, width * 3 / 4);
            let span = (hi - lo).max(1) as usize;
            for i in 0..n {
                let phase = i % (2 * span);
                let x = if phase < span {
                    lo + phase as u16
                } else {
                    hi - (phase - span) as u16
                };
                inputs.push(Input::Drag(x, y));
            }
            inputs.push(Input::Up(width / 2, y));
            inputs
        }
    }
}

/// The same stream as a headless script for the rattery host, one input
/// every `interval_ms`.
pub fn script(mode: Mode, width: u16, height: u16, n: usize, interval_ms: u64) -> String {
    let mut out = String::from("sleep 1500\n");
    for input in stream(mode, width, height, n) {
        let (kind, x, y) = match input {
            Input::Move(x, y) => ("move", x, y),
            Input::Down(x, y) => ("down", x, y),
            Input::Up(x, y) => ("up", x, y),
            Input::Drag(x, y) => ("drag", x, y),
        };
        out.push_str(&format!("mouse {kind} {x} {y}\nsleep {interval_ms}\n"));
    }
    out.push_str("key q\n");
    out
}

/// Summary statistics of a set of durations, as the benchmark lines report
/// them: `<prefix>_avg_ms=… <prefix>_p50_ms=… <prefix>_p95_ms=… <prefix>_max_ms=…`.
pub fn summary(prefix: &str, durations: &mut [std::time::Duration]) -> String {
    if durations.is_empty() {
        return format!("{prefix}_avg_ms=0 {prefix}_p50_ms=0 {prefix}_p95_ms=0 {prefix}_max_ms=0");
    }
    durations.sort();
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let pct = |p: f64| durations[((durations.len() - 1) as f64 * p) as usize];
    let total: std::time::Duration = durations.iter().sum();
    format!(
        "{prefix}_avg_ms={:.3} {prefix}_p50_ms={:.3} {prefix}_p95_ms={:.3} {prefix}_max_ms={:.3}",
        ms(total) / durations.len() as f64,
        ms(pct(0.5)),
        ms(pct(0.95)),
        ms(*durations.last().unwrap())
    )
}
