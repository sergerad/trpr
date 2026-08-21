use crate::github::Comment;
use crate::{notify, worktree};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
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

    let clone = worktree::ensure_clone(&ctx.inbox_dir, &ctx.repo).await?;
    let wt = worktree::prepare_worktree(&clone, task_dir, ctx.pr_number).await?;
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
        "json",
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

    let output = child.wait_with_output().await.context("waiting for claude")?;
    std::fs::write(task_dir.join("agent-output.json"), &output.stdout)?;
    std::fs::write(task_dir.join("agent-stderr.log"), &output.stderr)?;

    let (ok, summary) = parse_agent_output(&output.stdout, output.status.success());
    let has_diff = worktree::write_patch(&wt, &task_dir.join("changes.patch")).await?;

    Ok(TaskResult {
        ok,
        has_diff,
        summary,
        finished_at: chrono::Utc::now(),
    })
}

/// `--output-format json` yields a single object with `is_error` and a final
/// `result` string. Fall back gracefully if the shape ever changes.
fn parse_agent_output(stdout: &[u8], exit_ok: bool) -> (bool, String) {
    match serde_json::from_slice::<serde_json::Value>(stdout) {
        Ok(v) => {
            let is_error = v["is_error"].as_bool().unwrap_or(!exit_ok);
            let mut summary = v["result"].as_str().unwrap_or("").to_string();
            if summary.len() > 500 {
                summary.truncate(500);
                summary.push('…');
            }
            (!is_error, summary)
        }
        Err(_) => (exit_ok, "agent output was not valid JSON".to_string()),
    }
}

fn write_result(task_dir: &Path, result: &TaskResult) -> Result<()> {
    std::fs::create_dir_all(task_dir)?;
    let tmp = task_dir.join("result.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(result)?)?;
    std::fs::rename(&tmp, task_dir.join("result.json"))?;
    Ok(())
}
