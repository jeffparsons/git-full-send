//! End-to-end test for the `git-full-send` MVP (issue #22).
//!
//! Drives the **actual `git-full-send` binary** (`sync` and `update-worktree`)
//! as subprocesses across a full loopback round-trip, asserting the remote
//! worktree matches the client's synced state *exactly* — including
//! force-included `extra` files at their identity paths and deletions of both
//! `code`-tree files and dropped force-includes.
//!
//! The server (`listen`) runs **in-process** here rather than as a third
//! subprocess: `listen` needs a concrete port, and binding `127.0.0.1:0` lets
//! the OS pick a free one that we can read back deterministically (a child
//! process would have to report its chosen port back over some side channel —
//! racy and flaky). Driving `sync` and `update-worktree` through the binary
//! still exercises the finalised CLI surface for the two commands an operator
//! runs by hand each cycle; `listen`'s own arg parsing is covered by the
//! `command_line_surface_is_wired_up` smoke test below.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;

use gfs_common::{StreamId, code_ref, extra_ref};
use test_support::{commit_all, git, init_bare_repo, init_temp_repo, write_file};

/// Path to the built `git-full-send` binary (Cargo sets this for integration
/// tests of the crate that defines the binary).
const BIN: &str = env!("CARGO_BIN_EXE_git-full-send");

/// A fixed stream id so the produced refs are deterministic.
fn test_stream() -> StreamId {
    StreamId::new("e2e").unwrap()
}

/// Bind an in-process listener for `repo` on an ephemeral localhost port and
/// serve it on a background thread, returning the bound address.
fn start_server(repo: &Path) -> SocketAddr {
    let listener = gfs_server::bind("127.0.0.1:0".parse().unwrap(), repo.to_path_buf())
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        let _ = gfs_server::serve(listener);
    });
    addr
}

