use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct TestRepo {
    path: PathBuf,
}

impl TestRepo {
    fn new(name: &str) -> Result<Self> {
        let path = std::env::temp_dir().join("trueflow_tests").join(name);
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;

        Command::new("git")
            .arg("init")
            .current_dir(&path)
            .output()
            .context("Failed to init git repo")?;

        // Config user
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

    fn trueflow_cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_trueflow"));
        cmd.current_dir(&self.path);
        cmd
    }
}

fn record(id: &str, fingerprint: &str, timestamp: i64) -> Value {
    serde_json::json!({
        "id": id,
        "version": 1,
        "fingerprint": fingerprint,
        "check": "review",
        "verdict": "approved",
        "identity": { "type": "email", "email": "test@example.com" },
        "repo_ref": { "type": "vcs", "system": "git", "revision": "deadbeef" },
        "block_state": "committed",
        "timestamp": timestamp,
        "path_hint": null,
        "line_hint": null,
        "note": null,
        "tags": null
    })
}

fn write_reviews(path: &Path, records: &[Value]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut content = String::new();
    for record in records {
        content.push_str(&serde_json::to_string(record)?);
        content.push('\n');
    }
    fs::write(path, content)?;
    Ok(())
}

fn read_remote_reviews(remote_dir: &Path) -> Result<Vec<Value>> {
    let output = Command::new("git")
        .args(["show", "trueflow-db:reviews.jsonl"])
        .current_dir(remote_dir)
        .output()?;
    let stdout = String::from_utf8(output.stdout)?;
    let mut records = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        records.push(serde_json::from_str::<Value>(line)?);
    }
    Ok(records)
}

#[test]
fn test_vet_sync() -> Result<()> {
    // 1. Create "Remote" Repo (bare)
    let remote_dir = std::env::temp_dir()
        .join("trueflow_tests")
        .join("remote_repo.git");
    if remote_dir.exists() {
        fs::remove_dir_all(&remote_dir)?;
    }
    fs::create_dir_all(&remote_dir)?;
    Command::new("git")
        .args(["init", "--bare"])
        .current_dir(&remote_dir)
        .output()?;

    // 2. Create "Local" Repo
    let local = TestRepo::new("local_repo")?;

    // Add remote
    Command::new("git")
        .args(["remote", "add", "origin", remote_dir.to_str().unwrap()])
        .current_dir(&local.path)
        .output()?;

    // 3. Create some vet data locally
    local
        .trueflow_cmd()
        .args([
            "mark",
            "--fingerprint",
            "fp1",
            "--verdict",
            "approved",
            "--quiet",
        ])
        .output()?;

    // 4. Sync (Push)
    // First sync might fail fetch (remote empty), but should push
    let status = local.trueflow_cmd().arg("sync").status()?;
    assert!(status.success(), "First sync (push) failed");

    // Verify remote has the branch
    let output = Command::new("git")
        .args(["branch"])
        .current_dir(&remote_dir)
        .output()?;
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("trueflow-db"));

    // 5. Clone another repo (simulating colleague)
    let colleague = TestRepo::new("colleague_repo")?;
    Command::new("git")
        .args(["remote", "add", "origin", remote_dir.to_str().unwrap()])
        .current_dir(&colleague.path)
        .output()?;

    // 6. Sync Colleague (Fetch)
    let status = colleague.trueflow_cmd().arg("sync").status()?;
    assert!(status.success(), "Colleague sync (fetch) failed");

    // Verify colleague has data
    let output = Command::new("git")
        .args(["show", "trueflow-db:reviews.jsonl"])
        .current_dir(&colleague.path)
        .output()?;
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("fp1"));
    assert!(stdout.contains("approved"));

    Ok(())
}

#[test]
fn test_sync_dedupes_and_sorts_records() -> Result<()> {
    let remote_dir = std::env::temp_dir()
        .join("trueflow_tests")
        .join("remote_repo_dedup.git");
    if remote_dir.exists() {
        fs::remove_dir_all(&remote_dir)?;
    }
    fs::create_dir_all(&remote_dir)?;
    Command::new("git")
        .args(["init", "--bare"])
        .current_dir(&remote_dir)
        .output()?;

    let local = TestRepo::new("local_repo_dedup")?;
    Command::new("git")
        .args(["remote", "add", "origin", remote_dir.to_str().unwrap()])
        .current_dir(&local.path)
        .output()?;

    let local_records = vec![record("dup", "fp-remote", 1500)];
    write_reviews(
        &local.path.join(".trueflow").join("reviews.jsonl"),
        &local_records,
    )?;
    assert!(local.trueflow_cmd().arg("sync").status()?.success());

    let colleague = TestRepo::new("colleague_repo_dedup")?;
    Command::new("git")
        .args(["remote", "add", "origin", remote_dir.to_str().unwrap()])
        .current_dir(&colleague.path)
        .output()?;

    let colleague_records = vec![
        record("dup", "fp-local", 2000),
        record("unique", "fp-unique", 1000),
    ];
    write_reviews(
        &colleague.path.join(".trueflow").join("reviews.jsonl"),
        &colleague_records,
    )?;
    assert!(colleague.trueflow_cmd().arg("sync").status()?.success());

    let records = read_remote_reviews(&remote_dir)?;
    let ids: Vec<&str> = records
        .iter()
        .filter_map(|record| record["id"].as_str())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(ids.iter().filter(|id| **id == "dup").count(), 1);
    assert!(ids.contains(&"unique"));

    let timestamps: Vec<i64> = records
        .iter()
        .filter_map(|record| record["timestamp"].as_i64())
        .collect();
    assert!(timestamps.windows(2).all(|pair| pair[0] <= pair[1]));

    Ok(())
}
