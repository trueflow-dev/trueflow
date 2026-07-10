use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;

use trueflow_test_support::*;

use trueflow::analysis::Language;
use trueflow::block::{Block, BlockKind, ByteSpan, LineSpan};
use trueflow::block_splitter;
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

#[test]
fn test_scala_fixture_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("scala_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let workflow = files
        .iter()
        .find(|file| path_matches(file, "src/Workflow.scala"))
        .context("missing scan output for src/Workflow.scala")?;
    assert_eq!(workflow["language"].as_str(), Some("Scala"));

    let workflow_blocks = workflow["blocks"]
        .as_array()
        .context("workflow blocks should be array")?;
    let workflow_kinds = non_gap_kinds(workflow_blocks);

    for expected in [
        "module",
        "import",
        "const",
        "variable",
        "function",
        "class",
        "interface",
        "enum",
        "impl",
        "method",
    ] {
        assert!(
            workflow_kinds.contains(&expected),
            "expected Scala {expected} block in fixture (kinds={workflow_kinds:?})"
        );
    }
    assert!(
        !workflow_kinds.contains(&"code"),
        "did not expect generic code blocks in Scala fixture (kinds={workflow_kinds:?})"
    );
    assert!(
        !workflow_kinds.contains(&"Paragraph"),
        "did not expect textual fallback blocks in Scala fixture (kinds={workflow_kinds:?})"
    );

    let registry = workflow_blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("class")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("object Registry"))
        })
        .context("missing Registry object block")?;
    assert_eq!(registry["kind"].as_str(), Some("class"));

    let default_worker = workflow_blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("impl")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("given defaultWorker"))
        })
        .context("missing given defaultWorker block")?;
    assert_eq!(default_worker["kind"].as_str(), Some("impl"));

    let worker_like = workflow_blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("interface")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("trait WorkerLike"))
        })
        .context("missing WorkerLike trait block")?;
    assert_eq!(worker_like["kind"].as_str(), Some("interface"));

    let suite = files
        .iter()
        .find(|file| path_matches(file, "tests/WorkflowSuite.scala"))
        .context("missing scan output for tests/WorkflowSuite.scala")?;
    assert_eq!(suite["language"].as_str(), Some("Scala"));

    let suite_blocks = suite["blocks"]
        .as_array()
        .context("suite blocks should be array")?;
    assert!(
        suite_blocks.iter().any(|block| {
            block["content"]
                .as_str()
                .is_some_and(|content| content.contains("class WorkflowSuite extends AnyFunSuite"))
                && block["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test")))
        }),
        "expected test suite block to be tagged: {suite_blocks:#?}"
    );
    assert!(
        suite_blocks.iter().any(|block| {
            block["content"].as_str().is_some_and(|content| {
                content.contains("test(\"worker process returns adjusted sum\")")
            }) && block["tags"]
                .as_array()
                .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test")))
        }),
        "expected nested test case block to be tagged: {suite_blocks:#?}"
    );

    Ok(())
}

