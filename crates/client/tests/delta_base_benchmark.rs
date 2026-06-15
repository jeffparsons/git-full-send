//! Transfer benchmark for the ADR-0005 delta-base design (issue #51).
//!
//! Validates with **real numbers** that retaining the previous sync's tip as a
//! delta base is what makes a changed artifact cheap on the wire, and that the
//! per-chain delta policy (#50) is the right call: a delta-*friendly* artifact
//! gains an order of magnitude from `--thin` against a retained base, while a
//! delta-*hostile* (incompressible, rebuilt-from-scratch) artifact gains nothing
//! from `--thin` over the `extra` chain's predictable whole-object send.
//!
//! ## How it measures
//!
//! No new measurement seam: the server already records `bytes_in` — the inbound
//! pack size — per `git receive-pack` connection (ADR-0013,
//! `gfs_server::metrics::ReceiveRecord`), appended to the repo's JSONL sink at
//! `gfs_common::metrics::metrics_path(git_dir)`. The harness drives **single-ref**
//! pushes via [`gfs_client::push_ref`] (not full `sync`) so each measured push
//! maps to exactly one `receive` record, then reads that record's `bytes_in`.
//!
//! Base presence is controlled by **which fresh bare server already holds the
//! prior tip** — real push negotiation, not gc fiddling. Each scenario runs an
//! *establish* push (whose bytes are discarded) to seed the base, then a
//! *measured* push of the changed artifact.
//!
//! ## Running
//!
//! `#[ignore]`-d so a multi-MiB harness stays out of the default `cargo test`:
//!
//! ```text
//! cargo test -p gfs-client --test delta_base_benchmark -- --ignored --nocapture
//! ```
//!
//! `--nocapture` surfaces the results table. The asserted inequalities double as
//! a regression check whenever the benchmark is run.

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use gfs_client::DeltaPolicy;
use gfs_common::{StreamId, code_ref, extra_ref};
use test_support::{commit_all, git, init_bare_repo, init_temp_repo, write_file};

/// Artifact size for both profiles, in bytes. Large enough that the artifact
/// dominates the few tiny baseline objects, small enough that each push is
/// sub-second — and well under `core.bigFileThreshold` (512 MiB) so the artifact
/// stays delta-*eligible* (above it git sends whole unconditionally; Research-0003
/// §2.2).
const ARTIFACT_BYTES: usize = 4 * 1024 * 1024;

/// The file every scenario writes its artifact to.
const ARTIFACT: &str = "artifact.bin";

/// One measured `receive` record: the inbound pack size and the server-side
/// duration. `bytes_in` is the load-bearing number; `duration_ms` is a soft,
/// noisy secondary (in-process, single-machine) reported but never asserted on.
#[derive(Clone, Copy)]
struct ReceiveStat {
    bytes_in: u64,
    duration_ms: f64,
}

