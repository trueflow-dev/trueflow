use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;

mod common;
use common::*;

fn get_main_review_json(repo: &TestRepo) -> Result<Vec<Value>> {
    let output = repo.run(&["review", "--target", "main", "--json"])?;
    json_array(&output)
}

fn first_main_review_block_hash(repo: &TestRepo) -> Result<String> {
    let output = repo.run(&["review", "--target", "main", "--json"])?;
    first_block_hash(&output)
}

fn review_file_by_path<'a>(files: &'a [Value], path: &str) -> Result<&'a Value> {
    files
        .iter()
        .find(|file| file["path"].as_str() == Some(path))
        .with_context(|| format!("expected review output for path {path}"))
}

fn file_block_contents<'a>(file: &'a Value) -> Result<Vec<&'a str>> {
    file["blocks"]
        .as_array()
        .context("blocks")?
        .iter()
        .map(|block| block["content"].as_str().context("block content"))
        .collect()
}

fn first_file_block_hash(file: &Value) -> Result<String> {
    file["blocks"]
        .as_array()
        .context("blocks")?
        .first()
        .context("expected block")?["hash"]
        .as_str()
        .context("hash")
        .map(ToString::to_string)
}

const LIB_ADD: &str = include_str!("fixtures/diff_lib_add.rs");
const LIB_ADD_SUB: &str = include_str!("fixtures/diff_lib_add_sub.rs");
const RENAME_NEW: &str = include_str!("fixtures/diff_rename_new.rs");
const RENAME_OLD: &str = "pub fn alpha() {}\n";

fn checkout_branch(repo: &TestRepo, branch: &str) -> Result<()> {
    repo.git(&["checkout", "-b", branch])
}

fn head_revision(repo: &TestRepo) -> Result<String> {
    Ok(run_git_output(&repo.path, &["rev-parse", "HEAD"])?
        .trim()
        .to_string())
}

#[test]
fn test_main_review_json_uses_semantic_review_shape() -> Result<()> {
    let repo = TestRepo::new("initial_state")?;
    repo.write("src/main.rs", "fn main() { println!(\"Hello\"); }")?;
    repo.commit_all("Initial commit")?;

    checkout_branch(&repo, "feature/add-greeting")?;
    repo.write("src/main.rs", "fn main() { println!(\"Hello World\"); }")?;
    repo.commit_all("Update greeting")?;

    let files = get_main_review_json(&repo)?;
    assert_eq!(files.len(), 1);

    let file = &files[0];
    assert_eq!(file["path"].as_str().context("path")?, "src/main.rs");
    assert!(file.get("language").is_some(), "expected language field");
    assert!(file.get("fingerprint").is_none());
    assert!(file.get("status").is_none());
    assert!(file.get("diff_content").is_none());

    let blocks = file["blocks"].as_array().context("blocks")?;
    assert!(!blocks.is_empty(), "expected changed semantic blocks");
    assert!(blocks.iter().any(|block| {
        block["content"]
            .as_str()
            .is_some_and(|content| content.contains("Hello World"))
    }));
    assert!(blocks.iter().all(|block| block["hash"].as_str().is_some()));
    assert!(blocks.iter().all(|block| block["kind"].as_str().is_some()));

    Ok(())
}

#[test]
fn test_main_review_and_mark_flow_uses_block_reviews() -> Result<()> {
    let repo = TestRepo::new("mark_flow")?;
    repo.write("src/lib.rs", LIB_ADD)?;
    repo.commit_all("Initial")?;

    checkout_branch(&repo, "feature/sub")?;
    repo.write("src/lib.rs", LIB_ADD_SUB)?;
    repo.commit_all("Add sub")?;

    let hash = first_main_review_block_hash(&repo)?;

    let output = repo.run(&["review", "--target", "main"])?;
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

    let changes = get_main_review_json(&repo)?;
    assert!(changes.is_empty());

    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "rejected",
        "--quiet",
    ])?;

    let changes = get_main_review_json(&repo)?;
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

    let review_output = repo.run(&["review", "--target", "main"])?;
    assert!(review_output.contains("File: src/lib.rs"));

    let fp = first_main_review_block_hash(&repo)?;
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

    let fake_diff_like_fingerprint =
        "5555555555555555555555555555555555555555555555555555555555555555";
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
fn test_main_review_ignores_non_review_checks() -> Result<()> {
    let repo = TestRepo::new("diff_non_review")?;
    repo.write("src/lib.rs", LIB_ADD)?;
    repo.commit_all("Initial")?;

    checkout_branch(&repo, "feature/security")?;
    repo.write("src/lib.rs", LIB_ADD_SUB)?;
    repo.commit_all("Add sub")?;

    let fp = first_main_review_block_hash(&repo)?;

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

    let changes = get_main_review_json(&repo)?;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["path"].as_str().context("path")?, "src/lib.rs");

    Ok(())
}

#[test]
fn test_main_review_ignores_untracked_files() -> Result<()> {
    let repo = TestRepo::new("diff_untracked")?;
    repo.write("src/lib.rs", "pub fn stable() {}\n")?;
    repo.commit_all("Initial")?;

    repo.write("src/untracked.rs", "pub fn draft() {}\n")?;

    let changes = get_main_review_json(&repo)?;
    assert!(changes.is_empty());

    Ok(())
}

