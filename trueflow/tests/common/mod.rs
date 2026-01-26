#![allow(dead_code)]

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
        let status = Command::new("git")
            .args(args)
            .current_dir(&self.path)
            .status()?;
        if !status.success() {
            anyhow::bail!("git {:?} failed", args);
        }
        Ok(())
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
    let run = |args: &[&str]| -> Result<()> {
        let status = Command::new("git").args(args).current_dir(path).status()?;
        if !status.success() {
            anyhow::bail!("git {:?} failed during init", args);
        }
        Ok(())
    };

    run(&["init", "-q"])?;
    run(&["config", "user.email", "test@example.com"])?;
    run(&["config", "user.name", "Test User"])?;
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

// JSON helpers

pub fn json(output: &str) -> Result<Value> {
    serde_json::from_str(output).context("Invalid JSON")
}

pub fn json_array(output: &str) -> Result<Vec<Value>> {
    json(output)?
        .as_array()
        .cloned()
        .context("Output should be array")
}

pub fn first_file_blocks(output: &str) -> Result<Vec<Value>> {
    let files = json_array(output)?;
    let file = files.first().context("Expected file in output")?;
    Ok(file["blocks"]
        .as_array()
        .context("Blocks should be array")?
        .clone())
}

pub fn first_file_hash(output: &str) -> Result<String> {
    let files = json_array(output)?;
    let file = files.first().context("Expected file in output")?;
    let hash = file["file_hash"]
        .as_str()
        .context("file_hash should be string")?;
    Ok(hash.to_string())
}

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

pub fn find_tree_hash(output: &str, path: &str) -> Result<String> {
    let root = json(output)?;
    find_tree_hash_inner(&root, path)
        .with_context(|| format!("Tree node not found for path '{path}'"))
}

fn find_tree_hash_inner(node: &Value, path: &str) -> Option<String> {
    let node_path = node.get("path")?.as_str()?;
    if node_path == path {
        return node.get("hash").and_then(|value| value.as_str()).map(|value| value.to_string());
    }

    let children = node.get("children")?.as_array()?;
    for child in children {
        if let Some(hash) = find_tree_hash_inner(child, path) {
            return Some(hash);
        }
    }
    None
}

pub fn read_review_records(path: &Path) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path)?;
    Ok(content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}
