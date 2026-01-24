use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

struct TestFixture {
    root: PathBuf,
}

impl TestFixture {
    fn new(name: &str) -> Result<Self> {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let src_path = repo_root.join("example_repos").join(name);

        let temp_dir = std::env::temp_dir()
            .join("trueflow_e2e")
            .join(name)
            .join(Uuid::new_v4().to_string());

        if temp_dir.exists() {
            fs::remove_dir_all(&temp_dir)?;
        }
        fs::create_dir_all(&temp_dir)?;

        // Copy contents
        // Using cp -R because it's simple and we are on Unix
        let status = Command::new("cp")
            .arg("-R")
            // Append /. to copy contents, not the folder itself if we want flat structure
            .arg(format!("{}/.", src_path.display()))
            .arg(&temp_dir)
            .status()?;

        if !status.success() {
            return Err(anyhow::anyhow!("Failed to copy fixture"));
        }

        // Initialize git to simulate a repo (needed for 'dirty' checks default)
        Command::new("git")
            .arg("init")
            .current_dir(&temp_dir)
            .output()?;

        // Configure git user for commits
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&temp_dir)
            .output()?;
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(&temp_dir)
            .output()?;

        // If basic_changes, the files are now "Untracked" (Dirty).
        // This is perfect for default "review".

        Ok(Self { root: temp_dir })
    }

    fn write_file(&self, path: &str, content: &str) -> Result<()> {
        let p = self.root.join(path);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(p, content)?;
        Ok(())
    }

    fn git_add(&self, path: &str) -> Result<()> {
        Command::new("git")
            .arg("add")
            .arg(path)
            .current_dir(&self.root)
            .output()?;
        Ok(())
    }

    fn git_commit(&self, msg: &str) -> Result<()> {
        Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(&self.root)
            .output()?;
        Ok(())
    }

    fn run_trueflow(&self, args: &[&str]) -> Result<String> {
        let bin = env!("CARGO_BIN_EXE_trueflow");
        let output = Command::new(bin)
            .args(args)
            .current_dir(&self.root)
            .output()?;

        if !output.status.success() {
            eprintln!(
                "trueflow stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return Err(anyhow::anyhow!("trueflow failed"));
        }

        Ok(String::from_utf8(output.stdout)?)
    }
}

fn parse_json_array(output: &str) -> Result<Vec<Value>> {
    let json: Value = serde_json::from_str(output)?;
    Ok(json.as_array().context("Output should be array")?.clone())
}

fn first_file_blocks(output: &str) -> Result<Vec<Value>> {
    let files = parse_json_array(output)?;
    let file = files.first().context("Expected file in output")?;
    Ok(file["blocks"]
        .as_array()
        .context("Blocks should be array")?
        .clone())
}

fn first_block_hash(output: &str) -> Result<String> {
    let blocks = first_file_blocks(output)?;
    let hash = blocks.first().context("Expected block in output")?["hash"]
        .as_str()
        .context("Hash should be string")?;
    Ok(hash.to_string())
}

fn first_file_hash(output: &str) -> Result<String> {
    let files = parse_json_array(output)?;
    let file = files.first().context("Expected file in output")?;
    let hash = file["file_hash"]
        .as_str()
        .context("file_hash should be string")?;
    Ok(hash.to_string())
}

#[test]
fn test_empty_repo() -> Result<()> {
    let fixture = TestFixture::new("empty")?;

    // 1. Review default (should match nothing if clean, or nothing if empty)
    let output = fixture.run_trueflow(&["review", "--json"])?;

    // Parse output
    let files = parse_json_array(&output)?;
    assert!(files.is_empty());

    // 2. Review all
    let output = fixture.run_trueflow(&["review", "--all", "--json"])?;
    let files = parse_json_array(&output)?;
    assert!(files.is_empty());

    Ok(())
}

