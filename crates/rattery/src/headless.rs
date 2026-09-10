//! Scripted input for headless runs.
//!
//! A script is one command per line; blank lines and `#` comments are
//! ignored:
//!
//! ```text
//! sleep 500          # milliseconds
//! key k              # a single character
//! key ctrl-c         # modifiers: ctrl, alt, shift, super, meta
//! key enter          # enter esc up down left right tab backtab backspace
//!                    # delete insert home end pageup pagedown space f1..f24
//! type hello world   # one key event per character
//! paste some text    # a bracketed paste
//! resize 100 30      # columns rows; the app receives a resize event
//! snapshot           # capture the screen into the report
//! ```

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ratatui::backend::{Backend, TestBackend};

use crate::bindings::terminal::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseEvent,
    MouseEventKind, Size, Update,
};
use crate::terminal::{EventQueue, Screen};

/// One line of a headless script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptCommand {
    Sleep(Duration),
    Key {
        code: KeyCode,
        modifiers: KeyModifiers,
    },
    Type(String),
    Paste(String),
    Resize {
        width: u16,
        height: u16,
    },
    Snapshot,
    /// A pointer movement to (column, row).
    MouseMove {
        column: u16,
        row: u16,
    },
    /// `steps` pointer movements along the screen's diagonal, one every
    /// `interval`: a hover benchmark's input.
    Sweep {
        steps: usize,
        interval: Duration,
    },
    /// Tell the app an update is available (with this version string), as
    /// the watcher would. The app's `reload()` then restarts the same
    /// component, which is enough to test the handling.
    Update(Option<String>),
}

/// A list of [`ScriptCommand`]s.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Script(pub Vec<ScriptCommand>);

impl Script {
    /// Parse the text format described in the module docs.
    pub fn parse(text: &str) -> Result<Self> {
        let mut commands = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            commands.push(
                parse_line(line).with_context(|| format!("script line {}: {raw:?}", index + 1))?,
            );
        }
        Ok(Self(commands))
    }

    pub fn push(&mut self, command: ScriptCommand) -> &mut Self {
        self.0.push(command);
        self
    }
}

fn parse_line(line: &str) -> Result<ScriptCommand> {
    let (command, rest) = match line.split_once(char::is_whitespace) {
        Some((c, r)) => (c, r.trim()),
        None => (line, ""),
    };
    Ok(match command {
        "sleep" => {
            let ms: u64 = rest.parse().context("expected milliseconds")?;
            ScriptCommand::Sleep(Duration::from_millis(ms))
        }
        "key" => {
            let (code, modifiers) = parse_key(rest)?;
            ScriptCommand::Key { code, modifiers }
        }
        "type" => ScriptCommand::Type(rest.to_owned()),
        "paste" => ScriptCommand::Paste(rest.to_owned()),
        "resize" => {
            let (w, h) = rest
                .split_once(char::is_whitespace)
                .context("expected `resize W H`")?;
            ScriptCommand::Resize {
                width: w.trim().parse().context("bad width")?,
                height: h.trim().parse().context("bad height")?,
            }
        }
        "snapshot" => ScriptCommand::Snapshot,
        "update" => ScriptCommand::Update((!rest.is_empty()).then(|| rest.to_owned())),
        "mouse" => {
            let (x, y) = rest
                .strip_prefix("move")
                .and_then(|r| r.trim().split_once(char::is_whitespace))
                .context("expected `mouse move X Y`")?;
            ScriptCommand::MouseMove {
                column: x.trim().parse().context("bad column")?,
                row: y.trim().parse().context("bad row")?,
            }
        }
        "sweep" => {
            let (steps, ms) = rest
                .split_once(char::is_whitespace)
                .context("expected `sweep STEPS MS`")?;
            ScriptCommand::Sweep {
                steps: steps.trim().parse().context("bad step count")?,
                interval: Duration::from_millis(ms.trim().parse().context("bad interval")?),
            }
        }
        other => bail!("unknown command {other:?}"),
    })
}

