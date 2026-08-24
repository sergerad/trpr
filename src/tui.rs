//! The ratatui interface: pick comments, write instructions, launch the
//! agent, watch it work.

use crate::agent::{self, RunCtx, RunEvent, Step, StepKind};
use crate::github::{CommentItem, ItemKind, PrSummary};
use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use std::path::PathBuf;
use std::time::Instant;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

pub struct AppCtx {
    pub token: String,
    pub claude_bin: String,
    pub max_turns: u32,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Focus {
    List,
    Detail,
}

#[derive(Clone, PartialEq)]
pub enum Decision {
    Pending,
    Ignored,
    Instructed(String),
}

pub struct UiItem {
    pub item: CommentItem,
    pub decision: Decision,
    /// A prior run already committed for this comment: (short sha, epoch).
    pub handled: Option<(String, i64)>,
    /// Epoch at which this comment was previously ignored, if ever.
    pub prior_ignored: Option<i64>,
}

pub enum Phase {
    Select,
    /// Editing the instruction for the selected item (modal vim editor).
    Edit {
        editor: Box<VimEditor>,
    },
    Running,
    Done {
        ok: bool,
        summary: String,
    },
}

// ------------------------------------------------------------ vim editor ---

/// A modal vim layer over `tui_textarea::TextArea` (which supplies the
/// buffer, undo/redo history, and yank register). Esc goes to normal (never
/// closes); `:q`/`:q!` cancel, `:wq`/`:x`/`:w` save. Enter is a plain
/// newline in insert mode.
pub struct VimEditor {
    ta: tui_textarea::TextArea<'static>,
    mode: VimMode,
    /// Pending multi-key: 'g' (gg), 'd' (dd), 'y' (yy).
    pending: Option<char>,
    error: Option<String>,
}

enum VimMode {
    Normal,
    Insert,
    Command(String),
}

enum EditorAction {
    Continue,
    Save(String),
    Cancel,
}

/// Hand conversion instead of tui-textarea's `From<KeyEvent>` impl, so a
/// crossterm version skew between the two crates can never break input.
fn to_input(key: KeyEvent) -> tui_textarea::Input {
    use tui_textarea::Key;
    let k = match key.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Enter => Key::Enter,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Tab => Key::Tab,
        KeyCode::Delete => Key::Delete,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Esc => Key::Esc,
        _ => Key::Null,
    };
    tui_textarea::Input {
        key: k,
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT),
    }
}

impl VimEditor {
    fn new(text: &str) -> Self {
        let mut ta = if text.is_empty() {
            tui_textarea::TextArea::default()
        } else {
            tui_textarea::TextArea::from(text.lines())
        };
        ta.set_cursor_line_style(Style::default());
        // Empty instruction: drop straight into insert so the common case is
        // "e, type, Esc, :wq".
        let mode = if text.is_empty() {
            VimMode::Insert
        } else {
            VimMode::Normal
        };
        Self {
            ta,
            mode,
            pending: None,
            error: None,
        }
    }

    fn text(&self) -> String {
        self.ta.lines().join("\n")
    }

    fn handle(&mut self, key: KeyEvent) -> EditorAction {
        use tui_textarea::CursorMove as M;
        self.error = None;

        // Command mode first: its buffer lives in the mode enum.
        if matches!(self.mode, VimMode::Command(_)) {
            let VimMode::Command(mut cmd) = std::mem::replace(&mut self.mode, VimMode::Normal)
            else {
                unreachable!()
            };
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => match cmd.as_str() {
                    "q" | "q!" => return EditorAction::Cancel,
                    "w" | "wq" | "x" => return EditorAction::Save(self.text()),
                    other => self.error = Some(format!("not an editor command: {other}")),
                },
                KeyCode::Backspace => {
                    // Backspacing past the ':' closes the command line (vim).
                    if !cmd.is_empty() {
                        cmd.pop();
                        self.mode = VimMode::Command(cmd);
                    }
                }
                KeyCode::Char(c) => {
                    cmd.push(c);
                    self.mode = VimMode::Command(cmd);
                }
                _ => self.mode = VimMode::Command(cmd),
            }
            return EditorAction::Continue;
        }

        if matches!(self.mode, VimMode::Insert) {
            match key.code {
                KeyCode::Esc => {
                    self.mode = VimMode::Normal;
                    self.ta.move_cursor(M::Back);
                }
                _ => {
                    self.ta.input(to_input(key));
                }
            }
            return EditorAction::Continue;
        }

