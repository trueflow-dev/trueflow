use anyhow::Result;

use std::collections::HashSet;
use trueflow_test_support::{TestRepo, run_git_output};

use trueflow::repo_path::RepoPath;
use trueflow::vcs::ChangedPath;

fn repo_paths(paths: &[&str]) -> Result<HashSet<RepoPath>> {
    paths.iter().map(|&path| RepoPath::new(path)).collect()
}

#[test]
fn test_recent_commits_in_repo_returns_head_first() -> Result<()> {
    let repo = TestRepo::new("recent_commits")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("First commit")?;
    repo.write("src/main.rs", "fn main() { println!(\"hi\"); }\n")?;
    repo.commit_all("Second commit")?;

    let git_repo = gix::open(&repo.path)?;
    let commits = trueflow::vcs::recent_commits_in_repo(&git_repo, 8)?;

    assert!(commits.len() >= 2, "expected at least two commits");
    assert_eq!(commits[0].summary, "Second commit");
    assert_eq!(commits[1].summary, "First commit");

    Ok(())
}

#[test]
fn test_files_changed_main_to_head_in_repo() -> Result<()> {
    let repo = TestRepo::new("main_diff")?;
    repo.write("src/main.rs", "fn main() {}\n")?;
    repo.commit_all("Base")?;
    repo.git(&["checkout", "-B", "main"])?;
    repo.git(&["checkout", "-B", "feature"])?;
    repo.write("src/lib.rs", "pub fn helper() {}\n")?;
    repo.commit_all("Add helper")?;

    let git_repo = gix::open(&repo.path)?;
    let changed = trueflow::vcs::files_changed_main_to_head_in_repo(&git_repo)?;

    assert!(
        changed.contains(&ChangedPath::identity(RepoPath::new("src/lib.rs")?)),
        "expected diff to include src/lib.rs"
    );
    Ok(())
}

#[test]
fn test_files_changed_main_to_head_preserves_rename_source_and_destination() -> Result<()> {
    let repo = TestRepo::new("main_diff_rename_pair")?;
    repo.git(&["config", "diff.renames", "true"])?;
    repo.write(
        "src/old.rs",
        r#"pub fn retained_alpha() {
    println!("shared alpha");
}

pub fn retained_beta() {
    println!("shared beta");
}

pub fn retained_gamma() {
    println!("shared gamma");
}
"#,
    )?;
    repo.commit_all("Add rename source")?;
    repo.git(&["checkout", "-B", "main"])?;
    repo.git(&["checkout", "-B", "feature"])?;
    repo.git(&["mv", "src/old.rs", "src/new.rs"])?;
    repo.commit_all("Rename source")?;

    let git_repo = gix::open(&repo.path)?;
    let changed = trueflow::vcs::files_changed_main_to_head_in_repo(&git_repo)?;

    assert_eq!(changed.len(), 1, "expected exactly one renamed path pair");
    let pair = changed.iter().next().expect("renamed path pair");
    assert_eq!(pair.source_location, RepoPath::new("src/old.rs")?);
    assert_eq!(pair.location, RepoPath::new("src/new.rs")?);

    Ok(())
}

#[test]
fn test_diff_hunks_for_file_in_revision_uses_selected_revision() -> Result<()> {
    let repo = TestRepo::new("revision_diff_hunks")?;
    repo.write("src/lib.rs", "pub fn value() -> i32 {\n    1\n}\n")?;
    repo.commit_all("Base")?;

    repo.write("src/lib.rs", "pub fn value() -> i32 {\n    2\n}\n")?;
    repo.commit_all("Set value to 2")?;
    let target_revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?;

    repo.write("src/lib.rs", "pub fn value() -> i32 {\n    3\n}\n")?;
    repo.commit_all("Set value to 3")?;

    let git_repo = gix::open(&repo.path)?;
    let hunks = trueflow::vcs::diff_hunks_for_file_in_revision(
        &git_repo,
        target_revision.trim(),
        &trueflow::repo_path::RepoPath::new("src/lib.rs")?,
    )?;

    assert!(!hunks.is_empty(), "expected at least one hunk");
    let lines = hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .collect::<Vec<_>>();
    assert!(
        lines.iter().any(|line| {
            line.kind == trueflow::vcs::DiffLineKind::Added && line.text == "    2\n"
        }),
        "expected selected revision diff to include value 2"
    );
    assert!(
        !lines.iter().any(|line| {
            line.kind == trueflow::vcs::DiffLineKind::Added && line.text == "    3\n"
        }),
        "selected revision diff should not include later HEAD changes"
    );

    Ok(())
}

