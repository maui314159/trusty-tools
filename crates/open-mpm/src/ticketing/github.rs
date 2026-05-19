//! GitHub Issues adapter.
//!
//! Why: GitHub is the most common issue tracker for OSS projects. REST v3
//! is stable and well-documented; bearer auth is simple.
//! What: `GitHubClient` implements `TicketingClient` against
//! `https://api.github.com/repos/{owner}/{repo}/issues...`.
//! Test: Construction tests in `src/ticketing/mod.rs` cover missing
//! credentials; live calls are not exercised in unit tests.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Value, json};

use super::types::*;
use super::{TicketingClient, TicketingConfig};

const GH_API: &str = "https://api.github.com";

/// GitHub Issues client.
pub struct GitHubClient {
    client: reqwest::Client,
    owner: String,
    repo: String,
}

impl GitHubClient {
    /// Build a new GitHub client.
    ///
    /// Why: Fail fast on missing credentials so the agent gets a clear error
    /// rather than a cryptic 401 later.
    /// What: Resolves token from config or `GITHUB_TOKEN` env; splits
    /// `owner/repo`; builds a reqwest client with default auth headers.
    /// Test: `github_client_new_requires_token`, `github_client_new_requires_repo`.
    pub fn new(config: &TicketingConfig) -> Result<Self> {
        let token = config
            .github_token
            .clone()
            .or_else(|| std::env::var("GITHUB_TOKEN").ok())
            .ok_or_else(|| {
                anyhow!("GitHub token required (set github_token or GITHUB_TOKEN env)")
            })?;
        let repo_full = config
            .github_repo
            .clone()
            .ok_or_else(|| anyhow!("github_repo required (owner/repo)"))?;
        let (owner, repo) = repo_full.split_once('/').ok_or_else(|| {
            anyhow!("github_repo must be in 'owner/repo' format, got '{repo_full}'")
        })?;

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))
                .context("invalid characters in GitHub token")?,
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static("open-mpm/0.1.0"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build reqwest client for GitHub")?;

        Ok(Self {
            client,
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
    }

    fn issues_url(&self) -> String {
        format!("{GH_API}/repos/{}/{}/issues", self.owner, self.repo)
    }

    fn issue_url(&self, id: &str) -> String {
        format!("{GH_API}/repos/{}/{}/issues/{}", self.owner, self.repo, id)
    }
}

/// Map a GitHub issue JSON payload to our canonical `Ticket`.
fn issue_to_ticket(v: &Value) -> Result<Ticket> {
    let id = v
        .get("number")
        .and_then(Value::as_i64)
        .map(|n| n.to_string())
        .ok_or_else(|| anyhow!("issue missing 'number'"))?;
    let title = v
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let body = v
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let state = v.get("state").and_then(Value::as_str).unwrap_or("open");
    let status = match state {
        "closed" => TicketStatus::Closed,
        _ => TicketStatus::Open,
    };
    let labels: Vec<String> = v
        .get("labels")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let assignee = v
        .get("assignee")
        .and_then(|a| a.get("login").and_then(Value::as_str))
        .map(|s| s.to_string());
    let url = v
        .get("html_url")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let created_at = v
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    let updated_at = v
        .get("updated_at")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));

    Ok(Ticket {
        id,
        title,
        body,
        status,
        priority: None,
        labels,
        assignee,
        created_at,
        updated_at,
        url,
    })
}

#[async_trait]
impl TicketingClient for GitHubClient {
    fn provider_name(&self) -> &str {
        "github"
    }

    async fn create_ticket(&self, req: CreateTicketReq) -> Result<Ticket> {
        let mut body = json!({
            "title": req.title,
            "body": req.body,
        });
        if !req.labels.is_empty() {
            body["labels"] = json!(req.labels);
        }
        if let Some(a) = req.assignee {
            body["assignees"] = json!([a]);
        }
        let resp = self
            .client
            .post(self.issues_url())
            .json(&body)
            .send()
            .await
            .context("GitHub create_ticket: request failed")?;
        let status = resp.status();
        let v: Value = resp
            .json()
            .await
            .context("GitHub create_ticket: parse response")?;
        if !status.is_success() {
            anyhow::bail!("GitHub create_ticket HTTP {status}: {v}");
        }
        issue_to_ticket(&v)
    }

