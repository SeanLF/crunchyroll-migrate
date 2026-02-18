use crossterm::{
    ExecutableCommand,
    event::{self, Event, KeyCode},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    layout::{Constraint, Direction, Layout},
    prelude::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph},
};
use std::io::{self, IsTerminal};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub data_type: DataType,
    pub total: usize,
    pub processed: usize,
    pub added: usize,
    pub skipped: usize,
    pub already_present: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DataType {
    Watchlist,
    History,
    Crunchylists,
    Ratings,
}

impl std::fmt::Display for DataType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataType::Watchlist => write!(f, "Watchlist"),
            DataType::History => write!(f, "History"),
            DataType::Crunchylists => write!(f, "Crunchylists"),
            DataType::Ratings => write!(f, "Ratings"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub icon: char,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Progress(ProgressUpdate),
    Log(LogEntry),
    Done,
}

pub fn is_tty() -> bool {
    io::stdout().is_terminal()
}

pub struct DashboardState {
    pub operation: String,
    pub account: String,
    pub profile: String,
    pub started: Instant,
    pub progress: [Option<ProgressUpdate>; 4],
    pub phase_started: [Option<Instant>; 4],
    pub log: Vec<LogEntry>,
    pub done: bool,
}

impl DashboardState {
    fn new(operation: &str, account: &str, profile: &str) -> Self {
        Self {
            operation: operation.to_string(),
            account: account.to_string(),
            profile: profile.to_string(),
            started: Instant::now(),
            progress: [None, None, None, None],
            phase_started: [None, None, None, None],
            log: Vec::new(),
            done: false,
        }
    }

    fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::Progress(p) => {
                let idx = match p.data_type {
                    DataType::Watchlist => 0,
                    DataType::Crunchylists => 1,
                    DataType::Ratings => 2,
                    DataType::History => 3,
                };
                if self.phase_started[idx].is_none() {
                    self.phase_started[idx] = Some(Instant::now());
                }
                self.progress[idx] = Some(p);
            }
            UiEvent::Log(entry) => {
                self.log.push(entry);
                // Keep last 100 entries
                if self.log.len() > 100 {
                    self.log.drain(..self.log.len() - 100);
                }
            }
            UiEvent::Done => {
                self.done = true;
            }
        }
    }

    fn eta(&self, idx: usize) -> Option<Duration> {
        let p = self.progress[idx].as_ref()?;
        let start = self.phase_started[idx]?;
        if p.total == 0 || p.processed == 0 || p.processed >= p.total {
            return None;
        }
        let elapsed = start.elapsed().as_secs_f64();
        let rate = p.processed as f64 / elapsed;
        let remaining = (p.total - p.processed) as f64 / rate;
        Some(Duration::from_secs_f64(remaining))
    }
}

/// Sender handle for operations to report progress.
#[derive(Clone)]
pub struct ProgressReporter {
    tx: mpsc::UnboundedSender<UiEvent>,
    is_tty: bool,
}

impl ProgressReporter {
    pub fn progress(&self, update: ProgressUpdate) {
        if self.is_tty {
            let _ = self.tx.send(UiEvent::Progress(update));
        }
    }

    pub fn log_success(&self, message: &str) {
        if self.is_tty {
            let _ = self.tx.send(UiEvent::Log(LogEntry {
                icon: '\u{2713}',
                message: message.to_string(),
            }));
        } else {
            println!("  + {}", message);
        }
    }

    pub fn log_skip(&self, message: &str) {
        if self.is_tty {
            let _ = self.tx.send(UiEvent::Log(LogEntry {
                icon: '-',
                message: message.to_string(),
            }));
        } else {
            println!("  - {}", message);
        }
    }

    pub fn log_error(&self, message: &str) {
        if self.is_tty {
            let _ = self.tx.send(UiEvent::Log(LogEntry {
                icon: 'x',
                message: message.to_string(),
            }));
        } else {
            eprintln!("  x {}", message);
        }
    }

    pub fn done(&self) {
        let _ = self.tx.send(UiEvent::Done);
    }
}

/// Handle to wait for the dashboard thread to finish and restore the terminal.
pub struct DashboardHandle {
    join: Option<std::thread::JoinHandle<()>>,
}

