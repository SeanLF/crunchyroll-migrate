//! One coherent terminal view per command.
//!
//! A [`Session`] owns an inline ratatui viewport (drawn in place, left in scrollback when done):
//! header, step tracker, a per-data-type table whose columns fill in as stages run
//! (read -> compare -> copy), a per-show list, prompts, and key hints. Flows mutate shared
//! state; a UI thread redraws it and answers prompts. Without a TTY the same API falls back
//! to plain lines and dialoguer prompts.

use anyhow::{Result, bail};
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    layout::{Constraint, Layout, Position, Rect},
    prelude::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

const VIEW_HEIGHT: u16 = 24;
const COMPACT_HEIGHT: u16 = 12;
const NAME_WIDTH: usize = 30;
const OTHER_SHOW: &str = "Other (no title)";

// ---------------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Watchlist,
    History,
    Crunchylists,
    Ratings,
}

impl DataType {
    pub const ALL: [DataType; 4] = [
        DataType::Watchlist,
        DataType::History,
        DataType::Crunchylists,
        DataType::Ratings,
    ];

    fn idx(self) -> usize {
        self as usize
    }

    fn label(self) -> &'static str {
        match self {
            DataType::Watchlist => "Watchlist",
            DataType::History => "History",
            DataType::Crunchylists => "Crunchylists",
            DataType::Ratings => "Ratings",
        }
    }
}

impl std::fmt::Display for DataType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    SignIn,
    Source,
    Target,
    Read,
    Compare,
    Copy,
}

impl Stage {
    fn label(self) -> &'static str {
        match self {
            Stage::SignIn => "Sign in",
            Stage::Source => "Source",
            Stage::Target => "Target",
            Stage::Read => "Read",
            Stage::Compare => "Compare",
            Stage::Copy => "Copy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Copied,
    /// The write reported the item was already there (e.g. 409).
    Already,
    Failed,
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    source: usize,
    already: usize,
    to_copy: usize,
    copied: usize,
    late_already: usize,
    failed: usize,
}

impl Tally {
    fn done(&self) -> usize {
        self.copied + self.late_already + self.failed
    }
}

#[derive(Debug, Clone)]
struct Show {
    name: String,
    order: usize,
    touched: u64,
    t: [Tally; 4],
}

impl Show {
    fn to_copy(&self) -> usize {
        self.t.iter().map(|t| t.to_copy).sum()
    }
    fn done(&self) -> usize {
        self.t.iter().map(Tally::done).sum()
    }
    fn failed(&self) -> usize {
        self.t.iter().map(|t| t.failed).sum()
    }
}

/// Group key for an item: the show it belongs to.
pub fn show_key(series_title: &str, title: &str) -> String {
    if !series_title.is_empty() {
        series_title.to_string()
    } else if !title.is_empty() {
        title.to_string()
    } else {
        OTHER_SHOW.to_string()
    }
}

enum PromptKind {
    Secret,
    Text,
    Select(Vec<String>),
    Confirm,
}

enum Reply {
    Text(String),
    Index(usize),
    Yes(bool),
    Cancelled,
}

struct Prompt {
    label: String,
    hint: Option<String>,
    kind: PromptKind,
    input: String,
    selected: usize,
    reply: Option<oneshot::Sender<Reply>>,
}

struct State {
    operation: String,
    compact: bool,
    steps: Vec<Stage>,
    stage: Option<Stage>,
    route: String,
    status: String,
    started: Instant,
    copy_started: Option<Instant>,
    finished_in: Option<Duration>,
    kinds: [Tally; 4],
    read: bool,
    compared: bool,
    shows: Vec<Show>,
    index: HashMap<String, usize>,
    tick: u64,
    notes: Vec<Line<'static>>,
    prompt: Option<Prompt>,
    done: bool,
    /// Ended by an error rather than `finish()`.
    stopped: bool,
    scroll: usize,
}

impl State {
    fn new(operation: &str, steps: &[Stage], compact: bool) -> Self {
        Self {
            operation: operation.to_string(),
            compact,
            steps: steps.to_vec(),
            stage: None,
            route: String::new(),
            status: String::new(),
            started: Instant::now(),
            copy_started: None,
            finished_in: None,
            kinds: [Tally::default(); 4],
            read: false,
            compared: false,
            shows: Vec::new(),
            index: HashMap::new(),
            tick: 0,
            notes: Vec::new(),
            prompt: None,
            done: false,
            stopped: false,
            scroll: 0,
        }
    }

