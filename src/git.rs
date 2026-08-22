//! Local git introspection: which repo is this checkout, what branch is it
//! on, is the working tree dirty. All read-only.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct GitCtx {
    pub root: PathBuf,
    pub branch: String,
    /// "owner/name", parsed from the origin remote.
    pub repo: String,
    pub dirty: bool,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {args:?}"))?;
    if !out.status.success() {
        bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn discover(dir: &Path) -> Result<GitCtx> {
    let root = PathBuf::from(
        git(dir, &["rev-parse", "--show-toplevel"])
            .with_context(|| format!("{} is not inside a git repository", dir.display()))?,
    );
    let branch = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "HEAD" {
        bail!("detached HEAD — check out the PR branch first");
    }
    let url = git(&root, &["remote", "get-url", "origin"])
        .context("no 'origin' remote — trpr needs one pointing at github.com")?;
    let repo = parse_github_remote(&url)
        .with_context(|| format!("origin remote '{url}' is not a github.com repo"))?;
    let dirty = !git(&root, &["status", "--porcelain"])?.is_empty();
    Ok(GitCtx {
        root,
        branch,
        repo,
        dirty,
    })
}

/// Scan branch history for `Addresses: <url>` trailers left by earlier trpr
/// runs. Returns comment-url → (short sha, commit epoch) for the *newest*
/// commit addressing each comment. Tolerant: any git failure yields an empty
/// map (badges are hints, not correctness).
pub fn addressed_commits(root: &Path) -> std::collections::HashMap<String, (String, i64)> {
    let mut out = std::collections::HashMap::new();
    let Ok(log) = git(root, &["log", "--format=%h %ct%n%B%x1e", "-n", "1000"]) else {
        return out;
    };
    for record in log.split('\u{1e}') {
        let mut lines = record.trim().lines();
        let Some(head) = lines.next() else { continue };
        let mut parts = head.split_whitespace();
        let (Some(sha), Some(epoch)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(epoch) = epoch.parse::<i64>() else {
            continue;
        };
        for line in lines {
            if let Some(url) = line.trim().strip_prefix("Addresses:") {
                // git log is newest-first; keep the first (newest) sighting.
                out.entry(url.trim().to_string())
                    .or_insert_with(|| (sha.to_string(), epoch));
            }
        }
    }
    out
}

pub fn parse_github_remote(url: &str) -> Option<String> {
    let u = url.trim();
    let rest = u
        .strip_prefix("git@github.com:")
        .or_else(|| u.strip_prefix("ssh://git@github.com/"))
        .or_else(|| u.strip_prefix("https://github.com/"))
        .or_else(|| u.strip_prefix("http://github.com/"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut segs = rest.split('/').filter(|s| !s.is_empty());
    let owner = segs.next()?;
    let name = segs.next()?;
    Some(format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::parse_github_remote;

    #[test]
    fn parses_remote_urls() {
        for url in [
            "git@github.com:some-org/some-repo.git",
            "ssh://git@github.com/some-org/some-repo.git",
            "https://github.com/some-org/some-repo.git",
            "https://github.com/some-org/some-repo",
        ] {
            assert_eq!(
                parse_github_remote(url).as_deref(),
                Some("some-org/some-repo"),
                "failed for {url}"
            );
        }
        assert_eq!(parse_github_remote("https://gitlab.com/a/b.git"), None);
    }
}
