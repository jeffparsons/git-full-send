//! Loopback integration tests for the transfer leg (issue #18).
//!
//! Each test stands up a `gfs_server` listener against a temp bare "remote"
//! repo on an ephemeral localhost port, runs the client `sync` (or a raw push)
//! from a temp "client" repo, and inspects the result via the `git` CLI — keeping
//! the assertions independent of the implementation's own `gix`/transport code.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;

use gfs_common::{StreamId, code_ref, extra_ref, sent_extra_ref, sent_ref};
use test_support::{commit_all, git, init_bare_repo, init_temp_repo, write_file};

/// A fixed stream id for tests that only need one stream, so the produced refs
/// are deterministic.
fn test_stream() -> StreamId {
    StreamId::new("test").unwrap()
}

/// Bind a listener for `repo` on an ephemeral localhost port and serve it as a
/// task on the caller's Tokio runtime, returning the bound address.
///
/// The server shares the test's runtime with the in-process client: `push_refs`
/// is async (it `.await`s the receive-pack exchange), so a current-thread runtime
/// interleaves the two. The task is detached and stops when the test process
/// exits — the same fire-and-forget lifecycle the old `std::thread` helper had.
fn start_server(repo: &Path) -> SocketAddr {
    let listener = gfs_server::bind("127.0.0.1:0".parse().unwrap(), repo.to_path_buf())
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = gfs_server::serve_async(
            listener,
            gfs_server::ListenConfig::default(),
            std::future::pending::<()>(),
        )
        .await;
    });
    addr
}

/// The recursive set of paths in `tree_ish` (run with cwd inside the repo).
fn tree_paths(repo: &Path, tree_ish: &str) -> BTreeSet<String> {
    git(repo, &["ls-tree", "-r", "--name-only", tree_ish])
        .lines()
        .map(str::to_string)
        .collect()
}

/// The set of file paths under `dir`, relative to it and `/`-separated.
fn worktree_files(dir: &Path) -> BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).expect("read worktree dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).expect("under root");
                out.insert(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(dir, dir, &mut out);
    out
}

#[tokio::test]
async fn push_lands_code_ref_and_objects() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "committed.txt", "v1");
    commit_all(c, "baseline");
    // Dirty working-tree state the encode step folds in.
    write_file(c, "committed.txt", "modified");
    write_file(c, "untracked.txt", "new");

    let stream = test_stream();
    let code = code_ref(&stream);
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("sync succeeds");

    // The server's `code` ref now matches the client's encoded tip…
    let client_tip = git(c, &["rev-parse", &code]);
    let server_tip = git(server.path(), &["rev-parse", &code]);
    assert_eq!(
        server_tip.trim(),
        client_tip.trim(),
        "server has the code tip"
    );

    // …and the objects landed: the tree is walkable on the server with the
    // expected working-tree contents.
    assert_eq!(
        tree_paths(server.path(), &code),
        BTreeSet::from(["committed.txt".to_string(), "untracked.txt".to_string()]),
    );
    assert_eq!(
        git(
            server.path(),
            &["cat-file", "blob", &format!("{code}:committed.txt")]
        ),
        "modified",
    );
}

#[tokio::test]
async fn push_lands_extra_ref_alongside_code() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    // A force-include set pulling a gitignored build output into `extra`.
    write_file(c, ".gitignore", "dist/\n");
    write_file(c, ".git-full-send-include", "dist/\n");
    commit_all(c, "baseline");
    write_file(c, "dist/app.js", "built");

    let stream = test_stream();
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("sync succeeds");

    // The `extra` ref landed on the server in the same exchange as `code`, with
    // the selected build output at its identity path…
    let extra = extra_ref(&stream);
    assert_eq!(
        tree_paths(server.path(), &extra),
        BTreeSet::from(["dist/app.js".to_string()]),
        "the server has the extra tree",
    );
    assert_eq!(
        git(
            server.path(),
            &["cat-file", "blob", &format!("{extra}:dist/app.js")]
        ),
        "built",
    );
    // …the server `code` ref does not carry the gitignored output…
    assert!(
        !tree_paths(server.path(), &code_ref(&stream)).contains("dist/app.js"),
        "the gitignored output rides in `extra`, not `code`",
    );
    // …and the client retained the pushed `extra` tip as the next delta base.
    assert_eq!(
        git(c, &["rev-parse", &sent_extra_ref(&stream)]).trim(),
        git(c, &["rev-parse", &extra]).trim(),
        "the retention ref pins the pushed extra tip",
    );
}

