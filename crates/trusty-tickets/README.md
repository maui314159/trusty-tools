# trusty-tickets

Unified ticketing MCP server — one [Model Context Protocol](https://modelcontextprotocol.io/)
surface that can talk to **GitHub Issues**, **JIRA**, and **Linear** without
the model having to know which backend is configured.

Exposes 31 MCP tools (issues, comments, labels, milestones, projects,
epics, states, transitions) and resolves each call to the appropriate
backend at dispatch time.

## Installation

```sh
cargo install --path crates/trusty-tickets
```

This installs the `tickets-mcp` binary into `~/.cargo/bin`.

## Quick Start

1. **Configure at least one backend** — either via TOML or env vars.
2. **Wire into Claude Code** (or any MCP client):

   ```jsonc
   // ~/.claude.json
   {
     "mcpServers": {
       "tickets": {
         "command": "tickets-mcp"
       }
     }
   }
   ```

3. **Use from Claude Code:** `create_issue`, `search_issues`, `list_epics`,
   `transition_issue`, etc. The backend is selected by name or by the
   `default_backend` config setting.

## Configuration

`trusty-tickets` looks for config in this order:

1. `./.trusty-tickets/config.toml` (project)
2. `./.mcp-ticketer/config.json` (legacy compat)
3. `~/.trusty-tickets/config.toml` (user)

Env vars overlay file-based config on every load and can also auto-register
a backend when no config file exists.

### TOML example

```toml
default_backend = "work"

[backends.work]
backend = "github"
owner = "my-org"
repo = "my-repo"
# token can be set here or via $GITHUB_TOKEN / `gh auth token`

[backends.jira]
backend = "jira"
server = "https://my-org.atlassian.net"
email = "me@example.com"
project_key = "PROJ"
# api_token from $JIRA_API_TOKEN

[backends.linear]
backend = "linear"
team_key = "ENG"
# api_key from $LINEAR_API_KEY
```

### Environment variables

| Env var               | Backend  | Purpose                                |
|-----------------------|----------|----------------------------------------|
| `TICKETS_BACKEND`     | all      | Default backend name                   |
| `GITHUB_TOKEN`        | github   | PAT (or `gh auth token` is auto-tried) |
| `GITHUB_OWNER`        | github   | Repository owner                       |
| `GITHUB_REPO`         | github   | Repository name                        |
| `JIRA_SERVER`         | jira     | Cloud base URL                         |
| `JIRA_EMAIL`          | jira     | Account email                          |
| `JIRA_API_TOKEN`      | jira     | API token                              |
| `JIRA_PROJECT_KEY`    | jira     | Default project key                    |
| `LINEAR_API_KEY`      | linear   | API key                                |
| `LINEAR_TEAM_KEY`     | linear   | Team key (e.g. `ENG`)                  |
| `LINEAR_TEAM_ID`      | linear   | Team UUID (alternative to key)         |

If no config file exists but `GITHUB_TOKEN`/`JIRA_API_TOKEN`/`LINEAR_API_KEY`
is set, the corresponding backend auto-registers under its canonical name
(`github`, `jira`, `linear`).

## Tool Surface

31 tools, all routed to the active backend:

- **Issues:** `create_issue`, `get_issue`, `update_issue`, `close_issue`,
  `reopen_issue`, `list_issues`, `search_issues`, `assign_issue`,
  `transition_issue`
- **Comments:** `add_comment`, `list_comments`, `update_comment`,
  `delete_comment`
- **Labels:** `list_labels`, `create_label`, `add_labels`, `remove_labels`
- **Milestones:** `list_milestones`, `create_milestone`, `close_milestone`,
  `get_milestone_issues`
- **Projects / epics:** `list_projects`, `get_project`, `list_epics`,
  `get_epic_issues`, `create_project_update`, `list_project_updates`
- **Workflow:** `list_states`, `list_teams`
- **Meta:** `list_backends`

Each tool accepts an optional `backend` argument naming which configured
backend to use (defaults to `default_backend`).

The authoritative list with JSON Schemas lives in
[`src/tools.rs`](src/tools.rs).

## Architecture

- **`api::config`** — TOML/JSON loader with env-var overlay and auto-register.
- **`api::models`** — canonical ticketing types (`Issue`, `Comment`,
  `Label`, `Milestone`, `Priority`, `State`, ...). Backend-specific fields
  are carried opaquely in `Issue::extra` so nothing is lost in translation.
- **`api::backends`** — `Backend` trait (async, one method per operation)
  plus three implementations:
  - `github` — REST v3 + the GraphQL Projects v2 surface for project ops.
  - `jira` — Cloud REST API v3 (and the new `/search/jql` endpoint).
  - `linear` — GraphQL over `api.linear.app/graphql`.
- **`api::client`** — `BackendClient` holds a `HashMap<String, Arc<dyn Backend>>`
  and resolves the right one per call.
- **`server`** — MCP JSON-RPC dispatch on top of
  `trusty_mcp_core::run_stdio_loop`.

Adding a new backend means: implement `Backend`, plug it into
`BackendClient::from_config`, and (if it has fundamentally new ops)
extend the trait.

## Design Notes

- **One trait, many dialects.** GitHub speaks REST, JIRA speaks REST,
  Linear speaks GraphQL. The trait normalises them into one vocabulary
  so the MCP layer never branches on backend.
- **Lossless extras.** Anything the canonical model can't represent
  (custom fields, status categories, board metadata) is preserved in
  `Issue::extra: serde_json::Value`. The model gets every datum the
  backend returns; the API surface stays small.
- **Env-first auth.** Tokens never need to live on disk — every backend
  can be configured purely from environment variables, which is the
  pattern CI tooling expects.
- **GitHub `gh` CLI fallback.** When `GITHUB_TOKEN` is absent, the
  backend can shell out to `gh auth token`, so the same MCP server
  works with whatever auth your terminal already has.

## Testing

```sh
cargo test -p trusty-tickets
```

Covers config loading (TOML/env/legacy JSON), canonical model
serialisation round-trips, and backend dispatch. Live API calls
require credentials and are out of scope for the default suite.

## License

Licensed under the [Elastic License 2.0](https://www.elastic.co/licensing/elastic-license).

## Repository

<https://github.com/bobmatnyc/trusty-common>
