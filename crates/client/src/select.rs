//! Select the force-included, normally-gitignored files (ADR-0007).
//!
//! `git-full-send` deliberately syncs a controlled set of files that are
//! normally gitignored — CPU-intensive web-client build outputs, per-user config
//! — declared as **gitignore-syntax allow-list patterns** across two layers:
//!
//! * a committed, project-level file at the repo root ([`PROJECT_INCLUDE_FILE`]),
//!   shared and version-controlled; and
//! * an optional per-user file outside the repo ([`user_include_path`]), mirroring
//!   Git's `core.excludesFile`, read on the client only.
//!
//! The two layers are evaluated **`[project, then user]` with last-match-wins**:
//! both may add includes, and a per-user `!` can carve out a project include.
//! Note the **inverted polarity** versus `.gitignore` — here a bare pattern
//! *includes* and `!` *carves out*.
//!
//! ## Why an independent filesystem walk
//!
//! This is an **independent allow-list matched against the working-tree
//! filesystem**, *not* `!` negations on the project's real `.gitignore`. That
//! sidesteps Git's "cannot re-include a file under an excluded parent directory"
//! limitation, which would otherwise bite constantly (build outputs live under an
//! ignored `dist/`/`target/`).
//!
//! We can't reuse `gix-dir`'s walk for this: it refuses to recurse into an
//! *ignored* directory unless a positive **pathspec** matches it, and our
//! allow-list is gitignore-syntax, not pathspecs. So we walk the tree ourselves
//! and apply [`gix::ignore::Search`] (which gives last-match-wins + `!` negation
//! directly), descending into normally-ignored directories so the files beneath
//! them can be selected.
//!
//! ## Selection semantics
//!
//! The walk carries an inherited *included/excluded* state, starting excluded:
//!
//! * For a **directory**, a non-negative match sets the subtree to included, a
//!   `!` match sets it excluded, and no match inherits the parent's state. We
//!   **always descend** (except into `.git`) so a deeper pattern or `!` carve-out
//!   under an included parent still applies.
//! * For a **file or symlink**, a non-negative match selects it, a `!` match
//!   skips it, and no match selects it iff the inherited state is included — so a
//!   `dist/` directory pattern pulls its whole subtree while `!dist/secret`
//!   carves one file back out. This reproduces gitignore's directory-level
//!   semantics, which matching leaf paths alone would miss.
//!
//! **Performance — pruning the walk.** Descending every non-`.git` directory would
//! traverse an unrelated large ignored tree (e.g. `node_modules`) even when nothing
//! in it is selected. To avoid that, we skip a directory unless it is already inside
//! an included subtree *or* some include pattern could still match beneath it. The
//! test is derived from each positive pattern's *anchoring*: a pattern with a leading
//! `/` or an interior `/` (e.g. `/dist/`, `web-client/dist/`, `target/release/**`)
//! has a literal directory prefix we can compare against, so a directory off that
//! prefix is pruned. A pattern with no anchor — a bare basename or `basename/`
//! (`dist/`, `*.wasm`), or a leading `**`/wildcard — can match at *any* depth, so it
//! forces the full exhaustive walk, and we warn about it (such a pattern is most
//! often an accidental include). The prune is a deliberate over-approximation: it
//! never skips a directory the exhaustive walk would have selected from. See
//! Research 0004 for the original O(N·M)-once-per-sync accounting.

use std::path::{Path, PathBuf};

use gix::bstr::{BStr, BString, ByteSlice, ByteVec};
use gix::glob::pattern::Case;
use thiserror::Error;

/// The committed, project-level include file, looked for at the repo root.
pub const PROJECT_INCLUDE_FILE: &str = ".git-full-send-include";

/// Environment variable that overrides the per-user include file location.
///
/// When set (and non-empty) it names the per-user pattern file directly, taking
/// precedence over the `$XDG_CONFIG_HOME`/`$HOME` lookup. Primarily a test seam,
/// but a legitimate escape hatch for unusual setups too.
pub const USER_INCLUDE_ENV: &str = "GIT_FULL_SEND_USER_INCLUDE";

