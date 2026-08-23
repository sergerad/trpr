//! GitHub API client: find the PR for a branch, fetch its unresolved
//! comments. Read-only; authenticated with the read-only PAT.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

const API: &str = "https://api.github.com";

pub struct Gh {
    http: reqwest::Client,
    token: String,
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
    pub head: PrHead,
    pub html_url: String,
}

/// One row of the PR-list screen.
#[derive(Debug, Clone)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub branch: String,
    pub author: String,
    /// First assignee, when set.
    pub assignee: Option<String>,
    /// Unresolved review threads + conversation comments.
    pub unresolved: usize,
    /// Newest comment activity (epoch seconds); 0 when none/unknown.
    pub last_activity: i64,
}

impl PrSummary {
    /// Who's responsible for addressing this PR's reviews: the assignee, or
    /// the author when unassigned. The list groups and sorts by this.
    pub fn triage_owner(&self) -> &str {
        self.assignee.as_deref().unwrap_or(&self.author)
    }
}

/// One reviewable item shown in the TUI: an unresolved inline review thread,
/// or a conversation-tab comment.
#[derive(Debug, Clone)]
pub struct CommentItem {
    pub kind: ItemKind,
    pub author: String,
    /// The thread's first (or the conversation comment's only) body.
    pub body: String,
    /// Follow-up comments in the thread: (author, body).
    pub replies: Vec<(String, String)>,
    pub diff_hunk: Option<String>,
    /// The first comment's permalink — the stable key used in `Addresses:`
    /// commit trailers and the ignored-comments file.
    pub url: String,
    /// Newest activity in the thread (epoch seconds); 0 when unknown.
    pub last_activity: i64,
}

#[derive(Debug, Clone)]
pub enum ItemKind {
    Thread {
        path: String,
        line: Option<u64>,
        outdated: bool,
    },
    Conversation,
}

impl CommentItem {
    /// Short list label, e.g. "src/db/mod.rs:385 @mirko" or "@mirko (conversation)".
    pub fn label(&self) -> String {
        match &self.kind {
            ItemKind::Thread {
                path,
                line,
                outdated,
            } => format!(
                "{path}{} @{}{}",
                line.map(|l| format!(":{l}")).unwrap_or_default(),
                self.author,
                if *outdated { " (outdated)" } else { "" }
            ),
            ItemKind::Conversation => format!("@{} (conversation)", self.author),
        }
    }
}

fn is_bot(login: &str, kind: Option<&str>) -> bool {
    login.ends_with("[bot]") || kind == Some("Bot")
}

impl Gh {
    pub fn new(token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("trpr")
            .build()
            .context("building http client")?;
        Ok(Self { http, token })
    }

