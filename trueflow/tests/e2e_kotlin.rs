use anyhow::{Context, Result};
use serde_json::Value;

use trueflow_test_support::*;

fn scan_kotlin_fixture() -> Result<Vec<Value>> {
    let repo = TestRepo::fixture("kotlin_support")?;
    let output = repo.run(&["scan", "--json"])?;
    json_array(&output)
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
fn test_kotlin_detection_and_structural_blocks() -> Result<()> {
    let files = scan_kotlin_fixture()?;

    let main_file = find_file(&files, "src/main.kt")?;
    let script_file = find_file(&files, "build.main.kts")?;

    assert_eq!(main_file["language"].as_str(), Some("Kotlin"));
    assert_eq!(script_file["language"].as_str(), Some("Kotlin"));

    let blocks = main_file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .collect::<Vec<_>>();

    assert!(kinds.contains(&"module"), "kinds={kinds:?}");
    assert!(
        kinds.contains(&"import") || kinds.contains(&"Imports"),
        "kinds={kinds:?}"
    );
    assert!(kinds.contains(&"const"), "kinds={kinds:?}");
    assert!(kinds.contains(&"variable"), "kinds={kinds:?}");
    assert!(kinds.contains(&"function"), "kinds={kinds:?}");
    assert!(kinds.contains(&"class"), "kinds={kinds:?}");
    assert!(kinds.contains(&"interface"), "kinds={kinds:?}");
    assert!(kinds.contains(&"enum"), "kinds={kinds:?}");
    assert!(!kinds.contains(&"Paragraph"), "kinds={kinds:?}");

    let interface_block = find_block(blocks, "interface WorkerPort")?;
    assert_eq!(interface_block["kind"].as_str(), Some("interface"));

    let object_block = find_block(blocks, "object Registry")?;
    assert_eq!(object_block["kind"].as_str(), Some("class"));

    let test_block = find_block(blocks, "fun testWorkerProcessing")?;
    let test_tags = test_block["tags"]
        .as_array()
        .context("tags should be array")?;
    assert!(
        test_tags.iter().any(|tag| tag.as_str() == Some("test")),
        "tags={test_tags:?}"
    );

    let run_scenario = find_block(blocks, "fun runScenario")?;
    assert_eq!(run_scenario["complexity"].as_u64(), Some(7));

    let worker_class = find_block(blocks, "class Worker(private val scale: Int)")?;
    assert_eq!(worker_class["complexity"].as_u64(), Some(4));

    Ok(())
}

#[test]
fn test_kotlin_function_sub_blocks_are_review_units() -> Result<()> {
    let repo = TestRepo::fixture("kotlin_support")?;
    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let main_file = find_file(&files, "src/main.kt")?;
    let blocks = main_file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let function_block = find_block(blocks, "fun runScenario")?;
    let hash = function_block["hash"]
        .as_str()
        .context("hash should be string")?;

    let output = repo.run(&["inspect", "--fingerprint", hash, "--split"])?;
    let sub_blocks = json_array(&output)?;
    let kinds = block_kinds_without_gaps(&sub_blocks);

    assert!(kinds.contains(&"FunctionSignature"), "kinds={kinds:?}");
    assert!(kinds.contains(&"CodeParagraph"), "kinds={kinds:?}");
    assert!(
        sub_blocks
            .iter()
            .all(|block| block.get("complexity").is_none()),
        "expected Kotlin sub-block complexity to remain unset"
    );

    Ok(())
}

#[test]
fn test_kotlin_type_sub_blocks_are_structural() -> Result<()> {
    let repo = TestRepo::fixture("kotlin_support")?;
    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let main_file = find_file(&files, "src/main.kt")?;
    let blocks = main_file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let class_block = find_block(blocks, "class Worker(private val scale: Int)")?;
    let hash = class_block["hash"]
        .as_str()
        .context("hash should be string")?;

    let output = repo.run(&["inspect", "--fingerprint", hash, "--split"])?;
    let sub_blocks = json_array(&output)?;
    let kinds = block_kinds_without_gaps(&sub_blocks);

    assert!(
        kinds.contains(&"const") || kinds.contains(&"variable"),
        "kinds={kinds:?}"
    );
    assert!(kinds.contains(&"method"), "kinds={kinds:?}");
    assert!(!kinds.contains(&"class"), "kinds={kinds:?}");

    Ok(())
}