/// Errors returned by [`select_extra_paths`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SelectError {
    /// Reading a pattern file failed (a missing file is *not* an error).
    #[error("could not read include pattern file `{path}`")]
    ReadPatternFile {
        /// The pattern file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Listing a worktree directory failed.
    #[error("could not read worktree directory `{path}`")]
    ReadDir {
        /// The directory that could not be listed.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Stat-ing a worktree entry failed.
    #[error("could not inspect worktree entry `{path}`")]
    Metadata {
        /// The entry that could not be inspected.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A worktree path was not valid Unicode and could not be represented as a
    /// Git path (only reachable on platforms with non-UTF-8 paths).
    #[error("worktree path `{0}` is not representable as a Git path")]
    NonUnicodePath(PathBuf),
}

/// What the selection walk cost, alongside what it found (ADR-0017).
///
/// The walk already *warns* that an unanchored pattern forces an exhaustive
/// scan; these counters make the price of that warning visible, so an operator
/// can see a walk that entered 40,000 directories to select twelve files.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
#[non_exhaustive]
pub struct SelectStats {
    /// Directories the walk descended into (the repo root included).
    pub dirs_entered: usize,
    /// Directories skipped because no positive pattern could match beneath them.
    /// Zero whenever `unanchored_patterns` is non-zero — one unanchored pattern
    /// disables pruning entirely.
    pub dirs_pruned: usize,
    /// Entries the walk looked at and classified, of any kind.
    pub paths_considered: usize,
    /// Positive patterns that match at any depth and so force the exhaustive
    /// walk.
    pub unanchored_patterns: usize,
}

/// The outcome of a measured selection walk: the paths, and what finding them
/// cost.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Selection {
    /// The selected repo-relative paths (slash-separated, sorted, deduplicated).
    pub paths: Vec<BString>,
    /// What the walk cost to produce them.
    pub stats: SelectStats,
}

/// Select the force-included files under `workdir`, returning their
/// repo-relative paths (slash-separated, sorted, deduplicated).
///
/// Combines the project-level [`PROJECT_INCLUDE_FILE`] and the optional per-user
/// file ([`user_include_path`]) into one last-match-wins allow-list and walks the
/// working tree to enumerate the matches. With no pattern files present the
/// result is empty (the caller still writes an empty `extra` tree).
pub fn select_extra_paths(workdir: &Path) -> Result<Vec<BString>, SelectError> {
    select_extra_paths_with(workdir, None)
}

/// As [`select_extra_paths`], but with the per-user include file chosen
/// explicitly.
///
/// `user_include_override` takes precedence when `Some` (the `--user-include`
/// CLI flag); when `None` the per-user file is resolved from the environment via
/// [`user_include_path`] exactly as [`select_extra_paths`] does. A `Some` path
/// that does not exist is treated as an empty layer, like any other missing
/// pattern file.
pub fn select_extra_paths_with(
    workdir: &Path,
    user_include_override: Option<&Path>,
) -> Result<Vec<BString>, SelectError> {
    select_extra_paths_measured(workdir, user_include_override).map(|selection| selection.paths)
}

/// As [`select_extra_paths_with`], but also returning what the walk cost
/// ([`SelectStats`], ADR-0017).
///
/// This is what `sync` calls; the plainer forms above stay for callers (and
/// tests) that only want the paths.
pub fn select_extra_paths_measured(
    workdir: &Path,
    user_include_override: Option<&Path>,
) -> Result<Selection, SelectError> {
    match user_include_override {
        Some(path) => select_in(workdir, Some(path)),
        None => select_in(workdir, user_include_path().as_deref()),
    }
}

/// The positive force-include patterns of the repository at `repo_dir` that are
/// **unanchored**, as they are written in the pattern file.
///
/// An unanchored pattern can match at any depth, so it disables the walk's
/// pruning entirely and forces a full working-tree scan every sync. `sync`
/// already warns about them as it goes; this is the same finding, available to
/// `doctor` without running a sync (ADR-0018).
///
/// Both layers are consulted, exactly as selection does. `repo_dir` may be a
/// repository (its working tree is discovered) or a working tree directly; a
/// bare repository has no patterns and yields none.
pub fn unanchored_patterns(repo_dir: &Path) -> Result<Vec<String>, SelectError> {
    let workdir = match gix::discover(repo_dir) {
        Ok(repo) => match repo.workdir() {
            Some(workdir) => workdir.to_path_buf(),
            // Bare: no working tree, so no pattern files to find.
            None => return Ok(Vec::new()),
        },
        // Not a repository: treat the path as a working tree, so this also works
        // against a checked-out worktree that has no git dir of its own.
        Err(_) => repo_dir.to_path_buf(),
    };
    let search = load_search(&workdir, user_include_path().as_deref())?;
    let mut out: Vec<String> = Vec::new();
    for list in &search.patterns {
        for mapping in &list.patterns {
            let pattern = &mapping.pattern;
            // Negative carve-outs never force descent: they only shrink the result.
            if pattern.is_negative() || prunable_prefix(pattern).is_some() {
                continue;
            }
            let display = pattern.to_string();
            if !out.contains(&display) {
                out.push(display);
            }
        }
    }
    Ok(out)
}

/// The core of [`select_extra_paths`] with the per-user file path supplied
/// explicitly (rather than resolved from the environment), so tests can exercise
/// the two-layer semantics without mutating process-global environment.
fn select_in(workdir: &Path, user_include: Option<&Path>) -> Result<Selection, SelectError> {
    let search = load_search(workdir, user_include)?;
    let prune = build_prune_info(&search);

    let mut walk = Walk {
        search: &search,
        prune: &prune,
        out: Vec::new(),
        stats: SelectStats {
            unanchored_patterns: prune.unanchored_count,
            ..SelectStats::default()
        },
        #[cfg(test)]
        entered: Vec::new(),
    };
    walk.run(workdir, BString::default(), false)?;
    let mut paths = walk.out;
    paths.sort();
    paths.dedup();
    Ok(Selection {
        paths,
        stats: walk.stats,
    })
}

/// Build the combined allow-list. The project layer is added first and the user
/// layer second; because [`gix::ignore::Search`] matches pattern lists in
/// reverse, this realises `[project, then user]` last-match-wins (a user match
/// always wins over a project match, and within a layer the last matching line
/// wins).
fn load_search(
    workdir: &Path,
    user_include: Option<&Path>,
) -> Result<gix::ignore::Search, SelectError> {
    let mut search = gix::ignore::Search::default();
    let parse = gix::ignore::search::Ignore::default();

    let project = workdir.join(PROJECT_INCLUDE_FILE);
    if let Some(bytes) = read_optional(&project)? {
        search.add_patterns_buffer(&bytes, project, None, parse);
    }
    if let Some(user) = user_include
        && let Some(bytes) = read_optional(user)?
    {
        search.add_patterns_buffer(&bytes, user, None, parse);
    }
    Ok(search)
}

/// Resolve the per-user include file path, or `None` if none is configured.
///
/// [`USER_INCLUDE_ENV`] wins if set and non-empty; otherwise
/// `$XDG_CONFIG_HOME/git-full-send/include`, falling back to
/// `$HOME/.config/git-full-send/include`. The returned path may not exist — a
/// missing file is treated as an empty layer by [`select_extra_paths`].
pub fn user_include_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(USER_INCLUDE_ENV)
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("git-full-send").join("include"))
}