#[test]
fn test_scala_type_sub_split_returns_structural_children_matching_top_level_members() -> Result<()>
{
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/scala_support/src/Workflow.scala");

    let content = std::fs::read_to_string(&file_path)?;
    let blocks = block_splitter::split(&content, Language::Scala).blocks;

    let worker = blocks
        .iter()
        .find(|block| block.kind == BlockKind::Class && block.content.contains("class Worker"))
        .cloned()
        .context("expected Scala Worker class block")?;

    let top_level_members: Vec<_> = blocks
        .iter()
        .filter(|block| {
            block.hash != worker.hash
                && block.start_line >= worker.start_line
                && block.end_line <= worker.end_line
                && matches!(
                    block.kind,
                    BlockKind::Const
                        | BlockKind::Variable
                        | BlockKind::Method
                        | BlockKind::FunctionSignature
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

    let result = sub_splitter::split_result_for_child_navigation(&worker, Language::Scala)?;
    assert_eq!(result.semantics, SubSplitSemantics::StructuralChildren);

    let children: Vec<_> = result
        .blocks
        .iter()
        .filter(|block| {
            matches!(
                block.kind,
                BlockKind::Const
                    | BlockKind::Variable
                    | BlockKind::Method
                    | BlockKind::FunctionSignature
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
        .collect();

    assert_eq!(children, top_level_members);
    assert!(
        children
            .iter()
            .any(|(kind, _, _, _, _, _)| *kind == BlockKind::Const),
        "expected Scala class const-like member: {children:#?}"
    );
    assert!(
        children
            .iter()
            .any(|(kind, _, _, _, _, _)| *kind == BlockKind::Variable),
        "expected Scala class variable member: {children:#?}"
    );
    assert!(
        children
            .iter()
            .any(|(kind, _, _, _, _, _)| *kind == BlockKind::Method),
        "expected Scala class method member: {children:#?}"
    );

    Ok(())
}

#[test]
fn test_scala_type_sub_split_preserves_non_member_code_chunks() -> Result<()> {
    let content = "object Main {\n  println(\"boot\")\n  val answer = 1\n}\n";
    let block = Block::new(
        content.to_string(),
        BlockKind::Class,
        LineSpan::new(0, content.lines().count()),
        ByteSpan::new(0, content.len()),
    );

    let result = sub_splitter::split_result_for_child_navigation(&block, Language::Scala)?;
    assert_eq!(result.semantics, SubSplitSemantics::StructuralChildren);
    let kinds = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert!(
        kinds.contains(&BlockKind::CodeParagraph),
        "expected Scala type split to retain non-member code: {kinds:?}"
    );
    assert!(
        kinds.contains(&BlockKind::Const),
        "expected Scala type split to retain member declarations: {kinds:?}"
    );

    Ok(())
}

#[test]
fn test_scala_spec_suffix_without_test_markers_does_not_over_tag() {
    let content = "class DomainSpec {\n  def process(values: List[Int]): Int = values.size\n}\n";
    let blocks = block_splitter::split(content, Language::Scala).blocks;

    assert!(
        blocks.iter().all(|block| !block.has_tag("test")),
        "did not expect non-test Scala spec-like names to be tagged: {blocks:#?}"
    );
}

#[test]
fn test_scala_comment_prefixed_test_cases_still_split_by_signature() -> Result<()> {
    let content = "class WorkflowSuite extends AnyFunSuite {\n  // reviewer docs\n  test(\"works\") {\n    assert(1 == 1)\n  }\n}\n";
    let blocks = block_splitter::split(content, Language::Scala).blocks;

    let test_case = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Function
                && block.has_tag("test")
                && block.content.contains("test(\"works\")")
        })
        .cloned()
        .context("expected comment-prefixed Scala test case block")?;

    let result = sub_splitter::split_result_for_child_navigation(&test_case, Language::Scala)?;
    assert_eq!(result.semantics, SubSplitSemantics::ReviewUnits);
    let kinds = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(kinds.first().copied(), Some(BlockKind::FunctionSignature));

    Ok(())
}

#[test]
fn test_scala_enum_test_containers_are_tagged() {
    let content = "enum ParserSpec {\n  test(\"parses ints\") {\n    assert(true)\n  }\n}\n";
    let blocks = block_splitter::split(content, Language::Scala).blocks;

    assert!(
        blocks.iter().any(|block| {
            block.kind == BlockKind::Enum
                && block.has_tag("test")
                && block.content.contains("enum ParserSpec")
        }),
        "expected Scala enum test container to be tagged: {blocks:#?}"
    );
}

#[test]
fn test_scala_field_style_test_cases_are_tagged_and_split() -> Result<()> {
    let content = "class WorkflowSuite extends AnyFunSuite {\n  helper.test(\"scoped case\") {\n    val value = 1\n    assert(value == 1)\n  }\n}\n";
    let blocks = block_splitter::split(content, Language::Scala).blocks;

    let test_case = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Function
                && block.has_tag("test")
                && block.content.contains("helper.test(\"scoped case\")")
        })
        .cloned()
        .context("expected field-style Scala test case block")?;

    let result = sub_splitter::split_result_for_child_navigation(&test_case, Language::Scala)?;
    assert_eq!(result.semantics, SubSplitSemantics::ReviewUnits);
    let kinds = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(kinds.first().copied(), Some(BlockKind::FunctionSignature));

    Ok(())
}

#[test]
fn test_scala_function_like_sub_split_returns_review_units() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/scala_support/src/Workflow.scala");

    let content = std::fs::read_to_string(&file_path)?;
    let blocks = block_splitter::split(&content, Language::Scala).blocks;

    let process = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Method
                && block
                    .content
                    .contains("def process(values: List[Int]): Int =")
        })
        .cloned()
        .context("expected Scala process method block")?;

    let result = sub_splitter::split_result_for_child_navigation(&process, Language::Scala)?;
    assert_eq!(result.semantics, SubSplitSemantics::ReviewUnits);

    let kinds = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            BlockKind::FunctionSignature,
            BlockKind::CodeParagraph,
            BlockKind::CodeParagraph,
        ],
        "expected Scala method review-unit split: {kinds:?}"
    );

    Ok(())
}
