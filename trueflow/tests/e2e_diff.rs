use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;

mod common;
use common::*;

fn get_diff_json(repo: &TestRepo) -> Result<Vec<Value>> {
    let output = repo.run(&["diff", "--json"])?;
    json_array(&output)
}

const LIB_ADD: &str = include_str!("fixtures/diff_lib_add.rs");
const LIB_ADD_SUB: &str = include_str!("fixtures/diff_lib_add_sub.rs");
const RENAME_NEW: &str = include_str!("fixtures/diff_rename_new.rs");
const RENAME_OLD: &str = "pub fn alpha() {}\n";

fn checkout_branch(repo: &TestRepo, branch: &str) -> Result<()> {
    repo.git(&["checkout", "-b", branch])
}

#[test]
fn test_vet_diff_initial_state() -> Result<()> {
    let repo = TestRepo::new("initial_state")?;

    // 1. Create a file and commit it to main
    repo.write("src/main.rs", "fn main() { println!(\"Hello\"); }")?;
    repo.commit_all("Initial commit")?;

    // 2. Run `vet diff --json`
    // Since we just added code and haven't reviewed it, vet should show it as unreviewed.
    // Wait... if it's the initial commit, does it show up in diff?
    // "vet diff" usually compares HEAD vs main.
    // If we are ON main, diff main..HEAD is empty.
    // Ah, design says: "Get diff HEAD vs main".
    // If I just committed to main, HEAD == main.
    // So usually we vet a feature branch.

    // Let's create a feature branch.
    checkout_branch(&repo, "feature/add-greeting")?;

    repo.write("src/main.rs", "fn main() { println!(\"Hello World\"); }")?;
    repo.commit_all("Update greeting")?;

    // Now main has "Hello", feature has "Hello World".
    // vet diff should show the hunk.

    let changes = get_diff_json(&repo)?;

    // Validate we have 1 change
    assert_eq!(changes.len(), 1);

    let change = &changes[0];
    assert_eq!(change["file"].as_str().context("file")?, "src/main.rs");
    assert_eq!(change["status"].as_str().context("status")?, "unreviewed");

    let content = change["diff_content"].as_str().unwrap();
    assert!(content.contains("Hello World"));
    assert!(content.contains("-fn main() { println!(\"Hello\"); }"));

    Ok(())
}

