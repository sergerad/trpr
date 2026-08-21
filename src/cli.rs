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

// ------------------------------------------------------------------- run ---

/// What a `kwkly run` URL resolves to.
pub struct RunTarget {
    pub repo: String,
    pub pr: u64,
    /// A specific comment (from a #discussion_r / #issuecomment anchor), with
    /// the comment kind the anchor implies.
    pub comment: Option<CommentRef>,
}

pub struct CommentRef {
    pub id: u64,
    pub source: &'static str, // "review_comment" | "issue_comment"
}

/// Parse a GitHub PR or PR-comment URL:
///   https://github.com/OWNER/REPO/pull/N                      → whole PR
///   https://github.com/OWNER/REPO/pull/N#discussion_r<ID>     → inline review comment
///   https://github.com/OWNER/REPO/pull/N/files#r<ID>          → same, files-tab anchor
///   https://github.com/OWNER/REPO/pull/N#issuecomment-<ID>    → conversation comment
pub fn parse_run_target(input: &str) -> Result<RunTarget> {
    let s = input.trim();
    let s = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(s);
    let s = s.strip_prefix("www.").unwrap_or(s);
    let rest = s
        .strip_prefix("github.com/")
        .context("expected a github.com URL like https://github.com/owner/repo/pull/123")?;

    let (path, fragment) = match rest.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (rest, None),
    };
    let mut segs = path.trim_end_matches('/').split('/');
    let owner = segs.next().filter(|v| !v.is_empty()).context("missing owner in URL")?;
    let name = segs.next().filter(|v| !v.is_empty()).context("missing repo in URL")?;
    match segs.next() {
        Some("pull") => {}
        Some("issues") => anyhow::bail!(
            "that's an issue URL — kwkly handles pull requests only (for now)"
        ),
        _ => anyhow::bail!("expected .../pull/<number> in the URL"),
    }
    let pr: u64 = segs
        .next()
        .context("missing PR number in URL")?
        .parse()
        .context("PR number in URL is not a number")?;
    // Trailing path segments (/files, /commits, …) are irrelevant — ignore.

    let comment = match fragment {
        None | Some("") => None,
        Some(f) => {
            if let Some(id) = f.strip_prefix("discussion_r").and_then(|v| v.parse().ok()) {
                Some(CommentRef { id, source: "review_comment" })
            } else if let Some(id) = f.strip_prefix("issuecomment-").and_then(|v| v.parse().ok()) {
                Some(CommentRef { id, source: "issue_comment" })
            } else if let Some(id) = f.strip_prefix('r').and_then(|v| v.parse().ok()) {
                Some(CommentRef { id, source: "review_comment" })
            } else if f.starts_with("pullrequestreview-") {
                anyhow::bail!(
                    "that anchor is a review summary, which kwkly doesn't handle yet — \
                     link one of the review's inline comments (#discussion_r…) instead"
                );
            } else {
                anyhow::bail!("unrecognized comment anchor '#{f}'");
            }
        }
    };

    Ok(RunTarget { repo: format!("{owner}/{name}"), pr, comment })
}

