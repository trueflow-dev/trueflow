use anyhow::{Context, Result};
use std::path::PathBuf;

use trueflow_test_support::*;

use trueflow::analysis::Language;
use trueflow::block::{Block, BlockKind};
use trueflow::block_splitter;
use trueflow::review_units::MAX_REVIEW_UNIT_SPAN_LINES;
use trueflow::sub_splitter::{self, SubSplitSemantics};

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
fn test_go_fixture_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("go_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let main = files
        .iter()
        .find(|file| path_matches(file, "main.go"))
        .context("missing scan output for main.go")?;
    assert_eq!(main["language"].as_str(), Some("Go"));

    let blocks = main["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = block_kinds_without_gaps(blocks);
    for expected in [
        "module",
        "import",
        "const",
        "variable",
        "struct",
        "interface",
        "method",
        "function",
    ] {
        assert!(
            kinds.contains(&expected),
            "expected Go {expected} block in fixture (kinds={kinds:?})"
        );
    }
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect text fallback blocks (kinds={kinds:?})"
    );

    let tests = files
        .iter()
        .find(|file| path_matches(file, "main_test.go"))
        .context("missing scan output for main_test.go")?;
    let test_blocks = tests["blocks"]
        .as_array()
        .context("test blocks should be array")?;
    assert!(
        test_blocks.iter().any(|block| {
            block["kind"].as_str() == Some("function")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("func TestWorkerProcess"))
                && block["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test")))
        }),
        "expected Go test function to be tagged: {test_blocks:#?}"
    );
    assert!(
        test_blocks.iter().any(|block| {
            block["kind"].as_str() == Some("function")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("func BenchmarkCollectUntil"))
                && block["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test")))
        }),
        "expected Go benchmark function to be tagged: {test_blocks:#?}"
    );

    Ok(())
}

#[test]
fn test_go_type_sub_split_returns_structural_children() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/go_support/main.go");

    let content = std::fs::read_to_string(&file_path)?;
    let blocks = block_splitter::split(&content, Language::Go).blocks;

    let worker = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Struct && block.content.contains("type Worker struct")
        })
        .cloned()
        .context("expected Go Worker struct block")?;
    let worker_result =
        sub_splitter::split_result(&expand_block_for_review_splitting(worker), Language::Go)?;
    assert_eq!(
        worker_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let worker_kinds = worker_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert!(worker_kinds.contains(&BlockKind::Variable));

    let runner = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Interface && block.content.contains("type Runner interface")
        })
        .cloned()
        .context("expected Go Runner interface block")?;
    let runner_result =
        sub_splitter::split_result(&expand_block_for_review_splitting(runner), Language::Go)?;
    assert_eq!(
        runner_result.semantics,
        SubSplitSemantics::StructuralChildren
    );
    let runner_kinds = runner_result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert!(runner_kinds.contains(&BlockKind::Method));

    Ok(())
}

#[test]
fn test_go_function_like_sub_splits_and_complexity() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/go_support/main.go");

    let content = std::fs::read_to_string(&file_path)?;
    let blocks = block_splitter::split(&content, Language::Go).blocks;

    let process_method = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Method && block.content.contains("Process(values []int)")
        })
        .cloned()
        .context("expected Go Process method block")?;
    assert_eq!(process_method.complexity, Some(3));

    let result = sub_splitter::split_result(
        &expand_block_for_review_splitting(process_method),
        Language::Go,
    )?;
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
            BlockKind::Comment,
            BlockKind::CodeParagraph,
            BlockKind::CodeParagraph,
        ]
    );

    Ok(())
}
