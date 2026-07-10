use anyhow::Result;
use std::fs;
use std::process::Command;

use trueflow::store::{BlockState, ReviewTargetRef};
use trueflow_test_support::*;

#[test]
fn test_mark_uncommitted_state() -> Result<()> {
    let repo = TestRepo::new("uncommitted_state")?;

    // 1. Create committed file
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Initial commit")?;

    // 2. Modify it (make it dirty)
    repo.write("src/main.rs", "fn main() { println!(\"dirty\"); }\n")?;

    // 3. Scan to get hash of dirty block
    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    // 4. Mark it
    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "approved",
        "--path",
        "src/main.rs",
        "--quiet",
    ])?;

    // 5. Check DB
    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);

    assert_eq!(records[0].block_state, BlockState::Uncommitted);

    Ok(())
}

#[test]
fn mark_staged_only_change_is_uncommitted() -> Result<()> {
    let repo = TestRepo::new("staged_only_uncommitted_state")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Initial commit")?;

    repo.write("src/main.rs", "fn main() { println!(\"staged\"); }\n")?;
    repo.add("src/main.rs")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "approved",
        "--path",
        "src/main.rs",
        "--quiet",
    ])?;

    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].block_state, BlockState::Uncommitted);

    Ok(())
}

#[test]
fn test_mark_committed_state_normalizes_subdir_relative_path_hint() -> Result<()> {
    let repo = TestRepo::new("committed_state_subdir_hint")?;

    repo.write("pkg/src/lib.rs", "pub fn stable() {}\n")?;
    repo.commit_all("Initial commit")?;

    let package_dir = repo.path.join("pkg");
    let output = repo.run_in(&["review", "--all", "--json"], &package_dir)?;
    let hash = first_block_hash(&output)?;

    repo.run_in(
        &[
            "mark",
            "--fingerprint",
            &hash,
            "--verdict",
            "approved",
            "--path",
            "src/lib.rs",
            "--quiet",
        ],
        &package_dir,
    )?;

    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].block_state, BlockState::Committed);
    assert_eq!(
        records[0].path_hint.as_ref().map(|path| path.as_str()),
        Some("pkg/src/lib.rs")
    );

    Ok(())
}

#[test]
fn test_mark_unknown_state_no_path() -> Result<()> {
    let repo = TestRepo::new("unknown_state")?;

    // Just mark arbitrary hash without path
    let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    repo.run(&[
        "mark",
        "--fingerprint",
        hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);

    assert_eq!(records[0].block_state, BlockState::Unknown);

    Ok(())
}

#[test]
fn test_mark_diff_block_hash_records_block_target() -> Result<()> {
    let repo = TestRepo::new("mark_diff_block_hash_block_target")?;
    repo.write("src/lib.rs", "pub fn value() -> i32 { 1 }\n")?;
    repo.commit_all("Initial")?;
    repo.git(&["checkout", "-B", "main"])?;
    repo.git(&["checkout", "-b", "feature/diff-block-hash"])?;

    repo.write("src/lib.rs", "pub fn value() -> i32 { 2 }\n")?;
    repo.commit_all("Change value")?;

    let review_output = repo.run(&["review", "--target", "main", "--json"])?;
    let fingerprint = first_block_hash(&review_output)?;

    repo.run(&[
        "mark",
        "--fingerprint",
        &fingerprint,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);
    match &records[0].target {
        ReviewTargetRef::Block { hash } => assert_eq!(hash.as_str(), fingerprint),
        other => panic!("expected block target, got {other:?}"),
    }

    let changes_after = json_array(&repo.run(&["review", "--target", "main", "--json"])?)?;
    assert!(changes_after.is_empty());

    Ok(())
}

#[test]
fn test_store_subdirectory_discovery() -> Result<()> {
    let repo = TestRepo::new("subdir_discovery")?;

    let subdir = repo.path.join("subdir");
    fs::create_dir(&subdir)?;

    let hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // Run mark from subdir
    repo.run_in(
        &[
            "mark",
            "--fingerprint",
            hash,
            "--verdict",
            "approved",
            "--quiet",
        ],
        &subdir,
    )?;

    // Check DB at ROOT
    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    assert!(db_path.exists(), "DB should be at repo root");

    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);
    assert_eq!(record_target_key(&records[0]), hash);

    Ok(())
}

#[test]
fn test_mark_signing_failure() -> Result<()> {
    let repo = TestRepo::new("signing_fail")?;

    // Configure signing key that doesn't exist
    repo.git(&["config", "user.signingkey", "DEADBEEF"])?;

    let hash = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    // Attempt mark, expect failure
    let output = repo.run_err(&[
        "mark",
        "--fingerprint",
        hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    // The error message varies by environment (GPG version, locale, whether GPG is installed).
    // We just verify:
    // 1. The command failed (run_err ensures this)
    // 2. There's some error output mentioning signing-related keywords
    let output_lower = output.to_lowercase();
    assert!(
        output_lower.contains("gpg")
            || output_lower.contains("sign")
            || output_lower.contains("key")
            || output_lower.contains("spawn"),
        "Expected signing-related error message, got: {output}"
    );

    Ok(())
}

#[test]
fn test_store_parent_discovery_no_git() -> Result<()> {
    // Manually set up dirs without git
    let root = temp_test_dir("no_git_discovery");
    fs::create_dir_all(&root)?;

    // Create .trueflow at root
    let trueflow_dir = root.join(".trueflow");
    fs::create_dir(&trueflow_dir)?;

    // Create subdir
    let subdir = root.join("subdir");
    fs::create_dir(&subdir)?;

    let bin = env!("CARGO_BIN_EXE_trueflow");
    let hash = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    let output = Command::new(bin)
        .args([
            "mark",
            "--fingerprint",
            hash,
            "--verdict",
            "approved",
            "--quiet",
        ])
        .current_dir(&subdir)
        .output()?;

    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "trueflow failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Verify DB at root
    let db_path = trueflow_dir.join("reviews.jsonl");
    assert!(
        db_path.exists(),
        "DB should be found/created in parent .trueflow"
    );

    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);

    Ok(())
}

#[test]
fn test_commands_write_logs_under_trueflow_directory() -> Result<()> {
    let repo = TestRepo::new("logs_under_trueflow")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Initial")?;

    repo.run(&["scan", "--json"])?;

    let log_dir = repo.path.join(".trueflow").join("logs");
    assert!(log_dir.is_dir(), "expected .trueflow/logs to exist");

    let has_log_file = fs::read_dir(&log_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .any(|path| path.extension().and_then(|ext| ext.to_str()) == Some("log"));
    assert!(has_log_file, "expected at least one .log file in logs dir");

    Ok(())
}
