//! Token integration tests for the server.
//!
//! These do not exercise any server logic yet (it is stubbed with `todo!()`);
//! they establish the temp-git-repo harness that real tests will build on.

use std::process::Command;

use test_support::init_temp_repo;

#[test]
fn temp_repo_is_a_git_repository() {
    let repo = init_temp_repo();

    assert!(repo.path().join(".git").is_dir(), "`.git` directory exists");

    let output = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(repo.path())
        .output()
        .expect("run `git rev-parse`");
    assert!(output.status.success(), "`git rev-parse` succeeds");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "true");
}
