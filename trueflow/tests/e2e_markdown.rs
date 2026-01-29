use anyhow::{Context, Result};
mod common;
use common::*;

#[test]
fn test_markdown_split_hierarchy() -> Result<()> {
    let repo = TestRepo::new("markdown_split")?;
    repo.write(
        "README.md",
        "# Overview\nIntro sentence one. Intro sentence two.\n\n## Details\nFirst paragraph sentence one. Second sentence.\n\n- Item one explains the flow.\n- Item two provides more context.\n",
    )?;
    repo.commit_all("Add README")?;

    let output = repo.run(&["scan", "--json"])?;
    let files = json_array(&output)?;
    let file = files
        .iter()
        .find(|entry| {
            entry["path"]
                .as_str()
                .unwrap_or_default()
                .contains("README.md")
        })
        .context("README.md entry")?;
    let blocks = file["blocks"].as_array().context("blocks")?;
    let section = blocks
        .iter()
        .find(|block| block["kind"] == "Section")
        .context("Section block")?;
    let section_hash = section["hash"].as_str().context("hash")?;

    let output = repo.run(&["inspect", "--fingerprint", section_hash, "--split"])?;
    let subblocks = json_array(&output)?;
    let kinds: Vec<&str> = subblocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .filter(|kind| !kind.eq_ignore_ascii_case("gap"))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "Header",
            "Paragraph",
            "Header",
            "Paragraph",
            "ListItem",
            "ListItem"
        ]
    );

    let paragraph = subblocks
        .iter()
        .find(|block| block["kind"] == "Paragraph")
        .context("Paragraph block")?;
    let paragraph_hash = paragraph["hash"].as_str().context("hash")?;
    let output = repo.run(&["inspect", "--fingerprint", paragraph_hash, "--split"])?;
    let sentence_blocks = json_array(&output)?;
    let sentence_kinds: Vec<&str> = sentence_blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .filter(|kind| !kind.eq_ignore_ascii_case("gap"))
        .collect();
    assert!(sentence_kinds.iter().all(|kind| *kind == "Sentence"));
    assert_eq!(sentence_kinds.len(), 2);

    Ok(())
}
