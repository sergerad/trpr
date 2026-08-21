mod agent;
mod cli;
mod config;
mod github;
mod notify;
mod state;
mod worktree;

use anyhow::{bail, Result};
use chrono::Utc;
use state::TaskStatus;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

const USAGE: &str = "\
kwkly — watches your PRs for review comments, prepares diffs locally

USAGE:
  kwkly [config.toml]            run the daemon (default config: ./config.toml)
  kwkly daemon [config.toml]     same, explicit
  kwkly status [config.toml]     list tracked PRs and finished tasks
  kwkly review [config.toml]     walk unreviewed tasks interactively
  kwkly prune  [config.toml]     delete task dirs for merged/closed PRs

  kwkly run <github-pr-or-comment-url> [config.toml]
      trigger one agent run right now, bypassing polling and debounce.
      A PR URL handles all outstanding comments; a comment permalink
      (…#discussion_r… or …#issuecomment-…) handles exactly that comment
";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kwkly=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mode, cfg_arg) = match args.first().map(|s| s.as_str()) {
        Some("-h" | "--help" | "help") => {
            print!("{USAGE}");
            return Ok(());
        }
        Some("run") => {
            let Some(url) = args.get(1) else {
                bail!("usage: kwkly run <github-pr-or-comment-url> [config.toml]");
            };
            let target = cli::parse_run_target(url)?;
            let cfg_path = args.get(2).map(|s| s.as_str()).unwrap_or("config.toml");
            let cfg = config::Config::load(cfg_path)?;
            return cli::run_once(&cfg, target).await;
        }
        Some(m @ ("daemon" | "status" | "review" | "prune")) => (m, args.get(1)),
        // Legacy form: bare positional arg is a config path for the daemon.
        Some(_) => ("daemon", args.first()),
        None => ("daemon", None),
    };
    let cfg_path = cfg_arg.map(|s| s.as_str()).unwrap_or("config.toml");
    let cfg = config::Config::load(cfg_path)?;

    match mode {
        "status" => return cli::status(&cfg),
        "review" => return cli::review(&cfg),
        "prune" => return cli::prune(&cfg),
        _ => {}
    }

    // ---- daemon path ----
    let token = cfg.github_token()?;
    std::fs::create_dir_all(&cfg.inbox_dir)?;

    // One daemon per inbox_dir: state.json has a single writer. The OS lock
    // is released automatically on any kind of process exit.
    let lock_path = cfg.inbox_dir.join("daemon.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)?;
    match lock_file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => bail!(
            "another kwkly daemon is already running against {} — \
             one daemon per inbox_dir (use a separate inbox_dir per instance)",
            cfg.inbox_dir.display()
        ),
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    }

    let gh = github::Gh::new(token.clone())?;
    let state_path = cfg.inbox_dir.join("state.json");
    let mut st = state::State::load(&state_path)?;
    st.recover_running();

    let permits = Arc::new(Semaphore::new(cfg.max_concurrent_agents));
    info!(
        "kwkly started — watching {} repo(s), polling every {}s, inbox at {}",
        cfg.repos.len(),
        cfg.poll_interval_secs,
        cfg.inbox_dir.display()
    );

    loop {
        if let Err(e) = tick(&cfg, &gh, &token, &mut st, &permits).await {
            error!("tick failed: {e:#}");
        }
        if let Err(e) = st.save(&state_path) {
            error!("saving state failed: {e:#}");
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(cfg.poll_interval_secs)) => {}
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                break;
            }
        }
    }
    st.save(&state_path)?;
    drop(lock_file);
    Ok(())
}

async fn tick(
    cfg: &config::Config,
    gh: &github::Gh,
    token: &str,
    st: &mut state::State,
    permits: &Arc<Semaphore>,
) -> Result<()> {
    reconcile_finished(cfg, st);
    poll_repos(cfg, gh, st).await;
    dispatch_ready(cfg, token, st, permits);
    Ok(())
}