        // Normal mode.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let pending = self.pending.take();
        match (pending, key.code) {
            (Some('g'), KeyCode::Char('g')) => {
                self.ta.move_cursor(M::Top);
                self.ta.move_cursor(M::Head);
            }
            (Some('d'), KeyCode::Char('d')) => self.line_op(true),
            (Some('y'), KeyCode::Char('y')) => self.line_op(false),
            (_, KeyCode::Char('g')) => self.pending = Some('g'),
            (_, KeyCode::Char('d')) => self.pending = Some('d'),
            (_, KeyCode::Char('y')) => self.pending = Some('y'),
            (_, KeyCode::Char(':')) => self.mode = VimMode::Command(String::new()),
            (_, KeyCode::Char('i')) => self.mode = VimMode::Insert,
            (_, KeyCode::Char('a')) => {
                self.ta.move_cursor(M::Forward);
                self.mode = VimMode::Insert;
            }
            (_, KeyCode::Char('I')) => {
                self.ta.move_cursor(M::Head);
                self.mode = VimMode::Insert;
            }
            (_, KeyCode::Char('A')) => {
                self.ta.move_cursor(M::End);
                self.mode = VimMode::Insert;
            }
            (_, KeyCode::Char('o')) => {
                self.ta.move_cursor(M::End);
                self.ta.insert_newline();
                self.mode = VimMode::Insert;
            }
            (_, KeyCode::Char('O')) => {
                self.ta.move_cursor(M::Head);
                self.ta.insert_newline();
                self.ta.move_cursor(M::Up);
                self.mode = VimMode::Insert;
            }
            (_, KeyCode::Char('x')) => {
                self.ta.delete_next_char();
            }
            // D: cut to end of line (fills the yank register).
            (_, KeyCode::Char('D')) => {
                self.ta.delete_line_by_end();
            }
            // C: change to end of line — cut to EOL, then insert.
            (_, KeyCode::Char('C')) => {
                self.ta.delete_line_by_end();
                self.mode = VimMode::Insert;
            }
            // p: paste the yank register (filled by dd/yy/D/C/x).
            (_, KeyCode::Char('p')) => {
                self.ta.paste();
            }
            (_, KeyCode::Char('u')) => {
                self.ta.undo();
            }
            (_, KeyCode::Char('r')) if ctrl => {
                self.ta.redo();
            }
            (_, KeyCode::Char('h') | KeyCode::Left) => self.ta.move_cursor(M::Back),
            (_, KeyCode::Char('l') | KeyCode::Right) => self.ta.move_cursor(M::Forward),
            (_, KeyCode::Char('j') | KeyCode::Down) => self.ta.move_cursor(M::Down),
            (_, KeyCode::Char('k') | KeyCode::Up) => self.ta.move_cursor(M::Up),
            (_, KeyCode::Char('0')) => self.ta.move_cursor(M::Head),
            (_, KeyCode::Char('$')) => self.ta.move_cursor(M::End),
            (_, KeyCode::Char('G')) => {
                self.ta.move_cursor(M::Bottom);
                self.ta.move_cursor(M::Head);
            }
            (_, KeyCode::Char('w')) => self.ta.move_cursor(M::WordForward),
            (_, KeyCode::Char('b')) => self.ta.move_cursor(M::WordBack),
            _ => {}
        }
        EditorAction::Continue
    }

    /// dd (cut=true) / yy (cut=false): select the whole line including the
    /// trailing newline when there is one, so `p` behaves ~linewise.
    fn line_op(&mut self, cut: bool) {
        use tui_textarea::CursorMove as M;
        let (row, col) = self.ta.cursor();
        self.ta.move_cursor(M::Head);
        self.ta.start_selection();
        if row + 1 < self.ta.lines().len() {
            self.ta.move_cursor(M::Down);
            self.ta.move_cursor(M::Head);
        } else {
            self.ta.move_cursor(M::End);
        }
        if cut {
            self.ta.cut();
        } else {
            self.ta.copy();
            self.ta.move_cursor(M::Jump(row as u16, col as u16));
        }
    }
}

pub struct App {
    pub repo: String,
    pub repo_root: PathBuf,
    pub branch: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub items: Vec<UiItem>,
    /// Persisted ignore decisions: comment url → epoch when ignored.
    pub ignored_at: std::collections::HashMap<String, i64>,
    pub selected: usize,
    /// Which pane j/k act on in the Select phase.
    pub focus: Focus,
    /// Scroll offset of the detail pane (Select phase), in wrapped lines.
    pub detail_scroll: u16,
    /// A lone 'g' was just pressed (vim-style gg pending).
    pub pending_g: bool,
    pub phase: Phase,
    pub log: Vec<Step>,
    /// Scroll offset from the bottom of the log (0 = follow).
    pub log_offset: usize,
    pub notice: Option<String>,
    pub run_started: Option<Instant>,
    pub last_event: Option<Instant>,
    pub run_dir: Option<PathBuf>,
    /// Launched from the PR-list screen: Esc navigates back instead of quitting.
    pub from_list: bool,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: String,
        repo_root: PathBuf,
        branch: String,
        pr_number: u64,
        pr_title: String,
        items: Vec<CommentItem>,
        handled: std::collections::HashMap<String, (String, i64)>,
        ignored_at: std::collections::HashMap<String, i64>,
    ) -> Self {
        let items = items
            .into_iter()
            .map(|item| {
                let handled_info = handled.get(&item.url).cloned();
                let prior_ignored = ignored_at.get(&item.url).copied();
                // A persisted ignore holds unless someone commented since —
                // new activity resurfaces the item as Pending with a badge.
                let decision = match prior_ignored {
                    Some(ts) if item.last_activity <= ts => Decision::Ignored,
                    _ => Decision::Pending,
                };
                UiItem {
                    item,
                    decision,
                    handled: handled_info,
                    prior_ignored,
                }
            })
            .collect();
        Self {
            repo,
            repo_root,
            branch,
            pr_number,
            pr_title,
            items,
            ignored_at,
            selected: 0,
            focus: Focus::List,
            detail_scroll: 0,
            pending_g: false,
            phase: Phase::Select,
            log: Vec::new(),
            log_offset: 0,
            notice: None,
            run_started: None,
            last_event: None,
            run_dir: None,
            from_list: false,
        }
    }

    fn instructed_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.decision, Decision::Instructed(_)))
            .count()
    }
}

