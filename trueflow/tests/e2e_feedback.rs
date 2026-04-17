use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use serde_json::Value;

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

fn head_revision(repo: &TestRepo) -> Result<String> {
    Ok(run_git_output(&repo.path, &["rev-parse", "HEAD"])?
        .trim()
        .to_string())
}

fn build_block_review_record<'a>(
    hash: &str,
    revision: &'a str,
    path: &'a str,
    overrides: ReviewRecordOverrides<'a>,
) -> Value {
    build_review_record(
        hash,
        ReviewRecordOverrides {
            repo_revision: Some(revision),
            path_hint: Some(path),
            line_hint: Some(0),
            ..overrides
        },
    )
}

#[test]
fn test_feedback_since_unix_timestamp_includes_boundary_timestamp() -> Result<()> {
    let repo = TestRepo::new("feedback_since_boundary")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    let review_output = repo.run(&["review", "--all", "--json"])?;
    let hash = block_hash_for_path(&review_output, "src/lib.rs")?;
    let revision = head_revision(&repo)?;

    let old_review = build_block_review_record(
        &hash,
        &revision,
        "src/lib.rs",
        ReviewRecordOverrides {
            id: Some("old"),
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    let boundary_review = build_block_review_record(
        &hash,
        &revision,
        "src/lib.rs",
        ReviewRecordOverrides {
            id: Some("boundary"),
            verdict: Some("comment"),
            timestamp: Some(2000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&repo.path.join(".trueflow"), &[old_review, boundary_review])?;

    let output = repo.run(&["feedback", "--format", "json", "--since", "2000"])?;
    let entries = feedback_entries(&output)?;
    let entry = entries.first().context("expected feedback entry")?;
    let reviews = entry["reviews"]
        .as_array()
        .context("reviews should be array")?;

    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0]["id"].as_str(), Some("boundary"));
    assert_eq!(reviews[0]["timestamp"].as_i64(), Some(2000));

    Ok(())
}

#[test]
fn test_feedback_since_relative_duration_survives_tree_drift() -> Result<()> {
    let repo = TestRepo::new("feedback_relative_since_tree_drift")?;
    repo.write("src/lib.rs", "pub fn original() {}\n")?;
    repo.commit_all("Initial")?;

    let original_revision = head_revision(&repo)?;
    let review_output = repo.run(&["review", "--all", "--json"])?;
    let original_hash = block_hash_for_path(&review_output, "src/lib.rs")?;
    let recent_timestamp = (Utc::now() - Duration::hours(1)).timestamp();
    let review = build_block_review_record(
        &original_hash,
        &original_revision,
        "src/lib.rs",
        ReviewRecordOverrides {
            id: Some("recent-original"),
            verdict: Some("comment"),
            timestamp: Some(recent_timestamp),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&repo.path.join(".trueflow"), &[review])?;

    repo.write("src/lib.rs", "pub fn rewritten() {}\n")?;
    repo.commit_all("Rewrite lib")?;

    let output = repo.run(&["feedback", "--format", "json", "--since", "48h"])?;
    let entries = feedback_entries(&output)?;
    let entry = entries
        .first()
        .context("expected feedback entry after drift")?;
    let content = entry["block"]["content"]
        .as_str()
        .context("block content should be string")?;
    let reviews = entry["reviews"]
        .as_array()
        .context("reviews should be array")?;

    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0]["id"].as_str(), Some("recent-original"));
    assert!(content.contains("pub fn original() {}"));
    assert!(!content.contains("pub fn rewritten() {}"));

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
    let revision = head_revision(&repo)?;
    let keep_review = build_block_review_record(
        &keep_hash,
        &revision,
        "src/keep.rs",
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    let skip_review = build_block_review_record(
        &skip_hash,
        &revision,
        "src/skip.rs",
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

    let first_revision = head_revision(&repo)?;
    let review_output = repo.run(&["review", "--all", "--json"])?;
    let old_hash = block_hash_for_path(&review_output, "src/lib.rs")?;
    let old_review = build_block_review_record(
        &old_hash,
        &first_revision,
        "src/lib.rs",
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

    let first_revision = head_revision(&repo)?;
    let review_output = repo.run(&["review", "--all", "--json"])?;
    let src_hash = block_hash_for_path(&review_output, "src/lib.rs")?;
    let docs_hash = block_hash_for_path(&review_output, "docs/guide.md")?;
    let src_review = build_block_review_record(
        &src_hash,
        &first_revision,
        "src/lib.rs",
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    let docs_review = build_block_review_record(
        &docs_hash,
        &first_revision,
        "docs/guide.md",
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

#[test]
fn test_feedback_since_last_includes_new_same_second_record_without_repeating_old() -> Result<()> {
    let repo = TestRepo::new("feedback_since_last_same_second")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Initial")?;

    let revision = head_revision(&repo)?;
    let review_output = repo.run(&["review", "--all", "--json"])?;
    let hash = block_hash_for_path(&review_output, "src/lib.rs")?;
    let first_review = build_block_review_record(
        &hash,
        &revision,
        "src/lib.rs",
        ReviewRecordOverrides {
            id: Some("first"),
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(
        &repo.path.join(".trueflow"),
        std::slice::from_ref(&first_review),
    )?;

    let first_output = repo.run(&["feedback", "--format", "json", "--since", "last"])?;
    let first_entries = feedback_entries(&first_output)?;
    let first_reviews = first_entries[0]["reviews"]
        .as_array()
        .context("first reviews should be array")?;
    assert_eq!(first_reviews.len(), 1);
    assert_eq!(first_reviews[0]["id"].as_str(), Some("first"));

    let second_review = build_block_review_record(
        &hash,
        &revision,
        "src/lib.rs",
        ReviewRecordOverrides {
            id: Some("second"),
            verdict: Some("comment"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&repo.path.join(".trueflow"), &[first_review, second_review])?;

    let second_output = repo.run(&["feedback", "--format", "json", "--since", "last"])?;
    let second_entries = feedback_entries(&second_output)?;
    let second_reviews = second_entries[0]["reviews"]
        .as_array()
        .context("second reviews should be array")?;

    assert_eq!(second_reviews.len(), 1);
    assert_eq!(second_reviews[0]["id"].as_str(), Some("second"));

    Ok(())
}

#[test]
fn test_feedback_target_revision_range_filters_by_record_revision() -> Result<()> {
    let repo = TestRepo::new("feedback_target_revision_range_records")?;
    repo.write("src/lib.rs", "pub fn before() {}\n")?;
    repo.commit_all("A")?;
    let start_revision = head_revision(&repo)?;

    repo.write("src/lib.rs", "pub fn in_range() {}\n")?;
    repo.commit_all("B")?;
    let in_range_revision = head_revision(&repo)?;
    let in_range_output = repo.run(&["review", "--all", "--json"])?;
    let in_range_hash = block_hash_for_path(&in_range_output, "src/lib.rs")?;

    repo.write("docs/guide.md", "later docs change\n")?;
    repo.commit_all("C")?;
    let outside_revision = head_revision(&repo)?;

    let in_range_review = build_block_review_record(
        &in_range_hash,
        &in_range_revision,
        "src/lib.rs",
        ReviewRecordOverrides {
            id: Some("in-range"),
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    let outside_review = build_block_review_record(
        &in_range_hash,
        &outside_revision,
        "src/lib.rs",
        ReviewRecordOverrides {
            id: Some("outside-range"),
            verdict: Some("comment"),
            timestamp: Some(1001),
            ..Default::default()
        },
    );
    write_reviews_jsonl(
        &repo.path.join(".trueflow"),
        &[in_range_review, outside_review],
    )?;

    let output = repo.run(&[
        "feedback",
        "--format",
        "json",
        "--since",
        "all",
        "--target",
        &format!("rev:{start_revision}..{in_range_revision}"),
    ])?;
    let entries = feedback_entries(&output)?;
    let reviews = entries[0]["reviews"]
        .as_array()
        .context("reviews should be array")?;

    assert_eq!(entries.len(), 1);
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0]["id"].as_str(), Some("in-range"));

    Ok(())
}

#[test]
fn test_feedback_target_revision_range_uses_record_revision_context_after_later_drift() -> Result<()>
{
    let repo = TestRepo::new("feedback_target_revision_range_context")?;
    repo.write("src/lib.rs", "pub fn before() {}\n")?;
    repo.commit_all("A")?;
    let start_revision = head_revision(&repo)?;

    repo.write("src/lib.rs", "pub fn in_range() {}\n")?;
    repo.commit_all("B")?;
    let in_range_revision = head_revision(&repo)?;
    let in_range_output = repo.run(&["review", "--all", "--json"])?;
    let in_range_hash = block_hash_for_path(&in_range_output, "src/lib.rs")?;
    let in_range_review = build_block_review_record(
        &in_range_hash,
        &in_range_revision,
        "src/lib.rs",
        ReviewRecordOverrides {
            id: Some("mid-review"),
            verdict: Some("comment"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&repo.path.join(".trueflow"), &[in_range_review])?;

    repo.write("src/lib.rs", "pub fn after() {}\n")?;
    repo.commit_all("C")?;
    let end_revision = head_revision(&repo)?;

    let output = repo.run(&[
        "feedback",
        "--format",
        "json",
        "--since",
        "all",
        "--target",
        &format!("rev:{start_revision}..{end_revision}"),
    ])?;
    let entries = feedback_entries(&output)?;
    let content = entries[0]["block"]["content"]
        .as_str()
        .context("block content should be string")?;
    let reviews = entries[0]["reviews"]
        .as_array()
        .context("reviews should be array")?;

    assert_eq!(entries.len(), 1);
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0]["id"].as_str(), Some("mid-review"));
    assert!(content.contains("pub fn in_range() {}"));
    assert!(!content.contains("pub fn after() {}"));

    Ok(())
}
