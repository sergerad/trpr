use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

const API: &str = "https://api.github.com";

pub struct Gh {
    http: reqwest::Client,
    token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub login: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrHead {
    #[serde(rename = "ref")]
    pub branch: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pr {
    pub number: u64,
    pub title: String,
    pub user: User,
    pub head: PrHead,
}

/// One PR comment, from either the issue-comment or review-comment API.
/// Serialized into comments.json and embedded in the agent prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub user: User,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub html_url: String,
    /// Review comments only: the file the comment is anchored to.
    #[serde(default)]
    pub path: Option<String>,
    /// Review comments only: the diff context the reviewer saw.
    #[serde(default)]
    pub diff_hunk: Option<String>,
    /// "issue_comment" (PR conversation tab) or "review_comment" (inline on the diff).
    #[serde(default)]
    pub source: String,
}

impl Gh {
    pub fn new(token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("kwkly")
            .build()
            .context("building http client")?;
        Ok(Self { http, token })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("GET {url} -> {status}: {body}");
        }
        Ok(resp.json::<T>().await.with_context(|| format!("decoding {url}"))?)
    }

    pub async fn open_prs(&self, repo: &str) -> Result<Vec<Pr>> {
        let url = format!("{API}/repos/{repo}/pulls?state=open&per_page=100");
        let prs: Vec<Pr> = self.get_json(&url).await?;
        if prs.len() == 100 {
            warn!("{repo}: 100+ open PRs; pagination not implemented, later pages ignored");
        }
        Ok(prs)
    }

    /// All comments on a PR created after `since` — conversation comments and
    /// inline review comments merged, oldest first.
    pub async fn new_comments(
        &self,
        repo: &str,
        pr: u64,
        since: DateTime<Utc>,
    ) -> Result<Vec<Comment>> {
        let since_q = since.to_rfc3339();
        let issue_url = format!(
            "{API}/repos/{repo}/issues/{pr}/comments?per_page=100&since={since_q}"
        );
        let review_url = format!(
            "{API}/repos/{repo}/pulls/{pr}/comments?per_page=100&since={since_q}"
        );

        let mut issue: Vec<Comment> = self.get_json(&issue_url).await?;
        for c in &mut issue {
            c.source = "issue_comment".to_string();
        }
        let mut review: Vec<Comment> = self.get_json(&review_url).await?;
        for c in &mut review {
            c.source = "review_comment".to_string();
        }

        let mut all = issue;
        all.extend(review);
        // `since` filters on updated_at (>=); keep strictly-newer creations only,
        // so edits to old comments and boundary duplicates are dropped.
        all.retain(|c| c.created_at > since);
        all.sort_by_key(|c| c.created_at);
        Ok(all)
    }
}

/// Comments we never act on: our own, and bot chatter (CI, coverage, etc.).
pub fn is_noise(c: &Comment, own_username: &str) -> bool {
    c.user.login == own_username
        || c.user.login.ends_with("[bot]")
        || c.user.kind.as_deref() == Some("Bot")
}
