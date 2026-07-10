use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;
use trueflow::block::BlockKind;
use trueflow::commands::review::{ReviewRequest, ReviewTarget};
use trueflow::scanner::{
    ScanCacheReadStatus, ScanCacheWriteStatus, ScanOptions, ScanResult, scan_directory,
};

use trueflow_test_support::*;

fn review_all(repo: &TestRepo) -> Result<trueflow::commands::review::ReviewSummary> {
    repo.review_summary(ReviewRequest::AllFiles, &[], &[])
}

fn review_main(repo: &TestRepo) -> Result<trueflow::commands::review::ReviewSummary> {
    repo.review_summary(
        ReviewRequest::Targets(vec![ReviewTarget::MainDiff]),
        &[],
        &[],
    )
}

fn first_scan_block_hash_in_process(repo: &TestRepo) -> Result<String> {
    let scan = repo.scan_without_cache()?;
    let file = scan.files.first().context("Expected file in output")?;
    let block = file.blocks.first().context("Expected block in output")?;
    Ok(block.hash.to_string())
}

fn scan_with_cache_dir(repo: &TestRepo, cache_dir: &Path) -> Result<ScanResult> {
    scan_directory(
        &repo.path,
        &ScanOptions {
            cache_dir: Some(cache_dir.to_path_buf()),
            ..ScanOptions::default()
        },
    )
}

fn scan_contains_path(scan: &ScanResult, path: &str) -> bool {
    scan.files.iter().any(|file| file.path.as_str() == path)
}

#[test]
fn test_optimizer_import_merge_preserves_content() -> Result<()> {
    let repo = TestRepo::new("optimizer_import")?;
    repo.write("src/lib.rs", "use a;\n\nuse b;\nextern crate c;\n")?;
    let scan = repo.scan_without_cache()?;
    let blocks = &scan
        .files
        .first()
        .context("Expected file in output")?
        .blocks;

    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].kind, BlockKind::Imports);

    // Note: The optimizer preserves newlines between imports
    assert_eq!(blocks[0].content, "use a;\nuse b;\nextern crate c;");
    Ok(())
}

#[test]
fn test_optimizer_module_merge_preserves_content() -> Result<()> {
    let repo = TestRepo::new("optimizer_module")?;
    repo.write("src/lib.rs", "mod a;\nmod b;\n\nextern \"C\" { fn x(); }\n")?;
    let scan = repo.scan_without_cache()?;
    let blocks = &scan
        .files
        .first()
        .context("Expected file in output")?
        .blocks;

    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].kind, BlockKind::Modules);
    assert!(blocks[0].content.contains("mod a"));
    assert!(blocks[0].content.contains("extern \"C\""));
    Ok(())
}

#[test]
fn test_optimizer_module_merge_preserves_test_tags() -> Result<()> {
    let repo = TestRepo::new("optimizer_module_tags")?;
    repo.write(
        "src/lib.rs",
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn it_works() {}\n}\n\nmod helper {\n    pub fn noop() {}\n}\n",
    )?;

    let scan = repo.scan_without_cache()?;
    let blocks = &scan
        .files
        .first()
        .context("Expected file in output")?
        .blocks;

    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].kind, BlockKind::Modules);

    assert!(
        blocks[0].tags.iter().any(|tag| tag == "test"),
        "expected merged module block to retain test tag, got {:?}",
        blocks[0].tags
    );

    Ok(())
}

#[test]
fn test_optimizer_import_merge_respects_large_gap_boundary_e2e() -> Result<()> {
    let repo = TestRepo::new("optimizer_import_gap_boundary")?;
    repo.write(
        "src/lib.rs",
        "use std::fmt;\n\nuse std::io;\n\n\n\n\nuse std::fs;\n",
    )?;

    let scan = repo.scan_without_cache()?;
    let blocks = &scan
        .files
        .first()
        .context("Expected file in output")?
        .blocks;

    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].kind, BlockKind::Imports);
    assert_eq!(blocks[1].kind, BlockKind::Import);

    Ok(())
}

#[test]
fn test_optimizer_small_file_collapses_mixed_semantic_blocks_e2e() -> Result<()> {
    let repo = TestRepo::new("optimizer_small_file_collapse")?;
    repo.write(
        "src/lib.rs",
        "use std::fmt;\n\nfn run() {\n    if true {}\n}\n\nconst LIMIT: usize = 3;\n",
    )?;

    let scan = repo.scan_without_cache()?;
    let blocks = &scan
        .files
        .first()
        .context("Expected file in output")?
        .blocks;

    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].kind, BlockKind::Code);

    let complexity = blocks[0].complexity.context("complexity should be set")?;
    assert!(
        complexity >= 1,
        "expected collapsed block complexity to include function complexity, got {complexity}"
    );

    Ok(())
}

#[test]
fn test_diff_blocks_match_post_hunk_file_content() -> Result<()> {
    // GIVEN: a change that replaces a line in the working tree
    let repo = TestRepo::new("diff_new_content")?;
    let initial = include_str!("fixtures/diff_new_content_initial.rs");
    let updated = include_str!("fixtures/diff_new_content_updated.rs");
    repo.write("src/main.rs", initial)?;
    repo.commit_all("Initial")?;

    repo.git(&["checkout", "-b", "feature/update"])?;

    repo.write("src/main.rs", updated)?;
    repo.commit_all("Update message")?;

    // WHEN: we compute semantic diff output
    let summary = review_main(&repo)?;
    let blocks = &summary
        .files
        .first()
        .context("Expected file in review output")?
        .blocks;

    // THEN: semantic block content reflects the post-hunk file content
    let file_content = fs::read_to_string(repo.path.join("src/main.rs"))?;
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].content, file_content.trim_end_matches('\n'));
    Ok(())
}

#[test]
fn test_review_ignores_non_review_checks() -> Result<()> {
    let repo = TestRepo::new("review_check_filter")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    // GIVEN: a reviewable block with no review verdicts
    let hash = first_scan_block_hash_in_process(&repo)?;

    // WHEN: a non-review check is recorded for the block
    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "approved",
        "--check",
        "security",
        "--quiet",
    ])?;

    // THEN: the block is still present in review output
    let summary = review_all(&repo)?;
    assert!(!summary.files.is_empty());
    Ok(())
}

