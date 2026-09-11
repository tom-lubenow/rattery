//! Redraws the screen `frames` times in one of three patterns and prints
//! timing statistics. Parameters come from the location query string:
//! `?frames=200&mode=full|sparse|text`.
//!
//! - `full`: every cell changes every frame (worst case for the cell diff).
//! - `sparse`: one row changes per frame (typical of a live-updating UI).
//! - `text`: a wrapped paragraph re-rendered each frame (typical widget cost;
//!   few cells actually change).
//! - `http`: `frames` sequential GETs of the app's origin over `wasi:http`,
//!   to measure request latency through the host (needs `--origin`).
//! - `evil`: tries to inject escape sequences through cells, the title, and
//!   stdout, then exits; the host must contain all of it.
//! - `hover`: a tiled layout that highlights the tile under the pointer and
//!   redraws on every event, the way a tiling widget library does; feed it
//!   mouse movement (`sweep` in a headless script) and it reports how many
//!   events it saw and how many frames it drew, until `q`.
//! - `storage`: exercises origin-scoped storage and logging: bumps a run
//!   counter, tries to exceed the quota, logs a record with an escape sequence
//!   in it, then floods the log to hit the rate limit.

rattery_app::app!(run);

#[cfg(target_os = "wasi")]
mod bench {
    use std::time::Instant;

    use rattery_app::prelude::*;
    use rattery_app::ratatui::buffer::Buffer;
    use rattery_app::ratatui::widgets::{Block, Paragraph, Widget, Wrap};

    struct Params {
        frames: usize,
        mode: String,
        /// `hover`: render the layout this many times per frame, to stand in
        /// for a heavier UI.
        work: usize,
        /// Call `reload()` after the frames instead of exiting: a guest that
        /// tries to outlive its CPU budget by restarting.
        reload: bool,
    }

    fn params() -> Params {
        let mut params = Params {
            frames: 100,
            mode: "full".into(),
            work: 1,
            reload: false,
        };
        if let Some(location) = rattery_app::location()
            && let Some((_, query)) = location.split_once('?')
        {
            for pair in query.split('&') {
                match pair.split_once('=') {
                    Some(("frames", n)) => params.frames = n.parse().unwrap_or(params.frames),
                    Some(("mode", m)) => params.mode = m.to_owned(),
                    Some(("work", n)) => params.work = n.parse().unwrap_or(1).max(1),
                    Some(("reload", v)) => params.reload = v == "1",
                    _ => {}
                }
            }
        }
        params
    }

    struct FullFrame(usize);

