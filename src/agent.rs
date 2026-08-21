//! Spawns a headless Claude Code run in the developer's checkout and streams
//! its activity back as rendered steps.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const PROMPT_TEMPLATE: &str = include_str!("../assets/prompt.md");
const SETTINGS: &str = include_str!("../assets/agent-settings.json");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    Meta,
    Thinking,
    Text,
    ToolUse,
    ToolResult,
}

#[derive(Clone, Debug)]
pub struct Step {
    pub kind: StepKind,
    pub text: String,
}

pub enum RunEvent {
    Step(Step),
    Finished { ok: bool, summary: String },
}

pub struct RunCtx {
    pub repo_root: PathBuf,
    pub repo: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub branch: String,
    /// JSON array of the selected comments plus the developer's instructions.
    pub items_json: String,
    pub claude_bin: String,
    pub max_turns: u32,
    pub github_token: String,
    /// .trpr/runs/<timestamp> — prompt, settings, transcript, SUMMARY.md.
    pub run_dir: PathBuf,
}

/// Entry point for the spawned agent task. Always ends by sending Finished.
/// `abort_rx` kills the child (user quit / abort from the TUI).
pub async fn run_agent(
    ctx: RunCtx,
    tx: UnboundedSender<RunEvent>,
    abort_rx: UnboundedReceiver<()>,
) {
    let result = run_inner(&ctx, &tx, abort_rx).await;
    let _ = tx.send(match result {
        Ok((ok, summary)) => RunEvent::Finished { ok, summary },
        Err(e) => RunEvent::Finished {
            ok: false,
            summary: format!("{e:#}"),
        },
    });
}

async fn run_inner(
    ctx: &RunCtx,
    tx: &UnboundedSender<RunEvent>,
    mut abort_rx: UnboundedReceiver<()>,
) -> Result<(bool, String)> {
    std::fs::create_dir_all(&ctx.run_dir)?;
    let settings_path = ctx.run_dir.join("agent-settings.json");
    std::fs::write(&settings_path, SETTINGS)?;
    let summary_path = ctx.run_dir.join("SUMMARY.md");

    let prompt = PROMPT_TEMPLATE
        .replace("{{REPO}}", &ctx.repo)
        .replace("{{PR_NUMBER}}", &ctx.pr_number.to_string())
        .replace("{{PR_TITLE}}", &ctx.pr_title)
        .replace("{{BRANCH}}", &ctx.branch)
        .replace("{{SUMMARY_PATH}}", &summary_path.to_string_lossy())
        .replace("{{ITEMS_JSON}}", &ctx.items_json);
    std::fs::write(ctx.run_dir.join("prompt.md"), &prompt)?;

    let mut child = tokio::process::Command::new(&ctx.claude_bin)
        .args([
            "-p",
            "--output-format",
            "stream-json", // JSONL event stream; --verbose required with it in print mode
            "--verbose",
            "--max-turns",
            &ctx.max_turns.to_string(),
            "--settings",
            &settings_path.to_string_lossy(),
        ])
        .current_dir(&ctx.repo_root)
        // The read-only PAT is the only GitHub credential the agent sees —
        // `gh` inside the run cannot write to GitHub.
        .env("GH_TOKEN", &ctx.github_token)
        .env("GITHUB_TOKEN", &ctx.github_token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

    let stdout_pipe = child.stdout.take().context("child stdout unavailable")?;
    let mut transcript = std::fs::File::create(ctx.run_dir.join("transcript.jsonl"))?;
    let mut steps_log = std::fs::File::create(ctx.run_dir.join("steps.log"))?;
    let mut lines = tokio::io::BufReader::new(stdout_pipe).lines();
    let mut ok: Option<bool> = None;
    let mut summary = String::new();
    let mut aborted = false;

    loop {
        let line = tokio::select! {
            line = lines.next_line() => line.context("reading agent stream")?,
            _ = abort_rx.recv() => {
                aborted = true;
                let _ = child.kill().await;
                break;
            }
        };
        let Some(line) = line else { break };
        use std::io::Write as _;
        writeln!(transcript, "{line}")?;
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if event["type"].as_str() == Some("result") {
            ok = Some(!event["is_error"].as_bool().unwrap_or(false));
            summary = truncate(event["result"].as_str().unwrap_or(""), 2000);
            writeln!(
                steps_log,
                "── result: {}",
                if ok == Some(true) { "ok" } else { "ERROR" }
            )?;
        } else {
            for step in render_event(&event) {
                writeln!(steps_log, "{}", step.text)?;
                let _ = tx.send(RunEvent::Step(step));
            }
        }
    }

    let status = child.wait().await.context("waiting for claude")?;
    let stderr_buf = stderr_task.await.unwrap_or_default();
    std::fs::write(ctx.run_dir.join("stderr.log"), &stderr_buf)?;

    if aborted {
        return Ok((false, "aborted by user".to_string()));
    }
    let ok = ok.unwrap_or_else(|| status.success());
    if summary.is_empty() && !ok {
        summary = "agent exited abnormally — see stderr.log in the run dir".to_string();
    }
    Ok((ok, summary))
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
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}
