//! Handlers for `trusty-memory monitor status` and `monitor palaces`.
//!
//! Why: the `monitor web` / `monitor tui` subcommands surface the daemon
//! dashboard interactively, but scripts and CI need the same numbers as plain
//! text or JSON without launching a TUI. These handlers expose the daemon's
//! aggregate health and per-palace stats as scriptable output (issue #33).
//! What: `handle_status` prints daemon version and aggregate counts;
//! `handle_palaces` prints either a table of every palace or a single palace's
//! detail. Both accept a `--json` flag and exit 1 (via `Err`) when the daemon
//! is unreachable.
//! Test: unit tests cover `fmt_count`; live behaviour is exercised by
//! `cargo run -p trusty-memory -- monitor status` against a running daemon.

use anyhow::{bail, Result};
use trusty_common::monitor::dashboard::{MemoryData, PalaceRow};
use trusty_common::monitor::memory_client::{resolve_memory_url, MemoryClient};

/// Format a count with comma thousands separators (`8400` → `"8,400"`).
///
/// Why: vector and drawer counts in the plain-text table read far easier
/// grouped; an exact comma-grouped form keeps precision a script may want.
/// What: returns the decimal string of `n` with a `,` inserted every three
/// digits from the right.
/// Test: `fmt_count_groups_thousands`.
fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

/// Fetch the full trusty-memory dashboard payload or fail with a clear error.
///
/// Why: every monitor subcommand needs the same status + palace snapshot; this
/// centralises the daemon-URL resolution and the unreachable-daemon error so
/// each handler stays terse.
/// What: resolves the daemon URL from the service lock file (falling back to
/// the default port), then calls `MemoryClient::fetch_all`. A transport error
/// becomes an `Err` so `main()` exits 1.
/// Test: covered indirectly by the handler tests; the live path needs a daemon.
async fn fetch_memory_data() -> Result<MemoryData> {
    let url = resolve_memory_url();
    let client = MemoryClient::new(url.clone());
    client
        .fetch_all()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach trusty-memory daemon at {url}: {e}"))
}

/// Print daemon status: version and aggregate palace/drawer/vector counts.
///
/// Why: the quickest scriptable health check — "is the daemon up and how big
/// are its palaces" — without parsing a table or launching the TUI.
/// What: fetches the dashboard payload and prints either a JSON object or a
/// plain-text summary. Returns `Err` when the daemon is unreachable.
/// Test: `cargo run -p trusty-memory -- monitor status` against a live daemon.
pub async fn handle_status(json: bool) -> Result<()> {
    let data = fetch_memory_data().await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "online",
                "version": data.version,
                "palace_count": data.palace_count,
                "total_drawers": data.total_drawers,
                "total_vectors": data.total_vectors,
                "total_kg_triples": data.total_kg_triples,
            })
        );
    } else {
        println!("trusty-memory  v{}", data.version);
        println!("status:        online");
        println!("palaces:       {}", data.palace_count);
        println!("drawers:       {}", fmt_count(data.total_drawers));
        println!("vectors:       {}", fmt_count(data.total_vectors));
        println!("kg triples:    {}", fmt_count(data.total_kg_triples));
    }
    Ok(())
}

/// Print the palace table, or a single palace's detail when `id` is given.
///
/// Why: operators want the same per-palace vector-count view the TUI shows,
/// but reachable from a shell pipeline.
/// What: with no `id`, prints an `ID / NAME / VECTORS` table (or a JSON array);
/// with an `id`, prints that one palace's detail (or a JSON object), failing
/// with a clear error when the id is not registered.
/// Test: `cargo run -p trusty-memory -- monitor palaces` against a live daemon.
pub async fn handle_palaces(id: Option<String>, json: bool) -> Result<()> {
    let data = fetch_memory_data().await?;
    match id {
        Some(id) => print_palace_detail(&data.palaces, &id, json),
        None => {
            print_palace_table(&data.palaces, json);
            Ok(())
        }
    }
}

/// Render every palace as a JSON array or an aligned plain-text table.
///
/// Why: shared by `handle_palaces` for the list case; isolating it keeps the
/// handler's branching readable.
/// What: emits a JSON array of `{id, name, vectors}` objects when `json`,
/// otherwise a header row plus one aligned row per palace.
/// Test: side-effect-only (prints); the alignment is verified via the live
/// command.
fn print_palace_table(palaces: &[PalaceRow], json: bool) {
    if json {
        let arr: Vec<serde_json::Value> = palaces
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "vectors": p.vector_count,
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(arr));
        return;
    }

    if palaces.is_empty() {
        println!("(no palaces)");
        return;
    }

    let id_w = palaces
        .iter()
        .map(|p| p.id.len())
        .max()
        .unwrap_or(0)
        .max(12);
    let name_w = palaces
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(0)
        .max(12);
    println!("{:<id_w$}  {:<name_w$}  VECTORS", "ID", "NAME");
    for p in palaces {
        println!(
            "{:<id_w$}  {:<name_w$}  {}",
            p.id,
            p.name,
            fmt_count(p.vector_count),
        );
    }
}

/// Render one palace's detail as a JSON object or plain-text lines.
///
/// Why: shared by `handle_palaces` for the single-id case.
/// What: looks up `id` in `palaces`; on a hit prints the detail, on a miss
/// returns an `Err` listing the unknown id so `main()` exits 1.
/// Test: side-effect-only (prints); the not-found path is covered by the live
/// command behaviour.
fn print_palace_detail(palaces: &[PalaceRow], id: &str, json: bool) -> Result<()> {
    let Some(row) = palaces.iter().find(|p| p.id == id) else {
        bail!("no palace named '{id}' is registered");
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "id": row.id,
                "name": row.name,
                "vectors": row.vector_count,
            })
        );
    } else {
        println!("id:       {}", row.id);
        println!("name:     {}", row.name);
        println!("vectors:  {}", fmt_count(row.vector_count));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_count_groups_thousands() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(42), "42");
        assert_eq!(fmt_count(8_400), "8,400");
        assert_eq!(fmt_count(1_234_567), "1,234,567");
    }

    #[test]
    fn print_palace_detail_errors_on_unknown_id() {
        // Why: PalaceRow gained `drawer_count`, `last_write_at`, and
        // `description` fields after this test was written. Use struct-
        // update syntax with `Default::default()` so future additions don't
        // re-break the test.
        let rows = vec![PalaceRow {
            id: "default".into(),
            name: "default".into(),
            vector_count: 8_400,
            ..Default::default()
        }];
        assert!(print_palace_detail(&rows, "missing", false).is_err());
        assert!(print_palace_detail(&rows, "default", true).is_ok());
    }
}
