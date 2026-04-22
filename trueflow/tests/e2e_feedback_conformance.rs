use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use trueflow_test_support::{FeedbackScenario, ReviewRecordOverrides};

struct FeedbackCase {
    name: &'static str,
    args: Vec<String>,
    expected_review_ids: Vec<&'static str>,
    expected_files: Vec<&'static str>,
    expected_content_substring: Option<&'static str>,
    unexpected_content_substring: Option<&'static str>,
}

#[test]
fn feedback_since_conformance_cases() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_since_conformance")?;
    scenario.write("src/lib.rs", "pub fn core() {}\n")?;
    scenario.commit_all("Initial")?;
    scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        &ReviewRecordOverrides {
            id: Some("old"),
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;
    scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        &ReviewRecordOverrides {
            id: Some("boundary"),
            timestamp: Some(2000),
            ..Default::default()
        },
    )?;
    scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        &ReviewRecordOverrides {
            id: Some("newest"),
            timestamp: Some(3000),
            ..Default::default()
        },
    )?;

    let boundary_rfc3339 = DateTime::from_timestamp(2000, 0)
        .context("timestamp should be valid")?
        .to_rfc3339();
    let cases = vec![
        FeedbackCase {
            name: "all returns every review",
            args: vec!["--since".to_string(), "all".to_string()],
            expected_review_ids: vec!["old", "boundary", "newest"],
            expected_files: vec!["src/lib.rs"],
            expected_content_substring: Some("pub fn core() {}"),
            unexpected_content_substring: None,
        },
        FeedbackCase {
            name: "unix timestamp keeps the inclusive boundary",
            args: vec!["--since".to_string(), "2000".to_string()],
            expected_review_ids: vec!["boundary", "newest"],
            expected_files: vec!["src/lib.rs"],
            expected_content_substring: Some("pub fn core() {}"),
            unexpected_content_substring: None,
        },
        FeedbackCase {
            name: "rfc3339 keeps the inclusive boundary",
            args: vec!["--since".to_string(), boundary_rfc3339],
            expected_review_ids: vec!["boundary", "newest"],
            expected_files: vec!["src/lib.rs"],
            expected_content_substring: Some("pub fn core() {}"),
            unexpected_content_substring: None,
        },
    ];

    for case in cases {
        assert_feedback_case(&scenario, &case)?;
    }

    let relative_scenario = FeedbackScenario::new("feedback_since_relative_conformance")?;
    relative_scenario.write("src/lib.rs", "pub fn core() {}\n")?;
    relative_scenario.commit_all("Initial")?;
    relative_scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        &ReviewRecordOverrides {
            id: Some("older-than-window"),
            timestamp: Some((Utc::now() - Duration::hours(72)).timestamp()),
            ..Default::default()
        },
    )?;
    relative_scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        &ReviewRecordOverrides {
            id: Some("inside-window"),
            timestamp: Some((Utc::now() - Duration::hours(1)).timestamp()),
            ..Default::default()
        },
    )?;
    assert_feedback_case(
        &relative_scenario,
        &FeedbackCase {
            name: "relative duration keeps only records inside the recent window",
            args: vec!["--since".to_string(), "48h".to_string()],
            expected_review_ids: vec!["inside-window"],
            expected_files: vec!["src/lib.rs"],
            expected_content_substring: Some("pub fn core() {}"),
            unexpected_content_substring: None,
        },
    )?;

    Ok(())
}

#[test]
fn feedback_since_last_cursor_conformance_cases() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_since_last_conformance")?;
    scenario.write("src/lib.rs", "pub fn core() {}\n")?;
    scenario.commit_all("Initial")?;
    scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        &ReviewRecordOverrides {
            id: Some("first"),
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;

    let cases = [
        CursorCase {
            name: "first last export includes the initial record",
            before_export: None,
            expected_review_ids: &["first"],
        },
        CursorCase {
            name: "repeating last without new records is empty",
            before_export: None,
            expected_review_ids: &[],
        },
        CursorCase {
            name: "same-second new records are exported once",
            before_export: Some(CursorAction::AddReview {
                id: "second",
                timestamp: 1000,
                verdict: "comment",
            }),
            expected_review_ids: &["second"],
        },
        CursorCase {
            name: "repeating last after same-second export is empty",
            before_export: None,
            expected_review_ids: &[],
        },
    ];

    for case in cases {
        if let Some(CursorAction::AddReview {
            id,
            timestamp,
            verdict,
        }) = case.before_export
        {
            scenario.review_block_with_overrides(
                "src/lib.rs",
                verdict,
                &ReviewRecordOverrides {
                    id: Some(id),
                    timestamp: Some(timestamp),
                    ..Default::default()
                },
            )?;
        }

        let entries = scenario.feedback_json(&["--since", "last"])?;
        let actual_ids = review_ids(&entries)?;
        let mut expected_ids = case
            .expected_review_ids
            .iter()
            .map(|id| (*id).to_string())
            .collect::<Vec<_>>();
        expected_ids.sort();
        assert_eq!(
            actual_ids, expected_ids,
            "cursor case '{}' expected ids {:?}, got {:?}",
            case.name, case.expected_review_ids, actual_ids
        );
    }

    Ok(())
}