#[tokio::test]
async fn retains_pushed_tip_on_the_client() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "a.txt", "a");
    commit_all(c, "baseline");

    let stream = test_stream();
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("sync succeeds");

    assert_eq!(
        git(c, &["rev-parse", &sent_ref(&stream)]).trim(),
        git(c, &["rev-parse", &code_ref(&stream)]).trim(),
        "the retention ref pins the pushed code tip",
    );
}

#[tokio::test]
async fn rejects_refs_outside_the_namespace() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "a.txt", "a");
    commit_all(c, "baseline");
    // Give the client a code ref to push (a namespaced ref that is accepted).
    let code = code_ref(&test_stream());
    git(c, &["update-ref", &code, "HEAD"]);
    let remote = addr.to_string();

    // A namespaced ref is accepted…
    gfs_client::push_ref(c, &remote, &code)
        .await
        .expect("a refs/git-full-send/* push is accepted");
    // …but anything outside the namespace is declined by the pre-receive hook.
    assert!(
        gfs_client::push_ref(c, &remote, "refs/heads/main")
            .await
            .is_err(),
        "a non-namespaced push is rejected",
    );
    assert!(
        !Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", "refs/heads/main"])
            .current_dir(server.path())
            .status()
            .expect("run git rev-parse")
            .success(),
        "the rejected ref was not created on the server",
    );
}

#[tokio::test]
async fn second_sync_advances_the_server() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "a.txt", "one");
    commit_all(c, "baseline");
    let stream = test_stream();
    let code = code_ref(&stream);
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("first sync");
    let first = git(server.path(), &["rev-parse", &code]);

    // Change the working tree and sync again (the retained tip is the base).
    write_file(c, "a.txt", "two");
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("second sync");
    let second = git(server.path(), &["rev-parse", &code]);

    assert_ne!(first.trim(), second.trim(), "the server code ref advanced");
    assert_eq!(
        git(
            server.path(),
            &["cat-file", "blob", &format!("{code}:a.txt")]
        ),
        "two",
        "the server has the latest content",
    );
}

#[tokio::test]
async fn update_worktree_makes_worktree_match_code() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "keep.txt", "v1");
    write_file(c, "data.txt", "v1");
    commit_all(c, "baseline");
    // An untracked working-tree file the encode folds in.
    write_file(c, "new.txt", "v1");

    let stream = test_stream();
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("sync succeeds");

    // A disposable worktree pre-seeded with bait: a remote-side edit to a file
    // whose synced blob is unchanged, and a stale file absent from the tree.
    let worktree = tempfile::tempdir().expect("worktree dir");
    let wt = worktree.path();
    write_file(wt, "keep.txt", "REMOTE-EDIT");
    write_file(wt, "stale.txt", "junk");

    gfs_server::update_worktree(
        server.path().to_path_buf(),
        wt.to_path_buf(),
        stream.clone(),
        gfs_server::LockMode::default(),
    )
    .await
    .expect("update-worktree succeeds");

    // The worktree matches the synced tree exactly…
    assert_eq!(
        worktree_files(wt),
        tree_paths(server.path(), &code_ref(&stream)),
        "worktree contents equal the synced code tree",
    );
    // …the stale file is gone…
    assert!(!wt.join("stale.txt").exists(), "stale file was removed");
    // …and the pre-existing remote-local edit was stomped.
    assert_eq!(
        std::fs::read_to_string(wt.join("keep.txt")).expect("read keep.txt"),
        "v1",
        "the remote-local edit was overwritten",
    );
}

#[tokio::test]
async fn update_worktree_removes_files_dropped_between_syncs() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "keep.txt", "v1");
    write_file(c, "gone.txt", "v1");
    commit_all(c, "baseline");

    // First sync + checkout: the worktree gains gone.txt.
    let stream = test_stream();
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("first sync");
    let worktree = tempfile::tempdir().expect("worktree dir");
    let wt = worktree.path();
    gfs_server::update_worktree(
        server.path().to_path_buf(),
        wt.to_path_buf(),
        stream.clone(),
        gfs_server::LockMode::default(),
    )
    .await
    .expect("first update-worktree");
    assert!(
        wt.join("gone.txt").exists(),
        "gone.txt is checked out first"
    );

    // Delete gone.txt on the client and sync again; the same worktree updates.
    std::fs::remove_file(c.join("gone.txt")).expect("remove gone.txt");
    commit_all(c, "drop gone.txt");
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("second sync");
    gfs_server::update_worktree(
        server.path().to_path_buf(),
        wt.to_path_buf(),
        stream.clone(),
        gfs_server::LockMode::default(),
    )
    .await
    .expect("second update-worktree");

    assert!(
        !wt.join("gone.txt").exists(),
        "a file dropped between syncs is removed from the worktree",
    );
    assert_eq!(
        worktree_files(wt),
        tree_paths(server.path(), &code_ref(&stream)),
        "worktree still matches the synced tree exactly",
    );
}

