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

    fn scan_blocks(&self) -> Result<Vec<Value>> {
        let output = self.run_trueflow(&["scan", "--json"])?;
        let json: Value = serde_json::from_str(&output)?;
        Ok(json.as_array().context("Output should be array")?.clone())
    }
}

#[test]
fn test_inspect_errors_on_missing_block() -> Result<()> {
    let repo = TestRepo::new("inspect_missing")?;
    repo.write_file("src/lib.rs", "pub fn core() {}\n")?;

    let output = Command::new(env!("CARGO_BIN_EXE_trueflow"))
        .args(["inspect", "--fingerprint", "deadbeef"])
        .current_dir(&repo.path)
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Block not found"));

    Ok(())
}

#[test]
fn test_inspect_errors_on_duplicate_fingerprint() -> Result<()> {
    let repo = TestRepo::new("inspect_duplicate")?;
    repo.write_file("src/lib.rs", "pub fn alpha() {}\n\npub fn alpha() {}\n")?;

    let blocks = repo.scan_blocks()?;
    let hash = blocks[0]["blocks"][0]["hash"].as_str().context("hash")?;

    let output = Command::new(env!("CARGO_BIN_EXE_trueflow"))
        .args(["inspect", "--fingerprint", hash])
        .current_dir(&repo.path)
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Multiple blocks matched"));

    Ok(())
}

#[test]
fn test_inspect_split_preserves_order() -> Result<()> {
    let repo = TestRepo::new("inspect_split")?;
    let content = "fn main() {\n    part1();\n\n    part2();\n}\n";
    repo.write_file("src/main.rs", content)?;

    let blocks = repo.scan_blocks()?;
    let hash = blocks[0]["blocks"][0]["hash"].as_str().context("hash")?;

    let output = repo.run_trueflow(&["inspect", "--fingerprint", hash, "--split"])?;
    let sub_blocks: Vec<Value> = serde_json::from_str(&output)?;
    let reconstructed: String = sub_blocks
        .iter()
        .filter_map(|block| block["content"].as_str())
        .collect();

    assert_eq!(
        reconstructed.trim_end_matches('\n'),
        content.trim_end_matches('\n')
    );

    Ok(())
}
