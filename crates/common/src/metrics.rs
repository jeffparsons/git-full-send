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
//!
//! Since ADR-0017 a record is also an *output*, not only a side-effect: the value
//! written here is the value the operation returns, and `--json` prints it
//! verbatim. Every record therefore opens with the shared [`Envelope`], which
//! carries the [`SCHEMA_VERSION`] a parser needs to interpret the rest.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Version of the record shapes written to the sink and printed by `--json`.
///
/// `1` was the flat, `tool_version`-only shape of ADR-0013; `2` (ADR-0017) groups
/// fields by layer and concern, adds this field, and adds the cost-explaining
/// counts. Lines written before the field existed are schema 1 by omission.
pub const SCHEMA_VERSION: u32 = 2;

/// The fields every record opens with, flattened into the record that carries it.
///
/// `kind` tags which shape the rest of the line has (`sync`, `receive`,
/// `update_worktree`, …), so the one sink file stays self-describing.
#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    /// Which record shape this line is.
    pub kind: &'static str,
    /// The record-shape version — [`SCHEMA_VERSION`] at the time of writing.
    pub schema: u32,
    /// When the operation finished, in milliseconds since the Unix epoch.
    pub ts_unix_ms: u64,
    /// The `git-full-send` version that produced the record.
    pub tool_version: &'static str,
}

impl Envelope {
    /// Stamp an envelope for a record of the given `kind`, with the timestamp
    /// taken now.
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            schema: SCHEMA_VERSION,
            ts_unix_ms: now_unix_millis(),
            tool_version: tool_version(),
        }
    }
}

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

/// The distribution of one numeric field across a set of records.
#[derive(Debug, Clone, Serialize)]
pub struct FieldStats {
    /// Dotted path to the field, e.g. `code.push_ms`.
    pub field: String,
    /// How many records carried it.
    pub count: usize,
    /// Median.
    pub p50: f64,
    /// 95th percentile — where the pain actually lives.
    pub p95: f64,
    /// Largest observed value.
    pub max: f64,
}

/// Aggregated statistics for one `kind` of record.
#[derive(Debug, Clone, Serialize)]
pub struct KindStats {
    /// The record kind (`sync`, `receive`, `update_worktree`, …).
    pub kind: String,
    /// How many records of this kind were aggregated.
    pub records: usize,
    /// Every numeric field found, sorted by name.
    pub fields: Vec<FieldStats>,
}

/// Aggregate a sink's records by kind, reporting p50/p95/max per numeric field.
///
/// Closes ADR-0013's deferred "analysis / reporting tooling": the numbers are in
/// the file, and telling an operator to write `jq` to find the slow ones is not
/// the same as answering the question.
///
/// Deliberately **schema-agnostic**: records are flattened to dotted keys and
/// every numeric leaf is aggregated, so this keeps working across a schema change
/// instead of needing to know each record's shape. Unparseable lines are skipped;
/// `last` (when given) keeps only the most recent N records of each kind.
pub fn aggregate(git_dir: &Path, kind: Option<&str>, last: Option<usize>) -> Vec<KindStats> {
    use std::collections::BTreeMap;

    let Ok(contents) = std::fs::read_to_string(metrics_path(git_dir)) else {
        return Vec::new();
    };

    // kind → field → values, in file order.
    let mut by_kind: BTreeMap<String, (usize, BTreeMap<String, Vec<f64>>)> = BTreeMap::new();
    let mut records_by_kind: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    for line in contents.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let record_kind = record
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("unknown")
            .to_string();
        if kind.is_some_and(|wanted| wanted != record_kind) {
            continue;
        }
        records_by_kind.entry(record_kind).or_default().push(record);
    }

    for (record_kind, mut records) in records_by_kind {
        if let Some(last) = last
            && records.len() > last
        {
            records.drain(..records.len() - last);
        }
        let entry = by_kind.entry(record_kind).or_default();
        entry.0 = records.len();
        for record in &records {
            flatten_numbers("", record, &mut entry.1);
        }
    }

    by_kind
        .into_iter()
        .map(|(kind, (records, fields))| KindStats {
            kind,
            records,
            fields: fields
                .into_iter()
                .map(|(field, mut values)| {
                    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    FieldStats {
                        field,
                        count: values.len(),
                        p50: percentile(&values, 0.50),
                        p95: percentile(&values, 0.95),
                        max: values.last().copied().unwrap_or(0.0),
                    }
                })
                .collect(),
        })
        .collect()
}