#[test]
fn dirty_files_reports_staged_only_modification() -> Result<()> {
    let repo = TestRepo::new("dirty_staged_modification")?;
    repo.write("src/modified.rs", "pub fn value() -> i32 { 1 }\n")?;
    repo.commit_all("Add tracked file")?;

    repo.write("src/modified.rs", "pub fn value() -> i32 { 2 }\n")?;
    repo.add("src/modified.rs")?;

    let git_repo = gix::open(&repo.path)?;
    assert_eq!(
        trueflow::vcs::dirty_files(&git_repo)?,
        repo_paths(&["src/modified.rs"])?
    );

    Ok(())
}

#[test]
fn dirty_files_reports_staged_only_addition() -> Result<()> {
    let repo = TestRepo::new("dirty_staged_addition")?;
    repo.write("src/base.rs", "pub fn base() {}\n")?;
    repo.commit_all("Create HEAD")?;

    repo.write("src/added.rs", "pub fn added() {}\n")?;
    repo.add("src/added.rs")?;

    let git_repo = gix::open(&repo.path)?;
    assert_eq!(
        trueflow::vcs::dirty_files(&git_repo)?,
        repo_paths(&["src/added.rs"])?
    );

    Ok(())
}

#[test]
fn dirty_files_reports_staged_only_deletion() -> Result<()> {
    let repo = TestRepo::new("dirty_staged_deletion")?;
    repo.write("src/deleted.rs", "pub fn deleted() {}\n")?;
    repo.commit_all("Add tracked file")?;

    std::fs::remove_file(repo.path.join("src/deleted.rs"))?;
    repo.git(&["add", "-A"])?;

    let git_repo = gix::open(&repo.path)?;
    assert_eq!(
        trueflow::vcs::dirty_files(&git_repo)?,
        repo_paths(&["src/deleted.rs"])?
    );

    Ok(())
}

#[test]
fn dirty_files_reports_both_sides_of_staged_rename() -> Result<()> {
    let repo = TestRepo::new("dirty_staged_rename")?;
    repo.write("src/old.rs", "pub fn renamed() {}\n")?;
    repo.commit_all("Add tracked file")?;

    repo.git(&["mv", "src/old.rs", "src/new.rs"])?;

    let git_repo = gix::open(&repo.path)?;
    assert_eq!(
        trueflow::vcs::dirty_files(&git_repo)?,
        repo_paths(&["src/old.rs", "src/new.rs"])?
    );

    Ok(())
}

#[test]
fn dirty_files_deduplicates_index_and_worktree_path() -> Result<()> {
    let repo = TestRepo::new("dirty_staged_and_unstaged_modification")?;
    repo.write("src/modified.rs", "pub fn value() -> i32 { 1 }\n")?;
    repo.commit_all("Add tracked file")?;

    repo.write("src/modified.rs", "pub fn value() -> i32 { 2 }\n")?;
    repo.add("src/modified.rs")?;
    repo.write("src/modified.rs", "pub fn value() -> i32 { 3 }\n")?;

    let git_repo = gix::open(&repo.path)?;
    let dirty = trueflow::vcs::dirty_files(&git_repo)?;
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty, repo_paths(&["src/modified.rs"])?);

    Ok(())
}