    fn show_mut(&mut self, name: &str) -> &mut Show {
        self.tick += 1;
        let tick = self.tick;
        let idx = match self.index.get(name) {
            Some(&i) => i,
            None => {
                let i = self.shows.len();
                self.shows.push(Show {
                    name: name.to_string(),
                    order: i,
                    touched: 0,
                    t: [Tally::default(); 4],
                });
                self.index.insert(name.to_string(), i);
                i
            }
        };
        let show = &mut self.shows[idx];
        show.touched = tick;
        show
    }

    fn copying(&self) -> bool {
        self.copy_started.is_some()
    }

    fn has_step(&self, stage: Stage) -> bool {
        self.steps.contains(&stage)
    }

    fn total_to_copy(&self) -> usize {
        self.kinds.iter().map(|t| t.to_copy).sum()
    }

    fn total_done(&self) -> usize {
        self.kinds.iter().map(Tally::done).sum()
    }

    fn eta(&self) -> Option<Duration> {
        let start = self.copy_started?;
        let (done, total) = (self.total_done(), self.total_to_copy());
        if done == 0 || done >= total {
            return None;
        }
        let rate = done as f64 / start.elapsed().as_secs_f64();
        Some(Duration::from_secs_f64((total - done) as f64 / rate))
    }
}

// ---------------------------------------------------------------------------------------------
// Session API
// ---------------------------------------------------------------------------------------------

pub struct Session {
    state: Arc<Mutex<State>>,
    tty: bool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Session {
    /// Full view: header, steps, table, show list.
    pub fn start(operation: &str, steps: &[Stage]) -> Self {
        Self::launch(operation, steps, false)
    }

    /// Prompts only (status, rename): the view is cleared when finished.
    pub fn prompts_only(operation: &str) -> Self {
        Self::launch(operation, &[Stage::SignIn], true)
    }

    fn launch(operation: &str, steps: &[Stage], compact: bool) -> Self {
        let tty = io::stdout().is_terminal() && io::stdin().is_terminal();
        let state = Arc::new(Mutex::new(State::new(operation, steps, compact)));
        let thread = if tty {
            let s = state.clone();
            Some(std::thread::spawn(move || {
                if let Err(e) = run_view(&s) {
                    restore_terminal();
                    eprintln!("UI error: {e}");
                }
            }))
        } else {
            None
        };
        Self {
            state,
            tty,
            thread: Mutex::new(thread),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn stage(&self, stage: Stage) {
        let mut s = self.lock();
        s.stage = Some(stage);
        s.status.clear();
        if stage == Stage::Copy && s.copy_started.is_none() {
            s.copy_started = Some(Instant::now());
        }
        if stage == Stage::Read {
            s.read = true;
        }
        if !self.tty {
            println!("== {} ==", stage.label());
        }
    }

    /// Short activity text shown next to the elapsed time ("Signing in…").
    pub fn status(&self, text: &str) {
        self.lock().status = text.to_string();
        if !self.tty && !text.is_empty() {
            println!("{text}");
        }
    }

    /// "Sean" or "Sean -> Sean copy".
    pub fn route(&self, route: &str) {
        self.lock().route = route.to_string();
    }

    /// A permanent line above the view (kept in scrollback).
    pub fn note(&self, text: &str) {
        self.push_note(Line::from(vec![
            Span::styled("  ✓ ", Style::new().green()),
            Span::raw(text.to_string()),
        ]));
    }

    pub fn warn(&self, text: &str) {
        self.push_note(Line::from(vec![
            Span::styled("  ✗ ", Style::new().red()),
            Span::raw(text.to_string()),
        ]));
    }

    fn push_note(&self, line: Line<'static>) {
        if self.tty {
            self.lock().notes.push(line);
        } else {
            println!("{line}");
        }
    }

    /// An item exists on the source.
    pub fn source(&self, kind: DataType, show: &str) {
        let mut s = self.lock();
        s.read = true;
        s.kinds[kind.idx()].source += 1;
        s.show_mut(show).t[kind.idx()].source += 1;
    }

    /// Compare result for one source item.
    pub fn planned(&self, kind: DataType, show: &str, already: bool) {
        let mut s = self.lock();
        s.compared = true;
        let k = kind.idx();
        let show = s.show_mut(show);
        if already {
            show.t[k].already += 1;
        } else {
            show.t[k].to_copy += 1;
        }
        if already {
            s.kinds[k].already += 1;
        } else {
            s.kinds[k].to_copy += 1;
        }
    }

    /// Mark compare as finished even when nothing was planned (empty source).
    pub fn compared(&self) {
        self.lock().compared = true;
    }

    pub fn copied(&self, kind: DataType, show: &str, outcome: Outcome) {
        let mut s = self.lock();
        let k = kind.idx();
        let bump = |t: &mut Tally| match outcome {
            Outcome::Copied => t.copied += 1,
            Outcome::Already => t.late_already += 1,
            Outcome::Failed => t.failed += 1,
        };
        bump(&mut s.show_mut(show).t[k]);
        bump(&mut s.kinds[k]);
    }

    pub fn total_to_copy(&self) -> usize {
        self.lock().total_to_copy()
    }

    // --- prompts -----------------------------------------------------------------------------

    pub async fn secret(&self, label: &str, hint: Option<&str>) -> Result<String> {
        if !self.tty {
            if let Some(h) = hint {
                println!("{h}");
            }
            return Ok(rpassword::prompt_password(format!("{label}: "))?);
        }
        match self.ask(label, hint, PromptKind::Secret).await {
            Reply::Text(t) => Ok(t),
            _ => bail!("Cancelled"),
        }
    }

    pub async fn text(&self, label: &str) -> Result<String> {
        if !self.tty {
            return Ok(dialoguer::Input::new().with_prompt(label).interact_text()?);
        }
        match self.ask(label, None, PromptKind::Text).await {
            Reply::Text(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
            Reply::Text(_) => bail!("{label}: nothing entered"),
            _ => bail!("Cancelled"),
        }
    }

    pub async fn select(&self, label: &str, items: Vec<String>) -> Result<usize> {
        if !self.tty {
            return Ok(dialoguer::Select::new()
                .with_prompt(label)
                .items(&items)
                .default(0)
                .interact()?);
        }
        match self.ask(label, None, PromptKind::Select(items)).await {
            Reply::Index(i) => Ok(i),
            _ => bail!("Cancelled"),
        }
    }

    pub async fn confirm(&self, label: &str) -> Result<bool> {
        if !self.tty {
            return Ok(dialoguer::Confirm::new()
                .with_prompt(label)
                .default(true)
                .interact()?);
        }
        Ok(matches!(
            self.ask(label, None, PromptKind::Confirm).await,
            Reply::Yes(true)
        ))
    }

    async fn ask(&self, label: &str, hint: Option<&str>, kind: PromptKind) -> Reply {
        let (tx, rx) = oneshot::channel();
        self.lock().prompt = Some(Prompt {
            label: label.to_string(),
            hint: hint.map(str::to_string),
            kind,
            input: String::new(),
            selected: 0,
            reply: Some(tx),
        });
        rx.await.unwrap_or(Reply::Cancelled)
    }

    // --- end ---------------------------------------------------------------------------------

    /// Final redraw, restore the terminal, and leave the view in scrollback
    /// (or clear it, for prompt-only sessions). Plain mode prints the summary table.
    pub fn finish(&self) {
        {
            let mut s = self.lock();
            if s.done {
                return;
            }
            s.done = true;
            s.finished_in = Some(s.started.elapsed());
            s.prompt = None;
        }
        if let Some(t) = self.thread.lock().ok().and_then(|mut t| t.take()) {
            let _ = t.join();
        } else if !self.tty {
            let s = self.lock();
            if !s.compact && !s.stopped && s.read {
                println!();
                for line in table_lines(&s) {
                    println!("{line}");
                }
                println!();
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Errors bubble out of flows via `?`: mark the view as stopped (not done) and make
        // sure the terminal is usable before main prints the error.
        {
            let mut s = self.lock();
            if !s.done {
                s.stopped = true;
            }
        }
        self.finish();
    }
}

// ---------------------------------------------------------------------------------------------
// View thread
// ---------------------------------------------------------------------------------------------

pub fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), DisableBracketedPaste, crossterm::cursor::Show);
}

fn run_view(state: &Arc<Mutex<State>>) -> io::Result<()> {
    let lock = || {
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    let compact = lock().compact;
    let (_, rows) = crossterm::terminal::size()?;
    let height = if compact { COMPACT_HEIGHT } else { VIEW_HEIGHT }.min(rows.max(6));

    enable_raw_mode()?;
    execute!(io::stdout(), EnableBracketedPaste)?;
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )?;

    // Rows of the viewport used by the final frame, so the finished view can be trimmed
    let used_rows = loop {
        let (done, used) = {
            let mut s = lock();
            for line in std::mem::take(&mut s.notes) {
                terminal.insert_before(1, |buf| Paragraph::new(line).render(buf.area, buf))?;
            }
            let frame = terminal.draw(|f| draw(f, &mut s))?;
            let used = last_used_row(frame.buffer).map_or(0, |y| y + 1);
            (s.done, used)
        };
        if done {
            break used;
        }

        if event::poll(Duration::from_millis(80))? {
            let ev = event::read()?;
            let mut s = lock();
            match ev {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if handle_key(&mut s, key) {
                        drop(s);
                        terminal.clear()?;
                        restore_terminal();
                        eprintln!("Interrupted.");
                        std::process::exit(130);
                    }
                }
                Event::Paste(text) => {
                    if let Some(p) = s.prompt.as_mut()
                        && matches!(p.kind, PromptKind::Secret | PromptKind::Text)
                    {
                        p.input.push_str(text.trim());
                    }
                }
                Event::Resize(..) => terminal.autoresize()?,
                _ => {}
            }
        }
    };

    let area = terminal.get_frame().area();
    if compact {
        terminal.clear()?;
        terminal.set_cursor_position(Position::new(0, area.y))?;
    } else {
        // Leave the finished view in scrollback without the unused rows below it
        terminal.set_cursor_position(Position::new(0, area.y + used_rows))?;
        execute!(
            io::stdout(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::FromCursorDown)
        )?;
    }
    restore_terminal();
    if !compact {
        println!();
    }
    Ok(())
}

fn last_used_row(buf: &ratatui::buffer::Buffer) -> Option<u16> {
    (0..buf.area.height).rev().find(|&y| {
        (0..buf.area.width).any(|x| buf[(buf.area.x + x, buf.area.y + y)].symbol() != " ")
    })
}

/// Returns true when the user asked to quit.
fn handle_key(s: &mut State, key: KeyEvent) -> bool {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }
    let Some(p) = s.prompt.as_mut() else {
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Up | KeyCode::Char('k') => s.scroll = s.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => s.scroll += 1,
            KeyCode::PageUp => s.scroll = s.scroll.saturating_sub(10),
            KeyCode::PageDown => s.scroll += 10,
            KeyCode::Home => s.scroll = 0,
            KeyCode::End => s.scroll = usize::MAX,
            _ => {}
        }
        return false;
    };

