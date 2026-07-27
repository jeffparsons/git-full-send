//! Harvesting `git`'s own instrumentation from a child process (ADR-0017).
//!
//! An outer wall-clock timing says *that* a `git` step was slow; it cannot say
//! which part of it was. `git` already knows — it emits region timings and
//! counters through **trace2** — so where a single `git` child dominates a phase
//! we point `GIT_TRACE2_EVENT` at a per-invocation temp file and read the answer
//! back out afterwards.
//!
//! For `read-tree --reset -u` that yields the split the outer number cannot see:
//!
//! ```text
//! index:do_read_index          loading the per-worktree index
//! unpack_trees:traverse_trees  resolving the tree
//! unpack_trees:unpack_trees    the whole apply (traversal included)
//! index:do_write_index         writing the index back
//! ```
//!
//! plus `index read/cache_nr` / `write/cache_nr` (the index's entry count) and,
//! for free, **whether the index was warm**: a run with no index to read emits no
//! `read/cache_nr` at all.
//!
//! ## This is a diagnostic surface, not an API
//!
//! trace2's event names are not a compatibility promise, so every use here is
//! **best-effort** in the same sense as the metrics sink itself (ADR-0013):
//! capture that can't be set up, a file that can't be read, a line that doesn't
//! parse, and a region that isn't there all resolve to `None`. The operation runs
//! and its outer timings stand regardless.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// The environment variable `git` writes its trace2 event stream to.
const TRACE2_EVENT_ENV: &str = "GIT_TRACE2_EVENT";

/// A temp file wired up to receive one `git` child's trace2 event stream.
///
/// Create it with [`Trace2Capture::new`], attach it to the child with
/// [`Trace2Capture::apply`], and read the events back with
/// [`Trace2Capture::harvest`] once the child has exited.
#[derive(Debug)]
pub struct Trace2Capture {
    file: tempfile::NamedTempFile,
}

impl Trace2Capture {
    /// Prepare a capture, or `None` if we should not (or cannot) capture.
    ///
    /// Returns `None` when the caller's own environment already sets
    /// [`TRACE2_EVENT_ENV`]: an operator debugging `git-full-send` with their own
    /// trace2 destination outranks our metrics, and silently redirecting their
    /// stream would be worse than recording no sub-timings.
    pub fn new() -> Option<Self> {
        if std::env::var_os(TRACE2_EVENT_ENV).is_some() {
            return None;
        }
        tempfile::NamedTempFile::new()
            .ok()
            .map(|file| Self { file })
    }

    /// Point `command`'s trace2 event stream at this capture.
    ///
    /// `git` wants an absolute path (a bare name is interpreted as a file
    /// descriptor), which [`tempfile::NamedTempFile`] always gives us.
    pub fn apply(&self, command: &mut Command) {
        command.env(TRACE2_EVENT_ENV, self.file.path());
    }

    /// Read the events the child wrote, once it has exited.
    ///
    /// `None` if the stream is unreadable or contained nothing we recognise.
    pub fn harvest(self) -> Option<Trace2> {
        Trace2::from_file(self.file.path())
    }
}

/// The region timings and counters harvested from one `git` child.
#[derive(Debug, Default, Clone)]
pub struct Trace2 {
    /// `(category, label)` → total time inside that region, in milliseconds.
    /// Repeated regions are summed.
    regions_ms: HashMap<(String, String), f64>,
    /// `(category, key)` → the last integer value reported for that counter.
    data: HashMap<(String, String), i64>,
}

impl Trace2 {
    /// Parse a trace2 event stream from `path`.
    fn from_file(path: &Path) -> Option<Self> {
        let contents = std::fs::read_to_string(path).ok()?;
        let mut out = Self::default();
        for line in contents.lines() {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                // A truncated or unrecognised line costs us that line, nothing more.
                continue;
            };
            match event.get("event").and_then(|e| e.as_str()) {
                // A region's duration arrives with its *leave* event, as `t_rel`
                // (seconds since the matching enter).
                Some("region_leave") => {
                    let (Some(category), Some(label), Some(seconds)) = (
                        event.get("category").and_then(|v| v.as_str()),
                        event.get("label").and_then(|v| v.as_str()),
                        event.get("t_rel").and_then(|v| v.as_f64()),
                    ) else {
                        continue;
                    };
                    *out.regions_ms
                        .entry((category.to_string(), label.to_string()))
                        .or_insert(0.0) += seconds * 1000.0;
                }
                // Counters. `value` is usually a number but may be a string.
                Some("data") => {
                    let (Some(category), Some(key), Some(value)) = (
                        event.get("category").and_then(|v| v.as_str()),
                        event.get("key").and_then(|v| v.as_str()),
                        event.get("value").and_then(as_i64),
                    ) else {
                        continue;
                    };
                    out.data
                        .insert((category.to_string(), key.to_string()), value);
                }
                _ => {}
            }
        }
        if out.regions_ms.is_empty() && out.data.is_empty() {
            return None;
        }
        Some(out)
    }

    /// Total time spent inside the `category`/`label` region, in milliseconds.
    pub fn region_ms(&self, category: &str, label: &str) -> Option<f64> {
        self.regions_ms
            .get(&(category.to_string(), label.to_string()))
            .copied()
    }

    /// The integer counter reported under `category`/`key`.
    pub fn data_i64(&self, category: &str, key: &str) -> Option<i64> {
        self.data
            .get(&(category.to_string(), key.to_string()))
            .copied()
    }
}

