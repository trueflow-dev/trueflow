use anyhow::{Context, Result};
use std::path::PathBuf;

mod common;
use common::*;
use trueflow::analysis::Language;
use trueflow::block::BlockKind;
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

fn expand_block_for_review_splitting(mut block: trueflow::block::Block) -> trueflow::block::Block {
    block.end_line = block.start_line + MAX_REVIEW_UNIT_SPAN_LINES + 8;
    block
}

#[test]
fn test_elixir_fixture_detects_structural_blocks_and_test_tags() -> Result<()> {
    let repo = TestRepo::fixture("elixir_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let lib_file = files
        .iter()
        .find(|file| path_matches(file, "lib/demo.ex"))
        .context("missing scan output for lib/demo.ex")?;
    assert_eq!(lib_file["language"].as_str(), Some("Elixir"));

    let lib_blocks = lib_file["blocks"]
        .as_array()
        .context("lib blocks should be array")?;
    let lib_kinds = non_gap_kinds(lib_blocks);

    assert!(
        lib_kinds.contains(&"import") || lib_kinds.contains(&"Imports"),
        "expected alias/import/use block: {lib_kinds:?}"
    );
    assert!(
        lib_kinds.contains(&"module"),
        "expected module block: {lib_kinds:?}"
    );
    assert!(
        lib_kinds.contains(&"interface"),
        "expected defprotocol block: {lib_kinds:?}"
    );
    assert!(
        lib_kinds.contains(&"impl"),
        "expected defimpl block: {lib_kinds:?}"
    );
    assert!(
        lib_kinds.contains(&"function"),
        "expected function block: {lib_kinds:?}"
    );
    assert!(
        lib_kinds.contains(&"macro"),
        "expected macro block: {lib_kinds:?}"
    );
    assert!(
        !lib_kinds.contains(&"Paragraph"),
        "did not expect textual fallback blocks: {lib_kinds:?}"
    );

    let test_file = files
        .iter()
        .find(|file| path_matches(file, "test/demo_test.exs"))
        .context("missing scan output for test/demo_test.exs")?;
    assert_eq!(test_file["language"].as_str(), Some("Elixir"));

    let test_blocks = test_file["blocks"]
        .as_array()
        .context("test blocks should be array")?;

    let describe_block = test_blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("module")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("describe \"run/1\" do"))
        })
        .context("missing describe block")?;
    assert!(
        describe_block["tags"]
            .as_array()
            .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test"))),
        "expected describe block to be test-tagged: {describe_block:#?}"
    );

    let test_block = test_blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("function")
                && block["content"].as_str().is_some_and(|content| {
                    content.contains("test \"keeps positive doubled values\" do")
                })
        })
        .context("missing ExUnit test block")?;
    assert!(
        test_block["tags"]
            .as_array()
            .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test"))),
        "expected ExUnit test block to be test-tagged: {test_block:#?}"
    );

    let helper_block = test_blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("function")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("defp helper(value) do"))
        })
        .context("missing helper function block")?;
    assert!(
        !helper_block["tags"]
            .as_array()
            .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test"))),
        "did not expect helper function to be test-tagged: {helper_block:#?}"
    );

    Ok(())
}

#[test]
fn test_elixir_does_not_treat_library_test_prefix_functions_as_exunit_tests() -> Result<()> {
    let content =
        "defmodule Demo.Worker do\n  def test_connection(opts) do\n    opts\n  end\nend\n";
    let blocks = block_splitter::split(content, Language::Elixir).blocks;
    let block = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Function
                && block.content.contains("def test_connection(opts) do")
        })
        .context("missing test_connection function block")?;

    assert!(
        !block.has_tag("test"),
        "did not expect a normal library function with a test_ prefix to be tagged: {block:#?}"
    );

    Ok(())
}

#[test]
fn test_elixir_does_not_tag_custom_non_exunit_test_macros() -> Result<()> {
    let content = "defmodule Demo.CustomDsl do\n  test \"custom\" do\n    :ok\n  end\nend\n";
    let blocks = block_splitter::split(content, Language::Elixir).blocks;
    let block = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Function && block.content.contains("test \"custom\" do")
        })
        .context("missing custom test macro block")?;

    assert!(
        !block.has_tag("test"),
        "did not expect a non-ExUnit test macro call to be tagged: {block:#?}"
    );

    Ok(())
}