#[test]
fn test_review_since_matches_revision_range_target() -> Result<()> {
    let repo = TestRepo::new("review_since_range")?;
    repo.write("src/lib.rs", "pub fn one() {}\n")?;
    repo.commit_all("Initial")?;
    let base = head_revision(&repo)?;

    repo.write("src/lib.rs", "pub fn one() {}\npub fn two() {}\n")?;
    repo.commit_all("Add two")?;

    let since_output = repo.run(&["review", "--since", &base, "--json"])?;
    let range_output = repo.run(&["review", "--target", &format!("rev:{base}..HEAD"), "--json"])?;

    assert_eq!(json(&since_output)?, json(&range_output)?);
    Ok(())
}

#[test]
fn test_review_since_rejects_unknown_revision() -> Result<()> {
    let repo = TestRepo::new("review_since_invalid")?;
    repo.write("src/lib.rs", "pub fn one() {}\n")?;
    repo.commit_all("Initial")?;

    let stderr = repo.run_err(&["review", "--since", "definitely-not-a-real-revision"])?;
    assert!(
        stderr.contains("could not be resolved"),
        "unexpected stderr: {stderr}"
    );

    Ok(())
}

#[test]
fn test_main_review_handles_renamed_file() -> Result<()> {
    let repo = TestRepo::new("diff_rename")?;
    repo.write("src/old.rs", RENAME_OLD)?;
    repo.commit_all("Add alpha")?;

    checkout_branch(&repo, "feature/rename")?;

    repo.git(&["mv", "src/old.rs", "src/new.rs"])?;
    repo.write("src/new.rs", RENAME_NEW)?;
    repo.commit_all("Rename and expand")?;

    let changes = get_main_review_json(&repo)?;
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
fn test_main_review_skips_binary_changes() -> Result<()> {
    let repo = TestRepo::new("diff_binary")?;
    let binary_path = repo.path.join("binary.bin");
    fs::write(&binary_path, [0, 255, 0, 1])?;
    repo.commit_all("Add binary")?;

    checkout_branch(&repo, "feature/binary")?;

    fs::write(&binary_path, [0, 255, 2, 3])?;
    repo.commit_all("Update binary")?;

    let changes = get_main_review_json(&repo)?;
    assert!(changes.is_empty());

    Ok(())
}

#[test]
fn test_main_review_json_keeps_deleted_whole_file_semantic_blocks() -> Result<()> {
    // GIVEN: a file exists on main and the feature branch deletes that whole file
    let repo = TestRepo::new("diff_deleted_whole_file")?;
    repo.write(
        "src/obsolete.rs",
        "pub fn removed_helper() {\n    println!(\"base only marker\");\n}\n",
    )?;
    repo.commit_all("Add obsolete helper")?;

    checkout_branch(&repo, "feature/delete-obsolete")?;
    fs::remove_file(repo.path.join("src/obsolete.rs"))?;
    repo.commit_all("Delete obsolete helper")?;

    // WHEN: diff-scoped review is collected against main
    let files = get_main_review_json(&repo)?;
    let deleted_file = review_file_by_path(&files, "src/obsolete.rs")?;
    let contents = file_block_contents(deleted_file)?;

    // THEN: the deleted file still appears with its base-side semantic block content
    assert_eq!(files.len(), 1);
    assert!(
        contents
            .iter()
            .any(|content| content.contains("removed_helper")
                && content.contains("base only marker")),
        "expected deleted file review to reconstruct base-side content: {contents:?}"
    );

    Ok(())
}

#[test]
fn test_deleted_block_approval_round_trip_unblocks_check() -> Result<()> {
    // GIVEN: a feature branch deletes a reviewable semantic block from main
    let repo = TestRepo::new("diff_deleted_block_round_trip")?;
    repo.write(
        "src/legacy.rs",
        "pub fn legacy_path() {\n    println!(\"delete me\");\n}\n",
    )?;
    repo.commit_all("Add legacy path")?;

    checkout_branch(&repo, "feature/remove-legacy")?;
    fs::remove_file(repo.path.join("src/legacy.rs"))?;
    repo.commit_all("Delete legacy path")?;

    // WHEN: we approve the deleted block that review exposes
    let before_check = repo.run_raw(&["check"])?;
    let review_before = get_main_review_json(&repo)?;
    let deleted_file = review_file_by_path(&review_before, "src/legacy.rs")?;
    let deleted_hash = first_file_block_hash(deleted_file)?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &deleted_hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    // THEN: check succeeds and the deleted block disappears from review output
    assert!(
        !before_check.status.success(),
        "expected check to fail before approving deleted block"
    );
    assert!(get_main_review_json(&repo)?.is_empty());

    let after_check = repo.run(&["check"])?;
    assert!(
        after_check.trim().is_empty(),
        "expected check to stay silent after approving deleted block"
    );

    Ok(())
}

#[test]
fn test_main_review_errors_without_main_branch() -> Result<()> {
    let repo = TestRepo::new("diff_no_main")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Initial")?;

    repo.git(&["branch", "-m", "trunk"])?;

    let output = repo.run_err(&["review", "--target", "main", "--json"])?;
    assert!(output.contains("main") || output.contains("master"));

    Ok(())
}