    async fn get_ticket(&self, id: &str) -> Result<Ticket> {
        let resp = self
            .client
            .get(self.issue_url(id))
            .send()
            .await
            .context("GitHub get_ticket: request failed")?;
        let status = resp.status();
        let v: Value = resp.json().await.context("GitHub get_ticket: parse")?;
        if !status.is_success() {
            anyhow::bail!("GitHub get_ticket HTTP {status}: {v}");
        }
        issue_to_ticket(&v)
    }

    async fn update_ticket(&self, id: &str, req: UpdateTicketReq) -> Result<Ticket> {
        let mut body = serde_json::Map::new();
        if let Some(t) = req.title {
            body.insert("title".into(), json!(t));
        }
        if let Some(b) = req.body {
            body.insert("body".into(), json!(b));
        }
        if let Some(s) = req.status {
            let state = match s {
                TicketStatus::Closed | TicketStatus::Done | TicketStatus::Cancelled => "closed",
                _ => "open",
            };
            body.insert("state".into(), json!(state));
        }

        // Resolve label set with add/remove deltas.
        //
        // Why: `labels` (replace), `add_labels`, and `remove_labels` can
        // coexist in one request — start from the new replacement set when
        // provided, else from the current labels, then apply add/remove.
        // What: Fetch current labels only when needed; build the final set;
        // PATCH once.
        let needs_label_delta = req.add_labels.is_some() || req.remove_labels.is_some();
        if needs_label_delta || req.labels.is_some() {
            let mut current: Vec<String> = if let Some(ls) = req.labels {
                ls
            } else {
                self.get_ticket(id).await?.labels
            };
            if let Some(adds) = req.add_labels {
                for a in adds {
                    if !current.contains(&a) {
                        current.push(a);
                    }
                }
            }
            if let Some(rems) = req.remove_labels {
                current.retain(|l| !rems.contains(l));
            }
            body.insert("labels".into(), json!(current));
        }

        if let Some(a) = req.assignee {
            body.insert("assignees".into(), json!([a]));
        }
        if let Some(m) = req.milestone {
            // GitHub expects milestone *number* (int) or null. We accept a
            // string from callers and try to parse it as an int; if it
            // doesn't parse, send the string and let GitHub error.
            let v = m
                .parse::<i64>()
                .map(|n| json!(n))
                .unwrap_or_else(|_| json!(m));
            body.insert("milestone".into(), v);
        }
        let _ = req.priority; // GitHub has no native priority field.
        let resp = self
            .client
            .patch(self.issue_url(id))
            .json(&Value::Object(body))
            .send()
            .await
            .context("GitHub update_ticket: request failed")?;
        let status = resp.status();
        let v: Value = resp.json().await.context("GitHub update_ticket: parse")?;
        if !status.is_success() {
            anyhow::bail!("GitHub update_ticket HTTP {status}: {v}");
        }
        issue_to_ticket(&v)
    }

