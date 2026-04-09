//! Interactive session picker for the `haven` CLI.
//!
//! When the user runs `haven` with no arguments and has two or more sessions,
//! we render a small list: arrow keys move the selection, Enter attaches,
//! `n` creates a new session, `q` / Esc / Ctrl-C quits. One screen, no
//! scrolling — sessions list is always short in practice and a scrolling
//! picker is more complexity than it's worth here.

use anyhow::{anyhow, Result};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use haven_protocol::{SessionInfo, SessionStatus};
use std::io::{stdout, Write};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerResult {
    /// Attach to the session at this index in the list.
    Attach(usize),
    /// User pressed `n` — create a new session.
    New,
    /// User pressed `q` / Esc / Ctrl-C.
    Quit,
}

/// Show the picker. Takes ownership of the terminal briefly (alt screen, raw
/// mode) and restores it on every return path.
pub fn pick(sessions: &[SessionInfo]) -> Result<PickerResult> {
    if sessions.is_empty() {
        return Ok(PickerResult::New);
    }

    let mut stdout = stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;

    // Everything below this point MUST go through `cleanup` on the way out,
    // otherwise the user's terminal is left in raw mode / alt screen.
    let result = (|| -> Result<PickerResult> {
        let mut selected = 0usize;
        loop {
            render(&mut stdout, sessions, selected)?;
            stdout.flush()?;

            // Block on the next key event, with a short poll loop so Ctrl-C
            // still feels instant.
            if !event::poll(Duration::from_millis(500))? {
                continue;
            }
            if let Event::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            }) = event::read()?
            {
                if kind != KeyEventKind::Press {
                    continue;
                }
                match (code, modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL)
                    | (KeyCode::Char('q'), _)
                    | (KeyCode::Esc, _) => {
                        return Ok(PickerResult::Quit);
                    }
                    (KeyCode::Char('n'), _) | (KeyCode::Char('N'), _) => {
                        return Ok(PickerResult::New);
                    }
                    (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                        if selected > 0 {
                            selected -= 1;
                        }
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                        if selected + 1 < sessions.len() {
                            selected += 1;
                        }
                    }
                    (KeyCode::Home, _) | (KeyCode::Char('g'), _) => selected = 0,
                    (KeyCode::End, _) | (KeyCode::Char('G'), _) => {
                        selected = sessions.len() - 1;
                    }
                    (KeyCode::Enter, _) => {
                        return Ok(PickerResult::Attach(selected));
                    }
                    (KeyCode::Char(c), _) if c.is_ascii_digit() => {
                        // Press `1`..`9` to jump straight to that session.
                        let idx = c.to_digit(10).unwrap() as usize;
                        if idx >= 1 && idx <= sessions.len() {
                            return Ok(PickerResult::Attach(idx - 1));
                        }
                    }
                    _ => {}
                }
            }
        }
    })();

    // Cleanup, regardless of how we got here.
    let _ = execute!(stdout, cursor::Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();

    result.map_err(|e| anyhow!("picker error: {e}"))
}

fn render(
    stdout: &mut std::io::Stdout,
    sessions: &[SessionInfo],
    selected: usize,
) -> Result<()> {
    queue!(stdout, Clear(ClearType::All), cursor::MoveTo(0, 0))?;

    // Header.
    queue!(
        stdout,
        SetAttribute(Attribute::Bold),
        SetForegroundColor(Color::Green),
        Print("  haven"),
        ResetColor,
        Print("   "),
        SetForegroundColor(Color::DarkGrey),
        Print(format!("({} session{})", sessions.len(), if sessions.len() == 1 { "" } else { "s" })),
        ResetColor,
        Print("\r\n\r\n"),
    )?;

    // Rows.
    for (i, s) in sessions.iter().enumerate() {
        let is_selected = i == selected;
        let marker = if is_selected { "▶ " } else { "  " };
        let index_label = format!("{:>2}", i + 1);

        if is_selected {
            queue!(stdout, SetAttribute(Attribute::Reverse))?;
        }

        queue!(
            stdout,
            Print("  "),
            SetForegroundColor(if is_selected { Color::Green } else { Color::DarkGrey }),
            Print(marker),
            ResetColor,
        )?;

        if is_selected {
            queue!(stdout, SetAttribute(Attribute::Bold))?;
        }
        queue!(
            stdout,
            SetForegroundColor(Color::DarkGrey),
            Print(format!("{}  ", index_label)),
            ResetColor,
        )?;

        queue!(
            stdout,
            Print(format!("{:<24}", truncate(&s.name, 24))),
            Print("  "),
            status_badge(s.status),
            Print("  "),
            SetForegroundColor(Color::DarkGrey),
            Print(
                s.cwd
                    .as_ref()
                    .map(|p| truncate(&p.display().to_string(), 48))
                    .unwrap_or_else(|| "-".into())
            ),
            ResetColor,
        )?;

        if is_selected {
            queue!(stdout, SetAttribute(Attribute::NormalIntensity), SetAttribute(Attribute::Reset))?;
        }

        queue!(stdout, Print("\r\n"))?;
    }

    // Footer.
    queue!(
        stdout,
        Print("\r\n"),
        SetForegroundColor(Color::DarkGrey),
        Print("  ↑/↓ move   enter attach   n new   q quit\r\n"),
        ResetColor,
    )?;

    Ok(())
}

fn status_badge(status: SessionStatus) -> impl crossterm::Command {
    struct Badge(SessionStatus);
    impl crossterm::Command for Badge {
        fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
            let (color, label) = match self.0 {
                SessionStatus::Running => (Color::Green, "running"),
                SessionStatus::Idle => (Color::DarkGreen, "idle   "),
                SessionStatus::Exited => (Color::Red, "exited "),
                SessionStatus::Suspended => (Color::Yellow, "paused "),
            };
            SetForegroundColor(color).write_ansi(f)?;
            Print(label).write_ansi(f)?;
            ResetColor.write_ansi(f)?;
            Ok(())
        }
    }
    Badge(status)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
