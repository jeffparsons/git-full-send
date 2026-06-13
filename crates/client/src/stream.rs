//! Resolve which [`StreamId`] a sync uses, and persist a generated default.
//!
//! Per ADR-0012 the stream id is resolved in priority order:
//!
//! 1. an explicit id (CLI `--stream-id` / a library argument);
//! 2. else the effective `git-full-send.stream-id` from the repo's Git config
//!    (local, or inherited from global/system if the user set one there);
//! 3. else a freshly generated id, **persisted to the repo's local config** so
//!    every later sync of this repo reuses it.
//!
//! A *generated* (rather than constant) default means two unrelated repos
//! pushing to the same server don't collide by accident — safe behaviour is the
//! default, while a stable, reused id keeps the ADR-0005 delta base intact.
//!
//! Config is read and written via `git config` (a shell-out, symmetric with the
//! `git push` / `git receive-pack` shell-outs elsewhere) rather than gix's
//! config-write path.

use std::path::Path;
use std::process::Command;

use gfs_common::{StreamId, StreamIdError};
use thiserror::Error;

/// The Git config key holding a repo's default stream id.
const CONFIG_KEY: &str = "git-full-send.stream-id";

/// Errors from resolving or persisting the stream id.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StreamResolveError {
    /// An explicitly requested stream id was malformed. (Surfaced when the
    /// caller passes a raw string; a pre-validated [`StreamId`] cannot hit this.)
    #[error(transparent)]
    Invalid(#[from] StreamIdError),
    /// The value stored in `git-full-send.stream-id` is not a valid stream id.
    #[error("the `{CONFIG_KEY}` configured for this repository is invalid")]
    StoredInvalid(#[source] StreamIdError),
    /// Spawning `git config` failed.
    #[error("could not run `git config`")]
    RunGit(#[source] std::io::Error),
    /// `git config` exited unexpectedly (neither success nor the documented
    /// "key not set" status).
    #[error("`git config` failed: {0}")]
    GitConfig(String),
    /// Drawing random bytes for a generated id failed.
    #[error("could not generate a random stream id")]
    Generate(#[source] getrandom::Error),
}

/// Resolve the [`StreamId`] for a sync of the repository at `repo_dir`.
///
/// See the [module docs](self) for the resolution order. When neither an
/// explicit id nor a configured one exists, a new id is generated and persisted
/// to the repo's local config as a side effect.
pub fn resolve_stream(
    repo_dir: &Path,
    requested: Option<StreamId>,
) -> Result<StreamId, StreamResolveError> {
    if let Some(id) = requested {
        return Ok(id);
    }
    if let Some(stored) = read_config(repo_dir)? {
        return StreamId::new(stored).map_err(StreamResolveError::StoredInvalid);
    }
    let id = generate()?;
    write_config(repo_dir, id.as_str())?;
    Ok(id)
}

/// Read the effective `git-full-send.stream-id`, or `None` if unset.
///
/// No scope flag, so a value set in global/system config is honoured too; only
/// the *write* side pins to `--local`.
fn read_config(repo_dir: &Path) -> Result<Option<String>, StreamResolveError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["config", "--get", CONFIG_KEY])
        .output()
        .map_err(StreamResolveError::RunGit)?;
    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if value.is_empty() {
            return Ok(None);
        }
        return Ok(Some(value));
    }
    // `git config --get` exits 1 when the key is simply absent — the common
    // first-run case, not an error.
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    Err(StreamResolveError::GitConfig(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

/// Persist `value` to the repo's **local** config under [`CONFIG_KEY`].
fn write_config(repo_dir: &Path, value: &str) -> Result<(), StreamResolveError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["config", "--local", CONFIG_KEY, value])
        .output()
        .map_err(StreamResolveError::RunGit)?;
    if !output.status.success() {
        return Err(StreamResolveError::GitConfig(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

/// Generate a fresh stream id: 8 random bytes, lowercase-hex encoded.
///
/// Hex keeps the id ref-path-safe (no validation surprises) and the 64 bits of
/// entropy make accidental cross-repo collisions negligible. This is a
/// collision-avoidance token, not a secret.
fn generate() -> Result<StreamId, StreamResolveError> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(StreamResolveError::Generate)?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    // A 16-char hex string is always a valid stream id; new() cannot fail here.
    StreamId::new(hex).map_err(StreamResolveError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        dir
    }

    #[test]
    fn explicit_id_wins_and_does_not_persist() {
        let dir = init_repo();
        let id = StreamId::new("explicit").unwrap();
        let resolved = resolve_stream(dir.path(), Some(id.clone())).unwrap();
        assert_eq!(resolved, id);
        // Nothing was written to config.
        assert!(read_config(dir.path()).unwrap().is_none());
    }

    #[test]
    fn generated_default_is_persisted_and_reused() {
        let dir = init_repo();
        let first = resolve_stream(dir.path(), None).unwrap();
        // Persisted...
        assert_eq!(
            read_config(dir.path()).unwrap().as_deref(),
            Some(first.as_str())
        );
        // ...and reused on the next call.
        let second = resolve_stream(dir.path(), None).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn configured_id_is_used() {
        let dir = init_repo();
        git(dir.path(), &["config", "--local", CONFIG_KEY, "configured"]);
        let resolved = resolve_stream(dir.path(), None).unwrap();
        assert_eq!(resolved.as_str(), "configured");
    }

    #[test]
    fn invalid_stored_id_is_rejected() {
        let dir = init_repo();
        git(dir.path(), &["config", "--local", CONFIG_KEY, "bad//id"]);
        assert!(matches!(
            resolve_stream(dir.path(), None),
            Err(StreamResolveError::StoredInvalid(_))
        ));
    }
}
