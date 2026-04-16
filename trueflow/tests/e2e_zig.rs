use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;

use trueflow_test_support::*;

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
fn test_zig_fixture_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("zig_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let main = files
        .iter()
        .find(|file| path_matches(file, "src/main.zig"))
        .context("missing scan output for src/main.zig")?;
    assert_eq!(main["language"].as_str(), Some("Zig"));

    let blocks = main["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = non_gap_kinds(blocks);

    assert!(
        kinds.contains(&"import") || kinds.contains(&"Imports"),
        "expected Zig import blocks: {kinds:?}"
    );
    for expected in [
        "const", "variable", "enum", "type", "struct", "function", "method",
    ] {
        assert!(
            kinds.contains(&expected),
            "expected Zig {expected} block in fixture (kinds={kinds:?})"
        );
    }
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect text fallback blocks (kinds={kinds:?})"
    );

    assert!(
        blocks.iter().any(|block| {
            block["kind"].as_str() == Some("function")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("test \"helper increments\""))
                && block["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test")))
        }),
        "expected top-level Zig test block to be tagged: {blocks:#?}"
    );
    assert!(
        blocks.iter().any(|block| {
            block["kind"].as_str() == Some("function")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("test \"add updates total\""))
                && block["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test")))
        }),
        "expected nested Zig test block to be tagged: {blocks:#?}"
    );

    Ok(())
}

#[test]
fn test_zig_type_sub_split_returns_structural_children_matching_top_level_members() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/zig_support/src/main.zig");

    let content = std::fs::read_to_string(&file_path)?;
    let blocks = block_splitter::split(&content, Language::Zig).blocks;

    let accumulator = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Struct && block.content.contains("Accumulator = struct")
        })
        .cloned()
        .context("expected Zig Accumulator struct block")?;

    let top_level_members: Vec<_> = blocks
        .iter()
        .filter(|block| {
            block.hash != accumulator.hash
                && block.start_line >= accumulator.start_line
                && block.end_line <= accumulator.end_line
                && matches!(
                    block.kind,
                    BlockKind::Variable
                        | BlockKind::Const
                        | BlockKind::Struct
                        | BlockKind::Enum
                        | BlockKind::Type
                        | BlockKind::Import
                        | BlockKind::Method
                        | BlockKind::Function
                )
        })
        .map(|block| {
            (
                block.kind,
                block.hash.clone(),
                block.content.clone(),
                block.tags.clone(),
                block.start_line,
                block.end_line,
            )
        })
        .collect::<Vec<_>>();

    let result = sub_splitter::split_result(
        &expand_block_for_review_splitting(accumulator),
        Language::Zig,
    )?;
    assert_eq!(result.semantics, SubSplitSemantics::StructuralChildren);

    let children: Vec<_> = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| {
            (
                block.kind,
                block.hash.clone(),
                block.content.clone(),
                block.tags.clone(),
                block.start_line,
                block.end_line,
            )
        })
        .collect();

    assert_eq!(children, top_level_members);
    assert!(
        children.iter().any(|(kind, _, content, _, _, _)| {
            *kind == BlockKind::Struct && content.contains("Snapshot = struct")
        }),
        "expected nested stable container child: {children:#?}"
    );
    assert!(
        children.iter().any(|(kind, _, _, tags, _, _)| {
            *kind == BlockKind::Function && tags.iter().any(|tag| tag == "test")
        }),
        "expected nested Zig test child in struct split: {children:#?}"
    );

    Ok(())
}

#[test]
fn test_zig_function_like_sub_splits_cover_methods_and_tests() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/zig_support/src/main.zig");

    let content = std::fs::read_to_string(&file_path)?;
    let blocks = block_splitter::split(&content, Language::Zig).blocks;

    let add_method = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| block.kind == BlockKind::Method && block.content.contains("pub fn add"))
            .cloned()
            .context("expected Zig add method block")?,
    );
    let add_result = sub_splitter::split_result(&add_method, Language::Zig)?;
    assert_eq!(add_result.semantics, SubSplitSemantics::ReviewUnits);
    let add_kinds = add_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        add_kinds,
        vec![
            BlockKind::FunctionSignature,
            BlockKind::CodeParagraph,
            BlockKind::Comment,
            BlockKind::CodeParagraph,
        ]
    );

    let top_level_test = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Function
                    && block.has_tag("test")
                    && block.content.contains("test \"helper increments\"")
            })
            .cloned()
            .context("expected top-level Zig test block")?,
    );
    let test_result = sub_splitter::split_result(&top_level_test, Language::Zig)?;
    assert_eq!(test_result.semantics, SubSplitSemantics::ReviewUnits);
    let test_kinds = test_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        test_kinds,
        vec![BlockKind::FunctionSignature, BlockKind::CodeParagraph]
    );

    Ok(())
}
