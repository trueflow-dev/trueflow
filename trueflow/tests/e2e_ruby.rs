use anyhow::{Context, Result};

mod common;
use common::*;

#[test]
fn test_ruby_fixture_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("ruby_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let file = files
        .iter()
        .find(|entry| entry["path"].as_str() == Some("lib/app.rb"))
        .context("missing lib/app.rb in scan output")?;

    assert_eq!(file["language"].as_str(), Some("Ruby"));

    let blocks = file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .collect::<Vec<_>>();

    assert!(
        kinds
            .iter()
            .any(|kind| matches!(*kind, "import" | "Imports")),
        "expected require block: {kinds:?}"
    );
    assert!(
        kinds.contains(&"module"),
        "expected module block: {kinds:?}"
    );
    assert!(kinds.contains(&"class"), "expected class block: {kinds:?}");
    assert!(
        kinds.contains(&"method"),
        "expected method block: {kinds:?}"
    );
    assert!(kinds.contains(&"const"), "expected const block: {kinds:?}");
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect textual fallback blocks: {kinds:?}"
    );
    assert!(
        blocks.iter().any(|block| {
            block["kind"].as_str() == Some("method")
                && block["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test")))
        }),
        "expected a Ruby test method block"
    );
    let processor_class = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("class")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("class Processor"))
        })
        .context("expected Processor class block")?;
    assert_eq!(processor_class["complexity"].as_u64(), Some(1));

    let process_method = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("method")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("def process(values)"))
        })
        .context("expected process method block")?;
    assert_eq!(process_method["complexity"].as_u64(), Some(1));

    Ok(())
}

#[test]
fn test_ruby_inspect_split_returns_method_review_units() -> Result<()> {
    let repo = TestRepo::fixture("ruby_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let file = files
        .iter()
        .find(|entry| entry["path"].as_str() == Some("lib/app.rb"))
        .context("missing lib/app.rb in scan output")?;
    let blocks = file["blocks"]
        .as_array()
        .context("blocks should be array")?;

    let process_method = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("method")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("def process(values)"))
        })
        .context("expected process method block")?;
    let process_hash = process_method["hash"]
        .as_str()
        .context("hash should be string")?;

    let output = repo.run(&["inspect", "--fingerprint", process_hash, "--split"])?;
    let subblocks = json_array(&output)?;
    let kinds = block_kinds_without_gaps(&subblocks);

    assert_eq!(kinds.first().copied(), Some("FunctionSignature"));
    assert!(
        kinds
            .iter()
            .filter(|kind| **kind == "CodeParagraph")
            .count()
            >= 4,
        "expected multiple Ruby code review units: {kinds:?}"
    );
    assert!(
        kinds.len() >= 5,
        "expected multiple Ruby review units: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect textual fallback review units: {kinds:?}"
    );
    assert!(
        subblocks
            .iter()
            .all(|block| block.get("complexity").is_none()),
        "expected Ruby sub-block complexity to remain absent"
    );

    Ok(())
}

#[test]
fn test_ruby_inspect_split_returns_structural_children_for_large_module() -> Result<()> {
    let repo = TestRepo::fixture("ruby_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let file = files
        .iter()
        .find(|entry| entry["path"].as_str() == Some("lib/app.rb"))
        .context("missing lib/app.rb in scan output")?;
    let blocks = file["blocks"]
        .as_array()
        .context("blocks should be array")?;

    let module_block = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("module")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("module Trueflow"))
        })
        .context("expected Trueflow module block")?;
    let module_hash = module_block["hash"]
        .as_str()
        .context("hash should be string")?;

    let output = repo.run(&["inspect", "--fingerprint", module_hash, "--split"])?;
    let subblocks = json_array(&output)?;
    let kinds = block_kinds_without_gaps(&subblocks);

    assert!(
        kinds.contains(&"const"),
        "expected module const child: {kinds:?}"
    );
    assert!(
        kinds.contains(&"module"),
        "expected nested module child: {kinds:?}"
    );
    assert!(kinds.contains(&"class"), "expected class child: {kinds:?}");
    assert!(!kinds.contains(&"Paragraph"));

    Ok(())
}
