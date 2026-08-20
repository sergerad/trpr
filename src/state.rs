use crate::github::Comment;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TaskStatus {
    #[default]
    Idle,
    /// New comments seen; waiting out the debounce window.
    Pending,
    /// An agent run has been dispatched; waiting for result.json.
    Running,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrState {
    pub title: Option<String>,
    pub head_branch: Option<String>,
    pub status: TaskStatus,
    /// High-water mark: comments created at or before this are already handled.
    pub last_seen_comment_at: Option<DateTime<Utc>>,
    /// When the current Pending batch started accumulating.
    pub pending_since: Option<DateTime<Utc>>,
    /// Last time a new comment arrived — the debounce clock.
    pub last_activity_at: Option<DateTime<Utc>>,
    /// Comments accumulated for the next agent run. Drained at dispatch.
    #[serde(default)]
    pub pending_comments: Vec<Comment>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub repos: HashMap<String, HashMap<u64, PrState>>,
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path).context("reading state.json")?;
        Ok(serde_json::from_str(&raw).context("parsing state.json")?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path).context("committing state.json")?;
        Ok(())
    }

    pub fn pr_mut(&mut self, repo: &str, number: u64) -> &mut PrState {
        self.repos
            .entry(repo.to_string())
            .or_default()
            .entry(number)
            .or_default()
    }

    /// After a crash/restart, tasks marked Running never wrote result.json.
    /// Re-dispatch them: the comments they were given still exist on disk in
    /// comments.json, which the dispatcher falls back to.
    pub fn recover_running(&mut self) {
        for prs in self.repos.values_mut() {
            for st in prs.values_mut() {
                if st.status == TaskStatus::Running {
                    st.status = TaskStatus::Pending;
                    st.last_activity_at = None; // dispatch immediately, no debounce
                }
            }
        }
    }

    /// Drop entries for PRs that are no longer open (merged/closed).
    /// Running entries survive one cycle so their result gets reconciled.
    pub fn retain_open(&mut self, repo: &str, open: &HashSet<u64>) {
        if let Some(prs) = self.repos.get_mut(repo) {
            prs.retain(|n, st| open.contains(n) || st.status == TaskStatus::Running);
        }
    }
}
