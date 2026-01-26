use anyhow::Result;
use serde_json::Value;

mod common;
use common::{TestRepo, first_block_hash, json_array};

fn mark(repo: &TestRepo, hash: &str) -> Result<()> {
    repo.run(&[
        "mark",
        "--fingerprint",
        hash,
        "--verdict",
        "approved",
        "--quiet",
    ])?;
    Ok(())
}

fn is_gap(sub_block: &Value) -> bool {
    sub_block["kind"]
        .as_str()
        .expect("kind")
        .eq_ignore_ascii_case("gap")
}

#[test]
fn test_implicit_approval() -> Result<()> {
    let repo = TestRepo::new("subblock_implicit")?;
    repo.write("main.rs", "fn seed() {}\n")?;
    repo.commit_all("Seed main")?;
    repo.write("main.rs", "fn main() {\n    part1();\n\n    part2();\n}")?;

    let output = repo.run(&["scan", "--json"])?;
    let parent_hash = first_block_hash(&output)?;

    let output = repo.run(&["inspect", "--fingerprint", &parent_hash, "--split"])?;
    let sub_blocks = json_array(&output)?;

    let first_hash = sub_blocks
        .first()
        .and_then(|sb| sb["hash"].as_str())
        .expect("sub-block hash");

    let output = repo.run(&["review", "--exclude", "Gap", "--exclude", "gap"])?;
    assert!(output.contains("[Unreviewed]"));
    assert!(output.contains(&parent_hash));

    mark(&repo, first_hash)?;

    let output = repo.run(&["review", "--exclude", "Gap", "--exclude", "gap"])?;
    assert!(output.contains("[Unreviewed]"));
    assert!(output.contains(&parent_hash));

    for sb in sub_blocks.iter().skip(1) {
        if is_gap(sb) {
            continue;
        }
        let hash = sb["hash"].as_str().expect("hash");
        mark(&repo, hash)?;
    }

    let output = repo.run(&["review", "--exclude", "Gap", "--exclude", "gap"])?;
    assert!(output.contains("All clear"));

    let output = repo.run(&["check"])?;
    assert!(
        output.trim().is_empty(),
        "Expected check to be silent on stdout"
    );

    Ok(())
}

#[test]
fn test_markdown_implicit_approval() -> Result<()> {
    let repo = TestRepo::new("subblock_markdown")?;
    repo.write("README.md", "# Seed\n")?;
    repo.commit_all("Seed main")?;
    repo.write("README.md", "# Title\n\nPara one.\n\nPara two.\n")?;

    let output = repo.run(&["scan", "--json"])?;
    let parent_hash = first_block_hash(&output)?;

    let output = repo.run(&["inspect", "--fingerprint", &parent_hash, "--split"])?;
    let sub_blocks = json_array(&output)?;

    let output = repo.run(&["review", "--all", "--exclude", "gap"])?;
    assert!(output.contains("[Unreviewed]"));
    assert!(output.contains(&parent_hash));

    for sb in &sub_blocks {
        if is_gap(sb) {
            continue;
        }
        let hash = sb["hash"].as_str().expect("hash");
        mark(&repo, hash)?;
    }

    let output = repo.run(&["review", "--all", "--exclude", "gap"])?;
    assert!(output.contains("All clear"));

    let output = repo.run(&["check"])?;
    assert!(
        output.trim().is_empty(),
        "Expected check to be silent on stdout"
    );

    Ok(())
}
