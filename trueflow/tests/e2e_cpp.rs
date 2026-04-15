use anyhow::{Context, Result};
use std::path::PathBuf;

mod common;
use common::*;

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
fn test_cpp_fixture_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("cpp_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let main = files
        .iter()
        .find(|file| path_matches(file, "main.cpp"))
        .context("missing scan output for main.cpp")?;
    assert_eq!(main["language"].as_str(), Some("Cpp"));

    let blocks = main["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = block_kinds_without_gaps(blocks);
    assert!(
        kinds.contains(&"import") || kinds.contains(&"Imports"),
        "expected C++ import block in fixture (kinds={kinds:?})"
    );
    for expected in ["module", "type", "enum", "class", "method", "function"] {
        assert!(
            kinds.contains(&expected),
            "expected C++ {expected} block in fixture (kinds={kinds:?})"
        );
    }
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect text fallback blocks (kinds={kinds:?})"
    );

    assert!(
        blocks.iter().any(|block| {
            block["kind"].as_str() == Some("function")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("int test_process_worker()"))
                && block["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test")))
        }),
        "expected C++ test-like function to be tagged: {blocks:#?}"
    );

    Ok(())
}

#[test]
fn test_cpp_type_sub_split_returns_structural_children() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/cpp_support/main.cpp");

    let content = std::fs::read_to_string(&file_path)?;
    let blocks = block_splitter::split(&content, Language::Cpp).blocks;

    let worker = blocks
        .iter()
        .find(|block| block.kind == BlockKind::Class && block.content.contains("class Worker"))
        .cloned()
        .context("expected C++ Worker class block")?;
    let result =
        sub_splitter::split_result(&expand_block_for_review_splitting(worker), Language::Cpp)?;
    assert_eq!(result.semantics, SubSplitSemantics::StructuralChildren);

    let kinds = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect::<Vec<_>>();
    assert!(kinds.contains(&BlockKind::Method));
    assert!(kinds.contains(&BlockKind::Variable));

    Ok(())
}

#[test]
fn test_cpp_function_like_sub_splits_and_complexity() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let file_path = repo_root.join("example_repos/cpp_support/main.cpp");

    let content = std::fs::read_to_string(&file_path)?;
    let blocks = block_splitter::split(&content, Language::Cpp).blocks;

    let process_method = blocks
        .iter()
        .find(|block| {
            block.kind == BlockKind::Method
                && block.content.contains("int process(int value) const")
        })
        .cloned()
        .context("expected C++ process method block")?;
    assert_eq!(process_method.complexity, Some(1));

    let result = sub_splitter::split_result(
        &expand_block_for_review_splitting(process_method),
        Language::Cpp,
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
            BlockKind::Comment,
            BlockKind::CodeParagraph,
            BlockKind::CodeParagraph,
        ]
    );

    Ok(())
}
