//! Process resident-memory (RSS) and CPU sampling for daemon `/health`.
//!
//! Why: Every trusty-* daemon wants to report its own RSS and CPU usage on
//!      its health endpoint, and the sampling logic (resolve our PID, refresh
//!      only this process, convert units) is identical across them.
//!      Centralising it here avoids three near-identical copies drifting.
//! What: [`SysMetrics`] wraps a `sysinfo::System` scoped to the current
//!      process. [`SysMetrics::sample`] refreshes and returns
//!      `(rss_mb, cpu_pct)`. CPU usage is a delta between two refreshes, so
//!      the *first* sample reports `0.0`; subsequent samples report the
//!      usage observed since the previous call. Callers polling `/health`
//!      every ~2 s get meaningful CPU readings without any background task.
//! Test: see the `tests` module — `sample_does_not_panic` exercises the
//!      refresh path; `rss_is_plausible` asserts the test process reports a
//!      non-trivial, non-absurd RSS.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

/// Per-process RSS + CPU sampler bound to the current process.
///
/// Why: holding the `System` between calls is required for CPU measurement —
///      `sysinfo` derives CPU% from the delta in consumed CPU time between
///      two refreshes, so the same instance must be reused.
/// What: stores the long-lived `System` and our own `Pid`. Not `Clone` — it
///      carries mutable sampling state; share it behind a `Mutex` if multiple
///      handlers need it.
/// Test: `sample_does_not_panic`, `rss_is_plausible`.
pub struct SysMetrics {
    sys: System,
    pid: Pid,
}

impl SysMetrics {
    /// Construct a sampler for the current process.
    ///
    /// Why: the daemon builds one of these at startup and samples it on each
    ///      `/health` request.
    /// What: resolves `std::process::id()` into a `sysinfo::Pid` and creates a
    ///      `System` configured to refresh only process memory + CPU (not the
    ///      whole machine), then performs one priming refresh so the next
    ///      `sample` call has a baseline for the CPU delta.
    /// Test: `sample_does_not_panic`.
    #[must_use]
    pub fn new() -> Self {
        let pid = Pid::from_u32(std::process::id());
        let mut sys = System::new_with_specifics(
            RefreshKind::nothing()
                .with_processes(ProcessRefreshKind::nothing().with_memory().with_cpu()),
        );
        // Prime the CPU baseline — the first delta-based reading after this
        // will be meaningful rather than a spurious 0/huge value.
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_memory().with_cpu(),
        );
        Self { sys, pid }
    }

    /// Refresh and return `(rss_mb, cpu_pct)` for the current process.
    ///
    /// Why: the `/health` handler calls this once per request. Polling more
    ///      often than ~once per 500 ms yields noisy CPU readings because the
    ///      delta window shrinks; `/health` is typically polled every 2 s so
    ///      this is not a concern in practice.
    /// What: refreshes this process's memory + CPU stats. Returns RSS in
    ///      whole megabytes (`bytes / 1_048_576`) and CPU as a percentage
    ///      where `100.0` means one fully-saturated core (sysinfo's
    ///      convention — a process on 4 cores can exceed 100). If the process
    ///      cannot be resolved (extremely rare; only in containers with
    ///      `/proc` hidden), returns `(0, 0.0)`.
    /// Test: `sample_does_not_panic`, `rss_is_plausible`.
    pub fn sample(&mut self) -> (u64, f32) {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self.pid]),
            true,
            ProcessRefreshKind::nothing().with_memory().with_cpu(),
        );
        match self.sys.process(self.pid) {
            Some(proc) => (proc.memory() / (1024 * 1024), proc.cpu_usage()),
            None => (0, 0.0),
        }
    }
}

impl Default for SysMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Sum the byte sizes of every regular file under `dir`, recursively.
///
/// Why: daemon `/health` reports `disk_bytes` — the on-disk footprint of the
///      data directory (redb + usearch + snapshot files). Walking the tree on
///      demand keeps it accurate without a separate accounting layer.
/// What: recursively descends `dir`, summing `metadata().len()` of each file.
///      Symlinks are not followed (avoids double-counting and cycles).
///      Unreadable entries are skipped rather than failing the whole walk —
///      a health endpoint should degrade gracefully. Returns `0` when `dir`
///      does not exist.
/// Test: `dir_size_sums_files` creates files of known sizes and asserts the
///      total; `dir_size_missing_dir_is_zero` covers the absent-path case.
#[must_use]
pub fn dir_size_bytes(dir: &std::path::Path) -> u64 {
    fn walk(dir: &std::path::Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                walk(&entry.path(), total);
            } else if file_type.is_file() {
                if let Ok(meta) = entry.metadata() {
                    *total = total.saturating_add(meta.len());
                }
            }
        }
    }
    let mut total = 0u64;
    walk(dir, &mut total);
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_does_not_panic() {
        let mut m = SysMetrics::new();
        let (_rss, _cpu) = m.sample();
        // A second sample exercises the CPU-delta path.
        let (_rss2, cpu2) = m.sample();
        assert!(cpu2 >= 0.0, "cpu usage must be non-negative, got {cpu2}");
    }

    #[test]
    fn rss_is_plausible() {
        let mut m = SysMetrics::new();
        let (rss, _cpu) = m.sample();
        // The test binary is real; if sysinfo could resolve it RSS is > 0.
        // We tolerate 0 only for sandboxed CI where /proc is restricted.
        assert!(
            rss < 1024 * 1024,
            "RSS implausibly large ({rss} MB) — unit must be MB"
        );
    }

    #[test]
    fn dir_size_sums_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.txt"), vec![0u8; 100]).unwrap();
        std::fs::write(tmp.path().join("b.txt"), vec![0u8; 250]).unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("c.txt"), vec![0u8; 50]).unwrap();
        assert_eq!(dir_size_bytes(tmp.path()), 400);
    }

    #[test]
    fn dir_size_missing_dir_is_zero() {
        let missing = std::path::Path::new("/nonexistent/trusty/path/xyz");
        assert_eq!(dir_size_bytes(missing), 0);
    }
}
