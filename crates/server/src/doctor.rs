//! Checking whether the repository itself is the problem (ADR-0018).
//!
//! Two of the costs that made `git-full-send` slow in the field were properties
//! of the *repository*, not of the tool or the developer's diff, and neither was
//! ever reported:
//!
//! * 28,709 refs, so every connection carried a ~3.1 MB ref advertisement; and
//! * a broken `objects/info/alternates` entry, which made every `git` invocation
//!   print `error: unable to normalize alternate object path: …` while still
//!   working, and which `git-full-send` passed through in silence.
//!
//! Both were knowable in advance. `doctor` runs the checks whose failures an
//! operator can actually act on, and pairs each with a remedy rather than only a
//! number.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::ServerError;

/// A check's verdict.
///
/// `error` is reserved for something actually broken (so an orchestrator can
/// gate on the exit code); `warn` is "this will cost you"; `ok` includes purely
/// informational findings.
pub const OK: &str = "ok";
/// See [`OK`].
pub const WARN: &str = "warn";
/// See [`OK`].
pub const ERROR: &str = "error";

/// The wire cost of advertising one ref: a 4-byte pkt-line length header, a
/// 40-character object id, a space, the name, and a newline.
const REF_ADVERTISEMENT_OVERHEAD: u64 = 4 + 40 + 1 + 1;

/// Advertisement size at which the overhead is worth an operator's attention.
/// Below this it is noise next to any real push; above it, it *is* the push.
const ADVERTISEMENT_WARN_BYTES: u64 = 256 * 1024;

/// Packs beyond which a repository is worth repacking.
const PACK_WARN_COUNT: usize = 50;

/// One thing `doctor` looked at.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct Check {
    /// Stable identifier, e.g. `refs` or `alternates`.
    pub name: String,
    /// [`OK`], [`WARN`], or [`ERROR`].
    pub status: &'static str,
    /// What was found, in one line.
    pub summary: String,
    /// What to do about it, when there is something to do.
    pub remedy: Option<String>,
    /// The numbers behind the summary, for a machine reading `--json`.
    pub detail: BTreeMap<String, serde_json::Value>,
}

impl Check {
    /// A check with no structured detail.
    pub fn new(
        name: impl Into<String>,
        status: &'static str,
        summary: impl Into<String>,
        remedy: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status,
            summary: summary.into(),
            remedy,
            detail: BTreeMap::new(),
        }
    }

    /// Attach a number (or any JSON value) to the check.
    pub fn with(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.detail.insert(key.to_string(), value.into());
        self
    }
}

/// The result of a `doctor` run — the record written by `--json`.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct DoctorReport {
    /// `kind`/`schema`/`ts_unix_ms`/`tool_version`, flattened into the record.
    #[serde(flatten)]
    pub envelope: git_full_send_common::metrics::Envelope,
    /// The repository examined.
    pub repo: String,
    /// The worktree examined, if one was given.
    pub worktree: Option<String>,
    /// Every check, in the order run.
    pub checks: Vec<Check>,
}

impl DoctorReport {
    /// How many checks came back [`ERROR`].
    pub fn errors(&self) -> usize {
        self.checks.iter().filter(|c| c.status == ERROR).count()
    }

    /// How many came back [`WARN`].
    pub fn warnings(&self) -> usize {
        self.checks.iter().filter(|c| c.status == WARN).count()
    }

    /// Append a check produced elsewhere (the CLI adds the client-side
    /// force-include pattern check, which lives with the selection walk).
    pub fn push(&mut self, check: Check) {
        self.checks.push(check);
    }
}

/// Examine `repo` (and `worktree`, if given) and report what is likely to hurt.
///
/// Every check here is cheap — a ref enumeration, a few `readdir`s, a config
/// lookup — because the point is that an operator will actually run it.
pub fn doctor(repo: &Path, worktree: Option<&Path>) -> Result<DoctorReport, ServerError> {
    let discovered = gix::discover(repo).map_err(|_| ServerError::NotARepo(repo.to_path_buf()))?;
    let git_dir = discovered.git_dir().to_path_buf();
    let objects_dir = git_dir.join("objects");

    let mut checks = vec![
        check_refs(&discovered),
        check_alternates(&objects_dir),
        check_objects(&objects_dir),
        check_autogc(&discovered),
    ];
    if let Some(worktree) = worktree {
        checks.push(check_worktree(&discovered, &git_dir, worktree));
    }

    Ok(DoctorReport {
        envelope: git_full_send_common::metrics::Envelope::new("doctor"),
        repo: repo.display().to_string(),
        worktree: worktree.map(|w| w.display().to_string()),
        checks,
    })
}