    impl Widget for FullFrame {
        fn render(self, area: Rect, buf: &mut Buffer) {
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    let n = (x as usize + y as usize + self.0) % 26;
                    let cell = &mut buf[(x, y)];
                    cell.set_char((b'a' + n as u8) as char);
                    cell.fg = Color::Indexed((n * 9 % 256) as u8);
                    cell.bg = Color::Indexed(((n + self.0) % 16) as u8);
                }
            }
        }
    }

    struct SparseFrame(usize);

    impl Widget for SparseFrame {
        fn render(self, area: Rect, buf: &mut Buffer) {
            if area.height == 0 {
                return;
            }
            let y = area.top() + (self.0 % area.height as usize) as u16;
            for x in area.left()..area.right() {
                let n = (x as usize + self.0) % 26;
                buf[(x, y)].set_char((b'A' + n as u8) as char);
            }
        }
    }

    const LOREM: &str = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod \
        tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud \
        exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. ";

    async fn http_bench(frames: usize) -> Result<(), Box<dyn std::error::Error>> {
        use http_body_util::BodyExt;
        let origin = rattery_app::origin().ok_or("http mode needs --origin")?;
        let mut durations = Vec::with_capacity(frames);
        let started = Instant::now();
        for _ in 0..frames {
            let t = Instant::now();
            let request = http::Request::builder()
                .uri(format!("{origin}/"))
                .body(http_body_util::Empty::<bytes::Bytes>::new())?;
            let request = wasip3::http_compat::http_into_wasi_request(request)
                .map_err(|e| format!("{e:?}"))?;
            let response = wasip3::http::client::send(request)
                .await
                .map_err(|e| format!("{e:?}"))?;
            let response = wasip3::http_compat::http_from_wasi_response(response)
                .map_err(|e| format!("{e:?}"))?;
            let _ = response
                .into_body()
                .collect()
                .await
                .map_err(|e| format!("{e:?}"))?;
            durations.push(t.elapsed());
        }
        let total = started.elapsed();
        durations.sort();
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        let pct = |p: f64| durations[((durations.len() - 1) as f64 * p) as usize];
        println!(
            "bench mode=http frames={frames} screen_cells=0 avg_ms={:.3} p50_ms={:.3} p95_ms={:.3} max_ms={:.3} fps={:.0}",
            ms(total / frames.max(1) as u32),
            ms(pct(0.5)),
            ms(pct(0.95)),
            ms(*durations.last().unwrap()),
            frames as f64 / total.as_secs_f64().max(1e-9)
        );
        Ok(())
    }

    pub async fn run(mut terminal: Terminal) -> Result<(), Box<dyn std::error::Error>> {
        let Params {
            frames,
            mode,
            work,
            reload,
        } = params();
        if mode == "http" {
            return http_bench(frames).await;
        }
        if mode == "hover" {
            use bench_app::hover::Tiles;
            use rattery_app::event;
            let mut hover = None;
            let mut events = 0usize;
            let mut mouse_events = 0usize;
            let mut durations = Vec::new();
            let mut frame = 0usize;
            loop {
                let t = Instant::now();
                terminal.draw(|f| {
                    for _ in 1..work {
                        // Rendered and discarded: the cost without the cells.
                        let mut scratch = f.buffer_mut().clone();
                        rattery_app::ratatui::widgets::Widget::render(
                            Tiles { hover, frame },
                            f.area(),
                            &mut scratch,
                        );
                    }
                    f.render_widget(Tiles { hover, frame }, f.area())
                })?;
                durations.push(t.elapsed());
                frame += 1;
                match event::next().await {
                    Event::Mouse(m) => {
                        events += 1;
                        mouse_events += 1;
                        hover = Some((m.column, m.row));
                    }
                    Event::Key(k) if k.code == KeyCode::Char('q') => break,
                    _ => events += 1,
                }
            }
            durations.sort();
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            let pct = |p: f64| durations[((durations.len() - 1) as f64 * p) as usize];
            let total: std::time::Duration = durations.iter().sum();
            println!(
                "bench mode=hover work={work} frames={frame} events={events} mouse_events={mouse_events} avg_ms={:.3} p50_ms={:.3} p95_ms={:.3} max_ms={:.3} draw_total_ms={:.1}",
                ms(total / frame.max(1) as u32),
                ms(pct(0.5)),
                ms(pct(0.95)),
                ms(*durations.last().unwrap()),
                ms(total)
            );
            return Ok(());
        }
        if mode == "storage" {
            use rattery_app::storage;
            let runs = storage::get_string("runs")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0)
                + 1;
            storage::set_string("runs", &runs.to_string())?;
            let (used, quota) = storage::usage();
            let big = storage::set("big", &vec![0; quota as usize + 1]);
            let many = (0..10_000)
                .map(|i| storage::set(&format!("k{i}"), &[]))
                .find(Result::is_err);
            rattery_app::log::warn!("escape \u{1b}]0;pwned\u{7} in a log line");
            for i in 0..3000 {
                rattery_app::log::trace!("flood {i}");
            }
            println!(
                "storage runs={runs} used={used} quota={quota} big={big:?} many={many:?} keys={}",
                storage::keys().len()
            );
            storage::clear();
            storage::set_string("runs", &runs.to_string())?;
            drop(terminal);
            return Ok(());
        }
        if mode == "evil" {
            // A hostile app skips ratatui (whose buffer refuses such symbols)
            // and talks to the terminal binding directly.
            use rattery_app::bindings::terminal as t;
            rattery_app::set_title("safe\u{1b}]0;pwned\u{7}title");
            let cell = |symbol: &'static str| t::Cell {
                symbol,
                fg: t::Color::Reset,
                bg: t::Color::Reset,
                underline_color: t::Color::Reset,
                modifier: t::Modifier::empty(),
            };
            let rows = ["\u{1b}[2J", "\u{7}", "ok", "\u{9b}31m", "é"];
            let mut updates: Vec<t::CellUpdate> = rows
                .iter()
                .enumerate()
                .map(|(y, symbol)| t::CellUpdate {
                    x: 0,
                    y: y as u16,
                    cell: cell(symbol),
                })
                .collect();
            updates.push(t::CellUpdate {
                x: 60000,
                y: 60000,
                cell: cell("far"),
            });
            t::draw(&updates);
            t::flush();
            drop(terminal);
            println!("stdout\u{1b}[2J\u{7}injection");
            return Err("error\u{1b}[31mred".into());
        }
        let mut durations = Vec::with_capacity(frames);
        let mut cells = 0usize;
        let started = Instant::now();
        for frame in 0..frames {
            let t = Instant::now();
            let completed = terminal.draw(|f| {
                let area = f.area();
                match mode.as_str() {
                    "sparse" => f.render_widget(SparseFrame(frame), area),
                    "text" => {
                        let text = format!("frame {frame}: {}", LOREM.repeat(6));
                        f.render_widget(
                            Paragraph::new(text)
                                .wrap(Wrap { trim: true })
                                .block(Block::bordered().title(" bench ")),
                            area,
                        );
                    }
                    _ => f.render_widget(FullFrame(frame), area),
                }
            })?;
            durations.push(t.elapsed());
            cells = completed.area.area() as usize;
        }
        let total = started.elapsed();
        durations.sort();
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        let pct = |p: f64| durations[((durations.len() - 1) as f64 * p) as usize];
        let avg = total / frames.max(1) as u32;
        println!(
            "bench mode={mode} frames={frames} screen_cells={cells} avg_ms={:.3} p50_ms={:.3} p95_ms={:.3} max_ms={:.3} fps={:.0}",
            ms(avg),
            ms(pct(0.5)),
            ms(pct(0.95)),
            ms(*durations.last().unwrap()),
            frames as f64 / total.as_secs_f64().max(1e-9)
        );
        if reload {
            rattery_app::update::reload();
        }
        Ok(())
    }
}

#[cfg(target_os = "wasi")]
use bench::run;