#[test]
fn test_elixir_does_not_attach_non_attribute_unary_expressions_to_following_blocks() -> Result<()> {
    let content = "defmodule Demo.Worker do\n  -1\n\n  def run(value) do\n    value\n  end\nend\n";
    let blocks = block_splitter::split(content, Language::Elixir).blocks;
    let function_block = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Function && block.content.contains("def run(value) do")
        })
        .context("missing run function block")?;

    assert!(
        !function_block.content.trim_start().starts_with("-1"),
        "did not expect a non-attribute unary expression to be attached to the function block: {function_block:#?}"
    );

    Ok(())
}

#[test]
fn test_elixir_test_module_sub_split_preserves_content_without_descendant_overlap() -> Result<()> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("example_repos/elixir_support/test/demo_test.exs");
    let content = std::fs::read_to_string(&fixture)?;
    let blocks = block_splitter::split(&content, Language::Elixir).blocks;
    let module_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Module
                    && block.content.contains("defmodule Demo.WorkerTest do")
            })
            .cloned()
            .context("missing Demo.WorkerTest module block")?,
    );

    let result = sub_splitter::split_result(&module_block, Language::Elixir)?;
    let kinds = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();

    assert_eq!(
        result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        module_block.content
    );
    assert!(kinds.contains(&BlockKind::Import), "kinds={kinds:?}");
    assert!(kinds.contains(&BlockKind::Module), "kinds={kinds:?}");
    assert!(kinds.contains(&BlockKind::Function), "kinds={kinds:?}");

    Ok(())
}

#[test]
fn test_elixir_sub_block_support_for_function_and_module_blocks() -> Result<()> {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("example_repos/elixir_support/lib/demo.ex");
    let content = std::fs::read_to_string(&fixture)?;
    let blocks = block_splitter::split(&content, Language::Elixir).blocks;

    let function_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Function && block.content.contains("def run(values) do")
            })
            .cloned()
            .context("missing Demo.Worker.run block")?,
    );
    let function_result = sub_splitter::split_result(&function_block, Language::Elixir)?;
    let function_kinds = function_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();

    assert_eq!(function_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        function_kinds.first().copied(),
        Some(BlockKind::FunctionSignature)
    );
    assert_eq!(
        function_result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        function_block.content
    );
    assert!(
        function_kinds.contains(&BlockKind::Comment),
        "expected comment-preserving function review units: {function_kinds:?}"
    );
    assert!(
        function_kinds
            .iter()
            .filter(|kind| **kind == BlockKind::CodeParagraph)
            .count()
            >= 2,
        "expected code paragraphs in function split: {function_kinds:?}"
    );
    assert!(
        function_result
            .blocks
            .iter()
            .all(|block| block.complexity.is_none()),
        "expected Elixir sub-block complexity to remain unset"
    );

    let module_block = expand_block_for_review_splitting(
        blocks
            .iter()
            .find(|block| {
                block.kind == BlockKind::Module
                    && block.content.contains("defmodule Demo.Worker do")
            })
            .cloned()
            .context("missing Demo.Worker module block")?,
    );
    let module_result = sub_splitter::split_result(&module_block, Language::Elixir)?;
    let module_kinds = module_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();

    assert_eq!(module_result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(
        module_result
            .blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<String>(),
        module_block.content
    );
    assert!(
        module_kinds.contains(&BlockKind::Import),
        "kinds={module_kinds:?}"
    );
    assert!(
        module_kinds.contains(&BlockKind::Function),
        "kinds={module_kinds:?}"
    );
    assert!(
        module_kinds.contains(&BlockKind::Macro),
        "kinds={module_kinds:?}"
    );
    assert!(
        !module_kinds.contains(&BlockKind::Paragraph),
        "did not expect textual fallback sub-blocks: {module_kinds:?}"
    );

    Ok(())
}
