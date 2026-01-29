use anyhow::{Context, Result};
use serde_json::Value;

mod common;
use common::*;

fn scan_blocks(repo: &TestRepo) -> Result<Vec<Value>> {
    let output = repo.run(&["scan", "--json"])?;
    json_array(&output)
}

#[test]
fn test_inspect_errors_on_missing_block() -> Result<()> {
    let repo = TestRepo::new("inspect_missing")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;

    let output = repo.run_raw(&["inspect", "--fingerprint", "deadbeef"])?;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Block not found"));

    Ok(())
}

#[test]
fn test_inspect_errors_on_duplicate_fingerprint() -> Result<()> {
    let repo = TestRepo::new("inspect_duplicate")?;
    repo.write("src/lib.rs", "pub fn alpha() {}\n\npub fn alpha() {}\n")?;

    let blocks = scan_blocks(&repo)?;
    let hash = blocks[0]["blocks"][0]["hash"].as_str().context("hash")?;

    let output = repo.run_raw(&["inspect", "--fingerprint", hash])?;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Multiple blocks matched"));

    Ok(())
}

#[test]
fn test_inspect_split_preserves_order() -> Result<()> {
    let repo = TestRepo::new("inspect_split")?;
    let content = "fn main() {\n    part1();\n\n    part2();\n}\n";
    repo.write("src/main.rs", content)?;

    let blocks = scan_blocks(&repo)?;
    let hash = blocks[0]["blocks"][0]["hash"].as_str().context("hash")?;

    let output = repo.run(&["inspect", "--fingerprint", hash, "--split"])?;
    let sub_blocks: Vec<Value> = serde_json::from_str(&output)?;
    let reconstructed: String = sub_blocks
        .iter()
        .filter_map(|block| block["content"].as_str())
        .collect();

    assert_eq!(
        reconstructed.trim_end_matches('\n'),
        content.trim_end_matches('\n')
    );

    Ok(())
}
