//! End-to-end config loading via env vars.
//!
//! Why: The dispatcher relies on `Config::load()` finding the right
//! backends from `.toml` files and env-var fallbacks; we want a smoke
//! test that doesn't depend on real network creds.
//! What: Asserts defaults + that env-var-derived backends are registered.
//! Test: this file IS the test.

use trusty_tickets::api::config::{BackendConfig, Config};

#[test]
fn default_config_has_no_backends() {
    let cfg = Config::default();
    assert!(cfg.backends.is_empty());
    assert!(cfg.default_backend.is_none());
}

#[test]
fn linear_config_with_env_fills_api_key() {
    // SAFETY: the test sets env vars only for this process; harmless if
    // re-run sequentially.
    // Wrap in unsafe blocks per Rust 2024 edition rule.
    unsafe {
        std::env::set_var("LINEAR_API_KEY", "lin_test_key_xxx");
        std::env::set_var("LINEAR_TEAM_KEY", "ENG");
    }
    let lc = trusty_tickets::api::config::LinearConfig::default().with_env();
    assert_eq!(lc.api_key.as_deref(), Some("lin_test_key_xxx"));
    assert_eq!(lc.team_key.as_deref(), Some("ENG"));
    unsafe {
        std::env::remove_var("LINEAR_API_KEY");
        std::env::remove_var("LINEAR_TEAM_KEY");
    }
}

#[test]
fn parse_full_toml_with_all_backends() {
    let src = r#"
        default_backend = "linear"

        [backends.github]
        backend = "github"
        owner = "octocat"
        repo = "hello"
        token = "ghp_xxx"

        [backends.jira]
        backend = "jira"
        server = "https://x.atlassian.net"
        email = "me@example.com"
        api_token = "j_xxx" # pragma: allowlist secret
        project_key = "ENG"

        [backends.linear]
        backend = "linear"
        api_key = "lin_xxx" # pragma: allowlist secret
        team_key = "ENG"
    "#;
    let cfg: Config = toml::from_str(src).expect("parse");
    assert_eq!(cfg.default_backend.as_deref(), Some("linear"));
    assert_eq!(cfg.backends.len(), 3);
    for (name, bc) in &cfg.backends {
        match (name.as_str(), bc) {
            ("github", BackendConfig::Github(_)) => {}
            ("jira", BackendConfig::Jira(_)) => {}
            ("linear", BackendConfig::Linear(_)) => {}
            (n, _) => panic!("unexpected variant for backend {n}"),
        }
    }
}
