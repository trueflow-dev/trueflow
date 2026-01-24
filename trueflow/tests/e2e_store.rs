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
            .output()
            .context("Failed to init git repo")?;

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

fn first_block_hash(output: &str) -> Result<String> {
    let files = parse_json_array(output)?;
    let file = files.first().context("Expected file in output")?;
    let blocks = file["blocks"]
        .as_array()
        .context("Blocks should be array")?;
    let hash = blocks.first().context("Expected block in output")?["hash"]
        .as_str()
        .context("Hash should be string")?;
    Ok(hash.to_string())
}

#[test]
fn test_review_skips_invalid_db_lines() -> Result<()> {
    let repo = TestRepo::new("invalid_db")?;
    repo.write_file("src/lib.rs", "pub fn core() {}\n")?;
    repo.git_add_commit("Add lib")?;

    let output = repo.run_trueflow(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    repo.run_trueflow(&["mark", "--fingerprint", &hash, "--verdict", "approved"])?;

    let db_path = repo.path.join(".trueflow").join("reviews.jsonl");
    let mut content = fs::read_to_string(&db_path)?;
    content.push_str("not-json\n");
    fs::write(&db_path, content)?;

    let output = repo.run_trueflow(&["review", "--all", "--json"])?;
    let files = parse_json_array(&output)?;
    assert!(files.is_empty());

    Ok(())
}
