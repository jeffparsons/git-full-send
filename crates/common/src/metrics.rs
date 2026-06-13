//! The durable metrics sink shared by the client and the server (issue #42).
//!
//! Every `git-full-send` operation appends one structured JSON object — a JSON
//! Lines record — to `<git-dir>/git-full-send/metrics.jsonl`, alongside the
//! existing `git-full-send/` server state. The record *shapes* live in the crate
//! that produces them (the client's per-sync record, the server's per-receive and
//! per-worktree-update records); this module owns the cross-cutting plumbing:
//! where the file lives, how a record is appended without lines interleaving, and
//! the shared context fields (timestamp, tool version).
//!
//! Metrics are **best-effort observability** (ADR-0013): [`record`] never fails
//! an operation — a write error is logged via `tracing::warn!` and swallowed.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Serialises appends within this process so the server's concurrent
/// connection threads can't interleave half-written lines into the sink.
///
/// This guards against *intra*-process races only; two `git-full-send` processes
/// writing one repo's sink concurrently is out of scope (ADR-0013).
static SINK_LOCK: Mutex<()> = Mutex::new(());

/// The metrics sink path for a repository, given its git dir:
/// `<git-dir>/git-full-send/metrics.jsonl`.
pub fn metrics_path(git_dir: &Path) -> PathBuf {
    git_dir.join("git-full-send").join("metrics.jsonl")
}

/// Milliseconds since the Unix epoch, for a record's `ts_unix_ms` field.
///
/// A clock set before 1970 (so `duration_since` underflows) yields `0` rather
/// than an error — a metrics timestamp is never worth failing for.
pub fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The tool version stamped into every record (the crate version).
pub fn tool_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Append `record` as one JSON line to the sink under `git_dir`, creating the
/// parent directory if needed.
///
/// The record is serialised to a `String` first and written with a single
/// `write_all` under [`SINK_LOCK`], so a whole line lands atomically with
/// respect to other threads in this process. Returns any I/O or serialisation
/// error to the caller; prefer [`record`] for the best-effort behaviour.
pub fn append(git_dir: &Path, record: &impl Serialize) -> std::io::Result<()> {
    use std::io::Write;

    let path = metrics_path(git_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut line = serde_json::to_string(record)?;
    line.push('\n');

    let _guard = SINK_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())
}

/// Best-effort [`append`]: write `record` to the sink under `git_dir`, logging a
/// warning instead of propagating on failure.
///
/// Metrics are observability, never load-bearing, so a sink that can't be
/// written must not fail the operation it describes (ADR-0013).
pub fn record(git_dir: &Path, record: &impl Serialize) {
    if let Err(error) = append(git_dir, record) {
        tracing::warn!(%error, "could not write metrics record");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[derive(Serialize)]
    struct Sample {
        kind: &'static str,
        n: u64,
    }

    #[test]
    fn metrics_path_is_under_the_git_dir_state_dir() {
        let path = metrics_path(Path::new("/repo/.git"));
        assert_eq!(path, Path::new("/repo/.git/git-full-send/metrics.jsonl"));
    }

    #[test]
    fn append_creates_the_dir_and_appends_one_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path();
        append(git_dir, &Sample { kind: "a", n: 1 }).unwrap();
        append(git_dir, &Sample { kind: "b", n: 2 }).unwrap();

        let contents = std::fs::read_to_string(metrics_path(git_dir)).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "one line per record");
        // Each line is independently parseable JSON.
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["kind"], "a");
        assert_eq!(second["n"], 2);
    }

    #[test]
    fn concurrent_appends_produce_intact_lines() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().to_path_buf();
        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 50;

        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let git_dir = git_dir.clone();
                scope.spawn(move || {
                    for n in 0..PER_THREAD {
                        append(
                            &git_dir,
                            &Sample {
                                kind: "x",
                                n: t * PER_THREAD + n,
                            },
                        )
                        .unwrap();
                    }
                });
            }
        });

        let contents = std::fs::read_to_string(metrics_path(&git_dir)).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len() as u64, THREADS * PER_THREAD);
        // Every line parses — i.e. no two writes interleaved within a line.
        for line in lines {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }
    }
}