    let reply = match (&p.kind, key.code) {
        (_, KeyCode::Esc) => Some(match p.kind {
            PromptKind::Confirm => Reply::Yes(false),
            _ => Reply::Cancelled,
        }),
        (PromptKind::Secret | PromptKind::Text, KeyCode::Enter) => {
            Some(Reply::Text(std::mem::take(&mut p.input)))
        }
        (PromptKind::Secret | PromptKind::Text, KeyCode::Backspace) => {
            p.input.pop();
            None
        }
        (PromptKind::Secret | PromptKind::Text, KeyCode::Char(c)) => {
            p.input.push(c);
            None
        }
        (PromptKind::Select(items), KeyCode::Up | KeyCode::Char('k')) => {
            p.selected = p.selected.checked_sub(1).unwrap_or(items.len() - 1);
            None
        }
        (PromptKind::Select(items), KeyCode::Down | KeyCode::Char('j')) => {
            p.selected = (p.selected + 1) % items.len();
            None
        }
        (PromptKind::Select(_), KeyCode::Enter) => Some(Reply::Index(p.selected)),
        (PromptKind::Confirm, KeyCode::Enter | KeyCode::Char('y')) => Some(Reply::Yes(true)),
        (PromptKind::Confirm, KeyCode::Char('n')) => Some(Reply::Yes(false)),
        _ => None,
    };

