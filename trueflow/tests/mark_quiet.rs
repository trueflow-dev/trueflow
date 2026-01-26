use anyhow::Result;

mod common;
use common::{TestRepo, first_block_hash, json_array};

#[test]
fn test_mark_quiet_suppresses_output() -> Result<()> {
    let repo = TestRepo::new("mark_quiet")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    let mark_output = repo.run_raw(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;
    assert!(mark_output.status.success(), "Expected mark to succeed");
    let stdout = String::from_utf8(mark_output.stdout)?;
    assert!(stdout.trim().is_empty(), "Expected no stdout for --quiet");

    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;
    assert!(files.is_empty(), "Approved block should be filtered out");

    Ok(())
}

#[test]
fn test_mark_default_is_silent() -> Result<()> {
    let repo = TestRepo::new("mark_default_silent")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    let mark_output = repo.run_raw(&["mark", "--fingerprint", &hash, "--verdict", "approved"])?;
    assert!(mark_output.status.success(), "Expected mark to succeed");
    let stdout = String::from_utf8(mark_output.stdout)?;
    assert!(stdout.trim().is_empty(), "Expected no stdout by default");

    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;
    assert!(files.is_empty(), "Approved block should be filtered out");

    Ok(())
}
