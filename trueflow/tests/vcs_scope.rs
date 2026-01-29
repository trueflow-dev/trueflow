use anyhow::Result;

mod common;
use common::TestRepo;

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
