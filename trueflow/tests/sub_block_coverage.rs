use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use uuid::Uuid;

struct TestEnv {
    root: PathBuf,
}

impl TestEnv {
    fn new() -> Result<Self> {
        let temp_dir = std::env::temp_dir()
            .join("trueflow_test_subblock")
            .join(Uuid::new_v4().to_string());
        if temp_dir.exists() {
            fs::remove_dir_all(&temp_dir)?;
        }
        fs::create_dir_all(&temp_dir)?;

        // Init git to avoid "not a git repo" warnings if strict
        Command::new("git")
            .arg("init")
            .current_dir(&temp_dir)
            .output()?;

        Ok(Self { root: temp_dir })
    }

    fn run_trueflow(&self, args: &[&str]) -> Result<String> {
        let bin = env!("CARGO_BIN_EXE_trueflow");
        let output = Command::new(bin)
            .args(args)
            .current_dir(&self.root)
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

#[test]
fn test_implicit_approval() -> Result<()> {
    let env = TestEnv::new()?;
    let file_path = env.root.join("main.rs");

    // Create content that splits: two paragraphs
    let content = "fn main() {\n    part1();\n\n    part2();\n}";
    fs::write(&file_path, content)?;

    // Scan to find Parent Hash
    // Note: scan_directory uses WalkDir which ignores .git but finds main.rs
    let output = env.run_trueflow(&["scan", "--json"])?;
    let json: serde_json::Value = serde_json::from_str(&output)?;

    // Find the code block
    // We expect 1 file, 1 block (the whole function, as rust grammar creates 'function_item')
    // Wait, optimizer merges imports? There are no imports.
    let parent_block = &json[0]["blocks"][0];
    let parent_hash = parent_block["hash"].as_str().unwrap();

    // Inspect to find Sub Hashes
    let output = env.run_trueflow(&["inspect", "--fingerprint", parent_hash, "--split"])?;
    let sub_blocks: Vec<serde_json::Value> = serde_json::from_slice(output.as_bytes())?;

    // Check we have sub-blocks
    // "fn main() {\n    part1();" (CodeParagraph)
    // "\n\n" (Gap)
    // "    part2();\n}" (CodeParagraph)

    let mut sub_hashes = Vec::new();
    for sb in &sub_blocks {
        sub_hashes.push(sb["hash"].as_str().unwrap().to_string());
    }

    // 1. Initial Review: Parent unreviewed
    let output = env.run_trueflow(&["review", "--exclude", "Gap", "--exclude", "gap"])?;
    assert!(output.contains("[Unreviewed]"));
    assert!(output.contains(parent_hash));

    // 2. Mark first sub-block
    env.run_trueflow(&[
        "mark",
        "--fingerprint",
        &sub_hashes[0],
        "--verdict",
        "approved",
    ])?;

    // 3. Review: Parent STILL unreviewed (Partial)
    let output = env.run_trueflow(&["review", "--exclude", "Gap", "--exclude", "gap"])?;
    assert!(output.contains("[Unreviewed]"));
    assert!(output.contains(parent_hash));

    // 4. Mark remaining non-Gap sub-blocks
    for (i, sb) in sub_blocks.iter().enumerate() {
        if i == 0 {
            continue;
        } // Already marked
        let kind = sb["kind"].as_str().unwrap();
        if kind.eq_ignore_ascii_case("gap") {
            continue;
        } // Excluded

        let h = sb["hash"].as_str().unwrap();
        env.run_trueflow(&["mark", "--fingerprint", h, "--verdict", "approved"])?;
    }

    // 5. Review: Parent GONE (Implicitly Approved)
    let output = env.run_trueflow(&["review", "--exclude", "Gap", "--exclude", "gap"])?;
    assert!(output.contains("All clear"));

    Ok(())
}

#[test]
fn test_markdown_implicit_approval() -> Result<()> {
    let env = TestEnv::new()?;
    let file_path = env.root.join("README.md");
    let content = "# Title\n\nPara one.\n\nPara two.\n";
    fs::write(&file_path, content)?;

    let output = env.run_trueflow(&["scan", "--json"])?;
    let json: serde_json::Value = serde_json::from_str(&output)?;
    let parent_block = &json[0]["blocks"][0];
    let parent_hash = parent_block["hash"].as_str().context("parent hash")?;

    let output = env.run_trueflow(&["inspect", "--fingerprint", parent_hash, "--split"])?;
    let sub_blocks: Vec<serde_json::Value> = serde_json::from_slice(output.as_bytes())?;

    let output = env.run_trueflow(&["review", "--all", "--exclude", "gap"])?;
    assert!(output.contains("[Unreviewed]"));
    assert!(output.contains(parent_hash));

    for sb in &sub_blocks {
        let kind = sb["kind"].as_str().context("kind")?;
        if kind.eq_ignore_ascii_case("gap") {
            continue;
        }
        let hash = sb["hash"].as_str().context("hash")?;
        env.run_trueflow(&["mark", "--fingerprint", hash, "--verdict", "approved"])?;
    }

    let output = env.run_trueflow(&["review", "--all", "--exclude", "gap"])?;
    assert!(output.contains("All clear"));

    Ok(())
}
