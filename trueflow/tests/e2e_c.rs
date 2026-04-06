use anyhow::{Context, Result};
use serde_json::Value;

mod common;
use common::*;

#[test]
fn test_c_fixture_detects_c_and_preserves_cpp_mappings() -> Result<()> {
    let repo = TestRepo::fixture("c_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let mut detected = std::collections::HashMap::new();
    for file in files {
        let path = file["path"]
            .as_str()
            .context("path should be string")?
            .replace("./", "");
        let language = file["language"]
            .as_str()
            .context("language should be string")?
            .to_string();
        detected.insert(path, language);
    }

    assert_eq!(detected.get("main.c").map(String::as_str), Some("C"));
    assert_eq!(detected.get("compat.cpp").map(String::as_str), Some("Cpp"));
    assert_eq!(detected.get("compat.hpp").map(String::as_str), Some("Cpp"));
    assert_eq!(detected.get("compat.h").map(String::as_str), Some("Text"));

    Ok(())
}

#[test]
fn test_c_fixture_blocks_are_structural_and_long_function_splits() -> Result<()> {
    let repo = TestRepo::fixture("c_support")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let c_file = files
        .iter()
        .find(|file| {
            file["path"].as_str().map(|path| path.replace("./", "")) == Some("main.c".to_string())
        })
        .context("missing scan output for main.c")?;
    let blocks = c_file["blocks"]
        .as_array()
        .context("blocks should be array")?;

    let kinds = blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .collect::<Vec<_>>();
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(*kind, "import" | "Imports"))
    );
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(*kind, "type" | "struct" | "enum"))
    );
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(*kind, "variable" | "const" | "FunctionSignature"))
    );
    assert!(kinds.contains(&"function"));
    assert!(!kinds.contains(&"Paragraph"));

    let test_block = blocks
        .iter()
        .find(|block| {
            block["content"]
                .as_str()
                .is_some_and(|content| content.contains("test_process_worker"))
        })
        .context("expected test_process_worker block")?;
    let test_tags = test_block["tags"]
        .as_array()
        .context("tags should be array")?;
    assert!(test_tags.iter().any(|tag| tag.as_str() == Some("test")));

    let worker_type = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("type")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("typedef struct Worker"))
        })
        .context("expected Worker type block")?;
    assert_eq!(worker_type["complexity"].as_u64(), Some(0));

    let function_block = blocks
        .iter()
        .find(|block| {
            block["kind"].as_str() == Some("function")
                && block["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("int process_worker(Worker worker)"))
        })
        .context("expected process_worker block")?;
    assert_eq!(function_block["complexity"].as_u64(), Some(1));
    let fingerprint = function_block["hash"]
        .as_str()
        .context("function hash should be string")?;

    let split_output = repo.run(&["inspect", "--fingerprint", fingerprint, "--split"])?;
    let sub_blocks: Vec<Value> = serde_json::from_str(&split_output)?;
    let sub_kinds = sub_blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .collect::<Vec<_>>();

    assert!(sub_kinds.contains(&"FunctionSignature"));
    assert!(sub_kinds.contains(&"CodeParagraph"));
    assert!(sub_kinds.contains(&"comment"));

    Ok(())
}