#[test]
fn feedback_revision_range_conformance_cases() -> Result<()> {
    let scenario = FeedbackScenario::new("feedback_revision_range_conformance")?;
    scenario.write("src/lib.rs", "pub fn before() {}\n")?;
    let start_revision = scenario.commit_all("A")?;

    scenario.write("src/lib.rs", "pub fn in_range() {}\n")?;
    let in_range_revision = scenario.commit_all("B")?;
    scenario.review_block_with_overrides(
        "src/lib.rs",
        "rejected",
        &ReviewRecordOverrides {
            id: Some("in-range"),
            timestamp: Some(1000),
            ..Default::default()
        },
    )?;

    scenario.write("docs/guide.md", "docs changed\n")?;
    let unchanged_file_revision = scenario.commit_all("C")?;
    scenario.review_block_with_overrides(
        "src/lib.rs",
        "comment",
        &ReviewRecordOverrides {
            id: Some("unchanged-file-record"),
            timestamp: Some(1001),
            ..Default::default()
        },
    )?;

    scenario.write("src/lib.rs", "pub fn after() {}\n")?;
    scenario.commit_all("D")?;

    let cases = vec![
        FeedbackCase {
            name: "range membership excludes later records outside the requested range",
            args: vec![
                "--since".to_string(),
                "all".to_string(),
                "--target".to_string(),
                format!("rev:{start_revision}..{in_range_revision}"),
            ],
            expected_review_ids: vec!["in-range"],
            expected_files: vec!["src/lib.rs"],
            expected_content_substring: Some("pub fn in_range() {}"),
            unexpected_content_substring: Some("pub fn after() {}"),
        },
        FeedbackCase {
            name: "range membership is record-centric even when the file was unchanged in-range",
            args: vec![
                "--since".to_string(),
                "all".to_string(),
                "--target".to_string(),
                format!("rev:{start_revision}..{unchanged_file_revision}"),
            ],
            expected_review_ids: vec!["in-range", "unchanged-file-record"],
            expected_files: vec!["src/lib.rs"],
            expected_content_substring: Some("pub fn in_range() {}"),
            unexpected_content_substring: Some("pub fn after() {}"),
        },
    ];

    for case in cases {
        assert_feedback_case(&scenario, &case)?;
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum CursorAction {
    AddReview {
        id: &'static str,
        timestamp: i64,
        verdict: &'static str,
    },
}

struct CursorCase {
    name: &'static str,
    before_export: Option<CursorAction>,
    expected_review_ids: &'static [&'static str],
}

fn assert_feedback_case(scenario: &FeedbackScenario, case: &FeedbackCase) -> Result<()> {
    let args = case.args.iter().map(String::as_str).collect::<Vec<_>>();
    let entries = scenario.feedback_json(&args)?;
    let actual_ids = review_ids(&entries)?;
    let actual_files = file_paths(&entries)?;
    let mut expected_ids = case
        .expected_review_ids
        .iter()
        .map(|id| (*id).to_string())
        .collect::<Vec<_>>();
    expected_ids.sort();
    let mut expected_files = case
        .expected_files
        .iter()
        .map(|file| (*file).to_string())
        .collect::<Vec<_>>();
    expected_files.sort();
    expected_files.dedup();

    assert_eq!(
        actual_ids, expected_ids,
        "feedback case '{}' expected review ids {:?}, got {:?}",
        case.name, case.expected_review_ids, actual_ids
    );
    assert_eq!(
        actual_files, expected_files,
        "feedback case '{}' expected files {:?}, got {:?}",
        case.name, case.expected_files, actual_files
    );

    if let Some(expected) = case.expected_content_substring {
        let content = block_content(&entries).context("expected block content")?;
        assert!(
            content.contains(expected),
            "feedback case '{}' expected content to contain {:?}, got {:?}",
            case.name,
            expected,
            content
        );
    }
    if let Some(unexpected) = case.unexpected_content_substring {
        let content = block_content(&entries).context("expected block content")?;
        assert!(
            !content.contains(unexpected),
            "feedback case '{}' expected content to exclude {:?}, got {:?}",
            case.name,
            unexpected,
            content
        );
    }

    Ok(())
}

fn review_ids(entries: &[Value]) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for entry in entries {
        for review in entry["reviews"]
            .as_array()
            .context("reviews should be array")?
        {
            ids.push(
                review["id"]
                    .as_str()
                    .context("review id should be string")?
                    .to_string(),
            );
        }
    }
    ids.sort();
    Ok(ids)
}

fn file_paths(entries: &[Value]) -> Result<Vec<String>> {
    let mut files = entries
        .iter()
        .map(|entry| entry["file"].as_str().context("file should be string"))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    Ok(files)
}

fn block_content(entries: &[Value]) -> Option<&str> {
    entries.first()?.get("block")?.get("content")?.as_str()
}
