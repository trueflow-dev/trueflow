use anyhow::{Context, Result};

mod common;
use common::*;

#[test]
fn test_all_languages_detection() -> Result<()> {
    let repo = TestRepo::fixture("all_languages")?;

    // Run scan --json
    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    // Map filename -> detected language
    let mut detected = std::collections::HashMap::new();
    for file in files {
        let path = file["path"]
            .as_str()
            .context("path should be string")?
            .replace("./", "");
        let lang = file["language"]
            .as_str()
            .context("language should be string")?
            .to_string();
        detected.insert(path, lang);
    }

    // Assertions
    let expected = vec![
        ("main.rs", "Rust"),
        ("main.swift", "Swift"),
        ("main.el", "Elisp"),
        ("main.go", "Go"),
        ("main.cpp", "Cpp"),
        ("main.js", "JavaScript"),
        ("main.ts", "TypeScript"),
        ("main.java", "Java"),
        ("main.py", "Python"),
        ("main.sh", "Shell"),
        ("main.md", "Markdown"),
        ("main.toml", "Toml"),
        ("main.nix", "Nix"),
        ("main.just", "Just"),
        ("main.txt", "Text"),
        // Or "Text" if .org maps to Text now
    ];

    for (filename, expected_lang) in &expected {
        let actual = detected
            .get(*filename)
            .with_context(|| format!("Expected file {filename} not found in scan output"))?;
        assert_eq!(
            actual, expected_lang,
            "Language mismatch for {filename}: expected {expected_lang}, got {actual}"
        );
    }

    // Keep this as a baseline subset check so adding new languages doesn't require
    // every future language change to rewrite this shared test file.
    assert!(
        detected.len() >= expected.len(),
        "Expected at least {} files but found {}",
        expected.len(),
        detected.len()
    );

    Ok(())
}

#[test]
fn test_all_languages_toml_blocks_are_structural() -> Result<()> {
    let repo = TestRepo::fixture("all_languages")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let toml_file = files
        .iter()
        .find(|file| {
            file["path"].as_str().map(|path| path.replace("./", ""))
                == Some("main.toml".to_string())
        })
        .context("missing scan output for main.toml")?;
    let blocks = toml_file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = blocks
        .iter()
        .filter_map(|block| block.get("kind").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();

    assert!(
        kinds.contains(&"Content"),
        "expected a scalar content block in main.toml (kinds={kinds:?})"
    );
    assert!(
        kinds.contains(&"Section"),
        "expected a table section block in main.toml (kinds={kinds:?})"
    );
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect paragraph fallback blocks in main.toml (kinds={kinds:?})"
    );

    Ok(())
}

#[test]
fn test_all_languages_nix_blocks_are_structural() -> Result<()> {
    let repo = TestRepo::fixture("all_languages")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let nix_file = files
        .iter()
        .find(|file| {
            file["path"].as_str().map(|path| path.replace("./", "")) == Some("main.nix".to_string())
        })
        .context("missing scan output for main.nix")?;
    let blocks = nix_file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = blocks
        .iter()
        .filter_map(|block| block.get("kind").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();

    assert!(
        kinds.contains(&"FunctionSignature"),
        "expected a function signature block in main.nix (kinds={kinds:?})"
    );
    assert!(
        kinds.contains(&"variable"),
        "expected a variable block in main.nix (kinds={kinds:?})"
    );
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect paragraph fallback blocks in main.nix (kinds={kinds:?})"
    );

    Ok(())
}

