use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;

mod common;
use common::*;

#[test]
fn test_optimizer_import_merge_preserves_content() -> Result<()> {
    let repo = TestRepo::new("optimizer_import")?;
    repo.write("src/lib.rs", "use a;\n\nuse b;\nextern crate c;\n")?;
    let output = repo.run(&["scan", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["kind"], "Imports");

    // Note: The optimizer preserves newlines between imports
    assert_eq!(blocks[0]["content"], "use a;\nuse b;\nextern crate c;");
    Ok(())
}

#[test]
fn test_optimizer_module_merge_preserves_content() -> Result<()> {
    let repo = TestRepo::new("optimizer_module")?;
    repo.write("src/lib.rs", "mod a;\nmod b;\n\nextern \"C\" { fn x(); }\n")?;
    let output = repo.run(&["scan", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["kind"], "Modules");
    assert!(blocks[0]["content"].as_str().unwrap().contains("mod a"));
    assert!(
        blocks[0]["content"]
            .as_str()
            .unwrap()
            .contains("extern \"C\"")
    );
    Ok(())
}

#[test]
fn test_optimizer_module_merge_preserves_test_tags() -> Result<()> {
    let repo = TestRepo::new("optimizer_module_tags")?;
    repo.write(
        "src/lib.rs",
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn it_works() {}\n}\n\nmod helper {\n    pub fn noop() {}\n}\n",
    )?;

    let output = repo.run(&["scan", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["kind"], "Modules");

    let tags = blocks[0]["tags"]
        .as_array()
        .context("tags should be array")?;
    assert!(
        tags.iter().any(|tag| tag.as_str() == Some("test")),
        "expected merged module block to retain test tag, got {tags:?}"
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

    let output = repo.run(&["scan", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["kind"], "Imports");
    assert_eq!(blocks[1]["kind"], "import");

    Ok(())
}

#[test]
fn test_optimizer_small_file_collapses_mixed_semantic_blocks_e2e() -> Result<()> {
    let repo = TestRepo::new("optimizer_small_file_collapse")?;
    repo.write(
        "src/lib.rs",
        "use std::fmt;\n\nfn run() {\n    if true {}\n}\n\nconst LIMIT: usize = 3;\n",
    )?;

    let output = repo.run(&["scan", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["kind"], "code");

    let complexity = blocks[0]["complexity"]
        .as_u64()
        .context("complexity should be u64")?;
    assert!(
        complexity >= 1,
        "expected collapsed block complexity to include function complexity, got {complexity}"
    );

    Ok(())
}

#[test]
fn test_diff_new_content_matches_post_hunk() -> Result<()> {
    // GIVEN: a change that replaces a line in the working tree
    let repo = TestRepo::new("diff_new_content")?;
    let initial = include_str!("fixtures/diff_new_content_initial.rs");
    let updated = include_str!("fixtures/diff_new_content_updated.rs");
    repo.write("src/main.rs", initial)?;
    repo.commit_all("Initial")?;

    repo.git(&["checkout", "-b", "feature/update"])?;

    repo.write("src/main.rs", updated)?;
    repo.commit_all("Update message")?;

    // WHEN: we compute diff JSON
    let output = repo.run(&["diff", "--json"])?;
    let changes: Value = serde_json::from_str(&output)?;
    let change = changes
        .as_array()
        .context("Expected array")?
        .first()
        .context("Expected change")?;
    let new_content = change["new_content"].as_str().context("new_content")?;

    // THEN: new_content reflects the post-hunk file content
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

    // GIVEN: two review records for the same block with different timestamps
    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

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
    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;

    // THEN: the newer approval wins and nothing remains to review
    assert!(files.is_empty());
    Ok(())
}

#[test]
fn test_feedback_latest_timestamp_wins() -> Result<()> {
    let repo = TestRepo::new("feedback_timestamp")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

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

    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

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

    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    let trueflow_dir = repo.path.join(".trueflow");
    let first_review = build_review_record(
        &hash,
        ReviewRecordOverrides {
            verdict: Some("rejected"),
            timestamp: Some(1000),
            ..Default::default()
        },
    );
    write_reviews_jsonl(&trueflow_dir, &[first_review.clone()])?;

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
    let second_entry = second_entries.first().context("expected second feedback entry")?;
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
fn test_review_progress_counts_duplicate_blocks() -> Result<()> {
    let repo = TestRepo::new("review_duplicates")?;
    // Two identical functions
    let content =
        "fn duplicate() { println!(\"hello\"); }\n\nfn duplicate() { println!(\"hello\"); }\n";
    repo.write("src/lib.rs", content)?;
    repo.commit_all("Add duplicates")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;
    let blocks = &files[0]["blocks"].as_array().context("blocks")?;

    // Should have 2 blocks
    assert_eq!(blocks.len(), 2);

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
        if is_gap(kind) {
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

    // Restore permissions so cleanup can remove the directory
    let mut perms = fs::metadata(&secret_dir)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&secret_dir, perms)?;

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
fn test_scan_cache_write_permission_error_is_non_fatal() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new("scan_cache_write_perm")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Add main")?;

    let home = repo.path.join("readonly-home");
    fs::create_dir_all(&home)?;
    let mut perms = fs::metadata(&home)?.permissions();
    perms.set_mode(0o500);
    fs::set_permissions(&home, perms)?;

    let home_value = home.to_string_lossy().to_string();
    let run_result = repo.run_with_env(&["scan", "--json"], &[("HOME", home_value.as_str())]);

    let mut reset = fs::metadata(&home)?.permissions();
    reset.set_mode(0o755);
    fs::set_permissions(&home, reset)?;

    let output = run_result?;
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
fn test_scan_cache_detects_new_untracked_files() -> Result<()> {
    let repo = TestRepo::new("scan_cache_new_untracked")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Add main")?;

    let home = repo.path.join("cache-home");
    fs::create_dir_all(&home)?;
    let home_value = home.to_string_lossy().to_string();

    let initial = repo.run_with_env(&["scan", "--json"], &[("HOME", home_value.as_str())])?;
    let initial_files = json_array(&initial)?;
    assert!(initial_files.iter().any(|entry| {
        entry["path"]
            .as_str()
            .unwrap_or_default()
            .contains("src/main.rs")
    }));

    repo.write("src/new_file.rs", "pub fn new_file() {}\n")?;

    let rescanned = repo.run_with_env(&["scan", "--json"], &[("HOME", home_value.as_str())])?;
    let rescanned_files = json_array(&rescanned)?;
    assert!(rescanned_files.iter().any(|entry| {
        entry["path"]
            .as_str()
            .unwrap_or_default()
            .contains("src/new_file.rs")
    }));

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

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    assert!(files.iter().all(|entry| {
        !entry["path"]
            .as_str()
            .unwrap_or_default()
            .contains("mutants.out/")
    }));
    assert!(files.iter().any(|entry| {
        entry["path"]
            .as_str()
            .unwrap_or_default()
            .contains("src/main.rs")
    }));
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

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    assert!(files.iter().any(|entry| {
        entry["path"]
            .as_str()
            .unwrap_or_default()
            .contains(".envrc")
    }));
    assert!(files.iter().all(|entry| {
        !entry["path"]
            .as_str()
            .unwrap_or_default()
            .contains("ignored.txt")
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

    let scan_output = repo.run(&["scan", "--json"])?;
    let files = json_array(&scan_output)?;
    let file = files
        .iter()
        .find(|entry| entry["path"].as_str() == Some("src/lib.rs"))
        .context("expected src/lib.rs in scan output")?;
    let blocks = file["blocks"]
        .as_array()
        .context("blocks should be array")?;

    let impl_hash = blocks
        .iter()
        .find(|block| block["kind"].as_str() == Some("impl"))
        .and_then(|block| block["hash"].as_str())
        .context("expected impl block hash")?
        .to_string();

    let duplicate_hash = blocks
        .iter()
        .find(|block| block["kind"].as_str() == Some("function"))
        .and_then(|block| block["hash"].as_str())
        .context("expected function block hash")?
        .to_string();

    let function_start_line = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("function")
                && block["hash"].as_str() == Some(duplicate_hash.as_str())
        })
        .and_then(|block| block["start_line"].as_u64())
        .context("expected function start line")?;

    let method_start_line = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("method")
                && block["hash"].as_str() == Some(duplicate_hash.as_str())
        })
        .and_then(|block| block["start_line"].as_u64())
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
fn test_diff_respects_file_coverage_from_subdir() -> Result<()> {
    let repo = TestRepo::new("diff_file_coverage_subdir")?;
    repo.write("pkg/src/lib.rs", "pub fn value() { println!(\"one\"); }\n")?;
    repo.commit_all("Initial")?;
    repo.git(&["checkout", "-B", "main"])?;
    repo.git(&["checkout", "-b", "feature/subdir"])?;

    repo.write("pkg/src/lib.rs", "pub fn value() { println!(\"two\"); }\n")?;
    repo.commit_all("Change value")?;

    let scan = repo.run(&["scan", "--json"])?;
    let files = json_array(&scan)?;
    let file_hash = files
        .iter()
        .find(|file| file["path"].as_str() == Some("pkg/src/lib.rs"))
        .and_then(|file| file["file_hash"].as_str())
        .context("expected pkg/src/lib.rs file hash")?
        .to_string();

    let approved_file = build_review_record(&file_hash, ReviewRecordOverrides::default());
    write_reviews_jsonl(&repo.path.join(".trueflow"), &[approved_file])?;

    let root_output = repo.run(&["diff", "--json"])?;
    let root_changes = json_array(&root_output)?;
    assert!(root_changes.is_empty(), "expected root diff to be covered");

    let pkg_output = repo.run_in(&["diff", "--json"], &repo.path.join("pkg"))?;
    let pkg_changes = json_array(&pkg_output)?;
    assert!(
        pkg_changes.is_empty(),
        "expected subdir diff to honor file coverage"
    );

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