    if let Some(reply) = reply
        && let Some(p) = s.prompt.take()
        && let Some(tx) = p.reply
    {
        let _ = tx.send(reply);
    }
    false
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn mmss(d: Duration) -> String {
    format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
}

fn draw(f: &mut Frame, s: &mut State) {
    let area = f.area();
    let [header, steps, _, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(f, s, header);
    f.render_widget(Paragraph::new(steps_line(s)), steps);

    if s.prompt.is_some() {
        draw_prompt(f, s, body);
    } else if !s.compact {
        let table = table_lines(s);
        let [table_area, _, shows_area] = Layout::vertical([
            Constraint::Length(table.len() as u16),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(body);
        f.render_widget(Paragraph::new(table), table_area);
        draw_shows(f, s, shows_area);
    }

    f.render_widget(Paragraph::new(footer_line(s)).style(dim()), footer);
}

fn draw_header(f: &mut Frame, s: &State, area: Rect) {
    let mut left = vec![Span::styled(
        format!(" {} ", s.operation),
        Style::new().cyan().add_modifier(Modifier::BOLD),
    )];
    if !s.route.is_empty() {
        left.push(Span::raw(s.route.clone()));
    }

    let elapsed = s.finished_in.unwrap_or_else(|| s.started.elapsed());
    let right = if s.stopped {
        Line::from(vec![
            Span::styled("✗ Stopped ", Style::new().red().bold()),
            Span::styled(mmss(elapsed), dim()),
            Span::raw(" "),
        ])
    } else if s.done {
        Line::from(vec![
            Span::styled("✓ Done ", Style::new().green().bold()),
            Span::styled(mmss(elapsed), dim()),
            Span::raw(" "),
        ])
    } else {
        let mut parts = vec![];
        if !s.status.is_empty() {
            parts.push(Span::raw(format!("{}  ", s.status)));
        }
        if let Some(eta) = s.eta() {
            parts.push(Span::raw(format!("~{} left  ", mmss(eta))));
        }
        parts.push(Span::styled(format!("{} ", mmss(elapsed)), dim()));
        Line::from(parts)
    };

    let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(40)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(left)), l);
    f.render_widget(Paragraph::new(right).right_aligned(), r);
}

fn steps_line(s: &State) -> Line<'static> {
    let current = s.stage.and_then(|st| s.steps.iter().position(|&x| x == st));
    let mut spans = vec![Span::raw(" ")];
    for (i, step) in s.steps.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ›  ", dim()));
        }
        let (text, style) = match current {
            Some(c) if s.stopped && i == c => {
                (format!("✗ {}", step.label()), Style::new().red().bold())
            }
            _ if s.done && !s.stopped => (format!("✓ {}", step.label()), Style::new().green()),
            Some(c) if i < c => (format!("✓ {}", step.label()), Style::new().green()),
            Some(c) if i == c => (format!("● {}", step.label()), Style::new().cyan().bold()),
            _ => (step.label().to_string(), dim()),
        };
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// Data-type table. Columns appear for the stages this command runs.
fn table_lines(s: &State) -> Vec<Line<'static>> {
    let compare = s.has_step(Stage::Compare);
    let copy = s.has_step(Stage::Copy);

