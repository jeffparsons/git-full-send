//! Integration tests for the client `encode` step (issue #17).
//!
//! Each test builds a temp repo with the `git` CLI, runs [`gfs_client::encode`]
//! under a fixed stream, and inspects the resulting `code` commit — again via the
//! `git` CLI, to keep the assertions independent of the implementation's own
//! library (`gix`).

use std::collections::BTreeMap;
use std::path::Path;

use gfs_client::encode;
use gfs_common::{StreamId, code_ref};
use test_support::{commit_all, git, init_temp_repo, write_file};

/// A fixed stream id so each test's `code` ref is deterministic.
fn test_stream() -> StreamId {
    StreamId::new("test").unwrap()
}

/// The recursive contents of a tree-ish, as `path -> (mode, blob contents)`.
type TreeContents = BTreeMap<String, (String, String)>;

/// List the recursive contents of `tree_ish` as `path -> (mode, contents)`.
fn tree_contents(repo: &Path, tree_ish: &str) -> TreeContents {
    let listing = git(repo, &["ls-tree", "-r", "-z", tree_ish]);
    let mut out = TreeContents::new();
    for record in listing.split('\0').filter(|s| !s.is_empty()) {
        let (meta, path) = record.split_once('\t').expect("ls-tree path separator");
        let mut fields = meta.split_whitespace();
        let mode = fields.next().expect("mode").to_string();
        let _type = fields.next().expect("type");
        let oid = fields.next().expect("oid");
        let contents = git(repo, &["cat-file", "blob", oid]);
        out.insert(path.to_string(), (mode, contents));
    }
    out
}

/// Convenience: a regular-file expectation.
fn blob(contents: &str) -> (String, String) {
    ("100644".to_string(), contents.to_string())
}

#[test]
fn temp_repo_is_a_git_repository() {
    let repo = init_temp_repo();
    assert!(repo.path().join(".git").is_dir(), "`.git` directory exists");
    assert_eq!(
        git(repo.path(), &["rev-parse", "--is-inside-work-tree"]).trim(),
        "true",
    );
}

#[test]
fn code_tree_equals_on_disk_working_state() {
    let repo = init_temp_repo();
    let p = repo.path();

    // Committed baseline.
    write_file(p, "committed.txt", "v1");
    write_file(p, "keep/nested.txt", "nested");
    write_file(p, "to_modify.txt", "orig");
    write_file(p, "to_delete.txt", "bye");
    write_file(p, "to_stage_only.txt", "orig-stage");
    write_file(p, "staged_then_edited.txt", "c0");
    write_file(p, ".gitignore", "ignored.txt\n");
    commit_all(p, "baseline");

    // Working-tree state exercising every case at once.
    write_file(p, "to_modify.txt", "modified"); // tracked, modified, unstaged
    std::fs::remove_file(p.join("to_delete.txt")).unwrap(); // tracked, deleted on disk
    write_file(p, "to_stage_only.txt", "staged-content");
    git(p, &["add", "to_stage_only.txt"]); // staged, worktree == index
    write_file(p, "staged_then_edited.txt", "c1");
    git(p, &["add", "staged_then_edited.txt"]);
    write_file(p, "staged_then_edited.txt", "c2"); // staged, then edited again
    write_file(p, "untracked.txt", "new"); // untracked, not ignored
    write_file(p, "ignored.txt", "secret"); // gitignored -> excluded

    let stream = test_stream();
    let code = code_ref(&stream);
    let outcome = encode(p, &stream).expect("encode succeeds");

    // The ref points at the returned commit, parented on the untouched HEAD.
    assert_eq!(
        git(p, &["rev-parse", &code]).trim(),
        outcome.commit.to_string()
    );
    assert_eq!(outcome.code_ref, code, "outcome reports the ref it wrote");
    assert_eq!(
        git(p, &["rev-parse", &format!("{code}^")]).trim(),
        git(p, &["rev-parse", "HEAD"]).trim(),
        "code commit is parented on HEAD",
    );

    let expected: TreeContents = [
        ("committed.txt".to_string(), blob("v1")),
        ("keep/nested.txt".to_string(), blob("nested")),
        ("to_modify.txt".to_string(), blob("modified")),
        ("to_stage_only.txt".to_string(), blob("staged-content")),
        ("staged_then_edited.txt".to_string(), blob("c2")),
        ("untracked.txt".to_string(), blob("new")),
        (".gitignore".to_string(), blob("ignored.txt\n")),
    ]
    .into_iter()
    .collect();

    assert_eq!(
        tree_contents(p, &code),
        expected,
        "the code tree must equal the on-disk state (deletions and ignores excluded)",
    );
}

