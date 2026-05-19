## Ticket Source
GitHub Issues is the ticket source. Use `gh issue list`, `gh issue view`, `gh issue create` via version-control or ticketing agent.

## trusty-tools Monorepo Workflow

This is a unified Cargo workspace. Key rules:

### No [patch.crates-io] needed
Internal crates resolve via path deps. Never add `[patch.crates-io]` for internal crates.

### Required Workflow Sequence
(prompt → ticket) OR (check tickets) → read ticket → implement → test → build → version bump → publish → update consumers → verify CI

### Phase 0: Ticket
No work begins without a ticket reference.

### Phase 1: Read Ticket
Always read full ticket + comments before writing code.

### Phase 2: Implement
- Agent: rust-engineer (model: opus)
- No `unwrap()` in library code
- `thiserror` for crates, `anyhow` for binaries
- Why/What/Test doc pattern on every public item

### Phase 3: Test
- `cargo test --workspace` — all green
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo fmt --check` — clean

### Phase 4: Version Bump + Publish
- Semver: `feat` → minor, `fix`/`chore` → patch, `BREAKING` → major
- Tag format: `<crate-name>-v<version>`
- Publish: `cd crates/<name> && cargo publish`

### Commit Format
`feat|fix|chore|refactor|test|docs(<scope>): <description> (closes #N)`

### Cross-crate changes
When changing a shared library crate, update all crates in this workspace that depend on it. No separate repo coordination needed — everything is here.

### Former repos
The following repos are now READ-ONLY and point here: trusty-common, trusty-search, trusty-memory, trusty-analyze, trusty-git-analytics, trusty-mpm, open-mpm.