/// Move Running tasks whose agent has finished (result.json present) to
/// Done/Failed. If more comments arrived mid-run, flip straight back to
/// Pending so a follow-up run picks them up.
fn reconcile_finished(cfg: &config::Config, st: &mut state::State) {
    for (repo, prs) in st.repos.iter_mut() {
        for (number, pr) in prs.iter_mut() {
            if pr.status != TaskStatus::Running {
                continue;
            }
            let task_dir = worktree::task_dir(&cfg.inbox_dir, repo, *number);
            let result_path = task_dir.join("result.json");
            if !result_path.exists() {
                continue;
            }
            let result: Option<agent::TaskResult> = std::fs::read_to_string(&result_path)
                .ok()
                .and_then(|raw| serde_json::from_str(&raw).ok());
            std::fs::remove_file(&result_path).ok();
            match result {
                Some(r) if r.ok => {
                    pr.status = TaskStatus::Done;
                    info!("{repo} PR #{number}: done (diff: {}) — {}", r.has_diff, task_dir.display());
                }
                Some(r) => {
                    pr.status = TaskStatus::Failed;
                    warn!("{repo} PR #{number}: failed — {}", r.summary);
                }
                None => {
                    pr.status = TaskStatus::Failed;
                    warn!("{repo} PR #{number}: unreadable result.json");
                }
            }
            if !pr.pending_comments.is_empty() {
                pr.status = TaskStatus::Pending;
                pr.pending_since = Some(Utc::now());
                info!("{repo} PR #{number}: more comments queued during run, re-pending");
            }
        }
    }
}

/// Fetch open PRs and any comments newer than the per-PR high-water mark.
/// Per-repo failures are logged and skipped so one flaky repo can't stall
/// the others.
async fn poll_repos(cfg: &config::Config, gh: &github::Gh, st: &mut state::State) {
    for repo in &cfg.repos {
        let prs = match gh.open_prs(repo).await {
            Ok(p) => p,
            Err(e) => {
                warn!("{repo}: listing PRs failed: {e:#}");
                continue;
            }
        };
        let mut open: HashSet<u64> = HashSet::new();

        for pr in &prs {
            if cfg.only_my_prs && pr.user.login != cfg.username {
                continue;
            }
            open.insert(pr.number);
            let entry = st.pr_mut(repo, pr.number);
            entry.title = Some(pr.title.clone());
            entry.head_branch = Some(pr.head.branch.clone());

            // First sighting: baseline to "now" instead of replaying the PR's
            // entire comment history.
            let Some(since) = entry.last_seen_comment_at else {
                entry.last_seen_comment_at = Some(Utc::now());
                info!("{repo} PR #{}: now watching ({})", pr.number, pr.title);
                continue;
            };

            let mut new = match gh.new_comments(repo, pr.number, since).await {
                Ok(c) => c,
                Err(e) => {
                    warn!("{repo} PR #{}: fetching comments failed: {e:#}", pr.number);
                    continue;
                }
            };
            new.retain(|c| !github::is_noise(c, &cfg.username));
            if new.is_empty() {
                continue;
            }

            let newest = new.iter().map(|c| c.created_at).max().unwrap_or(since);
            entry.last_seen_comment_at = Some(newest.max(since));
            entry.last_activity_at = Some(Utc::now());
            info!("{repo} PR #{}: {} new comment(s)", pr.number, new.len());
            entry.pending_comments.extend(new);
            if entry.status != TaskStatus::Running {
                entry.status = TaskStatus::Pending;
                if entry.pending_since.is_none() {
                    entry.pending_since = Some(Utc::now());
                }
            }
        }

        st.retain_open(repo, &open);
    }
}

/// Dispatch Pending tasks whose debounce window has elapsed. Each task runs
/// in its own tokio task, gated by the concurrency semaphore, and reports
/// back via result.json — the spawned task never touches shared state.
fn dispatch_ready(
    cfg: &config::Config,
    token: &str,
    st: &mut state::State,
    permits: &Arc<Semaphore>,
) {
    let now = Utc::now();
    for (repo, prs) in st.repos.iter_mut() {
        for (number, pr) in prs.iter_mut() {
            if pr.status != TaskStatus::Pending {
                continue;
            }
            let quiet = pr
                .last_activity_at
                .map(|t| now - t >= chrono::Duration::seconds(cfg.debounce_secs as i64))
                .unwrap_or(true);
            if !quiet {
                continue;
            }

            let ctx = agent::TaskCtx {
                repo: repo.clone(),
                pr_number: *number,
                pr_title: pr.title.clone().unwrap_or_default(),
                inbox_dir: cfg.inbox_dir.clone(),
                claude_bin: cfg.claude_bin.clone(),
                max_turns: cfg.max_turns,
                github_token: token.to_string(),
                notifications: cfg.notifications,
                share_build_cache: cfg.share_build_cache,
                agent_env: cfg.agent_env.clone(),
                comments: std::mem::take(&mut pr.pending_comments),
            };
            pr.status = TaskStatus::Running;
            pr.pending_since = None;
            info!("{repo} PR #{number}: dispatching agent task");

            let permits = Arc::clone(permits);
            tokio::spawn(async move {
                let _permit = permits.acquire_owned().await.expect("semaphore closed");
                agent::run_task(ctx).await;
            });
        }
    }
}
