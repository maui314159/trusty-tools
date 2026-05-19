//! `gh` CLI ticketing backend (#245).
//!
//! Why: The token-based REST client (`github::GitHubClient`) requires the user
//! to mint and manage a `GITHUB_TOKEN` PAT. Many users already have the
//! official `gh` CLI installed and authenticated — we should be able to drive
//! GitHub Issues through it without a second credential. This adapter is a
//! drop-in `TicketingClient` that shells out to `gh issue ...` and parses the
//! `--json` output into our canonical `Ticket` shape.
//! What: `GhCliClient` implements `TicketingClient` by running `gh` subprocess
//! commands. `gh_available()` probes whether `gh` is installed AND
//! authenticated so the factory in `build_client()` can decide between the
//! REST and CLI backends.
//! Test: `tests::*` cover construction, label extraction, and status mapping.
//! Network/CLI calls are not exercised in unit tests (env-dependent).

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use super::TicketingClient;
use super::types::{
    CreateTicketReq, Priority, Tag, Ticket, TicketFilter, TicketStatus, TicketingCapabilities,
    UpdateTicketReq,
};

/// Check if `gh` is on PATH and authenticated.
///
/// Why: We only want to fall back to the CLI when it's actually usable —
/// otherwise users get confusing "command not found" or "not authenticated"
/// errors deep inside a tool call.
/// What: Runs `gh auth status` and returns `true` if the process exits zero.
/// Any error (binary missing, auth missing, IO) yields `false`.
/// Test: `gh_available_returns_bool` — only asserts no panic; the actual
/// boolean depends on the test environment.
pub async fn gh_available() -> bool {
    match Command::new("gh").args(["auth", "status"]).output().await {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}

/// `gh` CLI-backed `TicketingClient`.
///
/// Why: Mirrors `GitHubClient` but uses the user's existing `gh` auth,
/// removing the need for a separate `GITHUB_TOKEN`.
/// What: Holds an optional `repo` ("owner/repo"); when `None`, defers to
/// `gh`'s current-directory remote resolution.
/// Test: `gh_cli_client_new_with_repo`, `gh_cli_client_new_without_repo`.
pub struct GhCliClient {
    /// "owner/repo" — if `None`, `gh` uses the current directory's remote.
    repo: Option<String>,
}

impl GhCliClient {
    pub fn new(repo: Option<String>) -> Self {
        Self { repo }
    }

    /// Run a `gh` command and return stdout as `String`.
    ///
    /// Why: Centralizes subprocess spawning + error handling so each tool
    /// method doesn't have to repeat the success-check / stderr-capture
    /// boilerplate.
    /// What: Spawns `gh <args...>`, returns stdout on success or an error
    /// containing stderr on non-zero exit.
    async fn run(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("gh")
            .args(args)
            .output()
            .await
            .with_context(|| format!("failed to spawn 'gh {}'", args.join(" ")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "gh {} failed (exit {}): {}",
                args.join(" "),
                output.status,
                stderr.trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Run a `gh` command, prepending `--repo <repo>` if configured.
    ///
    /// Why: `gh` defaults to the current directory's git remote, but when the
    /// caller has explicitly set a repo we want every command to target it.
    /// What: If `self.repo` is `Some`, runs `gh --repo <repo> <args...>`;
    /// otherwise runs `gh <args...>`.
    async fn run_with_repo(&self, args: &[&str]) -> Result<String> {
        if let Some(repo) = &self.repo {
            let mut combined: Vec<&str> = vec!["--repo", repo.as_str()];
            combined.extend_from_slice(args);
            self.run(&combined).await
        } else {
            self.run(args).await
        }
    }
}

/// Map a `gh issue` JSON object to our canonical `Ticket`.
///
/// Why: `gh --json` output uses different field names from the REST API
/// (e.g. `number` instead of `id`, `OPEN`/`CLOSED` uppercase state, label
/// objects with `.name`). Centralizing the mapping keeps each tool method
/// focused on argv construction.
/// What: Reads `number`, `title`, `body`, `state`, `labels[].name`, `url`,
/// `createdAt`, `updatedAt` and produces a `Ticket`.
/// Test: `ticket_state_mapping`, `label_extraction_from_gh_json`.
fn gh_issue_to_ticket(v: &Value) -> Result<Ticket> {
    let id = v
        .get("number")
        .and_then(Value::as_i64)
        .map(|n| n.to_string())
        .ok_or_else(|| anyhow!("gh issue JSON missing 'number'"))?;
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
    let status = parse_gh_state(v.get("state").and_then(Value::as_str).unwrap_or("OPEN"));
    let labels = extract_labels(v.get("labels"));
    let url = v.get("url").and_then(Value::as_str).map(|s| s.to_string());
    let created_at = v
        .get("createdAt")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    let updated_at = v
        .get("updatedAt")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    let assignee = v
        .get("assignees")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|a| a.get("login").and_then(Value::as_str))
        .map(|s| s.to_string());

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

/// Map gh's uppercase `state` string to `TicketStatus`.
///
/// Why: `gh --json state` returns `"OPEN"` / `"CLOSED"` (uppercase) whereas
/// the REST API uses lowercase. Anything else (defensive) maps to `Open`.
/// Test: `ticket_state_mapping`.
fn parse_gh_state(state: &str) -> TicketStatus {
    match state {
        "CLOSED" | "closed" => TicketStatus::Closed,
        _ => TicketStatus::Open,
    }
}

/// Extract a list of label names from `gh --json labels` output.
///
/// Why: `gh` returns labels as an array of objects (`[{"id":..,
/// "name":"bug","color":".."}]`); we only care about the names.
/// Test: `label_extraction_from_gh_json`.
fn extract_labels(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

const TICKET_JSON_FIELDS: &str = "number,title,body,state,labels,url,createdAt,updatedAt,assignees";
const LIST_JSON_FIELDS: &str = "number,title,state,labels,url,createdAt,updatedAt,assignees";

/// Plan the sequence of `gh issue edit` invocations needed for an
/// `UpdateTicketReq`.
///
/// Why: Pulling the argv-construction out of `update_ticket` makes it pure
/// and unit-testable without spawning a real `gh` subprocess. Critically,
/// this is where #248 C2 was fixed — `add_labels` and `remove_labels` were
/// silently dropped before; the planner now emits dedicated `--add-label` /
/// `--remove-label` calls for them.
/// What: Returns a `Vec<Vec<String>>`; each inner vec is the arg list for
/// one `gh` invocation (excluding any `--repo` prefix, which `run_with_repo`
/// adds). Empty outer vec means "nothing to do".
/// Test: `plan_gh_issue_edit_calls_*` tests cover field combos and the
/// add/remove label paths.
fn plan_gh_issue_edit_calls(id: &str, req: &UpdateTicketReq) -> Vec<Vec<String>> {
    let mut calls: Vec<Vec<String>> = Vec::new();

    // Main combined edit (title/body/labels-replace/assignee).
    let mut main: Vec<String> = vec!["issue".into(), "edit".into(), id.to_string()];
    if let Some(t) = req.title.as_deref() {
        main.push("--title".into());
        main.push(t.to_string());
    }
    if let Some(b) = req.body.as_deref() {
        main.push("--body".into());
        main.push(b.to_string());
    }
    if let Some(labels) = req.labels.as_ref()
        && !labels.is_empty()
    {
        main.push("--add-label".into());
        main.push(labels.join(","));
    }
    if let Some(a) = req.assignee.as_deref() {
        main.push("--add-assignee".into());
        main.push(a.to_string());
    }
    if main.len() > 3 {
        calls.push(main);
    }

    // #248 C2: dedicated label-delta calls.
    if let Some(adds) = req.add_labels.as_ref()
        && !adds.is_empty()
    {
        calls.push(vec![
            "issue".into(),
            "edit".into(),
            id.to_string(),
            "--add-label".into(),
            adds.join(","),
        ]);
    }
    if let Some(rems) = req.remove_labels.as_ref()
        && !rems.is_empty()
    {
        calls.push(vec![
            "issue".into(),
            "edit".into(),
            id.to_string(),
            "--remove-label".into(),
            rems.join(","),
        ]);
    }

    calls
}

#[async_trait]
impl TicketingClient for GhCliClient {
    fn provider_name(&self) -> &str {
        "github-gh-cli"
    }

    async fn create_ticket(&self, req: CreateTicketReq) -> Result<Ticket> {
        // `gh issue create --json` is not supported on all gh versions, so
        // we create then look up by URL. `gh issue create` prints the URL on
        // stdout; the issue number is the last path segment.
        let labels_str;
        let mut args: Vec<&str> = vec![
            "issue", "create", "--title", &req.title, "--body", &req.body,
        ];
        if !req.labels.is_empty() {
            labels_str = req.labels.join(",");
            args.push("--label");
            args.push(&labels_str);
        }
        if let Some(a) = req.assignee.as_deref() {
            args.push("--assignee");
            args.push(a);
        }
        let stdout = self.run_with_repo(&args).await?;
        let url = stdout.trim();
        let num = url
            .rsplit('/')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| anyhow!("could not parse issue number from gh output: '{url}'"))?;
        let _ = req.priority; // priority not settable via gh issue create
        self.get_ticket(&num.to_string()).await
    }

    async fn get_ticket(&self, id: &str) -> Result<Ticket> {
        let stdout = self
            .run_with_repo(&["issue", "view", id, "--json", TICKET_JSON_FIELDS])
            .await?;
        let v: Value =
            serde_json::from_str(&stdout).context("failed to parse `gh issue view` JSON")?;
        gh_issue_to_ticket(&v)
    }

    async fn update_ticket(&self, id: &str, req: UpdateTicketReq) -> Result<Ticket> {
        // Plan all `gh issue edit` invocations (pure, see `plan_gh_issue_edit_calls`).
        // #248 C2: this now includes dedicated --add-label / --remove-label
        // calls for `req.add_labels` / `req.remove_labels`, which were
        // silently ignored before.
        for call in plan_gh_issue_edit_calls(id, &req) {
            let arg_refs: Vec<&str> = call.iter().map(|s| s.as_str()).collect();
            self.run_with_repo(&arg_refs).await?;
        }

        // Apply state transition separately (`gh issue close` / `reopen`).
        if let Some(s) = req.status {
            match s {
                TicketStatus::Closed | TicketStatus::Done | TicketStatus::Cancelled => {
                    self.run_with_repo(&["issue", "close", id]).await?;
                }
                TicketStatus::Open
                | TicketStatus::InProgress
                | TicketStatus::InReview
                | TicketStatus::Blocked => {
                    self.run_with_repo(&["issue", "reopen", id]).await?;
                }
                // Custom statuses can't be applied to a GitHub issue's
                // open/closed flag — silently ignore.
                TicketStatus::Custom(_) => {}
            }
        }

        self.get_ticket(id).await
    }

    async fn close_ticket(&self, id: &str) -> Result<()> {
        self.run_with_repo(&["issue", "close", id]).await?;
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
        let limit = filter.limit.unwrap_or(50).to_string();
        let mut args: Vec<String> = vec![
            "issue".into(),
            "list".into(),
            "--state".into(),
            state.into(),
            "--limit".into(),
            limit,
            "--json".into(),
            LIST_JSON_FIELDS.into(),
        ];
        if !filter.labels.is_empty() {
            args.push("--label".into());
            args.push(filter.labels.join(","));
        }
        if let Some(a) = filter.assignee.as_deref() {
            args.push("--assignee".into());
            args.push(a.to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let stdout = self.run_with_repo(&arg_refs).await?;
        let arr: Vec<Value> =
            serde_json::from_str(&stdout).context("failed to parse `gh issue list` JSON")?;
        let mut out = Vec::with_capacity(arr.len());
        for v in &arr {
            out.push(gh_issue_to_ticket(v).map(|mut t| {
                // list output omits body; leave empty string.
                if t.priority.is_none() {
                    t.priority = None::<Priority>;
                }
                t
            })?);
        }
        Ok(out)
    }

    async fn add_comment(&self, id: &str, body: &str) -> Result<()> {
        self.run_with_repo(&["issue", "comment", id, "--body", body])
            .await?;
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
        if !tags.is_empty() {
            let joined = tags.join(",");
            self.run_with_repo(&["issue", "edit", id, "--add-label", &joined])
                .await?;
        }
        self.get_ticket(id).await
    }

    async fn remove_tags(&self, id: &str, tags: &[String]) -> Result<Ticket> {
        if !tags.is_empty() {
            let joined = tags.join(",");
            self.run_with_repo(&["issue", "edit", id, "--remove-label", &joined])
                .await?;
        }
        self.get_ticket(id).await
    }

    async fn list_available_tags(&self) -> Result<Vec<Tag>> {
        let stdout = self
            .run_with_repo(&["label", "list", "--json", "name,color,description"])
            .await?;
        let arr: Vec<Value> =
            serde_json::from_str(&stdout).context("failed to parse `gh label list` JSON")?;
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
        self.run_with_repo(&["issue", "edit", id, "--add-assignee", assignee])
            .await?;
        self.get_ticket(id).await
    }

    async fn unassign(&self, id: &str) -> Result<Ticket> {
        // gh requires knowing the assignee to remove. Look up current,
        // then remove all of them.
        let t = self.get_ticket(id).await?;
        if let Some(a) = t.assignee.as_deref() {
            self.run_with_repo(&["issue", "edit", id, "--remove-assignee", a])
                .await?;
        }
        self.get_ticket(id).await
    }

    async fn search(&self, query: &str, filter: TicketFilter) -> Result<Vec<Ticket>> {
        let state = match filter.status {
            Some(TicketStatus::Closed)
            | Some(TicketStatus::Done)
            | Some(TicketStatus::Cancelled) => "closed",
            Some(_) => "open",
            None => "all",
        };
        let limit = filter.limit.unwrap_or(30).to_string();
        let args: Vec<String> = vec![
            "issue".into(),
            "list".into(),
            "--search".into(),
            query.to_string(),
            "--state".into(),
            state.into(),
            "--limit".into(),
            limit,
            "--json".into(),
            LIST_JSON_FIELDS.into(),
        ];
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let stdout = self.run_with_repo(&arg_refs).await?;
        let arr: Vec<Value> =
            serde_json::from_str(&stdout).context("failed to parse `gh issue list` JSON")?;
        let mut out = Vec::with_capacity(arr.len());
        for v in &arr {
            let mut t = gh_issue_to_ticket(v)?;
            if t.priority.is_none() {
                t.priority = None::<Priority>;
            }
            out.push(t);
        }
        Ok(out)
    }

    async fn count_open_issues(&self, repo: &str) -> Result<u32> {
        // #342: cheap "how many issues" signal for project-discovery UIs.
        // Why subprocess-and-tolerant: `gh` may not be authed for `repo` (or
        // installed at all). Failing here would surface a noisy error in
        // every `/projects` render; instead we degrade gracefully to 0.
        let output = Command::new("gh")
            .args([
                "issue", "list", "--repo", repo, "--state", "open", "--json", "number",
            ])
            .output()
            .await?;
        if !output.status.success() {
            return Ok(0);
        }
        let json: Value = serde_json::from_slice(&output.stdout)?;
        Ok(json.as_array().map(|a| a.len() as u32).unwrap_or(0))
    }

    async fn count_open_prs(&self, repo: &str) -> Result<u32> {
        // #342: companion to `count_open_issues` — same graceful-degrade
        // semantics on auth/install failure.
        let output = Command::new("gh")
            .args([
                "pr", "list", "--repo", repo, "--state", "open", "--json", "number",
            ])
            .output()
            .await?;
        if !output.status.success() {
            return Ok(0);
        }
        let json: Value = serde_json::from_slice(&output.stdout)?;
        Ok(json.as_array().map(|a| a.len() as u32).unwrap_or(0))
    }

    async fn available_transitions(&self, _id: &str) -> Result<Vec<TicketStatus>> {
        Ok(vec![
            TicketStatus::Open,
            TicketStatus::InProgress,
            TicketStatus::InReview,
            TicketStatus::Closed,
            TicketStatus::Cancelled,
        ])
    }

    async fn transition(&self, id: &str, to: TicketStatus) -> Result<Ticket> {
        match &to {
            TicketStatus::Closed | TicketStatus::Done => {
                self.run_with_repo(&["issue", "close", id]).await?;
            }
            TicketStatus::Cancelled => {
                self.run_with_repo(&["issue", "edit", id, "--add-label", "status:cancelled"])
                    .await?;
                self.run_with_repo(&["issue", "close", id]).await?;
            }
            TicketStatus::Open => {
                self.run_with_repo(&["issue", "reopen", id]).await?;
            }
            TicketStatus::InProgress => {
                self.run_with_repo(&["issue", "edit", id, "--add-label", "status:in-progress"])
                    .await?;
            }
            TicketStatus::InReview => {
                self.run_with_repo(&["issue", "edit", id, "--add-label", "status:in-review"])
                    .await?;
            }
            TicketStatus::Blocked => {
                self.run_with_repo(&["issue", "edit", id, "--add-label", "status:blocked"])
                    .await?;
            }
            TicketStatus::Custom(name) => {
                let label = format!("status:{}", name);
                self.run_with_repo(&["issue", "edit", id, "--add-label", &label])
                    .await?;
            }
        }
        self.get_ticket(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gh_cli_client_new_with_repo() {
        let c = GhCliClient::new(Some("owner/repo".to_string()));
        assert_eq!(c.repo.as_deref(), Some("owner/repo"));
    }

    #[test]
    fn gh_cli_client_new_without_repo() {
        let c = GhCliClient::new(None);
        assert!(c.repo.is_none());
    }

    #[test]
    fn provider_name_is_github_gh_cli() {
        let c = GhCliClient::new(None);
        assert_eq!(c.provider_name(), "github-gh-cli");
    }

    #[tokio::test]
    async fn gh_available_returns_bool() {
        // Just assert it returns without panic — env-dependent (CI may or
        // may not have gh installed and authed).
        let _ = gh_available().await;
    }

    #[test]
    fn ticket_state_mapping() {
        assert_eq!(parse_gh_state("OPEN"), TicketStatus::Open);
        assert_eq!(parse_gh_state("CLOSED"), TicketStatus::Closed);
        // Lowercase form (defensive) also works.
        assert_eq!(parse_gh_state("closed"), TicketStatus::Closed);
        // Anything unknown defaults to Open.
        assert_eq!(parse_gh_state("UNKNOWN"), TicketStatus::Open);
    }

    #[test]
    fn label_extraction_from_gh_json() {
        let labels = json!([
            {"id": "1", "name": "bug", "color": "red"},
            {"id": "2", "name": "feature", "color": "blue"},
        ]);
        let extracted = extract_labels(Some(&labels));
        assert_eq!(extracted, vec!["bug".to_string(), "feature".to_string()]);
    }

    #[test]
    fn label_extraction_handles_empty_or_missing() {
        assert!(extract_labels(None).is_empty());
        assert!(extract_labels(Some(&json!([]))).is_empty());
        // Object missing 'name' is filtered out.
        let bad = json!([{"id": "1"}, {"name": "ok"}]);
        assert_eq!(extract_labels(Some(&bad)), vec!["ok".to_string()]);
    }

    #[test]
    fn gh_issue_to_ticket_parses_canonical_fields() {
        let v = json!({
            "number": 42,
            "title": "Fix bug",
            "body": "Repro steps…",
            "state": "OPEN",
            "labels": [{"name": "bug"}],
            "url": "https://github.com/o/r/issues/42",
            "createdAt": "2024-01-01T00:00:00Z",
            "updatedAt": "2024-01-02T00:00:00Z",
            "assignees": [{"login": "alice"}],
        });
        let t = gh_issue_to_ticket(&v).expect("parses");
        assert_eq!(t.id, "42");
        assert_eq!(t.title, "Fix bug");
        assert_eq!(t.status, TicketStatus::Open);
        assert_eq!(t.labels, vec!["bug".to_string()]);
        assert_eq!(t.assignee.as_deref(), Some("alice"));
        assert!(t.url.is_some());
        assert!(t.created_at.is_some());
        assert!(t.updated_at.is_some());
    }

    #[test]
    fn gh_issue_to_ticket_requires_number() {
        let v = json!({"title": "no number"});
        assert!(gh_issue_to_ticket(&v).is_err());
    }

    /// Empty request → no `gh` calls planned.
    #[test]
    fn plan_gh_issue_edit_calls_empty_request_emits_nothing() {
        let req = UpdateTicketReq::default();
        let plan = plan_gh_issue_edit_calls("42", &req);
        assert!(plan.is_empty(), "expected no calls, got {:?}", plan);
    }

    /// #248 C2: `add_labels` produces a dedicated `--add-label` call even
    /// when no other fields are set.
    #[test]
    fn plan_gh_issue_edit_calls_add_labels_emits_add_label_call() {
        let req = UpdateTicketReq {
            add_labels: Some(vec!["bug".into(), "p0".into()]),
            ..Default::default()
        };
        let plan = plan_gh_issue_edit_calls("42", &req);
        assert_eq!(plan.len(), 1, "expected single call, got {:?}", plan);
        assert_eq!(
            plan[0],
            vec![
                "issue".to_string(),
                "edit".into(),
                "42".into(),
                "--add-label".into(),
                "bug,p0".into(),
            ]
        );
    }

    /// #248 C2: `remove_labels` produces a `--remove-label` call.
    #[test]
    fn plan_gh_issue_edit_calls_remove_labels_emits_remove_label_call() {
        let req = UpdateTicketReq {
            remove_labels: Some(vec!["wontfix".into()]),
            ..Default::default()
        };
        let plan = plan_gh_issue_edit_calls("7", &req);
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0],
            vec![
                "issue".to_string(),
                "edit".into(),
                "7".into(),
                "--remove-label".into(),
                "wontfix".into(),
            ]
        );
    }

    /// #248 C2: `add_labels` + `remove_labels` both emit, in order.
    #[test]
    fn plan_gh_issue_edit_calls_add_and_remove_labels_both_emit() {
        let req = UpdateTicketReq {
            add_labels: Some(vec!["bug".into()]),
            remove_labels: Some(vec!["needs-triage".into()]),
            ..Default::default()
        };
        let plan = plan_gh_issue_edit_calls("99", &req);
        assert_eq!(plan.len(), 2, "expected 2 calls, got {:?}", plan);
        assert!(plan[0].contains(&"--add-label".to_string()));
        assert!(plan[1].contains(&"--remove-label".to_string()));
    }

    /// Empty `add_labels: Some(vec![])` is a no-op (no spurious empty call).
    #[test]
    fn plan_gh_issue_edit_calls_empty_label_vecs_are_noop() {
        let req = UpdateTicketReq {
            add_labels: Some(vec![]),
            remove_labels: Some(vec![]),
            ..Default::default()
        };
        assert!(plan_gh_issue_edit_calls("1", &req).is_empty());
    }

    /// Title + add_labels emits a combined main edit AND a separate
    /// add-label call (deltas are never folded into the main edit).
    #[test]
    fn plan_gh_issue_edit_calls_combines_main_and_label_delta() {
        let req = UpdateTicketReq {
            title: Some("New title".into()),
            add_labels: Some(vec!["bug".into()]),
            ..Default::default()
        };
        let plan = plan_gh_issue_edit_calls("5", &req);
        assert_eq!(plan.len(), 2);
        // Main edit comes first, with --title.
        assert!(plan[0].contains(&"--title".to_string()));
        // Label delta is the second invocation.
        assert!(plan[1].contains(&"--add-label".to_string()));
    }
}
