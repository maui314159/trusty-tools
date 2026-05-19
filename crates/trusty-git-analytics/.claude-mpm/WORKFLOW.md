# trusty-git-analytics Dev Workflow

## Required Workflow Sequence

(prompt → ticket) OR (check tickets) → read ticket + comments → implement → test → build → **patch bump → install binary → smoke test** → verify CI/CD to crates.io

> `tga` is a CLI tool, not a daemon — no daemon restart phase.

## Phase Definitions

### Phase 0: Ticket
**Either:** user provides a prompt → create a GitHub issue capturing requirements and acceptance criteria
**Or:** check existing open tickets → pick the next item to work on

No work begins without a ticket reference.

### Phase 1: Read Ticket
- Read the full ticket body AND all comments
- Understand acceptance criteria completely before writing any code
- Agent: ticketing_agent (to fetch issue + comments)

### Phase 2: Implement
- Agent: rust-engineer
- Write code satisfying all acceptance criteria
- Follow coding rules: no `unwrap()` in library code, `thiserror` for crates, `anyhow` for binary

### Phase 3: Test
- Agent: rust-engineer (inline) or qa
- Run: `cargo test --workspace`
- Run: `cargo clippy --workspace --all-targets -- -D warnings`
- Run: `cargo fmt --check`
- Must show raw test output before proceeding
- All tests green, clippy clean, fmt clean → proceed; else fix and re-run

### Phase 4: Build
- Agent: local-ops
- Run: `cargo build --release`
- Confirms release binary compiles cleanly
- May be skipped if Phase 3 already ran a release build internally

### Phase 5: Patch Bump
- Agent: local-ops
- Run: `make patch` (or bump Cargo.toml, commit, tag `v<version>`)
- Commit message format: `feat|fix|chore|test(<scope>): <description> (closes #N)`

### Phase 6: Install Binary (MANDATORY — never skip)
- Agent: local-ops
- Install the new binary:
  ```bash
  cargo install --path . --locked
  ```
- Verify binary version matches patch bump: `tga --version`

### Phase 7: Smoke Test (MANDATORY — never skip)
- Agent: local-ops or qa
- Run a basic command to confirm the binary works end-to-end:
  ```bash
  tga --help
  tga version
  ```
- Any crash or unexpected output is a blocker

### Phase 8: Verify CI/CD
- Agent: local-ops or version-control
- Confirm GitHub Actions publish workflow triggered on the new tag
- Check workflow run status: `gh run list --repo bobmatnyc/trusty-git-analytics --limit 5`
- Confirm crates.io publish job passed (or dry-run passed if not a release tag)

## Skip Rules
- Phase 4 (build) may be skipped if Phase 3 already ran `cargo build` internally
- Phase 5 (patch) may be skipped for chore/docs-only changes with no binary impact
- Phase 6 (install) is **NEVER skipped** — the local binary must always be the latest version
- Phase 7 (smoke test) is **NEVER skipped** — must confirm the installed binary is healthy
- Phase 8 (CI/CD verify) may be skipped for non-tagged commits
- Phase 1 (ticket) is always required — no work without a ticket reference

## Commit Message Format
feat|fix|chore|refactor|test|docs(<scope>): <description> (closes #N)

## Success Criteria
All phases green → ticket closed on GitHub