/// Read a pattern file, mapping a non-existent file to `None` (not an error).
fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, SelectError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SelectError::ReadPatternFile {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// State threaded through the recursive worktree walk: the allow-list, the
/// derived prune information, and the accumulating list of selected paths.
struct Walk<'a> {
    search: &'a gix::ignore::Search,
    prune: &'a PruneInfo,
    out: Vec<BString>,
    /// What the walk has cost so far (ADR-0017).
    stats: SelectStats,
    /// Repo-relative prefixes of every directory the walk descended into,
    /// recorded so tests can assert the prune actually skipped a subtree rather
    /// than merely failing to match anything inside it.
    #[cfg(test)]
    entered: Vec<BString>,
}

impl Walk<'_> {
    /// Recursively walk `dir` (at repo-relative `rel_prefix`), appending the
    /// repo-relative paths of selected files to `self.out`. `inherited` is the
    /// included/excluded state propagated from the nearest matched ancestor.
    fn run(&mut self, dir: &Path, rel_prefix: BString, inherited: bool) -> Result<(), SelectError> {
        self.stats.dirs_entered += 1;
        #[cfg(test)]
        self.entered.push(rel_prefix.clone());

        let mut entries = std::fs::read_dir(dir)
            .map_err(|source| SelectError::ReadDir {
                path: dir.to_path_buf(),
                source,
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| SelectError::ReadDir {
                path: dir.to_path_buf(),
                source,
            })?;
        // Deterministic order so the resulting tree is reproducible.
        entries.sort_by_key(std::fs::DirEntry::file_name);

        for entry in entries {
            let name_os = entry.file_name();
            // Never descend into a Git directory (our own or a submodule's).
            if name_os.as_os_str() == std::ffi::OsStr::new(".git") {
                continue;
            }
            let name = gix::path::os_str_into_bstr(&name_os)
                .map_err(|_| SelectError::NonUnicodePath(entry.path()))?;
            let rel = join_rel(rel_prefix.as_bstr(), name);
            self.stats.paths_considered += 1;

            let file_type = entry.file_type().map_err(|source| SelectError::Metadata {
                path: entry.path(),
                source,
            })?;

            if file_type.is_dir() {
                let state = classify(self.search, rel.as_bstr(), true).unwrap_or(inherited);
                // Descend only if the subtree is already included or some include
                // pattern could still match beneath it; otherwise prune it.
                if state || self.prune.can_contain_match(rel.as_bstr()) {
                    self.run(&entry.path(), rel, state)?;
                } else {
                    self.stats.dirs_pruned += 1;
                }
            } else if (file_type.is_file() || file_type.is_symlink())
                && classify(self.search, rel.as_bstr(), false).unwrap_or(inherited)
            {
                self.out.push(rel);
            }
            // Anything else (FIFO, socket, …) is not representable in a tree; skip it.
        }
        Ok(())
    }
}

