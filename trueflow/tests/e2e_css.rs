use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;

use trueflow_test_support::*;

use trueflow::analysis::Language;
use trueflow::block::{Block, BlockKind};
use trueflow::block_splitter::{self, BlockSplitStrategy};
use trueflow::review_units::MAX_REVIEW_UNIT_SPAN_LINES;
use trueflow::sub_splitter::{self, SubSplitSemantics};

fn expand_block_for_review_splitting(mut block: Block) -> Block {
    block.end_line = block.start_line + MAX_REVIEW_UNIT_SPAN_LINES + 8;
    block
}

fn find_file<'a>(files: &'a [Value], path: &str) -> Result<&'a Value> {
    files
        .iter()
        .find(|file| file["path"].as_str() == Some(path))
        .with_context(|| format!("missing scan output for {path}"))
}

fn find_block<'a>(blocks: &'a [Value], needle: &str) -> Result<&'a Value> {
    blocks
        .iter()
        .find(|block| {
            block["content"]
                .as_str()
                .is_some_and(|content| content.contains(needle))
        })
        .with_context(|| format!("missing block containing {needle:?}"))
}

#[test]
fn test_css_fixture_scan_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("css_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let css_file = find_file(&files, "site.css")?;

    assert_eq!(css_file["language"].as_str(), Some("Css"));

    let blocks = css_file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = block_kinds_without_gaps(blocks);

    assert!(
        kinds.contains(&"import") || kinds.contains(&"Imports"),
        "kinds={kinds:?}"
    );
    assert!(kinds.contains(&"Section"), "kinds={kinds:?}");
    assert!(kinds.contains(&"Element"), "kinds={kinds:?}");
    assert!(!kinds.contains(&"Paragraph"), "kinds={kinds:?}");
    assert!(!kinds.contains(&"code"), "kinds={kinds:?}");

    assert!(
        matches!(
            find_block(blocks, "@import url(\"./print.css\");")?["kind"].as_str(),
            Some("import" | "Imports")
        ),
        "expected import-like block"
    );
    assert_eq!(
        find_block(blocks, ":root {")?["kind"].as_str(),
        Some("Section")
    );
    assert_eq!(
        find_block(blocks, "body,\nmain.dashboard {")?["kind"].as_str(),
        Some("Section")
    );
    assert_eq!(
        find_block(blocks, "@media screen and (min-width: 48rem) {")?["kind"].as_str(),
        Some("Section")
    );
    assert_eq!(
        find_block(blocks, "@layer components {")?["kind"].as_str(),
        Some("Section")
    );
    assert_eq!(
        find_block(blocks, "@tailwind utilities;")?["kind"].as_str(),
        Some("Element")
    );

    Ok(())
}

#[test]
fn test_css_ruleset_and_at_rule_sub_split_return_structural_review_units() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/css_support/site.css");
    let content = std::fs::read_to_string(&file_path)?;

    let split = block_splitter::split(&content, Language::Css);
    assert_eq!(split.strategy, BlockSplitStrategy::Structured);
    let blocks = split.blocks;

    let ruleset_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Section
                    && block.content.contains("body,\nmain.dashboard {")
            })
            .cloned()
            .context("expected css ruleset section block")?,
    );
    let ruleset_result = sub_splitter::split_result(&ruleset_block, Language::Css)?;
    assert_eq!(ruleset_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        ruleset_result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        ruleset_block.content
    );
    let ruleset_kinds = ruleset_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        ruleset_kinds.first().copied(),
        Some(BlockKind::FunctionSignature)
    );
    assert!(
        ruleset_kinds.contains(&BlockKind::CodeParagraph),
        "ruleset_kinds={ruleset_kinds:?}"
    );
    assert!(
        !ruleset_kinds.contains(&BlockKind::Paragraph),
        "ruleset_kinds={ruleset_kinds:?}"
    );

    let media_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Section
                    && block
                        .content
                        .contains("@media screen and (min-width: 48rem) {")
            })
            .cloned()
            .context("expected css media section block")?,
    );
    let media_result = sub_splitter::split_result(&media_block, Language::Css)?;
    assert_eq!(media_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        media_result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        media_block.content
    );
    let media_kinds = media_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        media_kinds.first().copied(),
        Some(BlockKind::FunctionSignature)
    );
    assert!(
        media_kinds.contains(&BlockKind::Section),
        "media_kinds={media_kinds:?}"
    );
    assert!(
        !media_kinds.contains(&BlockKind::CodeParagraph),
        "expected nested css sections instead of generic code paragraphs for media blocks: {media_kinds:?}"
    );

    let layer_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Section && block.content.contains("@layer components {")
            })
            .cloned()
            .context("expected css layer section block")?,
    );
    let layer_result = sub_splitter::split_result(&layer_block, Language::Css)?;
    assert_eq!(layer_result.semantics, SubSplitSemantics::ReviewUnits);
    let layer_kinds = layer_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        layer_kinds.first().copied(),
        Some(BlockKind::FunctionSignature)
    );
    assert!(
        layer_kinds.contains(&BlockKind::Section),
        "layer_kinds={layer_kinds:?}"
    );

    let keyframes_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Section && block.content.contains("@keyframes pulse {")
            })
            .cloned()
            .context("expected css keyframes section block")?,
    );
    let keyframes_result = sub_splitter::split_result(&keyframes_block, Language::Css)?;
    assert_eq!(keyframes_result.semantics, SubSplitSemantics::ReviewUnits);
    let keyframes_kinds = keyframes_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        keyframes_kinds.first().copied(),
        Some(BlockKind::FunctionSignature)
    );
    assert!(
        keyframes_kinds.contains(&BlockKind::Element),
        "keyframes_kinds={keyframes_kinds:?}"
    );

    Ok(())
}
