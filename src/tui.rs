//! Optional terminal dashboard (`HOMIE_TUI=1`).
//!
//! A passive observer: it never drives the bot. The `EventSub` loop pushes
//! [`UiEvent`]s onto a `tokio::sync::broadcast` channel and the bot's
//! `tracing` output is redirected into a [`LogBuffer`]; this module renders
//! both. Layout: a **Chat** panel and an **Activity** panel side by side on
//! top (channel points, subs, follows, raids), a **Logs** panel underneath.
//!
//! The render loop is synchronous and runs on its own `std::thread` so the
//! blocking terminal I/O never sits on the tokio runtime. It owns a
//! [`TerminalGuard`] that restores the terminal on the way out (including on
//! panic, via `Drop`).

use std::{
    collections::VecDeque,
    io::{self, IsTerminal, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem},
};
use tokio::sync::{broadcast, watch};

/// Max lines kept per panel (older lines are evicted).
const PANEL_CAP: usize = 1000;
/// Max log lines kept in the shared buffer.
const LOG_CAP: usize = 1000;

/// An observable event, broadcast by the `EventSub` loop to the dashboard.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// A chat message. `privileged` = broadcaster or moderator (for colour).
    Chat {
        user: String,
        privileged: bool,
        text: String,
    },
    /// A channel-point reward was redeemed.
    Redemption {
        user: String,
        reward: String,
        cost: i64,
        input: Option<String>,
    },
    /// A new (non-gifted) subscription.
    Sub { user: String, tier: String },
    /// A resub with the accumulated month count and optional message.
    Resub {
        user: String,
        tier: String,
        months: i64,
        message: Option<String>,
    },
    /// A batch of gifted subs.
    GiftSub {
        gifter: String,
        total: i64,
        tier: String,
    },
    /// A new follower.
    Follow { user: String },
    /// An incoming raid.
    Raid { from: String, viewers: i64 },
}

/// Shared, bounded ring of formatted log lines plus an "active" flag.
///
/// Until the dashboard is on screen, captured `tracing` output goes straight
/// to `stderr` so startup stays visible on the normal terminal — Maison
/// login, the interactive device-code prompt, any early error. Once the TUI
/// takes over the alternate screen it flips `active` and lines are buffered
/// for the Logs panel instead. Cheap to clone (just `Arc`s); also implements
/// `tracing_subscriber`'s `MakeWriter`.
#[derive(Clone, Default)]
pub struct LogBuffer {
    ring: Arc<Mutex<VecDeque<String>>>,
    active: Arc<AtomicBool>,
}

impl LogBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Route captured logs into the in-memory ring (TUI on screen).
    pub fn activate(&self) {
        self.active.store(true, Ordering::SeqCst);
    }

    /// Route captured logs back to stderr (TUI gone / not up yet).
    pub fn deactivate(&self) {
        self.active.store(false, Ordering::SeqCst);
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    fn push(&self, line: String) {
        if let Ok(mut buf) = self.ring.lock() {
            if buf.len() >= LOG_CAP {
                buf.pop_front();
            }
            buf.push_back(line);
        }
    }

    fn snapshot(&self) -> Vec<String> {
        self.ring
            .lock()
            .map(|b| b.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// Writer handed to the `tracing` fmt layer. Accumulates a single event's
/// bytes and, on drop (the fmt layer drops the writer per event), splits
/// them into lines and appends them to the [`LogBuffer`].
pub struct LogWriter {
    buf: LogBuffer,
    pending: Vec<u8>,
}

impl Write for LogWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_active() {
            let text = String::from_utf8_lossy(&self.pending);
            for line in text.split('\n') {
                let trimmed = line.trim_end();
                if !trimmed.is_empty() {
                    self.buf.push(trimmed.to_string());
                }
            }
        } else {
            // Dashboard not on screen yet: keep startup logs visible.
            let mut err = io::stderr();
            err.write_all(&self.pending)?;
            err.flush()?;
        }
        self.pending.clear();
        Ok(())
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter {
            buf: self.clone(),
            pending: Vec::new(),
        }
    }
}

/// A scrollable panel: newest line at the back, `scroll_back` lines lifted
/// off the bottom (0 = following the live tail).
struct Panel {
    lines: VecDeque<Line<'static>>,
    scroll_back: usize,
}

impl Panel {
    fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            scroll_back: 0,
        }
    }

    fn push(&mut self, line: Line<'static>) {
        if self.lines.len() >= PANEL_CAP {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Lines to draw for an inner viewport of `height` rows.
    fn visible(&self, height: usize) -> Vec<ListItem<'static>> {
        let len = self.lines.len();
        let back = self.scroll_back.min(len.saturating_sub(1));
        let end = len.saturating_sub(back);
        let start = end.saturating_sub(height);
        self.lines
            .iter()
            .skip(start)
            .take(end - start)
            .cloned()
            .map(ListItem::new)
            .collect()
    }

    fn scroll(&mut self, delta: isize) {
        let max = self.lines.len().saturating_sub(1);
        if delta < 0 {
            self.scroll_back = self
                .scroll_back
                .saturating_sub(delta.unsigned_abs())
                .min(max);
        } else {
            self.scroll_back = (self.scroll_back + usize::try_from(delta).unwrap_or(0)).min(max);
        }
    }
}