impl DashboardHandle {
    /// Block until the TUI thread has exited and restored the terminal.
    pub fn wait(mut self) {
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

/// Run the dashboard in a background task, returning a ProgressReporter and a handle.
/// When the operation is done, call `reporter.done()` then `handle.wait()` to ensure
/// the terminal is restored before continuing.
pub fn start_dashboard(
    operation: &str,
    account: &str,
    profile: &str,
) -> (ProgressReporter, DashboardHandle) {
    let (tx, rx) = mpsc::unbounded_channel();
    let tty = is_tty();

    if tty {
        let state = Arc::new(Mutex::new(DashboardState::new(operation, account, profile)));
        let state_clone = state.clone();

        // Spawn a thread (not tokio task) for the terminal UI to avoid blocking the async runtime
        let join = std::thread::spawn(move || {
            if let Err(e) = run_tui(state_clone, rx) {
                eprintln!("UI error: {}", e);
            }
        });

        // Give the TUI thread a moment to set up
        std::thread::sleep(Duration::from_millis(50));

        (
            ProgressReporter { tx, is_tty: true },
            DashboardHandle { join: Some(join) },
        )
    } else {
        // Non-TTY: just drain events in background
        tokio::spawn(async move {
            let mut rx = rx;
            while let Some(_event) = rx.recv().await {
                // Events handled inline by ProgressReporter print methods
            }
        });

        (
            ProgressReporter { tx, is_tty: false },
            DashboardHandle { join: None },
        )
    }
}

fn run_tui(
    state: Arc<Mutex<DashboardState>>,
    mut rx: mpsc::UnboundedReceiver<UiEvent>,
) -> io::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut scroll_offset: usize = 0;

    loop {
        // Process all pending events
        while let Ok(event) = rx.try_recv() {
            let mut s = state.lock().unwrap();
            s.apply(event);
        }

        terminal.draw(|f| {
            let s = state.lock().unwrap();
            scroll_offset = draw_dashboard(f, &s, scroll_offset);
        })?;

        let done = state.lock().unwrap().done;
        if done {
            // Show final state for a moment
            std::thread::sleep(Duration::from_secs(1));
            break;
        }

        // Check for keypresses
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    break;
                }
                KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    break;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    scroll_offset = scroll_offset.saturating_add(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    scroll_offset = scroll_offset.saturating_sub(1);
                }
                KeyCode::PageUp => {
                    scroll_offset = scroll_offset.saturating_add(10);
                }
                KeyCode::PageDown => {
                    scroll_offset = scroll_offset.saturating_sub(10);
                }
                KeyCode::End => {
                    scroll_offset = 0;
                }
                KeyCode::Home => {
                    let s = state.lock().unwrap();
                    scroll_offset = s.log.len();
                }
                _ => {}
            }
        }
    }

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    let s = state.lock().unwrap();
    let is_done = s.done;
    print_final_summary(&s);
    drop(s);

    if !is_done {
        // User quit mid-operation -- kill the process so the import actually stops
        std::process::exit(0);
    }

    Ok(())
}

