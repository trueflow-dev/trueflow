use anyhow::Result;
use std::fs;

mod common;
use common::*;
use trueflow::store::{BlockState, RepoRef, VcsSystem};

#[test]
fn test_review_skips_invalid_db_lines() -> Result<()> {
    let repo = TestRepo::new("invalid_db")?;
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
    content.push_str("not-json\n");
    fs::write(&db_path, content)?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;
    assert!(files.is_empty());

    let records = read_review_records(&db_path)?;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    match &record.repo_ref {
        RepoRef::Vcs { system, revision } => {
            assert_eq!(system, &VcsSystem::Git);
            assert!(!revision.is_empty());
        }
    }
    assert_eq!(record.block_state, BlockState::Committed);

    Ok(())
}
