use anyhow::Result;
use std::fs;

use trueflow::store::{BlockState, RepoRef, VcsSystem};
use trueflow_test_support::*;

#[test]
fn test_mark_recovers_from_truncated_db_tail() -> Result<()> {
    let repo = TestRepo::new("truncated_db_tail")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let (hash, path) = first_block_info(&output)?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "approved",
        "--path",
        &path,
        "--quiet",
    ])?;

    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    let mut content = fs::read_to_string(&db_path)?;
    content.push_str("{\"id\":\"torn\"");
    fs::write(&db_path, content)?;

    repo.write(
        "src/lib.rs",
        "pub fn core() {}\n\npub fn added_after_crash() {}\n",
    )?;
    repo.commit_all("Add second function")?;
    let output = repo.run(&["review", "--all", "--json"])?;
    let (hash, path) = first_block_info(&output)?;
    repo.run(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "approved",
        "--path",
        &path,
        "--quiet",
    ])?;

    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 2);
    for record in &records {
        match &record.repo_ref {
            RepoRef::Vcs { system, revision } => {
                assert_eq!(system, &VcsSystem::Git);
                assert!(!revision.as_str().is_empty());
            }
            RepoRef::Unknown => panic!("expected vcs repo ref"),
        }
        assert_eq!(record.block_state, BlockState::Committed);
    }

    let output = repo.run(&["review", "--all", "--json"])?;
    assert!(json_array(&output)?.is_empty());
    Ok(())
}