#[tokio::test]
#[ignore = "multi-MiB transfer benchmark; run explicitly with --ignored --nocapture"]
async fn delta_base_transfer_benchmark() {
    let stream = StreamId::new("bench").unwrap();
    let code = code_ref(&stream);
    let extra = extra_ref(&stream);

    // --- A: code/thin, base PRESENT (delta-friendly) --------------------------
    // Establish baseline + artifact v1, then measure the push of v1→v2 (a single
    // changed line) against the retained base: a small thin delta.
    let server_a = init_bare_repo();
    let addr_a = start_server(server_a.path());
    let repo_a = init_temp_repo();
    baseline(repo_a.path());
    commit_artifact(repo_a.path(), &delta_friendly(Variant::V1));
    git(repo_a.path(), &["update-ref", &code, "HEAD"]);
    establish_push(repo_a.path(), addr_a, &code, DeltaPolicy::Thin).await;
    commit_artifact(repo_a.path(), &delta_friendly(Variant::V2));
    git(repo_a.path(), &["update-ref", &code, "HEAD"]);
    let a = measure_push(
        server_a.path(),
        repo_a.path(),
        addr_a,
        &code,
        DeltaPolicy::Thin,
    )
    .await;

    // --- B: code/thin, base ABSENT (same v2 artifact) -------------------------
    // The base holds only the baseline (no prior artifact), so the same v2
    // artifact has nothing to delta against and is sent whole.
    let server_b = init_bare_repo();
    let addr_b = start_server(server_b.path());
    let repo_b = init_temp_repo();
    baseline(repo_b.path());
    git(repo_b.path(), &["update-ref", &code, "HEAD"]);
    establish_push(repo_b.path(), addr_b, &code, DeltaPolicy::Thin).await;
    commit_artifact(repo_b.path(), &delta_friendly(Variant::V2));
    git(repo_b.path(), &["update-ref", &code, "HEAD"]);
    let b = measure_push(
        server_b.path(),
        repo_b.path(),
        addr_b,
        &code,
        DeltaPolicy::Thin,
    )
    .await;

    // --- C: extra/whole-object, base PRESENT (delta-hostile) ------------------
    // The production `extra` policy on a rebuilt, incompressible artifact: only
    // the changed objects travel (negotiation excludes the base), sent whole.
    let server_c = init_bare_repo();
    let addr_c = start_server(server_c.path());
    let repo_c = init_temp_repo();
    baseline(repo_c.path());
    commit_artifact(repo_c.path(), &delta_hostile(HOSTILE_SEED_V1));
    git(repo_c.path(), &["update-ref", &extra, "HEAD"]);
    establish_push(repo_c.path(), addr_c, &extra, DeltaPolicy::WholeObject).await;
    commit_artifact(repo_c.path(), &delta_hostile(HOSTILE_SEED_V2));
    git(repo_c.path(), &["update-ref", &extra, "HEAD"]);
    let c = measure_push(
        server_c.path(),
        repo_c.path(),
        addr_c,
        &extra,
        DeltaPolicy::WholeObject,
    )
    .await;

    // --- D: code/thin of the same delta-hostile change (contrast) -------------
    // The thin delta search runs but finds no usable base (v2 shares nothing with
    // v1), so the object is sent whole anyway — ~the same bytes as C.
    let server_d = init_bare_repo();
    let addr_d = start_server(server_d.path());
    let repo_d = init_temp_repo();
    baseline(repo_d.path());
    commit_artifact(repo_d.path(), &delta_hostile(HOSTILE_SEED_V1));
    git(repo_d.path(), &["update-ref", &code, "HEAD"]);
    establish_push(repo_d.path(), addr_d, &code, DeltaPolicy::Thin).await;
    commit_artifact(repo_d.path(), &delta_hostile(HOSTILE_SEED_V2));
    git(repo_d.path(), &["update-ref", &code, "HEAD"]);
    let d = measure_push(
        server_d.path(),
        repo_d.path(),
        addr_d,
        &code,
        DeltaPolicy::Thin,
    )
    .await;

    report(a, b, c, d);

    // The core ADR-0005 payoff: a retained base turns a whole-object send into a
    // small delta. Conservative factor — the real gap is far larger — so the
    // assertion encodes the direction, not a brittle byte count.
    assert!(
        a.bytes_in.saturating_mul(4) < b.bytes_in,
        "delta-friendly: base-present push ({} B) should be far cheaper than \
         base-absent ({} B)",
        a.bytes_in,
        b.bytes_in,
    );
    // The #50 justification: on delta-hostile content `--thin` (D) buys ~nothing
    // over the whole-object policy (C) — they're within ~20%.
    assert!(
        d.bytes_in as f64 > c.bytes_in as f64 * 0.8,
        "delta-hostile: thin push ({} B) should not materially beat whole-object \
         ({} B)",
        d.bytes_in,
        c.bytes_in,
    );
    // Sanity: the rebuilt delta-hostile artifact really did cross whole — both
    // pushes are within a small factor of the raw artifact size, and dwarf the
    // delta-friendly base-present push.
    let floor = (ARTIFACT_BYTES as f64 * 0.5) as u64;
    assert!(
        c.bytes_in > floor && d.bytes_in > floor,
        "delta-hostile pushes ({} B / {} B) should approach the artifact size",
        c.bytes_in,
        d.bytes_in,
    );
    assert!(
        a.bytes_in.saturating_mul(4) < c.bytes_in,
        "the delta-friendly thin delta ({} B) should be far smaller than a \
         whole delta-hostile send ({} B)",
        a.bytes_in,
        c.bytes_in,
    );
}

/// Print the results table (visible under `--nocapture`).
fn report(a: ReceiveStat, b: ReceiveStat, c: ReceiveStat, d: ReceiveStat) {
    let row = |label: &str, chain: &str, policy: &str, base: &str, s: ReceiveStat| {
        println!(
            "| {label:<34} | {chain:<6} | {policy:<12} | {base:<7} | {:>9} | {:>8.1} |",
            s.bytes_in, s.duration_ms,
        );
    };
    println!();
    println!("delta-base transfer benchmark (issue #51) — artifact {ARTIFACT_BYTES} B");
    println!(
        "| {:<34} | {:<6} | {:<12} | {:<7} | {:>9} | {:>8} |",
        "Scenario", "Chain", "Policy", "Base", "bytes_in", "ms",
    );
    println!(
        "| {0:-<34} | {0:-<6} | {0:-<12} | {0:-<7} | {0:-<9} | {0:-<8} |",
        "",
    );
    row(
        "A delta-friendly, changed line",
        "code",
        "thin",
        "present",
        a,
    );
    row(
        "B delta-friendly, same content",
        "code",
        "thin",
        "absent",
        b,
    );
    row(
        "C delta-hostile, rebuilt artifact",
        "extra",
        "whole-object",
        "present",
        c,
    );
    row(
        "D delta-hostile, rebuilt artifact",
        "code",
        "thin",
        "present",
        d,
    );
    println!();
    println!(
        "base-retention payoff (B/A): {:.1}×   thin-vs-whole on delta-hostile (D/C): {:.2}×",
        b.bytes_in as f64 / a.bytes_in.max(1) as f64,
        d.bytes_in as f64 / c.bytes_in.max(1) as f64,
    );
    println!();
}