enum Flow {
    Continue,
    Quit,
    Back,
    StartRun,
    Abort,
}

/// How the comment view ended.
pub enum CommentExit {
    Quit,
    BackToList,
}

/// What the PR-list screen resolved to.
pub enum PickOutcome {
    Quit,
    Refresh,
    Pr(PrSummary),
    /// Ctrl-i / Tab: resume the most recently left comment view, state intact.
    Forward,
    /// 'm': toggle between "my PRs" and "all PRs".
    ToggleMine,
}

/// One terminal + one key-reader thread for the whole program, hosting both
/// screens (PR list, comment view) so they can hand off to each other.
pub struct Session {
    terminal: ratatui::DefaultTerminal,
    key_rx: UnboundedReceiver<Event>,
}

impl Session {
    pub fn new() -> Self {
        let terminal = ratatui::init();
        let (key_tx, key_rx) = unbounded_channel::<Event>();
        std::thread::spawn(move || {
            while let Ok(ev) = crossterm::event::read() {
                if key_tx.send(ev).is_err() {
                    break;
                }
            }
        });
        Self { terminal, key_rx }
    }

    pub fn close(self) {
        ratatui::restore();
    }

    /// Draw a one-off centered message (e.g. "fetching…") while the caller
    /// awaits something; no input handling.
    pub fn show_message(&mut self, msg: &str) -> Result<()> {
        self.terminal.draw(|f| {
            let popup = centered(f.area(), 60, 3);
            f.render_widget(Clear, popup);
            f.render_widget(
                Paragraph::new(msg.to_string()).block(Block::bordered()),
                popup,
            );
        })?;
        Ok(())
    }

    /// The PR-list screen. `seen`: pr number → epoch of when you last opened
    /// that PR's comments, for the NEW indicator.
    #[allow(clippy::too_many_arguments)]
    pub async fn pick_pr(
        &mut self,
        repo: &str,
        current_branch: &str,
        summaries: &[PrSummary],
        seen: &std::collections::HashMap<u64, i64>,
        notice: Option<&str>,
        can_forward: bool,
        mine_only: bool,
    ) -> Result<PickOutcome> {
        let mut selected = 0usize;
        let mut pending_g = false;
        loop {
            self.terminal.draw(|f| {
                draw_pr_list(
                    f,
                    repo,
                    current_branch,
                    summaries,
                    seen,
                    selected,
                    notice,
                    can_forward,
                    mine_only,
                )
            })?;
            let Some(ev) = self.key_rx.recv().await else {
                return Ok(PickOutcome::Quit);
            };
            let Event::Key(key) = ev else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let was_pending_g = std::mem::take(&mut pending_g);
            let max = summaries.len().saturating_sub(1);
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(PickOutcome::Quit),
                KeyCode::Char('r') => return Ok(PickOutcome::Refresh),
                KeyCode::Char('m') => return Ok(PickOutcome::ToggleMine),
                // Ctrl-i is Tab in most terminals (both 0x09) — accept either
                // as vim-style "forward" back into the last comment view.
                KeyCode::Tab if can_forward => return Ok(PickOutcome::Forward),
                KeyCode::Char('i')
                    if key.modifiers.contains(KeyModifiers::CONTROL) && can_forward =>
                {
                    return Ok(PickOutcome::Forward)
                }
                KeyCode::Enter => {
                    if let Some(s) = summaries.get(selected) {
                        return Ok(PickOutcome::Pr(s.clone()));
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(max),
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    selected = (selected + 10).min(max)
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    selected = selected.saturating_sub(10)
                }
                KeyCode::Char('g') => {
                    if was_pending_g {
                        selected = 0;
                    } else {
                        pending_g = true;
                    }
                }
                KeyCode::Char('G') => selected = max,
                _ => {}
            }
        }
    }

