use crate::github::Comment;
use crate::{notify, worktree};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tracing::{error, info};

const PROMPT_TEMPLATE: &str = include_str!("../assets/prompt.md");
const SETTINGS_TEMPLATE: &str = include_str!("../assets/agent-settings.json");

/// Everything a spawned task needs — cloned out of config/state so the task
/// owns its data and never touches shared state. Completion is communicated
/// back to the main loop via result.json in the task dir.
#[derive(Debug, Clone)]
pub struct TaskCtx {
    pub repo: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub inbox_dir: PathBuf,
    pub claude_bin: String,
    pub max_turns: u32,
    pub github_token: String,
    pub notifications: bool,
    /// Print the agent's steps (thinking, tool calls, text) live to stdout.
    /// On for foreground `kwkly run`; off for daemon tasks (which may run
    /// several agents concurrently — their transcripts go to files instead).
    pub stream_output: bool,
    pub share_build_cache: bool,
    pub agent_env: std::collections::HashMap<String, String>,
    pub comments: Vec<Comment>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TaskResult {
    pub ok: bool,
    pub has_diff: bool,
    pub summary: String,
    pub finished_at: chrono::DateTime<chrono::Utc>,
}

/// Entry point for a spawned task. Always writes result.json (success or
/// failure) so the reconcile pass in the main loop can move the PR out of
/// Running, then fires a desktop notification.
pub async fn run_task(ctx: TaskCtx) {
    let task_dir = worktree::task_dir(&ctx.inbox_dir, &ctx.repo, ctx.pr_number);

    if ctx.notifications {
        let title = format!("kwkly: {} #{}", ctx.repo, ctx.pr_number);
        // Empty comments = crash-recovered task re-running from comments.json.
        let msg = if ctx.comments.is_empty() {
            "Agent started (resuming recovered task)".to_string()
        } else {
            format!("Agent started on {} new comment(s)", ctx.comments.len())
        };
        notify::notify(&title, &msg).await;
    }

    let result = match run_inner(&ctx, &task_dir).await {
        Ok(r) => r,
        Err(e) => {
            error!("{} PR #{}: task failed: {e:#}", ctx.repo, ctx.pr_number);
            TaskResult {
                ok: false,
                has_diff: false,
                summary: format!("{e:#}"),
                finished_at: chrono::Utc::now(),
            }
        }
    };

    if let Err(e) = write_result(&task_dir, &result) {
        error!("{} PR #{}: writing result.json failed: {e:#}", ctx.repo, ctx.pr_number);
    }

    if ctx.notifications {
        let title = format!("kwkly: {} #{}", ctx.repo, ctx.pr_number);
        let msg = if !result.ok {
            "Task FAILED — see agent-stderr.log".to_string()
        } else if result.has_diff {
            "Diff ready for review".to_string()
        } else {
            "Done — no code changes (see PLAN.md)".to_string()
        };
        notify::notify(&title, &msg).await;
    }
}

async fn run_inner(ctx: &TaskCtx, task_dir: &Path) -> Result<TaskResult> {
    std::fs::create_dir_all(task_dir)?;

    // Persist the comment batch before doing anything else: it's both the
    // agent's input of record and the crash-recovery source (a task restarted
    // after a daemon crash arrives with ctx.comments empty and reads this file).
    let comments_path = task_dir.join("comments.json");
    let comments: Vec<Comment> = if ctx.comments.is_empty() {
        let raw = std::fs::read_to_string(&comments_path)
            .context("recovering task with no comments.json on disk")?;
        serde_json::from_str(&raw)?
    } else {
        std::fs::write(&comments_path, serde_json::to_string_pretty(&ctx.comments)?)?;
        ctx.comments.clone()
    };

    let clone = worktree::ensure_clone(&ctx.inbox_dir, &ctx.repo, &ctx.github_token).await?;
    let wt =
        worktree::prepare_worktree(&clone, task_dir, ctx.pr_number, &ctx.github_token).await?;
    let task_dir_abs = std::fs::canonicalize(task_dir)?;

    // Per-task settings: the static template plus this task's absolute dir as
    // an additional writable directory (for PLAN.md / REPLY-DRAFT.md).
    let settings_path = task_dir.join("agent-settings.json");
    let settings =
        SETTINGS_TEMPLATE.replace("{{TASK_DIR}}", &task_dir_abs.to_string_lossy());
    std::fs::write(&settings_path, settings)?;

    let prompt = PROMPT_TEMPLATE
        .replace("{{REPO}}", &ctx.repo)
        .replace("{{PR_NUMBER}}", &ctx.pr_number.to_string())
        .replace("{{PR_TITLE}}", &ctx.pr_title)
        .replace("{{TASK_DIR}}", &task_dir_abs.to_string_lossy())
        .replace("{{COMMENTS_JSON}}", &serde_json::to_string_pretty(&comments)?);

    info!(
        "{} PR #{}: launching agent on {} comment(s)",
        ctx.repo,
        ctx.pr_number,
        comments.len()
    );

    let mut cmd = tokio::process::Command::new(&ctx.claude_bin);
    cmd.args([
        "-p",
        "--output-format",
        "stream-json", // JSONL event stream; --verbose is required with it in print mode
        "--verbose",
        "--max-turns",
        &ctx.max_turns.to_string(),
        "--settings",
        &settings_path.to_string_lossy(),
    ])
    .current_dir(&wt)
    // The read-only PAT is the only GitHub credential the agent sees —
    // `gh` inside the sandbox physically cannot write to GitHub.
    .env("GH_TOKEN", &ctx.github_token)
    .env("GITHUB_TOKEN", &ctx.github_token)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let repo_root = worktree::repo_root(&ctx.inbox_dir, &ctx.repo);
    let repo_root = std::fs::canonicalize(&repo_root).unwrap_or(repo_root);

    // Rust repos: share one target dir across the repo's worktrees instead of
    // growing a fresh one per PR. Set before agent_env so an explicit
    // CARGO_TARGET_DIR there wins.
    if ctx.share_build_cache && wt.join("Cargo.toml").exists() {
        cmd.env("CARGO_TARGET_DIR", repo_root.join("build-cache"));
    }

    // User-configured env, with {repo_dir} expanded — the general hook for
    // build caches in other ecosystems, or anything the repo's tooling needs.
    for (key, value) in &ctx.agent_env {
        cmd.env(key, value.replace("{repo_dir}", &repo_root.to_string_lossy()));
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", ctx.claude_bin))?;

    child
        .stdin
        .take()
        .context("child stdin unavailable")?
        .write_all(prompt.as_bytes())
        .await?;
    // Closing stdin (drop) signals end of prompt.

    // Drain stderr concurrently so the child can't block on a full pipe.
    let mut stderr_pipe = child.stderr.take().context("child stderr unavailable")?;
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        buf
    });