#[test]
fn test_basic_changes() -> Result<()> {
    let fixture = TestFixture::new("basic_changes")?;

    // 1. Review All
    let output = fixture.run_trueflow(&["review", "--all", "--json"])?;
    let files = parse_json_array(&output)?;

    // Should find src/main.rs
    assert_eq!(files.len(), 1, "Should find exactly 1 file");
    let file_obj = &files[0];
    let path = file_obj["path"].as_str().context("path")?;
    assert!(path.contains("src/main.rs"));

    let blocks = file_obj["blocks"].as_array().context("blocks")?;
    // main and helper functions
    assert!(blocks.len() >= 2, "Should have at least 2 blocks");

    // Check for semantic kinds
    let kinds: Vec<&str> = blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .collect();

    assert!(kinds.contains(&"function"));

    Ok(())
}

#[test]
fn test_dirty_tree_filtering() -> Result<()> {
    let fixture = TestFixture::new("empty")?;

    // 1. Create a clean file
    fixture.write_file("clean.rs", "fn clean() {}")?;
    fixture.git_add("clean.rs")?;
    fixture.git_commit("Add clean file")?;

    // 2. Create a dirty file (committed first, then modified)
    fixture.write_file("dirty.rs", "fn dirty_v1() {}")?;
    fixture.git_add("dirty.rs")?;
    fixture.git_commit("Add dirty file")?;

    // Modify it
    fixture.write_file("dirty.rs", "fn dirty_v2() {}")?;

    // 3. Create a purely untracked file
    fixture.write_file("untracked.rs", "fn untracked() {}")?;

    // 4. Run review (default = dirty only)
    let output = fixture.run_trueflow(&["review", "--json"])?;
    let files = parse_json_array(&output)?;

    // Expect: dirty.rs and untracked.rs.
    // Expect NOT: clean.rs

    let paths: Vec<&str> = files
        .iter()
        .filter_map(|obj| obj["path"].as_str())
        .collect();

    // Check presence
    assert!(
        paths.iter().any(|p| p.contains("dirty.rs")),
        "dirty.rs should be present"
    );
    assert!(
        paths.iter().any(|p| p.contains("untracked.rs")),
        "untracked.rs should be present"
    );
    assert!(
        !paths.iter().any(|p| p.contains("clean.rs")),
        "clean.rs should NOT be present"
    );

    Ok(())
}

