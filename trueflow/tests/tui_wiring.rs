use anyhow::{Context, Result};

mod common;
use common::{TestRepo, first_file_blocks};

#[test]
fn test_review_json_output_filters_marked_blocks() -> Result<()> {
    let fixture = TestRepo::fixture("empty")?;

    fixture.write("src/main.rs", "fn main() {}\n\nfn helper() {}\n")?;
    fixture.add("src/main.rs")?;
    fixture.commit("Add main")?;

    fixture.git(&["checkout", "-B", "main"])?;
    let output = fixture.run(&["review", "--all", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    assert!(blocks.len() >= 2, "Expected multiple blocks");

    // Approve the first block and ensure it disappears
    let first_hash = blocks[0]["hash"].as_str().context("hash")?;
    fixture.run(&[
        "mark",
        "--fingerprint",
        first_hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;

    let output = fixture.run(&["review", "--all", "--json"])?;
    let blocks_after = first_file_blocks(&output)?;
    assert!(
        !blocks_after.iter().any(|block| block["hash"] == first_hash),
        "Approved block should be filtered out"
    );

    Ok(())
}