/// Run the `git-full-send` binary with `args`, asserting it exits zero.
///
/// `GIT_FULL_SEND_USER_INCLUDE` is pointed at a non-existent path so the test is
/// hermetic from any real per-user include file on the developer's machine (a
/// missing file is treated as an empty layer).
fn run_cli(args: &[&str]) {
    let output = Command::new(BIN)
        .args(args)
        .env(
            "GIT_FULL_SEND_USER_INCLUDE",
            "/nonexistent/git-full-send-include",
        )
        .output()
        .unwrap_or_else(|e| panic!("spawn `git-full-send {}`: {e}", args.join(" ")));
    assert!(
        output.status.success(),
        "`git-full-send {}` failed ({}):\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The recursive set of paths in `tree_ish` within `repo`.
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

/// The union of the server's `code` and `extra` trees — the exact set of paths
/// an `update-worktree` should materialise.
fn expected_union(server: &Path, stream: &StreamId) -> BTreeSet<String> {
    let mut expected = tree_paths(server, &code_ref(stream));
    expected.extend(tree_paths(server, &extra_ref(stream)));
    expected
}

/// Parse the JSON Lines metrics records (issue #42) at `git_dir`'s sink.
fn metrics_records(git_dir: &Path) -> Vec<serde_json::Value> {
    let path = git_dir.join("git-full-send").join("metrics.jsonl");
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read metrics file {}: {e}", path.display()));
    contents
        .lines()
        .map(|line| serde_json::from_str(line).expect("metrics line is valid JSON"))
        .collect()
}

#[test]
fn full_round_trip_through_the_cli_matches_exactly_including_extras_and_deletions() {
    let server = init_bare_repo();
    let addr = start_server(server.path());
    let remote = addr.to_string();

    let client = init_temp_repo();
    let c = client.path();

    // --- Committed baseline: ordinary code, a file that will later be deleted,
    // and `shared.txt` which is *both* tracked code and force-included (so it
    // collides at an identity path between the `code` and `extra` trees).
    write_file(c, "src/main.rs", "fn main() {}");
    write_file(c, "src/util.rs", "util-v1"); // deleted in round 2
    write_file(c, "README.md", "readme-v1");
    write_file(c, "shared.txt", "shared");
    write_file(c, ".gitignore", "dist/\n");
    write_file(c, ".git-full-send-include", "dist/\nshared.txt\n");
    commit_all(c, "baseline");

    // --- Working-tree state the encode folds into `code`: an unstaged tracked
    // edit, a staged file, and an untracked file…
    write_file(c, "README.md", "readme-edited");
    write_file(c, "staged.txt", "staged-content");
    git(c, &["add", "staged.txt"]);
    write_file(c, "untracked.txt", "untracked-content");
    // …plus two gitignored build outputs pulled into `extra` (vendor dropped in
    // round 2).
    write_file(c, "dist/app.js", "app");
    write_file(c, "dist/vendor.js", "vendor");

    let stream = test_stream();
    let stream_arg = stream.as_str();

    // === Round 1: sync, check out, assert exact match. ===
    run_cli(&[
        "sync",
        "--repo",
        c.to_str().unwrap(),
        "--remote",
        &remote,
        "--stream-id",
        stream_arg,
    ]);

    let worktree = tempfile::tempdir().expect("worktree dir");
    let wt = worktree.path();
    run_cli(&[
        "update-worktree",
        "--repo",
        server.path().to_str().unwrap(),
        "--worktree",
        wt.to_str().unwrap(),
        "--stream-id",
        stream_arg,
    ]);

    // The worktree is exactly the union of the synced `code` and `extra` trees.
    assert_eq!(
        worktree_files(wt),
        expected_union(server.path(), &stream),
        "round 1: worktree equals the code tree overlaid with the extra tree",
    );
    // Spot-check representative contents: the working-tree edit, the staged and
    // untracked additions, the force-included build output at its identity path,
    // and the same-path overlay file.
    let read = |rel: &str| {
        std::fs::read_to_string(wt.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
    };
    assert_eq!(read("README.md"), "readme-edited", "tracked edit folded in");
    assert_eq!(read("staged.txt"), "staged-content", "staged file synced");
    assert_eq!(
        read("untracked.txt"),
        "untracked-content",
        "untracked file synced"
    );
    assert_eq!(
        read("dist/app.js"),
        "app",
        "force-included output at identity path"
    );
    assert_eq!(
        read("shared.txt"),
        "shared",
        "same-path overlay file present"
    );

    // === Round 2: drop a code file and a force-included file; re-sync; assert. ===
    std::fs::remove_file(c.join("src/util.rs")).expect("remove util.rs");
    commit_all(c, "drop util.rs");
    std::fs::remove_file(c.join("dist/vendor.js")).expect("remove vendor.js");

    run_cli(&[
        "sync",
        "--repo",
        c.to_str().unwrap(),
        "--remote",
        &remote,
        "--stream-id",
        stream_arg,
    ]);
    run_cli(&[
        "update-worktree",
        "--repo",
        server.path().to_str().unwrap(),
        "--worktree",
        wt.to_str().unwrap(),
        "--stream-id",
        stream_arg,
    ]);

    // Both the dropped code file and the dropped force-include are gone…
    assert!(
        !wt.join("src/util.rs").exists(),
        "dropped code file removed"
    );
    assert!(
        !wt.join("dist/vendor.js").exists(),
        "dropped force-include removed"
    );
    // …survivors are intact…
    assert_eq!(read("dist/app.js"), "app", "surviving extra file remains");
    assert_eq!(
        read("src/main.rs"),
        "fn main() {}",
        "surviving code file remains"
    );
    // …and the worktree still equals the new (smaller) union exactly.
    assert_eq!(
        worktree_files(wt),
        expected_union(server.path(), &stream),
        "round 2: worktree still matches the code tree overlaid with the extra tree",
    );
}

#[test]
fn round_trip_records_metrics_on_both_sides() {
    // A sync + checkout writes a per-operation metrics record to each side's
    // sink (issue #42, ADR-0013): `sync` on the client, `receive` and
    // `update_worktree` on the server.
    let server = init_bare_repo();
    let addr = start_server(server.path());
    let remote = addr.to_string();

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "src/main.rs", "fn main() {}");
    write_file(c, ".gitignore", "dist/\n"); // `dist/` is gitignored…
    write_file(c, ".git-full-send-include", "dist/\n"); // …and force-included into `extra`
    commit_all(c, "baseline");
    write_file(c, "untracked.txt", "hello"); // 5 bytes folded into `code`
    write_file(c, "dist/app.js", "app"); // 3 bytes into `extra` only

    let stream = test_stream();
    let stream_arg = stream.as_str();

    run_cli(&[
        "sync",
        "--repo",
        c.to_str().unwrap(),
        "--remote",
        &remote,
        "--stream-id",
        stream_arg,
    ]);
    let worktree = tempfile::tempdir().expect("worktree dir");
    run_cli(&[
        "update-worktree",
        "--repo",
        server.path().to_str().unwrap(),
        "--worktree",
        worktree.path().to_str().unwrap(),
        "--stream-id",
        stream_arg,
    ]);

    // --- Client: one `sync` record with the per-layer sizes folded in.
    let client_records = metrics_records(&c.join(".git"));
    let sync = client_records
        .iter()
        .find(|r| r["kind"] == "sync")
        .expect("a sync record");
    assert_eq!(sync["stream"], stream_arg);
    assert_eq!(sync["remote"], remote);
    assert!(sync["total_ms"].as_f64().is_some(), "total_ms present");
    // `untracked.txt` is the one overlaid code-layer file; `dist/app.js` the one
    // extra file.
    assert_eq!(sync["code"]["files_overlaid"], 1);
    assert_eq!(sync["code"]["bytes_overlaid"], "hello".len() as u64);
    assert_eq!(sync["extra"]["files"], 1);
    assert_eq!(sync["extra"]["bytes"], "app".len() as u64);

    // --- Server: a `receive` record (positive bytes, accepted refs) and an
    // `update_worktree` record.
    let server_records = metrics_records(server.path());
    let receive = server_records
        .iter()
        .find(|r| r["kind"] == "receive")
        .expect("a receive record");
    assert_eq!(receive["success"], true);
    assert!(
        receive["bytes_in"].as_u64().unwrap_or(0) > 0,
        "bytes were counted off the socket",
    );
    let refs: Vec<&str> = receive["refs_updated"]
        .as_array()
        .expect("refs_updated array")
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    assert!(
        refs.contains(&code_ref(&stream).as_str()),
        "the code ref was recorded as updated: {refs:?}",
    );

    let update = server_records
        .iter()
        .find(|r| r["kind"] == "update_worktree")
        .expect("an update_worktree record");
    assert_eq!(update["stream"], stream_arg);
    assert!(
        update["total_ms"].as_f64().is_some(),
        "update_worktree total_ms present",
    );
}

/// The finalised CLI surface is wired up: every subcommand parses, and the new
/// `--user-include` flag and `listen --addr` option are present.
#[test]
fn command_line_surface_is_wired_up() {
    let help = |args: &[&str]| -> String {
        let output = Command::new(BIN).args(args).output().expect("spawn --help");
        assert!(
            output.status.success(),
            "`git-full-send {}` failed",
            args.join(" ")
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    let top = help(&["--help"]);
    for sub in [
        "sync",
        "listen",
        "update-worktree",
        "list-streams",
        "forget-stream",
    ] {
        assert!(top.contains(sub), "top-level help lists `{sub}`:\n{top}");
    }
    let forget_help = help(&["forget-stream", "--help"]);
    for flag in ["--repo", "--stream-id"] {
        assert!(
            forget_help.contains(flag),
            "forget-stream exposes {flag}:\n{forget_help}",
        );
    }
    assert!(
        help(&["sync", "--help"]).contains("--user-include"),
        "sync exposes --user-include",
    );
    let listen_help = help(&["listen", "--help"]);
    for flag in ["--addr", "--max-connections", "--connection-timeout"] {
        assert!(
            listen_help.contains(flag),
            "listen exposes {flag}:\n{listen_help}",
        );
    }
}