/// Trigger one agent run against a real PR immediately, bypassing polling and
/// debounce. With a comment anchor: exactly that comment, no filtering.
/// Without: all outstanding comments — everything the daemon hasn't already
/// seen or queued (all of the PR's comments if it isn't tracked), bots
/// excluded but own comments included, so commenting on your own PR works.
/// Dispatches in the foreground and reports the artifact paths. Never writes
/// state.json.
pub async fn run_once(cfg: &Config, target: RunTarget) -> Result<()> {
    let RunTarget { repo, pr: pr_number, comment } = target;
    let repo = repo.as_str();
    let token = cfg.github_token()?;
    let gh = crate::github::Gh::new(token.clone())?;

    let owner = repo.split('/').next().unwrap_or(repo);
    let pr = gh.pr(repo, pr_number).await.with_context(|| {
        format!(
            "fetching PR #{pr_number} from {repo}. A GitHub 404 here usually means one of:\n\
             - the token can't see the repo (private repo: the fine-grained PAT must have \
             '{owner}' as its resource owner, with this repo granted — and the org must \
             allow/approve fine-grained PATs)\n\
             - #{pr_number} is an issue, not a pull request (kwkly handles PR comments only)"
        )
    })?;
    let comments = gh
        .new_comments(repo, pr_number, chrono::DateTime::<chrono::Utc>::UNIX_EPOCH)
        .await?;
    if comments.is_empty() {
        anyhow::bail!("PR #{pr_number} has no comments to run against");
    }

    let selected = match comment {
        Some(cref) => {
            let Some(c) = comments
                .iter()
                .find(|c| c.id == cref.id && c.source == cref.source)
            else {
                println!("Comment {} not found on PR #{pr_number}. Comments there:", cref.id);
                for c in &comments {
                    let mut body = c.body.replace('\n', " ");
                    if body.len() > 70 {
                        body.truncate(69);
                        body.push('…');
                    }
                    println!("  {}\n    @{}: {}", c.html_url, c.user.login, body);
                }
                anyhow::bail!("pass one of the comment URLs above");
            };
            vec![c.clone()]
        }
        None => {
            // "Outstanding" = the daemon's queued-but-undispatched comments,
            // plus anything newer than its high-water mark. For an untracked
            // PR that's simply every comment. State is read-only here.
            let st = load_state(cfg)?;
            let entry = st.repos.get(repo).and_then(|prs| prs.get(&pr_number));
            let cutoff = entry.and_then(|e| e.last_seen_comment_at);
            let mut sel: Vec<crate::github::Comment> =
                entry.map(|e| e.pending_comments.clone()).unwrap_or_default();
            let queued: std::collections::HashSet<u64> = sel.iter().map(|c| c.id).collect();
            for c in &comments {
                let is_bot = c.user.login.ends_with("[bot]")
                    || c.user.kind.as_deref() == Some("Bot");
                let outstanding = cutoff.map(|t| c.created_at > t).unwrap_or(true);
                if outstanding && !is_bot && !queued.contains(&c.id) {
                    sel.push(c.clone());
                }
            }
            sel.sort_by_key(|c| c.created_at);
            if sel.is_empty() {
                anyhow::bail!(
                    "no outstanding comments on PR #{pr_number} — the daemon has already \
                     seen them all; pass a comment id to re-run against a specific one"
                );
            }
            sel
        }
    };

    println!("PR #{pr_number}: {}", pr.title);
    for c in &selected {
        let mut body = c.body.replace('\n', " ");
        if body.len() > 100 {
            body.truncate(99);
            body.push('…');
        }
        println!("Testing against [{}] @{}: {}", c.source, c.user.login, body);
    }

    // Warn if a daemon holds this inbox — a manual run on a PR the daemon is
    // actively working could interleave with a live task.
    std::fs::create_dir_all(&cfg.inbox_dir)?;
    let lock_probe = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(cfg.inbox_dir.join("daemon.lock"))?;
    if matches!(lock_probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
        println!(
            "note: a kwkly daemon is running against this inbox — if it has an \
             active task for this PR, the two runs may interfere"
        );
    }
    drop(lock_probe);

    let ctx = crate::agent::TaskCtx {
        repo: repo.to_string(),
        pr_number,
        pr_title: pr.title.clone(),
        inbox_dir: cfg.inbox_dir.clone(),
        claude_bin: cfg.claude_bin.clone(),
        max_turns: cfg.max_turns,
        github_token: token,
        notifications: cfg.notifications,
        share_build_cache: cfg.share_build_cache,
        agent_env: cfg.agent_env.clone(),
        comments: selected,
    };

    println!("Running agent (this can take a few minutes)…\n");
    crate::agent::run_task(ctx).await;

    // Consume result.json: the daemon must never reconcile a result it
    // didn't dispatch (it could otherwise mark a future live run Done early).
    let task_dir = worktree::task_dir(&cfg.inbox_dir, repo, pr_number);
    let result_path = task_dir.join("result.json");
    match std::fs::read_to_string(&result_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<crate::agent::TaskResult>(&raw).ok())
    {
        Some(r) => {
            std::fs::remove_file(&result_path).ok();
            println!("\n=== run {} ===", if r.ok { "succeeded" } else { "FAILED" });
            println!("diff produced: {}", if r.has_diff { "yes" } else { "no" });
            if !r.summary.is_empty() {
                println!("agent summary: {}", r.summary);
            }
        }
        None => println!("\n=== run finished but wrote no readable result ==="),
    }
    println!("\nArtifacts:");
    println!("  plan:     {}", task_dir.join("PLAN.md").display());
    println!("  worktree: {}", worktree::worktree_dir(&task_dir).display());
    println!("  patch:    {}", task_dir.join("changes.patch").display());
    println!("  log:      {}", task_dir.join("agent-stderr.log").display());
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

#[cfg(test)]
mod tests {
    use super::parse_run_target;

    #[test]
    fn parses_pr_and_comment_urls() {
        let t = parse_run_target("https://github.com/some-org/some-repo/pull/176").unwrap();
        assert_eq!((t.repo.as_str(), t.pr), ("some-org/some-repo", 176));
        assert!(t.comment.is_none());

        let t = parse_run_target(
            "https://github.com/some-org/some-repo/pull/176#discussion_r1234567890",
        )
        .unwrap();
        let c = t.comment.unwrap();
        assert_eq!((c.id, c.source), (1234567890, "review_comment"));

        let t = parse_run_target("github.com/a/b/pull/9#issuecomment-77").unwrap();
        let c = t.comment.unwrap();
        assert_eq!((t.pr, c.id, c.source), (9, 77, "issue_comment"));

        let t = parse_run_target("https://github.com/a/b/pull/9/files#r123").unwrap();
        assert_eq!(t.comment.unwrap().source, "review_comment");

        // Trailing path segments and slashes are tolerated
        assert!(parse_run_target("https://github.com/a/b/pull/9/commits").is_ok());
        assert!(parse_run_target("https://github.com/a/b/pull/9/").is_ok());
    }

    #[test]
    fn rejects_unsupported_urls() {
        assert!(parse_run_target("https://github.com/a/b/issues/5").is_err());
        assert!(parse_run_target("https://github.com/a/b").is_err());
        assert!(parse_run_target("https://gitlab.com/a/b/pull/5").is_err());
        assert!(parse_run_target("https://github.com/a/b/pull/notanumber").is_err());
        assert!(parse_run_target("https://github.com/a/b/pull/5#pullrequestreview-1").is_err());
        assert!(parse_run_target("https://github.com/a/b/pull/5#weird-anchor").is_err());
    }
}