/// Ref count, and the advertisement every single connection pays for it.
///
/// The advertisement is *estimated* from the ref names rather than measured —
/// `doctor` has no connection in hand. `git-full-send probe` measures the real
/// thing (ADR-0018).
fn check_refs(repo: &gix::Repository) -> Check {
    // The iterator borrows its platform, so both live for the loop below.
    let platform = match repo.references() {
        Ok(platform) => platform,
        Err(error) => return refs_unreadable(&error.to_string()),
    };
    let iter = match platform.all() {
        Ok(iter) => iter,
        Err(error) => return refs_unreadable(&error.to_string()),
    };
    let (mut total, mut ours, mut bytes) = (0u64, 0u64, 0u64);
    for reference in iter.flatten() {
        let name = reference.name().as_bstr().to_string();
        total += 1;
        bytes += REF_ADVERTISEMENT_OVERHEAD + name.len() as u64;
        if name.starts_with(git_full_send_common::REF_NAMESPACE) {
            ours += 1;
        }
    }

    let human = format_bytes(bytes);
    let status = if bytes >= ADVERTISEMENT_WARN_BYTES {
        WARN
    } else {
        OK
    };
    let summary = format!(
        "{total} ref(s) ({ours} git-full-send's) — about {human} of ref advertisement \
         on every connection"
    );
    let remedy = (status == WARN).then(|| {
        "Every push connection pays this before any of your data moves, and a sync makes \
         two. Sync into a dedicated repository whose objects/info/alternates points at \
         this one's object store: it keeps the delta bases that make pushes small while \
         advertising only a handful of refs."
            .to_string()
    });
    Check::new("refs", status, summary, remedy)
        .with("refs", total)
        .with("refs_ours", ours)
        .with("advertisement_bytes_estimate", bytes)
}

/// A repository whose refs cannot be enumerated at all.
fn refs_unreadable(error: &str) -> Check {
    Check::new(
        "refs",
        ERROR,
        format!("could not enumerate refs: {error}"),
        Some("check the repository is readable and not corrupt".into()),
    )
}

/// `objects/info/alternates` entries that do not resolve.
///
/// The failure this exists for: an alternates path that no longer exists makes
/// every `git` invocation print `error: unable to normalize alternate object
/// path: …` and carry on working, so nobody investigates until something else
/// breaks.
fn check_alternates(objects_dir: &Path) -> Check {
    let alternates = objects_dir.join("info").join("alternates");
    let contents = match std::fs::read_to_string(&alternates) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Check::new(
                "alternates",
                OK,
                "no alternate object stores configured",
                None,
            )
            .with("entries", 0);
        }
        Err(error) => {
            return Check::new(
                "alternates",
                ERROR,
                format!("could not read {}: {error}", alternates.display()),
                Some("check the file's permissions".into()),
            );
        }
    };

    let mut entries = 0u64;
    let mut broken = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        entries += 1;
        // A relative entry is relative to the objects dir that names it.
        let path = PathBuf::from(line);
        let resolved = if path.is_absolute() {
            path
        } else {
            objects_dir.join(path)
        };
        if !resolved.is_dir() {
            broken.push(line.to_string());
        }
    }

    if broken.is_empty() {
        return Check::new(
            "alternates",
            OK,
            format!("{entries} alternate object store(s), all reachable"),
            None,
        )
        .with("entries", entries);
    }
    Check::new(
        "alternates",
        ERROR,
        format!(
            "{} of {entries} alternate object store(s) unreachable: {}",
            broken.len(),
            broken.join(", "),
        ),
        Some(format!(
            "git prints `unable to normalize alternate object path` for these on every \
             invocation and carries on. Fix or remove the offending line(s) in {}",
            objects_dir.join("info").join("alternates").display(),
        )),
    )
    .with("entries", entries)
    .with("broken", broken)
}

/// Pack count and loose-object pressure.
///
/// Deliberately shallow: one `readdir` of `objects/pack` and one of `objects`
/// itself. Counting every loose object would mean up to 256 more directory
/// listings on a repository that may hold millions, which is not a price a
/// diagnostic should impose.
fn check_objects(objects_dir: &Path) -> Check {
    let packs = std::fs::read_dir(objects_dir.join("pack"))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "pack"))
                .count()
        })
        .unwrap_or(0);
    // The 256 two-hex-digit fanout directories that hold loose objects.
    let fanout = std::fs::read_dir(objects_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit())
                })
                .count()
        })
        .unwrap_or(0);

    let status = if packs > PACK_WARN_COUNT { WARN } else { OK };
    let summary = format!("{packs} pack(s); loose objects in {fanout}/256 fanout director(ies)");
    let remedy = (status == WARN).then(|| {
        format!(
            "more than {PACK_WARN_COUNT} packs slows every object lookup; \
             `git gc` (or `git repack -ad`) when no sync is in flight"
        )
    });
    Check::new("objects", status, summary, remedy)
        .with("packs", packs)
        .with("loose_fanout_dirs", fanout)
}

/// Whether the repo asks for automatic gc after a receive.
///
/// `git-full-send` passes `receive.autogc=false` for its own receive window
/// regardless (Research 0003: a post-receive gc can prune the delta bases the
/// next push needs), so this is informational — but a repo that sets it *on* is
/// a repo where someone expects gc to run, and a manual `git gc` between syncs
/// has the same effect.
fn check_autogc(repo: &gix::Repository) -> Check {
    let configured = repo.config_snapshot().boolean("receive.autogc");
    let (status, summary) = match configured {
        Some(true) => (
            OK,
            "receive.autogc is on in this repo; git-full-send disables it for its own \
             receives"
                .to_string(),
        ),
        Some(false) => (OK, "receive.autogc is off".to_string()),
        None => (
            OK,
            "receive.autogc unset (git's default is on); git-full-send disables it for \
             its own receives"
                .to_string(),
        ),
    };
    Check::new("receive_autogc", status, summary, None).with(
        "configured",
        configured
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null),
    )
}

