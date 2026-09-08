use std::cell::RefCell;
use std::collections::VecDeque;
use std::error::Error;
use std::rc::Rc;
use std::time::Duration;

use counter_shared::{Snapshot, adjust_count, fetch_snapshot, live_feed, slow_snapshot};
use futures::StreamExt;
use rattery::event;
use rattery::prelude::*;
use rattery::ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const FEED_LINES: usize = 50;

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
    /// Lines pushed by the server over a streaming server function.
    feed: Rc<RefCell<VecDeque<String>>>,
    /// The task reading the feed; dropping it closes the stream.
    _feed_task: Option<Task<()>>,
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
                    // The first reply established our session cookie; only now
                    // can the feed subscribe as the same session.
                    if self._feed_task.is_none() {
                        self.follow_feed();
                    }
                }
                Err(err) => self.last_error = Some(err.to_string()),
            }
        }
    }

    /// Subscribe to the server's live feed. Each line wakes the event loop so
    /// the panel redraws as soon as it arrives.
    fn follow_feed(&mut self) {
        let feed = self.feed.clone();
        let push = move |line: String| {
            let mut feed = feed.borrow_mut();
            if feed.len() == FEED_LINES {
                feed.pop_front();
            }
            feed.push_back(line);
            rattery::task::wake();
        };
        self._feed_task = Some(rattery::task::spawn(async move {
            let stream = match live_feed().await {
                Ok(stream) => stream,
                Err(err) => return push(format!("feed error: {err}")),
            };
            let mut stream = stream.into_inner();
            let mut partial = String::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(text) => {
                        partial.push_str(&text);
                        while let Some(end) = partial.find('\n') {
                            let line = partial[..end].to_owned();
                            partial.drain(..=end);
                            push(line);
                        }
                    }
                    Err(err) => return push(format!("feed error: {err}")),
                }
            }
            push("feed ended".to_owned());
        }));
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
    let [counter, feed] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);

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
            "session {}  ·  server pid {}  ·  up {}s  ·  {} calls",
            &s.session[..8.min(s.session.len())],
            s.server_pid,
            s.uptime_secs,
            app.calls
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
                .title(" count, per session "),
        ),
        counter,
    );

    let feed_height = feed.height.saturating_sub(2) as usize;
    let feed_lines: Vec<Line> = app
        .feed
        .borrow()
        .iter()
        .rev()
        .take(feed_height)
        .rev()
        .map(|line| Line::from(line.clone()))
        .collect();
    frame.render_widget(
        Paragraph::new(feed_lines).block(
            Block::bordered()
                .padding(Padding::horizontal(1))
                .title(" live feed (streaming server fn) "),
        ),
        feed,
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
