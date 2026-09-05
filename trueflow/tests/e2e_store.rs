use anyhow::Result;
use std::fs;

use trueflow::store::{FileStore, ReviewStore};
use trueflow_test_support::*;

fn approved_repo(name: &str) -> Result<TestRepo> {
    let repo = TestRepo::new(name)?;
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

    let output = repo.run(&["review", "--all", "--json"])?;
    assert!(json_array(&output)?.is_empty());
    Ok(repo)
}

#[test]
fn test_review_fails_closed_after_malformed_rejection() -> Result<()> {
    let repo = approved_repo("malformed_rejection")?;
    let store = FileStore::for_root(&repo.path)?;
    let approval = store.read_history()?.remove(0);
    let mut rejection = serde_json::to_value(&approval)?;
    rejection["id"] = serde_json::json!("malformed-rejection");
    rejection["verdict"] = serde_json::json!("rejected");
    rejection["timestamp"] = serde_json::json!(approval.timestamp + 1);
    rejection["identity"] = serde_json::json!({"type": "email"});

    // A blank line must not shift the reported physical line of the bad record.
    let content = format!(
        "{}\n\n{}\n",
        serde_json::to_string(&approval)?,
        serde_json::to_string(&rejection)?,
    );
    fs::write(store.db_path(), &content)?;

    let stderr = repo.run_err(&["review", "--all", "--json"])?;
    assert!(
        stderr.contains("line 3"),
        "history corruption must identify its physical line: {stderr}"
    );
    let error = store
        .read_history()
        .expect_err("a malformed rejection must not leave the earlier approval authoritative");
    assert!(
        error.to_string().contains("line 3"),
        "history corruption must identify its physical line: {error:#}"
    );
    assert_eq!(fs::read_to_string(store.db_path())?, content);
    Ok(())
}

#[test]
fn test_review_fails_closed_on_truncated_db_tail() -> Result<()> {
    let repo = approved_repo("truncated_db_tail")?;
    let store = FileStore::for_root(&repo.path)?;
    let mut content = fs::read_to_string(store.db_path())?;
    // Invalid JSON has no decoded declaration shape, but is still authoritative
    // history corruption rather than an ignorable interrupted write.
    content.push_str("{\"verdict\":\"rejected\",\"id\":\"torn\"");
    fs::write(store.db_path(), &content)?;

    let stderr = repo.run_err(&["review", "--all", "--json"])?;
    assert!(
        stderr.contains("line 2"),
        "the truncated tail must identify its physical line: {stderr}"
    );
    let error = store
        .read_history()
        .expect_err("syntactically malformed JSON must fail history loading");
    assert!(
        error.to_string().contains("line 2"),
        "the truncated tail must identify its physical line: {error:#}"
    );
    assert_eq!(fs::read_to_string(store.db_path())?, content);
    Ok(())
}
