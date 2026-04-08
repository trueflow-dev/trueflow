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
fn test_dart_fixture_scan_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("dart_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let lib_file = find_file(&files, "lib/app.dart")?;
    let test_file = find_file(&files, "test/app_test.dart")?;

    assert_eq!(lib_file["language"].as_str(), Some("Dart"));
    assert_eq!(test_file["language"].as_str(), Some("Dart"));

    let blocks = lib_file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = block_kinds_without_gaps(blocks);

    for expected_kind in [
        "module", "const", "variable", "type", "function", "class", "impl", "enum", "method",
    ] {
        assert!(kinds.contains(&expected_kind), "kinds={kinds:?}");
    }
    assert!(
        kinds.contains(&"import") || kinds.contains(&"Imports"),
        "kinds={kinds:?}"
    );
    assert!(!kinds.contains(&"code"), "kinds={kinds:?}");
    assert!(!kinds.contains(&"Paragraph"), "kinds={kinds:?}");

    assert_eq!(
        find_block(blocks, "typedef NameFormatter")?["kind"].as_str(),
        Some("type")
    );
    assert_eq!(
        find_block(blocks, "String summarize")?["kind"].as_str(),
        Some("function")
    );
    assert_eq!(
        find_block(blocks, "mixin CounterSupport")?["kind"].as_str(),
        Some("impl")
    );
    assert_eq!(
        find_block(blocks, "class Worker with CounterSupport")?["kind"].as_str(),
        Some("class")
    );
    assert_eq!(
        find_block(blocks, "extension WorkerTools on Worker")?["kind"].as_str(),
        Some("impl")
    );
    assert_eq!(
        find_block(blocks, "enum Mode")?["kind"].as_str(),
        Some("enum")
    );
    assert!(
        blocks.iter().any(|block| {
            block["kind"].as_str() == Some("const")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("static const version"))
        }),
        "missing const field block"
    );
    assert!(
        blocks.iter().any(|block| {
            block["kind"].as_str() == Some("variable")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("int jobs = 0;"))
        }),
        "missing variable field block"
    );
    assert!(
        blocks.iter().any(|block| {
            block["kind"].as_str() == Some("method")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("void process(List<int> values)"))
        }),
        "missing method block"
    );

    Ok(())
}

#[test]
fn test_dart_fixture_marks_obvious_test_groups_and_cases() -> Result<()> {
    let repo = TestRepo::fixture("dart_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let test_file = find_file(&files, "test/app_test.dart")?;
    let blocks = test_file["blocks"]
        .as_array()
        .context("blocks should be array")?;

    for needle in [
        "group('Worker'",
        "test('processes positive values'",
        "test('reset keeps worker ready'",
        "void testStandaloneSummary()",
    ] {
        let block = blocks
            .iter()
            .find(|block| {
                block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(needle))
                    && block["tags"]
                        .as_array()
                        .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test")))
            })
            .with_context(|| format!("missing tagged test block containing {needle:?}"))?;
        let tags = block["tags"].as_array().context("tags should be array")?;
        assert!(
            tags.iter().any(|tag| tag.as_str() == Some("test")),
            "expected test tag for {needle}: {block:#?}"
        );
    }

    Ok(())
}

#[test]
fn test_dart_sub_block_review_supports_functions_and_types() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/dart_support/lib/app.dart");
    let content = std::fs::read_to_string(&file_path)?;

    let split = block_splitter::split(&content, Language::Dart);
    assert_eq!(split.strategy, BlockSplitStrategy::Structured);
    let blocks = split.blocks;

    let function_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Function && block.content.contains("String summarize")
            })
            .cloned()
            .context("expected dart summarize function block")?,
    );
    let function_result = sub_splitter::split_result(&function_block, Language::Dart)?;
    assert_eq!(function_result.semantics, SubSplitSemantics::ReviewUnits);
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
        function_kinds.contains(&BlockKind::CodeParagraph),
        "function_kinds={function_kinds:?}"
    );

    let class_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Class
                    && block.content.contains("class Worker with CounterSupport")
            })
            .cloned()
            .context("expected dart Worker class block")?,
    );
    let class_result = sub_splitter::split_result(&class_block, Language::Dart)?;
    assert_eq!(class_result.semantics, SubSplitSemantics::ReviewUnits);
    let class_kinds = class_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert!(
        class_kinds.contains(&BlockKind::Const),
        "class_kinds={class_kinds:?}"
    );
    assert!(
        class_kinds.contains(&BlockKind::Variable),
        "class_kinds={class_kinds:?}"
    );
    assert!(
        class_kinds.contains(&BlockKind::Method),
        "class_kinds={class_kinds:?}"
    );

    Ok(())
}
