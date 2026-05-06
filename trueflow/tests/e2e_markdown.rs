use anyhow::{Context, Result};
use trueflow_test_support::*;

#[test]
fn test_new_markdown_file_review_is_section_granular() -> Result<()> {
    let repo = TestRepo::new("markdown_new_file_review")?;
    repo.write(
        "README.md",
        "# Guide\nIntro sentence.\n\n## Install\nInstall paragraph one.\n\nInstall paragraph two.\n\n## Usage\nUsage paragraph one.\n\nUsage paragraph two.\n",
    )?;
    repo.commit_all("Add README")?;

    let revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let output = repo.run(&["review", "--target", &format!("rev:{revision}"), "--json"])?;
    let files = json_array(&output)?;
    let file = files
        .iter()
        .find(|entry| entry["path"].as_str() == Some("README.md"))
        .context("README.md review entry")?;
    let blocks = file["blocks"].as_array().context("blocks")?;
    let section_contents = blocks
        .iter()
        .filter(|block| block["kind"] == "Section")
        .filter_map(|block| block["content"].as_str())
        .collect::<Vec<_>>();

    assert_eq!(section_contents.len(), 3);
    assert_eq!(section_contents[0], "# Guide\nIntro sentence.\n\n");
    assert_eq!(
        section_contents[1],
        "## Install\nInstall paragraph one.\n\nInstall paragraph two.\n\n"
    );
    assert_eq!(
        section_contents[2],
        "## Usage\nUsage paragraph one.\n\nUsage paragraph two.\n"
    );

    Ok(())
}

#[test]
fn test_large_markdown_leaf_section_review_uses_body_blocks() -> Result<()> {
    let repo = TestRepo::new("markdown_large_leaf_review")?;
    let mut content = String::from("# Notes\n");
    for index in 0..55 {
        content.push_str(&format!(
            "\nParagraph {index} covers one calm review unit.\n"
        ));
    }
    repo.write("README.md", &content)?;
    repo.commit_all("Add long README")?;

    let revision = run_git_output(&repo.path, &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let output = repo.run(&["review", "--target", &format!("rev:{revision}"), "--json"])?;
    let files = json_array(&output)?;
    let file = files
        .iter()
        .find(|entry| entry["path"].as_str() == Some("README.md"))
        .context("README.md review entry")?;
    let blocks = file["blocks"].as_array().context("blocks")?;

    assert!(blocks.len() > 1);
    assert!(blocks.iter().all(|block| {
        block["content"]
            .as_str()
            .is_none_or(|block_content| block_content != content)
    }));
    assert!(blocks.iter().any(|block| block["kind"] == "Paragraph"));

    Ok(())
}

#[test]
fn test_small_markdown_section_stays_whole() -> Result<()> {
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
    let kinds = block_kinds_without_gaps(&subblocks);
    assert_eq!(kinds, vec!["Section"]);

    Ok(())
}