fn parse_key(spec: &str) -> Result<(KeyCode, KeyModifiers)> {
    if spec.is_empty() {
        bail!("expected a key");
    }
    if spec == "-" {
        return Ok((KeyCode::Character('-'), KeyModifiers::empty()));
    }
    let mut parts: Vec<&str> = spec.split('-').collect();
    let name = parts.pop().unwrap();
    let mut modifiers = KeyModifiers::empty();
    for part in parts {
        modifiers |= match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            "super" => KeyModifiers::SUPER,
            "meta" => KeyModifiers::META,
            other => bail!("unknown modifier {other:?}"),
        };
    }
    let code = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "space" => KeyCode::Character(' '),
        lower => {
            if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                KeyCode::F(n)
            } else {
                let mut chars = name.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => KeyCode::Character(c),
                    _ => bail!("unknown key {name:?}"),
                }
            }
        }
    };
    Ok((code, modifiers))
}

fn key_event(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    })
}

fn mouse_move(column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

/// Feed the script to the app.
pub async fn run_script(
    script: Script,
    queue: Arc<EventQueue>,
    backend: Arc<Mutex<TestBackend>>,
    snapshots: Arc<Mutex<Vec<Screen>>>,
) {
    for command in script.0 {
        match command {
            ScriptCommand::Sleep(duration) => tokio::time::sleep(duration).await,
            ScriptCommand::Key { code, modifiers } => queue.push(key_event(code, modifiers)),
            ScriptCommand::Type(text) => {
                for c in text.chars() {
                    let modifiers = if c.is_uppercase() {
                        KeyModifiers::SHIFT
                    } else {
                        KeyModifiers::empty()
                    };
                    queue.push(key_event(KeyCode::Character(c), modifiers));
                }
            }
            ScriptCommand::Paste(text) => queue.push(Event::Paste(text)),
            ScriptCommand::Resize { width, height } => {
                backend.lock().unwrap().resize(width, height);
                queue.push(Event::Resize(Size { width, height }));
            }
            ScriptCommand::Snapshot => {
                let screen = Screen::from_backend(&backend.lock().unwrap());
                snapshots.lock().unwrap().push(screen);
            }
            ScriptCommand::Update(version) => queue.push(Event::UpdateAvailable(Update {
                version,
                deadline_ms: None,
            })),
            ScriptCommand::MouseMove { column, row } => queue.push(mouse_move(column, row)),
            ScriptCommand::Sweep { steps, interval } => {
                let size = backend.lock().unwrap().size().unwrap_or_default();
                for step in 0..steps {
                    let t = step as f64 / steps.max(1) as f64;
                    queue.push(mouse_move(
                        (t * size.width.saturating_sub(1) as f64) as u16,
                        (t * size.height.saturating_sub(1) as f64) as u16,
                    ));
                    tokio::time::sleep(interval).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        let script = Script::parse(
            "# comment\nsleep 250\nkey ctrl-c\nkey Q\nkey f12\nkey -\ntype hi\nresize 100 30\nsnapshot\nmouse move 3 4\nsweep 10 5\nupdate v2\n",
        )
        .unwrap();
        assert_eq!(
            script.0,
            vec![
                ScriptCommand::Sleep(Duration::from_millis(250)),
                ScriptCommand::Key {
                    code: KeyCode::Character('c'),
                    modifiers: KeyModifiers::CONTROL
                },
                ScriptCommand::Key {
                    code: KeyCode::Character('Q'),
                    modifiers: KeyModifiers::empty()
                },
                ScriptCommand::Key {
                    code: KeyCode::F(12),
                    modifiers: KeyModifiers::empty()
                },
                ScriptCommand::Key {
                    code: KeyCode::Character('-'),
                    modifiers: KeyModifiers::empty()
                },
                ScriptCommand::Type("hi".into()),
                ScriptCommand::Resize {
                    width: 100,
                    height: 30
                },
                ScriptCommand::Snapshot,
                ScriptCommand::MouseMove { column: 3, row: 4 },
                ScriptCommand::Sweep {
                    steps: 10,
                    interval: Duration::from_millis(5),
                },
                ScriptCommand::Update(Some("v2".into())),
            ]
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(Script::parse("jump 3").is_err());
        assert!(Script::parse("sleep soon").is_err());
        assert!(Script::parse("key hyper-q").is_err());
        assert!(Script::parse("key what").is_err());
        assert!(Script::parse("resize 80").is_err());
    }
}
