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
//! **Performance note.** The walk descends every non-`.git` directory that is not
//! carved out, so an unrelated large ignored tree (e.g. `node_modules`) is
//! traversed even when nothing in it is selected. Research 0004 accepts this
//! O(N·M)-once-per-sync cost over a curated list; pruning subtrees that cannot
//! contain a match is tracked as a follow-up in
//! `docs/follow-ups/prune-force-include-walk.md`.

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
    select_in(workdir, user_include_path().as_deref())
}

/// The core of [`select_extra_paths`] with the per-user file path supplied
/// explicitly (rather than resolved from the environment), so tests can exercise
/// the two-layer semantics without mutating process-global environment.
fn select_in(workdir: &Path, user_include: Option<&Path>) -> Result<Vec<BString>, SelectError> {
    let search = load_search(workdir, user_include)?;

    let mut selected = Vec::new();
    walk_dir(workdir, BString::default(), false, &search, &mut selected)?;
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
    if let Some(user) = user_include {
        if let Some(bytes) = read_optional(user)? {
            search.add_patterns_buffer(&bytes, user, None, parse);
        }
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
    if let Some(p) = std::env::var_os(USER_INCLUDE_ENV) {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
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

/// Recursively walk `dir` (at repo-relative `rel_prefix`), appending the
/// repo-relative paths of selected files to `out`. `inherited` is the
/// included/excluded state propagated from the nearest matched ancestor.
fn walk_dir(
    dir: &Path,
    rel_prefix: BString,
    inherited: bool,
    search: &gix::ignore::Search,
    out: &mut Vec<BString>,
) -> Result<(), SelectError> {
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
            let state = classify(search, rel.as_bstr(), true).unwrap_or(inherited);
            walk_dir(&entry.path(), rel, state, search, out)?;
        } else if (file_type.is_file() || file_type.is_symlink())
            && classify(search, rel.as_bstr(), false).unwrap_or(inherited)
        {
            out.push(rel);
        }
        // Anything else (FIFO, socket, …) is not representable in a tree; skip it.
    }
    Ok(())
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
