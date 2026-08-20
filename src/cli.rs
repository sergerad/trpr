//! Interactive subcommands: the human-facing side of the daemon's inbox.
//!
//! These are read-only with respect to state.json — the daemon owns that file
//! (enforced by its lock). The only thing the CLI writes are per-task marker
//! files (`.reviewed`) and, for `prune`, the removal of finished task dirs.

use crate::config::Config;
use crate::state::{State, TaskStatus};
use crate::worktree;
use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Command;

fn status_str(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Idle => "idle",
        TaskStatus::Pending => "pending",
        TaskStatus::Running => "running",
        TaskStatus::Done => "done",
        TaskStatus::Failed => "FAILED",
    }
}

fn load_state(cfg: &Config) -> Result<State> {
    State::load(&cfg.inbox_dir.join("state.json"))
}

fn reviewed_marker(task_dir: &Path) -> std::path::PathBuf {
    task_dir.join(".reviewed")
}

fn mark_reviewed(task_dir: &Path) {
    let _ = std::fs::write(
        reviewed_marker(task_dir),
        chrono::Utc::now().to_rfc3339(),
    );
}

// ---------------------------------------------------------------- status ---

pub fn status(cfg: &Config) -> Result<()> {
    let st = load_state(cfg)?;
    let mut rows: Vec<(String, u64, TaskStatus, bool, bool, String)> = Vec::new();
    for (repo, prs) in &st.repos {
        for (num, pr) in prs {
            let task_dir = worktree::task_dir(&cfg.inbox_dir, repo, *num);
            rows.push((
                repo.clone(),
                *num,
                pr.status,
                task_dir.join("changes.patch").exists(),
                reviewed_marker(&task_dir).exists(),
                pr.title.clone().unwrap_or_default(),
            ));
        }
    }
    if rows.is_empty() {
        println!("No PRs being tracked yet.");
        return Ok(());
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    println!(
        "{:<32} {:>6}  {:<8} {:<5} {:<9} TITLE",
        "REPO", "PR", "STATUS", "DIFF", "REVIEWED"
    );
    for (repo, num, status, diff, reviewed, title) in rows {
        let mut title = title;
        if title.len() > 50 {
            title.truncate(49);
            title.push('…');
        }
        println!(
            "{:<32} {:>6}  {:<8} {:<5} {:<9} {}",
            repo,
            format!("#{num}"),
            status_str(status),
            if diff { "yes" } else { "-" },
            if reviewed { "yes" } else { "-" },
            title
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- review ---

pub fn review(cfg: &Config) -> Result<()> {
    let st = load_state(cfg)?;
    let mut tasks: Vec<(String, u64, String, TaskStatus)> = Vec::new();
    for (repo, prs) in &st.repos {
        for (num, pr) in prs {
            let task_dir = worktree::task_dir(&cfg.inbox_dir, repo, *num);
            let finished = matches!(pr.status, TaskStatus::Done | TaskStatus::Failed);
            if finished && task_dir.exists() && !reviewed_marker(&task_dir).exists() {
                tasks.push((
                    repo.clone(),
                    *num,
                    pr.title.clone().unwrap_or_default(),
                    pr.status,
                ));
            }
        }
    }
    if tasks.is_empty() {
        println!("Nothing to review. (kwkly status shows everything tracked.)");
        return Ok(());
    }
    tasks.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    println!("{} task(s) to review.\n", tasks.len());

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();

    'tasks: for (repo, num, title, status) in tasks {
        let task_dir = worktree::task_dir(&cfg.inbox_dir, &repo, num);
        let wt = worktree::worktree_dir(&task_dir);

        println!("════════════════════════════════════════════════════════════");
        println!("{repo} PR #{num} [{}] — {title}", status_str(status));
        println!("task dir: {}", task_dir.display());
        println!("════════════════════════════════════════════════════════════");
        show_file(&task_dir.join("PLAN.md"), "PLAN.md");
        if task_dir.join("REPLY-DRAFT.md").exists() {
            println!("(REPLY-DRAFT.md present — drafted replies for you to post)");
        }

        loop {
            print!(
                "\n[{repo}#{num}] (p)lan (d)iff (a)pply (o)pen-claude (x)discard \
                 (e)rrlog (m)ark-reviewed (s)kip (q)uit > "
            );
            std::io::stdout().flush()?;
            let Some(Ok(line)) = lines.next() else {
                break 'tasks; // stdin closed
            };
            match line.trim() {
                "p" => show_file(&task_dir.join("PLAN.md"), "PLAN.md"),
                "d" => {
                    // Inherited stdio => git's own pager works.
                    run_visible("git", &["diff"], &wt)?;
                }
                "a" => {
                    let patch = task_dir.join("changes.patch");
                    if !patch.exists() {
                        println!("No changes.patch for this task.");
                        continue;
                    }
                    print!("Apply into which checkout (path to your real clone)? ");
                    std::io::stdout().flush()?;
                    let Some(Ok(target)) = lines.next() else { break 'tasks };
                    let target = target.trim();
                    if target.is_empty() {
                        continue;
                    }
                    let ok = run_visible(
                        "git",
                        &["apply", "--3way", &patch.to_string_lossy()],
                        Path::new(target),
                    )?;
                    if ok {
                        println!("Applied. Commit it from {target} when ready.");
                        mark_reviewed(&task_dir);
                        continue 'tasks;
                    }
                }
                "o" => {
                    println!("Opening interactive claude in the worktree (exit to return)…");
                    run_visible(&cfg.claude_bin, &[], &wt)?;
                }
                "x" => {
                    run_visible("git", &["reset", "-q"], &wt)?;
                    run_visible("git", &["checkout", "-q", "--", "."], &wt)?;
                    run_visible("git", &["clean", "-qfd"], &wt)?;
                    println!("Worktree changes discarded.");
                    mark_reviewed(&task_dir);
                    continue 'tasks;
                }
                "e" => show_tail(&task_dir.join("agent-stderr.log"), 25),
                "m" => {
                    mark_reviewed(&task_dir);
                    continue 'tasks;
                }
                "s" | "" => continue 'tasks,
                "q" => break 'tasks,
                other => println!("? unrecognized: {other}"),
            }
        }
    }
    println!("Review session ended.");
    Ok(())
}

// ----------------------------------------------------------------- prune ---

/// Remove task dirs whose PR is no longer tracked (merged/closed), including
/// their git worktrees and fetch branches. Asks before deleting anything.
pub fn prune(cfg: &Config) -> Result<()> {
    let st = load_state(cfg)?;
    let mut candidates: Vec<(String, u64, std::path::PathBuf)> = Vec::new();

    for entry in std::fs::read_dir(&cfg.inbox_dir).context("reading inbox dir")? {
        let entry = entry?;
        let repo_dir_name = entry.file_name().to_string_lossy().to_string();
        if !entry.path().is_dir() || !repo_dir_name.contains("__") {
            continue;
        }
        let repo = repo_dir_name.replacen("__", "/", 1);
        for task in std::fs::read_dir(entry.path())? {
            let task = task?;
            let name = task.file_name().to_string_lossy().to_string();
            let Some(num) = name.strip_prefix("pr-").and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            let tracked = st
                .repos
                .get(&repo)
                .map(|prs| prs.contains_key(&num))
                .unwrap_or(false);
            if !tracked {
                candidates.push((repo.clone(), num, task.path()));
            }
        }
    }

    if candidates.is_empty() {
        println!("Nothing to prune.");
        return Ok(());
    }
    println!("PRs no longer open — task dirs to delete:");
    for (repo, num, path) in &candidates {
        println!("  {repo} #{num}  ({})", path.display());
    }
    print!("Delete all of the above, including any unreviewed changes? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim().to_lowercase() != "y" {
        println!("Aborted.");
        return Ok(());
    }

    for (repo, num, task_path) in candidates {
        let clone = worktree::clone_dir(&cfg.inbox_dir, &repo);
        let wt = worktree::worktree_dir(&task_path);
        if clone.exists() {
            // Best-effort: unregister the worktree and drop the fetch branch.
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force", &wt.to_string_lossy()])
                .current_dir(&clone)
                .output();
            let _ = Command::new("git")
                .args(["branch", "-D", &format!("kwkly/pr-{num}")])
                .current_dir(&clone)
                .output();
        }
        std::fs::remove_dir_all(&task_path)
            .with_context(|| format!("removing {}", task_path.display()))?;
        println!("Pruned {repo} #{num}");
    }
    Ok(())
}

// --------------------------------------------------------------- helpers ---

fn show_file(path: &Path, label: &str) {
    match std::fs::read_to_string(path) {
        Ok(text) => println!("--- {label} ---\n{text}"),
        Err(_) => println!("({label} not found)"),
    }
}

fn show_tail(path: &Path, n: usize) {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let all: Vec<&str> = text.lines().collect();
            let start = all.len().saturating_sub(n);
            println!("--- last {} lines of {} ---", all.len() - start, path.display());
            for line in &all[start..] {
                println!("{line}");
            }
        }
        Err(_) => println!("(no {} found)", path.display()),
    }
}

/// Run a command with inherited stdio (visible output, working pagers and
/// interactive TUIs). Returns whether it exited successfully.
fn run_visible(bin: &str, args: &[&str], cwd: &Path) -> Result<bool> {
    let status = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("running {bin} {args:?}"))?;
    if !status.success() {
        println!("({bin} exited with {status})");
    }
    Ok(status.success())
}
