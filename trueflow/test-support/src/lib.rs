#[path = "fs_support.rs"]
mod fs_support;
#[path = "vt100_backend.rs"]
pub mod vt100_backend;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use trueflow::store::{Record, ReviewTargetRef};
use uuid::Uuid;

static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub struct TestRepo {
    pub path: PathBuf,
}

impl TestRepo {
    pub fn new(name: &str) -> Result<Self> {
        let path = temp_dir("trueflow_tests", name);
        fs::create_dir_all(&path)?;
        init_git_with_identity(&path, "test@example.com", "Test User")?;
        Ok(Self { path })
    }

    pub fn fixture(name: &str) -> Result<Self> {
        let path = prepare_fixture_repo(
            name,
            "trueflow_e2e",
            "test@example.com",
            "Test User",
            "test fixture",
        )?;
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

    pub fn run_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Result<String> {
        run_cmd_with_env(&self.path, args, envs)
    }

    pub fn run_in(&self, args: &[&str], dir: &Path) -> Result<String> {
        run_cmd(dir, args)
    }

    pub fn run_err(&self, args: &[&str]) -> Result<String> {
        let output = build_cmd(&self.path, args)?.output()?;
        if output.status.success() {
            anyhow::bail!("trueflow succeeded but expected failure");
        }
        Ok(String::from_utf8(output.stderr)?)
    }

    pub fn run_raw(&self, args: &[&str]) -> Result<std::process::Output> {
        Ok(build_cmd(&self.path, args)?.output()?)
    }
}

pub struct ReviewBenchRepo {
    pub path: PathBuf,
}

impl ReviewBenchRepo {
    pub fn fixture(name: &str) -> Result<Self> {
        let path = prepare_fixture_repo(
            name,
            "trueflow_review_bench",
            "bench@example.com",
            "Bench User",
            "benchmark fixture",
        )?;
        Ok(Self { path })
    }

    pub fn full_review_summary(&self) -> Result<trueflow::commands::review::ReviewSummary> {
        run_full_review(&self.path)
    }
}

impl Drop for ReviewBenchRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn temp_test_dir(name: &str) -> PathBuf {
    temp_dir("trueflow_tests", name)
}

fn temp_dir(base: &str, name: &str) -> PathBuf {
    std::env::temp_dir()
        .join(base)
        .join(name)
        .join(Uuid::new_v4().to_string())
}

fn fixture_source_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("example_repos")
        .join(name)
}

fn prepare_fixture_repo(
    name: &str,
    temp_base: &str,
    email: &str,
    user_name: &str,
    missing_label: &str,
) -> Result<PathBuf> {
    let src = fixture_source_dir(name);
    if !src.is_dir() {
        return Err(anyhow!("{missing_label} not found: {}", src.display()));
    }

    let path = temp_dir(temp_base, name);
    if path.exists() {
        fs::remove_dir_all(&path)?;
    }

    fs_support::copy_dir_all(&src, &path)?;
    init_git_with_identity(&path, email, user_name)?;
    Ok(path)
}

fn init_git_with_identity(path: &Path, email: &str, user_name: &str) -> Result<()> {
    run_git(path, &["init", "-q"])?;
    run_git(path, &["config", "user.email", email])?;
    run_git(path, &["config", "user.name", user_name])?;
    Ok(())
}

fn trueflow_bin() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_trueflow") {
        return Ok(PathBuf::from(path));
    }

    let test_exe = std::env::current_exe().context("failed to resolve current test executable")?;
    let debug_dir = test_exe
        .parent()
        .and_then(Path::parent)
        .context("failed to resolve target/debug directory from current test executable")?;
    let candidate = debug_dir.join(format!("trueflow{}", std::env::consts::EXE_SUFFIX));
    if candidate.is_file() {
        return Ok(candidate);
    }

    anyhow::bail!(
        "could not locate trueflow binary; checked CARGO_BIN_EXE_trueflow and {}",
        candidate.display()
    )
}

fn build_cmd(dir: &Path, args: &[&str]) -> Result<Command> {
    let mut cmd = Command::new(trueflow_bin()?);
    cmd.args(args).current_dir(dir);
    Ok(cmd)
}

