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
//! worktree are left exactly as they were — the only ref we move is the
//! stream's `code` ref (`gfs_common::code_ref`).

use std::path::{Path, PathBuf};

use gfs_common::StreamId;
use gix::bstr::{BStr, ByteSlice};
use gix::objs::tree::EntryKind;
use thiserror::Error;

/// Identity stamped on the synthetic commit. It is a scratch artifact for
/// transfer, not user-facing history, so a fixed identity is intentional.
const SYNTH_NAME: &str = "git-full-send";
const SYNTH_EMAIL: &str = "git-full-send@localhost";
const SYNTH_MESSAGE: &str = "git-full-send: working-tree snapshot";
const SYNTH_EXTRA_MESSAGE: &str = "git-full-send: extra (force-included) snapshot";

/// The result of a successful [`encode`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EncodeOutcome {
    /// The commit the stream's `code` ref now points at.
    pub commit: gix::ObjectId,
    /// The tree that commit holds.
    pub tree: gix::ObjectId,
    /// The `code` ref that was written (`gfs_common::code_ref` for the stream).
    pub code_ref: String,
    /// Size metadata for the code layer's working-tree *delta* this sync (issue
    /// #42). It is the delta, not the whole tree: the base is the index, and only
    /// changed/added/removed paths are walked, so the full tree size is not
    /// cheaply available here.
    pub stats: CodeLayerStats,
}

/// Size metadata for the code layer's index→worktree delta in one [`encode`].
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct CodeLayerStats {
    /// Files added or modified (overlaid from disk over the index base).
    pub files_overlaid: usize,
    /// Total content bytes of the overlaid files.
    pub bytes_overlaid: u64,
    /// Files removed from the worktree since the index base.
    pub files_removed: usize,
}

/// The result of a successful [`encode_extra`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ExtraOutcome {
    /// The commit the stream's `extra` ref now points at.
    pub commit: gix::ObjectId,
    /// The tree that commit holds.
    pub tree: gix::ObjectId,
    /// The `extra` ref that was written (`gfs_common::extra_ref` for the stream).
    pub extra_ref: String,
    /// Size metadata for the extra layer (issue #42). Unlike `code`, this is the
    /// *full* selected set each sync, since the whole force-include set is
    /// re-encoded every time.
    pub stats: ExtraLayerStats,
}