#[test]
fn test_review_latest_timestamp_wins() -> Result<()> {
    let repo = TestRepo::new("review_timestamp")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    // GIVEN: two review records for the same block with different timestamps
    let hash = first_scan_block_hash_in_process(&repo)?;

    let trueflow_dir = repo.path.join(".trueflow");
    let approved = build_review_record(
        &hash,
        ReviewRecordOverrides {
            timestamp: Some(2000),
            ..Default::default()
        },
    );
    let rejected = build_review_record(
        &hash,
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            email: Some("b@example.com"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&trueflow_dir, &[approved, rejected])?;

    // WHEN: we re-run review
    let summary = review_all(&repo)?;

    // THEN: the newer approval wins and nothing remains to review
    assert!(summary.files.is_empty());
    Ok(())
}

#[test]
fn test_feedback_latest_timestamp_wins() -> Result<()> {
    let repo = TestRepo::new("feedback_timestamp")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    let hash = first_scan_block_hash_in_process(&repo)?;

    let trueflow_dir = repo.path.join(".trueflow");
    let newer_approved = build_review_record(
        &hash,
        ReviewRecordOverrides {
            check: Some("security"),
            verdict: Some("approved"),
            timestamp: Some(2000),
            ..Default::default()
        },
    );
    let older_rejected = build_review_record(
        &hash,
        ReviewRecordOverrides {
            check: Some("security"),
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    // Intentionally write the older record last to verify timestamp, not file order, decides.
    write_reviews_jsonl(&trueflow_dir, &[newer_approved, older_rejected])?;

    let output = repo.run(&["feedback", "--format", "json", "--include-approved"])?;
    let entries = json_array(&output)?;
    let entry = entries.first().context("expected feedback entry")?;
    assert_eq!(
        entry["latest_verdict"].as_str().context("latest_verdict")?,
        "approved"
    );

    Ok(())
}

#[test]
fn test_feedback_since_unix_timestamp_filters_history() -> Result<()> {
    let repo = TestRepo::new("feedback_since_timestamp")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    let hash = first_scan_block_hash_in_process(&repo)?;

    let trueflow_dir = repo.path.join(".trueflow");
    let old_review = build_review_record(
        &hash,
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    let new_review = build_review_record(
        &hash,
        ReviewRecordOverrides {
            verdict: Some("comment"),
            timestamp: Some(2000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&trueflow_dir, &[old_review, new_review])?;

    let output = repo.run(&["feedback", "--format", "json", "--since", "1500"])?;
    let entries = json_array(&output)?;
    let entry = entries.first().context("expected feedback entry")?;
    let reviews = entry["reviews"]
        .as_array()
        .context("reviews should be array")?;
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0]["timestamp"].as_i64(), Some(2000));

    Ok(())
}

#[test]
fn test_feedback_since_last_uses_cursor_file() -> Result<()> {
    let repo = TestRepo::new("feedback_since_last_cursor")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    let hash = first_scan_block_hash_in_process(&repo)?;

    let trueflow_dir = repo.path.join(".trueflow");
    let first_review = build_review_record(
        &hash,
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&trueflow_dir, std::slice::from_ref(&first_review))?;

    let first_output = repo.run(&["feedback", "--format", "json", "--since", "last"])?;
    let first_entries = json_array(&first_output)?;
    assert_eq!(first_entries.len(), 1);

    let second_review = build_review_record(
        &hash,
        ReviewRecordOverrides {
            verdict: Some("comment"),
            timestamp: Some(2000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&trueflow_dir, &[first_review, second_review])?;

    let second_output = repo.run(&["feedback", "--format", "json", "--since", "last"])?;
    let second_entries = json_array(&second_output)?;
    let second_entry = second_entries
        .first()
        .context("expected second feedback entry")?;
    let second_reviews = second_entry["reviews"]
        .as_array()
        .context("reviews should be array")?;
    assert_eq!(second_reviews.len(), 1);
    assert_eq!(second_reviews[0]["timestamp"].as_i64(), Some(2000));

    Ok(())
}

#[test]
fn test_feedback_since_last_cursor_ignores_filtered_out_records() -> Result<()> {
    let repo = TestRepo::new("feedback_since_last_filtered_cursor")?;
    repo.write("src/a.rs", "pub fn a() {}\n")?;
    repo.write("src/b.rs", "pub fn b() {}\n")?;
    repo.commit_all("Add libs")?;

    let scan = repo.scan_without_cache()?;
    let block_for = |path: &str| -> Result<&trueflow::block::Block> {
        scan.files
            .iter()
            .find(|file| file.path.as_str() == path)
            .and_then(|file| file.blocks.first())
            .with_context(|| format!("expected block for {path}"))
    };
    let a_block = block_for("src/a.rs")?;
    let b_block = block_for("src/b.rs")?;

    let a_review = build_review_record(
        a_block.hash.as_str(),
        ReviewRecordOverrides {
            id: Some("a-review"),
            verdict: Some("comment"),
            timestamp: Some(1000),
            path_hint: Some("src/a.rs"),
            line_hint: Some(u32::try_from(a_block.start_line)?),
            ..Default::default()
        },
    );
    let b_review = build_review_record(
        b_block.hash.as_str(),
        ReviewRecordOverrides {
            id: Some("b-review"),
            verdict: Some("comment"),
            timestamp: Some(2000),
            path_hint: Some("src/b.rs"),
            line_hint: Some(u32::try_from(b_block.start_line)?),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&repo.path.join(".trueflow"), &[a_review, b_review])?;

    let first_output = repo.run(&[
        "feedback",
        "--format",
        "json",
        "--since",
        "last",
        "--target",
        "file:src/a.rs",
    ])?;
    let first_entries = json_array(&first_output)?;
    assert_eq!(first_entries.len(), 1);
    assert_eq!(first_entries[0]["file"].as_str(), Some("src/a.rs"));

    let second_output = repo.run(&["feedback", "--format", "json", "--since", "last"])?;
    let second_entries = json_array(&second_output)?;
    assert_eq!(second_entries.len(), 1);
    assert_eq!(second_entries[0]["file"].as_str(), Some("src/b.rs"));

    Ok(())
}

#[test]
fn test_feedback_uses_config_default_since_when_omitted() -> Result<()> {
    let repo = TestRepo::new("feedback_default_since_config")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.write("trueflow.toml", "[feedback]\ndefault_since = \"last\"\n")?;
    repo.commit_all("Add lib and config")?;

    let hash = first_scan_block_hash_in_process(&repo)?;

    let trueflow_dir = repo.path.join(".trueflow");
    let first_review = build_review_record(
        &hash,
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&trueflow_dir, std::slice::from_ref(&first_review))?;

    let first_output = repo.run(&["feedback", "--format", "json"])?;
    let first_entries = json_array(&first_output)?;
    assert_eq!(first_entries.len(), 1);

    let second_review = build_review_record(
        &hash,
        ReviewRecordOverrides {
            verdict: Some("comment"),
            timestamp: Some(2000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&trueflow_dir, &[first_review, second_review])?;

    let second_output = repo.run(&["feedback", "--format", "json"])?;
    let second_entries = json_array(&second_output)?;
    let second_entry = second_entries
        .first()
        .context("expected second feedback entry")?;
    let second_reviews = second_entry["reviews"]
        .as_array()
        .context("reviews should be array")?;
    assert_eq!(second_reviews.len(), 1);
    assert_eq!(second_reviews[0]["timestamp"].as_i64(), Some(2000));

    Ok(())
}

#[test]
fn test_review_revision_target_from_subdir() -> Result<()> {
    let repo = TestRepo::new("review_revision_subdir")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Initial")?;

    // GIVEN: a revision that changes a file under src/
    repo.git(&["checkout", "-b", "feature/rev"])?;
    repo.write("src/lib.rs", "pub fn core() {}\npub fn helper() {}\n")?;
    repo.commit_all("Add helper")?;

    let head = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let revision = head.trim();
    let subdir = repo.path.join("src");

    // WHEN: we request review from a subdirectory scoped to that revision
    let output = repo.run_in(
        &["review", "--json", "--target", &format!("rev:{revision}")],
        &subdir,
    )?;
    let files = json_array(&output)?;

    // THEN: we still see reviewable output
    assert!(!files.is_empty());

    Ok(())
}

#[test]
fn test_review_revision_target_includes_only_changed_blocks() -> Result<()> {
    let repo = TestRepo::new("review_revision_changed_blocks_only")?;
    repo.write(
        "src/lib.rs",
        "pub fn changed() {\n    println!(\"before\");\n}\n\npub fn untouched() {\n    println!(\"stable untouched marker\");\n}\n",
    )?;
    repo.commit_all("Initial")?;

    repo.git(&["checkout", "-b", "feature/rev-filter"])?;
    repo.write(
        "src/lib.rs",
        "pub fn changed() {\n    println!(\"after\");\n}\n\npub fn untouched() {\n    println!(\"stable untouched marker\");\n}\n",
    )?;
    repo.commit_all("Change one block")?;

    let head = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let revision = head.trim();
    let output = repo.run(&["review", "--json", "--target", &format!("rev:{revision}")])?;
    let files = json_array(&output)?;
    let blocks = files.first().context("Expected file in review output")?["blocks"]
        .as_array()
        .context("blocks")?;

    assert!(
        !blocks.is_empty(),
        "expected changed blocks in revision output"
    );
    for block in blocks {
        let content = block["content"].as_str().context("content")?;
        assert!(
            !content.contains("stable untouched marker"),
            "revision-scoped review included an unchanged block: {content}"
        );
    }

    Ok(())
}

#[test]
fn test_review_historical_revision_target_uses_target_revision_content() -> Result<()> {
    let repo = TestRepo::new("review_historical_revision")?;
    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"before\");\n}\n",
    )?;
    repo.commit_all("Initial")?;

    repo.git(&["checkout", "-b", "feature/history-rev"])?;
    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"middle target marker\");\n}\n",
    )?;
    repo.commit_all("Target revision")?;

    let target_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let target_revision = target_revision.trim().to_string();

    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"current head marker\");\n}\n",
    )?;
    repo.commit_all("Later revision")?;

    let output = repo.run(&[
        "review",
        "--json",
        "--target",
        &format!("rev:{target_revision}"),
    ])?;
    let files = json_array(&output)?;
    let blocks = files.first().context("Expected file in review output")?["blocks"]
        .as_array()
        .context("blocks")?;

    assert!(
        !blocks.is_empty(),
        "expected changed blocks for historical revision target"
    );

    let contents = blocks
        .iter()
        .filter_map(|block| block["content"].as_str())
        .collect::<Vec<_>>();

    assert!(
        contents
            .iter()
            .any(|content| content.contains("middle target marker")),
        "historical revision review did not use target commit content: {contents:?}"
    );
    assert!(
        contents
            .iter()
            .all(|content| !content.contains("current head marker")),
        "historical revision review leaked current checkout content: {contents:?}"
    );

    Ok(())
}

#[test]
fn test_review_historical_revision_range_uses_end_revision_content() -> Result<()> {
    let repo = TestRepo::new("review_historical_revision_range")?;
    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"before\");\n}\n",
    )?;
    repo.commit_all("Initial")?;

    let start_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let start_revision = start_revision.trim().to_string();

    repo.git(&["checkout", "-b", "feature/history-range"])?;
    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"range end marker\");\n}\n",
    )?;
    repo.commit_all("Range end")?;

    let end_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let end_revision = end_revision.trim().to_string();

    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"later head marker\");\n}\n",
    )?;
    repo.commit_all("Later revision")?;

    let output = repo.run(&[
        "review",
        "--json",
        "--target",
        &format!("rev:{start_revision}..{end_revision}"),
    ])?;
    let files = json_array(&output)?;
    let blocks = files.first().context("Expected file in review output")?["blocks"]
        .as_array()
        .context("blocks")?;

    assert!(
        !blocks.is_empty(),
        "expected changed blocks for historical revision range"
    );

    let contents = blocks
        .iter()
        .filter_map(|block| block["content"].as_str())
        .collect::<Vec<_>>();

    assert!(
        contents
            .iter()
            .any(|content| content.contains("range end marker")),
        "historical revision range review did not use end revision content: {contents:?}"
    );
    assert!(
        contents
            .iter()
            .all(|content| !content.contains("later head marker")),
        "historical revision range review leaked current checkout content: {contents:?}"
    );

    Ok(())
}

#[test]
fn test_review_historical_revision_target_from_subdir_uses_target_content() -> Result<()> {
    let repo = TestRepo::new("review_historical_revision_subdir")?;
    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"before\");\n}\n",
    )?;
    repo.commit_all("Initial")?;

    repo.git(&["checkout", "-b", "feature/history-subdir"])?;
    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"subdir target marker\");\n}\n",
    )?;
    repo.commit_all("Target revision")?;

    let target_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let target_revision = target_revision.trim().to_string();
    let subdir = repo.path.join("src");

    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"later subdir marker\");\n}\n",
    )?;
    repo.commit_all("Later revision")?;

    let output = repo.run_in(
        &[
            "review",
            "--json",
            "--target",
            &format!("rev:{target_revision}"),
        ],
        &subdir,
    )?;
    let files = json_array(&output)?;
    let blocks = files.first().context("Expected file in review output")?["blocks"]
        .as_array()
        .context("blocks")?;

    assert!(
        !blocks.is_empty(),
        "expected changed blocks for historical revision from subdir"
    );

    let contents = blocks
        .iter()
        .filter_map(|block| block["content"].as_str())
        .collect::<Vec<_>>();

    assert!(
        contents
            .iter()
            .any(|content| content.contains("subdir target marker")),
        "historical revision review from subdir did not use target content: {contents:?}"
    );
    assert!(
        contents
            .iter()
            .all(|content| !content.contains("later subdir marker")),
        "historical revision review from subdir leaked current checkout content: {contents:?}"
    );

    Ok(())
}