fn run_cmd(dir: &Path, args: &[&str]) -> Result<String> {
    let output = build_cmd(dir, args)?.output()?;
    if !output.status.success() {
        anyhow::bail!(
            "trueflow failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn run_cmd_with_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String> {
    let mut cmd = build_cmd(dir, args)?;
    for (key, value) in envs {
        cmd.env(key, value);
    }

    let output = cmd.output()?;
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

/// Parse CLI JSON output into a file array.
///
/// Supports both legacy top-level arrays and scan-result objects with a
/// top-level `files` array.
pub fn json_array(output: &str) -> Result<Vec<Value>> {
    let json = json(output)?;
    if let Some(array) = json.as_array() {
        return Ok(array.clone());
    }
    json["files"]
        .as_array()
        .cloned()
        .context("Output should be array or object with files array")
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
/// Input contract: JSON array or scan-result object with at least one file entry
/// containing a `blocks` array.
pub fn first_file_blocks(output: &str) -> Result<Vec<Value>> {
    let files = json_array(output)?;
    let file = files.first().context("Expected file in output")?;
    Ok(file["blocks"]
        .as_array()
        .context("Blocks should be array")?
        .clone())
}

/// Return the `tree_hash` from the first file entry in scan JSON output.
///
/// Input contract: JSON array or scan-result object with at least one file entry
/// containing `tree_hash`.
pub fn first_file_tree_hash(output: &str) -> Result<String> {
    let files = json_array(output)?;
    let file = files.first().context("Expected file in output")?;
    let hash = file["tree_hash"]
        .as_str()
        .context("tree_hash should be string")?;
    Ok(hash.to_string())
}

/// Return the first block hash from the first file entry in scan/review JSON output.
///
/// Input contract: JSON array or scan-result object with at least one file entry
/// containing a non-empty `blocks` array.
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
/// Input contract: JSON array or scan-result object with at least one file entry
/// containing a non-empty `blocks` array.
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
    Ok(content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Record>(line).ok())
        .collect())
}

#[derive(Debug, Clone, Default)]
pub struct ReviewRecordOverrides<'a> {
    pub id: Option<&'a str>,
    pub check: Option<&'a str>,
    pub verdict: Option<&'a str>,
    pub email: Option<&'a str>,
    pub timestamp: Option<i64>,
    pub repo_revision: Option<&'a str>,
    pub block_state: Option<&'a str>,
    pub target_kind: Option<&'a str>,
    pub attestations: Option<Value>,
}

pub fn record_target_key(record: &Record) -> &str {
    match &record.target {
        ReviewTargetRef::Block { hash }
        | ReviewTargetRef::File { hash }
        | ReviewTargetRef::Tree { hash } => hash.as_str(),
    }
}

pub fn build_review_record(target_key: &str, overrides: ReviewRecordOverrides<'_>) -> Value {
    let id = overrides
        .id
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let check = overrides.check.unwrap_or("review");
    let verdict = overrides.verdict.unwrap_or("approved");
    let email = overrides.email.unwrap_or("a@example.com");
    let repo_revision = overrides.repo_revision.unwrap_or("deadbeef");
    let block_state = overrides.block_state.unwrap_or("committed");
    let target_kind = overrides.target_kind.unwrap_or("block");
    let timestamp = overrides.timestamp.unwrap_or(0);
    let attestations = overrides.attestations.unwrap_or(Value::Null);

    serde_json::json!({
        "id": id,
        "version": 2,
        "target": { "kind": target_kind, "hash": target_key },
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

pub fn run_full_review(path: &Path) -> Result<trueflow::commands::review::ReviewSummary> {
    with_current_dir(path, || {
        let query = trueflow::commands::review::resolve_review_request(
            trueflow::commands::review::ReviewRequest::AllFiles,
            trueflow::config::BlockFilters::default(),
            trueflow::scanner::ScanOptions::default(),
        )?;

        trueflow::commands::review::collect_review_summary(&query)
    })
}

fn with_current_dir<T>(path: &Path, action: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = CWD_LOCK
        .lock()
        .map_err(|error| anyhow!("current-directory lock poisoned: {error}"))?;
    let original = std::env::current_dir().context("failed to capture original cwd")?;
    std::env::set_current_dir(path)
        .with_context(|| format!("failed to switch cwd to {}", path.display()))?;

    let result = action();
    let restore_result = std::env::set_current_dir(&original)
        .with_context(|| format!("failed to restore cwd to {}", original.display()));

    match (result, restore_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(restore_error)) => Err(restore_error),
        (Err(error), Err(restore_error)) => {
            Err(error.context(format!("also failed to restore cwd: {restore_error}")))
        }
    }
}
