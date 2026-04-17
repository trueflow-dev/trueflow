use anyhow::{Context, Result};
use chrono::{Duration, Utc};

use trueflow_test_support::{FeedbackScenario, ReviewRecordOverrides};

#[test]
fn test_feedback_since_unix_timestamp_includes_boundary_timestamp() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_since_boundary")?;
    scenario.write("src/lib.rs", "pub fn core() {}\n")?;
    scenario.commit_all("Add lib")?;

    scenario.review_block_with_overrides(
        "src/lib.rs",
        "rejected",
        ReviewRecordOverrides {
            id: Some("old"),
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;
    scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        ReviewRecordOverrides {
            id: Some("boundary"),
            timestamp: Some(2000),
            ..Default::default()
        },
    )?;

    let entries = scenario.feedback_json(&["--since", "2000"])?;
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
    let scenario = FeedbackScenario::new("feedback_relative_since_tree_drift")?;
    scenario.write("src/lib.rs", "pub fn original() {}\n")?;
    scenario.commit_all("Initial")?;

    let recent_timestamp = (Utc::now() - Duration::hours(1)).timestamp();
    scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        ReviewRecordOverrides {
            id: Some("recent-original"),
            timestamp: Some(recent_timestamp),
            ..Default::default()
        },
    )?;

    scenario.write("src/lib.rs", "pub fn rewritten() {}\n")?;
    scenario.commit_all("Rewrite lib")?;

    let entries = scenario.feedback_json(&["--since", "48h"])?;
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
    let scenario = FeedbackScenario::new("feedback_target_file")?;
    scenario.write("src/keep.rs", "pub fn keep() {}\n")?;
    scenario.write("src/skip.rs", "pub fn skip() {}\n")?;
    scenario.commit_all("Add files")?;

    scenario.review_block_with_overrides(
        "src/keep.rs",
        "rejected",
        ReviewRecordOverrides {
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;
    scenario.review_block_with_overrides(
        "src/skip.rs",
        "comment",
        ReviewRecordOverrides {
            timestamp: Some(1001),
            ..Default::default()
        },
    )?;

    let entries = scenario.feedback_json(&["--since", "all", "--target", "file:src/keep.rs"])?;

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["file"].as_str(), Some("src/keep.rs"));

    Ok(())
}

