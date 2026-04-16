use anyhow::{Context, Result};
use chrono::{Duration, Utc};

use trueflow_test_support::*;

fn path_matches(file: &serde_json::Value, expected: &str) -> bool {
    file["path"]
        .as_str()
        .is_some_and(|path| path.trim_start_matches("./") == expected)
}

fn block_hash_for_path(output: &str, expected_path: &str) -> Result<String> {
    let files = json_array(output)?;
    let file = files
        .iter()
        .find(|file| path_matches(file, expected_path))
        .with_context(|| format!("missing scan/review output for {expected_path}"))?;
    let blocks = file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let hash = blocks.first().context("expected at least one block")?["hash"]
        .as_str()
        .context("hash should be string")?;
    Ok(hash.to_string())
}

fn feedback_entries(output: &str) -> Result<Vec<serde_json::Value>> {
    json_array(output)
}

#[test]
fn test_feedback_since_relative_duration_filters_history() -> Result<()> {
    let repo = TestRepo::new("feedback_relative_since")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    let review_output = repo.run(&["review", "--all", "--json"])?;
    let hash = block_hash_for_path(&review_output, "src/lib.rs")?;

    let old_timestamp = (Utc::now() - Duration::hours(2)).timestamp();
    let new_timestamp = (Utc::now() - Duration::minutes(30)).timestamp();
    let old_review = build_review_record(
        &hash,
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            timestamp: Some(old_timestamp),
            ..Default::default()
        },
    );
    let new_review = build_review_record(
        &hash,
        ReviewRecordOverrides {
            verdict: Some("comment"),
            timestamp: Some(new_timestamp),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&repo.path.join(".trueflow"), &[old_review, new_review])?;

    let output = repo.run(&["feedback", "--format", "json", "--since", "1h"])?;
    let entries = feedback_entries(&output)?;
    let entry = entries.first().context("expected feedback entry")?;
    let reviews = entry["reviews"]
        .as_array()
        .context("reviews should be array")?;

    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0]["timestamp"].as_i64(), Some(new_timestamp));

    Ok(())
}

#[test]
fn test_feedback_target_file_filters_current_workdir() -> Result<()> {
    let repo = TestRepo::new("feedback_target_file")?;
    repo.write("src/keep.rs", "pub fn keep() {}\n")?;
    repo.write("src/skip.rs", "pub fn skip() {}\n")?;
    repo.commit_all("Add files")?;

    let review_output = repo.run(&["review", "--all", "--json"])?;
    let keep_hash = block_hash_for_path(&review_output, "src/keep.rs")?;
    let skip_hash = block_hash_for_path(&review_output, "src/skip.rs")?;
    let keep_review = build_review_record(
        &keep_hash,
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    let skip_review = build_review_record(
        &skip_hash,
        ReviewRecordOverrides {
            verdict: Some("comment"),
            timestamp: Some(1001),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&repo.path.join(".trueflow"), &[keep_review, skip_review])?;

    let output = repo.run(&[
        "feedback",
        "--format",
        "json",
        "--since",
        "all",
        "--target",
        "file:src/keep.rs",
    ])?;
    let entries = feedback_entries(&output)?;

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["file"].as_str(), Some("src/keep.rs"));

    Ok(())
}

#[test]
fn test_feedback_target_revision_anchors_to_historical_tree() -> Result<()> {
    let repo = TestRepo::new("feedback_target_revision")?;
    repo.write("src/lib.rs", "pub fn old() {}\n")?;
    repo.commit_all("Initial")?;

    let first_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let first_revision = first_revision.trim().to_string();
    let review_output = repo.run(&["review", "--all", "--json"])?;
    let old_hash = block_hash_for_path(&review_output, "src/lib.rs")?;
    let old_review = build_review_record(
        &old_hash,
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&repo.path.join(".trueflow"), &[old_review])?;

    repo.write("src/lib.rs", "pub fn new() {}\n")?;
    repo.commit_all("Rename function")?;

    let output = repo.run(&[
        "feedback",
        "--format",
        "json",
        "--since",
        "all",
        "--target",
        &format!("rev:{first_revision}"),
    ])?;
    let entries = feedback_entries(&output)?;
    let entry = entries
        .first()
        .context("expected historical feedback entry")?;
    let content = entry["block"]["content"]
        .as_str()
        .context("block content should be string")?;

    assert_eq!(entry["file"].as_str(), Some("src/lib.rs"));
    assert!(content.contains("pub fn old() {}"));
    assert!(!content.contains("pub fn new() {}"));

    Ok(())
}

#[test]
fn test_feedback_target_dir_and_revision_intersect() -> Result<()> {
    let repo = TestRepo::new("feedback_target_dir_revision")?;
    repo.write("src/lib.rs", "pub fn inside() {}\n")?;
    repo.write("docs/guide.md", "hello docs\n")?;
    repo.commit_all("Initial")?;

    let first_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let first_revision = first_revision.trim().to_string();
    let review_output = repo.run(&["review", "--all", "--json"])?;
    let src_hash = block_hash_for_path(&review_output, "src/lib.rs")?;
    let docs_hash = block_hash_for_path(&review_output, "docs/guide.md")?;
    let src_review = build_review_record(
        &src_hash,
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    let docs_review = build_review_record(
        &docs_hash,
        ReviewRecordOverrides {
            verdict: Some("comment"),
            timestamp: Some(1001),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&repo.path.join(".trueflow"), &[src_review, docs_review])?;

    repo.write("src/lib.rs", "pub fn inside_new() {}\n")?;
    repo.write("docs/guide.md", "updated docs\n")?;
    repo.commit_all("Update both files")?;

    let output = repo.run(&[
        "feedback",
        "--format",
        "json",
        "--since",
        "all",
        "--target",
        "dir:src",
        "--target",
        &format!("rev:{first_revision}"),
    ])?;
    let entries = feedback_entries(&output)?;

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["file"].as_str(), Some("src/lib.rs"));
    let content = entries[0]["block"]["content"]
        .as_str()
        .context("block content should be string")?;
    assert!(content.contains("inside"));
    assert!(!content.contains("guide"));

    Ok(())
}