#[tokio::test]
async fn update_worktree_overlays_extra_at_identity_paths() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    // Committed code plus a gitignored build output force-included into `extra`.
    write_file(c, "src/main.rs", "fn main() {}");
    write_file(c, ".gitignore", "dist/\n");
    write_file(c, ".git-full-send-include", "dist/\n");
    commit_all(c, "baseline");
    write_file(c, "dist/app.js", "built");

    let stream = test_stream();
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("sync succeeds");

    let worktree = tempfile::tempdir().expect("worktree dir");
    let wt = worktree.path();
    gfs_server::update_worktree(
        server.path().to_path_buf(),
        wt.to_path_buf(),
        stream.clone(),
        gfs_server::LockMode::default(),
    )
    .await
    .expect("update-worktree succeeds");

    // The force-included build output lands over the code checkout at its
    // identity path, with its content intact…
    assert_eq!(
        std::fs::read_to_string(wt.join("dist/app.js")).expect("read dist/app.js"),
        "built",
        "the force-included file lands at its identity path",
    );
    // …and the worktree is exactly the union of the code and extra trees (the
    // overlay added the extra file without disturbing the code checkout).
    let mut expected = tree_paths(server.path(), &code_ref(&stream));
    expected.extend(tree_paths(server.path(), &extra_ref(&stream)));
    assert_eq!(
        worktree_files(wt),
        expected,
        "worktree equals the code tree overlaid with the extra tree",
    );
}

#[tokio::test]
async fn update_worktree_removes_extra_dropped_between_syncs() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "keep.txt", "code");
    write_file(c, ".gitignore", "dist/\n");
    write_file(c, ".git-full-send-include", "dist/\n");
    commit_all(c, "baseline");
    write_file(c, "dist/app.js", "built");
    write_file(c, "dist/vendor.js", "vendored");

    // First sync + checkout: both force-included files land in the worktree.
    let stream = test_stream();
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("first sync");
    let worktree = tempfile::tempdir().expect("worktree dir");
    let wt = worktree.path();
    gfs_server::update_worktree(
        server.path().to_path_buf(),
        wt.to_path_buf(),
        stream.clone(),
        gfs_server::LockMode::default(),
    )
    .await
    .expect("first update-worktree");
    assert!(wt.join("dist/app.js").exists(), "app.js checked out first");
    assert!(
        wt.join("dist/vendor.js").exists(),
        "vendor.js checked out first"
    );

    // Drop one force-included file from the selection and re-sync the same worktree.
    std::fs::remove_file(c.join("dist/vendor.js")).expect("remove vendor.js");
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("second sync");
    gfs_server::update_worktree(
        server.path().to_path_buf(),
        wt.to_path_buf(),
        stream.clone(),
        gfs_server::LockMode::default(),
    )
    .await
    .expect("second update-worktree");

    // The dropped extra file is gone…
    assert!(
        !wt.join("dist/vendor.js").exists(),
        "an extra file dropped between syncs is removed from the worktree",
    );
    // …while the surviving extra file and the code-tree file are untouched.
    assert_eq!(
        std::fs::read_to_string(wt.join("dist/app.js")).expect("read app.js"),
        "built",
        "the surviving extra file remains",
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("keep.txt")).expect("read keep.txt"),
        "code",
        "the code-tree file is unaffected",
    );
    // The worktree is exactly the union of the (now smaller) code and extra trees.
    let mut expected = tree_paths(server.path(), &code_ref(&stream));
    expected.extend(tree_paths(server.path(), &extra_ref(&stream)));
    assert_eq!(
        worktree_files(wt),
        expected,
        "worktree matches the code tree overlaid with the extra tree",
    );
}