#[test]
fn test_review_historical_revision_range_from_subdir_uses_end_revision_content() -> Result<()> {
    let repo = TestRepo::new("review_historical_revision_range_subdir")?;
    repo.write(
        "src/lib.rs",
        r#"pub fn tracked() {
    println!("before");
}
"#,
    )?;
    repo.commit_all("Initial")?;

    let start_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let start_revision = start_revision.trim().to_string();

    repo.git(&["checkout", "-b", "feature/history-range-subdir"])?;
    repo.write(
        "src/lib.rs",
        r#"pub fn tracked() {
    println!("range subdir marker");
}
"#,
    )?;
    repo.commit_all("Range end")?;

    let end_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let end_revision = end_revision.trim().to_string();
    let subdir = repo.path.join("src");

    repo.write(
        "src/lib.rs",
        r#"pub fn tracked() {
    println!("later subdir head marker");
}
"#,
    )?;
    repo.commit_all("Later revision")?;

    let output = repo.run_in(
        &[
            "review",
            "--json",
            "--target",
            &format!("rev:{start_revision}..{end_revision}"),
        ],
        &subdir,
    )?;
    let files = json_array(&output)?;
    let blocks = files.first().context("Expected file in review output")?["blocks"]
        .as_array()
        .context("blocks")?;

    assert!(
        !blocks.is_empty(),
        "expected changed blocks for historical revision range from subdir"
    );

    let contents = blocks
        .iter()
        .filter_map(|block| block["content"].as_str())
        .collect::<Vec<_>>();

    assert!(
        contents
            .iter()
            .any(|content| content.contains("range subdir marker")),
        "historical revision range review from subdir did not use end revision content: {contents:?}"
    );
    assert!(
        contents
            .iter()
            .all(|content| !content.contains("later subdir head marker")),
        "historical revision range review from subdir leaked current checkout content: {contents:?}"
    );

    Ok(())
}

