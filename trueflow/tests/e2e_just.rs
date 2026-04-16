use anyhow::{Context, Result};

use trueflow_test_support::*;

#[test]
fn test_small_justfile_stays_one_review_block() -> Result<()> {
    let repo = TestRepo::new("small_justfile_stays_one_review_block")?;
    let content = "build:\n    echo build\n\nlint:\n    echo lint\n";
    repo.write("Justfile", content)?;

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

    assert_eq!(
        non_gap_blocks.len(),
        1,
        "expected a small Justfile to stay as one review block, got: {blocks:#?}"
    );
    assert_eq!(
        non_gap_blocks[0]["kind"].as_str(),
        Some("CodeParagraph"),
        "expected code-oriented block kind for small Justfile"
    );
    assert_eq!(
        non_gap_blocks[0]["content"].as_str(),
        Some(content),
        "expected merged block content to preserve the full file"
    );

    Ok(())
}