#[test]
fn test_vet_mark_flow() -> Result<()> {
    let repo = TestRepo::new("mark_flow")?;
    repo.write("src/lib.rs", LIB_ADD)?;
    repo.commit_all("Initial")?;

    // Feature
    checkout_branch(&repo, "feature/sub")?;
    repo.write("src/lib.rs", LIB_ADD_SUB)?;
    repo.commit_all("Add sub")?;

    // 1. Get Diff
    let changes = get_diff_json(&repo)?;
    let output = repo.run(&["diff"])?;
    assert!(
        output.trim().is_empty(),
        "Expected diff to be silent on stdout"
    );
    let fp = changes[0]["fingerprint"].as_str().unwrap().to_string();

    // 2. Mark Approved
    repo.run(&[
        "mark",
        "--fingerprint",
        &fp,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    // 3. Verify Diff is Empty
    let changes = get_diff_json(&repo)?;
    assert!(changes.is_empty());

    // 4. Mark Rejected
    repo.run(&[
        "mark",
        "--fingerprint",
        &fp,
        "--verdict",
        "rejected",
        "--quiet",
    ])?;

    // 5. Verify Diff shows Rejected
    let changes = get_diff_json(&repo)?;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["status"].as_str().context("status")?, "rejected");

    // 6. Non-JSON diff is silent on stdout
    let output = repo.run(&["diff"])?;
    assert!(
        output.trim().is_empty(),
        "Expected diff to be silent on stdout"
    );

    Ok(())
}

#[test]
fn test_check_command_gates_unreviewed_changes() -> Result<()> {
    let repo = TestRepo::new("check_gate")?;
    repo.write("src/lib.rs", LIB_ADD)?;
    repo.commit_all("Initial")?;

    checkout_branch(&repo, "feature/check")?;

    repo.write("src/lib.rs", LIB_ADD_SUB)?;
    repo.commit_all("Add sub")?;

    // Expect failure
    let output = repo.run_raw(&["check"])?;
    assert!(!output.status.success(), "Expected check to fail");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.trim().is_empty(),
        "Expected check to be silent on stdout"
    );

    let diff_output = repo.run(&["diff"])?;
    assert!(
        diff_output.trim().is_empty(),
        "Expected diff to be silent on stdout"
    );

    let changes = get_diff_json(&repo)?;
    let fp = changes[0]["fingerprint"].as_str().expect("fingerprint");

    // Mark approved
    repo.run(&[
        "mark",
        "--fingerprint",
        fp,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    // Check pass
    let output = repo.run(&["check"])?;
    assert!(
        output.trim().is_empty(),
        "Expected check to be silent on stdout"
    );

    Ok(())
}

#[test]
fn test_diff_ignores_non_review_checks() -> Result<()> {
    let repo = TestRepo::new("diff_non_review")?;
    repo.write("src/lib.rs", LIB_ADD)?;
    repo.commit_all("Initial")?;

    checkout_branch(&repo, "feature/security")?;

    repo.write("src/lib.rs", LIB_ADD_SUB)?;
    repo.commit_all("Add sub")?;

    let changes = get_diff_json(&repo)?;
    let fp = changes[0]["fingerprint"].as_str().context("fingerprint")?;

    repo.run(&[
        "mark",
        "--fingerprint",
        fp,
        "--verdict",
        "approved",
        "--check",
        "security",
        "--quiet",
    ])?;

    let changes = get_diff_json(&repo)?;
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0]["status"].as_str().context("status")?,
        "unreviewed"
    );

    Ok(())
}

#[test]
fn test_diff_ignores_untracked_files() -> Result<()> {
    let repo = TestRepo::new("diff_untracked")?;
    repo.write("src/lib.rs", "pub fn stable() {}\n")?;
    repo.commit_all("Initial")?;

    repo.write("src/untracked.rs", "pub fn draft() {}\n")?;

    let changes = get_diff_json(&repo)?;
    assert!(changes.is_empty());

    Ok(())
}

#[test]
fn test_diff_handles_renamed_file() -> Result<()> {
    let repo = TestRepo::new("diff_rename")?;
    repo.write("src/old.rs", RENAME_OLD)?;
    repo.commit_all("Add alpha")?;

    checkout_branch(&repo, "feature/rename")?;

    repo.git(&["mv", "src/old.rs", "src/new.rs"])?;
    repo.write("src/new.rs", RENAME_NEW)?;
    repo.commit_all("Rename and expand")?;

    let changes = get_diff_json(&repo)?;
    assert!(!changes.is_empty());
    assert!(changes.iter().any(|change| {
        change["file"]
            .as_str()
            .map(|path| path == "src/new.rs")
            .unwrap_or(false)
    }));

    Ok(())
}

#[test]
fn test_diff_skips_binary_changes() -> Result<()> {
    let repo = TestRepo::new("diff_binary")?;
    let binary_path = repo.path.join("binary.bin");
    fs::write(&binary_path, [0, 255, 0, 1])?;
    repo.commit_all("Add binary")?;

    checkout_branch(&repo, "feature/binary")?;

    fs::write(&binary_path, [0, 255, 2, 3])?;
    repo.commit_all("Update binary")?;

    let changes = get_diff_json(&repo)?;
    assert!(changes.is_empty());

    Ok(())
}

#[test]
fn test_diff_errors_without_main_branch() -> Result<()> {
    let repo = TestRepo::new("diff_no_main")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Initial")?;

    repo.git(&["branch", "-m", "trunk"])?;

    let output = repo.run_err(&["diff", "--json"])?;
    assert!(output.contains("main") || output.contains("master"));

    Ok(())
}