#[test]
fn test_review_historical_revision_dir_target_from_subdir_filters_subtree() -> Result<()> {
    let repo = TestRepo::new("review_historical_revision_dir_subdir")?;
    repo.write(
        "src/nested/keep.rs",
        "pub fn keep() {\n    println!(\"before keep\");\n}\n",
    )?;
    repo.write(
        "src/skip.rs",
        "pub fn skip() {\n    println!(\"before skip\");\n}\n",
    )?;
    repo.commit_all("Initial")?;

    repo.git(&["checkout", "-b", "feature/history-dir-subdir"])?;
    repo.write(
        "src/nested/keep.rs",
        "pub fn keep() {\n    println!(\"target keep marker\");\n}\n",
    )?;
    repo.write(
        "src/skip.rs",
        "pub fn skip() {\n    println!(\"target skip marker\");\n}\n",
    )?;
    repo.commit_all("Target revision")?;

    let target_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let target_revision = target_revision.trim().to_string();
    let subdir = repo.path.join("src");

    repo.write(
        "src/nested/keep.rs",
        "pub fn keep() {\n    println!(\"later keep marker\");\n}\n",
    )?;
    repo.write(
        "src/skip.rs",
        "pub fn skip() {\n    println!(\"later skip marker\");\n}\n",
    )?;
    repo.commit_all("Later revision")?;

    let output = repo.run_in(
        &[
            "review",
            "--json",
            "--target",
            "dir:src/nested",
            "--target",
            &format!("rev:{target_revision}"),
        ],
        &subdir,
    )?;
    let files = json_array(&output)?;

    assert_eq!(
        files.len(),
        1,
        "expected only the nested subtree file: {files:?}"
    );
    let blocks = files[0]["blocks"].as_array().context("blocks")?;
    let contents = blocks
        .iter()
        .filter_map(|block| block["content"].as_str())
        .collect::<Vec<_>>();

    assert!(
        contents
            .iter()
            .any(|content| content.contains("target keep marker")),
        "historical revision dir review did not use target subtree content: {contents:?}"
    );
    assert!(
        contents
            .iter()
            .all(|content| !content.contains("target skip marker")),
        "historical revision dir review leaked content outside the subtree: {contents:?}"
    );
    assert!(
        contents
            .iter()
            .all(|content| !content.contains("later keep marker")
                && !content.contains("later skip marker")),
        "historical revision dir review leaked later HEAD content: {contents:?}"
    );

    Ok(())
}

#[test]
fn test_review_historical_revision_range_dir_target_from_subdir_filters_subtree() -> Result<()> {
    let repo = TestRepo::new("review_historical_revision_range_dir_subdir")?;
    repo.write(
        "src/nested/keep.rs",
        "pub fn keep() {\n    println!(\"before keep\");\n}\n",
    )?;
    repo.write(
        "src/skip.rs",
        "pub fn skip() {\n    println!(\"before skip\");\n}\n",
    )?;
    repo.commit_all("Initial")?;
    let start_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let start_revision = start_revision.trim().to_string();

    repo.git(&["checkout", "-b", "feature/history-range-dir-subdir"])?;
    repo.write(
        "src/nested/keep.rs",
        "pub fn keep() {\n    println!(\"range keep marker\");\n}\n",
    )?;
    repo.write(
        "src/skip.rs",
        "pub fn skip() {\n    println!(\"range skip marker\");\n}\n",
    )?;
    repo.commit_all("Range end")?;

    let end_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let end_revision = end_revision.trim().to_string();
    let subdir = repo.path.join("src");

    repo.write(
        "src/nested/keep.rs",
        "pub fn keep() {\n    println!(\"later keep marker\");\n}\n",
    )?;
    repo.write(
        "src/skip.rs",
        "pub fn skip() {\n    println!(\"later skip marker\");\n}\n",
    )?;
    repo.commit_all("Later revision")?;

    let output = repo.run_in(
        &[
            "review",
            "--json",
            "--target",
            "dir:src/nested",
            "--target",
            &format!("rev:{start_revision}..{end_revision}"),
        ],
        &subdir,
    )?;
    let files = json_array(&output)?;

    assert_eq!(
        files.len(),
        1,
        "expected only the nested subtree file: {files:?}"
    );
    let blocks = files[0]["blocks"].as_array().context("blocks")?;
    let contents = blocks
        .iter()
        .filter_map(|block| block["content"].as_str())
        .collect::<Vec<_>>();

    assert!(
        contents
            .iter()
            .any(|content| content.contains("range keep marker")),
        "historical range dir review did not use end revision subtree content: {contents:?}"
    );
    assert!(
        contents
            .iter()
            .all(|content| !content.contains("range skip marker")),
        "historical range dir review leaked content outside the subtree: {contents:?}"
    );
    assert!(
        contents
            .iter()
            .all(|content| !content.contains("later keep marker")
                && !content.contains("later skip marker")),
        "historical range dir review leaked later HEAD content: {contents:?}"
    );

    Ok(())
}

