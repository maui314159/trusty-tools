//! Build script exposing git commit metadata to the binary.
//!
//! Why: Users of the binary benefit from knowing which exact commit produced
//! the running artifact; pairing `CARGO_PKG_VERSION` with the short git SHA
//! gives a deterministic identifier for bug reports and log correlation.
//! What: Queries `git rev-parse --short HEAD` at build time and exposes it
//! as the `GIT_COMMIT_HASH` environment variable to the compiled crate via
//! `cargo:rustc-env`. Falls back to `"unknown"` when git isn't available or
//! the directory isn't a git repo.
//! Test: After `cargo build`, `build_info::GIT_HASH` should be non-empty and
//! either a 7-char short hash or `"unknown"`.

fn main() {
    // Re-run whenever HEAD moves so a new commit triggers a rebuild.
    println!("cargo:rerun-if-changed=.git/HEAD");

    // Re-run whenever the built UI bundle changes so rust-embed re-inlines fresh
    // assets. We watch `ui/dist/index.html` (the output) rather than `ui/src/`
    // because cargo's `rerun-if-changed` only tracks the directory inode, not
    // recursive file mutations — edits to `.svelte`/`.ts` files inside `ui/src/`
    // wouldn't trigger a rebuild and stale assets would stay embedded. Watching
    // the build output is reliable: every `pnpm build` regenerates `index.html`,
    // and the `pnpm build` invocation below runs on every cargo build anyway,
    // so cargo will pick up the resulting change on the subsequent build cycle.
    println!("cargo:rerun-if-changed=ui/dist/index.html");
    println!("cargo:rerun-if-changed=ui/index.html");
    println!("cargo:rerun-if-changed=ui/package.json");

    // Build the Vite frontend so ui/dist/ exists for rust-embed to inline.
    // Skip when SKIP_UI_BUILD=1 (cargo publish --dry-run / CI publish flows)
    // or when pnpm is not available (CI environments that only need the API
    // binary without the web UI).
    let skip_ui = std::env::var("SKIP_UI_BUILD").as_deref() == Ok("1");
    if skip_ui {
        println!("cargo:warning=SKIP_UI_BUILD=1 — web UI build skipped");
    } else if std::process::Command::new("pnpm")
        .arg("--version")
        .output()
        .is_err()
    {
        println!("cargo:warning=pnpm not found — web UI will not be embedded");
    } else {
        if !std::path::Path::new("ui/node_modules").exists() {
            // Use --no-frozen-lockfile when pnpm-lock.yaml is absent (e.g.
            // inside a cargo publish verification sandbox where only
            // git-tracked files are present).
            let lockfile_exists = std::path::Path::new("ui/pnpm-lock.yaml").exists();
            let install_args: &[&str] = if lockfile_exists {
                &["install", "--frozen-lockfile"]
            } else {
                &["install", "--no-frozen-lockfile"]
            };
            let s = std::process::Command::new("pnpm")
                .args(install_args)
                .current_dir("ui")
                .status()
                .unwrap();
            assert!(s.success(), "pnpm install failed");
        }
        let s = std::process::Command::new("pnpm")
            .arg("build")
            .current_dir("ui")
            .status()
            .unwrap();
        assert!(s.success(), "pnpm build failed");
    }

    let git_hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_COMMIT_HASH={git_hash}");
}