#[test]
fn test_all_languages_java_blocks_are_structural() -> Result<()> {
    let repo = TestRepo::fixture("all_languages")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let java_file = files
        .iter()
        .find(|file| {
            file["path"].as_str().map(|path| path.replace("./", ""))
                == Some("main.java".to_string())
        })
        .context("missing scan output for main.java")?;
    let blocks = java_file["blocks"]
        .as_array()
        .context("blocks should be array")?;
    let kinds = blocks
        .iter()
        .filter_map(|block| block.get("kind").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();

    assert!(
        kinds.contains(&"module"),
        "expected a package/module block in main.java (kinds={kinds:?})"
    );
    assert!(
        kinds.contains(&"import"),
        "expected an import block in main.java (kinds={kinds:?})"
    );
    assert!(
        kinds.contains(&"class"),
        "expected a class block in main.java (kinds={kinds:?})"
    );
    assert!(
        kinds.contains(&"method"),
        "expected a method block in main.java (kinds={kinds:?})"
    );
    assert!(
        !kinds.contains(&"Paragraph"),
        "did not expect paragraph fallback blocks in main.java (kinds={kinds:?})"
    );

    Ok(())
}

#[test]
fn test_all_languages_test_blocks() -> Result<()> {
    let repo = TestRepo::fixture("all_languages")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;

    let mut tags_by_path = std::collections::HashMap::new();
    for file in files {
        let path = file["path"]
            .as_str()
            .context("path should be string")?
            .replace("./", "");
        let blocks = file["blocks"]
            .as_array()
            .context("blocks should be array")?;
        let tags = blocks
            .iter()
            .filter_map(|block| block.get("tags").and_then(|value| value.as_array()))
            .flat_map(|values| {
                values
                    .iter()
                    .filter_map(|tag| tag.as_str())
                    .map(|tag| tag.to_string())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        tags_by_path.insert(path, tags);
    }

    let expected = ["main.py", "main.js", "main.ts", "main.sh", "main.swift"];

    for filename in expected {
        let tags = tags_by_path
            .get(filename)
            .with_context(|| format!("missing scan output for {filename}"))?;
        assert!(
            tags.iter().any(|tag| tag == "test"),
            "expected at least one test tag in {filename} (tags={tags:?})"
        );
    }

    Ok(())
}

#[test]
fn test_all_languages_review_block_generation_smoke() -> Result<()> {
    let repo = TestRepo::fixture("all_languages")?;

    let output = repo.run(&["review", "--all", "--json"])?;
    let files = json_array(&output)?;

    let mut blocks_by_path = std::collections::HashMap::new();
    let mut total_blocks = 0usize;
    for file in files {
        let path = file["path"]
            .as_str()
            .context("path should be string")?
            .replace("./", "");
        let blocks = file["blocks"]
            .as_array()
            .context("blocks should be array")?
            .clone();
        total_blocks += blocks.len();
        blocks_by_path.insert(path, blocks);
    }

    assert!(
        blocks_by_path.len() >= 15,
        "expected broad multi-language review coverage, got {} files",
        blocks_by_path.len()
    );
    assert!(
        total_blocks >= 80,
        "expected nontrivial review block generation, got {total_blocks} blocks"
    );

    for path in [
        "main.rs",
        "main.java",
        "main.nix",
        "main.swift",
        "main.toml",
        "main.txt",
    ] {
        let blocks = blocks_by_path
            .get(path)
            .with_context(|| format!("missing review output for {path}"))?;
        assert!(
            !blocks.is_empty(),
            "expected at least one review block in {path}"
        );
    }

    let rust_kinds = blocks_by_path["main.rs"]
        .iter()
        .filter_map(|block| block.get("kind").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    assert!(
        rust_kinds.contains(&"struct") && rust_kinds.contains(&"function"),
        "expected struct + function review blocks in main.rs (kinds={rust_kinds:?})"
    );

    let java_kinds = blocks_by_path["main.java"]
        .iter()
        .filter_map(|block| block.get("kind").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    assert!(
        java_kinds.iter().filter(|kind| **kind == "method").count() >= 2
            && java_kinds.contains(&"variable"),
        "expected method + variable review blocks in main.java (kinds={java_kinds:?})"
    );

    let js_kinds = blocks_by_path["main.js"]
        .iter()
        .filter_map(|block| block.get("kind").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    assert!(
        js_kinds.contains(&"class") && js_kinds.contains(&"function"),
        "expected class + function review blocks in main.js (kinds={js_kinds:?})"
    );

    let nix_kinds = blocks_by_path["main.nix"]
        .iter()
        .filter_map(|block| block.get("kind").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    assert!(
        nix_kinds.contains(&"FunctionSignature"),
        "expected FunctionSignature review block in main.nix (kinds={nix_kinds:?})"
    );

    let toml_kinds = blocks_by_path["main.toml"]
        .iter()
        .filter_map(|block| block.get("kind").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    assert!(
        toml_kinds.contains(&"Content") && toml_kinds.contains(&"Section"),
        "expected structural TOML review blocks in main.toml (kinds={toml_kinds:?})"
    );
    assert!(
        !toml_kinds.contains(&"Paragraph"),
        "did not expect paragraph fallback blocks in main.toml (kinds={toml_kinds:?})"
    );

    let text_kinds = blocks_by_path["main.txt"]
        .iter()
        .filter_map(|block| block.get("kind").and_then(|value| value.as_str()))
        .collect::<Vec<_>>();
    assert!(
        text_kinds.contains(&"Paragraph"),
        "expected Paragraph fallback block in main.txt (kinds={text_kinds:?})"
    );

    Ok(())
}
