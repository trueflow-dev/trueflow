use anyhow::Result;
use std::fs;
use std::process::Command;

mod common;
use common::*;
use trueflow::store::BlockState;

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
fn test_mark_unknown_state_no_path() -> Result<()> {
    let repo = TestRepo::new("unknown_state")?;

    // Just mark arbitrary hash without path
    let hash = "abc1234567890abcdef1234567890abcdef12";
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
fn test_store_subdirectory_discovery() -> Result<()> {
    let repo = TestRepo::new("subdir_discovery")?;

    let subdir = repo.path.join("subdir");
    fs::create_dir(&subdir)?;

    let hash = "def1234567890abcdef1234567890abcdef12";

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
    assert_eq!(records[0].fingerprint, hash);

    Ok(())
}

#[test]
fn test_mark_signing_failure() -> Result<()> {
    let repo = TestRepo::new("signing_fail")?;

    // Configure signing key that doesn't exist
    Command::new("git")
        .args(["config", "user.signingkey", "DEADBEEF"])
        .current_dir(&repo.path)
        .output()?;

    let hash = "bad1234567890abcdef1234567890abcdef12";

    // Attempt mark, expect failure
    let output = repo.run_err(&[
        "mark",
        "--fingerprint",
        hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    assert!(output.contains("GPG signing failed") || output.contains("Failed to spawn gpg"));

    Ok(())
}

#[test]
fn test_store_parent_discovery_no_git() -> Result<()> {
    // Manually set up dirs without git
    let root = std::env::temp_dir()
        .join("trueflow_tests")
        .join("no_git_discovery")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&root)?;

    // Create .trueflow at root
    let trueflow_dir = root.join(".trueflow");
    fs::create_dir(&trueflow_dir)?;

    // Create subdir
    let subdir = root.join("subdir");
    fs::create_dir(&subdir)?;

    let bin = env!("CARGO_BIN_EXE_trueflow");
    let hash = "1234567890abcdef1234567890abcdef12";

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
