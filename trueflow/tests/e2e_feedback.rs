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

fn review_with_id(
    scenario: &FeedbackScenario,
    path: &str,
    verdict: &str,
    id: &str,
    timestamp: i64,
) -> Result<()> {
    scenario.review_block_in_process_with_overrides(
        path,
        verdict,
        &ReviewRecordOverrides {
            id: Some(id),
            timestamp: Some(timestamp),
            ..Default::default()
        },
    )?;
    Ok(())
}

#[test]
fn feedback_later_approval_at_new_commit_hides_comment() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_cross_revision_approval")?;
    scenario.write("src/lib.rs", "pub fn core() {}\n")?;
    scenario.commit_all("A")?;
    review_with_id(&scenario, "src/lib.rs", "comment", "comment", 1000)?;
    scenario.write("docs/x.md", "# Unrelated\n")?;
    scenario.commit_all("B")?;
    review_with_id(&scenario, "src/lib.rs", "approved", "approval", 2000)?;

    let entries = scenario.feedback_json_in_process(&["--since", "all"])?;

    assert!(entries.is_empty(), "stale comment exported: {entries:?}");
    Ok(())
}

#[test]
fn feedback_approval_after_committing_dirty_comment_hides_it() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_dirty_comment_then_approval")?;
    scenario.write("src/lib.rs", "pub fn core() {}\n")?;
    scenario.commit_all("A")?;
    scenario.write("src/lib.rs", "pub fn core() -> u8 { 1 }\n")?;
    review_with_id(&scenario, "src/lib.rs", "comment", "comment", 1000)?;
    scenario.commit_all("B")?;
    review_with_id(&scenario, "src/lib.rs", "approved", "approval", 2000)?;

    let entries = scenario.feedback_json_in_process(&["--since", "all"])?;

    assert!(entries.is_empty(), "stale comment exported: {entries:?}");
    Ok(())
}

#[test]
fn feedback_merges_unchanged_block_reviews_across_revisions() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_cross_revision_merge")?;
    scenario.write("src/lib.rs", "pub fn core() {}\n")?;
    scenario.commit_all("A")?;
    review_with_id(&scenario, "src/lib.rs", "comment", "first", 1000)?;
    scenario.write("docs/x.md", "# Unrelated\n")?;
    scenario.commit_all("B")?;
    review_with_id(&scenario, "src/lib.rs", "rejected", "second", 2000)?;

    let entries = scenario.feedback_json_in_process(&["--since", "all"])?;

    assert_eq!(entries.len(), 1, "one block, one entry: {entries:?}");
    let ids = entries[0]["reviews"]
        .as_array()
        .context("reviews should be array")?
        .iter()
        .filter_map(|review| review["id"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["first", "second"]);
    assert_eq!(entries[0]["latest_verdict"].as_str(), Some("rejected"));
    Ok(())
}

#[test]
fn feedback_uncommitted_copy_stays_on_its_own_path() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_uncommitted_copy_path")?;
    scenario.write("src/b.rs", "pub fn shared() {}\n")?;
    scenario.commit_all("A")?;
    scenario.write("src/a.rs", "pub fn shared() {}\n")?;
    review_with_id(&scenario, "src/a.rs", "comment", "copy-comment", 1000)?;

    let entries =
        scenario.feedback_json_in_process(&["--since", "all", "--target", "file:src/a.rs"])?;

    assert_eq!(entries.len(), 1, "comment lost from its path: {entries:?}");
    assert_eq!(entries[0]["file"].as_str(), Some("src/a.rs"));
    Ok(())
}