// --- harness --------------------------------------------------------------

/// Bind a listener for `repo` on an ephemeral localhost port and serve it as a
/// detached task on the test's runtime, returning the bound address. Mirrors the
/// helper in `transfer.rs` (test files are separate compilation units, so it
/// can't be shared without coupling `test_support` to `gfs_server`/`tokio`).
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

/// Commit a tiny baseline so the artifact (not repo setup) dominates every push.
fn baseline(repo: &Path) {
    write_file(repo, "README", "delta-base benchmark baseline\n");
    commit_all(repo, "baseline");
}

/// Overwrite the artifact with `contents` and commit it.
fn commit_artifact(repo: &Path, contents: &[u8]) {
    write_file(repo, ARTIFACT, contents);
    commit_all(repo, "artifact");
}

/// Seed the delta base: push `ref_name` and discard the byte count.
async fn establish_push(client: &Path, addr: SocketAddr, ref_name: &str, policy: DeltaPolicy) {
    gfs_client::push_ref(client, &addr.to_string(), ref_name, policy)
        .await
        .expect("establish push succeeds");
}

/// Push `ref_name` and return the `receive` record the server wrote for it.
///
/// The server writes the record *after* it shuts the connection down — just after
/// the client's push returns — so this counts the matching records first, pushes,
/// then polls the sink until a new one appears (a real ordering race). Yields via
/// `tokio::time::sleep` so the `spawn_blocking` connection handler can finish.
async fn measure_push(
    server_repo: &Path,
    client: &Path,
    addr: SocketAddr,
    ref_name: &str,
    policy: DeltaPolicy,
) -> ReceiveStat {
    let prior = matching_receive_records(server_repo, ref_name).len();
    gfs_client::push_ref(client, &addr.to_string(), ref_name, policy)
        .await
        .expect("measured push succeeds");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let records = matching_receive_records(server_repo, ref_name);
        if records.len() > prior {
            return *records.last().expect("a new record");
        }
        assert!(
            Instant::now() < deadline,
            "metrics record for `{ref_name}` did not appear within the deadline",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// All `receive` records in `server_repo`'s metrics sink whose `refs_updated`
/// includes `ref_name`, in file order. Lines that don't yet parse (a concurrent
/// partial append) are skipped; the caller polls, so a transient miss just retries.
fn matching_receive_records(server_repo: &Path, ref_name: &str) -> Vec<ReceiveStat> {
    let path = gfs_common::metrics::metrics_path(server_repo);
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("receive"))
        .filter(|v| {
            v.get("refs_updated")
                .and_then(|r| r.as_array())
                .is_some_and(|refs| refs.iter().any(|r| r.as_str() == Some(ref_name)))
        })
        .map(|v| ReceiveStat {
            bytes_in: v.get("bytes_in").and_then(|n| n.as_u64()).unwrap_or(0),
            duration_ms: v.get("duration_ms").and_then(|n| n.as_f64()).unwrap_or(0.0),
        })
        .collect()
}

// --- deterministic artifacts ----------------------------------------------

/// Which revision of the delta-friendly artifact to generate.
#[derive(Clone, Copy)]
enum Variant {
    V1,
    V2,
}

/// ~`ARTIFACT_BYTES` of structured text. `V2` differs from `V1` by a single line
/// in the middle, modelling an incremental rebuild that touches a small region —
/// the case `--thin` deltas dramatically against a retained base.
fn delta_friendly(variant: Variant) -> Vec<u8> {
    const LINE_LEN: usize = 52; // "line 00012345 lorem ipsum dolor sit amet consectetur\n"
    let lines = ARTIFACT_BYTES / LINE_LEN;
    let changed = lines / 2;
    let mut out = String::with_capacity(lines * LINE_LEN);
    for i in 0..lines {
        if matches!(variant, Variant::V2) && i == changed {
            out.push_str(&format!(
                "line {i:08} CHANGED-IN-V2 dolor sit amet consectetur\n"
            ));
        } else {
            out.push_str(&format!(
                "line {i:08} lorem ipsum dolor sit amet consectetur\n"
            ));
        }
    }
    out.into_bytes()
}

const HOSTILE_SEED_V1: u64 = 0x9E37_79B9_7F4A_7C15;
const HOSTILE_SEED_V2: u64 = 0xD1B5_4A32_D192_ED03;

/// ~`ARTIFACT_BYTES` of incompressible pseudo-random bytes from a seeded
/// xorshift64 (deterministic, so results reproduce; not `getrandom`). Different
/// seeds yield fully different content, modelling a rebuilt binary that shares
/// nothing with its predecessor — the case `--thin` cannot delta at all.
fn delta_hostile(seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(ARTIFACT_BYTES);
    let mut x = seed;
    while out.len() < ARTIFACT_BYTES {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(ARTIFACT_BYTES);
    out
}
