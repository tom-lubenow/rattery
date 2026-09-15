//! The workloads rendered natively with ratatui: the floor for the same
//! widgets without wasm or the host boundary. On `TestBackend` (pure render
//! cost) or on the terminal it runs in (`--backend terminal`: raw mode,
//! alternate screen, every changed cell written out, as a real app would).
//! The synthetic input is delivered in-process on a timer, and the
//! input-to-frame latency is measured the way the rattery host measures it:
//! from the moment an input arrives to the end of the frame that follows.
//!
//! Usage: bench-native --mode anim|click|drag|hover [--size 200x50]
//!   [--frames N] [--events N] [--interval MS] [--cadence MS] [--work N]
//!   [--backend test|terminal] [--out FILE] [--emit-script FILE]

#![cfg_attr(target_os = "wasi", allow(unused))]

#[cfg(target_os = "wasi")]
fn main() {
    eprintln!("bench-native is the native baseline; build it without --target");
}

#[cfg(not(target_os = "wasi"))]
fn main() {
    native::main()
}

#[cfg(not(target_os = "wasi"))]
mod native {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use bench_app::workloads::{Mode, Workload, script, stream, summary};
    use rattery_app::ratatui::Terminal;
    use rattery_app::ratatui::backend::{Backend, CrosstermBackend, TestBackend};

    struct Args {
        mode: Mode,
        width: u16,
        height: u16,
        frames: usize,
        events: usize,
        interval: Duration,
        cadence: Option<Duration>,
        work: usize,
        terminal: bool,
        out: Option<String>,
        emit_script: Option<String>,
    }

    fn args() -> Args {
        let mut a = Args {
            mode: Mode::Anim,
            width: 200,
            height: 50,
            frames: 300,
            events: 200,
            interval: Duration::from_millis(5),
            cadence: None,
            work: 1,
            terminal: false,
            out: None,
            emit_script: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let value = it.next().unwrap_or_default();
            match flag.as_str() {
                "--mode" => a.mode = Mode::parse(&value).expect("mode anim|click|drag|hover"),
                "--size" => {
                    let (w, h) = value.split_once('x').expect("COLSxROWS");
                    a.width = w.parse().expect("cols");
                    a.height = h.parse().expect("rows");
                }
                "--frames" => a.frames = value.parse().expect("frames"),
                "--events" => a.events = value.parse().expect("events"),
                "--interval" => a.interval = Duration::from_millis(value.parse().expect("ms")),
                "--cadence" => a.cadence = Some(Duration::from_millis(value.parse().expect("ms"))),
                "--work" => a.work = value.parse().expect("work"),
                "--backend" => a.terminal = value == "terminal",
                "--out" => a.out = Some(value),
                "--emit-script" => a.emit_script = Some(value),
                other => panic!("unknown flag {other}"),
            }
        }
        a
    }

    pub fn main() {
        let a = args();
        if let Some(path) = &a.emit_script {
            std::fs::write(
                path,
                script(
                    a.mode,
                    a.width,
                    a.height,
                    a.events,
                    a.interval.as_millis() as u64,
                ),
            )
            .expect("write script");
            return;
        }
        let line = if a.terminal {
            let mut out = std::io::stdout();
            crossterm::terminal::enable_raw_mode().expect("raw mode");
            crossterm::execute!(
                out,
                crossterm::terminal::EnterAlternateScreen,
                crossterm::event::EnableMouseCapture
            )
            .expect("alternate screen");
            let backend = CrosstermBackend::new(out);
            let size = backend.size().expect("size");
            let mut terminal = Terminal::new(backend).expect("terminal");
            let line = run(&a, &mut terminal, size.width, size.height);
            let mut out = std::io::stdout();
            let _ = crossterm::execute!(
                out,
                crossterm::event::DisableMouseCapture,
                crossterm::terminal::LeaveAlternateScreen
            );
            let _ = crossterm::terminal::disable_raw_mode();
            line
        } else {
            let mut terminal =
                Terminal::new(TestBackend::new(a.width, a.height)).expect("terminal");
            run(&a, &mut terminal, a.width, a.height)
        };
        match &a.out {
            Some(path) => {
                let mut f = std::fs::File::create(path).expect("out file");
                writeln!(f, "{line}").expect("write");
            }
            None => println!("{line}"),
        }
    }

    fn run<B: Backend>(a: &Args, terminal: &mut Terminal<B>, width: u16, height: u16) -> String {
        let mut w = Workload::new(a.mode, a.work, width);
        let area = rattery_app::ratatui::layout::Rect::new(0, 0, width, height);
        let mut frames = Vec::new();
        let backend = if a.terminal { "terminal" } else { "test" };
        if a.mode == Mode::Anim {
            // Frames back to back, or one per cadence with the interval between
            // frame starts recorded (the jitter an animation shows).
            let mut intervals = Vec::new();
            let started = Instant::now();
            let mut last_start = None;
            for i in 0..a.frames {
                let start = Instant::now();
                if let Some(last) = last_start {
                    intervals.push(start - last);
                }
                last_start = Some(start);
                w.tick();
                terminal.draw(|f| w.render(f)).expect("draw");
                frames.push(start.elapsed());
                if let Some(cadence) = a.cadence {
                    let next = started + cadence * (i as u32 + 1);
                    if let Some(wait) = next.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                }
            }
            let late = a
                .cadence
                .map_or(0, |c| intervals.iter().filter(|d| **d > c + c / 2).count());
            let cadence_ms = a.cadence.map_or(0, |c| c.as_millis() as u64);
            return format!(
                "native mode=anim backend={backend} cadence_ms={cadence_ms} work={} frames={} {} {} late={late} total_ms={:.1}",
                a.work,
                frames.len(),
                summary("frame", &mut frames),
                summary("interval", &mut intervals),
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        let inputs = stream(a.mode, width, height, a.events);
        terminal.draw(|f| w.render(f)).expect("draw");
        let mut latencies = Vec::new();
        let started = Instant::now();
        for (i, input) in inputs.iter().enumerate() {
            // The input arrives on its schedule, as a terminal would deliver it.
            let due = started + a.interval * (i as u32 + 1);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let arrived = Instant::now();
            w.apply(*input, area);
            w.tick();
            terminal.draw(|f| w.render(f)).expect("draw");
            let done = Instant::now();
            frames.push(done - arrived);
            latencies.push(done - arrived);
        }
        format!(
            "native mode={} backend={backend} work={} frames={} events={} toggles={} {} {} total_ms={:.1}",
            a.mode.name(),
            a.work,
            frames.len(),
            inputs.len(),
            w.toggles,
            summary("frame", &mut frames),
            summary("latency", &mut latencies),
            started.elapsed().as_secs_f64() * 1000.0
        )
    }
}
