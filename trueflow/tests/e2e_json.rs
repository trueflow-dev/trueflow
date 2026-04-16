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
fn test_json_fixture_scan_detects_stable_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("json_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let workflow = files
        .iter()
        .find(|file| file["path"].as_str() == Some("workflow.json"))
        .context("missing scan output for workflow.json")?;
    assert_eq!(workflow["language"].as_str(), Some("Json"));

    let workflow_blocks = workflow["blocks"]
        .as_array()
        .context("workflow blocks should be array")?;
    let workflow_kinds = block_kinds_without_gaps(workflow_blocks);

    assert!(
        workflow_blocks.iter().any(|block| {
            block["kind"].as_str() == Some("Content")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("\"version\": 1"))
        }),
        "expected stable scalar pair block: {workflow_blocks:#?}"
    );
    assert!(
        workflow_blocks.iter().any(|block| {
            block["kind"].as_str() == Some("Section")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("\"metadata\": {"))
        }),
        "expected nested object section block: {workflow_blocks:#?}"
    );
    assert!(
        workflow_blocks.iter().any(|block| {
            block["kind"].as_str() == Some("List")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("\"pipelines\": ["))
        }),
        "expected list block: {workflow_blocks:#?}"
    );
    assert!(
        !workflow_kinds.contains(&"code"),
        "did not expect generic code fallback blocks: {workflow_kinds:?}"
    );
    assert!(
        !workflow_kinds.contains(&"Paragraph"),
        "did not expect textual fallback blocks: {workflow_kinds:?}"
    );

    let queue = files
        .iter()
        .find(|file| file["path"].as_str() == Some("queue.json"))
        .context("missing scan output for queue.json")?;
    let queue_blocks = queue["blocks"]
        .as_array()
        .context("queue blocks should be array")?;
    let queue_kinds = block_kinds_without_gaps(queue_blocks);

    assert_eq!(queue["language"].as_str(), Some("Json"));
    assert!(
        queue_kinds
            .iter()
            .filter(|kind| **kind == "Section")
            .count()
            >= 2,
        "expected root array objects to produce section blocks: {queue_kinds:?}"
    );
    assert!(
        !queue_kinds.contains(&"Paragraph"),
        "did not expect textual fallback blocks for queue.json: {queue_kinds:?}"
    );

    Ok(())
}

#[test]
fn test_json_sub_block_review_prefers_container_children_over_scalar_fragments() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/json_support/workflow.json");
    let content = std::fs::read_to_string(&file_path)?;

    let split = block_splitter::split(&content, Language::Json);
    assert_eq!(split.strategy, BlockSplitStrategy::Structured);
    let blocks = split.blocks;

    let metadata_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Section && block.content.contains("\"metadata\": {")
            })
            .cloned()
            .context("expected metadata section block")?,
    );
    let metadata_result = sub_splitter::split_result(&metadata_block, Language::Json)?;
    assert_eq!(
        metadata_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let metadata_children = metadata_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        metadata_children.iter().any(|block| {
            block.kind == BlockKind::Content && block.content.contains("\"owner\": \"platform\"")
        }),
        "metadata children={metadata_children:#?}"
    );
    assert!(
        metadata_children.iter().any(|block| {
            block.kind == BlockKind::Section && block.content.contains("\"labels\": {")
        }),
        "metadata children={metadata_children:#?}"
    );
    assert!(
        metadata_children.iter().all(|block| {
            !matches!(
                block.kind,
                BlockKind::Paragraph | BlockKind::Sentence | BlockKind::CodeParagraph
            )
        }),
        "did not expect paragraph-like scalar fragmentation: {metadata_children:#?}"
    );

    let pipelines_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::List && block.content.contains("\"pipelines\": [")
            })
            .cloned()
            .context("expected pipelines list block")?,
    );
    let pipelines_result = sub_splitter::split_result(&pipelines_block, Language::Json)?;
    assert_eq!(
        pipelines_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let pipeline_items = pipelines_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        pipeline_items
            .iter()
            .filter(|block| block.kind == BlockKind::Section)
            .count()
            >= 2,
        "expected object list items to stay whole: {pipeline_items:#?}"
    );
    assert!(
        pipeline_items.iter().all(|block| {
            !matches!(
                block.kind,
                BlockKind::Paragraph | BlockKind::Sentence | BlockKind::CodeParagraph
            )
        }),
        "did not expect textual/code fallback units in list children: {pipeline_items:#?}"
    );

    let alerts_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| block.kind == BlockKind::List && block.content.contains("\"alerts\": ["))
            .cloned()
            .context("expected alerts list block")?,
    );
    let alerts_result = sub_splitter::split_result(&alerts_block, Language::Json)?;
    assert_eq!(
        alerts_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let alert_items = alerts_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        alert_items.iter().any(|block| {
            block.kind == BlockKind::Content && block.content.contains("\"pagerduty\"")
        }),
        "alert items={alert_items:#?}"
    );
    assert!(
        alert_items.iter().any(|block| {
            block.kind == BlockKind::Content && block.content.contains("\"slack\"")
        }),
        "alert items={alert_items:#?}"
    );

    Ok(())
}