/// Which directories the walk may safely skip, derived once from the allow-list.
///
/// Built from the **positive** (include) patterns only: negative `!` carve-outs
/// can never *add* a selection, so a directory with no possible positive match is
/// safe to prune regardless of any `!` inside it.
struct PruneInfo {
    /// At least one positive pattern is unanchored (matches at any depth), so no
    /// directory can be pruned — fall back to the exhaustive walk.
    any_unanchored: bool,
    /// How many distinct positive patterns are unanchored, for the walk's
    /// [`SelectStats`]: the warning says *that* the walk went exhaustive, this
    /// says how many patterns to go and fix.
    unanchored_count: usize,
    /// Literal leading directory prefixes of the anchored positive patterns, each
    /// as a list of path segments (e.g. `web-client/dist/` → `["web-client", "dist"]`).
    prefixes: Vec<Vec<BString>>,
}

impl PruneInfo {
    /// Whether a directory at repo-relative `rel` could contain a file matching
    /// some positive include pattern, and so must be descended into.
    ///
    /// True if any positive pattern is unanchored, or if some anchored prefix is
    /// *path-compatible* with `rel` — i.e. the shorter of the two is a segment-wise
    /// prefix of the longer, so the prefix lies on, at, or under `rel` (or vice
    /// versa). This over-approximates: it may descend a little more than strictly
    /// necessary, but never prunes a directory the exhaustive walk would select from.
    fn can_contain_match(&self, rel: &BStr) -> bool {
        if self.any_unanchored {
            return true;
        }
        let dir: Vec<&[u8]> = rel.split(|&b| b == b'/').collect();
        self.prefixes.iter().any(|prefix| {
            let common = prefix.len().min(dir.len());
            (0..common).all(|i| prefix[i].as_slice() == dir[i])
        })
    }
}

/// Inspect the allow-list and derive its [`PruneInfo`], warning about any positive
/// pattern that is completely unanchored (and so forces a full working-tree scan).
fn build_prune_info(search: &gix::ignore::Search) -> PruneInfo {
    let mut any_unanchored = false;
    let mut prefixes = Vec::new();
    // Deduplicated display forms of unanchored positive patterns, for one warning.
    let mut unanchored: Vec<(String, Option<PathBuf>)> = Vec::new();

    for list in &search.patterns {
        for mapping in &list.patterns {
            let pattern = &mapping.pattern;
            // Negative carve-outs never force descent: they only shrink the result.
            if pattern.is_negative() {
                continue;
            }
            match prunable_prefix(pattern) {
                Some(segments) => prefixes.push(segments),
                None => {
                    any_unanchored = true;
                    let display = pattern.to_string();
                    if !unanchored.iter().any(|(text, _)| *text == display) {
                        unanchored.push((display, list.source.clone()));
                    }
                }
            }
        }
    }

    for (pattern, source) in &unanchored {
        match source {
            Some(path) => tracing::warn!(
                pattern = %pattern,
                file = %path.display(),
                "force-include pattern is unanchored: it matches at any directory \
                 depth and forces a full working-tree scan. Anchor it with a leading \
                 `/` or a path prefix (e.g. `/dist/` or `web-client/dist/`) if you \
                 meant a specific location.",
            ),
            None => tracing::warn!(
                pattern = %pattern,
                "force-include pattern is unanchored: it matches at any directory \
                 depth and forces a full working-tree scan. Anchor it with a leading \
                 `/` or a path prefix (e.g. `/dist/` or `web-client/dist/`) if you \
                 meant a specific location.",
            ),
        }
    }

    PruneInfo {
        any_unanchored,
        unanchored_count: unanchored.len(),
        prefixes,
    }
}

