use anyhow::Result;
use std::fs;
use std::process::Command;
use trueflow::analysis::Language;
use trueflow::block::{Block, BlockKind};
use trueflow::store::{BlockState, ReviewTargetRef};
use trueflow::{block_splitter, sub_splitter};
use trueflow_test_support::*;

#[test]
fn test_mark_uncommitted_state() -> Result<()> {
    let repo = TestRepo::new("uncommitted_state")?;

    // 1. Create committed file
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Initial commit")?;

    // 2. Modify it (make it dirty)
    repo.write("src/main.rs", "fn main() { println!(\"dirty\"); }\n")?;

    // 3. Scan to get hash of dirty block
    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    // 4. Mark it
    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "approved",
        "--path",
        "src/main.rs",
        "--quiet",
    ])?;

    // 5. Check DB
    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);

    assert_eq!(records[0].block_state, BlockState::Uncommitted);

    Ok(())
}

#[test]
fn mark_staged_only_change_is_uncommitted() -> Result<()> {
    let repo = TestRepo::new("staged_only_uncommitted_state")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Initial commit")?;

    repo.write("src/main.rs", "fn main() { println!(\"staged\"); }\n")?;
    repo.add("src/main.rs")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "approved",
        "--path",
        "src/main.rs",
        "--quiet",
    ])?;

    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].block_state, BlockState::Uncommitted);

    Ok(())
}

#[test]
fn test_mark_committed_state_normalizes_subdir_relative_path_hint() -> Result<()> {
    let repo = TestRepo::new("committed_state_subdir_hint")?;

    repo.write("pkg/src/lib.rs", "pub fn stable() {}\n")?;
    repo.commit_all("Initial commit")?;

    let package_dir = repo.path.join("pkg");
    let output = repo.run_in(&["review", "--all", "--json"], &package_dir)?;
    let hash = first_block_hash(&output)?;

    repo.run_in(
        &[
            "mark",
            "--fingerprint",
            &hash,
            "--verdict",
            "approved",
            "--path",
            "src/lib.rs",
            "--quiet",
        ],
        &package_dir,
    )?;

    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].block_state, BlockState::Committed);
    assert_eq!(
        records[0].path_hint.as_ref().map(|path| path.as_str()),
        Some("pkg/src/lib.rs")
    );

    Ok(())
}

#[test]
fn narrowing_review_scope_does_not_transfer_another_files_approval() -> Result<()> {
    let repo = TestRepo::new("scoped_approval_identity")?;
    repo.write("src/a.rs", "pub fn same() {}\n")?;
    repo.write("src/b.rs", "pub fn same() {}\n")?;
    repo.commit_all("Identical occurrences")?;
    let hash = first_block_hash(&repo.run(&["review", "--target", "file:src/a.rs", "--json"])?)?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--path",
        "src/a.rs",
        "--quiet",
    ])?;
    let all: serde_json::Value = serde_json::from_str(&repo.run(&["review", "--all", "--json"])?)?;
    let scoped: serde_json::Value =
        serde_json::from_str(&repo.run(&["review", "--target", "file:src/b.rs", "--json"])?)?;
    assert_eq!(all[0]["path"], "src/b.rs");
    assert_eq!(scoped, all);
    Ok(())
}

