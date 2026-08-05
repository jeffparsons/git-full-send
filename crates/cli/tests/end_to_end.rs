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
/// serve it as a task on the caller's Tokio runtime, returning the bound address.
///
/// The server shares the test's runtime with the binary's driver: `run_cli`
/// awaits the subprocess via `tokio::process`, so the current-thread runtime
/// stays live to poll this accept loop. The task is detached and stops when the
/// test process exits (the same fire-and-forget lifecycle the old `std::thread`
/// helper had).
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

/// Run the `git-full-send` binary with `args`, asserting it exits zero.
///
/// Async (`tokio::process`) so awaiting the subprocess keeps the test's
/// current-thread runtime live for the co-located server task spawned by
/// [`start_server`].
///
/// `GIT_FULL_SEND_USER_INCLUDE` is pointed at a non-existent path so the test is
/// hermetic from any real per-user include file on the developer's machine (a
/// missing file is treated as an empty layer).
async fn run_cli(args: &[&str]) {
    run_cli_capture(args).await;
}

/// Like [`run_cli`], but returns the command's captured stdout — used to assert on
/// operator-facing output such as the end-of-sync summary block (issue #53).
async fn run_cli_capture(args: &[&str]) -> String {
    let output = cli_command(args)
        .output()
        .await
        .unwrap_or_else(|e| panic!("spawn `git-full-send {}`: {e}", args.join(" ")));
    assert!(
        output.status.success(),
        "`git-full-send {}` failed ({}):\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("stdout is utf-8")
}

/// Run the binary expecting it to *fail*, returning its captured stderr — for the
/// paths where the diagnosis is the deliverable (ADR-0019's refusal to start).
async fn run_cli_expecting_failure(args: &[&str]) -> String {
    let output = cli_command(args)
        .output()
        .await
        .unwrap_or_else(|e| panic!("spawn `git-full-send {}`: {e}", args.join(" ")));
    assert!(
        !output.status.success(),
        "`git-full-send {}` unexpectedly succeeded",
        args.join(" "),
    );
    String::from_utf8(output.stderr).expect("stderr is utf-8")
}

/// The binary, with the environment pinned so a test never picks up the
/// developer's own configuration.
fn cli_command(args: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(BIN);
    command
        .args(args)
        // A shared secret in the developer's shell must not silently authenticate
        // (or fail) a test that says nothing about tokens (ADR-0019).
        .env_remove("GIT_FULL_SEND_TOKEN")
        .env(
            "GIT_FULL_SEND_USER_INCLUDE",
            "/nonexistent/git-full-send-include",
        );
    command
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

#[tokio::test]
async fn full_round_trip_through_the_cli_matches_exactly_including_extras_and_deletions() {
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
    ])
    .await;

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
    ])
    .await;

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
    ])
    .await;
    run_cli(&[
        "update-worktree",
        "--repo",
        server.path().to_str().unwrap(),
        "--worktree",
        wt.to_str().unwrap(),
        "--stream-id",
        stream_arg,
    ])
    .await;

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

