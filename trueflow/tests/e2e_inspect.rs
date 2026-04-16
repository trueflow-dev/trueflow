use anyhow::{Context, Result};
use serde_json::Value;

use trueflow_test_support::*;

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

#[test]
fn test_inspect_coverage_reports_direct_and_effective_review_facts() -> Result<()> {
    let repo = TestRepo::new("inspect_coverage")?;
    repo.write("src/lib.rs", "pub fn core() {}\n")?;
    repo.commit_all("Add lib")?;

    let scan_output = repo.run(&["scan", "--json"])?;
    let files = json_array(&scan_output)?;
    let file_hash = first_file_tree_hash(&scan_output)?;
    let file = files.first().context("expected file")?;
    let block = file["blocks"].as_array().context("blocks")?[0].clone();
    let block_hash = block["hash"].as_str().context("block hash")?;
    let start_line = block["start_line"]
        .as_u64()
        .context("block start line")?
        .to_string();

    repo.run(&[
        "mark",
        "--fingerprint",
        &file_hash,
        "--verdict",
        "approved",
        "--path",
        "src/lib.rs",
        "--quiet",
    ])?;
    repo.run(&[
        "mark",
        "--fingerprint",
        block_hash,
        "--verdict",
        "approved",
        "--check",
        "security",
        "--path",
        "src/lib.rs",
        "--line",
        &start_line,
        "--quiet",
    ])?;

    let output = repo.run(&["inspect", "--fingerprint", block_hash, "--coverage"])?;
    let inspected: Value = serde_json::from_str(&output)?;

    assert_eq!(inspected["block"]["hash"].as_str(), Some(block_hash));
    assert_eq!(
        inspected["coverage"]["checks"]["review"]["direct_latest_verdict"],
        Value::Null
    );
    assert_eq!(
        inspected["coverage"]["checks"]["review"]["effective_latest_verdict"].as_str(),
        Some("approved")
    );
    assert_eq!(
        inspected["coverage"]["checks"]["review"]["direct_identity_count"].as_u64(),
        Some(0)
    );
    assert_eq!(
        inspected["coverage"]["checks"]["review"]["effective_identity_count"].as_u64(),
        Some(1)
    );
    assert_eq!(
        inspected["coverage"]["checks"]["security"]["direct_latest_verdict"].as_str(),
        Some("approved")
    );
    assert_eq!(
        inspected["coverage"]["checks"]["security"]["effective_latest_verdict"].as_str(),
        Some("approved")
    );
    assert_eq!(
        inspected["coverage"]["policies"]["single_review_effective"].as_bool(),
        Some(true)
    );
    assert_eq!(
        inspected["coverage"]["policies"]["single_review_direct"].as_bool(),
        Some(false)
    );

    Ok(())
}
