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

use gfs_client::{CODE_REF, SENT_REF};
use test_support::{commit_all, git, init_bare_repo, init_temp_repo, write_file};

/// Bind a listener for `repo` on an ephemeral localhost port and serve it on a
/// background thread, returning the bound address.
fn start_server(repo: &Path) -> SocketAddr {
    let listener = gfs_server::bind("127.0.0.1:0".parse().unwrap(), repo.to_path_buf())
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        let _ = gfs_server::serve(listener);
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

    gfs_client::sync(c.to_path_buf(), addr.to_string())
        .await
        .expect("sync succeeds");

    // The server's `code` ref now matches the client's encoded tip…
    let client_tip = git(c, &["rev-parse", CODE_REF]);
    let server_tip = git(server.path(), &["rev-parse", "refs/git-full-send/code"]);
    assert_eq!(
        server_tip.trim(),
        client_tip.trim(),
        "server has the code tip"
    );

    // …and the objects landed: the tree is walkable on the server with the
    // expected working-tree contents.
    assert_eq!(
        tree_paths(server.path(), "refs/git-full-send/code"),
        BTreeSet::from(["committed.txt".to_string(), "untracked.txt".to_string()]),
    );
    assert_eq!(
        git(
            server.path(),
            &["cat-file", "blob", "refs/git-full-send/code:committed.txt"]
        ),
        "modified",
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

    gfs_client::sync(c.to_path_buf(), addr.to_string())
        .await
        .expect("sync succeeds");

    assert_eq!(
        git(c, &["rev-parse", SENT_REF]).trim(),
        git(c, &["rev-parse", CODE_REF]).trim(),
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
    git(c, &["update-ref", CODE_REF, "HEAD"]);
    let remote = addr.to_string();

    // A namespaced ref is accepted…
    gfs_client::push_ref(c, &remote, CODE_REF).expect("a refs/git-full-send/* push is accepted");
    // …but anything outside the namespace is declined by the pre-receive hook.
    assert!(
        gfs_client::push_ref(c, &remote, "refs/heads/main").is_err(),
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
    gfs_client::sync(c.to_path_buf(), addr.to_string())
        .await
        .expect("first sync");
    let first = git(server.path(), &["rev-parse", "refs/git-full-send/code"]);

    // Change the working tree and sync again (the retained tip is the base).
    write_file(c, "a.txt", "two");
    gfs_client::sync(c.to_path_buf(), addr.to_string())
        .await
        .expect("second sync");
    let second = git(server.path(), &["rev-parse", "refs/git-full-send/code"]);

    assert_ne!(first.trim(), second.trim(), "the server code ref advanced");
    assert_eq!(
        git(
            server.path(),
            &["cat-file", "blob", "refs/git-full-send/code:a.txt"]
        ),
        "two",
        "the server has the latest content",
    );
}