    /// The comment view (select → instruct → run → done).
    pub async fn run_comments(&mut self, app: &mut App, actx: &AppCtx) -> Result<CommentExit> {
        let mut agent_rx: Option<UnboundedReceiver<RunEvent>> = None;
        let mut abort_tx: Option<UnboundedSender<()>> = None;
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));

        loop {
            self.terminal.draw(|f| draw(f, app))?;

            enum Ev {
                Key(Event),
                Agent(RunEvent),
                Tick,
            }
            let ev = tokio::select! {
                Some(k) = self.key_rx.recv() => Ev::Key(k),
                r = recv_opt(&mut agent_rx) => match r {
                    Some(x) => Ev::Agent(x),
                    None => { agent_rx = None; Ev::Tick }
                },
                _ = tick.tick() => Ev::Tick,
            };

            match ev {
                Ev::Tick => {}
                Ev::Agent(RunEvent::Step(step)) => {
                    app.log.push(step);
                    app.last_event = Some(Instant::now());
                }
                Ev::Agent(RunEvent::Finished { ok, summary }) => {
                    app.phase = Phase::Done { ok, summary };
                    agent_rx = None;
                    abort_tx = None;
                }
                Ev::Key(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    app.notice = None;
                    match handle_key(app, key) {
                        Flow::Quit => {
                            if let Some(tx) = &abort_tx {
                                let _ = tx.send(());
                            }
                            return Ok(CommentExit::Quit);
                        }
                        Flow::Back => {
                            if let Some(tx) = &abort_tx {
                                let _ = tx.send(());
                            }
                            return Ok(CommentExit::BackToList);
                        }
                        Flow::Abort => {
                            if let Some(tx) = &abort_tx {
                                let _ = tx.send(());
                                app.notice = Some("aborting…".into());
                            }
                        }
                        Flow::StartRun => {
                            let (tx, rx) = unbounded_channel();
                            let (atx, arx) = unbounded_channel();
                            let run_dir = prepare_run_dir(&app.repo, app.pr_number)?;
                            let ctx = RunCtx {
                                repo_root: app.repo_root.clone(),
                                repo: app.repo.clone(),
                                pr_number: app.pr_number,
                                pr_title: app.pr_title.clone(),
                                branch: app.branch.clone(),
                                items_json: items_json(&app.items),
                                claude_bin: actx.claude_bin.clone(),
                                max_turns: actx.max_turns,
                                github_token: actx.token.clone(),
                                run_dir: run_dir.clone(),
                            };
                            app.run_dir = Some(run_dir);
                            app.phase = Phase::Running;
                            app.run_started = Some(Instant::now());
                            app.last_event = Some(Instant::now());
                            app.log.clear();
                            app.log_offset = 0;
                            agent_rx = Some(rx);
                            abort_tx = Some(atx);
                            tokio::spawn(agent::run_agent(ctx, tx, arx));
                        }
                        Flow::Continue => {}
                    }
                }
                Ev::Key(_) => {}
            }
        }
    }
}

/// Per-repo "seen" state: pr number → epoch of when you last opened that
/// PR's comment view. Drives the NEW indicator — independent of runs.
fn seen_path(repo: &str) -> Result<PathBuf> {
    Ok(data_dir()?
        .join("runs")
        .join(repo.replace('/', "__"))
        .join("seen.json"))
}

pub fn load_seen(repo: &str) -> std::collections::HashMap<u64, i64> {
    seen_path(repo)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Best-effort: losing this file costs highlight accuracy, not correctness.
pub fn mark_seen(repo: &str, pr: u64) {
    let mut seen = load_seen(repo);
    seen.insert(pr, chrono::Utc::now().timestamp());
    let Ok(path) = seen_path(repo) else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&seen) {
        let _ = std::fs::write(path, json);
    }
}

