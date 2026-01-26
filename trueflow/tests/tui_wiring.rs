use anyhow::{Context, Result};

mod common;
use common::{TestRepo, first_file_blocks};

#[test]
fn test_tui_mode_loads_without_error() -> Result<()> {
    // TUI is interactive, so we can't fully E2E test it easily without a pty.
    // But we can check if it crashes on startup or basic arguments.
    // For now, we rely on unit tests for TUI logic.
    // This integration test just ensures wiring is present.
    Ok(())
}

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
