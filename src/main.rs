mod agent;
mod git;
mod github;
mod tui;

use anyhow::{Context, Result};
use std::path::PathBuf;

const USAGE: &str = "\
trpr — address PR review comments with a Claude Code agent, in your checkout

USAGE:
  trpr [path]     open the TUI for the git checkout at path (default: cwd)

The checkout's current branch must have an open PR on github.com (origin).
The TUI lists the PR's unresolved comments; you attach instructions to the
ones you want handled and hit go. The agent edits the checkout in place and
never commits, pushes, or posts to GitHub.

ENVIRONMENT:
  TRPR_GITHUB_TOKEN   required — fine-grained PAT with READ-ONLY
                       Contents/Issues/Pull-requests on the repo
  TRPR_CLAUDE_BIN     Claude Code binary (default: claude)
  TRPR_MAX_TURNS      agent turn cap per run (default: 60)
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = match args.first().map(|s| s.as_str()) {
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            return Ok(());
        }
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().context("getting cwd")?,
    };

    let gctx = git::discover(&dir)?;
    let token = std::env::var("TRPR_GITHUB_TOKEN").context(
        "TRPR_GITHUB_TOKEN not set — export a fine-grained PAT with READ-ONLY \
         Contents/Issues/Pull-requests permissions on this repo",
    )?;

    eprintln!(
        "{} on branch {} — looking up PR…",
        gctx.repo, gctx.branch
    );
    let gh = github::Gh::new(token.clone())?;
    let pr = gh
        .pr_for_branch(&gctx.repo, &gctx.branch)
        .await?
        .with_context(|| {
            format!(
                "no open PR found for branch '{}' in {} — push the branch and open a PR first",
                gctx.branch, gctx.repo
            )
        })?;
    eprintln!("PR #{}: {} — fetching comments…", pr.number, pr.title);
    let items = gh.unresolved_items(&gctx.repo, pr.number).await?;
    if items.is_empty() {
        println!(
            "No unresolved comments on PR #{} ({}) — nothing to do.",
            pr.number, pr.html_url
        );
        return Ok(());
    }

    let app = tui::App::new(
        gctx.repo,
        gctx.root,
        gctx.branch,
        pr.number,
        pr.title,
        gctx.dirty,
        items,
    );
    let actx = tui::AppCtx {
        token,
        claude_bin: std::env::var("TRPR_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string()),
        max_turns: std::env::var("TRPR_MAX_TURNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60),
    };
    tui::run(app, actx).await
}