/// Draw the dashboard and return the clamped scroll offset for the log panel.
/// Callers should write back the returned value to avoid dead zones in scroll input.
fn draw_dashboard(f: &mut Frame, state: &DashboardState, scroll_offset: usize) -> usize {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // header
            Constraint::Length(8), // progress gauges
            Constraint::Min(6),    // log
            Constraint::Length(3), // stats bar
        ])
        .split(f.area());

    // Header
    let elapsed = state.started.elapsed();
    let header = Paragraph::new(vec![
        Line::from(vec![Span::styled(
            format!(" {} ", state.operation),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]),
        Line::from(format!(
            " {} / {}  |  elapsed: {}m {}s",
            state.account,
            state.profile,
            elapsed.as_secs() / 60,
            elapsed.as_secs() % 60,
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" crunchyroll-migrate "),
    );
    f.render_widget(header, chunks[0]);

    // Progress gauges
    let gauge_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
        ])
        .split(chunks[1]);

    let labels = ["Watchlist", "Crunchylists", "Ratings", "History"];
    for (i, label) in labels.iter().enumerate() {
        let (ratio, info) = match &state.progress[i] {
            Some(p) if p.total > 0 => {
                let ratio = p.processed as f64 / p.total as f64;
                let eta_str = format_eta(state.eta(i));
                let info = format!(
                    "{}/{} ({} added, {} skip, {} fail){}",
                    p.processed,
                    p.total,
                    p.added,
                    p.skipped + p.already_present,
                    p.failed,
                    eta_str
                );
                (ratio, info)
            }
            Some(p) if p.processed > 0 => {
                // Streaming (unknown total) - show count so far
                (0.0, format!("{} so far", p.processed))
            }
            Some(_) => (0.0, "0 items".to_string()),
            None => (0.0, "waiting...".to_string()),
        };

        let gauge = Gauge::default()
            .block(Block::default().title(format!(" {} ", label)))
            .gauge_style(Style::default().fg(Color::Green))
            .ratio(ratio.clamp(0.0, 1.0))
            .label(info);
        f.render_widget(gauge, gauge_area[i]);
    }

    // Log
    let visible_lines = chunks[2].height.saturating_sub(2) as usize;
    let max_scroll = state.log.len().saturating_sub(visible_lines);
    let clamped_offset = scroll_offset.min(max_scroll);
    let start = max_scroll.saturating_sub(clamped_offset);
    let log_lines: Vec<Line> = state.log[start..]
        .iter()
        .take(visible_lines)
        .map(|entry| {
            let color = match entry.icon {
                '\u{2713}' => Color::Green,
                'x' => Color::Red,
                _ => Color::DarkGray,
            };
            Line::from(vec![
                Span::styled(format!(" {} ", entry.icon), Style::default().fg(color)),
                Span::raw(&entry.message),
            ])
        })
        .collect();

    let log_title = if clamped_offset > 0 {
        let last = (start + visible_lines).min(state.log.len());
        format!(" Log [{}-{}/{}] ", start + 1, last, state.log.len())
    } else {
        " Log ".to_string()
    };
    let log_widget =
        Paragraph::new(log_lines).block(Block::default().borders(Borders::ALL).title(log_title));
    f.render_widget(log_widget, chunks[2]);

    // Stats bar
    let mut total_added = 0;
    let mut total_already = 0;
    let mut total_failed = 0;
    for p in state.progress.iter().flatten() {
        total_added += p.added;
        total_already += p.already_present + p.skipped;
        total_failed += p.failed;
    }

    let stats = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" {} added ", total_added),
            Style::default().fg(Color::Green),
        ),
        Span::raw("| "),
        Span::styled(
            format!("{} skipped ", total_already),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("| "),
        Span::styled(
            format!("{} failed ", total_failed),
            Style::default().fg(Color::Red),
        ),
        Span::raw(if state.done { "| DONE" } else { "" }),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(stats, chunks[3]);

    clamped_offset
}

fn print_final_summary(state: &DashboardState) {
    let elapsed = state.started.elapsed();
    println!(
        "\n{} complete in {}m {}s\n",
        state.operation,
        elapsed.as_secs() / 60,
        elapsed.as_secs() % 60
    );

    let labels = ["Watchlist", "Crunchylists", "Ratings", "History"];
    let mut total_added = 0;
    let mut total_already = 0;
    let mut total_failed = 0;

    for (i, label) in labels.iter().enumerate() {
        if let Some(p) = &state.progress[i] {
            println!(
                "  {:14} {} added, {} already there, {} failed",
                label,
                p.added,
                p.already_present + p.skipped,
                p.failed
            );
            total_added += p.added;
            total_already += p.already_present + p.skipped;
            total_failed += p.failed;
        }
    }

    println!("  {}", "─".repeat(50));
    println!(
        "  {:14} {} added, {} already there, {} failed\n",
        "Total", total_added, total_already, total_failed
    );
}

fn format_eta(eta: Option<Duration>) -> String {
    match eta {
        Some(d) if d.as_secs() >= 60 => {
            format!(" | ~{}m{}s left", d.as_secs() / 60, d.as_secs() % 60)
        }
        Some(d) => format!(" | ~{}s left", d.as_secs()),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_eta_none() {
        assert_eq!(format_eta(None), "");
    }

    #[test]
    fn format_eta_seconds() {
        assert_eq!(format_eta(Some(Duration::from_secs(45))), " | ~45s left");
    }

    #[test]
    fn format_eta_minutes() {
        assert_eq!(format_eta(Some(Duration::from_secs(125))), " | ~2m5s left");
    }

    #[test]
    fn data_type_display() {
        assert_eq!(DataType::Watchlist.to_string(), "Watchlist");
        assert_eq!(DataType::History.to_string(), "History");
        assert_eq!(DataType::Crunchylists.to_string(), "Crunchylists");
        assert_eq!(DataType::Ratings.to_string(), "Ratings");
    }

    #[test]
    fn progress_update_fields() {
        let update = ProgressUpdate {
            data_type: DataType::Watchlist,
            total: 10,
            processed: 7,
            added: 5,
            skipped: 0,
            already_present: 1,
            failed: 1,
        };
        assert_eq!(update.total, 10);
        assert_eq!(update.processed, 7);
    }
}
