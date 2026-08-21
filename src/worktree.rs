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
    git_with_auth(cwd, args, None).await
}

/// Network-touching git commands authenticate with the read-only PAT when one
/// is given, via git's environment-based config (git 2.31+) — never on the
/// command line (visible in `ps`) and never written to .git/config.
async fn git_with_auth(
    cwd: &Path,
    args: &[&str],
    token: Option<&str>,
) -> Result<std::process::Output> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(cwd);
    if let Some(token) = token {
        let header = format!(
            "Authorization: Basic {}",
            base64(format!("x-access-token:{token}").as_bytes())
        );
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
            .env("GIT_CONFIG_VALUE_0", header);
    }
    let out = cmd
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

/// Minimal base64 (standard alphabet, padded) — only used for the git auth
/// header; not worth a dependency.
fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Clone over https, authenticating with the read-only PAT so private repos
/// work. The token is passed per-invocation via env — nothing credential-like
/// lands in the clone's .git/config.
pub async fn ensure_clone(inbox: &Path, repo: &str, token: &str) -> Result<PathBuf> {
    let clone = clone_dir(inbox, repo);
    if clone.join(".git").exists() {
        return Ok(clone);
    }
    let parent = clone.parent().context("clone dir has no parent")?.to_path_buf();
    std::fs::create_dir_all(&parent)?;
    let url = format!("https://github.com/{repo}.git");
    git_with_auth(&parent, &["clone", &url, "clone"], Some(token)).await?;
    Ok(clone)
}

/// Check out the PR head into a dedicated worktree. `pull/<n>/head` works for
/// both same-repo and fork PRs. If the worktree already exists (new comments on
/// a PR we've handled before), it is left as-is so the agent builds on its own
/// previous, still-unreviewed changes.
pub async fn prepare_worktree(
    clone: &Path,
    task_dir: &Path,
    pr: u64,
    token: &str,
) -> Result<PathBuf> {
    let wt = worktree_dir(task_dir);
    if wt.exists() {
        return Ok(wt);
    }
    std::fs::create_dir_all(task_dir)?;
    let branch = format!("kwkly/pr-{pr}");
    git_with_auth(
        clone,
        &["fetch", "origin", &format!("+pull/{pr}/head:refs/heads/{branch}")],
        Some(token),
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
