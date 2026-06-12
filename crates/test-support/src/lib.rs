//! Internal test helpers for `git-full-send`.
//!
//! Shared fixtures for the integration tests across the workspace. Not
//! published; depended on only as a `dev-dependency`.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

/// Create a fresh temporary directory and initialise an empty Git repository in
/// it, returning the [`TempDir`] guard.
///
/// The repository is given a deterministic identity and default branch so tests
/// do not depend on the developer's (or CI's) global Git configuration.
///
/// Shells out to the `git` CLI, which `git-full-send` assumes is present on both
/// sides (see ADR-0002). The caller must keep the returned [`TempDir`] alive for
/// as long as the repository is needed — dropping it deletes the directory.
///
/// # Panics
///
/// Panics if the temp dir cannot be created or any `git` command fails;
/// intended for use in tests only.
pub fn init_temp_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path();

    git(path, &["init", "--quiet", "--initial-branch=main"]);
    git(path, &["config", "user.name", "git-full-send tests"]);
    git(
        path,
        &["config", "user.email", "tests@git-full-send.invalid"],
    );

    dir
}

/// Run `git` with `args` in `repo`, asserting it succeeds, and return its
/// captured stdout as a `String`.
///
/// # Panics
///
/// Panics if the command cannot be spawned or exits non-zero.
pub fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|e| panic!("spawn `git {}`: {e}", args.join(" ")));
    assert!(
        output.status.success(),
        "`git {}` failed ({}):\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("git stdout is UTF-8")
}

/// Write `contents` to `rel` (relative to `repo`), creating parent directories
/// as needed.
///
/// # Panics
///
/// Panics on any I/O error.
pub fn write_file(repo: &Path, rel: &str, contents: impl AsRef<[u8]>) {
    let path = repo.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dirs");
    }
    std::fs::write(&path, contents).expect("write file");
}

/// Stage everything and create a commit with `message`.
///
/// # Panics
///
/// Panics if either `git` command fails.
pub fn commit_all(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "--message", message]);
}