#[tokio::test]
async fn two_streams_do_not_clobber_each_other() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    // Two independent clients sync different content under different streams.
    let alice = init_temp_repo();
    write_file(alice.path(), "who.txt", "alice");
    commit_all(alice.path(), "alice baseline");
    let alice_stream = StreamId::new("alice").unwrap();
    gfs_client::sync(
        alice.path().to_path_buf(),
        addr.to_string(),
        Some(alice_stream.clone()),
        None,
    )
    .await
    .expect("alice sync");

    let bob = init_temp_repo();
    write_file(bob.path(), "who.txt", "bob");
    commit_all(bob.path(), "bob baseline");
    let bob_stream = StreamId::new("bob").unwrap();
    gfs_client::sync(
        bob.path().to_path_buf(),
        addr.to_string(),
        Some(bob_stream.clone()),
        None,
    )
    .await
    .expect("bob sync");

    // Both streams' code refs coexist with their own content — no clobbering.
    assert_eq!(
        git(
            server.path(),
            &[
                "cat-file",
                "blob",
                &format!("{}:who.txt", code_ref(&alice_stream))
            ]
        ),
        "alice",
    );
    assert_eq!(
        git(
            server.path(),
            &[
                "cat-file",
                "blob",
                &format!("{}:who.txt", code_ref(&bob_stream))
            ]
        ),
        "bob",
    );

    // And each stream checks out independently into its own worktree.
    for (stream, expected) in [(&alice_stream, "alice"), (&bob_stream, "bob")] {
        let wt = tempfile::tempdir().expect("worktree dir");
        gfs_server::update_worktree(
            server.path().to_path_buf(),
            wt.path().to_path_buf(),
            stream.clone(),
            gfs_server::LockMode::default(),
        )
        .await
        .expect("update-worktree");
        assert_eq!(
            std::fs::read_to_string(wt.path().join("who.txt")).unwrap(),
            expected,
        );
    }

    // `list_streams` reports exactly the two synced streams.
    let mut listed: Vec<String> = gfs_server::list_streams(server.path())
        .expect("list streams")
        .iter()
        .map(|s| s.as_str().to_string())
        .collect();
    listed.sort();
    assert_eq!(listed, vec!["alice".to_string(), "bob".to_string()]);
}

/// Whether `ref_name` resolves in `repo` (a non-asserting `git rev-parse`).
fn ref_exists(repo: &Path, ref_name: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", ref_name])
        .output()
        .expect("run git rev-parse")
        .status
        .success()
}

#[tokio::test]
async fn forget_stream_removes_a_streams_server_refs_only(/* issue #48 */) {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    // Two streams synced from independent clients.
    for who in ["alice", "bob"] {
        let client = init_temp_repo();
        write_file(client.path(), "who.txt", who);
        commit_all(client.path(), "baseline");
        gfs_client::sync(
            client.path().to_path_buf(),
            addr.to_string(),
            Some(StreamId::new(who).unwrap()),
            None,
        )
        .await
        .expect("sync");
    }
    let alice = StreamId::new("alice").unwrap();
    let bob = StreamId::new("bob").unwrap();

    // Forgetting `alice` removes its `code` and `extra` refs (2) and nothing else.
    let removed = gfs_server::forget_stream(server.path(), &alice).expect("forget alice");
    assert_eq!(removed, 2, "alice's code + extra refs were removed");
    assert!(!ref_exists(server.path(), &code_ref(&alice)));
    assert!(!ref_exists(server.path(), &extra_ref(&alice)));

    // `bob` is untouched and is now the only stream the server lists.
    assert!(ref_exists(server.path(), &code_ref(&bob)));
    assert_eq!(
        gfs_server::list_streams(server.path()).unwrap(),
        vec![bob],
        "only bob remains after forgetting alice",
    );
}

#[tokio::test]
async fn forget_stream_drops_client_local_sent_refs(/* issue #48 */) {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "f.txt", "v1");
    commit_all(c, "baseline");
    let stream = test_stream();
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("first sync");

    // After a sync the client holds the stream's local refs: the scratch
    // `code`/`extra` refs `encode` pushes from, plus the `sent/*` delta-base pins.
    for r in [
        code_ref(&stream),
        extra_ref(&stream),
        sent_ref(&stream),
        sent_extra_ref(&stream),
    ] {
        assert!(ref_exists(c, &r), "`{r}` exists after sync");
    }

    // Forgetting the stream *in the client repo* drops all of them — there is no
    // local footprint of the stream left behind.
    let removed = gfs_server::forget_stream(c, &stream).expect("forget on client");
    assert_eq!(
        removed, 4,
        "code + extra + sent/code + sent/extra were removed"
    );
    for r in [
        code_ref(&stream),
        extra_ref(&stream),
        sent_ref(&stream),
        sent_extra_ref(&stream),
    ] {
        assert!(!ref_exists(c, &r), "`{r}` is gone after forget");
    }

    // Forgetting locally is safe: a subsequent sync regenerates them and succeeds.
    write_file(c, "f.txt", "v2");
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("sync after local forget");
    assert!(ref_exists(c, &sent_ref(&stream)), "sent/code regenerated");
}