    // Stream events line by line. Every raw line goes to the transcript file
    // (agent-output.jsonl); rendered human-readable steps go to
    // agent-steps.log — both written live, so a daemon task can be watched
    // with `tail -f`. With stream_output on, steps also print to stdout
    // (styled). The final "result" event carries success + summary.
    let stdout_pipe = child.stdout.take().context("child stdout unavailable")?;
    let mut transcript = std::fs::File::create(task_dir.join("agent-output.jsonl"))?;
    let mut steps_log = std::fs::File::create(task_dir.join("agent-steps.log"))?;
    let mut lines = tokio::io::BufReader::new(stdout_pipe).lines();
    let mut ok: Option<bool> = None;
    let mut summary = String::new();
    while let Some(line) = lines.next_line().await.context("reading agent stream")? {
        use std::io::Write as _;
        writeln!(transcript, "{line}")?;
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if event["type"].as_str() == Some("result") {
            ok = Some(!event["is_error"].as_bool().unwrap_or(false));
            summary = truncate(event["result"].as_str().unwrap_or(""), 500);
            writeln!(steps_log, "── result: {}", if ok == Some(true) { "ok" } else { "ERROR" })?;
        } else {
            for step in render_event(&event) {
                writeln!(steps_log, "{}", step.text)?;
                if ctx.stream_output {
                    println!("{}", step.styled());
                }
            }
        }
    }

