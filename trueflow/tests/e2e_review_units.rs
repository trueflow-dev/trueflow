use anyhow::{Context, Result};

mod common;
use common::*;

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