#[test]
fn test_mark_unknown_state_no_path() -> Result<()> {
    let repo = TestRepo::new("unknown_state")?;

    // Just mark arbitrary hash without path
    let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    repo.run(&[
        "mark",
        "--fingerprint",
        hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);

    assert_eq!(records[0].block_state, BlockState::Unknown);

    Ok(())
}

#[test]
fn test_mark_rejects_empty_or_reversed_comment_scope() -> Result<()> {
    let repo = TestRepo::new("invalid_comment_scope")?;
    let fingerprint = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    for (start, end) in [("4", "4"), ("5", "4")] {
        let error = repo.run_err(&[
            "mark",
            "--fingerprint",
            fingerprint,
            "--comment-scope-start",
            start,
            "--comment-scope-end",
            end,
        ])?;
        assert!(
            error.contains("comment scope end must be greater than start"),
            "expected invalid comment scope error, got: {error}"
        );
    }

    assert!(!repo.path.join(".trueflow/reviews.jsonl").exists());
    Ok(())
}

#[test]
fn test_mark_comment_requires_visible_note_text() -> Result<()> {
    let repo = TestRepo::new("comment_requires_note")?;
    let fingerprint = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    for note in [None, Some("   ")] {
        let mut args = vec!["mark", "--fingerprint", fingerprint, "--verdict", "comment"];
        if let Some(note) = note {
            args.extend(["--note", note]);
        }

        let error = repo.run_err(&args)?;
        assert!(
            error.contains("comment verdict requires non-empty note text"),
            "expected missing comment note error, got: {error}"
        );
    }

    assert!(!repo.path.join(".trueflow/reviews.jsonl").exists());
    Ok(())
}

#[test]
fn test_mark_diff_block_hash_records_block_target() -> Result<()> {
    let repo = TestRepo::new("mark_diff_block_hash_block_target")?;
    repo.write("src/lib.rs", "pub fn value() -> i32 { 1 }\n")?;
    repo.commit_all("Initial")?;
    repo.git(&["checkout", "-B", "main"])?;
    repo.git(&["checkout", "-b", "feature/diff-block-hash"])?;

    repo.write("src/lib.rs", "pub fn value() -> i32 { 2 }\n")?;
    repo.commit_all("Change value")?;

    let review_output = repo.run(&["review", "--target", "main", "--json"])?;
    let fingerprint = first_block_hash(&review_output)?;

    repo.run(&[
        "mark",
        "--fingerprint",
        &fingerprint,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);
    match &records[0].target {
        ReviewTargetRef::Block { hash } => assert_eq!(hash.as_str(), fingerprint),
        other => panic!("expected block target, got {other:?}"),
    }

    let changes_after = json_array(&repo.run(&["review", "--target", "main", "--json"])?)?;
    assert!(changes_after.is_empty());

    Ok(())
}

fn ignored_log_repo(name: &str) -> Result<(TestRepo, String)> {
    let repo = TestRepo::new(name)?;
    repo.write("trueflow.toml", "[scan]\nignore_globs = [\"*.log\"]\n")?;
    repo.write("src/a.rs", "pub fn a() {}\n")?;
    repo.write("src/x.log", "ignored by the configured scan\n")?;
    repo.commit_all("Initial")?;
    let tree = json(&repo.run(&["scan", "--json", "--tree"])?)?;
    let dir_hash = find_tree_hash(&tree, "src")?;
    Ok((repo, dir_hash))
}

#[test]
fn mark_infers_directory_target_with_configured_scan_options() -> Result<()> {
    let (repo, dir_hash) = ignored_log_repo("mark_dir_scan_options")?;

    repo.run(&[
        "mark",
        "--fingerprint",
        &dir_hash,
        "--path",
        "src",
        "--quiet",
    ])?;

    let records = read_review_records(&repo.path.join(".trueflow/reviews.jsonl"))?;
    assert_eq!(records.len(), 1);
    match &records[0].target {
        ReviewTargetRef::Tree { hash } => assert_eq!(hash.as_str(), dir_hash),
        other => panic!("expected tree target, got {other:?}"),
    }
    assert!(json_array(&repo.run(&["review", "--target", "dir:src", "--json"])?)?.is_empty());
    Ok(())
}

#[test]
fn mark_rejects_fingerprint_that_does_not_match_its_directory_path() -> Result<()> {
    let (repo, _) = ignored_log_repo("mark_dir_stale_fingerprint")?;
    let stale = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    let error = repo.run_err(&["mark", "--fingerprint", stale, "--path", "src", "--quiet"])?;

    assert!(
        error.contains("does not match directory `src`"),
        "expected directory mismatch error, got: {error}"
    );
    assert!(!repo.path.join(".trueflow/reviews.jsonl").exists());
    Ok(())
}

/// The children `C` reveals in the TUI for `block`: the forced child split
/// without gaps or a child spanning the whole parent.
fn tui_child_blocks(block: &Block) -> Result<Vec<Block>> {
    Ok(
        sub_splitter::split_result_for_child_navigation(block, Language::Rust)?
            .blocks
            .into_iter()
            .filter(|child| child.kind != BlockKind::Gap && child.byte_span() != block.byte_span())
            .collect(),
    )
}

fn mark_block_approved(repo: &TestRepo, block: &Block) -> Result<()> {
    let line = block.start_line.to_string();
    repo.run(&[
        "mark",
        "--fingerprint",
        block.hash.as_str(),
        "--verdict",
        "approved",
        "--path",
        "src/lib.rs",
        "--line",
        &line,
        "--quiet",
    ])?;
    Ok(())
}

#[test]
fn approvals_of_tui_expanded_children_cover_their_short_parent() -> Result<()> {
    let repo = TestRepo::new("tui_expanded_children_cover_parent")?;
    repo.write("src/lib.rs", "pub fn value() -> i32 {\n    1\n}\n")?;
    repo.commit_all("Initial")?;
    repo.git(&["checkout", "-B", "main"])?;
    repo.git(&["checkout", "-b", "feature/expanded-children"])?;
    let source = "pub fn value() -> i32 {\n    let a = 1;\n    let b = 2;\n\n    a + b\n}\n";
    repo.write("src/lib.rs", source)?;
    repo.commit_all("Split value")?;

    let function = block_splitter::split(source, Language::Rust)
        .into_review_blocks(source)
        .into_iter()
        .find(|block| block.kind == BlockKind::Function)
        .ok_or_else(|| anyhow::anyhow!("expected the function review block"))?;
    let children = tui_child_blocks(&function)?;
    assert!(children.len() > 1, "expected TUI children: {children:?}");
    let (last_child, other_children) = children
        .split_last()
        .ok_or_else(|| anyhow::anyhow!("expected TUI children"))?;

    for child in other_children {
        mark_block_approved(&repo, child)?;
    }
    let partial = repo.run_raw(&["check"])?;
    assert!(
        !partial.status.success(),
        "an unapproved child must keep the function unreviewed"
    );

    mark_block_approved(&repo, last_child)?;
    let review = json_array(&repo.run(&["review", "--target", "main", "--json"])?)?;
    assert!(
        review.is_empty(),
        "approving every TUI child must cover the function: {review:?}"
    );
    let check = repo.run_raw(&["check"])?;
    assert!(
        check.status.success(),
        "check must count approvals of TUI children: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    Ok(())
}

#[test]
fn test_store_subdirectory_discovery() -> Result<()> {
    let repo = TestRepo::new("subdir_discovery")?;

    let subdir = repo.path.join("subdir");
    fs::create_dir(&subdir)?;

    let hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // Run mark from subdir
    repo.run_in(
        &[
            "mark",
            "--fingerprint",
            hash,
            "--verdict",
            "approved",
            "--quiet",
        ],
        &subdir,
    )?;

    // Check DB at ROOT
    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    assert!(db_path.exists(), "DB should be at repo root");

    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);
    assert_eq!(record_target_key(&records[0]), hash);

    Ok(())
}

#[test]
fn test_mark_signing_failure() -> Result<()> {
    let repo = TestRepo::new("signing_fail")?;

    // Configure signing key that doesn't exist
    repo.git(&["config", "user.signingkey", "DEADBEEF"])?;

    let hash = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    // Attempt mark, expect failure
    let output = repo.run_err(&[
        "mark",
        "--fingerprint",
        hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    // The error message varies by environment (GPG version, locale, whether GPG is installed).
    // We just verify:
    // 1. The command failed (run_err ensures this)
    // 2. There's some error output mentioning signing-related keywords
    let output_lower = output.to_lowercase();
    assert!(
        output_lower.contains("gpg")
            || output_lower.contains("sign")
            || output_lower.contains("key")
            || output_lower.contains("spawn"),
        "Expected signing-related error message, got: {output}"
    );

    Ok(())
}

#[test]
fn test_store_parent_discovery_no_git() -> Result<()> {
    // Manually set up dirs without git
    let root = temp_test_dir("no_git_discovery");
    fs::create_dir_all(&root)?;

    // Create .trueflow at root
    let trueflow_dir = root.join(".trueflow");
    fs::create_dir(&trueflow_dir)?;

    // Create subdir
    let subdir = root.join("subdir");
    fs::create_dir(&subdir)?;

    let bin = env!("CARGO_BIN_EXE_trueflow");
    let hash = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    let output = Command::new(bin)
        .args([
            "mark",
            "--fingerprint",
            hash,
            "--verdict",
            "approved",
            "--quiet",
        ])
        .current_dir(&subdir)
        .output()?;

    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "trueflow failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Verify DB at root
    let db_path = trueflow_dir.join("reviews.jsonl");
    assert!(
        db_path.exists(),
        "DB should be found/created in parent .trueflow"
    );

    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);

    Ok(())
}

#[test]
fn store_discovery_outside_git_ignores_home_cache_dir() -> Result<()> {
    let home = temp_test_dir("no_git_home_cache_dir");
    fs::create_dir_all(home.join(".trueflow").join("cache"))?;
    let project = home.join("proj");
    fs::create_dir_all(&project)?;

    let hash = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let output = Command::new(env!("CARGO_BIN_EXE_trueflow"))
        .args([
            "mark",
            "--fingerprint",
            hash,
            "--verdict",
            "approved",
            "--quiet",
        ])
        .env("HOME", &home)
        .current_dir(&project)
        .output()?;
    assert!(
        output.status.success(),
        "trueflow failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let project_store = project.join(".trueflow");
    assert_eq!(
        read_review_records(&project_store.join("reviews.jsonl"))?.len(),
        1
    );
    assert!(project_store.join("logs").is_dir());
    let home_store = home.join(".trueflow");
    assert!(
        !home_store.join("reviews.jsonl").exists(),
        "the global cache directory must not collect project reviews"
    );
    assert!(!home_store.join("logs").exists());

    Ok(())
}

#[test]
fn test_commands_write_logs_under_trueflow_directory() -> Result<()> {
    let repo = TestRepo::new("logs_under_trueflow")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Initial")?;

    repo.run(&["scan", "--json"])?;

    let log_dir = repo.path.join(".trueflow").join("logs");
    assert!(log_dir.is_dir(), "expected .trueflow/logs to exist");

    let has_log_file = fs::read_dir(&log_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .any(|path| path.extension().and_then(|ext| ext.to_str()) == Some("log"));
    assert!(has_log_file, "expected at least one .log file in logs dir");

    Ok(())
}

#[test]
fn default_scan_cache_writes_outside_home_dot_trueflow_and_evicts_excess_entries() -> Result<()> {
    let home = temp_test_dir("scan_cache_isolated_home");
    let xdg_cache = home.join("xdg-cache");
    fs::create_dir_all(&xdg_cache)?;

    let proj = temp_test_dir("scan_cache_isolated_proj");
    fs::create_dir_all(proj.join("src"))?;
    fs::write(proj.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n")?;
    run_git(&proj, &["init", "-q"])?;

    let bin = env!("CARGO_BIN_EXE_trueflow");
    let output = Command::new(bin)
        .args(["scan", "--json"])
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env_remove("TRUEFLOW_CACHE_DIR")
        .current_dir(&proj)
        .output()?;
    assert!(
        output.status.success(),
        "trueflow scan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !home.join(".trueflow").exists(),
        "default scan cache must not create ~/.trueflow"
    );

    let expected_cache_dir = if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Caches")
            .join("trueflow")
            .join("scan")
    } else {
        xdg_cache.join("trueflow").join("scan")
    };
    let cached_files: Vec<_> = fs::read_dir(&expected_cache_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("scan-") && name.ends_with(".json"))
        })
        .collect();
    assert_eq!(cached_files.len(), 1);

    let bounded_cache = temp_test_dir("scan_cache_bounded_override");
    fs::create_dir_all(&bounded_cache)?;
    for idx in 0..135_u32 {
        fs::write(
            bounded_cache.join(format!("scan-stale-{idx:03}-0000.json")),
            b"{}",
        )?;
    }

    let proj2 = temp_test_dir("scan_cache_bounded_proj");
    fs::create_dir_all(proj2.join("src"))?;
    fs::write(proj2.join("src/lib.rs"), "pub fn fresh() -> u32 { 7 }\n")?;
    run_git(&proj2, &["init", "-q"])?;

    let output2 = Command::new(bin)
        .args(["scan", "--json"])
        .env("HOME", &home)
        .env("TRUEFLOW_CACHE_DIR", &bounded_cache)
        .current_dir(&proj2)
        .output()?;
    assert!(
        output2.status.success(),
        "bounded scan failed: {}",
        String::from_utf8_lossy(&output2.stderr)
    );

    let remaining_scan_files = fs::read_dir(&bounded_cache)?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("scan-") && name.ends_with(".json"))
        })
        .count();
    assert!(
        remaining_scan_files <= 128,
        "expected scan cache to evict down to <= 128 files, got {remaining_scan_files}"
    );

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&proj);
    let _ = fs::remove_dir_all(&bounded_cache);
    let _ = fs::remove_dir_all(&proj2);
    Ok(())
}

#[test]
fn store_discovery_outside_git_ignores_cache_only_ancestor_dir() -> Result<()> {
    let home = temp_test_dir("no_git_cache_ancestor_home");
    fs::create_dir_all(&home)?;
    let outer = temp_test_dir("no_git_cache_ancestor_outer");
    fs::create_dir_all(outer.join(".trueflow").join("cache"))?;
    let project = outer.join("proj");
    fs::create_dir_all(&project)?;

    let hash = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let output = Command::new(env!("CARGO_BIN_EXE_trueflow"))
        .args([
            "mark",
            "--fingerprint",
            hash,
            "--verdict",
            "approved",
            "--quiet",
        ])
        .env("HOME", &home)
        .current_dir(&project)
        .output()?;
    assert!(
        output.status.success(),
        "trueflow mark failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        read_review_records(&project.join(".trueflow").join("reviews.jsonl"))?.len(),
        1
    );
    assert!(
        !outer.join(".trueflow").join("reviews.jsonl").exists(),
        "cache-only .trueflow ancestor must not be adopted as a project review store"
    );

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&outer);
    Ok(())
}