/// Compact relative age: "5m", "3h", "2d".
fn rel_age(epoch: i64) -> String {
    let s = (chrono::Utc::now().timestamp() - epoch).max(0);
    if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_pr_list(
    f: &mut Frame,
    repo: &str,
    current_branch: &str,
    summaries: &[PrSummary],
    seen: &std::collections::HashMap<u64, i64>,
    selected: usize,
    notice: Option<&str>,
    can_forward: bool,
    mine_only: bool,
) {
    let [header, main, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(f.area());

    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "trpr ",
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{repo} — open PRs (current branch: {current_branch})")),
            ]),
            Line::styled(
                "select a PR to triage its comments — selecting another branch switches your checkout",
                Style::new().fg(Color::DarkGray),
            ),
        ]),
        header,
    );

    // Content-sized columns (clamped), with the title taking all remaining
    // width and padded to it — so every column aligns exactly and each row
    // spans the full box (making the highlight bar full-width too). `fit`
    // guarantees exact widths; naive truncation drifted by the '…' char.
    let inner_w = main.width.saturating_sub(2) as usize;
    let num_w = summaries
        .iter()
        .map(|s| s.number.to_string().len())
        .max()
        .unwrap_or(1);
    let branch_w = summaries
        .iter()
        .map(|s| s.branch.chars().count())
        .max()
        .unwrap_or(6)
        .clamp(6, 32);
    let owner_w = summaries
        .iter()
        .map(|s| s.triage_owner().chars().count() + 1) // "@" prefix
        .max()
        .unwrap_or(5)
        .clamp(5, 16);
    let open_w = summaries
        .iter()
        .map(|s| s.unresolved.to_string().len())
        .max()
        .unwrap_or(1)
        .max(2);
    const BADGE_W: usize = 12; // "seen · 12d" / "● NEW 12d"
    let fixed = 2 + 1 + num_w + 1 + branch_w + 1 + owner_w + 1 + open_w + 5 + 2 + BADGE_W + 2;
    let title_w = inner_w.saturating_sub(fixed).max(8);

    let rows: Vec<ListItem> = summaries
        .iter()
        .map(|s| {
            // NEW = someone (not you, not a bot) commented since you last
            // opened this PR's comments in trpr.
            let is_new =
                s.last_activity > 0 && s.last_activity > seen.get(&s.number).copied().unwrap_or(0);
            let (badge, badge_style) = if s.last_activity == 0 {
                ("—".to_string(), Style::new().fg(Color::DarkGray))
            } else if is_new {
                (
                    format!("● NEW {}", rel_age(s.last_activity)),
                    Style::new().fg(Color::Yellow),
                )
            } else {
                (
                    format!("seen · {}", rel_age(s.last_activity)),
                    Style::new().fg(Color::DarkGray),
                )
            };
            let current = if s.branch == current_branch {
                "▶"
            } else {
                " "
            };
            let main_style = if s.branch == current_branch {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(
                        "{current} #{:<num_w$} {} {} {:>open_w$} open  ",
                        s.number,
                        fit(&s.branch, branch_w),
                        fit(&format!("@{}", s.triage_owner()), owner_w),
                        s.unresolved,
                    ),
                    main_style,
                ),
                Span::styled(fit(&badge, BADGE_W), badge_style),
                Span::styled(format!("  {}", fit(&s.title, title_w)), main_style),
            ]))
        })
        .collect();
    let list = List::new(rows)
        .block(Block::bordered().title(format!(
            "{} open PR(s) — {} · newest activity first",
            summaries.len(),
            if mine_only { "yours" } else { "all" },
        )))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    state.select(Some(selected.min(summaries.len().saturating_sub(1))));
    f.render_stateful_widget(list, main, &mut state);

    let mut help =
        "j/k move · gg/G · ^d/^u · Enter open · m yours/all · r refresh · q quit".to_string();
    if can_forward {
        help.push_str(" · ^i/Tab resume last view");
    }
    let mut lines = vec![Line::raw(help)];
    if let Some(n) = notice {
        lines.push(Line::styled(n.to_string(), Style::new().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(lines), footer);
}

/// Pad or truncate to exactly `w` display columns (char-counted): the
/// building block for aligned table rows. Truncation ends in '…'.
fn fit(s: &str, w: usize) -> String {
    let count = s.chars().count();
    match count.cmp(&w) {
        std::cmp::Ordering::Equal => s.to_string(),
        std::cmp::Ordering::Less => format!("{s}{}", " ".repeat(w - count)),
        std::cmp::Ordering::Greater => {
            let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
            t.push('…');
            t
        }
    }
}

/// The detail pane's maximum scroll offset — the comment's (unwrapped) line
/// count. Wrapped height is >= that, so the bottom stays reachable without
/// endless blank scrolling.
fn detail_max(app: &App) -> u16 {
    app.items
        .get(app.selected)
        .map(|i| detail_text(i).lines.len() as u16)
        .unwrap_or(0)
}

fn scroll_detail_down(app: &mut App, n: u16) {
    app.detail_scroll = app.detail_scroll.saturating_add(n).min(detail_max(app));
}

async fn recv_opt(rx: &mut Option<UnboundedReceiver<RunEvent>>) -> Option<RunEvent> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

fn handle_key(app: &mut App, key: KeyEvent) -> Flow {
    // vim-style gg: a lone 'g' arms this; any other key disarms it.
    let pending_g = std::mem::take(&mut app.pending_g);
    match &mut app.phase {
        Phase::Select => match key.code {
            KeyCode::Char('q') => return Flow::Quit,
            KeyCode::Esc => {
                return if app.from_list {
                    Flow::Back
                } else {
                    Flow::Quit
                };
            }
            // vim-style jump back to the PR list (list mode only; ^i/Tab on
            // the list then resumes this view with its state intact).
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if app.from_list {
                    return Flow::Back;
                }
            }
            KeyCode::Tab => {
                app.focus = match app.focus {
                    Focus::List => Focus::Detail,
                    Focus::Detail => Focus::List,
                };
            }
            KeyCode::Down | KeyCode::Char('j') => match app.focus {
                Focus::List => {
                    if app.selected + 1 < app.items.len() {
                        app.selected += 1;
                    }
                    app.detail_scroll = 0;
                }
                Focus::Detail => scroll_detail_down(app, 1),
            },
            KeyCode::Up | KeyCode::Char('k') => match app.focus {
                Focus::List => {
                    app.selected = app.selected.saturating_sub(1);
                    app.detail_scroll = 0;
                }
                Focus::Detail => app.detail_scroll = app.detail_scroll.saturating_sub(1),
            },
            KeyCode::PageDown => scroll_detail_down(app, 10),
            KeyCode::PageUp => {
                app.detail_scroll = app.detail_scroll.saturating_sub(10);
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match app.focus {
                    Focus::List => {
                        app.selected = (app.selected + 10).min(app.items.len().saturating_sub(1));
                        app.detail_scroll = 0;
                    }
                    Focus::Detail => scroll_detail_down(app, 10),
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match app.focus {
                    Focus::List => {
                        app.selected = app.selected.saturating_sub(10);
                        app.detail_scroll = 0;
                    }
                    Focus::Detail => app.detail_scroll = app.detail_scroll.saturating_sub(10),
                }
            }
            KeyCode::Char('g') => {
                if pending_g {
                    match app.focus {
                        Focus::List => {
                            app.selected = 0;
                            app.detail_scroll = 0;
                        }
                        Focus::Detail => app.detail_scroll = 0,
                    }
                } else {
                    app.pending_g = true;
                }
            }
            KeyCode::Char('G') => match app.focus {
                Focus::List => {
                    app.selected = app.items.len().saturating_sub(1);
                    app.detail_scroll = 0;
                }
                Focus::Detail => {
                    app.detail_scroll = detail_max(app);
                }
            },
            KeyCode::Char('a') => {
                if let Some(item) = app.items.get_mut(app.selected) {
                    item.decision = Decision::Instructed(
                        "Implement this comment as the reviewer stated.".to_string(),
                    );
                }
            }
            KeyCode::Char('x') => {
                if let Some(item) = app.items.get_mut(app.selected) {
                    let url = item.item.url.clone();
                    if item.decision == Decision::Ignored {
                        item.decision = Decision::Pending;
                        // A deliberate un-ignore clears the marker entirely —
                        // otherwise the stale timestamp masquerades as a
                        // "resurfaced" badge.
                        item.prior_ignored = None;
                        app.ignored_at.remove(&url);
                    } else {
                        let now = chrono::Utc::now().timestamp();
                        item.decision = Decision::Ignored;
                        item.prior_ignored = Some(now);
                        app.ignored_at.insert(url, now);
                    }
                    save_ignored(app); // best-effort persistence
                }
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                if let Some(item) = app.items.get(app.selected) {
                    let existing = match &item.decision {
                        Decision::Instructed(s) => s.clone(),
                        _ => String::new(),
                    };
                    app.phase = Phase::Edit {
                        editor: Box::new(VimEditor::new(&existing)),
                    };
                }
            }
            KeyCode::Char('r') => {
                if app.instructed_count() == 0 {
                    app.notice =
                        Some("nothing to do — instruct at least one comment (a or e)".into());
                } else {
                    return Flow::StartRun;
                }
            }
            _ => {}
        },
        Phase::Edit { editor } => match editor.handle(key) {
            EditorAction::Continue => {}
            EditorAction::Cancel => app.phase = Phase::Select,
            EditorAction::Save(text) => {
                let text = text.trim().to_string();
                let decision = if text.is_empty() {
                    Decision::Pending
                } else {
                    Decision::Instructed(text)
                };
                if let Some(item) = app.items.get_mut(app.selected) {
                    item.decision = decision;
                }
                app.phase = Phase::Select;
            }
        },
        Phase::Running => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Flow::Abort,
            other => log_scroll_key(app, other, key.modifiers, pending_g),
        },
        Phase::Done { .. } => match key.code {
            KeyCode::Char('q') => return Flow::Quit,
            KeyCode::Esc => {
                return if app.from_list {
                    Flow::Back
                } else {
                    Flow::Quit
                };
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if app.from_list {
                    return Flow::Back;
                }
            }
            other => log_scroll_key(app, other, key.modifiers, pending_g),
        },
    }
    Flow::Continue
}