#[test]
fn test_mark_flow() -> Result<()> {
    // Reuse empty fixture to start fresh
    let fixture = TestFixture::new("empty")?;

    // 1. Create content
    fixture.write_file("src/main.rs", "fn main() { println!(\"Review me\"); }")?;
    fixture.git_add("src/main.rs")?; // Make it tracked so we can use default review if we wanted, but we use --all
    fixture.git_commit("Add main")?;

    // 2. Get hash
    let output = fixture.run_trueflow(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    // 3. Mark Approved
    fixture.run_trueflow(&["mark", "--fingerprint", &hash, "--verdict", "approved"])?;

    // 4. Verify gone
    let output = fixture.run_trueflow(&["review", "--all", "--json"])?;
    let files = parse_json_array(&output)?;
    assert!(files.is_empty(), "Should have no unreviewed files");

    // 5. Mark Rejected
    fixture.run_trueflow(&["mark", "--fingerprint", &hash, "--verdict", "rejected"])?;

    // 6. Verify back
    let output = fixture.run_trueflow(&["review", "--all", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    assert!(!blocks.is_empty());
    let returned_hash = blocks[0]["hash"].as_str().context("hash")?;
    assert_eq!(returned_hash, hash);

    Ok(())
}

#[test]
fn test_feedback_export() -> Result<()> {
    let fixture = TestFixture::new("empty")?;

    // 1. Create content
    fixture.write_file("src/lib.rs", "fn core() { }")?;
    fixture.git_add("src/lib.rs")?;
    fixture.git_commit("Add lib")?;

    // 2. Get hash
    let output = fixture.run_trueflow(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    // 3. Mark with Comment
    fixture.run_trueflow(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "rejected",
        "--note",
        "Needs optimization",
    ])?;

    // 4. Run Feedback
    let xml_output = fixture.run_trueflow(&["feedback", "--format", "xml"])?;

    // 5. Assertions
    assert!(xml_output.contains("<trueflow_feedback>"));
    assert!(xml_output.contains("path=\"./src/lib.rs\"")); // Scanner output has ./
    assert!(xml_output.contains("verdict=\"rejected\""));
    assert!(xml_output.contains("<comment>Needs optimization</comment>"));
    assert!(xml_output.contains("<![CDATA[\nfn core() { }\n]]>"));

    Ok(())
}

#[test]
fn test_feedback_json_includes_non_review_check() -> Result<()> {
    let fixture = TestFixture::new("empty")?;

    fixture.write_file("src/lib.rs", "fn core() { }")?;
    fixture.git_add("src/lib.rs")?;
    fixture.git_commit("Add lib")?;

    let output = fixture.run_trueflow(&["review", "--all", "--json"])?;
    let hash = first_block_hash(&output)?;

    fixture.run_trueflow(&[
        "mark",
        "--fingerprint",
        &hash,
        "--verdict",
        "rejected",
        "--check",
        "security",
    ])?;

    let output = fixture.run_trueflow(&["feedback", "--format", "json"])?;
    let feedback = parse_json_array(&output)?;
    let entry = feedback.first().context("Expected feedback entry")?;

    let latest_verdict = entry["latest_verdict"].as_str().context("latest_verdict")?;
    assert_eq!(latest_verdict, "rejected");
    assert!(
        entry["file"]
            .as_str()
            .unwrap_or_default()
            .contains("src/lib.rs")
    );

    let reviews = entry["reviews"]
        .as_array()
        .context("Reviews should be array")?;
    let review = reviews.first().context("Expected review entry")?;
    let check = review["check"].as_str().context("check")?;
    let verdict = review["verdict"].as_str().context("verdict")?;
    assert_eq!(check, "security");
    assert_eq!(verdict, "rejected");

    Ok(())
}

#[test]
fn test_half_reviewed_blocks() -> Result<()> {
    let fixture = TestFixture::new("empty")?;

    fixture.write_file("src/main.rs", "fn alpha() {}\n\nfn beta() {}\n")?;
    fixture.git_add("src/main.rs")?;
    fixture.git_commit("Add functions")?;

    let output = fixture.run_trueflow(&["review", "--all", "--json"])?;
    let blocks = first_file_blocks(&output)?;
    assert!(blocks.len() >= 2, "Expected at least 2 blocks");

    let approved_hash = blocks[0]["hash"]
        .as_str()
        .context("Hash should be string")?;
    fixture.run_trueflow(&[
        "mark",
        "--fingerprint",
        approved_hash,
        "--verdict",
        "approved",
    ])?;

    let output = fixture.run_trueflow(&["review", "--all", "--json"])?;
    let blocks_after = first_file_blocks(&output)?;

    assert_eq!(blocks_after.len(), blocks.len() - 1);
    assert!(
        !blocks_after
            .iter()
            .any(|block| block["hash"] == approved_hash)
    );

    Ok(())
}

#[test]
fn test_file_hash_approval_hides_blocks() -> Result<()> {
    let fixture = TestFixture::new("empty")?;

    fixture.write_file("src/lib.rs", "pub fn alpha() {}\n")?;
    fixture.git_add("src/lib.rs")?;
    fixture.git_commit("Add lib")?;

    let output = fixture.run_trueflow(&["scan", "--json"])?;
    let file_hash = first_file_hash(&output)?;

    fixture.run_trueflow(&["mark", "--fingerprint", &file_hash, "--verdict", "approved"])?;

    let output = fixture.run_trueflow(&["review", "--all", "--json"])?;
    let files = parse_json_array(&output)?;
    assert!(files.is_empty());

    Ok(())
}
