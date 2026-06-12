//! Internal test helpers for `git-full-send`.
//!
//! Shared fixtures for the integration tests across the workspace. Not
//! published; depended on only as a `dev-dependency`.

use std::process::Command;

use tempfile::TempDir;

/// Create a fresh temporary directory and initialise an empty Git repository in
/// it, returning the [`TempDir`] guard.
///
/// Shells out to the `git` CLI, which `git-full-send` assumes is present on both
/// sides (see ADR-0002). The caller must keep the returned [`TempDir`] alive for
/// as long as the repository is needed — dropping it deletes the directory.
///
/// # Panics
///
/// Panics if the temp dir cannot be created or `git init` fails; intended for
/// use in tests only.
pub fn init_temp_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("create temp dir");

    let status = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(dir.path())
        .status()
        .expect("run `git init`");
    assert!(status.success(), "`git init` failed: {status}");

    dir
}