#[tokio::test]
async fn forget_stream_is_idempotent_for_an_unknown_stream(/* issue #48 */) {
    let server = init_bare_repo();
    // Never synced: forgetting it removes nothing and is not an error.
    let removed = gfs_server::forget_stream(server.path(), &StreamId::new("ghost").unwrap())
        .expect("forget unknown stream");
    assert_eq!(removed, 0);
}

#[tokio::test]
async fn branch_shaped_stream_id_round_trips() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "f.txt", "v1");
    commit_all(c, "baseline");

    // A slash-containing (branch-shaped) id must survive encode → push → checkout.
    let stream = StreamId::new("feature/foo").unwrap();
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("sync succeeds");

    let wt = tempfile::tempdir().expect("worktree dir");
    gfs_server::update_worktree(
        server.path().to_path_buf(),
        wt.path().to_path_buf(),
        stream.clone(),
        gfs_server::LockMode::default(),
    )
    .await
    .expect("update-worktree succeeds");
    assert_eq!(
        std::fs::read_to_string(wt.path().join("f.txt")).unwrap(),
        "v1",
    );
    assert_eq!(
        gfs_server::list_streams(server.path()).unwrap(),
        vec![stream],
        "the slash-shaped id is recovered intact",
    );
}

#[tokio::test]
async fn default_stream_is_generated_persisted_and_reused() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "a.txt", "one");
    commit_all(c, "baseline");

    // No explicit stream: one is generated and persisted to the repo config.
    gfs_client::sync(c.to_path_buf(), addr.to_string(), None, None)
        .await
        .expect("first sync");
    let id = git(
        c,
        &["config", "--local", "--get", "git-full-send.stream-id"],
    )
    .trim()
    .to_string();
    assert!(!id.is_empty(), "a default stream id was persisted");

    let stream = StreamId::new(id).unwrap();
    let code = code_ref(&stream);
    let first = git(server.path(), &["rev-parse", &code]);

    // A second default sync reuses the same stream (the server ref advances in
    // place rather than spawning a second stream).
    write_file(c, "a.txt", "two");
    gfs_client::sync(c.to_path_buf(), addr.to_string(), None, None)
        .await
        .expect("second sync");
    let second = git(server.path(), &["rev-parse", &code]);
    assert_ne!(first.trim(), second.trim(), "same stream ref advanced");
    assert_eq!(
        gfs_server::list_streams(server.path()).unwrap(),
        vec![stream],
        "only one stream exists after two default syncs",
    );
}

#[tokio::test]
async fn update_worktree_without_a_synced_stream_errors() {
    let server = init_bare_repo();

    let wt = tempfile::tempdir().expect("worktree dir");
    let err = gfs_server::update_worktree(
        server.path().to_path_buf(),
        wt.path().to_path_buf(),
        StreamId::new("never-synced").unwrap(),
        gfs_server::LockMode::default(),
    )
    .await
    .expect_err("checking out a never-synced stream fails");
    assert!(
        matches!(err, gfs_server::ServerError::MissingCodeRef { .. }),
        "got {err:?}",
    );
}

// --- Per-worktree locking (issue #49) -------------------------------------

/// Stand up a server with one synced stream, returning the server repo guard,
/// its address, and the stream id. The shared preamble for the locking tests.
async fn server_with_synced_stream() -> (tempfile::TempDir, SocketAddr, StreamId) {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "data.txt", "v1");
    commit_all(c, "baseline");

    let stream = test_stream();
    gfs_client::sync(
        c.to_path_buf(),
        addr.to_string(),
        Some(stream.clone()),
        None,
    )
    .await
    .expect("sync succeeds");

    (server, addr, stream)
}