    fn req(&self, url: &str) -> reqwest::RequestBuilder {
        self.http
            .get(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self
            .req(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("GET {url} -> {status}: {body}");
        }
        resp.json::<T>()
            .await
            .with_context(|| format!("decoding {url}"))
    }

    /// The open PR whose head is `branch`. Tries the indexed head filter
    /// first, then falls back to scanning open PRs (covers fork-headed PRs).
    pub async fn pr_for_branch(&self, repo: &str, branch: &str) -> Result<Option<Pr>> {
        let owner = repo.split('/').next().unwrap_or("");
        let url = format!("{API}/repos/{repo}/pulls?state=open&head={owner}:{branch}");
        let prs: Vec<Pr> = self.get_json(&url).await.with_context(|| {
            format!(
                "listing PRs for {repo} — a 404 usually means the token can't see the \
                 repo (private repo: the fine-grained PAT needs '{owner}' as resource \
                 owner with this repo granted)"
            )
        })?;
        if let Some(pr) = prs.into_iter().next() {
            return Ok(Some(pr));
        }
        let prs: Vec<Pr> = self
            .get_json(&format!("{API}/repos/{repo}/pulls?state=open&per_page=100"))
            .await?;
        Ok(prs.into_iter().find(|p| p.head.branch == branch))
    }

    /// All open PRs with enough detail for the list screen, in one GraphQL
    /// round-trip: unresolved-thread counts and latest comment activity.
    pub async fn open_pr_summaries(&self, repo: &str) -> Result<Vec<PrSummary>> {
        let (owner, name) = repo.split_once('/').context("repo must be owner/name")?;
        let query = r#"
            query($owner: String!, $name: String!) {
              repository(owner: $owner, name: $name) {
                pullRequests(states: OPEN, first: 50,
                             orderBy: {field: UPDATED_AT, direction: DESC}) {
                  nodes {
                    number
                    title
                    headRefName
                    author { login }
                    assignees(first: 1) { nodes { login } }
                    comments(last: 1) { totalCount nodes { createdAt } }
                    reviewThreads(first: 100) {
                      nodes {
                        isResolved
                        comments(last: 1) { nodes { createdAt } }
                      }
                    }
                  }
                }
              }
            }"#;
        let v = self
            .graphql(query, serde_json::json!({ "owner": owner, "name": name }))
            .await?;

        let mut out = Vec::new();
        let nodes = v["data"]["repository"]["pullRequests"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for pr in nodes {
            let mut unresolved = 0usize;
            let mut last_activity = 0i64;
            for t in pr["reviewThreads"]["nodes"]
                .as_array()
                .unwrap_or(&Vec::new())
            {
                if t["isResolved"].as_bool().unwrap_or(true) {
                    continue;
                }
                unresolved += 1;
                if let Some(ts) = t["comments"]["nodes"][0]["createdAt"]
                    .as_str()
                    .and_then(parse_epoch)
                {
                    last_activity = last_activity.max(ts);
                }
            }
            unresolved += pr["comments"]["totalCount"].as_u64().unwrap_or(0) as usize;
            if let Some(ts) = pr["comments"]["nodes"][0]["createdAt"]
                .as_str()
                .and_then(parse_epoch)
            {
                last_activity = last_activity.max(ts);
            }
            out.push(PrSummary {
                number: pr["number"].as_u64().unwrap_or(0),
                title: pr["title"].as_str().unwrap_or("").to_string(),
                branch: pr["headRefName"].as_str().unwrap_or("").to_string(),
                author: pr["author"]["login"].as_str().unwrap_or("?").to_string(),
                assignee: pr["assignees"]["nodes"][0]["login"]
                    .as_str()
                    .map(|s| s.to_string()),
                unresolved,
                last_activity,
            });
        }
        // Grouped by who owns the triage (assignee, else author), PR number
        // ascending within a group.
        out.sort_by(|a, b| {
            a.triage_owner()
                .to_lowercase()
                .cmp(&b.triage_owner().to_lowercase())
                .then(a.number.cmp(&b.number))
        });
        Ok(out)
    }

    async fn graphql(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{API}/graphql"))
            .bearer_auth(&self.token)
            .header("User-Agent", "trpr")
            .json(&serde_json::json!({ "query": query, "variables": variables }))
            .send()
            .await
            .context("POST /graphql")?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await.context("decoding graphql response")?;
        if !status.is_success() || v["errors"].is_array() {
            bail!("graphql query failed ({status}): {}", v["errors"]);
        }
        Ok(v)
    }

    /// Unresolved inline review threads (GraphQL — REST doesn't expose
    /// resolved state) plus all conversation-tab comments. Bots excluded.
    pub async fn unresolved_items(&self, repo: &str, pr: u64) -> Result<Vec<CommentItem>> {
        let mut items = self.unresolved_threads(repo, pr).await?;
        items.extend(self.conversation_comments(repo, pr).await?);
        Ok(items)
    }

    async fn unresolved_threads(&self, repo: &str, pr: u64) -> Result<Vec<CommentItem>> {
        let (owner, name) = repo.split_once('/').context("repo must be owner/name")?;
        let query = r#"
            query($owner: String!, $name: String!, $number: Int!) {
              repository(owner: $owner, name: $name) {
                pullRequest(number: $number) {
                  reviewThreads(first: 100) {
                    nodes {
                      isResolved
                      isOutdated
                      path
                      line
                      comments(first: 50) {
                        nodes { author { login } body url diffHunk createdAt }
                      }
                    }
                  }
                }
              }
            }"#;
        let v = self
            .graphql(
                query,
                serde_json::json!({ "owner": owner, "name": name, "number": pr }),
            )
            .await?;

        let mut out = Vec::new();
        let nodes = v["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for node in nodes {
            if node["isResolved"].as_bool().unwrap_or(true) {
                continue;
            }
            let comments = node["comments"]["nodes"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let Some(first) = comments.first() else {
                continue;
            };
            let author = first["author"]["login"].as_str().unwrap_or("?").to_string();
            if is_bot(&author, None) {
                continue;
            }
            let replies = comments[1..]
                .iter()
                .map(|c| {
                    (
                        c["author"]["login"].as_str().unwrap_or("?").to_string(),
                        c["body"].as_str().unwrap_or("").to_string(),
                    )
                })
                .collect();
            out.push(CommentItem {
                kind: ItemKind::Thread {
                    path: node["path"].as_str().unwrap_or("?").to_string(),
                    line: node["line"].as_u64(),
                    outdated: node["isOutdated"].as_bool().unwrap_or(false),
                },
                author,
                body: first["body"].as_str().unwrap_or("").to_string(),
                replies,
                diff_hunk: first["diffHunk"].as_str().map(|s| s.to_string()),
                url: first["url"].as_str().unwrap_or("").to_string(),
                last_activity: comments
                    .iter()
                    .filter_map(|c| parse_epoch(c["createdAt"].as_str()?))
                    .max()
                    .unwrap_or(0),
            });
        }
        Ok(out)
    }

    async fn conversation_comments(&self, repo: &str, pr: u64) -> Result<Vec<CommentItem>> {
        #[derive(Deserialize)]
        struct User {
            login: String,
            #[serde(rename = "type", default)]
            kind: Option<String>,
        }
        #[derive(Deserialize)]
        struct IssueComment {
            user: User,
            body: String,
            html_url: String,
            #[serde(default)]
            created_at: Option<String>,
        }
        let url = format!("{API}/repos/{repo}/issues/{pr}/comments?per_page=100");
        let comments: Vec<IssueComment> = self.get_json(&url).await?;
        Ok(comments
            .into_iter()
            .filter(|c| !is_bot(&c.user.login, c.user.kind.as_deref()))
            .map(|c| CommentItem {
                kind: ItemKind::Conversation,
                author: c.user.login,
                body: c.body,
                replies: Vec::new(),
                diff_hunk: None,
                url: c.html_url,
                last_activity: c.created_at.as_deref().and_then(parse_epoch).unwrap_or(0),
            })
            .collect())
    }
}

fn parse_epoch(iso: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|d| d.timestamp())
}
