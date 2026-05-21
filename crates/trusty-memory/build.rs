//! build.rs — compiles the Svelte admin UI bundle before the Rust build.
//!
//! Why: `src/web.rs` embeds the compiled UI via `rust_embed` from
//! `$CARGO_MANIFEST_DIR/ui/dist/`. For `rust-embed` to pick up fresh assets,
//! `ui/dist/` must be rebuilt whenever the UI sources change. This script
//! shells out to the project's package manager (`pnpm`, falling back to
//! `npm`) to run the Vite build.
//! What: emits `cargo:rerun` directives for UI source changes, then either
//! honours `SKIP_UI_BUILD=1` (CI / `cargo publish` flow where `ui/dist/` is
//! already populated) or runs `<pm> install` + `<pm> run build` inside `ui/`.
//! If no package manager is available, or the build fails, it emits a
//! placeholder `ui/dist/index.html` so `RustEmbed` still compiles and warns
//! loudly.
//! Test: `SKIP_UI_BUILD=1 cargo check -p trusty-memory` exits without invoking
//! any JS toolchain; a plain `cargo build` on a host with pnpm/npm installed
//! populates `ui/dist/index.html`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let crate_root = crate_root();
    let ui_dir = crate_root.join("ui");
    let ui_dist = ui_dir.join("dist");

    emit_rerun_directives();

    // Honour explicit skip (CI / `cargo publish --verify`).
    if skip_requested() {
        handle_skip(&ui_dist);
        return;
    }

    // No `ui/` tree at all (e.g. running build.rs inside an extracted crate
    // tarball that already shipped `ui/dist/`) → nothing to build.
    if !has_ui_sources(&ui_dir) {
        ensure_placeholder(&ui_dist);
        return;
    }

    build_ui(&ui_dir, &ui_dist);
}

/// Resolve the crate root from Cargo's environment.
fn crate_root() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default())
}

/// Tell Cargo to re-run this build script when any UI source changes (or the
/// `SKIP_UI_BUILD` flag flips).
fn emit_rerun_directives() {
    println!("cargo:rerun-if-env-changed=SKIP_UI_BUILD");
    println!("cargo:rerun-if-changed=ui/package.json");
    println!("cargo:rerun-if-changed=ui/vite.config.js");
    println!("cargo:rerun-if-changed=ui/index.html");
    println!("cargo:rerun-if-changed=ui/src");
}

/// True when the operator opted out of the JS build (`SKIP_UI_BUILD=1`).
fn skip_requested() -> bool {
    std::env::var("SKIP_UI_BUILD").as_deref() == Ok("1")
}

/// Skip-flag branch: emit a warning if `ui/dist/` is empty, then ensure the
/// stub `index.html` exists so `RustEmbed` still compiles.
fn handle_skip(ui_dist: &Path) {
    if !ui_dist.join("index.html").exists() {
        println!(
            "cargo:warning=SKIP_UI_BUILD=1 but ui/dist/ is empty. \
             Run `pnpm -C ui build` before publishing."
        );
        ensure_placeholder(ui_dist);
    }
}

/// True when the crate has a `ui/package.json` (i.e. it's a working copy, not
/// an extracted publish tarball).
fn has_ui_sources(ui_dir: &Path) -> bool {
    ui_dir.join("package.json").exists()
}

/// Detect the package manager: prefer `pnpm` (the project's lockfile format),
/// fall back to `npm`. The `ui_dir` argument is currently unused — pnpm is
/// preferred whenever it is installed — but kept for symmetry with a possible
/// future per-lockfile selection.
fn package_manager(_ui_dir: &Path) -> Option<&'static str> {
    let has = |bin: &str| {
        Command::new(bin)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if has("pnpm") {
        Some("pnpm")
    } else if has("npm") {
        Some("npm")
    } else {
        None
    }
}

/// Run `<pm> install` followed by `<pm> run build` inside `ui/`. Any failure
/// is downgraded to a placeholder `ui/dist/index.html` plus a `cargo:warning`
/// so the Rust build still completes.
fn build_ui(ui_dir: &Path, ui_dist: &Path) {
    let Some(pm) = package_manager(ui_dir) else {
        println!(
            "cargo:warning=no pnpm/npm found; cannot build trusty-memory UI. \
             Run `pnpm -C ui build` manually or set SKIP_UI_BUILD=1."
        );
        ensure_placeholder(ui_dist);
        return;
    };

    let install = Command::new(pm).arg("install").current_dir(ui_dir).status();
    if !matches!(install, Ok(s) if s.success()) {
        println!("cargo:warning=`{pm} install` failed for trusty-memory UI");
        ensure_placeholder(ui_dist);
        return;
    }

    let build = Command::new(pm)
        .args(["run", "build"])
        .current_dir(ui_dir)
        .status();
    match build {
        Ok(s) if s.success() => {}
        Ok(s) => {
            println!("cargo:warning=`{pm} run build` exited with {s:?}");
            ensure_placeholder(ui_dist);
        }
        Err(e) => {
            println!("cargo:warning=failed to spawn `{pm} run build` ({e})");
            ensure_placeholder(ui_dist);
        }
    }
}

/// Emit a stub `ui/dist/index.html` so `RustEmbed` still compiles even when
/// the JS build did not run.
fn ensure_placeholder(ui_dist: &Path) {
    if ui_dist.join("index.html").exists() {
        return;
    }
    let _ = std::fs::create_dir_all(ui_dist);
    let _ = std::fs::write(
        ui_dist.join("index.html"),
        "<!doctype html><html><body><p>trusty-memory: UI assets not built. \
         Run <code>pnpm -C ui build</code> and rebuild.</p></body></html>",
    );
}
