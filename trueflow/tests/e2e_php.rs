use anyhow::{Context, Result};
use std::path::PathBuf;

use trueflow_test_support::*;

use trueflow::analysis::Language;
use trueflow::block::{Block, BlockKind};
use trueflow::block_splitter;
use trueflow::review_units::MAX_REVIEW_UNIT_SPAN_LINES;
use trueflow::sub_splitter::{self, SubSplitSemantics};

fn expand_block_for_review_splitting(mut block: Block) -> Block {
    block.end_line = block.start_line + MAX_REVIEW_UNIT_SPAN_LINES + 8;
    block
}

#[test]
fn test_php_fixture_scan_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("php_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let php_file = files
        .iter()
        .find(|file| {
            file["path"].as_str().map(|path| path.replace("./", ""))
                == Some("src/App.php".to_string())
        })
        .context("missing scan output for src/App.php")?;

    assert_eq!(php_file["language"].as_str(), Some("Php"));

    let blocks = php_file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = blocks
        .iter()
        .filter_map(|block| block.get("kind").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();

    for expected_kind in [
        "module",
        "interface",
        "impl",
        "enum",
        "class",
        "function",
        "method",
    ] {
        assert!(
            kinds.contains(&expected_kind),
            "expected {expected_kind} block in php fixture (kinds={kinds:?})"
        );
    }
    assert!(
        kinds.contains(&"import") || kinds.contains(&"Imports"),
        "expected import-like php block in fixture (kinds={kinds:?})"
    );

    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect paragraph fallback blocks in php fixture (kinds={kinds:?})"
    );
    let report_builder = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("class")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("final class ReportBuilder"))
        })
        .context("missing ReportBuilder class block")?;
    assert_eq!(report_builder["complexity"].as_u64(), Some(4));

    let process_data = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("method")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("function processData(array $values)"))
        })
        .context("missing processData method block")?;
    assert_eq!(process_data["complexity"].as_u64(), Some(3));

    Ok(())
}

#[test]
fn test_php_fixture_marks_obvious_test_blocks() -> Result<()> {
    let repo = TestRepo::fixture("php_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let php_file = files
        .iter()
        .find(|file| {
            file["path"].as_str().map(|path| path.replace("./", ""))
                == Some("src/App.php".to_string())
        })
        .context("missing scan output for src/App.php")?;

    let blocks = php_file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let tagged = blocks
        .iter()
        .filter(|block| {
            block
                .get("tags")
                .and_then(|value| value.as_array())
                .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test")))
        })
        .collect::<Vec<_>>();

    assert!(!tagged.is_empty(), "expected at least one php test block");
    assert!(
        tagged.iter().any(|block| {
            block["content"].as_str().is_some_and(|content| {
                content.contains("testFormatsRecords") || content.contains("test_standalone_helper")
            })
        }),
        "expected tagged php test block to include an obvious test declaration: {tagged:#?}"
    );

    Ok(())
}

#[test]
fn test_php_fixture_sub_splitting_supports_functions_and_types() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/php_support/src/App.php");

    let content = std::fs::read_to_string(&file_path)?;
    let blocks = block_splitter::split(&content, Language::Php).blocks;

    let method_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Method && block.content.contains("function processData")
            })
            .cloned()
            .context("expected php processData method block")?,
    );

    let method_result = sub_splitter::split_result(&method_block, Language::Php)?;
    assert_eq!(method_result.semantics, SubSplitSemantics::ReviewUnits);
    let method_kinds = method_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        method_kinds,
        vec![
            BlockKind::FunctionSignature,
            BlockKind::CodeParagraph,
            BlockKind::CodeParagraph,
            BlockKind::Comment,
            BlockKind::CodeParagraph,
            BlockKind::CodeParagraph,
        ]
    );

    let class_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Class && block.content.contains("class ReportBuilder")
            })
            .cloned()
            .context("expected php ReportBuilder class block")?,
    );

    let class_result = sub_splitter::split_result(&class_block, Language::Php)?;
    assert_eq!(class_result.semantics, SubSplitSemantics::ReviewUnits);
    let class_kinds = class_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert!(
        class_kinds.contains(&BlockKind::Const),
        "expected php class sub-blocks to expose const members: {class_kinds:?}"
    );
    assert!(
        class_kinds.contains(&BlockKind::Variable),
        "expected php class sub-blocks to expose property members: {class_kinds:?}"
    );
    assert!(
        class_kinds
            .iter()
            .filter(|kind| **kind == BlockKind::Method)
            .count()
            >= 3,
        "expected php class sub-blocks to expose methods: {class_kinds:?}"
    );

    Ok(())
}
