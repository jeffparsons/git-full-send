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
    match user_include_override {
        Some(path) => select_in(workdir, Some(path)),
        None => select_in(workdir, user_include_path().as_deref()),
    }
}

/// The core of [`select_extra_paths`] with the per-user file path supplied
/// explicitly (rather than resolved from the environment), so tests can exercise
/// the two-layer semantics without mutating process-global environment.
fn select_in(workdir: &Path, user_include: Option<&Path>) -> Result<Vec<BString>, SelectError> {
    let search = load_search(workdir, user_include)?;
    let prune = build_prune_info(&search);

    let mut walk = Walk {
        search: &search,
        prune: &prune,
        out: Vec::new(),
        #[cfg(test)]
        entered: Vec::new(),
    };
    walk.run(workdir, BString::default(), false)?;
    let mut selected = walk.out;
    selected.sort();
    selected.dedup();
    Ok(selected)
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
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// As [`select`], but also return the repo-relative prefixes of every directory
    /// the walk descended into, so a test can assert the prune skipped a subtree
    /// rather than merely failing to match anything inside it.
    fn select_recording(root: &Path, user: Option<&Path>) -> (Vec<String>, Vec<String>) {
        let search = load_search(root, user).unwrap();
        let prune = build_prune_info(&search);
        let mut walk = Walk {
            search: &search,
            prune: &prune,
            out: Vec::new(),
            entered: Vec::new(),
        };
        walk.run(root, BString::default(), false).unwrap();
        let mut selected: Vec<String> = walk.out.iter().map(ToString::to_string).collect();
        selected.sort();
        selected.dedup();
        let entered: Vec<String> = walk.entered.iter().map(ToString::to_string).collect();
        (selected, entered)
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
}
