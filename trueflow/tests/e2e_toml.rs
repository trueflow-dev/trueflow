use anyhow::{Context, Result};
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

#[test]
fn test_toml_fixture_scan_detects_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("toml_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let config = files
        .iter()
        .find(|file| file["path"].as_str() == Some("config.toml"))
        .context("missing scan output for config.toml")?;
    assert_eq!(config["language"].as_str(), Some("Toml"));

    let blocks = config["blocks"]
        .as_array()
        .context("config blocks should be array")?;
    let kinds = block_kinds_without_gaps(blocks);

    assert!(
        blocks.iter().any(|block| {
            block["kind"].as_str() == Some("Content")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("title = \"deploy\""))
        }),
        "expected scalar key/value block: {blocks:#?}"
    );
    assert!(
        blocks.iter().any(|block| {
            block["kind"].as_str() == Some("List")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("keywords = ["))
        }),
        "expected array-valued list block: {blocks:#?}"
    );
    assert!(
        blocks.iter().any(|block| {
            block["kind"].as_str() == Some("Section")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("[database]"))
        }),
        "expected explicit table section block: {blocks:#?}"
    );
    assert!(
        kinds.iter().filter(|kind| **kind == "Section").count() >= 4,
        "expected table + array-of-table sections: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect textual fallback blocks: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"CodeParagraph"),
        "did not expect code fallback blocks: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"code"),
        "did not expect generic code fallback blocks: {kinds:?}"
    );

    Ok(())
}

#[test]
fn test_toml_sub_block_review_prefers_structural_children() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/toml_support/config.toml");
    let content = std::fs::read_to_string(&file_path)?;

    let split = block_splitter::split(&content, Language::Toml);
    assert_eq!(split.strategy, BlockSplitStrategy::Structured);
    let blocks = split.blocks;

    let database_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| block.kind == BlockKind::Section && block.content.contains("[database]"))
            .cloned()
            .context("expected database section block")?,
    );
    let database_result = sub_splitter::split_result(&database_block, Language::Toml)?;
    assert_eq!(
        database_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let database_children = database_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        database_children
            .iter()
            .any(|block| block.kind == BlockKind::List && block.content.contains("ports = [")),
        "database children={database_children:#?}"
    );
    assert!(
        database_children.iter().any(|block| {
            block.kind == BlockKind::Section && block.content.contains("targets = {")
        }),
        "database children={database_children:#?}"
    );

    let targets_block = expand_block_for_review_splitting(
        database_children
            .iter()
            .find(|block| block.kind == BlockKind::Section && block.content.contains("targets = {"))
            .map(|block| (*block).clone())
            .context("expected inline-table section block")?,
    );
    let targets_result = sub_splitter::split_result(&targets_block, Language::Toml)?;
    assert_eq!(
        targets_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let target_children = targets_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        target_children.iter().any(|block| {
            block.kind == BlockKind::Content && block.content.contains("primary = \"cache\"")
        }),
        "target children={target_children:#?}"
    );
    assert!(
        target_children.iter().any(|block| {
            block.kind == BlockKind::Content && block.content.contains("secondary = \"backup\"")
        }),
        "target children={target_children:#?}"
    );

    let keywords_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| block.kind == BlockKind::List && block.content.contains("keywords = ["))
            .cloned()
            .context("expected keywords list block")?,
    );
    let keywords_result = sub_splitter::split_result(&keywords_block, Language::Toml)?;
    assert_eq!(
        keywords_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let keyword_children = keywords_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        keyword_children.iter().any(|block| {
            block.kind == BlockKind::Content && block.content.contains("\"blue\"")
        }),
        "keyword children={keyword_children:#?}"
    );
    assert!(
        keyword_children.iter().any(|block| {
            block.kind == BlockKind::Content && block.content.contains("\"green\"")
        }),
        "keyword children={keyword_children:#?}"
    );
    assert!(
        keyword_children.iter().all(|block| {
            !matches!(
                block.kind,
                BlockKind::Paragraph | BlockKind::Sentence | BlockKind::CodeParagraph
            )
        }),
        "did not expect textual/code fallback review units: {keyword_children:#?}"
    );

    Ok(())
}
