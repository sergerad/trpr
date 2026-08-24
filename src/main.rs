mod agent;
mod git;
mod github;
mod tui;

use anyhow::{Context, Result};
use std::path::PathBuf;

const USAGE: &str = "\
trpr — Triage Pull-Request: address PR review comments with a Claude Code
agent, from inside your checkout

USAGE:
  trpr             PR-list mode: list the repo's open PRs (cwd's repo);
                   selecting one switches your checkout to its branch and
                   opens its comments
  trpr <path>      direct mode: jump straight to the PR of the checkout's
                   current branch (`trpr .` for the cwd)

The agent implements the comments you instruct, committing one commit per
handled comment (with an `Addresses:` trailer). It never pushes and never
posts to GitHub — you review the commits and push yourself.

ENVIRONMENT:
  TRPR_GITHUB_TOKEN   required — fine-grained PAT with READ-ONLY
                      Contents/Issues/Pull-requests on the repo
  TRPR_CLAUDE_BIN     Claude Code binary (default: claude)
  TRPR_MAX_TURNS      agent turn cap per run (default: 60)
  TRPR_DATA_DIR       where run artifacts live (default: ~/.trpr)
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dir, direct) = match args.first().map(|s| s.as_str()) {
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            return Ok(());
        }
        Some(p) => (PathBuf::from(p), true),
        None => (std::env::current_dir().context("getting cwd")?, false),
    };

    let gctx = git::discover(&dir)?;
    if gctx.dirty {
        anyhow::bail!(
            "working tree has uncommitted changes — commit or stash them first \
             (trpr commits per handled comment, so it needs a clean tree to keep \
             its commits attributable)"
        );
    }
    let token = std::env::var("TRPR_GITHUB_TOKEN").context(
        "TRPR_GITHUB_TOKEN not set — export a fine-grained PAT with READ-ONLY \
         Contents/Issues/Pull-requests permissions on this repo",
    )?;
    let gh = github::Gh::new(token.clone())?;
    let actx = tui::AppCtx {
        token,
        claude_bin: std::env::var("TRPR_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string()),
        max_turns: std::env::var("TRPR_MAX_TURNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60),
    };

    if direct {
        direct_mode(gctx, gh, actx).await
    } else {
        list_mode(gctx, gh, actx).await
    }
}

/// `trpr <path>`: straight to the current branch's PR.
async fn direct_mode(gctx: git::GitCtx, gh: github::Gh, actx: tui::AppCtx) -> Result<()> {
    eprintln!("{} on branch {} — looking up PR…", gctx.repo, gctx.branch);
    let pr = gh
        .pr_for_branch(&gctx.repo, &gctx.branch)
        .await?
        .with_context(|| {
            format!(
                "no open PR found for branch '{}' in {} — push the branch and open a PR \
                 first (or run bare `trpr` to pick from all open PRs)",
                gctx.branch, gctx.repo
            )
        })?;
    eprintln!("PR #{}: {} — fetching comments…", pr.number, pr.title);
    tui::mark_seen(&gctx.repo, pr.number);
    let items = gh.unresolved_items(&gctx.repo, pr.number).await?;
    if items.is_empty() {
        println!(
            "No unresolved comments on PR #{} ({}) — nothing to do.",
            pr.number, pr.html_url
        );
        return Ok(());
    }

    let mut app = build_app(
        &gctx,
        gctx.branch.clone(),
        pr.number,
        pr.title.clone(),
        items,
    );
    let mut session = tui::Session::new();
    let result = session.run_comments(&mut app, &actx).await;
    session.close();
    print_run_outcome(&app);
    result.map(|_| ())
}

/// Bare `trpr`: PR list first; selecting a PR switches the checkout to its
/// branch and opens its comments. Esc/Ctrl-o returns to the list, and the
/// left comment view is kept — Ctrl-i (or Tab) on the list resumes it with
/// all in-progress instructions intact, vim-jumplist style.
async fn list_mode(gctx: git::GitCtx, gh: github::Gh, actx: tui::AppCtx) -> Result<()> {
    // Who you are on GitHub — derived from the token, no config needed.
    let viewer = gh.viewer().await.context("fetching your GitHub identity")?;
    eprintln!("authenticated as @{viewer}");
    let mut current_branch = gctx.branch.clone();
    let mut mine_only = true;
    let mut session = tui::Session::new();
    let mut notice: Option<String> = None;
    // The most recently left comment view — the Ctrl-i "forward" slot.
    let mut stored: Option<tui::App> = None;
    let mut last_run_dir: Option<std::path::PathBuf> = None;

    let result = 'outer: loop {
        if let Err(e) = session.show_message("fetching open PRs…") {
            break 'outer Err(e);
        }
        let summaries = match gh.open_pr_summaries(&gctx.repo, &viewer).await {
            Ok(s) => s,
            Err(e) => break 'outer Err(e),
        };
        if summaries.is_empty() {
            break 'outer Ok(());
        }
        let mut view: Vec<github::PrSummary> = if mine_only {
            summaries
                .iter()
                .filter(|s| s.author == viewer)
                .cloned()
                .collect()
        } else {
            summaries.clone()
        };
        if mine_only && view.is_empty() {
            view = summaries.clone();
            if notice.is_none() {
                notice = Some(format!("no open PRs by @{viewer} — showing all"));
            }
        }
        let seen = tui::load_seen(&gctx.repo);

        let outcome = match session
            .pick_pr(
                &gctx.repo,
                &current_branch,
                &view,
                &seen,
                notice.as_deref(),
                stored.is_some(),
                mine_only,
            )
            .await
        {
            Ok(o) => o,
            Err(e) => break 'outer Err(e),
        };
        notice = None;

        let mut app = match outcome {
            tui::PickOutcome::Quit => break 'outer Ok(()),
            tui::PickOutcome::Refresh => continue,
            tui::PickOutcome::ToggleMine => {
                mine_only = !mine_only;
                continue;
            }
            tui::PickOutcome::Forward => match stored.take() {
                // Resume the left view as-is: no refetch, instructions intact.
                Some(a) if a.branch == current_branch => a,
                Some(_) | None => {
                    notice = Some("nothing to resume — open a PR with Enter".into());
                    continue;
                }
            },
            tui::PickOutcome::Pr(picked) => {
                tui::mark_seen(&gctx.repo, picked.number);
                if picked.branch != current_branch {
                    if let Err(e) =
                        session.show_message(&format!("switching to {}…", picked.branch))
                    {
                        break 'outer Err(e);
                    }
                    if let Err(e) = git::switch_branch(&gctx.root, &picked.branch) {
                        notice = Some(format!("branch switch failed: {e:#}"));
                        continue;
                    }
                    current_branch = picked.branch.clone();
                    // Opening a different branch's PR invalidates the
                    // forward slot — its view belongs to the old branch.
                    stored = None;
                }
                if let Err(e) = session.show_message("fetching comments…") {
                    break 'outer Err(e);
                }
                let items = match gh.unresolved_items(&gctx.repo, picked.number).await {
                    Ok(i) => i,
                    Err(e) => break 'outer Err(e),
                };
                if items.is_empty() {
                    notice = Some(format!("PR #{}: no unresolved comments", picked.number));
                    continue;
                }
                let mut app = build_app(
                    &gctx,
                    current_branch.clone(),
                    picked.number,
                    picked.title.clone(),
                    items,
                );
                app.from_list = true;
                app
            }
        };

        let exit = session.run_comments(&mut app, &actx).await;
        if let Some(d) = &app.run_dir {
            last_run_dir = Some(d.clone());
        }
        match exit {
            Ok(tui::CommentExit::BackToList) => {
                if app.run_dir.is_some() {
                    notice = Some("run finished — commits are on the branch".into());
                }
                stored = Some(app);
                continue;
            }
            Ok(tui::CommentExit::Quit) => break 'outer Ok(()),
            Err(e) => break 'outer Err(e),
        }
    };

    session.close();
    print_artifacts(last_run_dir.as_deref(), &gctx.root);
    result
}

fn build_app(
    gctx: &git::GitCtx,
    branch: String,
    pr_number: u64,
    pr_title: String,
    items: Vec<github::CommentItem>,
) -> tui::App {
    // Cross-session awareness: commits with `Addresses:` trailers on the
    // branch mark comments already handled; the ignored file marks ones you
    // decided to skip.
    let handled = git::addressed_commits(&gctx.root);
    let ignored = tui::load_ignored(&gctx.repo, pr_number);
    tui::App::new(
        gctx.repo.clone(),
        gctx.root.clone(),
        branch,
        pr_number,
        pr_title,
        items,
        handled,
        ignored,
    )
}

fn print_run_outcome(app: &tui::App) {
    print_artifacts(app.run_dir.as_deref(), &app.repo_root);
}

fn print_artifacts(run_dir: Option<&std::path::Path>, root: &std::path::Path) {
    if let Some(dir) = run_dir {
        println!("Run artifacts: {}", dir.display());
        println!(
            "Agent commits (if any) are on your branch in {} — review with `git log`, then push yourself.",
            root.display()
        );
    }
}