/// Shared scrolling for the run-log view (Running and Done phases).
/// log_offset counts up from the bottom: 0 = follow the tail.
fn log_scroll_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers, pending_g: bool) {
    let up = |app: &mut App, n: usize| {
        app.log_offset = (app.log_offset + n).min(app.log.len());
    };
    let down = |app: &mut App, n: usize| {
        app.log_offset = app.log_offset.saturating_sub(n);
    };
    match code {
        KeyCode::Up | KeyCode::Char('k') => up(app, 1),
        KeyCode::Down | KeyCode::Char('j') => down(app, 1),
        KeyCode::PageUp => up(app, 20),
        KeyCode::PageDown => down(app, 20),
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => up(app, 10),
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => down(app, 10),
        KeyCode::Char('g') => {
            if pending_g {
                app.log_offset = app.log.len(); // top
            } else {
                app.pending_g = true;
            }
        }
        KeyCode::Char('G') => app.log_offset = 0, // bottom (follow)
        _ => {}
    }
}

/// Global data dir: $TRPR_DATA_DIR, or ~/.trpr. Living outside the checkout
/// means no per-repo gitignore games and artifacts survive checkout deletion.
fn data_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("TRPR_DATA_DIR") {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".trpr"))
}

fn pr_dir(repo: &str, pr_number: u64) -> Result<PathBuf> {
    Ok(data_dir()?
        .join("runs")
        .join(repo.replace('/', "__"))
        .join(format!("pr-{pr_number}")))
}

/// Persisted ignore decisions (comment url → epoch ignored). Ignores are the
/// one triage decision git can't carry — no change means no commit — so they
/// get a small per-PR file instead.
pub fn load_ignored(repo: &str, pr_number: u64) -> std::collections::HashMap<String, i64> {
    pr_dir(repo, pr_number)
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join("ignored.json")).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Best-effort save; losing this file costs badges, not correctness.
fn save_ignored(app: &App) {
    let Ok(dir) = pr_dir(&app.repo, app.pr_number) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(&app.ignored_at) {
        let _ = std::fs::write(dir.join("ignored.json"), json);
    }
}

/// <data-dir>/runs/<owner>__<repo>/pr-<n>/<timestamp> — repo dimension is
/// explicit now that all repos share one tree. Returned absolute, since the
/// agent needs it as an additional writable directory outside its cwd.
fn prepare_run_dir(repo: &str, pr_number: u64) -> Result<PathBuf> {
    let run_dir =
        pr_dir(repo, pr_number)?.join(chrono::Local::now().format("%Y-%m-%d_%H%M%S").to_string());
    std::fs::create_dir_all(&run_dir)?;
    Ok(std::fs::canonicalize(&run_dir)?)
}

