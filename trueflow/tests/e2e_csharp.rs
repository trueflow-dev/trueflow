use anyhow::{Context, Result};
use serde_json::Value;

use trueflow_test_support::*;

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
fn test_csharp_fixture_detects_language_and_structural_blocks() -> Result<()> {
    let repo = TestRepo::fixture("csharp_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let workflow = files
        .iter()
        .find(|file| path_matches(file, "src/Workflow.cs"))
        .context("missing scan output for src/Workflow.cs")?;
    assert_eq!(workflow["language"].as_str(), Some("CSharp"));

    let blocks = workflow["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = non_gap_kinds(blocks);

    assert!(
        kinds.contains(&"import") || kinds.contains(&"Imports"),
        "expected using block: {kinds:?}"
    );
    assert!(
        kinds.contains(&"module"),
        "expected namespace block: {kinds:?}"
    );
    assert!(
        kinds.contains(&"interface"),
        "expected interface block: {kinds:?}"
    );
    assert!(
        kinds.contains(&"struct"),
        "expected struct/record block: {kinds:?}"
    );
    assert!(kinds.contains(&"enum"), "expected enum block: {kinds:?}");
    assert!(kinds.contains(&"class"), "expected class block: {kinds:?}");
    assert!(
        kinds.contains(&"method"),
        "expected method block: {kinds:?}"
    );
    assert!(
        kinds.contains(&"variable"),
        "expected property block: {kinds:?}"
    );
    let greeter_class = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("class")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("class Greeter : IGreeter"))
        })
        .context("missing Greeter class block")?;
    assert_eq!(greeter_class["complexity"].as_u64(), Some(5));

    let build_greeting = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("method")
                && block["content"].as_str().is_some_and(|content| {
                    content.contains("public GreetingResult BuildGreeting(string target)")
                })
        })
        .context("missing BuildGreeting method block")?;
    assert_eq!(build_greeting["complexity"].as_u64(), Some(5));

    let test_file = files
        .iter()
        .find(|file| path_matches(file, "tests/WorkflowTests.cs"))
        .context("missing scan output for tests/WorkflowTests.cs")?;
    let test_blocks = test_file["blocks"]
        .as_array()
        .context("test blocks should be array")?;
    assert!(
        test_blocks.iter().any(|block| {
            block["content"]
                .as_str()
                .is_some_and(|content| content.contains("BuildGreeting_uses_the_target_name"))
                && block["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("test")))
        }),
        "expected an obvious C# test method to be tagged: {test_blocks:#?}"
    );

    Ok(())
}

#[test]
fn test_csharp_sub_block_review_splitting_for_type_and_method() -> Result<()> {
    let repo = TestRepo::fixture("csharp_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let workflow = files
        .iter()
        .find(|file| path_matches(file, "src/Workflow.cs"))
        .context("missing scan output for src/Workflow.cs")?;
    let blocks = workflow["blocks"]
        .as_array()
        .context("blocks should be array")?;

    let class_hash = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("class")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("class Greeter"))
        })
        .and_then(|block| block["hash"].as_str())
        .context("missing Greeter class block hash")?;
    let method_hash = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("method")
                && block["content"].as_str().is_some_and(|content| {
                    content.contains("public GreetingResult BuildGreeting(string target)")
                })
        })
        .and_then(|block| block["hash"].as_str())
        .context("missing BuildGreeting method block hash")?;

    let class_output = repo.run(&["inspect", "--fingerprint", class_hash, "--split"])?;
    let class_sub_blocks = json_array(&class_output)?;
    let class_kinds = non_gap_kinds(&class_sub_blocks);
    assert!(
        class_sub_blocks.len() > 1,
        "expected class sub-blocks: {class_sub_blocks:#?}"
    );
    assert!(
        class_kinds.contains(&"variable"),
        "expected property sub-blocks: {class_kinds:?}"
    );
    assert!(
        class_kinds.contains(&"method"),
        "expected method sub-blocks: {class_kinds:?}"
    );

    let method_output = repo.run(&["inspect", "--fingerprint", method_hash, "--split"])?;
    let method_sub_blocks = json_array(&method_output)?;
    let method_kinds = non_gap_kinds(&method_sub_blocks);
    assert!(
        method_sub_blocks.len() > 1,
        "expected method sub-blocks: {method_sub_blocks:#?}"
    );
    assert_eq!(method_kinds.first().copied(), Some("FunctionSignature"));
    assert!(
        method_kinds.contains(&"CodeParagraph"),
        "expected code paragraphs in method split: {method_kinds:?}"
    );

    Ok(())
}
