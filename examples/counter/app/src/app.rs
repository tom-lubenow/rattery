use std::error::Error;

use counter_shared::{Snapshot, adjust_count, fetch_snapshot};
use rattery::event;
use rattery::prelude::*;
use rattery::ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

#[derive(Default)]
struct App {
    snapshot: Option<Snapshot>,
    last_error: Option<String>,
    origin: Option<String>,
    calls: u32,
    quit: bool,
}

impl App {
    fn apply(&mut self, result: Result<Snapshot, ServerFnError>) {
        self.calls += 1;
        match result {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.last_error = None;
            }
            Err(err) => self.last_error = Some(err.to_string()),
        }
    }
}

pub async fn run(mut terminal: Terminal) -> Result<(), Box<dyn Error>> {
    rattery::set_title("rattery counter");
    let mut app = App {
        origin: rattery::origin(),
        ..App::default()
    };
    app.apply(fetch_snapshot().await);

    while !app.quit {
        terminal.draw(|frame| ui(frame, &app))?;

        match event::next().await {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.quit = true
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('+') => {
                    app.apply(adjust_count(1).await)
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('-') => {
                    app.apply(adjust_count(-1).await)
                }
                KeyCode::Char('r') => app.apply(fetch_snapshot().await),
                _ => {}
            },
            _ => {}
        }
    }
    Ok(())
}

fn ui(frame: &mut Frame, app: &App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    let origin = app
        .origin
        .as_deref()
        .unwrap_or("(no origin: loaded from a file)");
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            " rattery counter ".bold().reversed(),
            "  served from ".dim(),
            origin.cyan(),
        ]))
        .block(Block::new().borders(Borders::BOTTOM)),
        header,
    );

    let count = match &app.snapshot {
        Some(s) => s.count.to_string(),
        None => "?".to_owned(),
    };
    let detail = match &app.snapshot {
        Some(s) => format!(
            "server pid {}  ·  up {}s  ·  {} calls",
            s.server_pid, s.uptime_secs, app.calls
        ),
        None => format!("{} calls", app.calls),
    };
    let mut lines = vec![
        Line::from(""),
        Line::from(count.bold().yellow()).centered(),
        Line::from(""),
        Line::from(detail.dim()).centered(),
    ];
    if let Some(err) = &app.last_error {
        lines.push(Line::from(""));
        lines.push(Line::from(format!("error: {err}").red()).centered());
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::bordered()
                .padding(Padding::horizontal(1))
                .title(" count lives on the server "),
        ),
        body,
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            " ↑/k/+ ".bold(),
            "increment  ".into(),
            " ↓/j/- ".bold(),
            "decrement  ".into(),
            " r ".bold(),
            "refresh  ".into(),
            " q ".bold(),
            "quit".into(),
        ]))
        .block(Block::new().borders(Borders::TOP)),
        footer,
    );
}