fn items_json(items: &[UiItem]) -> String {
    let selected: Vec<serde_json::Value> = items
        .iter()
        .filter_map(|ui| {
            let Decision::Instructed(instruction) = &ui.decision else {
                return None;
            };
            let (kind, path, line, outdated) = match &ui.item.kind {
                ItemKind::Thread {
                    path,
                    line,
                    outdated,
                } => ("review_thread", Some(path.clone()), *line, *outdated),
                ItemKind::Conversation => ("conversation_comment", None, None, false),
            };
            Some(serde_json::json!({
                "kind": kind,
                "path": path,
                "line": line,
                "outdated": outdated,
                "author": ui.item.author,
                "comment": ui.item.body,
                "thread_replies": ui.item.replies.iter()
                    .map(|(a, b)| serde_json::json!({"author": a, "body": b}))
                    .collect::<Vec<_>>(),
                "diff_hunk": ui.item.diff_hunk,
                "url": ui.item.url,
                "previously_committed": ui.handled.as_ref().map(|(sha, _)| sha.clone()),
                "developer_instruction": instruction,
            }))
        })
        .collect();
    serde_json::to_string_pretty(&selected).unwrap_or_else(|_| "[]".to_string())
}

// ------------------------------------------------------------------ draw ---

fn draw(f: &mut Frame, app: &App) {
    let [header, main, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(f.area());

    draw_header(f, app, header);
    match &app.phase {
        Phase::Select | Phase::Edit { .. } => draw_select(f, app, main),
        Phase::Running | Phase::Done { .. } => draw_log(f, app, main),
    }
    draw_footer(f, app, footer);

    if let Phase::Edit { editor } = &app.phase {
        draw_edit_popup(f, editor, f.area());
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "trpr ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "{} PR #{} — {} (branch {})",
            app.repo, app.pr_number, app.pr_title, app.branch
        )),
    ])];
    lines.push(Line::styled(
        "agent commits per handled comment (Addresses: trailers) — you review, then push",
        Style::new().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_select(f: &mut Frame, app: &App, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(area);

    let items: Vec<ListItem> = app
        .items
        .iter()
        .map(|ui| {
            let (glyph, style) = match &ui.decision {
                Decision::Instructed(_) => ("✓ ", Style::new().fg(Color::Green)),
                Decision::Ignored => ("✗ ", Style::new().fg(Color::DarkGray)),
                Decision::Pending => match &ui.handled {
                    // Committed in an earlier run, but the thread has moved on.
                    Some((_, t)) if ui.item.last_activity > *t => {
                        ("↺ ", Style::new().fg(Color::Yellow))
                    }
                    // Committed in an earlier run, nothing new since.
                    Some(_) => ("✔ ", Style::new().fg(Color::DarkGray)),
                    // Previously ignored, and there's genuinely new activity.
                    None if ui
                        .prior_ignored
                        .is_some_and(|ts| ui.item.last_activity > ts) =>
                    {
                        ("! ", Style::new().fg(Color::Yellow))
                    }
                    None => ("· ", Style::new()),
                },
            };
            ListItem::new(Line::styled(format!("{glyph}{}", ui.item.label()), style))
        })
        .collect();
    let focused = Style::new().fg(Color::Cyan);
    let unfocused = Style::new().fg(Color::DarkGray);
    let (list_border, detail_border) = match app.focus {
        Focus::List => (focused, unfocused),
        Focus::Detail => (unfocused, focused),
    };

    let list = List::new(items)
        .block(Block::bordered().border_style(list_border).title(format!(
            "comments ({} to implement)",
            app.instructed_count()
        )))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    state.select(Some(app.selected.min(app.items.len().saturating_sub(1))));
    f.render_stateful_widget(list, left, &mut state);

    let detail = app
        .items
        .get(app.selected)
        .map(detail_text)
        .unwrap_or_default();
    let title = match (app.focus, app.detail_scroll) {
        (Focus::Detail, 0) => "detail — j/k scroll · Tab back".to_string(),
        (Focus::Detail, n) => format!("detail — j/k scroll (+{n}) · Tab back"),
        (Focus::List, 0) => "detail — Tab to focus & scroll".to_string(),
        (Focus::List, n) => format!("detail (+{n}) — Tab to focus & scroll"),
    };
    f.render_widget(
        Paragraph::new(detail)
            .block(Block::bordered().border_style(detail_border).title(title))
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll, 0)),
        right,
    );
}