struct App {
    chat: Panel,
    activity: Panel,
    logs: LogBuffer,
    quit: bool,
}

impl App {
    fn new(logs: LogBuffer) -> Self {
        Self {
            chat: Panel::new(),
            activity: Panel::new(),
            logs,
            quit: false,
        }
    }

    fn ingest(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Chat {
                user,
                privileged,
                text,
            } => {
                let name_style = if privileged {
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan)
                };
                self.chat.push(Line::from(vec![
                    Span::styled(user, name_style),
                    Span::raw(": "),
                    Span::raw(text),
                ]));
            }
            UiEvent::Redemption {
                user,
                reward,
                cost,
                input,
            } => {
                let mut spans = vec![
                    Span::styled("★ ", Style::default().fg(Color::Yellow)),
                    Span::styled(user, Style::default().fg(Color::Yellow)),
                    Span::raw(" → "),
                    Span::styled(reward, Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(format!(" ({cost} pts)")),
                ];
                if let Some(input) = input {
                    spans.push(Span::styled(
                        format!("  “{input}”"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                self.activity.push(Line::from(spans));
            }
            UiEvent::Sub { user, tier } => self.activity.push(Self::accent(
                "☆ ",
                Color::Green,
                format!("{user} subscribed ({tier})"),
            )),
            UiEvent::Resub {
                user,
                tier,
                months,
                message,
            } => {
                let mut text = format!("{user} resubscribed — {months} months ({tier})");
                if let Some(msg) = message {
                    text.push_str(": ");
                    text.push_str(&msg);
                }
                self.activity.push(Self::accent("☆ ", Color::Green, text));
            }
            UiEvent::GiftSub {
                gifter,
                total,
                tier,
            } => self.activity.push(Self::accent(
                "🎁 ",
                Color::LightGreen,
                format!("{gifter} gifted {total} sub(s) ({tier})"),
            )),
            UiEvent::Follow { user } => {
                self.activity
                    .push(Self::accent("+ ", Color::Blue, format!("{user} followed")));
            }
            UiEvent::Raid { from, viewers } => self.activity.push(Self::accent(
                "➜ ",
                Color::LightRed,
                format!("{from} raided with {viewers} viewer(s)"),
            )),
        }
    }

    fn accent(marker: &'static str, color: Color, text: String) -> Line<'static> {
        Line::from(vec![
            Span::styled(marker, Style::default().fg(color)),
            Span::styled(text, Style::default().fg(color)),
        ])
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Up => self.chat.scroll(1),
            KeyCode::Down => self.chat.scroll(-1),
            KeyCode::PageUp => self.chat.scroll(10),
            KeyCode::PageDown => self.chat.scroll(-10),
            KeyCode::End => self.chat.scroll_back = 0,
            _ => {}
        }
    }

    fn render(&self, frame: &mut Frame) {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
            .split(frame.area());

        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(outer[0]);

        let chat_title = if self.chat.scroll_back == 0 {
            " Chat ".to_string()
        } else {
            format!(" Chat  [↑{} — End=live] ", self.chat.scroll_back)
        };
        render_panel(frame, top[0], &chat_title, self.chat.visible(rows(top[0])));
        render_panel(
            frame,
            top[1],
            " Activity ",
            self.activity.visible(rows(top[1])),
        );

        let log_rows = rows(outer[1]);
        let logs = self.logs.snapshot();
        let start = logs.len().saturating_sub(log_rows);
        let log_items: Vec<ListItem> = logs[start..].iter().map(|l| log_line(l)).collect();
        render_panel(
            frame,
            outer[1],
            " Logs  ·  q/Esc quit · ↑↓/PgUp/PgDn scroll chat ",
            log_items,
        );
    }
}

/// Inner (border-excluded) row count of a rect.
fn rows(area: ratatui::layout::Rect) -> usize {
    usize::from(area.height.saturating_sub(2))
}

fn render_panel(frame: &mut Frame, area: ratatui::layout::Rect, title: &str, items: Vec<ListItem>) {
    let list = List::new(items).block(Block::bordered().title(title.to_string()));
    frame.render_widget(list, area);
}

/// Colourise a captured tracing line by its level token.
fn log_line(raw: &str) -> ListItem<'static> {
    let color = if raw.contains("ERROR") {
        Color::Red
    } else if raw.contains(" WARN") {
        Color::Yellow
    } else if raw.contains("DEBUG") || raw.contains("TRACE") {
        Color::DarkGray
    } else {
        Color::Gray
    };
    ListItem::new(Line::from(Span::styled(
        raw.to_string(),
        Style::default().fg(color),
    )))
}

/// Restores the terminal on drop — even if the render loop panics — and
/// flips log routing back to stderr so shutdown/panic logs stay visible.
struct TerminalGuard {
    logs: LogBuffer,
}

impl TerminalGuard {
    fn enter(logs: LogBuffer) -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        logs.activate();
        Ok(Self { logs })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.logs.deactivate();
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Signals `shutdown` once, on drop, so *every* exit path of [`run`] — clean
/// quit, setup/I/O error, or panic — also stops the rest of the bot instead
/// of leaving it running headless and silent.
struct SignalOnDrop(Arc<watch::Sender<bool>>);

impl Drop for SignalOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

/// Run the dashboard until the user quits or `shutdown` flips to `true`.
///
/// Blocking; call this on a dedicated `std::thread`. On exit it signals
/// `shutdown` so the rest of the bot stops too.
///
/// # Errors
/// Returns an error if stdout is not an interactive terminal, or on
/// terminal setup / draw I/O failure. The bot is signalled to stop on any
/// of these (see [`SignalOnDrop`]).
pub fn run(
    mut events: broadcast::Receiver<UiEvent>,
    logs: LogBuffer,
    shutdown: Arc<watch::Sender<bool>>,
) -> io::Result<()> {
    // Stop the rest of the bot whenever this function returns, by any path.
    let stop = SignalOnDrop(shutdown);

    if !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "HOMIE_TUI is set but stdout is not an interactive terminal \
             (run homie directly in a terminal, not under a pipe/redirect)",
        ));
    }

