use anyhow::{Context, Result};
use std::path::PathBuf;

mod common;
use common::*;
use trueflow::analysis::Language;
use trueflow::block::BlockKind;
use trueflow::block_splitter;
use trueflow::review_units::MAX_REVIEW_UNIT_SPAN_LINES;
use trueflow::sub_splitter::{self, SubSplitSemantics};

fn fixture_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("example_repos/elisp_support/elisp-support.el")
}

#[test]
fn test_elisp_fixture_scans_as_structured_language() -> Result<()> {
    let repo = TestRepo::fixture("elisp_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let file = files
        .iter()
        .find(|file| {
            file["path"].as_str().map(|path| path.replace("./", ""))
                == Some("elisp-support.el".to_string())
        })
        .context("missing scan output for elisp-support.el")?;

    assert_eq!(file["language"], "Elisp");

    let blocks = file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = blocks
        .iter()
        .filter_map(|block| block.get("kind").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();

    assert!(
        kinds
            .iter()
            .any(|kind| matches!(*kind, "import" | "Imports")),
        "expected import-like blocks in elisp fixture (kinds={kinds:?})"
    );
    assert!(
        kinds.contains(&"const"),
        "expected const block in elisp fixture (kinds={kinds:?})"
    );
    assert!(
        kinds.contains(&"variable"),
        "expected variable block in elisp fixture (kinds={kinds:?})"
    );
    assert!(
        kinds.contains(&"macro"),
        "expected macro block in elisp fixture (kinds={kinds:?})"
    );
    assert!(
        kinds.contains(&"function"),
        "expected function block in elisp fixture (kinds={kinds:?})"
    );
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(*kind, "module" | "Modules")),
        "expected module-like provide block in elisp fixture (kinds={kinds:?})"
    );
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect paragraph fallback blocks in elisp fixture (kinds={kinds:?})"
    );

    assert!(
        blocks.iter().any(|block| {
            block["tags"]
                .as_array()
                .is_some_and(|tags| tags.iter().any(|tag| tag == "test"))
        }),
        "expected at least one test-tagged block in elisp fixture"
    );

    let run_block = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("function")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.starts_with("(defun elisp-support-run"))
        })
        .context("missing elisp-support-run block")?;
    assert_eq!(run_block["complexity"].as_u64(), Some(2));

    Ok(())
}

#[test]
fn test_elisp_function_subblocks_are_review_units() -> Result<()> {
    let fixture = fixture_file();
    let content = std::fs::read_to_string(&fixture)?;
    let mut block = block_splitter::split(&content, Language::Elisp)
        .into_review_blocks()
        .into_iter()
        .find(|block| {
            block.kind == BlockKind::Function
                && block.content.starts_with("(defun elisp-support-run")
        })
        .context("missing elisp-support-run function block")?;
    block.end_line = block.start_line + MAX_REVIEW_UNIT_SPAN_LINES + 8;

    let result = sub_splitter::split_result(&block, Language::Elisp)?;
    let kinds: Vec<_> = result
        .blocks
        .iter()
        .filter(|block| block.kind != BlockKind::Gap)
        .map(|block| block.kind)
        .collect();

    assert_eq!(result.semantics, SubSplitSemantics::ReviewUnits);
    assert_eq!(kinds[0], BlockKind::FunctionSignature);
    assert!(
        kinds
            .iter()
            .filter(|kind| **kind == BlockKind::CodeParagraph)
            .count()
            >= 3,
        "expected multiple elisp body review units (kinds={kinds:?})"
    );
    assert!(result.blocks.iter().all(|block| block.complexity.is_none()));

    Ok(())
}
