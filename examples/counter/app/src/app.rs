use std::error::Error;
use std::time::Duration;

use counter_shared::{Snapshot, adjust_count, fetch_snapshot, slow_snapshot};
use rattery::event;
use rattery::prelude::*;
use rattery::ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

type Call = Task<Result<Snapshot, ServerFnError>>;

#[derive(Default)]
struct App {
    snapshot: Option<Snapshot>,
    last_error: Option<String>,
    origin: Option<String>,
    calls: u32,
    /// The server call in flight, if any. Starting a new one cancels it.
    pending: Option<Call>,
    spinner: usize,
    quit: bool,
}

impl App {
    /// Start a server call in the background; the UI keeps running.
    fn start(&mut self, call: impl Future<Output = Result<Snapshot, ServerFnError>> + 'static) {
        self.pending = Some(rattery::task::spawn(call));
    }

    /// Collect the result of a finished call.
    fn settle(&mut self) {
        if let Some(result) = self.pending.as_mut().and_then(Task::try_take) {
            self.pending = None;
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
}

pub async fn run(mut terminal: Terminal) -> Result<(), Box<dyn Error>> {
    rattery::set_title("rattery counter");
    let mut app = App {
        origin: rattery::origin(),
        ..App::default()
    };
    app.start(fetch_snapshot());

    while !app.quit {
        terminal.draw(|frame| ui(frame, &app))?;

        // Animate the spinner while a call is in flight; otherwise wait for input.
        let event = if app.pending.is_some() {
            match event::next_timeout(Duration::from_millis(80)).await {
                Some(event) => event,
                None => {
                    app.spinner = (app.spinner + 1) % SPINNER.len();
                    continue;
                }
            }
        } else {
            event::next().await
        };

        match event {
            Event::Wake => app.settle(),
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.quit = true
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('+') => app.start(adjust_count(1)),
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('-') => {
                    app.start(adjust_count(-1))
                }
                KeyCode::Char('r') => app.start(fetch_snapshot()),
                KeyCode::Char('s') => app.start(slow_snapshot(2000)),
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
    let status = if app.pending.is_some() {
        Span::from(format!("  {} calling server", SPINNER[app.spinner])).yellow()
    } else {
        Span::from("  idle").dim()
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            " rattery counter ".bold().reversed(),
            "  served from ".dim(),
            origin.cyan(),
            status,
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
                .title(" count lives on the server, per session "),
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
            " s ".bold(),
            "slow call (2s)  ".into(),
            " q ".bold(),
            "quit".into(),
        ]))
        .block(Block::new().borders(Borders::TOP)),
        footer,
    );
}
