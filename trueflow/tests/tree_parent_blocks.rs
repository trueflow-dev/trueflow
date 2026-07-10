use anyhow::{Context, Result};
use serde_json::Value;

use trueflow_test_support::{TestRepo, json, json_array};

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

fn tree_node_with_span(node: &Value, start_byte: usize, end_byte: usize) -> Option<&Value> {
    if node["start_byte"].as_u64() == u64::try_from(start_byte).ok()
        && node["end_byte"].as_u64() == u64::try_from(end_byte).ok()
    {
        return Some(node);
    }
    node["children"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|child| tree_node_with_span(child, start_byte, end_byte))
}

fn child_has_span(node: &Value, start_byte: usize, end_byte: usize) -> bool {
    node["children"].as_array().is_some_and(|children| {
        children.iter().any(|child| {
            child["start_byte"].as_u64() == u64::try_from(start_byte).ok()
                && child["end_byte"].as_u64() == u64::try_from(end_byte).ok()
        })
    })
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

#[test]
fn test_scan_tree_keeps_identical_rust_blocks_on_same_line() -> Result<()> {
    let repo = TestRepo::new("tree_same_line_rust")?;
    let source = "const _: () = (); const _: () = (); const _: () = ();\n";
    repo.write("src/lib.rs", source)?;

    let scan_output = repo.run(&["scan", "--json"])?;
    let files = json_array(&scan_output)?;
    let blocks = files[0]["blocks"]
        .as_array()
        .context("expected scanned blocks")?
        .iter()
        .filter(|block| block["kind"].as_str() == Some("const"))
        .collect::<Vec<_>>();
    assert_eq!(blocks.len(), 3);

    let expected_spans = [(0, 17), (18, 35), (36, 53)];
    let hashes = blocks
        .iter()
        .map(|block| block["hash"].as_str().context("expected block hash"))
        .collect::<Result<Vec<_>>>()?;
    assert!(hashes.windows(2).all(|pair| pair[0] == pair[1]));
    for (block, (start_byte, end_byte)) in blocks.iter().zip(expected_spans) {
        assert_eq!(block["start_line"].as_u64(), Some(0));
        assert_eq!(block["end_line"].as_u64(), Some(1));
        assert_eq!(block["start_byte"].as_u64(), u64::try_from(start_byte).ok());
        assert_eq!(block["end_byte"].as_u64(), u64::try_from(end_byte).ok());
    }

    let tree_output = repo.run(&["scan", "--json", "--tree"])?;
    let tree = json(&tree_output)?;
    let distinct_nodes = expected_spans
        .into_iter()
        .map(|(start_byte, end_byte)| {
            tree_node_with_span(&tree, start_byte, end_byte)
                .context("expected byte-distinct tree node")
        })
        .collect::<Result<Vec<_>>>()?;
    assert!(
        distinct_nodes
            .windows(2)
            .all(|pair| !std::ptr::eq(pair[0], pair[1]))
    );

    Ok(())
}

#[test]
fn test_scan_tree_keeps_same_line_java_siblings_under_outer_class() -> Result<()> {
    let repo = TestRepo::new("tree_same_line_java")?;
    let source = "class Outer { class A {} class B {} void x() {} void y() {} }";
    repo.write("src/Main.java", source)?;

    let tree_output = repo.run(&["scan", "--json", "--tree"])?;
    let tree = json(&tree_output)?;
    let outer = tree_node_with_span(&tree, 0, source.len()).context("expected Outer node")?;

    for member in ["class A {}", "class B {}", "void x() {}", "void y() {}"] {
        let start = source
            .find(member)
            .with_context(|| format!("expected member source {member}"))?;
        assert!(
            child_has_span(outer, start, start + member.len()),
            "expected {member} to be a direct child of Outer"
        );
    }

    let a_start = source.find("class A {}").context("expected A")?;
    let b_start = source.find("class B {}").context("expected B")?;
    let a = tree_node_with_span(&tree, a_start, a_start + "class A {}".len())
        .context("expected A node")?;
    let b = tree_node_with_span(&tree, b_start, b_start + "class B {}".len())
        .context("expected B node")?;
    assert!(!child_has_span(a, b_start, b_start + "class B {}".len()));
    assert!(!child_has_span(b, a_start, a_start + "class A {}".len()));

    Ok(())
}