#[test]
fn test_review_historical_deletion_target_and_range_preserve_deleted_base_content() -> Result<()> {
    // GIVEN: a historical target deletes a file and later HEAD reintroduces that path with different content
    let repo = TestRepo::new("review_historical_deleted_content")?;
    repo.write(
        "src/history.rs",
        "pub fn removed_in_target() {\n    println!(\"deleted base marker\");\n}\n",
    )?;
    repo.commit_all("Initial")?;
    let start_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let start_revision = start_revision.trim().to_string();

    repo.git(&["checkout", "-b", "feature/history-delete"])?;
    fs::remove_file(repo.path.join("src/history.rs"))?;
    repo.commit_all("Delete historical file")?;
    let delete_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let delete_revision = delete_revision.trim().to_string();

    repo.write(
        "src/history.rs",
        "pub fn later_head_version() {\n    println!(\"later head marker\");\n}\n",
    )?;
    repo.commit_all("Re-add different file")?;

    // WHEN: we review the deletion commit directly and as a revision range endpoint
    let target_output = repo.run(&[
        "review",
        "--json",
        "--target",
        &format!("rev:{delete_revision}"),
    ])?;
    let range_output = repo.run(&[
        "review",
        "--json",
        "--target",
        &format!("rev:{start_revision}..{delete_revision}"),
    ])?;
    let target_files = json_array(&target_output)?;
    let range_files = json_array(&range_output)?;

    // THEN: both historical views show the deleted base content and never leak the later HEAD content
    for files in [&target_files, &range_files] {
        let file = files
            .iter()
            .find(|entry| entry["path"].as_str() == Some("src/history.rs"))
            .context("expected deleted historical file in review output")?;
        let contents = file["blocks"]
            .as_array()
            .context("blocks")?
            .iter()
            .filter_map(|block| block["content"].as_str())
            .collect::<Vec<_>>();

        assert!(
            contents
                .iter()
                .any(|content| content.contains("removed_in_target")
                    && content.contains("deleted base marker")),
            "historical deletion review did not preserve deleted base content: {contents:?}"
        );
        assert!(
            contents
                .iter()
                .all(|content| !content.contains("later head marker")),
            "historical deletion review leaked later HEAD content: {contents:?}"
        );
    }

    Ok(())
}

#[test]
fn test_review_rejects_mixed_historical_targets_with_different_content_revisions() -> Result<()> {
    let repo = TestRepo::new("review_mixed_historical_targets")?;
    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"before\");\n}\n",
    )?;
    repo.commit_all("Initial")?;

    repo.git(&["checkout", "-b", "feature/mixed-history"])?;
    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"target one\");\n}\n",
    )?;
    repo.commit_all("Target one")?;
    let revision_one = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let revision_one = revision_one.trim().to_string();

    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"target two\");\n}\n",
    )?;
    repo.commit_all("Target two")?;
    let revision_two = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let revision_two = revision_two.trim().to_string();

    let err = repo.run_err(&[
        "review",
        "--json",
        "--target",
        &format!("rev:{revision_one}"),
        "--target",
        &format!("rev:{revision_two}"),
    ])?;

    assert!(
        err.contains(
            "Multiple historical targets with different content revisions are not supported"
        ),
        "expected mixed historical target error, got: {err}"
    );

    Ok(())
}

#[test]
fn test_review_rejects_all_with_explicit_targets() -> Result<()> {
    let repo = TestRepo::new("review_mixed_historical_and_worktree_targets")?;
    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"before\");\n}\n",
    )?;
    repo.commit_all("Initial")?;

    repo.git(&["checkout", "-b", "feature/mixed-history-worktree"])?;
    repo.write(
        "src/lib.rs",
        "pub fn tracked() {\n    println!(\"target revision\");\n}\n",
    )?;
    repo.commit_all("Target revision")?;
    let target_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;
    let target_revision = target_revision.trim().to_string();

    let err = repo.run_err(&[
        "review",
        "--json",
        "--all",
        "--target",
        &format!("rev:{target_revision}"),
    ])?;

    assert!(
        err.contains("Explicit review targets cannot be combined with --all"),
        "expected all-plus-target error, got: {err}"
    );

    Ok(())
}

#[test]
fn test_review_progress_counts_duplicate_blocks() -> Result<()> {
    let repo = TestRepo::new("review_duplicates")?;
    // Two identical functions
    let content =
        "fn duplicate() { println!(\"hello\"); }\n\nfn duplicate() { println!(\"hello\"); }\n";
    repo.write("src/lib.rs", content)?;
    repo.commit_all("Add duplicates")?;

    let summary = review_all(&repo)?;
    let blocks = &summary
        .files
        .first()
        .context("Expected file in review output")?
        .blocks;

    // Should have 2 blocks
    assert_eq!(blocks.len(), 2);

    Ok(())
}

#[test]
fn test_review_uses_precise_block_approval_for_duplicate_hashes() -> Result<()> {
    let repo = TestRepo::new("review_duplicate_precise_approval")?;
    repo.write(
        "src/lib.rs",
        "fn duplicate() { println!(\"hello\"); }\n\nfn duplicate() { println!(\"hello\"); }\n",
    )?;
    repo.commit_all("Add duplicates")?;

    let summary = review_all(&repo)?;
    let blocks = &summary
        .files
        .first()
        .context("Expected file in review output")?
        .blocks;
    assert_eq!(blocks.len(), 2, "expected duplicate blocks before approval");

    let duplicate_hash = blocks[0].hash.to_string();
    let first_start_line = blocks[0].start_line;
    let second_start_line = blocks[1].start_line;
    assert_eq!(blocks[1].hash.to_string(), duplicate_hash);
    assert_ne!(first_start_line, second_start_line);

    repo.run(&[
        "mark",
        "--fingerprint",
        &duplicate_hash,
        "--verdict",
        "approved",
        "--path",
        "src/lib.rs",
        "--line",
        &first_start_line.to_string(),
        "--quiet",
    ])?;

    let summary = review_all(&repo)?;
    let remaining_blocks = &summary
        .files
        .first()
        .context("Expected remaining file in review output")?
        .blocks;

    assert_eq!(
        remaining_blocks.len(),
        1,
        "expected one duplicate block to remain after exact approval"
    );

    assert_eq!(remaining_blocks[0].start_line, second_start_line);

    Ok(())
}

#[test]
fn test_block_rejection_overrides_file_approval_in_review() -> Result<()> {
    let repo = TestRepo::new("review_block_rejection_overrides_file_approval")?;
    repo.write(
        "src/lib.rs",
        "fn needs_review() { println!(\"check me\"); }\n",
    )?;
    repo.commit_all("Add review target")?;

    let scan = repo.scan_without_cache()?;
    let file = scan
        .files
        .iter()
        .find(|file| file.path.as_str() == "src/lib.rs")
        .context("expected src/lib.rs in scan output")?;
    let block = file.blocks.first().context("expected review block")?;

    let approved_file = build_review_record(
        file.tree_hash.as_str(),
        ReviewRecordOverrides {
            id: Some("approved-file"),
            target_kind: Some("file"),
            verdict: Some("approved"),
            timestamp: Some(1),
            path_hint: Some("src/lib.rs"),
            ..Default::default()
        },
    );
    let rejected_block = build_review_record(
        block.hash.as_str(),
        ReviewRecordOverrides {
            id: Some("rejected-block"),
            target_kind: Some("block"),
            verdict: Some("rejected"),
            timestamp: Some(2),
            path_hint: Some("src/lib.rs"),
            line_hint: Some(u32::try_from(block.start_line).context("block line fits u32")?),
            ..Default::default()
        },
    );
    write_reviews_jsonl(
        &repo.path.join(".trueflow"),
        &[approved_file, rejected_block],
    )?;

    let summary = review_all(&repo)?;
    let blocks = summary
        .files
        .iter()
        .find(|file| file.path.as_str() == "src/lib.rs")
        .map(|file| file.blocks.as_slice())
        .unwrap_or(&[]);

    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].hash, block.hash);
    Ok(())
}

