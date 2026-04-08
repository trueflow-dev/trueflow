use anyhow::{Context, Result};
use std::path::PathBuf;

mod common;
use common::*;

use trueflow::analysis::Language;
use trueflow::block::{Block, BlockKind};
use trueflow::block_splitter;
use trueflow::review_units::MAX_REVIEW_UNIT_SPAN_LINES;
use trueflow::sub_splitter::{self, SubSplitSemantics};

fn non_gap_kinds(blocks: &[serde_json::Value]) -> Vec<&str> {
    blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .filter(|kind| !is_gap(kind))
        .collect()
}

fn path_matches(file: &serde_json::Value, expected: &str) -> bool {
    file["path"]
        .as_str()
        .is_some_and(|path| path.trim_start_matches("./") == expected)
}

fn expand_block_for_review_splitting(mut block: Block) -> Block {
    block.end_line = block.start_line + MAX_REVIEW_UNIT_SPAN_LINES + 8;
    block
}

#[test]
fn test_haskell_fixture_detects_language_structural_blocks_and_test_tags() -> Result<()> {
    let repo = TestRepo::fixture("haskell_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let worker = files
        .iter()
        .find(|file| path_matches(file, "src/Demo/Worker.hs"))
        .context("missing scan output for src/Demo/Worker.hs")?;
    assert_eq!(worker["language"].as_str(), Some("Haskell"));

    let worker_blocks = worker["blocks"]
        .as_array()
        .context("worker blocks should be array")?;
    let worker_kinds = non_gap_kinds(worker_blocks);

    assert!(worker_kinds.contains(&"module"), "kinds={worker_kinds:?}");
    assert!(
        worker_kinds.contains(&"import") || worker_kinds.contains(&"Imports"),
        "kinds={worker_kinds:?}"
    );
    assert!(worker_kinds.contains(&"class"), "kinds={worker_kinds:?}");
    assert!(worker_kinds.contains(&"impl"), "kinds={worker_kinds:?}");
    assert!(worker_kinds.contains(&"function"), "kinds={worker_kinds:?}");
    assert!(
        worker_blocks.iter().any(|block| {
            block["kind"].as_str() == Some("type")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("data Mode"))
        }),
        "expected data declaration block: {worker_blocks:#?}"
    );
    assert!(
        worker_blocks.iter().any(|block| {
            block["kind"].as_str() == Some("type")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("newtype WorkerId"))
        }),
        "expected newtype declaration block: {worker_blocks:#?}"
    );
    assert!(
        worker_blocks.iter().any(|block| {
            block["kind"].as_str() == Some("type")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("type Rendered = String"))
        }),
        "expected type synonym block: {worker_blocks:#?}"
    );
    assert!(
        !worker_kinds.contains(&"Paragraph"),
        "did not expect text fallback blocks: {worker_kinds:?}"
    );

    let spec = files
        .iter()
        .find(|file| path_matches(file, "test/Spec.hs"))
        .context("missing scan output for test/Spec.hs")?;
    assert_eq!(spec["language"].as_str(), Some("Haskell"));

    let spec_blocks = spec["blocks"]
        .as_array()
        .context("spec blocks should be array")?;

    for expected in [
        "spec :: Spec",
        "testFormatWorker :: IO ()",
        "prop_workerIdRoundTrip",
    ] {
        let block = spec_blocks
            .iter()
            .find(|block| {
                block["kind"].as_str() == Some("function")
                    && block["content"]
                        .as_str()
                        .is_some_and(|content| content.contains(expected))
            })
            .with_context(|| format!("missing Haskell function block for {expected}"))?;
        assert!(
            block["tags"]
                .as_array()
                .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test"))),
            "expected {expected} block to be test-tagged: {block:#?}"
        );
    }

    let helper = spec_blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("function")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("helper :: Int -> Int"))
        })
        .context("missing helper function block")?;
    assert!(
        !helper["tags"]
            .as_array()
            .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test"))),
        "did not expect helper to be test-tagged: {helper:#?}"
    );

    Ok(())
}

#[test]
fn test_haskell_function_like_sub_split_returns_review_units() -> Result<()> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("example_repos/haskell_support/src/Demo/Worker.hs");
    let content = std::fs::read_to_string(&fixture)?;
    let blocks = block_splitter::split(&content, Language::Haskell).blocks;

    let function_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Function
                    && block
                        .content
                        .contains("formatWorker :: WorkerId -> [Int] -> Rendered")
            })
            .cloned()
            .context("missing Haskell formatWorker block")?,
    );
    let result = sub_splitter::split_result(&function_block, Language::Haskell)?;
    let kinds = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();

    assert_eq!(result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(kinds.first().copied(), Some(BlockKind::FunctionSignature));
    assert!(kinds.contains(&BlockKind::Comment), "kinds={kinds:?}");
    assert!(
        kinds
            .iter()
            .filter(|kind| **kind == BlockKind::CodeParagraph)
            .count()
            >= 2,
        "expected code paragraphs in Haskell function split: {kinds:?}"
    );
    assert_eq!(
        result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        function_block.content
    );
    assert!(
        result.blocks.iter().all(|block| block.complexity.is_none()),
        "expected Haskell sub-block complexity to remain unset"
    );

    Ok(())
}

#[test]
fn test_haskell_declaration_groups_split_into_structural_children() -> Result<()> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("example_repos/haskell_support/src/Demo/Worker.hs");
    let content = std::fs::read_to_string(&fixture)?;
    let blocks = block_splitter::split(&content, Language::Haskell).blocks;

    let class_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Class && block.content.contains("class Renderable a where")
            })
            .cloned()
            .context("missing Haskell class block")?,
    );
    let class_result = sub_splitter::split_result(&class_block, Language::Haskell)?;
    let class_kinds = class_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();

    assert_eq!(
        class_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    assert!(
        class_kinds.contains(&BlockKind::Function),
        "expected grouped class member function child for recursive review splitting: {class_kinds:?}"
    );
    assert!(
        !class_kinds.contains(&BlockKind::CodeParagraph),
        "did not expect structural-child scaffolding blocks: {class_kinds:?}"
    );

    let instance_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Impl
                    && block.content.contains("instance Renderable WorkerId where")
            })
            .cloned()
            .context("missing Haskell instance block")?,
    );
    let instance_result = sub_splitter::split_result(&instance_block, Language::Haskell)?;
    let instance_kinds = instance_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();

    assert_eq!(
        instance_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    assert!(
        instance_kinds.contains(&BlockKind::Function),
        "kinds={instance_kinds:?}"
    );
    assert!(
        !instance_kinds.contains(&BlockKind::CodeParagraph),
        "did not expect structural-child scaffolding blocks: {instance_kinds:?}"
    );

    Ok(())
}
