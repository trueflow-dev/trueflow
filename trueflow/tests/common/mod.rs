#![allow(dead_code)]

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use trueflow::store::Record;
use uuid::Uuid;

pub struct TestRepo {
    pub path: PathBuf,
}

impl TestRepo {
    pub fn new(name: &str) -> Result<Self> {
        let path = temp_dir("trueflow_tests", name);
        fs::create_dir_all(&path)?;
        init_git(&path)?;
        Ok(Self { path })
    }

    pub fn fixture(name: &str) -> Result<Self> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("example_repos")
            .join(name);

        let path = temp_dir("trueflow_e2e", name);
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }

        if !src.exists() {
            fs::create_dir_all(&src)?;
        }

        fs::create_dir_all(&path)?;

        // Copy contents
        let status = Command::new("cp")
            .arg("-R")
            .arg(format!("{}/.", src.display()))
            .arg(&path)
            .status()?;

        if !status.success() {
            anyhow::bail!("Failed to copy fixture");
        }

        init_git(&path)?;
        Ok(Self { path })
    }

    pub fn write(&self, path: &str, content: &str) -> Result<()> {
        let p = self.path.join(path);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(p, content)?;
        Ok(())
    }

    pub fn git(&self, args: &[&str]) -> Result<()> {
        run_git(&self.path, args)
    }

    pub fn add(&self, path: &str) -> Result<()> {
        self.git(&["add", path])
    }

    pub fn commit(&self, msg: &str) -> Result<()> {
        self.git(&["commit", "-m", msg])
    }

    pub fn commit_all(&self, msg: &str) -> Result<()> {
        self.add(".")?;
        self.commit(msg)
    }

    pub fn run(&self, args: &[&str]) -> Result<String> {
        run_cmd(&self.path, args)
    }

    pub fn run_in(&self, args: &[&str], dir: &Path) -> Result<String> {
        run_cmd(dir, args)
    }

    pub fn run_err(&self, args: &[&str]) -> Result<String> {
        let output = build_cmd(&self.path, args).output()?;
        if output.status.success() {
            anyhow::bail!("trueflow succeeded but expected failure");
        }
        Ok(String::from_utf8(output.stderr)?)
    }

    pub fn run_raw(&self, args: &[&str]) -> Result<std::process::Output> {
        Ok(build_cmd(&self.path, args).output()?)
    }
}

// Helpers

fn temp_dir(base: &str, name: &str) -> PathBuf {
    std::env::temp_dir()
        .join(base)
        .join(name)
        .join(Uuid::new_v4().to_string())
}

fn init_git(path: &Path) -> Result<()> {
    run_git(path, &["init", "-q"])?;
    run_git(path, &["config", "user.email", "test@example.com"])?;
    run_git(path, &["config", "user.name", "Test User"])?;
    Ok(())
}

fn build_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_trueflow"));
    cmd.args(args).current_dir(dir);
    cmd
}