#[test]
fn test_exclude_gap_case_insensitive_for_subblocks() -> Result<()> {
    let repo = TestRepo::new("exclude_gap_case")?;
    repo.write(
        "src/main.rs",
        "fn main() {\n    part1();\n\n    part2();\n}\n",
    )?;
    repo.commit_all("Add main")?;

    let scan = repo.scan_without_cache()?;
    let file = scan.files.first().context("Expected file in output")?;
    let block = file.blocks.first().context("Expected block in output")?;
    let records = trueflow::sub_splitter::split(block, file.language)?
        .into_iter()
        .filter(|sub_block| sub_block.kind != BlockKind::Gap)
        .map(|sub_block| {
            build_review_record(sub_block.hash.as_str(), ReviewRecordOverrides::default())
        })
        .collect::<Vec<_>>();
    write_reviews_jsonl(&repo.path.join(".trueflow"), &records)?;

    let summary = repo.review_summary(ReviewRequest::AllFiles, &[], &[BlockKind::Gap])?;
    assert!(summary.files.is_empty());
    Ok(())
}

#[test]
fn test_scan_skips_unreadable_entries() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new("scan_unreadable")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Add main")?;

    let secret_dir = repo.path.join("secret");
    fs::create_dir_all(&secret_dir)?;
    fs::write(secret_dir.join("hidden.txt"), "nope")?;

    let mut perms = fs::metadata(&secret_dir)?.permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&secret_dir, perms)?;

    let scan_result = repo.scan_without_cache();

    // Restore permissions so cleanup can remove the directory
    let mut perms = fs::metadata(&secret_dir)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&secret_dir, perms)?;

    let scan = scan_result?;
    assert!(
        scan.files
            .iter()
            .any(|file| file.path.as_str().contains("src/main.rs"))
    );
    Ok(())
}

#[test]
fn test_scan_cache_write_permission_error_is_non_fatal() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new("scan_cache_write_perm")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Add main")?;

    let cache_dir = temp_test_dir("scan_cache_write_perm_store");
    fs::create_dir_all(&cache_dir)?;
    let mut perms = fs::metadata(&cache_dir)?.permissions();
    perms.set_mode(0o500);
    fs::set_permissions(&cache_dir, perms)?;

    let scan_result = scan_with_cache_dir(&repo, &cache_dir);

    let mut reset = fs::metadata(&cache_dir)?.permissions();
    reset.set_mode(0o755);
    fs::set_permissions(&cache_dir, reset)?;

    let scan = scan_result?;
    assert!(scan_contains_path(&scan, "src/main.rs"));
    assert_eq!(scan.cache.write, ScanCacheWriteStatus::Error);
    assert!(
        scan.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.reason.contains("failed to write scan cache")),
        "expected scan cache write diagnostic, got {:?}",
        scan.diagnostics
    );
    Ok(())
}

#[test]
fn test_scan_cache_detects_new_untracked_files() -> Result<()> {
    let repo = TestRepo::new("scan_cache_new_untracked")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Add main")?;

    let cache_dir = temp_test_dir("scan_cache_new_untracked_store");

    let initial = scan_with_cache_dir(&repo, &cache_dir)?;
    assert!(scan_contains_path(&initial, "src/main.rs"));

    repo.write("src/new_file.rs", "pub fn new_file() {}\n")?;

    let rescanned = scan_with_cache_dir(&repo, &cache_dir)?;
    assert!(scan_contains_path(&rescanned, "src/new_file.rs"));

    Ok(())
}

#[test]
fn test_scan_cache_reuses_unchanged_files_when_one_file_changes() -> Result<()> {
    let repo = TestRepo::new("scan_cache_incremental_reuse")?;
    repo.write("src/a.rs", "pub fn a() -> u32 { 1 }\n")?;
    repo.write("src/b.rs", "pub fn b() -> u32 { 2 }\n")?;
    repo.commit_all("Add files")?;

    let cache_dir = temp_test_dir("scan_cache_incremental_reuse_store");

    let initial = scan_with_cache_dir(&repo, &cache_dir)?;
    assert_eq!(initial.cache.read, ScanCacheReadStatus::Miss);
    assert_eq!(initial.cache.reused_files, 0);
    assert_eq!(initial.cache.rescanned_files, 2);

    repo.write("src/b.rs", "pub fn b() -> u32 { 22 }\n")?;

    let rescanned = scan_with_cache_dir(&repo, &cache_dir)?;
    assert_eq!(rescanned.cache.read, ScanCacheReadStatus::Hit);
    assert_eq!(rescanned.cache.reused_files, 1);
    assert_eq!(rescanned.cache.rescanned_files, 1);

    Ok(())
}

#[test]
fn test_scan_cache_reuses_invalid_utf8_diagnostic_for_unchanged_file() -> Result<()> {
    let repo = TestRepo::new("scan_cache_invalid_utf8_reuse")?;
    fs::write(repo.path.join("bad.txt"), [0xFF, 0xFE, 0xFD])?;

    let cache_dir = temp_test_dir("scan_cache_invalid_utf8_reuse_store");

    let initial = scan_with_cache_dir(&repo, &cache_dir)?;
    assert!(initial.diagnostics.iter().any(|diagnostic| {
        diagnostic
            .path
            .as_ref()
            .is_some_and(|path| path.as_str() == "bad.txt")
            && diagnostic.reason.contains("invalid UTF-8")
    }));
    assert_eq!(initial.cache.rescanned_files, 1);

    let reused = scan_with_cache_dir(&repo, &cache_dir)?;
    assert!(reused.diagnostics.iter().any(|diagnostic| {
        diagnostic
            .path
            .as_ref()
            .is_some_and(|path| path.as_str() == "bad.txt")
            && diagnostic.reason.contains("invalid UTF-8")
    }));
    assert_eq!(reused.cache.read, ScanCacheReadStatus::Hit);
    assert_eq!(reused.cache.reused_files, 1);
    assert_eq!(reused.cache.rescanned_files, 0);

    Ok(())
}

#[test]
fn test_scan_ignores_mutants_out_directory() -> Result<()> {
    let repo = TestRepo::new("scan_ignores_mutants_out")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Add main")?;

    repo.write(
        "mutants.out/log/baseline.log",
        "generated logs should be ignored\n",
    )?;
    repo.write("mutants.out/mutants.json", "{}\n")?;

    let scan = repo.scan_without_cache()?;

    assert!(
        scan.files
            .iter()
            .all(|file| !file.path.as_str().contains("mutants.out/"))
    );
    assert!(
        scan.files
            .iter()
            .any(|file| file.path.as_str().contains("src/main.rs"))
    );
    Ok(())
}

