use anyhow::{Context, Result};
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

#[test]
fn test_yaml_fixture_scan_detects_stable_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("yaml_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let workflow = files
        .iter()
        .find(|file| file["path"].as_str() == Some("workflow.yaml"))
        .context("missing scan output for workflow.yaml")?;
    assert_eq!(workflow["language"].as_str(), Some("Yaml"));

    let workflow_blocks = workflow["blocks"]
        .as_array()
        .context("workflow blocks should be array")?;
    let workflow_kinds = block_kinds_without_gaps(workflow_blocks);

    assert!(
        workflow_blocks.iter().any(|block| {
            block["kind"].as_str() == Some("Content")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("service: billing"))
        }),
        "expected stable scalar mapping entry: {workflow_blocks:#?}"
    );
    assert!(
        workflow_blocks.iter().any(|block| {
            block["kind"].as_str() == Some("Section")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("metadata:"))
        }),
        "expected nested mapping section block: {workflow_blocks:#?}"
    );
    assert!(
        workflow_blocks.iter().any(|block| {
            block["kind"].as_str() == Some("List")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("pipelines:"))
        }),
        "expected sequence block: {workflow_blocks:#?}"
    );
    assert!(
        !workflow_kinds.contains(&"code"),
        "did not expect generic code fallback blocks: {workflow_kinds:?}"
    );
    assert!(
        !workflow_kinds.contains(&"Paragraph"),
        "did not expect textual fallback blocks: {workflow_kinds:?}"
    );

    let workers = files
        .iter()
        .find(|file| file["path"].as_str() == Some("workers.yml"))
        .context("missing scan output for workers.yml")?;
    let worker_blocks = workers["blocks"]
        .as_array()
        .context("workers blocks should be array")?;
    let worker_kinds = block_kinds_without_gaps(worker_blocks);

    assert_eq!(workers["language"].as_str(), Some("Yaml"));
    assert!(
        worker_kinds
            .iter()
            .filter(|kind| **kind == "Section")
            .count()
            >= 2,
        "expected root sequence mappings to produce section blocks: {worker_kinds:?}"
    );
    assert!(
        !worker_kinds.contains(&"Paragraph"),
        "did not expect textual fallback blocks for workers.yml: {worker_kinds:?}"
    );

    Ok(())
}

#[test]
fn test_yaml_sub_block_review_prefers_container_children_over_scalar_fragments() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/yaml_support/workflow.yaml");
    let content = std::fs::read_to_string(&file_path)?;

    let split = block_splitter::split(&content, Language::Yaml);
    assert_eq!(split.strategy, BlockSplitStrategy::Structured);
    let blocks = split.blocks;

    let metadata_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| block.kind == BlockKind::Section && block.content.contains("metadata:"))
            .cloned()
            .context("expected metadata section block")?,
    );
    let metadata_result = sub_splitter::split_result(&metadata_block, Language::Yaml)?;
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
            block.kind == BlockKind::Content && block.content.contains("owner: platform")
        }),
        "metadata children={metadata_children:#?}"
    );
    assert!(
        metadata_children
            .iter()
            .any(|block| { block.kind == BlockKind::Section && block.content.contains("labels:") }),
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
            .find(|block| block.kind == BlockKind::List && block.content.contains("pipelines:"))
            .cloned()
            .context("expected pipelines list block")?,
    );
    let pipelines_result = sub_splitter::split_result(&pipelines_block, Language::Yaml)?;
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
        "expected sequence mappings to stay whole: {pipeline_items:#?}"
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

    let labels_block = expand_block_for_review_splitting(
        metadata_children
            .iter()
            .find(|block| block.kind == BlockKind::Section && block.content.contains("labels:"))
            .map(|block| (*block).clone())
            .context("expected labels child section block")?,
    );
    let labels_result = sub_splitter::split_result(&labels_block, Language::Yaml)?;
    assert_eq!(
        labels_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let label_children = labels_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .collect::<Vec<_>>();
    assert!(
        label_children.iter().any(|block| {
            block.kind == BlockKind::Content && block.content.contains("tier: backend")
        }),
        "label children={label_children:#?}"
    );
    assert!(
        label_children.iter().any(|block| {
            block.kind == BlockKind::Content && block.content.contains("oncall: billing")
        }),
        "label children={label_children:#?}"
    );

    Ok(())
}