fn run_cmd(dir: &Path, args: &[&str]) -> Result<String> {
    let output = build_cmd(dir, args).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "trueflow failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

pub fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git").args(args).current_dir(dir).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

pub fn run_git_output(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).current_dir(dir).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Parse CLI JSON output into a serde_json::Value.
pub fn json(output: &str) -> Result<Value> {
    serde_json::from_str(output).with_context(|| format!("Invalid JSON: {}", truncate(output, 200)))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

/// Parse CLI JSON output into a top-level array.
pub fn json_array(output: &str) -> Result<Vec<Value>> {
    json(output)?
        .as_array()
        .cloned()
        .context("Output should be array")
}

/// Check if a block kind is "gap" (case-insensitive).
pub fn is_gap(kind: &str) -> bool {
    kind.eq_ignore_ascii_case("gap")
}

/// Extract block kinds from a blocks array, filtering out gaps.
pub fn block_kinds_without_gaps(blocks: &[Value]) -> Vec<&str> {
    blocks
        .iter()
        .filter_map(|block| block["kind"].as_str())
        .filter(|kind| !is_gap(kind))
        .collect()
}

/// Return the first file's blocks from scan/review JSON output.
///
/// Input contract: JSON array with at least one file entry containing a `blocks` array.
pub fn first_file_blocks(output: &str) -> Result<Vec<Value>> {
    let files = json_array(output)?;
    let file = files.first().context("Expected file in output")?;
    Ok(file["blocks"]
        .as_array()
        .context("Blocks should be array")?
        .clone())
}

/// Return the `file_hash` from the first file entry in scan JSON output.
///
/// Input contract: JSON array with at least one file entry containing `file_hash`.
pub fn first_file_hash(output: &str) -> Result<String> {
    let files = json_array(output)?;
    let file = files.first().context("Expected file in output")?;
    let hash = file["file_hash"]
        .as_str()
        .context("file_hash should be string")?;
    Ok(hash.to_string())
}

/// Return the first block hash from the first file entry in scan/review JSON output.
///
/// Input contract: JSON array with at least one file entry containing a non-empty `blocks` array.
pub fn first_block_hash(output: &str) -> Result<String> {
    let files = json_array(output)?;
    let file = files.first().context("Expected file in output")?;
    let blocks = file["blocks"]
        .as_array()
        .context("Blocks should be array")?;
    let hash = blocks.first().context("Expected block in output")?["hash"]
        .as_str()
        .context("Hash should be string")?;
    Ok(hash.to_string())
}

/// Return the first block hash and its file path from scan/review JSON output.
///
/// Input contract: JSON array with at least one file entry containing a non-empty `blocks` array.
pub fn first_block_info(output: &str) -> Result<(String, String)> {
    let files = json_array(output)?;
    let file = files.first().context("Expected file in output")?;
    let path = file["path"].as_str().context("Path should be string")?;
    let blocks = file["blocks"]
        .as_array()
        .context("Blocks should be array")?;
    let hash = blocks.first().context("Expected block in output")?["hash"]
        .as_str()
        .context("Hash should be string")?;
    Ok((hash.to_string(), path.to_string()))
}

/// Locate a tree node hash for the given path in scan --tree JSON output.
pub fn find_tree_hash(root: &Value, path: &str) -> Result<String> {
    find_tree_hash_inner(root, path)
        .with_context(|| format!("Tree node not found for path '{path}'"))
}

/// Depth-first search for a tree node hash by path.
fn find_tree_hash_inner(node: &Value, path: &str) -> Option<String> {
    let node_path = node.get("path")?.as_str()?;
    if node_path == path {
        return node
            .get("hash")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
    }

    let children = node.get("children")?.as_array()?;
    for child in children {
        if let Some(hash) = find_tree_hash_inner(child, path) {
            return Some(hash);
        }
    }
    None
}

pub fn read_review_records(path: &Path) -> Result<Vec<Record>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)?;
    // Note: Skips invalid JSON lines intentionally. Some tests put
    // corrupted data in the file to verify trueflow handles it gracefully.
    Ok(content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Record>(l).ok())
        .collect())
}

/// Overrides for building test review records with stable defaults.
#[derive(Debug, Clone, Default)]
pub struct ReviewRecordOverrides<'a> {
    pub id: Option<&'a str>,
    pub check: Option<&'a str>,
    pub verdict: Option<&'a str>,
    pub email: Option<&'a str>,
    pub timestamp: Option<i64>,
    pub repo_revision: Option<&'a str>,
    pub block_state: Option<&'a str>,
    pub attestations: Option<Value>,
}

/// Build a review record JSON value for tests.
pub fn build_review_record(fingerprint: &str, overrides: ReviewRecordOverrides<'_>) -> Value {
    let id = overrides
        .id
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let check = overrides.check.unwrap_or("review");
    let verdict = overrides.verdict.unwrap_or("approved");
    let email = overrides.email.unwrap_or("a@example.com");
    let repo_revision = overrides.repo_revision.unwrap_or("deadbeef");
    let block_state = overrides.block_state.unwrap_or("committed");
    let timestamp = overrides.timestamp.unwrap_or(0);
    let attestations = overrides.attestations.unwrap_or(Value::Null);

    serde_json::json!({
        "id": id,
        "version": 1,
        "fingerprint": fingerprint,
        "check": check,
        "verdict": verdict,
        "identity": { "type": "email", "email": email },
        "repo_ref": { "type": "vcs", "system": "git", "revision": repo_revision },
        "block_state": block_state,
        "timestamp": timestamp,
        "path_hint": null,
        "line_hint": null,
        "note": null,
        "tags": null,
        "attestations": attestations
    })
}

pub fn write_reviews_jsonl(dir: &Path, records: &[Value]) -> Result<()> {
    fs::create_dir_all(dir)?;
    let file = fs::File::create(dir.join("reviews.jsonl"))?;
    let mut writer = BufWriter::new(file);
    for record in records {
        serde_json::to_writer(&mut writer, record)?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}