#[test]
fn test_scan_honors_gitignore_and_keeps_nonignored_dotfiles() -> Result<()> {
    let repo = TestRepo::new("scan_gitignore_and_dotfiles")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.write(".gitignore", "ignored.txt\n")?;
    repo.write("ignored.txt", "this should be ignored by scanner\n")?;
    repo.write(".envrc", "export DEV_MODE=1\n")?;
    repo.commit_all("Add scan fixtures")?;

    let scan = repo.scan_without_cache()?;

    assert!(
        scan.files
            .iter()
            .any(|file| file.path.as_str().contains(".envrc"))
    );
    assert!(
        scan.files
            .iter()
            .all(|file| !file.path.as_str().contains("ignored.txt"))
    );

    Ok(())
}

#[test]
fn test_scan_sorts_files_by_repo_path() -> Result<()> {
    let repo = TestRepo::new("scan_sorted_paths")?;
    repo.write("src/z.rs", "fn z() {}\n")?;
    repo.write("src/a.rs", "fn a() {}\n")?;
    repo.write("src/m.rs", "fn m() {}\n")?;

    let scan = repo.scan_without_cache()?;
    let paths: Vec<&str> = scan.files.iter().map(|file| file.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);

    Ok(())
}

#[test]
fn test_scan_config_ignores_path_prefixes() -> Result<()> {
    let repo = TestRepo::new("scan_config_ignore_prefix")?;
    repo.write(
        "trueflow.toml",
        "[scan]\nignore_path_prefixes = [\"vendor\"]\n",
    )?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.write("vendor/lib.rs", "pub fn vendored() {}\n")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    assert!(
        files
            .iter()
            .any(|entry| entry["path"].as_str() == Some("src/main.rs"))
    );
    assert!(files.iter().all(|entry| {
        entry["path"].as_str().unwrap_or_default().split('/').next() != Some("vendor")
    }));

    Ok(())
}

#[test]
fn test_feedback_uses_precise_block_lookup_for_coverage() -> Result<()> {
    let repo = TestRepo::new("feedback_precise_lookup")?;
    repo.write(
        "src/lib.rs",
        "    fn dup() {\n        println!(\"same\");\n    }\n\nstruct Foo;\n\nimpl Foo {\n    fn dup() {\n        println!(\"same\");\n    }\n}\n",
    )?;
    repo.commit_all("Add duplicate hash blocks")?;

    let scan = repo.scan_without_cache()?;
    let file = scan
        .files
        .iter()
        .find(|entry| entry.path.as_str() == "src/lib.rs")
        .context("expected src/lib.rs in scan output")?;
    let blocks = &file.blocks;

    let impl_hash = blocks
        .iter()
        .find(|block| block.kind == BlockKind::Impl)
        .map(|block| block.hash.to_string())
        .context("expected impl block hash")?;

    let duplicate_hash = blocks
        .iter()
        .find(|block| block.kind == BlockKind::Function)
        .map(|block| block.hash.to_string())
        .context("expected function block hash")?;

    let function_start_line = blocks
        .iter()
        .find(|block| block.kind == BlockKind::Function && block.hash.to_string() == duplicate_hash)
        .map(|block| block.start_line as u64)
        .context("expected function start line")?;

    let method_start_line = blocks
        .iter()
        .find(|block| block.kind == BlockKind::Method && block.hash.to_string() == duplicate_hash)
        .map(|block| block.start_line as u64)
        .context("expected method start line")?;

    assert_ne!(function_start_line, method_start_line);

    repo.run(&[
        "mark",
        "--fingerprint",
        &impl_hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &duplicate_hash,
        "--verdict",
        "question",
        "--quiet",
    ])?;

    let feedback_output = repo.run(&["feedback", "--format", "json"])?;
    let feedback = json_array(&feedback_output)?;
    let duplicate_entries: Vec<&Value> = feedback
        .iter()
        .filter(|entry| {
            entry["file"].as_str() == Some("src/lib.rs")
                && entry["block"]["hash"].as_str() == Some(duplicate_hash.as_str())
        })
        .collect();

    assert_eq!(
        duplicate_entries.len(),
        1,
        "expected only uncovered duplicate hash block in feedback"
    );
    assert_eq!(
        duplicate_entries[0]["block"]["start_line"].as_u64(),
        Some(function_start_line)
    );

    Ok(())
}

#[test]
fn test_filestore_uses_repo_root_from_subdir() -> Result<()> {
    let repo = TestRepo::new("filestore_root")?;
    let nested = repo.path.join("nested");
    fs::create_dir_all(&nested)?;

    repo.run_in(
        &[
            "mark",
            "--fingerprint",
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "--verdict",
            "approved",
            "--quiet",
        ],
        &nested,
    )?;

    assert!(repo.path.join(".trueflow").exists());
    assert!(!nested.join(".trueflow").exists());
    Ok(())
}

#[test]
fn test_main_review_uses_merge_base() -> Result<()> {
    let repo = TestRepo::new("diff_merge_base")?;
    repo.write("src/file1.rs", "fn one() {}\n")?;
    repo.commit_all("Add file1")?;
    repo.git(&["checkout", "-B", "main"])?;

    repo.git(&["checkout", "-b", "feature/one"])?;

    repo.write("src/file1.rs", "fn one() { println!(\"feat\"); }\n")?;
    repo.commit_all("Update file1")?;

    repo.git(&["checkout", "main"])?;

    repo.write("src/file2.rs", "fn two() {}\n")?;
    repo.commit_all("Add file2")?;

    repo.git(&["checkout", "feature/one"])?;

    let summary = review_main(&repo)?;
    let files: Vec<&str> = summary
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();

    assert!(files.contains(&"src/file1.rs"));
    assert!(!files.contains(&"src/file2.rs")); // file2 is on main, not in diff base..head?
    // main..head(feature) should include changes in feature not in main.
    // file1 modified. file2 added on main.
    // merge-base is the split point.
    // Diff is base..head.
    // base = split point.
    // head = feature tip.
    // So file2 (on main) is NOT in range. Correct.
    Ok(())
}

