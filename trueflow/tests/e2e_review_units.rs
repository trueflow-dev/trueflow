use anyhow::{Context, Result};

use trueflow_test_support::*;

#[test]
fn test_small_text_file_stays_one_review_block() -> Result<()> {
    let repo = TestRepo::new("small_text_file_stays_one_review_block")?;
    let content = "first paragraph\n\nsecond paragraph\n";
    repo.write("notes.txt", content)?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let file = files.first().context("expected scanned file")?;
    let blocks = file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let non_gap_blocks: Vec<_> = blocks
        .iter()
        .filter(|block| block["kind"].as_str().is_some_and(|kind| !is_gap(kind)))
        .collect();

    assert_eq!(non_gap_blocks.len(), 1);
    assert_eq!(non_gap_blocks[0]["kind"].as_str(), Some("Paragraph"));
    assert_eq!(non_gap_blocks[0]["content"].as_str(), Some(content));

    Ok(())
}

#[test]
fn shell_review_units_are_identical_in_worktree_and_revision() -> Result<()> {
    let repo = TestRepo::new("shell_snapshot_parity")?;
    repo.write(
        "runner",
        "#!/bin/sh\n\nhello() {\n    echo hello\n}\n\nhello\n",
    )?;
    repo.commit_all("shell script without extension")?;
    let worktree = json_array(&repo.run(&["review", "--all", "--json"])?)?;
    let revision = json_array(&repo.run(&["review", "--target", "rev:HEAD", "--json"])?)?;
    assert_eq!(worktree[0]["language"], "Shell");
    assert!(
        worktree[0]["blocks"]
            .as_array()
            .context("blocks")?
            .iter()
            .any(|block| block["kind"] == "function")
    );
    assert_eq!(revision, worktree);
    Ok(())
}

#[test]
fn revision_review_preserves_source_parser_degradation_diagnostics() -> Result<()> {
    let repo = TestRepo::new("parser_diagnostic_parity")?;
    repo.write("blank.toml", "  \n")?;
    repo.commit_all("source without semantic blocks")?;
    let worktree = repo.run_raw(&["review", "--all", "--json"])?;
    let revision = repo.run_raw(&["review", "--target", "rev:HEAD", "--json"])?;
    assert!(worktree.status.success());
    assert!(revision.status.success());
    let warning = String::from_utf8(worktree.stderr)?;
    assert!(
        warning.contains("blank.toml") && warning.contains("fallback"),
        "{warning}"
    );
    assert_eq!(String::from_utf8(revision.stderr)?, warning);
    assert_eq!(
        json_array(&String::from_utf8(revision.stdout)?)?,
        json_array(&String::from_utf8(worktree.stdout)?)?,
    );
    Ok(())
}