/// Coerce a trace2 `value` to an integer, accepting the numeric and the
/// stringified spelling.
fn as_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a `Trace2` from a literal event stream, as `git` would write it.
    fn parse(events: &str) -> Option<Trace2> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(events.as_bytes()).unwrap();
        Trace2::from_file(file.path())
    }

    #[test]
    fn region_durations_and_counters_are_harvested() {
        // Trimmed from a real `git read-tree --reset -u` stream (git 2.52).
        let t2 = parse(
            r#"{"event":"version","evt":"3","exe":"2.52.0"}
{"event":"region_enter","category":"index","label":"do_read_index"}
{"event":"data","category":"index","key":"read/cache_nr","value":34012}
{"event":"region_leave","category":"index","label":"do_read_index","t_rel":0.040000}
{"event":"region_enter","category":"unpack_trees","label":"unpack_trees"}
{"event":"region_enter","category":"unpack_trees","label":"traverse_trees"}
{"event":"region_leave","category":"unpack_trees","label":"traverse_trees","t_rel":0.015000}
{"event":"region_leave","category":"unpack_trees","label":"unpack_trees","t_rel":3.900000}
"#,
        )
        .expect("a parseable stream");

        assert_eq!(t2.region_ms("index", "do_read_index"), Some(40.0));
        assert_eq!(t2.region_ms("unpack_trees", "traverse_trees"), Some(15.0));
        assert_eq!(t2.region_ms("unpack_trees", "unpack_trees"), Some(3900.0));
        assert_eq!(t2.data_i64("index", "read/cache_nr"), Some(34012));
        // Absent regions and counters are simply missing, never an error.
        assert_eq!(t2.region_ms("index", "do_write_index"), None);
        assert_eq!(t2.data_i64("index", "write/cache_nr"), None);
    }

    #[test]
    fn repeated_regions_sum() {
        let t2 = parse(
            r#"{"event":"region_leave","category":"c","label":"l","t_rel":0.001}
{"event":"region_leave","category":"c","label":"l","t_rel":0.002}
"#,
        )
        .expect("a parseable stream");
        assert_eq!(t2.region_ms("c", "l"), Some(3.0));
    }

    #[test]
    fn a_stringified_counter_value_is_accepted() {
        let t2 = parse(r#"{"event":"data","category":"index","key":"read/cache_nr","value":"7"}"#)
            .expect("a parseable stream");
        assert_eq!(t2.data_i64("index", "read/cache_nr"), Some(7));
    }

    /// Garbage, truncation, and an unrecognised git version cost us the
    /// sub-timings and nothing else — the caller keeps its outer measurement.
    #[test]
    fn unparseable_or_empty_streams_yield_nothing_rather_than_failing() {
        assert!(parse("").is_none());
        assert!(parse("not json at all\n{\"event\":\"tru").is_none());
        // Recognised shape, unrecognised content: still no panic, just misses.
        let t2 = parse(r#"{"event":"region_leave","category":"new","label":"thing","t_rel":0.5}"#)
            .expect("a parseable stream");
        assert_eq!(t2.region_ms("index", "do_read_index"), None);
    }

    /// A capture never hijacks an operator's own trace2 destination.
    #[test]
    fn capture_stands_aside_when_the_environment_already_sets_a_destination() {
        // Safety: this test is single-threaded with respect to the env var it
        // touches — no other test in this module reads or writes it.
        unsafe { std::env::set_var(TRACE2_EVENT_ENV, "/tmp/operators-own-stream") };
        let capture = Trace2Capture::new();
        unsafe { std::env::remove_var(TRACE2_EVENT_ENV) };
        assert!(capture.is_none(), "the operator's destination wins");
        assert!(Trace2Capture::new().is_some(), "and ours is used otherwise");
    }
}