#[test]
fn test_main_review_respects_file_coverage_from_subdir() -> Result<()> {
    let repo = TestRepo::new("diff_file_coverage_subdir")?;
    repo.write("pkg/src/lib.rs", "pub fn value() { println!(\"one\"); }\n")?;
    repo.commit_all("Initial")?;
    repo.git(&["checkout", "-B", "main"])?;
    repo.git(&["checkout", "-b", "feature/subdir"])?;

    repo.write("pkg/src/lib.rs", "pub fn value() { println!(\"two\"); }\n")?;
    repo.commit_all("Change value")?;

    let scan = repo.scan_without_cache()?;
    let file_hash = scan
        .files
        .iter()
        .find(|file| file.path.as_str() == "pkg/src/lib.rs")
        .map(|file| file.tree_hash.to_string())
        .context("expected pkg/src/lib.rs tree hash")?;

    let approved_file = build_review_record(
        &file_hash,
        ReviewRecordOverrides {
            target_kind: Some("file"),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&repo.path.join(".trueflow"), &[approved_file])?;

    let root_summary = review_main(&repo)?;
    assert!(
        root_summary.files.is_empty(),
        "expected root diff to be covered"
    );

    let pkg_summary = repo.review_summary_in(
        &repo.path.join("pkg"),
        ReviewRequest::Targets(vec![ReviewTarget::MainDiff]),
        &[],
        &[],
    )?;
    assert!(
        pkg_summary.files.is_empty(),
        "expected subdir diff to honor file coverage"
    );

    Ok(())
}

#[test]
fn test_feedback_xml_escapes_cdata_end() -> Result<()> {
    let repo = TestRepo::new("feedback_cdata")?;
    repo.write("src/lib.rs", "pub fn core() { println!(\"]]>\"); }\n")?;
    repo.commit_all("Add lib")?;

    let hash = first_scan_block_hash_in_process(&repo)?;
    let record = build_review_record(
        &hash,
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            note: Some("Contains CDATA terminator"),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&repo.path.join(".trueflow"), &[record])?;

    let output = repo.run(&["feedback", "--format", "xml"])?;
    assert!(output.contains("<trueflow_feedback>"));
    assert!(output.contains("]]]]><![CDATA[>"));
    Ok(())
}

#[test]
fn review_dirty_staged_only_modification_is_not_empty() -> Result<()> {
    let repo = TestRepo::new("review_dirty_staged_modification")?;
    repo.write("src/lib.rs", "pub fn value() -> i32 { 1 }\n")?;
    repo.commit_all("Initial commit")?;

    repo.write("src/lib.rs", "pub fn value() -> i32 { 2 }\n")?;
    repo.add("src/lib.rs")?;

    let files = json_array(&repo.run(&["review", "--target", "dirty", "--json"])?)?;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"].as_str().context("path")?, "src/lib.rs");

    Ok(())
}

#[test]
fn review_dirty_staged_only_addition_is_not_empty() -> Result<()> {
    let repo = TestRepo::new("review_dirty_staged_addition")?;
    repo.write("src/base.rs", "pub fn base() {}\n")?;
    repo.commit_all("Create HEAD")?;

    repo.write("src/added.rs", "pub fn added() {}\n")?;
    repo.add("src/added.rs")?;

    let files = json_array(&repo.run(&["review", "--target", "dirty", "--json"])?)?;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"].as_str().context("path")?, "src/added.rs");

    Ok(())
}

#[test]
fn review_dirty_staged_only_rename_emits_destination_once() -> Result<()> {
    let repo = TestRepo::new("review_dirty_staged_rename")?;
    repo.write("src/old.rs", "pub fn renamed() {}\n")?;
    repo.commit_all("Initial commit")?;

    repo.git(&["mv", "src/old.rs", "src/new.rs"])?;

    let files = json_array(&repo.run(&["review", "--target", "dirty", "--json"])?)?;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"].as_str().context("path")?, "src/new.rs");

    Ok(())
}

#[cfg(unix)]
const EXTERNAL_SYMLINK_SENTINEL: &str = "TRUEFLOW_OUTSIDE_SYMLINK_SENTINEL_17";

#[cfg(unix)]
fn create_external_symlink(repo: &TestRepo) -> Result<TestRepo> {
    use std::os::unix::fs::symlink;

    let outside = TestRepo::new("external_symlink_sentinel")?;
    outside.write(
        "secret.rs",
        &format!(
            "pub fn {EXTERNAL_SYMLINK_SENTINEL}_escaped() {{ println!(\"{EXTERNAL_SYMLINK_SENTINEL}\"); }}\n"
        ),
    )?;
    symlink(
        outside.path.join("secret.rs"),
        repo.path.join("src/link.rs"),
    )?;
    Ok(outside)
}

#[cfg(unix)]
fn assert_external_symlink_is_not_reviewed(output: std::process::Output) -> Result<()> {
    assert!(
        output.status.success(),
        "review should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    let files = json_array(&stdout)?;
    assert!(
        !files
            .iter()
            .any(|file| file["path"].as_str() == Some("src/link.rs")),
        "review must omit selected symlink: {stdout}"
    );
    assert!(
        !stdout.contains(EXTERNAL_SYMLINK_SENTINEL),
        "review must not expose external sentinel: {stdout}"
    );
    assert!(
        stderr.contains("src/link.rs") && stderr.contains("symbolic link"),
        "targeted symlink rejection should name its path and reason: {stderr}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn review_main_rejects_stable_external_symlink_sentinel() -> Result<()> {
    let repo = TestRepo::new("review_main_external_symlink")?;
    repo.write("src/base.rs", "pub fn base() {}\n")?;
    repo.commit_all("Base")?;
    repo.git(&["checkout", "-B", "main"])?;
    repo.git(&["checkout", "-B", "feature"])?;
    let _outside = create_external_symlink(&repo)?;
    repo.add("src/link.rs")?;
    repo.commit("Add external link")?;

    assert_external_symlink_is_not_reviewed(
        repo.run_raw(&["review", "--target", "main", "--json"])?,
    )?;

    let full = repo.run_raw(&["review", "--all", "--json"])?;
    assert!(full.status.success(), "full review should succeed");
    let stdout = String::from_utf8(full.stdout)?;
    let files = json_array(&stdout)?;
    assert!(
        !files
            .iter()
            .any(|file| file["path"].as_str() == Some("src/link.rs")),
        "full review must omit symlink: {stdout}"
    );
    assert!(
        !stdout.contains(EXTERNAL_SYMLINK_SENTINEL),
        "full review must not expose external sentinel: {stdout}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn review_dirty_rejects_stable_external_symlink_sentinel() -> Result<()> {
    let repo = TestRepo::new("review_dirty_external_symlink")?;
    repo.write("src/base.rs", "pub fn base() {}\n")?;
    repo.commit_all("Base")?;
    let _outside = create_external_symlink(&repo)?;

    assert_external_symlink_is_not_reviewed(
        repo.run_raw(&["review", "--target", "dirty", "--json"])?,
    )
}

#[cfg(unix)]
#[test]
fn review_file_target_rejects_stable_external_symlink_sentinel() -> Result<()> {
    let repo = TestRepo::new("review_file_external_symlink")?;
    repo.write("src/base.rs", "pub fn base() {}\n")?;
    repo.commit_all("Base")?;
    let _outside = create_external_symlink(&repo)?;

    assert_external_symlink_is_not_reviewed(repo.run_raw(&[
        "review",
        "--target",
        "file:src/link.rs",
        "--json",
    ])?)
}
