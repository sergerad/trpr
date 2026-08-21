use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Your GitHub login — used to skip your own comments and (optionally) filter PRs to yours.
    pub username: String,
    /// Repositories to watch, as "owner/name".
    pub repos: Vec<String>,
    /// Where clones, worktrees, and task artifacts live.
    #[serde(default = "d_inbox")]
    pub inbox_dir: PathBuf,
    #[serde(default = "d_poll")]
    pub poll_interval_secs: u64,
    /// Quiet window after the last new comment before a task is dispatched,
    /// so a burst of review comments becomes one agent run.
    #[serde(default = "d_debounce")]
    pub debounce_secs: u64,
    #[serde(default = "d_concurrency")]
    pub max_concurrent_agents: usize,
    /// --max-turns passed to each headless Claude Code run.
    #[serde(default = "d_turns")]
    pub max_turns: u32,
    #[serde(default = "d_claude")]
    pub claude_bin: String,
    /// Only react to PRs you authored. Set false to watch every open PR in the repo.
    #[serde(default = "d_true")]
    pub only_my_prs: bool,
    /// Name of the env var holding the READ-ONLY fine-grained PAT.
    #[serde(default = "d_token_env")]
    pub github_token_env: String,
    /// macOS desktop notifications when a task finishes.
    #[serde(default = "d_true")]
    pub notifications: bool,
    /// Extra env vars for each agent run. "{repo_dir}" in a value expands to
    /// the repo's directory under inbox_dir — e.g. share one cargo target dir
    /// across all of a repo's worktrees:
    ///   [agent_env]
    ///   CARGO_TARGET_DIR = "{repo_dir}/build-cache"
    #[serde(default)]
    pub agent_env: std::collections::HashMap<String, String>,
}

fn d_inbox() -> PathBuf {
    PathBuf::from("~/agent-inbox")
}
fn d_poll() -> u64 {
    120
}
fn d_debounce() -> u64 {
    300
}
fn d_concurrency() -> usize {
    2
}
fn d_turns() -> u32 {
    40
}
fn d_claude() -> String {
    "claude".to_string()
}
fn d_true() -> bool {
    true
}
fn d_token_env() -> String {
    "KWKLY_GITHUB_TOKEN".to_string()
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {path} (copy config.example.toml to config.toml to get started)"))?;
        let mut cfg: Config = toml::from_str(&raw).context("parsing config.toml")?;
        cfg.inbox_dir = expand_tilde(&cfg.inbox_dir);
        Ok(cfg)
    }

    pub fn github_token(&self) -> Result<String> {
        std::env::var(&self.github_token_env).with_context(|| {
            format!(
                "env var {} not set — export a fine-grained PAT with READ-ONLY \
                 Contents/Issues/Pull-requests permissions on the watched repos",
                self.github_token_env
            )
        })
    }
}

fn expand_tilde(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.to_path_buf()
}
