//! The ratatui interface: pick comments, write instructions, launch the
//! agent, watch it work.

use crate::agent::{self, RunCtx, RunEvent, Step, StepKind};
use crate::github::{CommentItem, ItemKind};
use anyhow::Result;
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
}

pub enum Phase {
    Select,
    /// Editing the instruction for the selected item.
    Edit {
        buffer: String,
    },
    ConfirmDirty,
    Running,
    Done {
        ok: bool,
        summary: String,
    },
}

pub struct App {
    pub repo: String,
    pub repo_root: PathBuf,
    pub branch: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub dirty: bool,
    pub items: Vec<UiItem>,
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
}

impl App {
    pub fn new(
        repo: String,
        repo_root: PathBuf,
        branch: String,
        pr_number: u64,
        pr_title: String,
        dirty: bool,
        items: Vec<CommentItem>,
    ) -> Self {
        Self {
            repo,
            repo_root,
            branch,
            pr_number,
            pr_title,
            dirty,
            items: items
                .into_iter()
                .map(|item| UiItem {
                    item,
                    decision: Decision::Pending,
                })
                .collect(),
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
    StartRun,
    Abort,
}

pub async fn run(mut app: App, actx: AppCtx) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &actx).await;
    ratatui::restore();
    if let Some(dir) = &app.run_dir {
        println!("Run artifacts: {}", dir.display());
        println!(
            "Agent changes (if any) are uncommitted in {}",
            app.repo_root.display()
        );
    }
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    actx: &AppCtx,
) -> Result<()> {
    // Blocking key reader thread bridged into the async loop.
    let (key_tx, mut key_rx) = unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if key_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let mut agent_rx: Option<UnboundedReceiver<RunEvent>> = None;
    let mut abort_tx: Option<UnboundedSender<()>> = None;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));

    loop {
        terminal.draw(|f| draw(f, app))?;

        enum Ev {
            Key(Event),
            Agent(RunEvent),
            Tick,
        }
        let ev = tokio::select! {
            Some(k) = key_rx.recv() => Ev::Key(k),
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
                        return Ok(());
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
                        let run_dir = prepare_run_dir(&app.repo_root)?;
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
            KeyCode::Char('q') | KeyCode::Esc => return Flow::Quit,
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
                    item.decision = if item.decision == Decision::Ignored {
                        Decision::Pending
                    } else {
                        Decision::Ignored
                    };
                }
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                if let Some(item) = app.items.get(app.selected) {
                    let buffer = match &item.decision {
                        Decision::Instructed(s) => s.clone(),
                        _ => String::new(),
                    };
                    app.phase = Phase::Edit { buffer };
                }
            }
            KeyCode::Char('r') => {
                if app.instructed_count() == 0 {
                    app.notice =
                        Some("nothing to do — instruct at least one comment (a or e)".into());
                } else if app.dirty {
                    app.phase = Phase::ConfirmDirty;
                } else {
                    return Flow::StartRun;
                }
            }
            _ => {}
        },
        Phase::Edit { buffer } => match key.code {
            KeyCode::Esc => app.phase = Phase::Select,
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => buffer.push('\n'),
            KeyCode::Enter => {
                let text = buffer.trim().to_string();
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
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(c) => buffer.push(c),
            _ => {}
        },
        Phase::ConfirmDirty => match key.code {
            KeyCode::Char('y') | KeyCode::Enter => return Flow::StartRun,
            _ => app.phase = Phase::Select,
        },
        Phase::Running => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Flow::Abort,
            other => log_scroll_key(app, other, key.modifiers, pending_g),
        },
        Phase::Done { .. } => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Flow::Quit,
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

/// .trpr/runs/<timestamp>, plus the self-ignoring .trpr/.gitignore so the
/// directory never shows up in git status or the PR diff.
fn prepare_run_dir(repo_root: &std::path::Path) -> Result<PathBuf> {
    let trpr = repo_root.join(".trpr");
    std::fs::create_dir_all(&trpr)?;
    let gitignore = trpr.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "*\n")?;
    }
    let run_dir = trpr
        .join("runs")
        .join(chrono::Local::now().format("%Y-%m-%d_%H%M%S").to_string());
    std::fs::create_dir_all(&run_dir)?;
    Ok(run_dir)
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
        Phase::Select | Phase::Edit { .. } | Phase::ConfirmDirty => {
            draw_select(f, app, main);
        }
        Phase::Running | Phase::Done { .. } => draw_log(f, app, main),
    }
    draw_footer(f, app, footer);

    if let Phase::Edit { buffer } = &app.phase {
        draw_edit_popup(f, buffer, f.area());
    }
    if matches!(app.phase, Phase::ConfirmDirty) {
        draw_confirm_popup(f, f.area());
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
    if app.dirty {
        lines.push(Line::styled(
            "⚠ working tree has uncommitted changes — agent edits will mix with them",
            Style::new().fg(Color::Yellow),
        ));
    }
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
                Decision::Pending => ("· ", Style::new()),
                Decision::Ignored => ("✗ ", Style::new().fg(Color::DarkGray)),
                Decision::Instructed(_) => ("✓ ", Style::new().fg(Color::Green)),
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
        Phase::Select => lines.push(Line::raw(match app.focus {
            Focus::List => {
                "j/k move · gg/G top/bot · ^d/^u jump · Tab→detail · a as-stated · e edit · x ignore · r run · q quit"
            }
            Focus::Detail => {
                "j/k scroll · gg/G top/bot · ^d/^u jump · Tab→list · a as-stated · e edit · x ignore · r run · q quit"
            }
        })),
        Phase::Edit { .. } => lines.push(Line::raw(
            "Enter save · Alt+Enter newline · Esc cancel",
        )),
        Phase::ConfirmDirty => lines.push(Line::raw("y start anyway · any other key: back")),
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
                    if *ok { "done — changes are uncommitted in your tree" } else { "failed" },
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

fn draw_edit_popup(f: &mut Frame, buffer: &str, area: Rect) {
    let popup = centered(area, 70, 9);
    f.render_widget(Clear, popup);
    let mut text: Vec<Line> = buffer.lines().map(|l| Line::raw(l.to_string())).collect();
    if buffer.ends_with('\n') || text.is_empty() {
        text.push(Line::raw(""));
    }
    if let Some(last) = text.last_mut() {
        last.spans
            .push(Span::styled("█", Style::new().fg(Color::Cyan)));
    }
    f.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().title("instruction for the agent"))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_confirm_popup(f: &mut Frame, area: Rect) {
    let popup = centered(area, 60, 5);
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(
            "Working tree has uncommitted changes.\nAgent edits will mix with them in git diff.\nStart anyway? (y/n)",
        )
        .block(Block::bordered().title("dirty working tree"))
        .wrap(Wrap { trim: false }),
        popup,
    );
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