fn detail_text(ui: &UiItem) -> Text<'static> {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::styled(
        format!("@{} — {}", ui.item.author, ui.item.url),
        Style::new().fg(Color::DarkGray),
    ));
    if let Some((sha, t)) = &ui.handled {
        let suffix = if ui.item.last_activity > *t {
            " — NEW REPLY since that commit"
        } else {
            ""
        };
        lines.push(Line::styled(
            format!("previously committed in {sha}{suffix}"),
            Style::new().fg(Color::Yellow),
        ));
    } else if ui.decision == Decision::Pending
        && ui
            .prior_ignored
            .is_some_and(|ts| ui.item.last_activity > ts)
    {
        lines.push(Line::styled(
            "previously ignored — resurfaced due to new activity".to_string(),
            Style::new().fg(Color::Yellow),
        ));
    }
    if let Some(hunk) = &ui.item.diff_hunk {
        for l in hunk
            .lines()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            lines.push(Line::styled(
                l.to_string(),
                Style::new().fg(Color::DarkGray),
            ));
        }
    }
    lines.push(Line::raw(""));
    for l in ui.item.body.lines() {
        lines.push(Line::raw(l.to_string()));
    }
    for (author, body) in &ui.item.replies {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("↳ @{author}:"),
            Style::new().fg(Color::DarkGray),
        ));
        for l in body.lines() {
            lines.push(Line::raw(l.to_string()));
        }
    }
    match &ui.decision {
        Decision::Instructed(s) => {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "instruction:",
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            ));
            for l in s.lines() {
                lines.push(Line::styled(l.to_string(), Style::new().fg(Color::Green)));
            }
        }
        Decision::Ignored => {
            lines.push(Line::raw(""));
            lines.push(Line::styled("(ignored)", Style::new().fg(Color::DarkGray)));
        }
        Decision::Pending => {}
    }
    Text::from(lines)
}

fn draw_log(f: &mut Frame, app: &App, area: Rect) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let end = app.log.len().saturating_sub(app.log_offset);
    let start = end.saturating_sub(inner_height);
    let lines: Vec<Line> = app.log[start..end]
        .iter()
        .map(|s| {
            let style = match s.kind {
                StepKind::Thinking | StepKind::ToolResult | StepKind::Meta => {
                    Style::new().fg(Color::DarkGray)
                }
                StepKind::ToolUse => Style::new().add_modifier(Modifier::BOLD),
                StepKind::Text => Style::new(),
            };
            Line::styled(s.text.clone(), style)
        })
        .collect();
    let title = match &app.phase {
        Phase::Done { ok: true, .. } => "agent — finished ok".to_string(),
        Phase::Done { ok: false, .. } => "agent — FAILED".to_string(),
        _ => "agent — running".to_string(),
    };
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    match &app.phase {
        Phase::Select => {
            let mut help = match app.focus {
                Focus::List => {
                    "j/k move · gg/G top/bot · ^d/^u jump · Tab→detail · a as-stated · e edit · x ignore · r run · q quit"
                }
                Focus::Detail => {
                    "j/k scroll · gg/G top/bot · ^d/^u jump · Tab→list · a as-stated · e edit · x ignore · r run · q quit"
                }
            }
            .to_string();
            if app.from_list {
                help.push_str(" · ^o PR list");
            }
            lines.push(Line::raw(help));
        }
        Phase::Edit { .. } => lines.push(Line::raw(
            "vim: i/a/o insert · Esc normal · hjkl w b 0 $ gg G · x dd D C · yy p · u ^r · :wq save · :q cancel",
        )),
        Phase::Running => {
            let elapsed = app.run_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            let quiet = app.last_event.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            let mut s = format!("running {elapsed}s · q abort · j/k gg/G ^d/^u scroll");
            if quiet >= 30 {
                s.push_str(&format!(
                    " · last output {quiet}s ago (long tool calls show nothing until done)"
                ));
            }
            lines.push(Line::raw(s));
        }
        Phase::Done { ok, summary } => {
            let style = if *ok {
                Style::new().fg(Color::Green)
            } else {
                Style::new().fg(Color::Red)
            };
            lines.push(Line::styled(
                format!(
                    "{} — {} · q quit",
                    if *ok {
                        "done — commits are on your branch (review, then push yourself)"
                    } else {
                        "failed — check git log/status for partial work"
                    },
                    crate::agent::truncate(summary, 120)
                ),
                style,
            ));
        }
    }
    if let Some(n) = &app.notice {
        lines.push(Line::styled(n.clone(), Style::new().fg(Color::Yellow)));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_edit_popup(f: &mut Frame, ed: &VimEditor, area: Rect) {
    let height = ((ed.ta.lines().len() as u16) + 3)
        .clamp(6, 16)
        .min(area.height);
    let popup = centered(area, 70, height);
    f.render_widget(Clear, popup);

    let block = Block::bordered().title("instruction — :wq save · :q cancel");
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let [text_area, status_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

    // The TextArea widget renders its own cursor and scrolls to keep it visible.
    f.render_widget(&ed.ta, text_area);

    // Status line: mode / command line / error, vim-style.
    let status = match &ed.mode {
        VimMode::Command(cmd) => Line::from(vec![
            Span::raw(format!(":{cmd}")),
            Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
        ]),
        VimMode::Insert => Line::styled("-- INSERT --", Style::new().add_modifier(Modifier::BOLD)),
        VimMode::Normal => match &ed.error {
            Some(e) => Line::styled(e.clone(), Style::new().fg(Color::Red)),
            None => Line::styled("-- NORMAL --", Style::new().fg(Color::DarkGray)),
        },
    };
    f.render_widget(Paragraph::new(status), status_area);
}

fn centered(area: Rect, pct_x: u16, height: u16) -> Rect {
    let width = area.width * pct_x / 100;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height: height.min(area.height),
    }
}
