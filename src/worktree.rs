use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Directory layout under the inbox:
///   <inbox>/<owner>__<name>/clone/            shared clone, one per repo
///   <inbox>/<owner>__<name>/pr-<n>/           task dir (PLAN.md, comments.json, changes.patch, ...)
///   <inbox>/<owner>__<name>/pr-<n>/worktree/  PR branch checkout the agent works in
pub fn repo_root(inbox: &Path, repo: &str) -> PathBuf {
    inbox.join(repo.replace('/', "__"))
}

pub fn clone_dir(inbox: &Path, repo: &str) -> PathBuf {
    repo_root(inbox, repo).join("clone")
}

pub fn task_dir(inbox: &Path, repo: &str, pr: u64) -> PathBuf {
    repo_root(inbox, repo).join(format!("pr-{pr}"))
}

pub fn worktree_dir(task_dir: &Path) -> PathBuf {
    task_dir.join("worktree")
}

async fn git(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("spawning git {args:?}"))?;
    if !out.status.success() {
        bail!(
            "git {:?} in {} failed: {}",
            args,
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(out)
}

/// Anonymous https clone — fine for public open-source repos, and it means the
/// clone itself carries no credentials at all.
pub async fn ensure_clone(inbox: &Path, repo: &str) -> Result<PathBuf> {
    let clone = clone_dir(inbox, repo);
    if clone.join(".git").exists() {
        return Ok(clone);
    }
    let parent = clone.parent().context("clone dir has no parent")?.to_path_buf();
    std::fs::create_dir_all(&parent)?;
    let url = format!("https://github.com/{repo}.git");
    git(&parent, &["clone", &url, "clone"]).await?;
    Ok(clone)
}

/// Check out the PR head into a dedicated worktree. `pull/<n>/head` works for
/// both same-repo and fork PRs. If the worktree already exists (new comments on
/// a PR we've handled before), it is left as-is so the agent builds on its own
/// previous, still-unreviewed changes.
pub async fn prepare_worktree(clone: &Path, task_dir: &Path, pr: u64) -> Result<PathBuf> {
    let wt = worktree_dir(task_dir);
    if wt.exists() {
        return Ok(wt);
    }
    std::fs::create_dir_all(task_dir)?;
    let branch = format!("kwkly/pr-{pr}");
    git(
        clone,
        &["fetch", "origin", &format!("+pull/{pr}/head:refs/heads/{branch}")],
    )
    .await?;
    let wt_str = wt.to_string_lossy().to_string();
    git(clone, &["worktree", "add", &wt_str, &branch]).await?;
    Ok(wt)
}

/// Snapshot the agent's uncommitted work as a reviewable patch.
/// `add -N` makes newly created files show up in `git diff`.
/// Returns false when the agent made no file changes (e.g. reply-draft only).
pub async fn write_patch(wt: &Path, patch_path: &Path) -> Result<bool> {
    git(wt, &["add", "-N", "."]).await?;
    let out = git(wt, &["diff"]).await?;
    if out.stdout.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(false);
    }
    std::fs::write(patch_path, &out.stdout).context("writing changes.patch")?;
    Ok(true)
}
