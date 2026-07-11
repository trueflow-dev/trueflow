use anyhow::{Context, Result};
use chrono::{Duration, Utc};

use trueflow_test_support::{FeedbackScenario, ReviewRecordOverrides};


#[test]
fn test_feedback_since_relative_duration_survives_tree_drift() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_relative_since_tree_drift")?;
    scenario.write("src/lib.rs", "pub fn original() {}\n")?;
    scenario.commit_all("Initial")?;

    let recent_timestamp = (Utc::now() - Duration::hours(1)).timestamp();
    scenario.review_block_in_process_with_overrides(
        "src/lib.rs",
        "comment",
        &ReviewRecordOverrides {
            id: Some("recent-original"),
            timestamp: Some(recent_timestamp),
            ..Default::default()
        },
    )?;

    scenario.write("src/lib.rs", "pub fn rewritten() {}\n")?;
    scenario.commit_all("Rewrite lib")?;

    let entries = scenario.feedback_json_in_process(&["--since", "48h"])?;
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

    scenario.review_block_in_process_with_overrides(
        "src/keep.rs",
        "rejected",
        &ReviewRecordOverrides {
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;
    scenario.review_block_in_process_with_overrides(
        "src/skip.rs",
        "comment",
        &ReviewRecordOverrides {
            timestamp: Some(1001),
            ..Default::default()
        },
    )?;

    let entries =
        scenario.feedback_json_in_process(&["--since", "all", "--target", "file:src/keep.rs"])?;

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["file"].as_str(), Some("src/keep.rs"));

    Ok(())
}

#[test]
fn test_feedback_target_revision_anchors_to_historical_tree() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_target_revision")?;
    scenario.write("src/lib.rs", "pub fn old() {}\n")?;
    let first_revision = scenario.commit_all("Initial")?;

    scenario.review_block_in_process_with_overrides(
        "src/lib.rs",
        "rejected",
        &ReviewRecordOverrides {
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;

    scenario.write("src/lib.rs", "pub fn new() {}\n")?;
    scenario.commit_all("Rename function")?;

    let entries = scenario.feedback_json_in_process(&[
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

    scenario.review_block_in_process_with_overrides(
        "src/lib.rs",
        "rejected",
        &ReviewRecordOverrides {
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;
    scenario.review_block_in_process_with_overrides(
        "docs/guide.md",
        "comment",
        &ReviewRecordOverrides {
            timestamp: Some(1001),
            ..Default::default()
        },
    )?;

    scenario.write("src/lib.rs", "pub fn inside_new() {}\n")?;
    scenario.write("docs/guide.md", "updated docs\n")?;
    scenario.commit_all("Update both files")?;

    let entries = scenario.feedback_json_in_process(&[
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
fn test_feedback_target_revision_range_includes_in_range_reviews_on_unchanged_files() -> Result<()>
{
    let scenario = FeedbackScenario::new("feedback_target_revision_range_record_centric")?;
    scenario.write("src/stable.rs", "pub fn stable() {}\n")?;
    scenario.write("docs/seed.md", "seed\n")?;
    let start_revision = scenario.commit_all("A")?;

    scenario.write("docs/guide.md", "first docs change\n")?;
    scenario.commit_all("B")?;
    scenario.review_block_in_process_with_overrides(
        "src/stable.rs",
        "comment",
        &ReviewRecordOverrides {
            id: Some("stable-in-range"),
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;

    scenario.write("docs/guide.md", "second docs change\n")?;
    let end_revision = scenario.commit_all("C")?;

    let entries = scenario.feedback_json_in_process(&[
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

