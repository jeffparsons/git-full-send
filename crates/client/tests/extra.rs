//! Integration tests for the `extra` (force-included) encode step (issue #20).
//!
//! Each test builds a temp repo with the `git` CLI, runs
//! [`gfs_client::encode_extra`] under a fixed stream, and inspects the resulting
//! `extra` commit/tree via the `git` CLI — keeping the assertions independent of
//! the implementation's own `gix`.
//!
//! These tests drive selection through the **project** pattern file only. The
//! per-user layer is resolved from the environment by `encode_extra`, so it is
//! exercised race-free in `select`'s own unit tests instead; here we assume no
//! interfering real per-user include file (true in CI and on a clean machine).

use std::collections::BTreeSet;
use std::path::Path;

use gfs_client::encode_extra;
use gfs_common::{StreamId, extra_ref, sent_extra_ref};
use test_support::{commit_all, git, init_temp_repo, write_file};

/// Git's canonical empty-tree object id.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A fixed stream id so each test's `extra` ref is deterministic.
fn test_stream() -> StreamId {
    StreamId::new("test").unwrap()
}

/// The recursive set of paths in `tree_ish`.
fn tree_paths(repo: &Path, tree_ish: &str) -> BTreeSet<String> {
    git(repo, &["ls-tree", "-r", "--name-only", tree_ish])
        .lines()
        .map(str::to_string)
        .collect()
}

/// The parent commit ids of `commit_ish` (empty for a rootless commit).
fn parents(repo: &Path, commit_ish: &str) -> Vec<String> {
    let line = git(repo, &["rev-list", "--parents", "-n", "1", commit_ish]);
    // `<commit> <parent1> <parent2> …` — drop the commit itself.
    line.split_whitespace()
        .skip(1)
        .map(str::to_string)
        .collect()
}

#[test]
fn selects_gitignored_build_outputs_with_a_carve_out() {
    let repo = init_temp_repo();
    let p = repo.path();

    // `dist/` is genuinely gitignored; the force-include set pulls it back in,
    // minus a per-file carve-out — independent of Git's ignore tree.
    write_file(p, ".gitignore", "dist/\n");
    write_file(p, ".git-full-send-include", "dist/\n!dist/secret.txt\n");
    commit_all(p, "baseline");
    write_file(p, "dist/app.js", "j");
    write_file(p, "dist/nested/app.wasm", "w");
    write_file(p, "dist/secret.txt", "s");
    write_file(p, "src/main.rs", "fn main() {}"); // not force-included

    let stream = test_stream();
    encode_extra(p, &stream, None).expect("encode_extra succeeds");

    assert_eq!(
        tree_paths(p, &extra_ref(&stream)),
        BTreeSet::from([
            "dist/app.js".to_string(),
            "dist/nested/app.wasm".to_string(),
        ]),
        "gitignored build outputs are selected; the carve-out and unmatched files are not",
    );
}

#[test]
fn empty_selection_still_produces_an_extra_commit() {
    let repo = init_temp_repo();
    let p = repo.path();
    write_file(p, "src/main.rs", "fn main() {}");
    commit_all(p, "baseline");

    // No include file at all: an `extra` commit is still written, with an empty
    // tree, so the chain and the push stay uniform across syncs.
    let stream = test_stream();
    let outcome = encode_extra(p, &stream, None).expect("encode_extra succeeds");

    assert_eq!(
        git(
            p,
            &["rev-parse", &format!("{}^{{tree}}", extra_ref(&stream))]
        )
        .trim(),
        EMPTY_TREE,
        "the extra tree is empty",
    );
    assert_eq!(
        git(p, &["rev-parse", &extra_ref(&stream)]).trim(),
        outcome.commit.to_string(),
        "the extra ref points at the returned commit",
    );
}

#[test]
fn first_extra_commit_is_rootless_then_chains_onto_the_retained_tip() {
    let repo = init_temp_repo();
    let p = repo.path();
    write_file(p, ".git-full-send-include", "dist/\n");
    commit_all(p, "baseline");
    write_file(p, "dist/app.js", "v1");

    let stream = test_stream();

    // First sync: no retained `extra` tip yet, so the commit is rootless.
    let first = encode_extra(p, &stream, None).expect("first encode_extra");
    assert!(
        parents(p, &extra_ref(&stream)).is_empty(),
        "the first extra commit has no parent",
    );

    // Simulate the post-push retention (`sync` advances `sent/extra` only after a
    // successful push) so the next commit can chain onto it.
    git(
        p,
        &[
            "update-ref",
            &sent_extra_ref(&stream),
            &first.commit.to_string(),
        ],
    );

    // Second sync with changed content chains onto the retained tip.
    write_file(p, "dist/app.js", "v2");
    let second = encode_extra(p, &stream, None).expect("second encode_extra");
    assert_ne!(
        first.commit, second.commit,
        "the extra commit advanced with the new content",
    );
    assert_eq!(
        parents(p, &extra_ref(&stream)),
        vec![first.commit.to_string()],
        "the second extra commit is parented on the retained first",
    );
}