    let _guard = TerminalGuard::enter(logs.clone())?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App::new(logs);
    let mut shutdown_rx = stop.0.subscribe();

    while !app.quit {
        if *shutdown_rx.borrow_and_update() {
            break;
        }

        loop {
            match events.try_recv() {
                Ok(ev) => app.ingest(ev),
                // Empty: nothing more this frame. Lagged: we fell behind, skip
                // the gap. Either way, stop draining until the next frame.
                Err(
                    broadcast::error::TryRecvError::Empty
                    | broadcast::error::TryRecvError::Lagged(_),
                ) => break,
                Err(broadcast::error::TryRecvError::Closed) => {
                    app.quit = true;
                    break;
                }
            }
        }

        terminal.draw(|frame| app.render(frame))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Release {
                    app.on_key(key.code, key.modifiers);
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;

    #[test]
    fn panel_evicts_oldest_past_cap() {
        let mut p = Panel::new();
        for i in 0..(PANEL_CAP + 50) {
            p.push(Line::from(i.to_string()));
        }
        assert_eq!(p.lines.len(), PANEL_CAP);
    }

    #[test]
    fn panel_visible_tails_by_default() {
        let mut p = Panel::new();
        for i in 0..10 {
            p.push(Line::from(i.to_string()));
        }
        // A 3-row viewport with no scroll shows the last 3 lines.
        assert_eq!(p.visible(3).len(), 3);
        assert_eq!(p.scroll_back, 0);
    }

    #[test]
    fn panel_scroll_clamps_both_ends() {
        let mut p = Panel::new();
        for i in 0..5 {
            p.push(Line::from(i.to_string()));
        }
        p.scroll(-100); // below zero clamps to 0
        assert_eq!(p.scroll_back, 0);
        p.scroll(100); // above len-1 clamps
        assert_eq!(p.scroll_back, 4);
    }

    #[test]
    fn log_buffer_is_bounded_and_ordered() {
        let buf = LogBuffer::new();
        for i in 0..(LOG_CAP + 10) {
            buf.push(i.to_string());
        }
        let snap = buf.snapshot();
        assert_eq!(snap.len(), LOG_CAP);
        assert_eq!(snap.last().unwrap(), &(LOG_CAP + 9).to_string());
    }

    #[test]
    fn log_writer_splits_on_newlines() {
        let buf = LogBuffer::new();
        buf.activate(); // otherwise the writer would tee to stderr
        {
            let mut w = buf.make_writer();
            w.write_all(b"line one\nline two\n").unwrap();
        } // drop flushes
        assert_eq!(buf.snapshot(), vec!["line one", "line two"]);
    }

    #[test]
    fn chat_and_activity_route_to_distinct_panels() {
        let mut app = App::new(LogBuffer::new());
        app.ingest(UiEvent::Chat {
            user: "viewer".into(),
            privileged: false,
            text: "hi".into(),
        });
        app.ingest(UiEvent::Follow {
            user: "newbie".into(),
        });
        app.ingest(UiEvent::Raid {
            from: "bigstreamer".into(),
            viewers: 42,
        });
        assert_eq!(app.chat.lines.len(), 1);
        assert_eq!(app.activity.lines.len(), 2);
    }
}