/// Size metadata for the full force-include set in one [`encode_extra`].
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct ExtraLayerStats {
    /// Number of force-included files encoded.
    pub files: usize,
    /// Total content bytes of those files.
    pub bytes: u64,
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
    /// Resolving the previous `extra` tip (the parent of the new `extra`
    /// commit) failed.
    #[error("could not resolve the previous `extra` tip")]
    ExtraParent(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Selecting the force-included files failed.
    #[error(transparent)]
    Select(#[from] crate::select::SelectError),
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
    #[error("could not update `{ref_name}`")]
    UpdateRef {
        /// The ref we tried to write.
        ref_name: String,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Encode the current code state of the repository discovered from `repo_dir`
/// into a commit under `stream`'s `code` ref, returning the commit id.
///
/// The user's branch, index, and working tree are left untouched; only the
/// stream's `code` ref (`gfs_common::code_ref`) is created or force-updated.
pub fn encode(repo_dir: &Path, stream: &StreamId) -> Result<EncodeOutcome, EncodeError> {
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

    // Accumulate the code layer's delta size as we overlay (issue #42).
    let mut stats = CodeLayerStats::default();

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
                    stats.files_removed += 1;
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
                    let bytes =
                        overlay_from_disk(&repo, &mut editor, &workdir, rela_path.as_bstr())?;
                    stats.files_overlaid += 1;
                    stats.bytes_overlaid += bytes;
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
                    let bytes =
                        overlay_from_disk(&repo, &mut editor, &workdir, entry.rela_path.as_bstr())?;
                    stats.files_overlaid += 1;
                    stats.bytes_overlaid += bytes;
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

    let commit_id = write_synth_commit(&repo, tree_id, parent, SYNTH_MESSAGE)?;

    let code_ref = gfs_common::code_ref(stream);
    update_ref(
        &repo,
        &code_ref,
        commit_id,
        "git-full-send: encode code state",
    )?;

    Ok(EncodeOutcome {
        commit: commit_id,
        tree: tree_id,
        code_ref,
        stats,
    })
}

/// Encode the force-included (normally-gitignored) files of the repository
/// discovered from `repo_dir` into a commit under `stream`'s `extra` ref,
/// returning the commit id.
///
/// The selected set (ADR-0007, [`crate::select`]) becomes the `extra` tree built
/// with the gix `Editor`; the commit is **parented on the previous sync's
/// retained `extra` tip** (`gfs_common::sent_extra_ref`) so the prior, often
/// large, build outputs stay available as delta bases (ADR-0004/ADR-0005), or is
/// rootless on the first sync. Parenting on the *retained* tip — what the server
/// is known to have — rather than the local `extra` ref means a failed push never
/// leaves a later commit parented on something the server lacks.
///
/// With no patterns / no matches the `extra` tree is empty, but a commit is still
/// produced so the chain (and the push alongside `code`) stays uniform. The
/// user's branch, index, and working tree are untouched; only the stream's
/// `extra` ref (`gfs_common::extra_ref`) is created or force-updated.
///
/// `user_include` overrides the per-user include file (the `--user-include` CLI
/// flag); `None` falls back to the environment-resolved path
/// ([`crate::select::user_include_path`]). The committed project-level file
/// ([`crate::select::PROJECT_INCLUDE_FILE`]) is always consulted regardless.
pub fn encode_extra(
    repo_dir: &Path,
    stream: &StreamId,
    user_include: Option<&Path>,
) -> Result<ExtraOutcome, EncodeError> {
    let repo = gix::discover(repo_dir).map_err(|source| EncodeError::OpenRepo {
        path: repo_dir.to_path_buf(),
        source: Box::new(source),
    })?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| EncodeError::NoWorktree(repo_dir.to_path_buf()))?
        .to_path_buf();

    // Parent on the previously-pushed `extra` tip if we have one, else rootless.
    let sent_extra = gfs_common::sent_extra_ref(stream);
    let parent = match repo
        .try_find_reference(sent_extra.as_str())
        .map_err(|e| EncodeError::ExtraParent(Box::new(e)))?
    {
        Some(mut reference) => Some(
            reference
                .peel_to_id()
                .map_err(|e| EncodeError::ExtraParent(Box::new(e)))?
                .detach(),
        ),
        None => None,
    };

    // Build the `extra` tree from the selected paths, seeded from the empty tree.
    let paths = crate::select::select_extra_paths_with(&workdir, user_include)?;
    let empty_tree = repo.empty_tree().id;
    let mut editor = repo
        .edit_tree(empty_tree)
        .map_err(|e| EncodeError::BuildTree(Box::new(e)))?;
    let mut stats = ExtraLayerStats {
        files: paths.len(),
        bytes: 0,
    };
    for rela_path in &paths {
        stats.bytes += overlay_from_disk(&repo, &mut editor, &workdir, rela_path.as_bstr())?;
    }
    let tree_id = editor
        .write()
        .map_err(|e| EncodeError::BuildTree(Box::new(e)))?
        .detach();

    let commit_id = write_synth_commit(&repo, tree_id, parent, SYNTH_EXTRA_MESSAGE)?;

    let extra_ref = gfs_common::extra_ref(stream);
    update_ref(
        &repo,
        &extra_ref,
        commit_id,
        "git-full-send: encode extra state",
    )?;

    Ok(ExtraOutcome {
        commit: commit_id,
        tree: tree_id,
        extra_ref,
        stats,
    })
}

/// Read the on-disk file (or symlink) at `rela_path`, write it as a blob, and
/// upsert it into `editor` with the mode taken from disk.
///
/// Returns the number of content bytes written (the file's length, or the
/// symlink target's length), or `0` for a path skipped as not representable in a
/// tree — so callers can sum a layer's overlaid size as they go.
fn overlay_from_disk(
    repo: &gix::Repository,
    editor: &mut gix::object::tree::Editor<'_>,
    workdir: &Path,
    rela_path: &BStr,
) -> Result<u64, EncodeError> {
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
        return Ok(0);
    };

    let bytes = content.len() as u64;
    let id = repo
        .write_blob(content)
        .map_err(|e| EncodeError::WriteObject(Box::new(e)))?
        .detach();
    editor
        .upsert(rela_path, kind, id)
        .map_err(|e| EncodeError::BuildTree(Box::new(e)))?;
    Ok(bytes)
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

/// Write a synthetic commit holding `tree_id` with the fixed `git-full-send`
/// identity, the given `parents`, and `message`. Shared by [`encode`] and
/// [`encode_extra`].
fn write_synth_commit(
    repo: &gix::Repository,
    tree_id: gix::ObjectId,
    parents: impl IntoIterator<Item = gix::ObjectId>,
    message: &str,
) -> Result<gix::ObjectId, EncodeError> {
    let signature = gix::actor::Signature {
        name: SYNTH_NAME.into(),
        email: SYNTH_EMAIL.into(),
        time: gix::date::Time::now_local_or_utc(),
    };
    let commit = gix::objs::Commit {
        tree: tree_id,
        parents: parents.into_iter().collect(),
        author: signature.clone(),
        committer: signature,
        encoding: None,
        message: message.into(),
        extra_headers: Vec::new(),
    };
    Ok(repo
        .write_object(&commit)
        .map_err(|e| EncodeError::WriteObject(Box::new(e)))?
        .detach())
}

/// Create or force-update `ref_name` to point at `commit_id`, recording
/// `reflog_message` in the reflog.
///
/// We deliberately use a raw ref transaction with [`PreviousValue::Any`] rather
/// than [`gix::Repository::commit_as`], whose precondition is tied to the
/// commit's first parent and would reject both first-run creation and a scratch
/// ref pointing at a previous synthetic commit.
fn update_ref(
    repo: &gix::Repository,
    ref_name: &str,
    commit_id: gix::ObjectId,
    reflog_message: &str,
) -> Result<(), EncodeError> {
    use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
    use gix::refs::{FullName, Target};

    let name = FullName::try_from(ref_name).map_err(|e| EncodeError::UpdateRef {
        ref_name: ref_name.to_string(),
        source: Box::new(e),
    })?;
    repo.edit_reference(RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: reflog_message.into(),
            },
            expected: PreviousValue::Any,
            new: Target::Object(commit_id),
        },
        name,
        deref: false,
    })
    .map_err(|e| EncodeError::UpdateRef {
        ref_name: ref_name.to_string(),
        source: Box::new(e),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_ref_is_under_the_namespace() {
        let stream = StreamId::new("test").unwrap();
        assert!(gfs_common::code_ref(&stream).starts_with(gfs_common::REF_NAMESPACE));
    }

    #[test]
    fn extra_ref_is_under_the_namespace() {
        let stream = StreamId::new("test").unwrap();
        assert!(gfs_common::extra_ref(&stream).starts_with(gfs_common::REF_NAMESPACE));
    }
}