#[test]
fn leaves_user_branch_index_and_worktree_untouched() {
    let repo = init_temp_repo();
    let p = repo.path();

    write_file(p, "a.txt", "a");
    write_file(p, "b.txt", "b");
    commit_all(p, "baseline");
    // A dirty mix: staged change, unstaged change, untracked file.
    write_file(p, "a.txt", "a-modified");
    write_file(p, "b.txt", "b-staged");
    git(p, &["add", "b.txt"]);
    write_file(p, "c.txt", "c-untracked");

    let head_before = git(p, &["rev-parse", "HEAD"]);
    let branch_before = git(p, &["rev-parse", "main"]);
    let status_before = git(p, &["status", "--porcelain=v2", "--branch"]);

    let stream = test_stream();
    let code = code_ref(&stream);
    encode(p, &stream).expect("encode succeeds");

    assert_eq!(
        git(p, &["rev-parse", "HEAD"]),
        head_before,
        "HEAD unchanged"
    );
    assert_eq!(
        git(p, &["rev-parse", "main"]),
        branch_before,
        "branch ref unchanged",
    );
    assert_eq!(
        git(p, &["status", "--porcelain=v2", "--branch"]),
        status_before,
        "index and working tree unchanged",
    );
    // The scratch ref now exists.
    assert!(
        !git(p, &["rev-parse", &code]).trim().is_empty(),
        "{code} was written",
    );
}

#[test]
fn encodes_unborn_head_with_untracked_files() {
    let repo = init_temp_repo();
    let p = repo.path();
    // No commits yet: HEAD is unborn.
    write_file(p, "fresh.txt", "hello");

    let stream = test_stream();
    let code = code_ref(&stream);
    let outcome = encode(p, &stream).expect("encode succeeds on an unborn HEAD");

    let parents = git(p, &["rev-list", "--parents", "-n", "1", &code]);
    assert_eq!(
        parents.split_whitespace().count(),
        1,
        "the commit has no parents (only its own id is listed)",
    );
    assert_eq!(
        outcome.commit.to_string(),
        git(p, &["rev-parse", &code]).trim()
    );

    let expected: TreeContents = [("fresh.txt".to_string(), blob("hello"))]
        .into_iter()
        .collect();
    assert_eq!(tree_contents(p, &code), expected);
}

#[cfg(unix)]
#[test]
fn preserves_executable_bit_and_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let repo = init_temp_repo();
    let p = repo.path();

    // Committed executable + committed symlink.
    write_file(p, "script.sh", "#!/bin/sh\necho hi\n");
    std::fs::set_permissions(p.join("script.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    symlink("script.sh", p.join("link-committed")).unwrap();
    commit_all(p, "baseline");

    // An untracked symlink + an untracked executable in the working tree.
    symlink("committed-target", p.join("link-untracked")).unwrap();
    write_file(p, "tool", "#!/bin/sh\n");
    std::fs::set_permissions(p.join("tool"), std::fs::Permissions::from_mode(0o755)).unwrap();

    let stream = test_stream();
    encode(p, &stream).expect("encode succeeds");
    let tree = tree_contents(p, &code_ref(&stream));

    assert_eq!(
        tree["script.sh"].0, "100755",
        "committed exec bit preserved"
    );
    assert_eq!(tree["tool"].0, "100755", "untracked exec bit preserved");
    assert_eq!(
        tree["link-committed"],
        ("120000".to_string(), "script.sh".to_string()),
        "committed symlink preserved as its target",
    );
    assert_eq!(
        tree["link-untracked"],
        ("120000".to_string(), "committed-target".to_string()),
        "untracked symlink preserved as its target",
    );
}

#[test]
fn code_layer_stats_count_the_working_tree_delta() {
    // The metrics record (issue #42) reports the code layer's index→worktree
    // delta: files overlaid + their bytes, and files removed. Unchanged tracked
    // files (the index base) are not counted.
    let repo = init_temp_repo();
    let p = repo.path();

    write_file(p, "unchanged.txt", "stays the same");
    write_file(p, "modify.txt", "orig");
    write_file(p, "delete.txt", "bye");
    commit_all(p, "baseline");

    write_file(p, "modify.txt", "edited!"); // 7 bytes overlaid
    std::fs::remove_file(p.join("delete.txt")).unwrap(); // 1 removed
    write_file(p, "added.txt", "fresh"); // 5 bytes overlaid, untracked

    let outcome = encode(p, &test_stream()).expect("encode succeeds");

    assert_eq!(outcome.stats.files_overlaid, 2, "modify + added");
    assert_eq!(outcome.stats.files_removed, 1, "delete");
    assert_eq!(
        outcome.stats.bytes_overlaid,
        ("edited!".len() + "fresh".len()) as u64,
        "summed bytes of the overlaid files only",
    );

    // The delta is also reported against the scale it came out of, so a slow
    // encode can be attributed to the index's size or to the delta's (ADR-0017).
    assert_eq!(
        outcome.stats.index_entries, 3,
        "the three committed files are the index base",
    );
    assert_eq!(
        outcome.stats.status_items, 3,
        "modify + delete + added were what the status pass yielded; \
         `unchanged.txt` never came up",
    );
    // Phases add up to no more than the whole, and hashing is attributed to the
    // files that were actually read.
    let phases = &outcome.stats.encode_phases;
    assert!(
        phases.hash_ms > 0.0,
        "two files were read and hashed: {phases:?}",
    );
    assert!(
        phases.status_ms >= 0.0 && phases.load_index_ms >= 0.0,
        "the status pass's own cost excludes the hashing it triggered: {phases:?}",
    );
    // The tree/commit ids the record carries match the written ref.
    assert_eq!(
        git(
            p,
            &[
                "rev-parse",
                &format!("{}^{{tree}}", code_ref(&test_stream()))
            ]
        )
        .trim(),
        outcome.tree.to_string(),
    );
}
