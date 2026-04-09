use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;

mod common;
use common::*;

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
fn test_html_fixture_scan_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("html_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let html_file = find_file(&files, "index.html")?;

    assert_eq!(html_file["language"].as_str(), Some("Html"));

    let blocks = html_file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = block_kinds_without_gaps(blocks);

    assert!(kinds.contains(&"Preamble"), "kinds={kinds:?}");
    assert!(kinds.contains(&"Section"), "kinds={kinds:?}");
    assert!(kinds.contains(&"Element"), "kinds={kinds:?}");
    assert!(!kinds.contains(&"Paragraph"), "kinds={kinds:?}");
    assert!(!kinds.contains(&"code"), "kinds={kinds:?}");

    assert_eq!(
        find_block(blocks, "<!DOCTYPE html>")?["kind"].as_str(),
        Some("Preamble")
    );
    assert_eq!(
        find_block(blocks, "<head>")?["kind"].as_str(),
        Some("Section")
    );
    assert_eq!(
        find_block(blocks, "<body>")?["kind"].as_str(),
        Some("Section")
    );
    assert_eq!(
        find_block(blocks, "<main id=\"overview\">")?["kind"].as_str(),
        Some("Section")
    );
    assert_eq!(
        find_block(blocks, "<article id=\"activity\">")?["kind"].as_str(),
        Some("Section")
    );
    let style_block = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("Element")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(".hero { color: rebeccapurple; }"))
        })
        .context("missing dedicated style element block")?;
    assert_eq!(style_block["kind"].as_str(), Some("Element"));

    Ok(())
}

#[test]
fn test_html_section_sub_split_returns_structural_review_units() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/html_support/index.html");
    let content = std::fs::read_to_string(&file_path)?;

    let split = block_splitter::split(&content, Language::Html);
    assert_eq!(split.strategy, BlockSplitStrategy::Structured);
    let blocks = split.blocks;

    let body_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Section && block.content.trim_start().starts_with("<body>")
            })
            .cloned()
            .context("expected html body section block")?,
    );
    let result = sub_splitter::split_result(&body_block, Language::Html)?;
    assert_eq!(result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        body_block.content
    );

    let kinds = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert!(kinds.contains(&BlockKind::Section), "kinds={kinds:?}");
    assert!(kinds.contains(&BlockKind::Content), "kinds={kinds:?}");
    assert!(
        !kinds.contains(&BlockKind::CodeParagraph),
        "expected html structural review units instead of generic code paragraphs: {kinds:?}"
    );
    assert!(
        result.blocks.iter().any(|block| {
            block.kind == BlockKind::Section && block.content.contains("<main id=\"overview\">")
        }),
        "result={:#?}",
        result.blocks
    );
    assert!(
        result.blocks.iter().any(|block| {
            block.kind == BlockKind::Section && block.content.contains("<footer>")
        }),
        "result={:#?}",
        result.blocks
    );

    let head_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Section && block.content.trim_start().starts_with("<head>")
            })
            .cloned()
            .context("expected html head section block")?,
    );
    let head_result = sub_splitter::split_result(&head_block, Language::Html)?;
    assert_eq!(head_result.semantics, SubSplitSemantics::ReviewUnits);
    assert!(
        head_result.blocks.iter().any(|block| {
            block.kind == BlockKind::Element
                && block
                    .content
                    .contains("<style>\n      .hero { color: rebeccapurple; }\n    </style>")
        }),
        "head_result={:#?}",
        head_result.blocks
    );

    Ok(())
}