#[tokio::test]
async fn round_trip_records_metrics_on_both_sides() {
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

    let sync_stdout = run_cli_capture(&[
        "sync",
        "--repo",
        c.to_str().unwrap(),
        "--remote",
        &remote,
        "--stream-id",
        stream_arg,
    ])
    .await;

    // The operator-facing summary block (issue #53) reports the same fixture the
    // metrics record does: stream/remote, one code file ("hello" = 5 B) and one
    // extra file ("app" = 3 B). Bytes are well under 1 KiB, so they print as `B`.
    assert!(
        sync_stdout.contains(&format!("Synced stream {stream_arg} to {remote}")),
        "summary names the stream and remote:\n{sync_stdout}",
    );
    assert!(
        sync_stdout.contains("code:  1 file(s) (+5 B), 0 removed"),
        "summary reports the code layer:\n{sync_stdout}",
    );
    assert!(
        sync_stdout.contains("extra: 1 file(s) (3 B)"),
        "summary reports the extra layer:\n{sync_stdout}",
    );

    let worktree = tempfile::tempdir().expect("worktree dir");
    run_cli(&[
        "update-worktree",
        "--repo",
        server.path().to_str().unwrap(),
        "--worktree",
        worktree.path().to_str().unwrap(),
        "--stream-id",
        stream_arg,
    ])
    .await;

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
    assert_eq!(receive["outcome"], "updated");
    assert_eq!(receive["success"], true);
    // The on-wire bytes are split into protocol overhead and payload on both
    // ends (ADR-0017), so a large ref advertisement can never again be mistaken
    // for transferred data.
    assert!(
        receive["inbound"]["pack"].as_u64().unwrap_or(0) > 0,
        "the pack was counted: {receive}",
    );
    assert!(
        receive["inbound"]["command_pkts"].as_u64().unwrap_or(0) > 0,
        "ref-update commands arrived: {receive}",
    );
    assert!(
        receive["outbound"]["advertisement"].as_u64().unwrap_or(0) > 0,
        "the ref advertisement was counted separately: {receive}",
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

/// `--json` prints, on stdout, exactly the record that lands in the sink
/// (ADR-0017) — for `sync` on the client and for `update-worktree` on the server,
/// which is how a client driving a remote checkout over SSH gets the server's
/// numbers back.
///
/// Equality with the sink line is the point: an integrator parsing stdout and an
/// operator reading `metrics.jsonl` must see the same numbers, not two
/// hand-maintained spellings of them.
#[tokio::test]
async fn json_output_is_the_same_record_that_lands_in_the_sink() {
    let server = init_bare_repo();
    let addr = start_server(server.path());
    let remote = addr.to_string();

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "src/main.rs", "fn main() {}");
    commit_all(c, "baseline");
    write_file(c, "untracked.txt", "hello");

    let stream = test_stream();
    let stream_arg = stream.as_str();

    let sync_stdout = run_cli_capture(&[
        "sync",
        "--repo",
        c.to_str().unwrap(),
        "--remote",
        &remote,
        "--stream-id",
        stream_arg,
        "--json",
    ])
    .await;

    // One JSON object, and nothing else: the human block is suppressed.
    let printed: serde_json::Value =
        serde_json::from_str(sync_stdout.trim()).expect("sync --json prints one JSON object");
    assert_eq!(
        sync_stdout.trim().lines().count(),
        1,
        "--json prints the record alone:\n{sync_stdout}",
    );
    let recorded = metrics_records(&c.join(".git"))
        .into_iter()
        .find(|r| r["kind"] == "sync")
        .expect("a sync record in the sink");
    assert_eq!(printed, recorded, "stdout and the sink carry one record");

    // It is self-describing, and carries every number the human summary shows.
    assert_eq!(printed["kind"], "sync");
    assert_eq!(printed["schema"], gfs_common::metrics::SCHEMA_VERSION);
    assert_eq!(printed["stream"], stream_arg);
    assert_eq!(printed["remote"], remote);
    for field in ["total_ms", "retain_ms"] {
        assert!(printed[field].as_f64().is_some(), "{field} present");
    }
    for layer in ["code", "extra"] {
        for field in ["encode_ms", "push_ms", "commit", "tree"] {
            assert!(
                !printed[layer][field].is_null(),
                "{layer}.{field} present:\n{printed}",
            );
        }
    }
    assert_eq!(printed["code"]["files_overlaid"], 1);
    assert_eq!(printed["code"]["bytes_overlaid"], "hello".len() as u64);

    // The same contract for the server-side checkout.
    let worktree = tempfile::tempdir().expect("worktree dir");
    let update_stdout = run_cli_capture(&[
        "update-worktree",
        "--repo",
        server.path().to_str().unwrap(),
        "--worktree",
        worktree.path().to_str().unwrap(),
        "--stream-id",
        stream_arg,
        "--json",
    ])
    .await;
    let printed: serde_json::Value = serde_json::from_str(update_stdout.trim())
        .expect("update-worktree --json prints one JSON object");
    let recorded = metrics_records(server.path())
        .into_iter()
        .find(|r| r["kind"] == "update_worktree")
        .expect("an update_worktree record in the sink");
    assert_eq!(printed, recorded, "stdout and the sink carry one record");
    assert_eq!(printed["stream"], stream_arg);
    for field in ["total_ms", "resolve_ms", "read_tree_ms", "clean_ms"] {
        assert!(printed[field].as_f64().is_some(), "{field} present");
    }
}

/// Without `--json`, both commands keep their human summary block as the default
/// (ADR-0017 does not regress the surface issue #53 added), and
/// `update-worktree` — which previously printed nothing — now has one.
#[tokio::test]
async fn the_human_summary_stays_the_default_on_both_commands() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "src/main.rs", "fn main() {}");
    commit_all(c, "baseline");

    let stream = test_stream();
    let sync_stdout = run_cli_capture(&[
        "sync",
        "--repo",
        c.to_str().unwrap(),
        "--remote",
        &addr.to_string(),
        "--stream-id",
        stream.as_str(),
    ])
    .await;
    assert!(
        sync_stdout.starts_with("Synced stream "),
        "sync still leads with the human summary:\n{sync_stdout}",
    );

    let worktree = tempfile::tempdir().expect("worktree dir");
    let update_stdout = run_cli_capture(&[
        "update-worktree",
        "--repo",
        server.path().to_str().unwrap(),
        "--worktree",
        worktree.path().to_str().unwrap(),
        "--stream-id",
        stream.as_str(),
    ])
    .await;
    assert!(
        update_stdout.starts_with("Updated worktree "),
        "update-worktree prints a human summary:\n{update_stdout}",
    );
    assert!(
        update_stdout.contains("read-tree"),
        "and names its phases:\n{update_stdout}",
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(update_stdout.trim()).is_err(),
        "the default surface is prose, not JSON:\n{update_stdout}",
    );
}

