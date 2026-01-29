use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;

use trueflow::block::{Block, BlockKind};
use trueflow::hashing::hash_str;
use trueflow::optimizer;

mod common;
use common::*;

#[test]
fn test_optimizer_import_merge_preserves_content() {
    let blocks = vec![
        Block::new("use foo;".to_string(), BlockKind::Import, 0, 1),
        Block::new("/*comment*/".to_string(), BlockKind::Gap, 1, 1),
        Block::new("use bar;".to_string(), BlockKind::Import, 1, 2),
    ];

    let expected: String = blocks.iter().map(|b| b.content.as_str()).collect();
    let optimized = optimizer::optimize(blocks);

    assert_eq!(optimized.len(), 1);
    assert_eq!(optimized[0].kind, BlockKind::Imports);
    assert_eq!(optimized[0].content, expected);
    assert_eq!(optimized[0].hash, hash_str(&optimized[0].content));
}

#[test]
fn test_optimizer_module_merge_preserves_content() {
    let blocks = vec![
        Block::new("mod a;\n".to_string(), BlockKind::Module, 0, 1),
        Block::new("// comment\n".to_string(), BlockKind::Comment, 1, 2),
        Block::new("\n".to_string(), BlockKind::Gap, 2, 3),
        Block::new("mod b;\n".to_string(), BlockKind::Module, 3, 4),
    ];

    let expected: String = blocks.iter().map(|b| b.content.as_str()).collect();
    let optimized = optimizer::optimize(blocks);

    assert_eq!(optimized.len(), 1);
    assert_eq!(optimized[0].kind, BlockKind::Modules);
    assert_eq!(optimized[0].content, expected);
    assert_eq!(optimized[0].hash, hash_str(&optimized[0].content));
}

#[test]
fn test_diff_new_content_matches_post_hunk() -> Result<()> {
    let repo = TestRepo::new("diff_new_content")?;
    let initial = include_str!("fixtures/diff_new_content_initial.rs");
    let updated = include_str!("fixtures/diff_new_content_updated.rs");
    repo.write("src/main.rs", initial)?;
    repo.commit_all("Initial")?;

    repo.git(&["checkout", "-b", "feature/update"])?;

    repo.write("src/main.rs", updated)?;
    repo.commit_all("Update message")?;

    let output = repo.run(&["diff", "--json"])?;
    let changes: Value = serde_json::from_str(&output)?;
    let change = changes
        .as_array()
        .context("Expected array")?
        .first()
        .context("Expected change")?;
    let new_content = change["new_content"].as_str().context("new_content")?;

    let file_content = fs::read_to_string(repo.path.join("src/main.rs"))?;
    assert_eq!(new_content, file_content);
    Ok(())
}

#[test]
fn test_review_ignores_non_review_checks() -> Result<()> {
    let repo = TestRepo::new("review_check_filter")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    // GIVEN: a reviewable block with no review verdicts
    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

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
    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;
    assert!(!files.is_empty());
    Ok(())
}

#[test]
fn test_review_latest_timestamp_wins() -> Result<()> {
    let repo = TestRepo::new("review_timestamp")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    let trueflow_dir = repo.path.join(".trueflow");
    let approved = review_record(
        &hash,
        ReviewRecordOverrides {
            timestamp: Some(2000),
            ..Default::default()
        },
    );
    let rejected = review_record(
        &hash,
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            email: Some("b@example.com"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&trueflow_dir, &[approved, rejected])?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;
    assert!(files.is_empty());
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

    let output = repo.run(&["review", "--all", "--json"])?;
    let json: Value = serde_json::from_str(&output)?;
    let block = &json.as_array().context("Expected array")?[0]["blocks"][0];
    let parent_hash = block["hash"].as_str().context("hash")?;

    let output = repo.run(&["inspect", "--fingerprint", parent_hash, "--split"])?;
    let sub_blocks: Vec<Value> = serde_json::from_str(&output)?;

    for sub_block in &sub_blocks {
        let kind = sub_block["kind"].as_str().context("kind")?;
        if kind.eq_ignore_ascii_case("gap") {
            continue;
        }
        let hash = sub_block["hash"].as_str().context("hash")?;
        repo.run(&[
            "mark",
            "--fingerprint",
            hash,
            "--verdict",
            "approved",
            "--quiet",
        ])?;
    }

    let output = repo.run(&["review", "--all", "--exclude", "gap", "--json"])?;
    let json: Value = serde_json::from_str(&output)?;
    assert!(json.as_array().context("Expected array")?.is_empty());
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

    let output = repo.run(&["scan", "--json"])?;
    let json: Value = serde_json::from_str(&output)?;
    let files = json.as_array().context("Expected array")?;
    assert!(files.iter().any(|entry| {
        entry["path"]
            .as_str()
            .unwrap_or_default()
            .contains("src/main.rs")
    }));
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
            "deadbeef",
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
fn test_diff_uses_merge_base() -> Result<()> {
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

    let output = repo.run(&["diff", "--json"])?;
    let changes: Value = serde_json::from_str(&output)?;
    let files: Vec<&str> = changes
        .as_array()
        .context("Expected array")?
        .iter()
        .filter_map(|entry| entry["file"].as_str())
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
fn test_feedback_xml_escapes_cdata_end() -> Result<()> {
    let repo = TestRepo::new("feedback_cdata")?;
    repo.write("src/lib.rs", "pub fn core() { println!(\"]]>\"); }\n")?;
    repo.commit_all("Add lib")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let json: Value = serde_json::from_str(&output)?;
    let block = &json.as_array().context("Expected array")?[0]["blocks"][0];
    let hash = block["hash"].as_str().context("hash")?;

    repo.run(&[
        "mark",
        "--fingerprint",
        hash,
        "--verdict",
        "rejected",
        "--note",
        "Contains CDATA terminator",
        "--quiet",
    ])?;

    let output = repo.run(&["feedback", "--format", "xml"])?;
    assert!(output.contains("<trueflow_feedback>"));
    assert!(output.contains("]]]]><![CDATA[>"));
    Ok(())
}