    let mut head = format!(" {:<14}{:>10}", "", "On source");
    if compare {
        head.push_str(&format!("{:>16}{:>10}", "Already there", "To copy"));
    }
    if copy {
        head.push_str(&format!("{:>10}", "Copied"));
    }
    let mut lines = vec![Line::styled(head, dim())];

    for kind in DataType::ALL {
        let t = s.kinds[kind.idx()];
        let cell = |known: bool, n: usize, w: usize| {
            if known {
                format!("{n:>w$}")
            } else {
                format!("{:>w$}", "·")
            }
        };
        let empty = s.read && t.source == 0;
        let mut spans = vec![Span::raw(format!(" {:<14}", kind.label()))];
        spans.push(Span::raw(cell(s.read, t.source, 10)));
        if compare {
            spans.push(Span::raw(cell(s.compared, t.already, 16)));
            spans.push(Span::raw(cell(s.compared, t.to_copy, 10)));
        }
        if copy {
            spans.push(Span::raw(cell(s.copying(), t.copied, 10)));
            if s.copying() && t.to_copy > 0 {
                let ratio = t.done() as f64 / t.to_copy as f64;
                spans.push(Span::raw("  "));
                spans.extend(bar(ratio, 16));
                if t.done() >= t.to_copy && t.failed == 0 {
                    spans.push(Span::styled(" ✓", Style::new().green()));
                }
            }
            if t.failed > 0 {
                spans.push(Span::styled(
                    format!("  {} failed", t.failed),
                    Style::new().red(),
                ));
            }
        }
        let mut line = Line::from(spans);
        if empty {
            line = line.style(dim());
        }
        lines.push(line);
    }
    lines
}

fn bar(ratio: f64, width: usize) -> Vec<Span<'static>> {
    let filled = ((ratio.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    vec![
        Span::styled("━".repeat(filled), Style::new().green()),
        Span::styled("─".repeat(width - filled), dim()),
    ]
}

/// Order: in progress, then queued, then finished, then nothing to do. Stable within groups.
fn show_order(s: &State) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..s.shows.len()).collect();
    if s.copying() {
        let rank = |sh: &Show| {
            let (to, done) = (sh.to_copy(), sh.done());
            if to == 0 {
                3
            } else if done >= to {
                2
            } else if done > 0 {
                0
            } else {
                1
            }
        };
        idx.sort_by_key(|&i| {
            let sh = &s.shows[i];
            let r = rank(sh);
            (r, if r == 0 { u64::MAX - sh.touched } else { 0 }, sh.order)
        });
    } else if s.compared {
        idx.sort_by_key(|&i| (s.shows[i].to_copy() == 0, s.shows[i].order));
    }
    idx
}

