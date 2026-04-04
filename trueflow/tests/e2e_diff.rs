use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;

mod common;
use common::*;

fn get_diff_json(repo: &TestRepo) -> Result<Vec<Value>> {
    let output = repo.run(&["diff", "--json"])?;
    json_array(&output)
}

fn first_diff_block_hash(repo: &TestRepo) -> Result<String> {
    let output = repo.run(&["diff", "--json"])?;
    first_block_hash(&output)
}

const LIB_ADD: &str = include_str!("fixtures/diff_lib_add.rs");
const LIB_ADD_SUB: &str = include_str!("fixtures/diff_lib_add_sub.rs");
const RENAME_NEW: &str = include_str!("fixtures/diff_rename_new.rs");
const RENAME_OLD: &str = "pub fn alpha() {}\n";

fn checkout_branch(repo: &TestRepo, branch: &str) -> Result<()> {
    repo.git(&["checkout", "-b", branch])
}

#[test]
fn test_diff_reports_unreviewed_blocks_for_main_diff() -> Result<()> {
    let repo = TestRepo::new("initial_state")?;
    repo.write("src/main.rs", "fn main() { println!(\"Hello\"); }")?;
    repo.commit_all("Initial commit")?;

    checkout_branch(&repo, "feature/add-greeting")?;
    repo.write("src/main.rs", "fn main() { println!(\"Hello World\"); }")?;
    repo.commit_all("Update greeting")?;

    let files = get_diff_json(&repo)?;
    assert_eq!(files.len(), 1);

    let file = &files[0];
    assert_eq!(file["path"].as_str().context("path")?, "src/main.rs");
    let blocks = file["blocks"].as_array().context("blocks")?;
    assert!(
        !blocks.is_empty(),
        "expected changed block in semantic diff"
    );
    assert!(blocks.iter().any(|block| {
        block["content"]
            .as_str()
            .is_some_and(|content| content.contains("Hello World"))
    }));

    Ok(())
}

#[test]
fn test_diff_and_mark_flow_uses_block_reviews() -> Result<()> {
    let repo = TestRepo::new("mark_flow")?;
    repo.write("src/lib.rs", LIB_ADD)?;
    repo.commit_all("Initial")?;

    checkout_branch(&repo, "feature/sub")?;
    repo.write("src/lib.rs", LIB_ADD_SUB)?;
    repo.commit_all("Add sub")?;

    let hash = first_diff_block_hash(&repo)?;

    let output = repo.run(&["diff"])?;
    assert!(output.contains("File: src/lib.rs"));
    assert!(output.contains("[Unreviewed]"));

    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    let changes = get_diff_json(&repo)?;
    assert!(changes.is_empty());

    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "rejected",
        "--quiet",
    ])?;

    let changes = get_diff_json(&repo)?;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["path"].as_str().context("path")?, "src/lib.rs");

    Ok(())
}

#[test]
fn test_check_command_gates_semantic_main_diff_blocks() -> Result<()> {
    let repo = TestRepo::new("check_gate")?;
    repo.write("src/lib.rs", LIB_ADD)?;
    repo.commit_all("Initial")?;

    checkout_branch(&repo, "feature/check")?;

    repo.write("src/lib.rs", LIB_ADD_SUB)?;
    repo.commit_all("Add sub")?;

    let output = repo.run_raw(&["check"])?;
    assert!(!output.status.success(), "Expected check to fail");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.trim().is_empty(),
        "Expected check to be silent on stdout"
    );

    let diff_output = repo.run(&["diff"])?;
    assert!(diff_output.contains("File: src/lib.rs"));

    let fp = first_diff_block_hash(&repo)?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &fp,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    let output = repo.run(&["check"])?;
    assert!(
        output.trim().is_empty(),
        "Expected check to be silent on stdout"
    );

    Ok(())
}

#[test]
fn test_check_ignores_legacy_diff_like_fingerprint_marks() -> Result<()> {
    let repo = TestRepo::new("check_diff_like_fingerprint_ignored")?;
    repo.write("src/lib.rs", LIB_ADD)?;
    repo.commit_all("Initial")?;

    checkout_branch(&repo, "feature/check-diff-like")?;
    repo.write("src/lib.rs", LIB_ADD_SUB)?;
    repo.commit_all("Add sub")?;

    let output = repo.run_raw(&["check"])?;
    assert!(!output.status.success(), "Expected check to fail");

    let fake_diff_like_fingerprint = "5555555555555555555555555555555555555555555555555555555555555555";
    repo.run(&[
        "mark",
        "--fingerprint",
        fake_diff_like_fingerprint,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    let output = repo.run_raw(&["check"])?;
    assert!(!output.status.success(), "Expected check to keep failing");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.trim().is_empty(),
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

    let fp = first_diff_block_hash(&repo)?;

    repo.run(&[
        "mark",
        "--fingerprint",
        &fp,
        "--verdict",
        "approved",
        "--check",
        "security",
        "--quiet",
    ])?;

    let changes = get_diff_json(&repo)?;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["path"].as_str().context("path")?, "src/lib.rs");

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
        change["path"]
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
