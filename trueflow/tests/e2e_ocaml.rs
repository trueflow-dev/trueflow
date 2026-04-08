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
fn test_ocaml_fixture_scan_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("ocaml_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let ml_file = find_file(&files, "lib/demo.ml")?;
    let mli_file = find_file(&files, "lib/demo.mli")?;

    assert_eq!(ml_file["language"].as_str(), Some("OCaml"));
    assert_eq!(mli_file["language"].as_str(), Some("OCaml"));

    let ml_blocks = ml_file["blocks"]
        .as_array()
        .context("ml blocks should be array")?;
    let ml_kinds = block_kinds_without_gaps(ml_blocks);

    assert!(
        matches!(
            find_block(ml_blocks, "open Core")?["kind"].as_str(),
            Some("import" | "Imports")
        ),
        "expected open to stay import-like"
    );
    assert!(
        matches!(
            find_block(ml_blocks, "include Shared")?["kind"].as_str(),
            Some("import" | "Imports")
        ),
        "expected include to stay import-like"
    );
    assert_eq!(
        find_block(ml_blocks, "module type WORKER = sig")?["kind"].as_str(),
        Some("interface")
    );
    assert_eq!(
        find_block(ml_blocks, "module Helpers = struct")?["kind"].as_str(),
        Some("module")
    );
    assert_eq!(
        find_block(ml_blocks, "type job = {")?["kind"].as_str(),
        Some("struct")
    );
    assert_eq!(
        find_block(ml_blocks, "type mode =")?["kind"].as_str(),
        Some("enum")
    );
    assert_eq!(
        find_block(ml_blocks, "exception Invalid_job")?["kind"].as_str(),
        Some("type")
    );
    assert_eq!(
        find_block(ml_blocks, "external render")?["kind"].as_str(),
        Some("FunctionSignature")
    );
    assert_eq!(
        find_block(ml_blocks, "let default_name = \"worker\"")?["kind"].as_str(),
        Some("const")
    );
    assert_eq!(
        find_block(ml_blocks, "let run values =")?["kind"].as_str(),
        Some("function")
    );
    assert!(ml_kinds.contains(&"function"), "ml_kinds={ml_kinds:?}");
    assert!(!ml_kinds.contains(&"Paragraph"), "ml_kinds={ml_kinds:?}");

    let mli_blocks = mli_file["blocks"]
        .as_array()
        .context("mli blocks should be array")?;
    let mli_kinds = block_kinds_without_gaps(mli_blocks);
    assert_eq!(
        find_block(mli_blocks, "module type WORKER = sig")?["kind"].as_str(),
        Some("interface")
    );
    assert_eq!(
        find_block(mli_blocks, "module Helpers : sig")?["kind"].as_str(),
        Some("module")
    );
    assert_eq!(
        find_block(mli_blocks, "val render : int -> string")?["kind"].as_str(),
        Some("FunctionSignature")
    );
    assert_eq!(
        find_block(mli_blocks, "val default_name : string")?["kind"].as_str(),
        Some("const")
    );
    assert!(!mli_kinds.contains(&"Paragraph"), "mli_kinds={mli_kinds:?}");

    Ok(())
}

#[test]
fn test_ocaml_function_and_module_sub_blocks_are_reviewable() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/ocaml_support/lib/demo.ml");
    let content = std::fs::read_to_string(&file_path)?;

    let split = block_splitter::split(&content, Language::OCaml);
    assert_eq!(split.strategy, BlockSplitStrategy::Structured);
    let blocks = split.blocks;

    let function_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Function && block.content.contains("let build value =")
            })
            .cloned()
            .context("expected OCaml build function block")?,
    );
    let function_result = sub_splitter::split_result(&function_block, Language::OCaml)?;
    assert_eq!(function_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        function_result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        function_block.content
    );
    let function_kinds = function_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        function_kinds.first().copied(),
        Some(BlockKind::FunctionSignature)
    );
    assert!(
        function_kinds.contains(&BlockKind::Comment),
        "function_kinds={function_kinds:?}"
    );
    assert!(
        function_kinds
            .iter()
            .filter(|kind| **kind == BlockKind::CodeParagraph)
            .count()
            >= 2,
        "function_kinds={function_kinds:?}"
    );
    assert!(
        function_result
            .blocks
            .iter()
            .all(|block| block.complexity.is_none()),
        "expected OCaml sub-block complexity to remain unset"
    );

    let module_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Module && block.content.contains("module Helpers = struct")
            })
            .cloned()
            .context("expected OCaml Helpers module block")?,
    );
    let module_result = sub_splitter::split_result(&module_block, Language::OCaml)?;
    assert_eq!(module_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        module_result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        module_block.content
    );
    let module_kinds = module_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    for expected in [
        BlockKind::Struct,
        BlockKind::Enum,
        BlockKind::Const,
        BlockKind::Module,
        BlockKind::Function,
    ] {
        assert!(
            module_kinds.contains(&expected),
            "module_kinds={module_kinds:?}"
        );
    }
    assert!(
        !module_kinds.contains(&BlockKind::Paragraph),
        "module_kinds={module_kinds:?}"
    );

    Ok(())
}

#[test]
fn test_ocaml_signature_and_type_sub_blocks_are_stable() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ml_path = repo_root.join("example_repos/ocaml_support/lib/demo.ml");
    let ml_content = std::fs::read_to_string(&ml_path)?;
    let ml_blocks = block_splitter::split(&ml_content, Language::OCaml).blocks;

    let signature_block = expand_block_for_review_splitting(
        ml_blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Interface
                    && block.content.contains("module type WORKER = sig")
            })
            .cloned()
            .context("expected OCaml module type block")?,
    );
    let signature_result = sub_splitter::split_result(&signature_block, Language::OCaml)?;
    assert_eq!(signature_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        signature_result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        signature_block.content
    );
    let signature_kinds = signature_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert!(
        signature_kinds.contains(&BlockKind::FunctionSignature),
        "signature_kinds={signature_kinds:?}"
    );
    assert!(
        signature_kinds.contains(&BlockKind::Enum),
        "signature_kinds={signature_kinds:?}"
    );

    let record_block = expand_block_for_review_splitting(
        ml_blocks
            .iter()
            .find(|block| block.kind == BlockKind::Struct && block.content.contains("type job = {"))
            .cloned()
            .context("expected OCaml record type block")?,
    );
    let record_result = sub_splitter::split_result(&record_block, Language::OCaml)?;
    assert_eq!(record_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        record_result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        record_block.content
    );
    let record_kinds = record_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert!(
        record_kinds.contains(&BlockKind::Variable),
        "record_kinds={record_kinds:?}"
    );

    let variant_block = expand_block_for_review_splitting(
        ml_blocks
            .iter()
            .find(|block| block.kind == BlockKind::Enum && block.content.contains("type mode ="))
            .cloned()
            .context("expected OCaml variant type block")?,
    );
    let variant_result = sub_splitter::split_result(&variant_block, Language::OCaml)?;
    assert_eq!(variant_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        variant_result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        variant_block.content
    );
    let variant_kinds = variant_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert!(
        variant_kinds.contains(&BlockKind::Enum),
        "variant_kinds={variant_kinds:?}"
    );

    Ok(())
}
