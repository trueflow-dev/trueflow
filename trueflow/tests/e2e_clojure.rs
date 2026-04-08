use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;

mod common;
use common::*;

use trueflow::analysis::Language;
use trueflow::block::{Block, BlockKind};
use trueflow::block_splitter;
use trueflow::review_units::MAX_REVIEW_UNIT_SPAN_LINES;
use trueflow::sub_splitter::{self, SubSplitSemantics};

fn non_gap_kinds(blocks: &[Value]) -> Vec<&str> {
    blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .filter(|kind| !is_gap(kind))
        .collect()
}

fn path_matches(file: &Value, expected: &str) -> bool {
    file["path"]
        .as_str()
        .is_some_and(|path| path.trim_start_matches("./") == expected)
}

fn expand_block_for_review_splitting(mut block: Block) -> Block {
    block.end_line = block.start_line + MAX_REVIEW_UNIT_SPAN_LINES + 8;
    block
}

#[test]
fn test_clojure_fixture_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("clojure_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let lib_file = files
        .iter()
        .find(|file| path_matches(file, "src/demo/core.clj"))
        .context("missing scan output for src/demo/core.clj")?;
    assert_eq!(lib_file["language"].as_str(), Some("Clojure"));

    let lib_blocks = lib_file["blocks"]
        .as_array()
        .context("lib blocks should be array")?;
    let lib_kinds = non_gap_kinds(lib_blocks);

    for expected_kind in [
        "module",
        "import",
        "variable",
        "function",
        "macro",
        "method",
        "interface",
        "struct",
        "type",
    ] {
        assert!(
            lib_kinds.contains(&expected_kind),
            "expected {expected_kind} block in Clojure fixture (kinds={lib_kinds:?})"
        );
    }
    assert!(
        lib_kinds.contains(&"FunctionSignature"),
        "expected protocol method signature blocks in Clojure fixture (kinds={lib_kinds:?})"
    );
    assert!(
        !lib_kinds.contains(&"code"),
        "did not expect generic code blocks in Clojure fixture (kinds={lib_kinds:?})"
    );
    assert!(
        !lib_kinds.contains(&"Paragraph"),
        "did not expect text fallback blocks in Clojure fixture (kinds={lib_kinds:?})"
    );

    let test_file = files
        .iter()
        .find(|file| path_matches(file, "test/demo/core_test.clj"))
        .context("missing scan output for test/demo/core_test.clj")?;
    assert_eq!(test_file["language"].as_str(), Some("Clojure"));

    let test_blocks = test_file["blocks"]
        .as_array()
        .context("test blocks should be array")?;
    let deftest_block = test_blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("function")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("(deftest normalize-test"))
        })
        .context("missing deftest block")?;
    assert!(
        deftest_block["tags"]
            .as_array()
            .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test"))),
        "expected deftest block to be test-tagged: {deftest_block:#?}"
    );

    let helper_block = test_blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("function")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("(defn helper-value"))
        })
        .context("missing helper function block")?;
    assert!(
        !helper_block["tags"]
            .as_array()
            .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test"))),
        "did not expect helper function to be test-tagged: {helper_block:#?}"
    );

    Ok(())
}

#[test]
fn test_clojure_sub_block_review_support_for_function_and_container_forms() -> Result<()> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("example_repos/clojure_support/src/demo/core.clj");
    let content = std::fs::read_to_string(&fixture)?;
    let blocks = block_splitter::split(&content, Language::Clojure).blocks;

    let normalize_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Function && block.content.contains("(defn normalize")
            })
            .cloned()
            .context("missing normalize function block")?,
    );
    let normalize_result = sub_splitter::split_result(&normalize_block, Language::Clojure)?;
    let normalize_kinds = normalize_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();

    assert_eq!(normalize_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        normalize_kinds.first().copied(),
        Some(BlockKind::FunctionSignature)
    );
    assert_eq!(
        normalize_result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        normalize_block.content
    );
    assert!(
        normalize_kinds.contains(&BlockKind::Comment),
        "expected comment-preserving Clojure function review units: {normalize_kinds:?}"
    );
    assert!(
        normalize_kinds.contains(&BlockKind::CodeParagraph),
        "expected code paragraphs in Clojure function split: {normalize_kinds:?}"
    );
    assert!(
        normalize_result
            .blocks
            .iter()
            .all(|block| block.complexity.is_none()),
        "expected Clojure sub-block complexity to remain unset"
    );

    let ns_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Module && block.content.contains("(ns demo.core")
            })
            .cloned()
            .context("missing ns module block")?,
    );
    let ns_result = sub_splitter::split_result(&ns_block, Language::Clojure)?;
    let ns_kinds = ns_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();

    assert_eq!(ns_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        ns_result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        ns_block.content
    );
    assert!(ns_kinds.contains(&BlockKind::Import), "kinds={ns_kinds:?}");
    assert!(ns_kinds.contains(&BlockKind::Module), "kinds={ns_kinds:?}");
    assert!(
        !ns_kinds.contains(&BlockKind::Paragraph),
        "did not expect text fallback sub-blocks: {ns_kinds:?}"
    );

    let record_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Struct && block.content.contains("(defrecord User")
            })
            .cloned()
            .context("missing defrecord block")?,
    );
    let record_result = sub_splitter::split_result(&record_block, Language::Clojure)?;
    let record_kinds = record_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();

    assert_eq!(record_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        record_result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        record_block.content
    );
    assert!(
        record_kinds.contains(&BlockKind::Struct),
        "kinds={record_kinds:?}"
    );
    assert!(
        record_kinds.contains(&BlockKind::Method),
        "kinds={record_kinds:?}"
    );

    Ok(())
}
