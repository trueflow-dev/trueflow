use anyhow::{Context, Result};
use serde_json::Value;

mod common;
use common::{TestRepo, json, json_array};

fn tree_contains_hash(node: &Value, target: &str) -> bool {
    if node.get("hash").and_then(|value| value.as_str()) == Some(target) {
        return true;
    }

    let Some(children) = node.get("children").and_then(|value| value.as_array()) else {
        return false;
    };

    children
        .iter()
        .any(|child| tree_contains_hash(child, target))
}

#[test]
fn test_scan_tree_contains_parent_block_hash() -> Result<()> {
    let repo = TestRepo::new("tree_parent_blocks")?;
    repo.write(
        "src/main.rs",
        "fn main() {\n    let value = 1;\n\n    if value > 0 {\n        println!(\"{}\", value);\n    }\n}\n",
    )?;

    let scan_output = repo.run(&["scan", "--json"])?;
    let files = json_array(&scan_output)?;
    let file = files.first().context("expected scan output file")?;
    let blocks = file["blocks"].as_array().context("expected blocks array")?;
    let function_block = blocks
        .iter()
        .find(|block| block["kind"].as_str() == Some("function"))
        .context("expected a function block")?;
    let block_hash = function_block["hash"].as_str().context("expected hash")?;

    let tree_output = repo.run(&["scan", "--json", "--tree"])?;
    let tree = json(&tree_output)?;

    assert!(
        tree_contains_hash(&tree, block_hash),
        "expected tree to contain parent block hash"
    );

    Ok(())
}