/// The literal leading directory prefix an anchored positive pattern requires its
/// matches to start with, as a list of path segments — or `None` if the pattern is
/// **unanchored** (matches at any depth) and so cannot prune any directory.
///
/// A pattern is unanchored when it has neither a leading `/` nor an interior `/`
/// (gitignore matches such a pattern against the basename at any depth: `dist/`,
/// `*.wasm`, `foo`), or when its literal prefix is empty because a wildcard leads
/// (`**/foo`, `/*.wasm`). Otherwise the prefix is the complete leading segments up
/// to the first wildcard, dropping the partial segment that holds it (`dist*/app`
/// → no complete segment → `None`).
fn prunable_prefix(pattern: &gix::glob::Pattern) -> Option<Vec<BString>> {
    use gix::glob::pattern::Mode;

    // No leading `/` and no interior `/`: matched against the basename anywhere.
    if pattern.mode.contains(Mode::NO_SUB_DIR) && !pattern.mode.contains(Mode::ABSOLUTE) {
        return None;
    }

    // `first_wildcard_pos` indexes into `text` (already stripped of any leading
    // `!`/`/` and trailing `/`). Take the literal head, then keep only whole
    // segments — the segment containing the first wildcard is partial.
    let text: &[u8] = pattern.text.as_ref();
    let literal: &[u8] = match pattern.first_wildcard_pos {
        Some(pos) => match text[..pos].rfind_byte(b'/') {
            Some(slash) => &text[..slash],
            None => &[],
        },
        None => text,
    };

    let segments: Vec<BString> = literal
        .split(|&b| b == b'/')
        .filter(|segment| !segment.is_empty())
        .map(BString::from)
        .collect();

    if segments.is_empty() {
        None
    } else {
        Some(segments)
    }
}

/// Match `rel` against the allow-list, returning `Some(true)` for an include,
/// `Some(false)` for a `!` carve-out, or `None` if no pattern matched.
fn classify(search: &gix::ignore::Search, rel: &BStr, is_dir: bool) -> Option<bool> {
    search
        .pattern_matching_relative_path(rel, Some(is_dir), Case::Sensitive)
        .map(|m| !m.pattern.is_negative())
}