/// Collect every numeric leaf of `value` into `out`, keyed by dotted path.
///
/// Timestamps are skipped: aggregating `ts_unix_ms` would report a percentile of
/// wall-clock instants, which is noise dressed as a statistic.
fn flatten_numbers(
    prefix: &str,
    value: &serde_json::Value,
    out: &mut std::collections::BTreeMap<String, Vec<f64>>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == "ts_unix_ms" || key == "schema" {
                    continue;
                }
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_numbers(&path, child, out);
            }
        }
        serde_json::Value::Number(n) => {
            if let Some(n) = n.as_f64() {
                out.entry(prefix.to_string()).or_default().push(n);
            }
        }
        // Arrays (e.g. `refs_updated`) contribute their length, which is the
        // aggregatable thing about them.
        serde_json::Value::Array(items) => {
            out.entry(format!("{prefix}.len"))
                .or_default()
                .push(items.len() as f64);
        }
        _ => {}
    }
}

/// The `q`-quantile of a **sorted** slice, by nearest rank.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (q * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
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

    /// Aggregation answers "which phase is slow" without anyone writing `jq`,
    /// and does it without knowing any record's shape (ADR-0017).
    #[test]
    fn aggregate_reports_percentiles_per_kind_and_nested_field() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path();
        // Ten syncs whose nested `code.push_ms` runs 10, 20, … 100, plus a record
        // of a different kind that must not be mixed in.
        for i in 1..=10 {
            let record = serde_json::json!({
                "kind": "sync",
                "ts_unix_ms": 1_000_000 + i,
                "total_ms": (i * 10) as f64,
                "code": { "push_ms": (i * 10) as f64, "files_overlaid": i },
                "refs": ["a", "b"],
            });
            append(git_dir, &record).unwrap();
        }
        append(
            git_dir,
            &serde_json::json!({ "kind": "receive", "duration_ms": 5.0 }),
        )
        .unwrap();

        let all = aggregate(git_dir, None, None);
        assert_eq!(all.len(), 2, "one entry per kind");

        let sync = all.iter().find(|k| k.kind == "sync").expect("sync stats");
        assert_eq!(sync.records, 10);
        let push = sync
            .fields
            .iter()
            .find(|f| f.field == "code.push_ms")
            .expect("nested fields are flattened to dotted keys");
        assert_eq!(push.count, 10);
        assert_eq!(push.p50, 60.0);
        assert_eq!(push.p95, 100.0);
        assert_eq!(push.max, 100.0);
        // Arrays contribute their length, which is the aggregatable thing.
        assert!(sync.fields.iter().any(|f| f.field == "refs.len"));
        // A timestamp percentile would be noise dressed as a statistic.
        assert!(!sync.fields.iter().any(|f| f.field == "ts_unix_ms"));

        // Filtering by kind, and windowing to the most recent N.
        let recent = aggregate(git_dir, Some("sync"), Some(3));
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].records, 3, "only the last three syncs");
        let push = recent[0]
            .fields
            .iter()
            .find(|f| f.field == "code.push_ms")
            .unwrap();
        assert_eq!(push.max, 100.0);
        assert_eq!(push.p50, 90.0, "the last three are 80, 90, 100");
    }

    /// A sink that doesn't exist, or holds a truncated line, must not panic —
    /// it is an append-only file a process may be writing right now.
    #[test]
    fn aggregate_tolerates_a_missing_sink_and_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        assert!(aggregate(dir.path(), None, None).is_empty());

        let path = metrics_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"kind\":\"sync\",\"total_ms\":1.0}\n{\"kind\":\"sy",
        )
        .unwrap();
        let stats = aggregate(dir.path(), None, None);
        assert_eq!(stats.len(), 1, "the intact record still counts");
        assert_eq!(stats[0].records, 1);
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
