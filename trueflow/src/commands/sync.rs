use crate::context::TrueflowContext;
use crate::store::{FileStore, Record, ReviewStore};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

pub fn run(_context: &TrueflowContext) -> Result<()> {
    // 1. Fetch origin/trueflow-db to ensure we have the latest
    println!("Fetching from origin...");
    let _ = Command::new("git")
        .args(["fetch", "origin", "trueflow-db"])
        .output(); // Ignore error if branch doesn't exist

    // 2. Get Remote Content (if any)
    let remote_content = get_remote_content().ok();

    // 3. Get Local Content
    let store = FileStore::new()?;
    let local_records = store.read_history().unwrap_or_default();

    // 4. Merge
    let mut all_records = Vec::new();
    let mut seen_ids = HashSet::new();

    // Add remote records first (historical base)
    if let Some(content) = &remote_content {
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_str::<Record>(line)
                && seen_ids.insert(record.id.clone())
            {
                all_records.push(record);
            }
        }
    }

    // Add local records (new additions)
    for record in local_records {
        if seen_ids.insert(record.id.clone()) {
            all_records.push(record);
        }
    }

    // Sort by timestamp to ensure deterministic ordering (roughly)
    all_records.sort_by_key(|r| r.timestamp);

    // 5. Write back to local file
    let mut file_content = String::new();
    for record in &all_records {
        file_content.push_str(&serde_json::to_string(record)?);
        file_content.push('\n');
    }

    // We need to write to .trueflow/reviews.jsonl
    // FileStore uses absolute path logic, let's just reuse the file path logic or simple write
    // The FileStore::new() logic finds the root.
    // Let's assume we are in root or can use the store's knowledge if we exposed it?
    // FileStore doesn't expose path. But we can overwrite the file via store if we had a method?
    // Easier: Just overwrite .trueflow/reviews.jsonl in current dir?
    // Actually FileStore searches for .trueflow up the tree. We should respect that.
    // For now, let's just write to the file we read from?
    // We can't ask store for path.
    // Let's just create a new FileStore and write? No, append only.
    // We need "overwrite".
    // Let's just use `std::fs::write` to the path found by FileStore logic?
    // I'll copy the path finding logic briefly or expose it.
    // Exposing `root_path` from `FileStore` would be best.
    // But `FileStore` is in `store.rs`.

    // Hack: Assuming `FileStore::new()` worked, we can try to find the file again.
    let db_path = store.db_path();
    fs::write(&db_path, &file_content)?;

    // 6. Commit to Orphan Branch (Plumbing)
    println!("Preparing commit...");
    let blob_hash = git_hash_object(&file_content)?;
    let tree_hash = git_mktree(&blob_hash)?;

    // Parent is the current origin/trueflow-db tip if it exists
    let parent_hash = get_remote_head();

    let commit_hash = git_commit_tree(&tree_hash, parent_hash.as_deref(), "Sync reviews")?;

    // 7. Update local ref (so we track what we just synced)
    Command::new("git")
        .args(["update-ref", "refs/heads/trueflow-db", &commit_hash])
        .output()
        .context("Failed to update local trueflow-db ref")?;

    // 8. Push
    println!("Pushing to origin...");
    let push_status = Command::new("git")
        .args([
            "push",
            "origin",
            &format!("{}:refs/heads/trueflow-db", commit_hash),
        ])
        .status()
        .context("Failed to execute git push")?;

    if !push_status.success() {
        anyhow::bail!("Failed to push trueflow-db to origin (maybe conflict? try syncing again)");
    }

    println!("Sync complete.");
    Ok(())
}

fn get_remote_content() -> Result<String> {
    let output = Command::new("git")
        .args(["show", "origin/trueflow-db:reviews.jsonl"])
        .output()?;
    if output.status.success() {
        Ok(String::from_utf8(output.stdout)?)
    } else {
        Err(anyhow::anyhow!("Remote content not found"))
    }
}

fn get_remote_head() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "origin/trueflow-db"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
    } else {
        None
    }
}

fn git_hash_object(content: &str) -> Result<String> {
    let mut child = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    {
        let stdin = child.stdin.as_mut().context("Failed to open stdin")?;
        stdin.write_all(content.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn git_mktree(blob_hash: &str) -> Result<String> {
    let entry = format!("100644 blob {}\treviews.jsonl", blob_hash);
    let mut child = Command::new("git")
        .arg("mktree")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    {
        let stdin = child.stdin.as_mut().context("Failed to open stdin")?;
        stdin.write_all(entry.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn git_commit_tree(tree_hash: &str, parent: Option<&str>, message: &str) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("commit-tree").arg(tree_hash);

    if let Some(p) = parent {
        cmd.arg("-p").arg(p);
    }

    cmd.arg("-m").arg(message);

    let output = cmd.output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