/// Join a repo-relative directory prefix with a child name, using `/` as Git
/// does. An empty prefix yields the bare name (root-level entries).
fn join_rel(prefix: &BStr, name: &BStr) -> BString {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        let mut rel = prefix.to_owned();
        rel.push(b'/');
        rel.push_str(name);
        rel
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Write `contents` to `rel` under `root`, creating parent directories.
    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Run selection and return the matches as sorted `String`s.
    fn select(root: &Path, user: Option<&Path>) -> Vec<String> {
        select_in(root, user)
            .unwrap()
            .paths
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Run the walk under an explicitly supplied `prune` and return the
    /// (sorted/deduped) selection together with the repo-relative prefixes of every
    /// directory the walk descended into. The single walk-construction site shared
    /// by [`select_recording`] and the prune-invariant property test.
    fn walk_under(
        root: &Path,
        user: Option<&Path>,
        prune: &PruneInfo,
    ) -> (Vec<String>, Vec<String>) {
        let search = load_search(root, user).unwrap();
        let mut walk = Walk {
            search: &search,
            prune,
            out: Vec::new(),
            stats: SelectStats::default(),
            entered: Vec::new(),
        };
        walk.run(root, BString::default(), false).unwrap();
        let mut selected: Vec<String> = walk.out.iter().map(ToString::to_string).collect();
        selected.sort();
        selected.dedup();
        let entered: Vec<String> = walk.entered.iter().map(ToString::to_string).collect();
        (selected, entered)
    }

    /// As [`select`], but also return the repo-relative prefixes of every directory
    /// the walk descended into, so a test can assert the prune skipped a subtree
    /// rather than merely failing to match anything inside it.
    fn select_recording(root: &Path, user: Option<&Path>) -> (Vec<String>, Vec<String>) {
        let search = load_search(root, user).unwrap();
        let prune = build_prune_info(&search);
        walk_under(root, user, &prune)
    }

    /// The walk reports what it cost, not just what it found (ADR-0017): an
    /// anchored pattern prunes the unrelated tree, an unanchored one is forced to
    /// walk all of it, and the counters make the difference legible instead of
    /// leaving it as a warning nobody can price.
    #[test]
    fn the_walk_reports_the_cost_of_unanchored_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "dist/app.js", "j");
        for i in 0..5 {
            write(root, &format!("node_modules/pkg{i}/index.js"), "x");
        }

        // Anchored: `node_modules/` can't contain a match, so it is never entered.
        write(root, PROJECT_INCLUDE_FILE, "/dist/\n");
        let anchored = select_in(root, None).unwrap();
        assert_eq!(anchored.paths.len(), 1);
        assert_eq!(anchored.stats.unanchored_patterns, 0);
        assert!(
            anchored.stats.dirs_pruned >= 1,
            "the unrelated tree was pruned: {:?}",
            anchored.stats,
        );

        // Unanchored: the same selection, reached the expensive way.
        write(root, PROJECT_INCLUDE_FILE, "dist/\n");
        let unanchored = select_in(root, None).unwrap();
        assert_eq!(
            unanchored.paths, anchored.paths,
            "the prune never changes what is selected",
        );
        assert_eq!(unanchored.stats.unanchored_patterns, 1);
        assert_eq!(
            unanchored.stats.dirs_pruned, 0,
            "one unanchored pattern disables pruning entirely",
        );
        assert!(
            unanchored.stats.dirs_entered > anchored.stats.dirs_entered,
            "and the walk visits strictly more: {:?} vs {:?}",
            unanchored.stats,
            anchored.stats,
        );
        assert!(
            unanchored.stats.paths_considered > unanchored.paths.len(),
            "far more paths were considered than selected: {:?}",
            unanchored.stats,
        );
    }

    #[test]
    fn no_patterns_selects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "dist/app.js", "j");
        write(dir.path(), "src/main.rs", "m");
        assert!(select(dir.path(), None).is_empty());
    }

    #[test]
    fn directory_pattern_pulls_the_whole_subtree() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), PROJECT_INCLUDE_FILE, "dist/\n");
        write(dir.path(), "dist/app.js", "j");
        write(dir.path(), "dist/nested/app.wasm", "w");
        write(dir.path(), "src/main.rs", "m"); // not under an include
        assert_eq!(
            select(dir.path(), None),
            ["dist/app.js", "dist/nested/app.wasm"],
        );
    }

    #[test]
    fn bang_carves_a_file_back_out_of_an_included_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            PROJECT_INCLUDE_FILE,
            "dist/\n!dist/secret.txt\n",
        );
        write(dir.path(), "dist/app.js", "j");
        write(dir.path(), "dist/secret.txt", "s");
        assert_eq!(select(dir.path(), None), ["dist/app.js"]);
    }

    #[test]
    fn deeply_anchored_pattern_descends_unmatched_parents() {
        // `target/` is never matched as a whole, yet we descend into it to reach
        // the one anchored file — the independence from any ignore tree.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), PROJECT_INCLUDE_FILE, "target/release/app\n");
        write(dir.path(), "target/release/app", "bin");
        write(dir.path(), "target/release/app.d", "dep");
        write(dir.path(), "target/debug/app", "bin");
        assert_eq!(select(dir.path(), None), ["target/release/app"]);
    }

    #[test]
    fn user_layer_carves_out_a_project_include() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), PROJECT_INCLUDE_FILE, "dist/\n");
        write(dir.path(), "dist/app.js", "j");
        write(dir.path(), "dist/private.txt", "p");
        // A separate per-user file (outside the project layer) carves one out.
        let user_dir = tempfile::tempdir().unwrap();
        let user = user_dir.path().join("include");
        fs::write(&user, "!dist/private.txt\n").unwrap();
        assert_eq!(select(dir.path(), Some(&user)), ["dist/app.js"]);
    }

    #[test]
    fn user_layer_adds_its_own_include() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), PROJECT_INCLUDE_FILE, "dist/\n");
        write(dir.path(), "dist/app.js", "j");
        write(dir.path(), "config/local.toml", "x");
        write(dir.path(), "config/other.toml", "y");
        let user_dir = tempfile::tempdir().unwrap();
        let user = user_dir.path().join("include");
        fs::write(&user, "config/local.toml\n").unwrap();
        assert_eq!(
            select(dir.path(), Some(&user)),
            ["config/local.toml", "dist/app.js"],
        );
    }

    #[test]
    fn anchored_pattern_does_not_descend_unrelated_tree() {
        // An anchored include (`web-client/dist/`) must reach its match without
        // traversing a large, unrelated ignored tree like `node_modules`.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), PROJECT_INCLUDE_FILE, "web-client/dist/\n");
        write(dir.path(), "web-client/dist/app.js", "j");
        write(dir.path(), "node_modules/pkg/index.js", "x");
        write(dir.path(), "node_modules/pkg/deep/more.js", "y");

        let (selected, entered) = select_recording(dir.path(), None);
        assert_eq!(selected, ["web-client/dist/app.js"]);
        // The prune skipped `node_modules` entirely — its contents were never read.
        assert!(
            !entered
                .iter()
                .any(|d| d == "node_modules" || d.starts_with("node_modules/")),
            "walk descended into node_modules: {entered:?}",
        );
        // Sanity: it did descend toward the anchored match.
        assert!(entered.iter().any(|d| d == "web-client/dist"));
    }

    #[test]
    fn unanchored_pattern_still_matches_anywhere() {
        // A bare-basename pattern (`*.wasm`) is unanchored: it must keep finding
        // matches at any depth, including under an otherwise-prunable tree.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), PROJECT_INCLUDE_FILE, "*.wasm\n");
        write(dir.path(), "node_modules/pkg/thing.wasm", "w");
        write(dir.path(), "src/main.rs", "m");

        let (selected, entered) = select_recording(dir.path(), None);
        assert_eq!(selected, ["node_modules/pkg/thing.wasm"]);
        // Unanchored => exhaustive fallback: node_modules was descended.
        assert!(entered.iter().any(|d| d == "node_modules/pkg"));
    }

    #[test]
    fn prunable_prefix_classifies_anchoring() {
        fn prefix(pattern: &str) -> Option<Vec<String>> {
            let parsed = gix::glob::Pattern::from_bytes(pattern.as_bytes()).unwrap();
            super::prunable_prefix(&parsed)
                .map(|segments| segments.iter().map(ToString::to_string).collect())
        }
        let segs = |s: &[&str]| Some(s.iter().map(ToString::to_string).collect::<Vec<_>>());

        // Anchored — leading `/` or interior `/` gives a literal directory prefix.
        assert_eq!(prefix("web-client/dist/"), segs(&["web-client", "dist"]));
        assert_eq!(prefix("target/release/**"), segs(&["target", "release"]));
        assert_eq!(prefix("/dist/"), segs(&["dist"]));
        assert_eq!(prefix("/a/b/c"), segs(&["a", "b", "c"]));
        // Unanchored — bare basename, basename-dir, or a leading/partial wildcard.
        assert_eq!(prefix("dist/"), None);
        assert_eq!(prefix("*.wasm"), None);
        assert_eq!(prefix("**/foo"), None);
        assert_eq!(prefix("dist*/app"), None);
        assert_eq!(prefix("foo"), None);
        // Root-anchored-only patterns are safe-but-unoptimised: treated as full walk.
        assert_eq!(prefix("/*.wasm"), None);
    }

    #[test]
    fn can_contain_match_compatibility() {
        let prune = PruneInfo {
            any_unanchored: false,
            unanchored_count: 0,
            prefixes: vec![vec!["web-client".into(), "dist".into()]],
        };
        let can = |s: &str| prune.can_contain_match(BStr::new(s));
        assert!(can("web-client")); // ancestor of the prefix → descend toward it
        assert!(can("web-client/dist")); // equals the prefix
        assert!(can("web-client/dist/sub")); // under the prefix
        assert!(!can("node_modules")); // unrelated
        assert!(!can("web-client/other")); // diverges below the first segment

        // An unanchored set short-circuits to always-descend.
        let exhaustive = PruneInfo {
            any_unanchored: true,
            unanchored_count: 1,
            prefixes: vec![],
        };
        assert!(exhaustive.can_contain_match(BStr::new("anything/at/all")));
    }

    #[test]
    fn build_prune_info_flags_unanchored_and_ignores_negatives() {
        let mut search = gix::ignore::Search::default();
        let parse = gix::ignore::search::Ignore::default();
        // One anchored positive, one unanchored positive, one negative carve-out.
        search.add_patterns_buffer(
            b"web-client/dist/\n*.wasm\n!web-client/dist/secret\n",
            PathBuf::from(PROJECT_INCLUDE_FILE),
            None,
            parse,
        );

        let prune = build_prune_info(&search);
        assert!(
            prune.any_unanchored,
            "`*.wasm` should mark the set unanchored"
        );
        // Only the anchored positive contributes a prefix; the negative is ignored.
        assert_eq!(prune.prefixes.len(), 1);
        assert_eq!(
            prune.prefixes[0]
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["web-client", "dist"],
        );
    }

    #[test]
    fn dot_git_is_never_descended() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), PROJECT_INCLUDE_FILE, "*\n"); // include everything…
        write(dir.path(), ".git/config", "secret");
        write(dir.path(), "keep.txt", "k");
        let got = select(dir.path(), None);
        assert!(got.contains(&"keep.txt".to_string()));
        assert!(!got.iter().any(|p| p.starts_with(".git/")), "got {got:?}",);
    }

    // --- Property test: pruning never changes the selected set -------------------
    //
    // The walk prune (`PruneInfo::can_contain_match`) is a deliberate
    // over-approximation: it changes only *which directories are entered*, never
    // *which files are selected* (see the module docs). We assert that property
    // directly over randomly generated trees and pattern sets by comparing the
    // pruned walk against an exhaustive (prune-disabled) walk.
    mod prune_invariant {
        use super::*;
        use proptest::prelude::*;
        use std::collections::BTreeSet;

        // Disjoint name sets: intermediate path segments come from `DIR_NAMES`,
        // leaf (file) names from `FILE_NAMES`. Keeping them disjoint guarantees no
        // generated file path is an ancestor of another, so materialising the tree
        // never hits a file-vs-directory collision on disk. The names are chosen so
        // the generated patterns below actually match (a shared alphabet — random
        // unique names would make almost every pattern a no-op, a vacuous test).
        const DIR_NAMES: &[&str] = &["a", "b", "dist", "sub"];
        const FILE_NAMES: &[&str] = &["app", "x.wasm", "y.txt", "app.js"];

        // Candidate include patterns spanning every anchoring class the prune
        // distinguishes: anchored multi-segment (`a/dist/`, `a/b/app`, `a/dist/**`),
        // root-anchored (`/dist/`, `/a/b`), interior-wildcard (`a/*/y.txt`),
        // unanchored basename / basename-dir (`dist/`, `app`, `*.wasm`, `sub/`), and
        // leading `**` (`**/app`). Each may be emitted as a `!` carve-out too.
        const PATTERNS: &[&str] = &[
            "a/dist/",
            "a/b/app",
            "a/dist/**",
            "/dist/",
            "/a/b",
            "a/*/y.txt",
            "dist/",
            "app",
            "*.wasm",
            "sub/",
            "**/app",
        ];

        /// A random repo-relative file path: 0–3 directory segments then a file name.
        fn file_path() -> impl Strategy<Value = Vec<&'static str>> {
            (
                proptest::collection::vec(proptest::sample::select(DIR_NAMES.to_vec()), 0..4),
                proptest::sample::select(FILE_NAMES.to_vec()),
            )
                .prop_map(|(mut segments, file)| {
                    segments.push(file);
                    segments
                })
        }

        /// A random include line, optionally negated into a `!` carve-out.
        fn pattern_line() -> impl Strategy<Value = String> {
            (proptest::sample::select(PATTERNS.to_vec()), any::<bool>()).prop_map(
                |(pattern, negate)| {
                    if negate {
                        format!("!{pattern}")
                    } else {
                        pattern.to_string()
                    }
                },
            )
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            /// The pruned walk and an exhaustive (prune-disabled) walk must select
            /// exactly the same files for any tree and pattern set.
            #[test]
            fn prune_never_changes_selection(
                files in proptest::collection::vec(file_path(), 1..12),
                patterns in proptest::collection::vec(pattern_line(), 0..6),
            ) {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path();

                // Materialise the random tree (parents created by `write`).
                for segments in &files {
                    write(root, &segments.join("/"), "x");
                }
                // Project-layer include file from the random patterns.
                let include: String = patterns.iter().map(|p| format!("{p}\n")).collect();
                write(root, PROJECT_INCLUDE_FILE, &include);

                let search = load_search(root, None).unwrap();
                let pruned = build_prune_info(&search);
                // An always-descend prune is exactly the exhaustive walk: every
                // directory (bar `.git`) is entered, nothing is skipped.
                let exhaustive = PruneInfo {
                    any_unanchored: true,
                    unanchored_count: 1,
                    prefixes: Vec::new(),
                };

                let (sel_pruned, entered_pruned) = walk_under(root, None, &pruned);
                let (sel_exhaustive, entered_exhaustive) = walk_under(root, None, &exhaustive);

                // Primary invariant: pruning never changes the selected set.
                prop_assert_eq!(&sel_pruned, &sel_exhaustive);

                // Bonus: the prune only ever *skips* directories, so every directory
                // the pruned walk entered was also entered by the exhaustive walk.
                // This guards against a vacuous pass (e.g. both walks selecting
                // nothing while the prune silently misbehaves).
                let exhaustive_dirs: BTreeSet<&String> = entered_exhaustive.iter().collect();
                for entered in &entered_pruned {
                    prop_assert!(
                        exhaustive_dirs.contains(entered),
                        "pruned walk entered {entered:?} the exhaustive walk did not",
                    );
                }
            }
        }
    }
}
