//! Encode the developer's current code state into a Git commit.
//!
//! The first building block of `sync` (see ADR-0004): represent the committed
//! history **plus** the working-tree changes (staged & unstaged, collapsed to
//! current on-disk contents) as a single commit written under the scratch ref
//! [`CODE_REF`], parented on `HEAD` — **without** touching the user's branch,
//! index, or working tree.
//!
//! ## How the tree is built
//!
//! We lean on Git's index rather than re-hashing the worktree. The index
//! already caches, per tracked path, the blob id *and* the stat used to decide
//! cheaply whether the worktree copy is still that blob. So:
//!
//! * The **base** tree is the index itself — already the staged state, with
//!   object ids known, costing zero hashing and zero worktree I/O.
//! * We then overlay only the **index → worktree** delta via a single
//!   [`gix::Repository::status`] pass. `gix`'s `index_as_worktree` applies the
//!   same stat shortcut Git uses, so only files Git itself would consider dirty
//!   get read and hashed. Untracked, non-ignored files come from the same
//!   pass's directory walk.
//!
//! Unchanged tracked files are never touched; hashing is bounded by the actual
//! working-tree delta, not the repository size. The index snapshot is read
//! only and never written back, so the user's `.git/index`, branch, and
//! worktree are left exactly as they were — the only ref we move is
//! [`CODE_REF`].

use std::path::{Path, PathBuf};

use gix::bstr::{BStr, ByteSlice};
use gix::objs::tree::EntryKind;
use thiserror::Error;

/// The scratch ref the encoded code commit is written to (under
/// [`gfs_common::REF_NAMESPACE`]).
///
/// Re-exported from [`gfs_common::CODE_REF`] — the canonical definition shared
/// with the server — so `gfs_client::CODE_REF` keeps resolving for callers.
pub use gfs_common::CODE_REF;

/// Identity stamped on the synthetic commit. It is a scratch artifact for
/// transfer, not user-facing history, so a fixed identity is intentional.
const SYNTH_NAME: &str = "git-full-send";
const SYNTH_EMAIL: &str = "git-full-send@localhost";
const SYNTH_MESSAGE: &str = "git-full-send: working-tree snapshot";

/// The result of a successful [`encode`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EncodeOutcome {
    /// The commit [`CODE_REF`] now points at.
    pub commit: gix::ObjectId,
}