    async fn close_ticket(&self, id: &str) -> Result<()> {
        let resp = self
            .client
            .patch(self.issue_url(id))
            .json(&json!({ "state": "closed" }))
            .send()
            .await
            .context("GitHub close_ticket: request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub close_ticket HTTP {status}: {text}");
        }
        Ok(())
    }

    async fn list_tickets(&self, filter: TicketFilter) -> Result<Vec<Ticket>> {
        let state = match filter.status {
            Some(TicketStatus::Closed)
            | Some(TicketStatus::Done)
            | Some(TicketStatus::Cancelled) => "closed",
            Some(_) => "open",
            None => "all",
        };
        let mut q: Vec<(String, String)> = vec![("state".into(), state.into())];
        if !filter.labels.is_empty() {
            q.push(("labels".into(), filter.labels.join(",")));
        }
        if let Some(a) = filter.assignee {
            q.push(("assignee".into(), a));
        }
        if let Some(n) = filter.limit {
            q.push(("per_page".into(), n.to_string()));
        }
        let resp = self
            .client
            .get(self.issues_url())
            .query(&q)
            .send()
            .await
            .context("GitHub list_tickets: request failed")?;
        let status = resp.status();
        let v: Value = resp.json().await.context("GitHub list_tickets: parse")?;
        if !status.is_success() {
            anyhow::bail!("GitHub list_tickets HTTP {status}: {v}");
        }
        let arr = v
            .as_array()
            .ok_or_else(|| anyhow!("GitHub list_tickets: expected array"))?;
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            // Skip PRs which also appear under /issues.
            if item.get("pull_request").is_some() {
                continue;
            }
            out.push(issue_to_ticket(item)?);
        }
        Ok(out)
    }

    async fn add_comment(&self, id: &str, body: &str) -> Result<()> {
        let url = format!("{}/comments", self.issue_url(id));
        let resp = self
            .client
            .post(url)
            .json(&json!({ "body": body }))
            .send()
            .await
            .context("GitHub add_comment: request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub add_comment HTTP {status}: {text}");
        }
        Ok(())
    }

    // ----- #246: capabilities, tagging, ownership, search, transitions -----

    fn capabilities(&self) -> TicketingCapabilities {
        TicketingCapabilities {
            tagging: true,
            transitions: true,
            ownership: true,
            search: true,
            milestones: false,
        }
    }

    async fn add_tags(&self, id: &str, tags: &[String]) -> Result<Ticket> {
        // GitHub's POST /issues/{n}/labels appends without replacing.
        let url = format!("{}/labels", self.issue_url(id));
        let resp = self
            .client
            .post(url)
            .json(&json!({ "labels": tags }))
            .send()
            .await
            .context("GitHub add_tags: request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub add_tags HTTP {status}: {text}");
        }
        self.get_ticket(id).await
    }

    async fn remove_tags(&self, id: &str, tags: &[String]) -> Result<Ticket> {
        // GitHub requires DELETE per-label; loop sequentially.
        for t in tags {
            let url = format!("{}/labels/{}", self.issue_url(id), urlencoding(t));
            let resp = self
                .client
                .delete(url)
                .send()
                .await
                .context("GitHub remove_tags: request failed")?;
            // 404 means label wasn't on the issue — ignore. Other errors bubble.
            if !resp.status().is_success() && resp.status().as_u16() != 404 {
                let s = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("GitHub remove_tags HTTP {s}: {text}");
            }
        }
        self.get_ticket(id).await
    }

    async fn list_available_tags(&self) -> Result<Vec<Tag>> {
        let url = format!("{GH_API}/repos/{}/{}/labels", self.owner, self.repo);
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .context("GitHub list_available_tags: request failed")?;
        let status = resp.status();
        let v: Value = resp
            .json()
            .await
            .context("GitHub list_available_tags: parse")?;
        if !status.is_success() {
            anyhow::bail!("GitHub list_available_tags HTTP {status}: {v}");
        }
        let arr = v
            .as_array()
            .ok_or_else(|| anyhow!("GitHub list_available_tags: expected array"))?;
        let out = arr
            .iter()
            .filter_map(|l| {
                let name = l.get("name").and_then(Value::as_str)?.to_string();
                let color = l.get("color").and_then(Value::as_str).map(String::from);
                let description = l
                    .get("description")
                    .and_then(Value::as_str)
                    .map(String::from);
                Some(Tag {
                    name,
                    color,
                    description,
                })
            })
            .collect();
        Ok(out)
    }

    async fn assign(&self, id: &str, assignee: &str) -> Result<Ticket> {
        let resp = self
            .client
            .patch(self.issue_url(id))
            .json(&json!({ "assignees": [assignee] }))
            .send()
            .await
            .context("GitHub assign: request failed")?;
        let status = resp.status();
        let v: Value = resp.json().await.context("GitHub assign: parse")?;
        if !status.is_success() {
            anyhow::bail!("GitHub assign HTTP {status}: {v}");
        }
        issue_to_ticket(&v)
    }

    async fn unassign(&self, id: &str) -> Result<Ticket> {
        let resp = self
            .client
            .patch(self.issue_url(id))
            .json(&json!({ "assignees": Vec::<String>::new() }))
            .send()
            .await
            .context("GitHub unassign: request failed")?;
        let status = resp.status();
        let v: Value = resp.json().await.context("GitHub unassign: parse")?;
        if !status.is_success() {
            anyhow::bail!("GitHub unassign HTTP {status}: {v}");
        }
        issue_to_ticket(&v)
    }

    async fn search(&self, query: &str, filter: TicketFilter) -> Result<Vec<Ticket>> {
        // GitHub search syntax: q=foo+repo:owner/repo+is:issue+state:open
        let mut q = format!("{query} repo:{}/{} is:issue", self.owner, self.repo);
        if let Some(s) = &filter.status {
            let state = match s {
                TicketStatus::Closed | TicketStatus::Done | TicketStatus::Cancelled => "closed",
                _ => "open",
            };
            q.push_str(&format!(" state:{state}"));
        }
        for label in &filter.labels {
            q.push_str(&format!(" label:\"{label}\""));
        }
        if let Some(a) = &filter.assignee {
            q.push_str(&format!(" assignee:{a}"));
        }
        let mut params: Vec<(String, String)> = vec![("q".into(), q)];
        if let Some(n) = filter.limit {
            params.push(("per_page".into(), n.to_string()));
        }
        let url = format!("{GH_API}/search/issues");
        let resp = self
            .client
            .get(url)
            .query(&params)
            .send()
            .await
            .context("GitHub search: request failed")?;
        let status = resp.status();
        let v: Value = resp.json().await.context("GitHub search: parse")?;
        if !status.is_success() {
            anyhow::bail!("GitHub search HTTP {status}: {v}");
        }
        let items = v
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("GitHub search: missing items"))?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            if item.get("pull_request").is_some() {
                continue;
            }
            out.push(issue_to_ticket(item)?);
        }
        Ok(out)
    }

    async fn available_transitions(&self, _id: &str) -> Result<Vec<TicketStatus>> {
        // GitHub has no formal workflow — expose the canonical set we map.
        Ok(vec![
            TicketStatus::Open,
            TicketStatus::InProgress,
            TicketStatus::InReview,
            TicketStatus::Closed,
            TicketStatus::Cancelled,
        ])
    }

    async fn transition(&self, id: &str, to: TicketStatus) -> Result<Ticket> {
        // Map non-terminal statuses to a label like "status:in-progress" so
        // the transition is visible even though GitHub only has open/closed.
        let (state, label_hint): (&str, Option<&str>) = match &to {
            TicketStatus::Open => ("open", None),
            TicketStatus::InProgress => ("open", Some("status:in-progress")),
            TicketStatus::InReview => ("open", Some("status:in-review")),
            TicketStatus::Blocked => ("open", Some("status:blocked")),
            TicketStatus::Closed | TicketStatus::Done => ("closed", None),
            TicketStatus::Cancelled => ("closed", Some("status:cancelled")),
            TicketStatus::Custom(name) => {
                // Custom statuses don't change open/closed; just label.
                let label = format!("status:{}", name);
                let _ = self.add_tags(id, &[label]).await?;
                return self.get_ticket(id).await;
            }
        };
        let resp = self
            .client
            .patch(self.issue_url(id))
            .json(&json!({ "state": state }))
            .send()
            .await
            .context("GitHub transition: request failed")?;
        let s = resp.status();
        if !s.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub transition HTTP {s}: {text}");
        }
        if let Some(l) = label_hint {
            let _ = self.add_tags(id, &[l.to_string()]).await; // best-effort
        }
        self.get_ticket(id).await
    }
}

/// Minimal URL-path encoder for label names (which can contain spaces/`#`/`/`).
///
/// Why: `reqwest` doesn't auto-encode path segments and labels may contain
/// characters that break the URL.
/// What: Percent-encodes anything outside the unreserved set.
/// Test: Indirectly via remove_tags integration (not unit-tested here).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            other => out.push_str(&format!("%{:02X}", other)),
        }
    }
    out
}