#[test]
fn test_feedback_target_revision_anchors_to_historical_tree() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_target_revision")?;
    scenario.write("src/lib.rs", "pub fn old() {}\n")?;
    let first_revision = scenario.commit_all("Initial")?;

    scenario.review_block_with_overrides(
        "src/lib.rs",
        "rejected",
        ReviewRecordOverrides {
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;

    scenario.write("src/lib.rs", "pub fn new() {}\n")?;
    scenario.commit_all("Rename function")?;

    let entries = scenario.feedback_json(&[
        "--since",
        "all",
        "--target",
        &format!("rev:{first_revision}"),
    ])?;
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
    let scenario = FeedbackScenario::new("feedback_target_dir_revision")?;
    scenario.write("src/lib.rs", "pub fn inside() {}\n")?;
    scenario.write("docs/guide.md", "hello docs\n")?;
    let first_revision = scenario.commit_all("Initial")?;

    scenario.review_block_with_overrides(
        "src/lib.rs",
        "rejected",
        ReviewRecordOverrides {
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;
    scenario.review_block_with_overrides(
        "docs/guide.md",
        "comment",
        ReviewRecordOverrides {
            timestamp: Some(1001),
            ..Default::default()
        },
    )?;

    scenario.write("src/lib.rs", "pub fn inside_new() {}\n")?;
    scenario.write("docs/guide.md", "updated docs\n")?;
    scenario.commit_all("Update both files")?;

    let entries = scenario.feedback_json(&[
        "--since",
        "all",
        "--target",
        "dir:src",
        "--target",
        &format!("rev:{first_revision}"),
    ])?;

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
    let scenario = FeedbackScenario::new("feedback_since_last_same_second")?;
    scenario.write("src/lib.rs", "pub fn core() {}\n")?;
    scenario.commit_all("Initial")?;

    scenario.review_block_with_overrides(
        "src/lib.rs",
        "rejected",
        ReviewRecordOverrides {
            id: Some("first"),
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;

    let first_entries = scenario.feedback_json(&["--since", "last"])?;
    let first_reviews = first_entries[0]["reviews"]
        .as_array()
        .context("first reviews should be array")?;
    assert_eq!(first_reviews.len(), 1);
    assert_eq!(first_reviews[0]["id"].as_str(), Some("first"));

    scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        ReviewRecordOverrides {
            id: Some("second"),
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;

    let second_entries = scenario.feedback_json(&["--since", "last"])?;
    let second_reviews = second_entries[0]["reviews"]
        .as_array()
        .context("second reviews should be array")?;

    assert_eq!(second_reviews.len(), 1);
    assert_eq!(second_reviews[0]["id"].as_str(), Some("second"));

    Ok(())
}

#[test]
fn test_feedback_target_revision_range_filters_by_record_revision() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_target_revision_range_records")?;
    scenario.write("src/lib.rs", "pub fn before() {}\n")?;
    let start_revision = scenario.commit_all("A")?;

    scenario.write("src/lib.rs", "pub fn in_range() {}\n")?;
    let in_range_revision = scenario.commit_all("B")?;
    scenario.review_block_with_overrides(
        "src/lib.rs",
        "rejected",
        ReviewRecordOverrides {
            id: Some("in-range"),
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;

    scenario.write("docs/guide.md", "later docs change\n")?;
    scenario.commit_all("C")?;
    scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        ReviewRecordOverrides {
            id: Some("outside-range"),
            timestamp: Some(1001),
            ..Default::default()
        },
    )?;

    let entries = scenario.feedback_json(&[
        "--since",
        "all",
        "--target",
        &format!("rev:{start_revision}..{in_range_revision}"),
    ])?;
    let reviews = entries[0]["reviews"]
        .as_array()
        .context("reviews should be array")?;

    assert_eq!(entries.len(), 1);
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0]["id"].as_str(), Some("in-range"));

    Ok(())
}

#[test]
fn test_feedback_target_revision_range_includes_in_range_reviews_on_unchanged_files() -> Result<()>
{
    let scenario = FeedbackScenario::new("feedback_target_revision_range_record_centric")?;
    scenario.write("src/stable.rs", "pub fn stable() {}\n")?;
    scenario.write("docs/seed.md", "seed\n")?;
    let start_revision = scenario.commit_all("A")?;

    scenario.write("docs/guide.md", "first docs change\n")?;
    scenario.commit_all("B")?;
    scenario.review_block_with_overrides(
        "src/stable.rs",
        "comment",
        ReviewRecordOverrides {
            id: Some("stable-in-range"),
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;

    scenario.write("docs/guide.md", "second docs change\n")?;
    let end_revision = scenario.commit_all("C")?;

    let entries = scenario.feedback_json(&[
        "--since",
        "all",
        "--target",
        &format!("rev:{start_revision}..{end_revision}"),
    ])?;
    let reviews = entries[0]["reviews"]
        .as_array()
        .context("reviews should be array")?;

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["file"].as_str(), Some("src/stable.rs"));
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0]["id"].as_str(), Some("stable-in-range"));

    Ok(())
}

#[test]
fn test_feedback_target_revision_range_uses_record_revision_context_after_later_drift() -> Result<()>
{
    let scenario = FeedbackScenario::new("feedback_target_revision_range_context")?;
    scenario.write("src/lib.rs", "pub fn before() {}\n")?;
    let start_revision = scenario.commit_all("A")?;

    scenario.write("src/lib.rs", "pub fn in_range() {}\n")?;
    scenario.commit_all("B")?;
    scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        ReviewRecordOverrides {
            id: Some("mid-review"),
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;

    scenario.write("src/lib.rs", "pub fn after() {}\n")?;
    let end_revision = scenario.commit_all("C")?;

    let entries = scenario.feedback_json(&[
        "--since",
        "all",
        "--target",
        &format!("rev:{start_revision}..{end_revision}"),
    ])?;
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
