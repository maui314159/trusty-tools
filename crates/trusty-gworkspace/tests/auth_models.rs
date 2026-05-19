//! Integration test: round-trip a real-world tokens.json fixture through
//! the `StoredToken` model.
//!
//! Why: This proves the Rust port can read tokens.json files written by
//! the Python `gworkspace-mcp setup` CLI.
//! What: Writes a temp tokens.json with two profiles, loads via
//! `TokenStorage::with_path`, asserts default-profile resolution.
//! Test: this file.

use std::collections::HashMap;

use trusty_gworkspace::api::auth::{StoredToken, TokenStorage};

const FIXTURE: &str = r#"{
  "primary": {
    "version": 1,
    "metadata": {
      "service_name": "primary",
      "provider": "google",
      "created_at": "2024-01-01T00:00:00Z",
      "last_refreshed": null,
      "email": "first@example.com",
      "is_default": true
    },
    "token": {
      "access_token": "first-access",
      "refresh_token": "first-refresh",
      "expires_at": "2099-01-01T00:00:00Z",
      "scopes": ["https://www.googleapis.com/auth/calendar"],
      "token_type": "Bearer"
    }
  },
  "secondary": {
    "version": 1,
    "metadata": {
      "service_name": "secondary",
      "provider": "google",
      "created_at": "2024-01-02T00:00:00Z",
      "last_refreshed": null,
      "email": "second@example.com",
      "is_default": false
    },
    "token": {
      "access_token": "second-access",
      "refresh_token": "second-refresh",
      "expires_at": "2099-01-01T00:00:00Z",
      "scopes": ["https://www.googleapis.com/auth/gmail.modify"],
      "token_type": "Bearer"
    }
  }
}"#;

#[test]
fn stored_token_map_round_trips() {
    let parsed: HashMap<String, StoredToken> = serde_json::from_str(FIXTURE).expect("parse");
    assert_eq!(parsed.len(), 2);
    assert!(parsed.contains_key("primary"));
    assert_eq!(
        parsed["primary"].metadata.email.as_deref(),
        Some("first@example.com")
    );
    assert!(parsed["primary"].metadata.is_default);
    assert!(!parsed["secondary"].metadata.is_default);
}

#[test]
fn token_storage_resolves_default() {
    let dir = tempdir();
    let path = dir.join("tokens.json");
    std::fs::write(&path, FIXTURE).expect("write fixture");

    let storage = TokenStorage::with_path(path);
    let accounts = storage.list_accounts().expect("list");
    assert_eq!(accounts.len(), 2);

    let default = storage.get_default().expect("get_default");
    let default = default.expect("default present");
    assert_eq!(default.metadata.email.as_deref(), Some("first@example.com"));
    assert!(default.metadata.is_default);

    let secondary = storage
        .get_profile("secondary")
        .expect("get_profile")
        .expect("secondary present");
    assert_eq!(
        secondary.metadata.email.as_deref(),
        Some("second@example.com")
    );
}

fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "trusty-gworkspace-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).expect("mkdir");
    p
}
