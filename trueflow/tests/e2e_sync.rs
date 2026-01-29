use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;
use uuid::Uuid;

mod common;
use common::*;

fn record(id: &str, fingerprint: &str, timestamp: i64) -> Value {
    review_record(
        fingerprint,
        ReviewRecordOverrides {
            id: Some(id),
            email: Some("test@example.com"),
            timestamp: Some(timestamp),
            ..Default::default()
        },
    )
}

fn read_remote_reviews(remote_dir: &Path) -> Result<Vec<Value>> {
    let stdout = git_output(remote_dir, &["show", "trueflow-db:reviews.jsonl"])?;
    let mut records = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        records.push(serde_json::from_str::<Value>(line)?);
    }
    Ok(records)
}

#[test]
fn test_vet_sync() -> Result<()> {
    // GIVEN: a bare remote and a local repo with a review record
    // 1. Create "Remote" Repo (bare)
    let remote_dir = std::env::temp_dir()
        .join("trueflow_tests")
        .join(format!("remote_repo_{}.git", Uuid::new_v4()));
    if remote_dir.exists() {
        fs::remove_dir_all(&remote_dir)?;
    }
    fs::create_dir_all(&remote_dir)?;
    git_in(&remote_dir, &["init", "--bare"])?;

    // 2. Create "Local" Repo
    let local = TestRepo::new("local_repo")?;

    // Add remote
    let remote = remote_dir.to_str().context("remote repo path")?;
    git_in(&local.path, &["remote", "add", "origin", remote])?;

    // 3. Create some vet data locally
    local.run(&[
        "mark",
        "--fingerprint",
        "fp1",
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    // WHEN: we sync the local repo (push)
    // First sync might fail fetch (remote empty), but should push
    local.run(&["sync"])?;

    // Verify remote has the branch
    let stdout = git_output(&remote_dir, &["branch"])?;
    assert!(stdout.contains("trueflow-db"));

    // 5. Clone another repo (simulating colleague)
    let colleague = TestRepo::new("colleague_repo")?;
    git_in(&colleague.path, &["remote", "add", "origin", remote])?;

    // WHEN: a colleague syncs from the same remote (fetch)
    colleague.run(&["sync"])?;

    // THEN: the colleague sees the review records
    let stdout = git_output(&colleague.path, &["show", "trueflow-db:reviews.jsonl"])?;
    assert!(stdout.contains("fp1"));
    assert!(stdout.contains("approved"));

    Ok(())
}

#[test]
fn test_sync_dedupes_and_sorts_records() -> Result<()> {
    // GIVEN: local and colleague repos with overlapping review IDs
    let remote_dir = std::env::temp_dir()
        .join("trueflow_tests")
        .join(format!("remote_repo_dedup_{}.git", Uuid::new_v4()));
    if remote_dir.exists() {
        fs::remove_dir_all(&remote_dir)?;
    }
    fs::create_dir_all(&remote_dir)?;
    git_in(&remote_dir, &["init", "--bare"])?;

    let local = TestRepo::new("local_repo_dedup")?;
    let remote = remote_dir.to_str().context("remote repo path")?;
    git_in(&local.path, &["remote", "add", "origin", remote])?;

    let local_records = vec![record("dup", "fp-remote", 1500)];
    write_reviews_jsonl(&local.path.join(".trueflow"), &local_records)?;
    local.run(&["sync"])?;

    let colleague = TestRepo::new("colleague_repo_dedup")?;
    git_in(&colleague.path, &["remote", "add", "origin", remote])?;

    let colleague_records = vec![
        record("dup", "fp-local", 2000),
        record("unique", "fp-unique", 1000),
    ];
    write_reviews_jsonl(&colleague.path.join(".trueflow"), &colleague_records)?;
    colleague.run(&["sync"])?;

    // THEN: remote records are deduped by id and sorted by timestamp
    let records = read_remote_reviews(&remote_dir)?;
    let ids: Vec<&str> = records
        .iter()
        .filter_map(|record| record["id"].as_str())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(ids.iter().filter(|id| **id == "dup").count(), 1);
    assert!(ids.contains(&"unique"));

    let timestamps: Vec<i64> = records
        .iter()
        .filter_map(|record| record["timestamp"].as_i64())
        .collect();
    assert!(timestamps.windows(2).all(|pair| pair[0] <= pair[1]));

    Ok(())
}
