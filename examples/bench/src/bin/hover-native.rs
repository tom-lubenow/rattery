//! The hover benchmark rendered natively on ratatui's `TestBackend`: the
//! floor for the same widget without wasm or the host boundary. Sweeps the
//! pointer across the screen once per frame, `frames` times.
//! Usage: `hover-native [frames] [COLSxROWS] [work]`.

use std::time::Instant;

use bench_app::hover::Tiles;
use rattery_app::ratatui::Terminal;
use rattery_app::ratatui::backend::TestBackend;

fn main() {
    let frames: usize = std::env::args()
        .nth(1)
        .and_then(|f| f.parse().ok())
        .unwrap_or(400);
    let (width, height) = std::env::args()
        .nth(2)
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((200u16, 50u16));
    let work: usize = std::env::args()
        .nth(3)
        .and_then(|w| w.parse().ok())
        .unwrap_or(1);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut durations = Vec::with_capacity(frames);
    let started = Instant::now();
    for frame in 0..frames {
        let t = frame as f64 / frames.max(1) as f64;
        let hover = Some((
            (t * (width - 1) as f64) as u16,
            (t * (height - 1) as f64) as u16,
        ));
        let at = Instant::now();
        terminal
            .draw(|f| {
                for _ in 1..work {
                    let mut scratch = f.buffer_mut().clone();
                    rattery_app::ratatui::widgets::Widget::render(
                        Tiles { hover, frame },
                        f.area(),
                        &mut scratch,
                    );
                }
                f.render_widget(Tiles { hover, frame }, f.area())
            })
            .unwrap();
        durations.push(at.elapsed());
    }
    let total = started.elapsed();
    durations.sort();
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let pct = |p: f64| durations[((durations.len() - 1) as f64 * p) as usize];
    println!(
        "native mode=hover work={work} frames={frames} screen_cells={} avg_ms={:.3} p50_ms={:.3} p95_ms={:.3} max_ms={:.3}",
        width as usize * height as usize,
        ms(total / frames.max(1) as u32),
        ms(pct(0.5)),
        ms(pct(0.95)),
        ms(*durations.last().unwrap())
    );
}