fn draw_shows(f: &mut Frame, s: &mut State, area: Rect) {
    if s.shows.is_empty() || area.height == 0 {
        return;
    }
    let order = show_order(s);
    let rows = area.height as usize;
    let needs_more = order.len() > rows;
    let visible = if needs_more { rows - 1 } else { rows };
    let max_scroll = order.len().saturating_sub(visible);
    s.scroll = s.scroll.min(max_scroll);

    let mut lines: Vec<Line> = order[s.scroll..]
        .iter()
        .take(visible)
        .map(|&i| show_line(s, &s.shows[i]))
        .collect();
    if needs_more {
        let below = order.len() - s.scroll - visible;
        let text = match (s.scroll, below) {
            (0, b) => format!("   … {b} more shows  (↑↓ to scroll)"),
            (a, 0) => format!("   … {a} above"),
            (a, b) => format!("   … {a} above, {b} below"),
        };
        lines.push(Line::styled(text, dim()));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn show_line(s: &State, sh: &Show) -> Line<'static> {
    let (to, done, failed) = (sh.to_copy(), sh.done(), sh.failed());
    let (marker, mstyle) = if failed > 0 {
        ("✗", Style::new().red())
    } else if s.copying() && to > 0 && done >= to {
        ("✓", Style::new().green())
    } else if s.copying() && done > 0 {
        ("●", Style::new().cyan())
    } else if s.compared && to == 0 {
        ("✓", dim())
    } else {
        ("○", dim())
    };

    let mut name: String = sh.name.chars().take(NAME_WIDTH).collect();
    if sh.name.chars().count() > NAME_WIDTH {
        name.pop();
        name.push('…');
    }
    let name_style = if s.compared && to == 0 {
        dim()
    } else {
        Style::new()
    };
    let mut spans = vec![
        Span::styled(format!(" {marker} "), mstyle),
        Span::styled(format!("{name:<NAME_WIDTH$}  "), name_style),
    ];

    let tag_style = |t: &Tally| {
        if t.failed > 0 {
            Style::new().red()
        } else if t.to_copy > 0 && t.done() >= t.to_copy {
            Style::new().green()
        } else if s.compared && t.to_copy == 0 {
            dim()
        } else {
            Style::new()
        }
    };

    let w = sh.t[DataType::Watchlist.idx()];
    spans.push(Span::styled(
        format!("{:<13}", if w.source > 0 { "★ watchlist" } else { "" }),
        tag_style(&w),
    ));

    let h = sh.t[DataType::History.idx()];
    let eps = |n: usize| if n == 1 { "ep" } else { "eps" };
    let history = if h.source == 0 {
        String::new()
    } else if s.compared && h.to_copy > 0 {
        format!("{}/{} {}", h.done(), h.to_copy, eps(h.to_copy))
    } else {
        format!("{} {}", h.source, eps(h.source))
    };
    spans.push(Span::styled(format!("{history:<13}"), tag_style(&h)));

    let r = sh.t[DataType::Ratings.idx()];
    spans.push(Span::styled(
        format!("{:<7}", if r.source > 0 { "rated" } else { "" }),
        tag_style(&r),
    ));

    let c = sh.t[DataType::Crunchylists.idx()];
    if c.source > 0 {
        let lists = if c.source == 1 { "list" } else { "lists" };
        spans.push(Span::styled(
            format!("in {} {lists}  ", c.source),
            tag_style(&c),
        ));
    }

    if s.copying() && h.to_copy > 1 && h.done() < h.to_copy {
        spans.extend(bar(h.done() as f64 / h.to_copy as f64, 12));
    }
    Line::from(spans)
}

fn draw_prompt(f: &mut Frame, s: &State, area: Rect) {
    let Some(p) = &s.prompt else { return };
    let mut lines = vec![Line::from(Span::styled(
        format!(" {}", p.label),
        Style::new().bold(),
    ))];
    if let Some(h) = &p.hint {
        lines.push(Line::styled(format!(" {h}"), dim()));
    }
    lines.push(Line::raw(""));

    let cursor = Span::styled("▏", Style::new().cyan());
    match &p.kind {
        PromptKind::Secret => {
            let n = p.input.chars().count();
            let shown = if n > 40 {
                format!("{}  ({n} chars)", "•".repeat(40))
            } else {
                "•".repeat(n)
            };
            lines.push(Line::from(vec![
                Span::styled(" › ", Style::new().cyan()),
                Span::raw(shown),
                cursor,
            ]));
        }
        PromptKind::Text => lines.push(Line::from(vec![
            Span::styled(" › ", Style::new().cyan()),
            Span::raw(p.input.clone()),
            cursor,
        ])),
        PromptKind::Select(items) => {
            let room = (area.height as usize).saturating_sub(lines.len()).max(1);
            let start = p.selected.saturating_sub(room - 1);
            for (i, item) in items.iter().enumerate().skip(start).take(room) {
                lines.push(if i == p.selected {
                    Line::styled(format!(" › {item}"), Style::new().cyan().bold())
                } else {
                    Line::raw(format!("   {item}"))
                });
            }
        }
        PromptKind::Confirm => {}
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn footer_line(s: &State) -> Line<'static> {
    let text = match s.prompt.as_ref().map(|p| &p.kind) {
        Some(PromptKind::Secret | PromptKind::Text) => " enter submit · esc cancel · paste works",
        Some(PromptKind::Select(_)) => " ↑↓ choose · enter select · esc cancel",
        Some(PromptKind::Confirm) => " enter yes · esc no",
        None if s.done => "",
        None if s.compact => " ctrl+c quit",
        None => " ↑↓ scroll shows · q quit",
    };
    Line::raw(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn render(s: &mut State, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, s)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn plan(s: &mut State, kind: DataType, show: &str, already: bool) {
        let k = kind.idx();
        s.kinds[k].source += 1;
        let sh = s.show_mut(show);
        sh.t[k].source += 1;
        if already {
            sh.t[k].already += 1;
            s.kinds[k].already += 1;
        } else {
            sh.t[k].to_copy += 1;
            s.kinds[k].to_copy += 1;
        }
    }

    fn migrate_state() -> State {
        let steps = [
            Stage::Source,
            Stage::Read,
            Stage::Target,
            Stage::Compare,
            Stage::Copy,
        ];
        let mut s = State::new("Migrate", &steps, false);
        s.route = "Sean → Sean copy".into();
        s.stage = Some(Stage::Copy);
        s.read = true;
        s.compared = true;
        s.copy_started = Some(Instant::now());
        s
    }

    #[test]
    fn show_key_falls_back() {
        assert_eq!(show_key("Frieren", "Ep 1"), "Frieren");
        assert_eq!(show_key("", "Your Name."), "Your Name.");
        assert_eq!(show_key("", ""), OTHER_SHOW);
    }

    #[test]
    fn copy_view_renders_table_and_shows() {
        let mut s = migrate_state();
        plan(&mut s, DataType::History, "Gachiakuta", true);
        plan(&mut s, DataType::Watchlist, "Frieren", false);
        for _ in 0..4 {
            plan(&mut s, DataType::History, "Frieren", false);
        }
        s.kinds[DataType::History.idx()].copied = 2;
        s.show_mut("Frieren").t[DataType::History.idx()].copied = 2;

        let out = render(&mut s, 110, 16);
        assert!(out.contains("● Copy"), "{out}");
        assert!(out.contains("Already there"), "{out}");
        assert!(out.contains("2/4 eps"), "{out}");
        assert!(out.contains("★ watchlist"), "{out}");
        // In-progress Frieren sorts above Gachiakuta, which had nothing to copy
        assert!(
            out.find("Frieren").unwrap() < out.find("Gachiakuta").unwrap(),
            "{out}"
        );
    }

    #[test]
    fn stopped_view_marks_failed_step() {
        let mut s = migrate_state();
        s.stage = Some(Stage::Compare);
        s.done = true;
        s.stopped = true;
        let out = render(&mut s, 110, 12);
        assert!(out.contains("✓ Read"), "{out}");
        assert!(out.contains("✗ Compare"), "{out}");
        assert!(out.contains("Stopped"), "{out}");
        assert!(!out.contains("Done"), "{out}");
    }

    #[test]
    fn export_view_hides_compare_columns() {
        let mut s = State::new("Export", &[Stage::SignIn, Stage::Read], false);
        s.stage = Some(Stage::Read);
        s.read = true;
        let out = render(&mut s, 100, 12);
        assert!(out.contains("On source"));
        assert!(!out.contains("To copy"));
    }

    #[test]
    fn secret_prompt_masks_input() {
        let mut s = State::new("Export", &[Stage::SignIn, Stage::Read], false);
        s.prompt = Some(Prompt {
            label: "etp_rt cookie".into(),
            hint: None,
            kind: PromptKind::Secret,
            input: "hunter2".into(),
            selected: 0,
            reply: None,
        });
        let out = render(&mut s, 80, 10);
        assert!(out.contains("•••••••"), "{out}");
        assert!(!out.contains("hunter2"));
    }

    #[test]
    fn select_prompt_wraps_and_replies() {
        let mut s = State::new("Export", &[Stage::SignIn], false);
        let (tx, mut rx) = oneshot::channel();
        s.prompt = Some(Prompt {
            label: "Profile".into(),
            hint: None,
            kind: PromptKind::Select(vec!["a".into(), "b".into()]),
            input: String::new(),
            selected: 0,
            reply: Some(tx),
        });
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        handle_key(&mut s, key(KeyCode::Up));
        handle_key(&mut s, key(KeyCode::Enter));
        assert!(matches!(rx.try_recv(), Ok(Reply::Index(1))));
        assert!(s.prompt.is_none());
    }
}
