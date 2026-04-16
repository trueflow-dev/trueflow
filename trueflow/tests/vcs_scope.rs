use anyhow::Result;

use trueflow_test_support::{TestRepo, run_git_output};

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
        changed.contains("src/lib.rs"),
        "expected diff to include src/lib.rs"
    );

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