/// Errors returned by [`encode`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EncodeError {
    /// No Git repository could be found at or above the given path.
    #[error("could not open a Git repository at or above `{path}`")]
    OpenRepo {
        /// The path encoding started from.
        path: PathBuf,
        /// The underlying `gix` discovery error.
        source: Box<gix::discover::Error>,
    },
    /// The repository has no working tree (it is bare); there is nothing to
    /// encode.
    #[error("repository at `{0}` has no working tree")]
    NoWorktree(PathBuf),
    /// Resolving `HEAD` failed.
    #[error("could not resolve HEAD")]
    Head(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Opening the index failed.
    #[error("could not open the repository index")]
    OpenIndex(#[source] Box<gix::worktree::open_index::Error>),
    /// Reading a worktree file (or symlink target) failed.
    #[error("could not read worktree path `{path}`")]
    ReadWorktree {
        /// The on-disk path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// Computing the index-vs-worktree status failed.
    #[error("could not compute working-tree status")]
    Status(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Building the tree failed.
    #[error("could not build the working-tree tree")]
    BuildTree(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Writing a blob, tree, or commit object failed.
    #[error("could not write a Git object")]
    WriteObject(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Updating the scratch ref failed.
    #[error("could not update `{CODE_REF}`")]
    UpdateRef(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Encode the current code state of the repository discovered from `repo_dir`
/// into a commit under [`CODE_REF`], returning the commit id.
///
/// The user's branch, index, and working tree are left untouched; only
/// [`CODE_REF`] is created or force-updated.
pub fn encode(repo_dir: &Path) -> Result<EncodeOutcome, EncodeError> {
    let repo = gix::discover(repo_dir).map_err(|source| EncodeError::OpenRepo {
        path: repo_dir.to_path_buf(),
        source: Box::new(source),
    })?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| EncodeError::NoWorktree(repo_dir.to_path_buf()))?
        .to_path_buf();

    // The parent of the synthetic commit is the current HEAD. `None` means an
    // unborn HEAD (a fresh repository with no commits yet).
    let parent = repo
        .head()
        .map_err(|e| EncodeError::Head(Box::new(e)))?
        .try_into_peeled_id()
        .map_err(|e| EncodeError::Head(Box::new(e)))?
        .map(|id| id.detach());

    // Base tree = the index (the staged state). Seed an editor from the empty
    // tree and add each index entry from its existing object id and mode — no
    // hashing, no worktree I/O. The index snapshot is never written back.
    // `index_or_empty` so a fresh repo with no index file yet (unborn HEAD,
    // nothing ever staged) is treated as an empty base rather than an error.
    let index = repo
        .index_or_empty()
        .map_err(|e| EncodeError::OpenIndex(Box::new(e)))?;
    let empty_tree = repo.empty_tree().id;
    let mut editor = repo
        .edit_tree(empty_tree)
        .map_err(|e| EncodeError::BuildTree(Box::new(e)))?;
    for entry in index.entries() {
        // Skip conflict (non-zero stage) entries; the status pass below reports
        // conflicted paths and we take their on-disk content there instead.
        if entry.stage() != gix::index::entry::Stage::Unconflicted {
            continue;
        }
        let Some(kind) = entry.mode.to_tree_entry_mode().map(|mode| mode.kind()) else {
            continue;
        };
        editor
            .upsert(entry.path(&index), kind, entry.id)
            .map_err(|e| EncodeError::BuildTree(Box::new(e)))?;
    }

    // Overlay the index → worktree delta in a single status pass. Rename
    // tracking is off, untracked files are emitted individually, and ignored
    // files are left out (those are the separate `extra` ticket's concern).
    let platform = repo
        .status(gix::progress::Discard)
        .map_err(|e| EncodeError::Status(Box::new(e)))?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .index_worktree_rewrites(None);
    let iter = platform
        .into_index_worktree_iter(Vec::new())
        .map_err(|e| EncodeError::Status(Box::new(e)))?;
    for item in iter {
        use gix::status::index_worktree::Item;
        use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};

        let item = item.map_err(|e| EncodeError::Status(Box::new(e)))?;
        match item {
            Item::Modification {
                rela_path, status, ..
            } => match status {
                // The file is gone from the worktree.
                EntryStatus::Change(Change::Removed) => {
                    editor
                        .remove(rela_path.as_bstr())
                        .map_err(|e| EncodeError::BuildTree(Box::new(e)))?;
                }
                // A modified submodule: keep the index's gitlink unchanged.
                EntryStatus::Change(Change::SubmoduleModification(_)) => {}
                // The content (or type, or just the exec bit) changed, the
                // entry is intent-to-add, or it is conflicted: in every case
                // the on-disk file is the source of truth.
                EntryStatus::Change(Change::Modification { .. })
                | EntryStatus::Change(Change::Type { .. })
                | EntryStatus::IntentToAdd
                | EntryStatus::Conflict { .. } => {
                    overlay_from_disk(&repo, &mut editor, &workdir, rela_path.as_bstr())?;
                }
                // Content is unchanged; the base (index) entry already holds it.
                EntryStatus::NeedsUpdate(_) => {}
            },
            Item::DirectoryContents { entry, .. }
                if entry.status == gix::dir::entry::Status::Untracked =>
            {
                if let Some(gix::dir::entry::Kind::File | gix::dir::entry::Kind::Symlink) =
                    entry.disk_kind
                {
                    overlay_from_disk(&repo, &mut editor, &workdir, entry.rela_path.as_bstr())?;
                }
            }
            // Rename tracking is disabled, so rewrites are not expected; other
            // directory-walk entries (ignored, tracked, pruned) are irrelevant.
            Item::DirectoryContents { .. } | Item::Rewrite { .. } => {}
        }
    }

    let tree_id = editor
        .write()
        .map_err(|e| EncodeError::BuildTree(Box::new(e)))?
        .detach();

    let signature = gix::actor::Signature {
        name: SYNTH_NAME.into(),
        email: SYNTH_EMAIL.into(),
        time: gix::date::Time::now_local_or_utc(),
    };
    let commit = gix::objs::Commit {
        tree: tree_id,
        parents: parent.into_iter().collect(),
        author: signature.clone(),
        committer: signature,
        encoding: None,
        message: SYNTH_MESSAGE.into(),
        extra_headers: Vec::new(),
    };
    let commit_id = repo
        .write_object(&commit)
        .map_err(|e| EncodeError::WriteObject(Box::new(e)))?
        .detach();

    update_code_ref(&repo, commit_id)?;

    Ok(EncodeOutcome { commit: commit_id })
}

/// Read the on-disk file (or symlink) at `rela_path`, write it as a blob, and
/// upsert it into `editor` with the mode taken from disk.
fn overlay_from_disk(
    repo: &gix::Repository,
    editor: &mut gix::object::tree::Editor<'_>,
    workdir: &Path,
    rela_path: &BStr,
) -> Result<(), EncodeError> {
    let abs = workdir.join(gix::path::from_bstr(rela_path));
    let meta = std::fs::symlink_metadata(&abs).map_err(|source| EncodeError::ReadWorktree {
        path: abs.clone(),
        source,
    })?;
    let file_type = meta.file_type();

    let (kind, content) = if file_type.is_symlink() {
        let target = std::fs::read_link(&abs).map_err(|source| EncodeError::ReadWorktree {
            path: abs.clone(),
            source,
        })?;
        (
            EntryKind::Link,
            gix::path::into_bstr(target).into_owned().into(),
        )
    } else if file_type.is_file() {
        let bytes = std::fs::read(&abs).map_err(|source| EncodeError::ReadWorktree {
            path: abs.clone(),
            source,
        })?;
        (blob_kind(&meta), bytes)
    } else {
        // Not representable in a tree (e.g. a FIFO or socket): skip it.
        return Ok(());
    };

    let id = repo
        .write_blob(content)
        .map_err(|e| EncodeError::WriteObject(Box::new(e)))?
        .detach();
    editor
        .upsert(rela_path, kind, id)
        .map_err(|e| EncodeError::BuildTree(Box::new(e)))?;
    Ok(())
}

/// Classify a regular file as an executable or plain blob.
///
/// Unix consults the owner-execute bit; elsewhere the bit is not meaningful, so
/// we default to a plain blob.
#[cfg(unix)]
fn blob_kind(meta: &std::fs::Metadata) -> EntryKind {
    use std::os::unix::fs::MetadataExt;
    if meta.mode() & 0o100 != 0 {
        EntryKind::BlobExecutable
    } else {
        EntryKind::Blob
    }
}

#[cfg(not(unix))]
fn blob_kind(_meta: &std::fs::Metadata) -> EntryKind {
    EntryKind::Blob
}

/// Create or force-update [`CODE_REF`] to point at `commit_id`.
///
/// We deliberately use a raw ref transaction with [`PreviousValue::Any`] rather
/// than [`gix::Repository::commit_as`], whose precondition is tied to the
/// commit's first parent and would reject both first-run creation and a scratch
/// ref pointing at a previous synthetic commit.
fn update_code_ref(repo: &gix::Repository, commit_id: gix::ObjectId) -> Result<(), EncodeError> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
    use gix::refs::{FullName, Target};

    let name = FullName::try_from(CODE_REF).map_err(|e| EncodeError::UpdateRef(Box::new(e)))?;
    repo.edit_reference(RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: "git-full-send: encode code state".into(),
            },
            expected: PreviousValue::Any,
            new: Target::Object(commit_id),
        },
        name,
        deref: false,
    })
    .map_err(|e| EncodeError::UpdateRef(Box::new(e)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_ref_is_under_the_namespace() {
        assert!(CODE_REF.starts_with(gfs_common::REF_NAMESPACE));
    }
}
