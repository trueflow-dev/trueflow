use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

use trueflow::block::{Block, BlockKind};
use trueflow::hashing::hash_str;
use trueflow::optimizer;

struct TestRepo {
    path: PathBuf,
}

impl TestRepo {
    fn new(name: &str) -> Result<Self> {
        let path = std::env::temp_dir()
            .join("trueflow_regressions")
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

        let _ = Command::new("git")
            .args(["branch", "-m", "main"])
            .current_dir(&path)
            .output();

        Ok(Self { path })
    }

    fn write_file(&self, rel: &str, content: &str) -> Result<()> {
        let path = self.path.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
        Ok(())
    }

    fn git(&self, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.path)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    fn git_add_commit(&self, msg: &str) -> Result<()> {
        self.git(&["add", "."])?;
        self.git(&["commit", "-m", msg])?;
        Ok(())
    }

    fn run_trueflow(&self, args: &[&str]) -> Result<String> {
        let bin = env!("CARGO_BIN_EXE_trueflow");
        let output = Command::new(bin)
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

    fn run_trueflow_in(&self, cwd: &Path, args: &[&str]) -> Result<String> {
        let bin = env!("CARGO_BIN_EXE_trueflow");
        let output = Command::new(bin).args(args).current_dir(cwd).output()?;

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
fn test_optimizer_import_merge_preserves_content() {
    let blocks = vec![
        Block {
            hash: hash_str("use foo;"),
            content: "use foo;".to_string(),
            kind: BlockKind::Import,
            start_line: 0,
            end_line: 1,
        },
        Block {
            hash: hash_str("/*comment*/"),
            content: "/*comment*/".to_string(),
            kind: BlockKind::Gap,
            start_line: 1,
            end_line: 1,
        },
        Block {
            hash: hash_str("use bar;"),
            content: "use bar;".to_string(),
            kind: BlockKind::Import,
            start_line: 1,
            end_line: 2,
        },
    ];

    let expected: String = blocks.iter().map(|b| b.content.as_str()).collect();
    let optimized = optimizer::optimize(blocks);

    assert_eq!(optimized.len(), 1);
    assert_eq!(optimized[0].kind, BlockKind::Imports);
    assert_eq!(optimized[0].content, expected);
    assert_eq!(optimized[0].hash, hash_str(&optimized[0].content));
}

#[test]
fn test_diff_new_content_matches_post_hunk() -> Result<()> {
    let repo = TestRepo::new("diff_new_content")?;
    repo.write_file(
        "src/main.rs",
        "fn main() {\n    println!(\"Hello\");\n    println!(\"World\");\n}\n",
    )?;
    repo.git_add_commit("Initial")?;

    repo.git(&["checkout", "-b", "feature/update"])?;
    repo.write_file(
        "src/main.rs",
        "fn main() {\n    println!(\"Hello\");\n    println!(\"Trueflow\");\n}\n",
    )?;
    repo.git_add_commit("Update message")?;

    let output = repo.run_trueflow(&["diff", "--json"])?;
    let changes: Value = serde_json::from_str(&output)?;
    let change = changes
        .as_array()
        .context("Expected array")?
        .first()
        .context("Expected change")?;
    let new_content = change["new_content"].as_str().context("new_content")?;

    let file_content = fs::read_to_string(repo.path.join("src/main.rs"))?;
    assert_eq!(new_content, file_content);
    Ok(())
}

#[test]
fn test_review_ignores_non_review_checks() -> Result<()> {
    let repo = TestRepo::new("review_check_filter")?;
    repo.write_file("src/lib.rs", "pub fn core() {}\n")?;
    repo.git_add_commit("Add lib")?;

    let output = repo.run_trueflow(&["review", "--all", "--json"])?;
    let json: Value = serde_json::from_str(&output)?;
    let block = &json.as_array().context("Expected array")?[0]["blocks"][0];
    let hash = block["hash"].as_str().context("hash")?;

    repo.run_trueflow(&[
        "mark",
        "--fingerprint",
        hash,
        "--verdict",
        "approved",
        "--check",
        "security",
    ])?;

    let output = repo.run_trueflow(&["review", "--all", "--json"])?;
    let json: Value = serde_json::from_str(&output)?;
    assert!(!json.as_array().context("Expected array")?.is_empty());
    Ok(())
}

#[test]
fn test_review_latest_timestamp_wins() -> Result<()> {
    let repo = TestRepo::new("review_timestamp")?;
    repo.write_file("src/lib.rs", "pub fn core() {}\n")?;
    repo.git_add_commit("Add lib")?;

    let output = repo.run_trueflow(&["review", "--all", "--json"])?;
    let json: Value = serde_json::from_str(&output)?;
    let block = &json.as_array().context("Expected array")?[0]["blocks"][0];
    let hash = block["hash"].as_str().context("hash")?;

    let trueflow_dir = repo.path.join(".trueflow");
    fs::create_dir_all(&trueflow_dir)?;

    let approved = serde_json::json!({
        "id": Uuid::new_v4().to_string(),
        "fingerprint": hash,
        "check": "review",
        "verdict": "approved",
        "identity": { "type": "email", "email": "a@example.com" },
        "timestamp": 2000,
        "path_hint": null,
        "line_hint": null,
        "note": null,
        "tags": null
    });
    let rejected = serde_json::json!({
        "id": Uuid::new_v4().to_string(),
        "fingerprint": hash,
        "check": "review",
        "verdict": "rejected",
        "identity": { "type": "email", "email": "b@example.com" },
        "timestamp": 1000,
        "path_hint": null,
        "line_hint": null,
        "note": null,
        "tags": null
    });

    let file_content = format!(
        "{}\n{}\n",
        serde_json::to_string(&approved)?,
        serde_json::to_string(&rejected)?
    );
    fs::write(trueflow_dir.join("reviews.jsonl"), file_content)?;

    let output = repo.run_trueflow(&["review", "--all", "--json"])?;
    let json: Value = serde_json::from_str(&output)?;
    assert!(json.as_array().context("Expected array")?.is_empty());
    Ok(())
}

#[test]
fn test_exclude_gap_case_insensitive_for_subblocks() -> Result<()> {
    let repo = TestRepo::new("exclude_gap_case")?;
    repo.write_file(
        "src/main.rs",
        "fn main() {\n    part1();\n\n    part2();\n}\n",
    )?;
    repo.git_add_commit("Add main")?;

    let output = repo.run_trueflow(&["review", "--all", "--json"])?;
    let json: Value = serde_json::from_str(&output)?;
    let block = &json.as_array().context("Expected array")?[0]["blocks"][0];
    let parent_hash = block["hash"].as_str().context("hash")?;

    let output = repo.run_trueflow(&["inspect", "--fingerprint", parent_hash, "--split"])?;
    let sub_blocks: Vec<Value> = serde_json::from_str(&output)?;

    for sub_block in &sub_blocks {
        let kind = sub_block["kind"].as_str().context("kind")?;
        if kind.eq_ignore_ascii_case("gap") {
            continue;
        }
        let hash = sub_block["hash"].as_str().context("hash")?;
        repo.run_trueflow(&["mark", "--fingerprint", hash, "--verdict", "approved"])?;
    }

    let output = repo.run_trueflow(&["review", "--all", "--exclude", "gap", "--json"])?;
    let json: Value = serde_json::from_str(&output)?;
    assert!(json.as_array().context("Expected array")?.is_empty());
    Ok(())
}

#[test]
fn test_scan_skips_unreadable_entries() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new("scan_unreadable")?;
    repo.write_file("src/main.rs", "fn main() {}\n")?;
    repo.git_add_commit("Add main")?;

    let secret_dir = repo.path.join("secret");
    fs::create_dir_all(&secret_dir)?;
    fs::write(secret_dir.join("hidden.txt"), "nope")?;

    let mut perms = fs::metadata(&secret_dir)?.permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&secret_dir, perms)?;

    let output = repo.run_trueflow(&["scan", "--json"])?;
    let json: Value = serde_json::from_str(&output)?;
    let files = json.as_array().context("Expected array")?;
    assert!(files.iter().any(|entry| {
        entry["path"]
            .as_str()
            .unwrap_or_default()
            .contains("src/main.rs")
    }));
    Ok(())
}

#[test]
fn test_filestore_uses_repo_root_from_subdir() -> Result<()> {
    let repo = TestRepo::new("filestore_root")?;
    let nested = repo.path.join("nested");
    fs::create_dir_all(&nested)?;

    repo.run_trueflow_in(
        &nested,
        &["mark", "--fingerprint", "deadbeef", "--verdict", "approved"],
    )?;

    assert!(repo.path.join(".trueflow").exists());
    assert!(!nested.join(".trueflow").exists());
    Ok(())
}

#[test]
fn test_diff_uses_merge_base() -> Result<()> {
    let repo = TestRepo::new("diff_merge_base")?;
    repo.write_file("src/file1.rs", "fn one() {}\n")?;
    repo.git_add_commit("Add file1")?;

    repo.git(&["checkout", "-b", "feature/one"])?;
    repo.write_file("src/file1.rs", "fn one() { println!(\"feat\"); }\n")?;
    repo.git_add_commit("Update file1")?;

    repo.git(&["checkout", "main"])?;
    repo.write_file("src/file2.rs", "fn two() {}\n")?;
    repo.git_add_commit("Add file2")?;

    repo.git(&["checkout", "feature/one"])?;

    let output = repo.run_trueflow(&["diff", "--json"])?;
    let changes: Value = serde_json::from_str(&output)?;
    let files: Vec<&str> = changes
        .as_array()
        .context("Expected array")?
        .iter()
        .filter_map(|entry| entry["file"].as_str())
        .collect();

    assert!(!files.is_empty());
    assert!(files.iter().all(|file| file.contains("file1.rs")));
    Ok(())
}

#[test]
fn test_feedback_xml_escapes_cdata_end() -> Result<()> {
    let repo = TestRepo::new("feedback_cdata")?;
    repo.write_file("src/lib.rs", "pub fn core() { println!(\"]]>\"); }\n")?;
    repo.git_add_commit("Add lib")?;

    let output = repo.run_trueflow(&["review", "--all", "--json"])?;
    let json: Value = serde_json::from_str(&output)?;
    let block = &json.as_array().context("Expected array")?[0]["blocks"][0];
    let hash = block["hash"].as_str().context("hash")?;

    repo.run_trueflow(&[
        "mark",
        "--fingerprint",
        hash,
        "--verdict",
        "rejected",
        "--note",
        "Contains CDATA terminator",
    ])?;

    let output = repo.run_trueflow(&["feedback", "--format", "xml"])?;
    assert!(output.contains("<trueflow_feedback>"));
    assert!(output.contains("]]]]><![CDATA[>"));
    Ok(())
}