/// Open and exclusively `flock` `worktree`'s lock file, returning the guard so
/// the caller can simulate a concurrent `update-worktree` holding it. Dropping
/// (or `unlock`ing) the returned handle releases the lock.
fn hold_worktree_lock(repo: &Path, worktree: &Path) -> std::fs::File {
    let path = gfs_server::worktree_lock_path(repo, worktree).expect("lock path");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .expect("open lock file");
    file.try_lock().expect("acquire the test-side lock");
    file
}

#[tokio::test]
async fn update_worktree_fails_fast_when_locked() {
    let (server, _addr, stream) = server_with_synced_stream().await;

    // A first update establishes the worktree (and its lock file).
    let worktree = tempfile::tempdir().expect("worktree dir");
    let wt = worktree.path();
    gfs_server::update_worktree(
        server.path().to_path_buf(),
        wt.to_path_buf(),
        stream.clone(),
        gfs_server::LockMode::FailFast,
    )
    .await
    .expect("first update succeeds");

    // Simulate a concurrent run by holding the lock, then a default (fail-fast)
    // update must bounce off it rather than interleave its git steps.
    let _held = hold_worktree_lock(server.path(), wt);
    let err = gfs_server::update_worktree(
        server.path().to_path_buf(),
        wt.to_path_buf(),
        stream.clone(),
        gfs_server::LockMode::FailFast,
    )
    .await
    .expect_err("a busy worktree fails fast");
    assert!(
        matches!(err, gfs_server::ServerError::WorktreeBusy { .. }),
        "got {err:?}",
    );
}

#[tokio::test]
async fn update_worktree_wait_times_out_when_held() {
    let (server, _addr, stream) = server_with_synced_stream().await;

    let worktree = tempfile::tempdir().expect("worktree dir");
    let wt = worktree.path();
    let _held = hold_worktree_lock(server.path(), wt);

    let timeout = std::time::Duration::from_millis(250);
    let start = std::time::Instant::now();
    let err = gfs_server::update_worktree(
        server.path().to_path_buf(),
        wt.to_path_buf(),
        stream.clone(),
        gfs_server::LockMode::Wait {
            timeout: Some(timeout),
        },
    )
    .await
    .expect_err("waiting past the deadline fails");
    assert!(
        matches!(err, gfs_server::ServerError::LockTimeout { .. }),
        "got {err:?}",
    );
    assert!(
        start.elapsed() >= timeout,
        "it should have waited at least the timeout, waited {:?}",
        start.elapsed(),
    );
}

#[tokio::test]
async fn update_worktree_wait_proceeds_once_lock_is_released() {
    let (server, _addr, stream) = server_with_synced_stream().await;

    let worktree = tempfile::tempdir().expect("worktree dir");
    let wt = worktree.path().to_path_buf();
    // Hold the lock, then launch a `--wait` (no timeout) update that must block
    // on it rather than proceed.
    let held = hold_worktree_lock(server.path(), &wt);

    let repo = server.path().to_path_buf();
    let wt_for_update = wt.clone();
    let stream_for_update = stream.clone();
    let update = tokio::spawn(async move {
        gfs_server::update_worktree(
            repo,
            wt_for_update,
            stream_for_update,
            gfs_server::LockMode::Wait { timeout: None },
        )
        .await
    });

    // Give the update a moment to reach the blocking `lock()`, then release; it
    // should now acquire the lock and complete.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    drop(held);

    update
        .await
        .expect("update task joins")
        .expect("update succeeds once the lock frees");
    assert_eq!(
        worktree_files(&wt),
        tree_paths(server.path(), &code_ref(&stream)),
        "the waited-for update checked the tree out",
    );
}

#[tokio::test]
async fn distinct_worktrees_do_not_contend() {
    let (server, _addr, stream) = server_with_synced_stream().await;

    // Hold worktree A's lock…
    let a = tempfile::tempdir().expect("worktree A");
    let _held_a = hold_worktree_lock(server.path(), a.path());

    // …a fail-fast update of a *different* worktree B is unaffected.
    let b = tempfile::tempdir().expect("worktree B");
    gfs_server::update_worktree(
        server.path().to_path_buf(),
        b.path().to_path_buf(),
        stream.clone(),
        gfs_server::LockMode::FailFast,
    )
    .await
    .expect("a distinct worktree is independent of A's lock");
    assert_eq!(
        worktree_files(b.path()),
        tree_paths(server.path(), &code_ref(&stream)),
        "worktree B was checked out",
    );
}