/// Run the binary and return `(exit code, stdout)` without asserting success —
/// for commands whose non-zero exit is the thing under test.
async fn run_cli_status(args: &[&str]) -> (i32, String) {
    let output = tokio::process::Command::new(BIN)
        .args(args)
        .env(
            "GIT_FULL_SEND_USER_INCLUDE",
            "/nonexistent/git-full-send-include",
        )
        .output()
        .await
        .unwrap_or_else(|e| panic!("spawn `git-full-send {}`: {e}", args.join(" ")));
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// `doctor` reports the repo conditions that predictably hurt, and exits non-zero
/// on a *broken* one so an orchestrator can gate on it (ADR-0018).
///
/// The fixture is the exact failure from #75: an `objects/info/alternates`
/// entry pointing at a path that no longer exists, which git complains about on
/// every invocation while carrying on, and which gfs used to pass through in
/// silence.
#[tokio::test]
async fn doctor_reports_a_broken_alternates_and_exits_non_zero() {
    let server = init_bare_repo();
    let repo = server.path().to_str().unwrap().to_string();

    // Healthy to begin with.
    let (code, healthy) = run_cli_status(&["doctor", "--repo", &repo]).await;
    assert_eq!(code, 0, "a healthy repo exits zero:\n{healthy}");
    assert!(
        healthy.contains("refs") && healthy.contains("alternates"),
        "the checks are named:\n{healthy}",
    );

    // Now break the alternates.
    let info = server.path().join("objects").join("info");
    std::fs::create_dir_all(&info).expect("create objects/info");
    std::fs::write(info.join("alternates"), "/gone/nowhere/objects\n").expect("write alternates");

    let (code, broken) = run_cli_status(&["doctor", "--repo", &repo]).await;
    assert_eq!(code, 1, "a broken repo exits non-zero:\n{broken}");
    assert!(
        broken.contains("/gone/nowhere/objects"),
        "it names the unreachable entry:\n{broken}",
    );
    assert!(
        broken.contains("ERROR"),
        "and calls it an error, not a warning:\n{broken}",
    );

    // The same findings, structurally, for a caller that parses.
    let (_, json) = run_cli_status(&["doctor", "--repo", &repo, "--json"]).await;
    let report: serde_json::Value =
        serde_json::from_str(json.trim()).expect("doctor --json prints one JSON object");
    assert_eq!(report["kind"], "doctor");
    let alternates = report["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|c| c["name"] == "alternates")
        .expect("an alternates check");
    assert_eq!(alternates["status"], "error");
    assert!(
        alternates["remedy"].is_string(),
        "a finding carries what to do about it: {alternates}",
    );
}

/// `metrics` summarises the sink so `docs/operating.md` can stop handing out
/// `jq` incantations (ADR-0017).
#[tokio::test]
async fn metrics_summarises_the_sink_after_a_round_trip() {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "src/main.rs", "fn main() {}");
    commit_all(c, "baseline");

    let stream = test_stream();
    run_cli(&[
        "sync",
        "--repo",
        c.to_str().unwrap(),
        "--remote",
        &addr.to_string(),
        "--stream-id",
        stream.as_str(),
    ])
    .await;

    let summary = run_cli_capture(&["metrics", "--repo", c.to_str().unwrap()]).await;
    assert!(
        summary.contains("sync (1 record(s))"),
        "the client's sink holds one sync:\n{summary}",
    );
    for header in ["p50", "p95", "max"] {
        assert!(summary.contains(header), "reports {header}:\n{summary}");
    }
    assert!(
        summary.contains("code.push_ms"),
        "including nested fields, flattened to dotted keys:\n{summary}",
    );

    // The server's side, filtered to one kind, as JSON.
    let json = run_cli_capture(&[
        "metrics",
        "--repo",
        server.path().to_str().unwrap(),
        "--kind",
        "receive",
        "--json",
    ])
    .await;
    let stats: serde_json::Value =
        serde_json::from_str(json.trim()).expect("metrics --json parses");
    let kinds = stats.as_array().expect("an array of per-kind stats");
    assert_eq!(kinds.len(), 1, "filtered to `receive`: {stats}");
    assert_eq!(kinds[0]["kind"], "receive");
    assert!(
        kinds[0]["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .any(|f| f["field"] == "outbound.advertisement"),
        "including the advertisement split: {stats}",
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
        "probe",
        "listen",
        "update-worktree",
        "list-streams",
        "forget-stream",
        "reap",
        "doctor",
        "metrics",
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
    let reap_help = help(&["reap", "--help"]);
    for flag in ["--repo", "--older-than-days", "--dry-run"] {
        assert!(
            reap_help.contains(flag),
            "reap exposes {flag}:\n{reap_help}",
        );
    }
    let listen_help = help(&["listen", "--help"]);
    for flag in ["--token-file", "--allow-anonymous"] {
        assert!(
            listen_help.contains(flag),
            "listen exposes {flag}:\n{listen_help}",
        );
    }
    let sync_help = help(&["sync", "--help"]);
    for flag in [
        "--user-include",
        "--extra-include",
        "--json",
        "--token-file",
    ] {
        assert!(
            sync_help.contains(flag),
            "sync exposes {flag}:\n{sync_help}"
        );
    }
    let update_help = help(&["update-worktree", "--help"]);
    for flag in ["--json", "--measure-worktree"] {
        assert!(
            update_help.contains(flag),
            "update-worktree exposes {flag}:\n{update_help}",
        );
    }
    assert!(
        help(&["probe", "--help"]).contains("--remote"),
        "probe exposes --remote",
    );
    let doctor_help = help(&["doctor", "--help"]);
    for flag in ["--repo", "--worktree", "--json"] {
        assert!(
            doctor_help.contains(flag),
            "doctor exposes {flag}:\n{doctor_help}",
        );
    }
    let metrics_help = help(&["metrics", "--help"]);
    for flag in ["--repo", "--kind", "--last"] {
        assert!(
            metrics_help.contains(flag),
            "metrics exposes {flag}:\n{metrics_help}",
        );
    }
    let listen_help = help(&["listen", "--help"]);
    for flag in ["--addr", "--max-connections", "--connection-timeout"] {
        assert!(
            listen_help.contains(flag),
            "listen exposes {flag}:\n{listen_help}",
        );
    }
}

/// Seconds since the Unix epoch, for back-dating a stream's `code` ref.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs() as i64
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

/// `reap` through the real binary (issue #63): a back-dated stream is reported by
/// `--dry-run` without being deleted, then removed by the real run and gone from
/// `list-streams`.
#[tokio::test]
async fn reap_through_the_cli_dry_run_then_deletes() {
    let server = init_bare_repo();
    let addr = start_server(server.path());
    let server_path = server.path().to_str().unwrap().to_string();

    // Sync one stream through the binary.
    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "src/main.rs", "fn main() {}");
    commit_all(c, "baseline");
    let stream = test_stream();
    run_cli(&[
        "sync",
        "--repo",
        c.to_str().unwrap(),
        "--remote",
        &addr.to_string(),
        "--stream-id",
        stream.as_str(),
    ])
    .await;

    // Back-date its `code` ref to ~100 days ago so it is reapable now.
    let date = format!("{} +0000", now_unix() - 100 * 86_400);
    let tree = git(
        server.path(),
        &["rev-parse", &format!("{}^{{tree}}", code_ref(&stream))],
    );
    let out = Command::new("git")
        .arg("-C")
        .arg(server.path())
        // Identity via env so this works in a bare server repo with no configured
        // `user.*` (CI has no global identity).
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_NAME", "git-full-send tests")
        .env("GIT_COMMITTER_EMAIL", "tests@git-full-send.invalid")
        .env("GIT_AUTHOR_NAME", "git-full-send tests")
        .env("GIT_AUTHOR_EMAIL", "tests@git-full-send.invalid")
        .args(["commit-tree", tree.trim(), "-m", "backdated"])
        .output()
        .expect("run git commit-tree");
    assert!(
        out.status.success(),
        "commit-tree failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    let oid = String::from_utf8(out.stdout).expect("utf8 oid");
    git(
        server.path(),
        &["update-ref", &code_ref(&stream), oid.trim()],
    );

    // Dry run: names the stale stream, deletes nothing.
    let dry = run_cli_capture(&[
        "reap",
        "--repo",
        &server_path,
        "--older-than-days",
        "30",
        "--dry-run",
    ])
    .await;
    assert!(
        dry.contains(stream.as_str()),
        "dry-run names the stale stream:\n{dry}",
    );
    assert!(
        ref_exists(server.path(), &code_ref(&stream)),
        "dry-run must not delete anything",
    );

    // Real run: removes the refs; the stream leaves `list-streams`.
    let real = run_cli_capture(&["reap", "--repo", &server_path, "--older-than-days", "30"]).await;
    assert!(
        real.contains(stream.as_str()),
        "reap names the reaped stream:\n{real}",
    );
    assert!(!ref_exists(server.path(), &code_ref(&stream)));
    assert!(!ref_exists(server.path(), &extra_ref(&stream)));
    let listed = run_cli_capture(&["list-streams", "--repo", &server_path]).await;
    assert!(
        listed.trim().is_empty(),
        "no streams remain after reaping:\n{listed}",
    );
}

// --- Additive extra includes (issue #80) -------------------------------------

/// `--extra-include` layers on top of the per-user lookup instead of replacing
/// it: a sync given a real per-user file (via `GIT_FULL_SEND_USER_INCLUDE`)
/// *and* an `--extra-include` file delivers force-includes from both layers —
/// the anti-replacement guarantee the flag exists for.
#[tokio::test]
async fn extra_include_layers_on_top_of_the_user_lookup(/* issue #80 */) {
    let server = init_bare_repo();
    let addr = start_server(server.path());

    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "src/main.rs", "fn main() {}");
    write_file(c, ".gitignore", "dist/\nconfig/\nout/\n");
    write_file(c, ".git-full-send-include", "/dist/\n");
    commit_all(c, "baseline");
    // One gitignored file per layer: project, user, extra.
    write_file(c, "dist/app.js", "app");
    write_file(c, "config/local.toml", "cfg");
    write_file(c, "out/artifact.bin", "bin");

    let aux = tempfile::tempdir().expect("aux dir");
    let user = aux.path().join("user-include");
    std::fs::write(&user, "/config/local.toml\n").expect("write user include");
    let extra = aux.path().join("extra-include");
    std::fs::write(&extra, "/out/artifact.bin\n").expect("write extra include");

    let stream = test_stream();
    let mut command = cli_command(&[
        "sync",
        "--repo",
        c.to_str().unwrap(),
        "--remote",
        &addr.to_string(),
        "--stream-id",
        stream.as_str(),
        "--extra-include",
        extra.to_str().unwrap(),
    ]);
    // Point the pinned env var at the real per-user file for this one test.
    command.env("GIT_FULL_SEND_USER_INCLUDE", &user);
    let output = command.output().await.expect("spawn sync");
    assert!(
        output.status.success(),
        "sync with --extra-include failed ({}):\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let synced = tree_paths(server.path(), &extra_ref(&stream));
    for path in ["dist/app.js", "config/local.toml", "out/artifact.bin"] {
        assert!(
            synced.contains(path),
            "`{path}` was force-included (all three layers contribute): {synced:?}",
        );
    }
}

/// A missing `--extra-include` file fails the sync, naming the path. Unlike the
/// other pattern files it is never an empty layer: the flag exists for tooling
/// passing a file it just wrote, and silently dropping its patterns is the
/// failure mode the flag was added to eliminate.
#[tokio::test]
async fn sync_fails_when_an_extra_include_file_is_missing(/* issue #80 */) {
    let client = init_temp_repo();
    let c = client.path();
    write_file(c, "src/main.rs", "fn main() {}");
    commit_all(c, "baseline");

    // The failure comes from reading the pattern file, before any connection —
    // the unroutable remote proves it as well as documenting it.
    let stderr = run_cli_expecting_failure(&[
        "sync",
        "--repo",
        c.to_str().unwrap(),
        "--remote",
        "127.0.0.1:1",
        "--stream-id",
        test_stream().as_str(),
        "--extra-include",
        "/nonexistent/extra-include",
    ])
    .await;
    assert!(
        stderr.contains("/nonexistent/extra-include"),
        "the error names the missing file:\n{stderr}",
    );
}

// --- Authentication (issue #81, ADR-0019) -----------------------------------

/// `listen` must not be able to end up unauthenticated by omission: it checks out
/// what it is given, and the receiving machine's tooling then runs those files. So
/// the operator names a posture, and the error names both ways to do it.
#[tokio::test]
async fn listen_refuses_to_start_without_an_authentication_choice(/* issue #81 */) {
    let server = init_bare_repo();
    let repo = server.path().display().to_string();

    let stderr = run_cli_expecting_failure(&["listen", "--repo", &repo]).await;
    for remedy in ["--token-file", "--allow-anonymous"] {
        assert!(
            stderr.contains(remedy),
            "the refusal names `{remedy}`:\n{stderr}",
        );
    }

    // And it never got as far as binding: the choice is resolved first.
    assert!(
        !stderr.contains("serving git receive-pack"),
        "no listener was started:\n{stderr}",
    );
}

/// The shared secret across the real CLI surface: a `--token-file` sync lands on
/// an authenticated server, and the same sync without one does not.
#[tokio::test]
async fn a_token_file_sync_round_trips_through_the_binary(/* issue #81 */) {
    let server = init_bare_repo();
    let listener = gfs_server::bind("127.0.0.1:0".parse().unwrap(), server.path().to_path_buf())
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    let secret = "the-shared-secret-value";
    let config = gfs_server::ListenConfig {
        auth: std::sync::Arc::new(gfs_server::Auth::Token(
            gfs_common::auth::Token::new(secret, "the test").expect("a valid token"),
        )),
        auth_timeout: std::time::Duration::from_millis(300),
        ..Default::default()
    };
    tokio::spawn(async move {
        let _ = gfs_server::serve_async(listener, config, std::future::pending::<()>()).await;
    });

    let client = init_temp_repo();
    write_file(client.path(), "src/main.rs", "fn main() {}");
    commit_all(client.path(), "baseline");
    let client_path = client.path().display().to_string();
    let remote = addr.to_string();

    let token_dir = tempfile::tempdir().expect("token dir");
    let token_file = token_dir.path().join("token");
    std::fs::write(&token_file, format!("{secret}\n")).expect("write token file");
    let token_file = token_file.display().to_string();

    // Without a token: refused, in the server's own words.
    let stderr = run_cli_expecting_failure(&[
        "sync",
        "--repo",
        &client_path,
        "--remote",
        &remote,
        "--stream-id",
        test_stream().as_str(),
    ])
    .await;
    assert!(
        stderr.contains("authentication required"),
        "the client was told why:\n{stderr}",
    );

    // With one: an ordinary sync. A trailing newline in the file is fine.
    run_cli(&[
        "sync",
        "--repo",
        &client_path,
        "--remote",
        &remote,
        "--stream-id",
        test_stream().as_str(),
        "--token-file",
        &token_file,
    ])
    .await;

    let worktree = tempfile::tempdir().expect("worktree dir");
    run_cli(&[
        "update-worktree",
        "--repo",
        &server.path().display().to_string(),
        "--worktree",
        &worktree.path().display().to_string(),
        "--stream-id",
        test_stream().as_str(),
    ])
    .await;
    assert_eq!(
        std::fs::read_to_string(worktree.path().join("src/main.rs")).unwrap(),
        "fn main() {}",
    );
}