    let status = child.wait().await.context("waiting for claude")?;
    let stderr_buf = stderr_task.await.unwrap_or_default();
    std::fs::write(task_dir.join("agent-stderr.log"), &stderr_buf)?;

    let ok = ok.unwrap_or_else(|| status.success());
    if summary.is_empty() && !ok {
        summary = "agent exited abnormally — see agent-stderr.log".to_string();
    }
    let has_diff = worktree::write_patch(&wt, &task_dir.join("changes.patch")).await?;

    Ok(TaskResult {
        ok,
        has_diff,
        summary,
        finished_at: chrono::Utc::now(),
    })
}

/// One rendered line of agent activity. `text` is plain (what the step log
/// gets); `styled()` adds terminal colors for live stdout streaming.
struct Step {
    kind: StepKind,
    text: String,
}

enum StepKind {
    Meta,
    Thinking,
    Text,
    ToolUse,
    ToolResult,
}

impl Step {
    fn styled(&self) -> String {
        match self.kind {
            StepKind::Thinking | StepKind::ToolResult | StepKind::Meta => {
                format!("\x1b[2m{}\x1b[0m", self.text) // dim
            }
            StepKind::ToolUse => format!("\x1b[1m{}\x1b[0m", self.text), // bold
            StepKind::Text => self.text.clone(),
        }
    }
}

/// Human-readable rendering of one stream-json event, mirroring the shape of
/// an interactive Claude Code session: thinking, text, tool calls, results.
fn render_event(event: &serde_json::Value) -> Vec<Step> {
    let mut out = Vec::new();
    let mut push = |kind: StepKind, text: String| out.push(Step { kind, text });
    match event["type"].as_str() {
        Some("system") if event["subtype"].as_str() == Some("init") => {
            if let Some(model) = event["model"].as_str() {
                push(StepKind::Meta, format!("· session started ({model})"));
            }
        }
        Some("assistant") => {
            if let Some(blocks) = event["message"]["content"].as_array() {
                for b in blocks {
                    match b["type"].as_str() {
                        Some("thinking") => {
                            for l in b["thinking"].as_str().unwrap_or("").lines() {
                                push(StepKind::Thinking, format!("✻ {l}"));
                            }
                        }
                        Some("text") => {
                            for l in b["text"].as_str().unwrap_or("").lines() {
                                push(StepKind::Text, l.to_string());
                            }
                        }
                        Some("tool_use") => {
                            let name = b["name"].as_str().unwrap_or("?");
                            push(
                                StepKind::ToolUse,
                                format!("⏺ {name}({})", brief_input(&b["input"])),
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
        Some("user") => {
            if let Some(blocks) = event["message"]["content"].as_array() {
                for b in blocks {
                    if b["type"].as_str() == Some("tool_result") {
                        let text = tool_result_text(b);
                        let first = text.lines().next().unwrap_or("");
                        let more = text.lines().count().saturating_sub(1);
                        let mut line = format!("  ⎿ {}", truncate(first, 160));
                        if more > 0 {
                            line.push_str(&format!(" (+{more} lines)"));
                        }
                        push(StepKind::ToolResult, line);
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// The most informative one-liner for a tool call's input.
fn brief_input(input: &serde_json::Value) -> String {
    for key in ["command", "file_path", "path", "pattern", "url"] {
        if let Some(v) = input[key].as_str() {
            return truncate(v, 120);
        }
    }
    truncate(&input.to_string(), 120)
}

/// tool_result content is either a plain string or a list of text blocks.
fn tool_result_text(block: &serde_json::Value) -> String {
    if let Some(s) = block["content"].as_str() {
        return s.to_string();
    }
    if let Some(parts) = block["content"].as_array() {
        return parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

/// Char-boundary-safe truncation with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

fn write_result(task_dir: &Path, result: &TaskResult) -> Result<()> {
    std::fs::create_dir_all(task_dir)?;
    let tmp = task_dir.join("result.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(result)?)?;
    std::fs::rename(&tmp, task_dir.join("result.json"))?;
    Ok(())
}
