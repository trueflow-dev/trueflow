use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use uuid::Uuid;

struct TestRepo {
    path: PathBuf,
}

impl TestRepo {
    fn new(name: &str) -> Result<Self> {
        let path = std::env::temp_dir()
            .join("trueflow_tests")
            .join(name)
            .join(Uuid::new_v4().to_string());
        fs::create_dir_all(&path)?;

        Command::new("git")
            .arg("init")
            .current_dir(&path)
            .output()?;
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&path)
            .output()?;
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(&path)
            .output()?;

        Ok(Self { path })
    }

    fn write_file(&self, filename: &str, content: &str) -> Result<()> {
        let path = self.path.join(filename);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        Ok(())
    }

    fn git_add_commit(&self, msg: &str) -> Result<()> {
        Command::new("git")
            .args(["add", "."])
            .current_dir(&self.path)
            .output()?;
        Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(&self.path)
            .output()?;
        Ok(())
    }

    fn run_trueflow(&self, args: &[&str]) -> Result<String> {
        let output = Command::new(env!("CARGO_BIN_EXE_trueflow"))
            .args(args)
            .current_dir(&self.path)
            .output()?;
        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "trueflow failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8(output.stdout)?)
    }
}

fn parse_json_array(output: &str) -> Result<Vec<Value>> {
    let json: Value = serde_json::from_str(output)?;
    Ok(json.as_array().context("Output should be array")?.clone())
}

#[test]
fn test_markdown_split_hierarchy() -> Result<()> {
    let repo = TestRepo::new("markdown_split")?;
    repo.write_file(
        "README.md",
        "# Overview\nIntro sentence one. Intro sentence two.\n\n## Details\nFirst paragraph sentence one. Second sentence.\n\n- Item one explains the flow.\n- Item two provides more context.\n",
    )?;
    repo.git_add_commit("Add README")?;

    let output = repo.run_trueflow(&["scan", "--json"])?;
    let files = parse_json_array(&output)?;
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

    let output = repo.run_trueflow(&["inspect", "--fingerprint", section_hash, "--split"])?;
    let subblocks = parse_json_array(&output)?;
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
    let output = repo.run_trueflow(&["inspect", "--fingerprint", paragraph_hash, "--split"])?;
    let sentence_blocks = parse_json_array(&output)?;
    let sentence_kinds: Vec<&str> = sentence_blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .filter(|kind| !kind.eq_ignore_ascii_case("gap"))
        .collect();
    assert!(sentence_kinds.iter().all(|kind| *kind == "Sentence"));
    assert_eq!(sentence_kinds.len(), 2);

    Ok(())
}
