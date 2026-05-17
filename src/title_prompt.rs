//! Startup modal to edit the Twitch stream title.
//!
//! A clean centred `ratatui` screen (its own alternate screen + raw mode),
//! prefilled with the current title: type to edit, **Enter** to submit,
//! **Esc** / **Ctrl-C** to dismiss and keep the current title. Blocking —
//! run it on a blocking task before the `EventSub` loop / main TUI start.
//! No-op when stdout is not an interactive terminal.

use std::io::{self, IsTerminal};
use std::time::Duration;

use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
};

/// Restores the terminal on drop — even on error/panic.
struct Guard;

impl Guard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Show the editable modal. Returns `Some(text)` if the user submitted
/// (possibly edited) a title, `None` if dismissed or non-interactive.
///
/// # Errors
/// Propagates terminal setup / draw I/O errors.
pub fn prompt(current: &str) -> io::Result<Option<String>> {
    if !io::stdout().is_terminal() {
        return Ok(None);
    }
    let _guard = Guard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut buf = current.to_string();

    loop {
        terminal.draw(|frame| draw(frame, &buf))?;
        // Poll so a stray write repaints itself; also keeps input snappy.
        if event::poll(Duration::from_millis(150))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                match key.code {
                    KeyCode::Enter => return Ok(Some(buf)),
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(None);
                    }
                    KeyCode::Char(c) => buf.push(c),
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    _ => {}
                }
            }
        }
    }
}

fn draw(frame: &mut Frame, buf: &str) {
    let modal = centered(frame.area(), 80, 9);
    let block = Block::bordered().title(" 📺 Edit stream title ");
    let body = vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                buf.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "Enter = save · Esc / Ctrl-C = keep current title",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let para = Paragraph::new(body)
        .block(block)
        .wrap(Wrap { trim: false })
        .alignment(Alignment::Left);
    frame.render_widget(para, modal);
}

/// A `width`×`height` rect centred in `area` (clamped to it).
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let [_, row, _] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(h),
            Constraint::Min(0),
        ])
        .areas(area);
    let [_, cell, _] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(w),
            Constraint::Min(0),
        ])
        .areas(row);
    cell
}