/// The target worktree: whether it is the repository's *own* working tree, and
/// what state its per-worktree index is in.
///
/// Checking out over the repo's own working tree is exactly the configuration
/// behind #75's measurements (`--repo` and `--worktree` the same path), and
/// it is worth saying out loud: the checkout is authoritative and destructive
/// (ADR-0008), so anything uncommitted in that tree is gfs's to overwrite.
fn check_worktree(repo: &gix::Repository, git_dir: &Path, worktree: &Path) -> Check {
    let canonical = worktree.canonicalize().ok();
    let repo_workdir = repo.workdir().and_then(|w| w.canonicalize().ok());

    let index = canonical
        .as_deref()
        .and_then(|w| crate::worktree_index_path(git_dir, w).ok());
    let index_bytes = index
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());

    let is_own_worktree = match (&canonical, &repo_workdir) {
        (Some(w), Some(r)) => w == r,
        _ => false,
    };

    let index_note = match index_bytes {
        Some(bytes) => format!("per-worktree index present ({})", format_bytes(bytes)),
        None => "no per-worktree index yet (the first checkout will be a full one)".to_string(),
    };

    if is_own_worktree {
        return Check::new(
            "worktree",
            WARN,
            format!("the target worktree is this repository's own working tree; {index_note}"),
            Some(
                "update-worktree is authoritative and destructive (ADR-0008): it stomps \
                 edits and removes untracked non-ignored files in that tree. Check out \
                 into a dedicated, disposable directory unless you mean this."
                    .into(),
            ),
        )
        .with("is_repo_own_worktree", true)
        .with(
            "index_bytes",
            index_bytes
                .map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::Null),
        );
    }

    Check::new(
        "worktree",
        OK,
        format!("dedicated worktree directory; {index_note}"),
        None,
    )
    .with("is_repo_own_worktree", false)
    .with(
        "index_bytes",
        index_bytes
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null),
    )
}

/// Format a byte count with binary units, for a check's one-line summary.
fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// The cheap subset of [`doctor`] worth running unprompted at `listen` startup:
/// the two findings that are both nearly free to obtain and expensive to ignore.
///
/// Logged rather than returned, because the operator who most needs them is the
/// one who did not think to run `doctor` (ADR-0018).
pub(crate) fn log_startup_checks(repo: &Path) {
    let Ok(discovered) = gix::discover(repo) else {
        return;
    };
    let objects_dir = discovered.git_dir().join("objects");

    let refs = check_refs(&discovered);
    if refs.status == WARN {
        tracing::warn!(
            refs = ?refs.detail.get("refs"),
            advertisement_bytes = ?refs.detail.get("advertisement_bytes_estimate"),
            "this repo's ref count makes every connection carry a large ref \
             advertisement; run `git-full-send doctor --repo <this repo>` for detail",
        );
    }
    let alternates = check_alternates(&objects_dir);
    if alternates.status == ERROR {
        tracing::error!(summary = %alternates.summary, "alternate object store problem");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_alternates_file_is_not_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let check = check_alternates(dir.path());
        assert_eq!(check.status, OK);
    }

    /// The observation that motivated the check: an alternates entry pointing at
    /// a path that no longer exists (#75).
    #[test]
    fn an_unreachable_alternates_entry_is_an_error_with_a_remedy() {
        let dir = tempfile::tempdir().unwrap();
        let objects = dir.path();
        std::fs::create_dir_all(objects.join("info")).unwrap();
        std::fs::write(
            objects.join("info").join("alternates"),
            "/gone/repo.git/objects\n",
        )
        .unwrap();

        let check = check_alternates(objects);
        assert_eq!(check.status, ERROR);
        assert!(
            check.summary.contains("/gone/repo.git/objects"),
            "it names the offending entry: {}",
            check.summary,
        );
        assert!(check.remedy.is_some(), "and says what to do about it");
    }

    /// A relative entry resolves against the objects dir that names it, so a
    /// valid one must not be reported as broken.
    #[test]
    fn a_reachable_relative_alternates_entry_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let objects = dir.path().join("objects");
        std::fs::create_dir_all(objects.join("info")).unwrap();
        std::fs::create_dir_all(dir.path().join("other-objects")).unwrap();
        // Relative to the objects dir that names it: `<dir>/objects/..` is
        // `<dir>`, so this is `<dir>/other-objects`.
        std::fs::write(
            objects.join("info").join("alternates"),
            "../other-objects\n",
        )
        .unwrap();

        let check = check_alternates(&objects);
        assert_eq!(check.status, OK, "{}", check.summary);
    }

    #[test]
    fn bytes_format_with_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MiB");
    }
}
