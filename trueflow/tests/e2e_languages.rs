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
        ("main.el", "Elisp"),
        ("main.js", "JavaScript"),
        ("main.ts", "TypeScript"),
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
            .with_context(|| format!("Expected file {} not found in scan output", filename))?;
        assert_eq!(
            actual, expected_lang,
            "Language mismatch for {}: expected {}, got {}",
            filename, expected_lang, actual
        );
    }

    // Verify we found all expected files
    assert_eq!(
        detected.len(),
        expected.len(),
        "Expected {} files but found {}",
        expected.len(),
        detected.len()
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

    let expected = ["main.py", "main.js", "main.ts", "main.sh"];

    for filename in expected {
        let tags = tags_by_path
            .get(filename)
            .with_context(|| format!("missing scan output for {}", filename))?;
        assert!(
            tags.iter().any(|tag| tag == "test"),
            "expected at least one test tag in {} (tags={:?})",
            filename,
            tags
        );
    }

    Ok(())
}
