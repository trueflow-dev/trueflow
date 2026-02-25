use crate::config::load as load_config;
use crate::context::TrueflowContext;
use crate::store::{FileStore, Record, ReviewStore};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use tracing::info;

pub fn run(_context: &TrueflowContext) -> Result<()> {
    let config = load_config()?;
    let branch = config.storage.branch.trim();
    if branch.is_empty() {
        anyhow::bail!("storage.branch cannot be empty");
    }

    // 1. Fetch origin storage branch to ensure we have the latest
    info!("Fetching from origin...");
    let _ = Command::new("git")
        .args(["fetch", "origin", branch])
        .output(); // Ignore error if branch doesn't exist

    // 2. Get Remote Content (if any)
    let remote_content = get_remote_content(branch).ok();

    // 3. Get Local Content
    let store = FileStore::new()?;
    let local_records = store
        .read_history()
        .context("Failed to read local review history")?;

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

    // Write content with exclusive lock
    use fs2::FileExt;
    let db_path = store.db_path();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&db_path)?;

    file.lock_exclusive()?;
    file.write_all(file_content.as_bytes())?;
    // Lock releases on drop

    // 6. Commit to Orphan Branch (Plumbing)
    info!("Preparing commit...");
    let blob_hash = git_hash_object(&file_content)?;
    let tree_hash = git_mktree(&blob_hash)?;

    // Parent is the current remote storage branch tip if it exists
    let parent_hash = get_remote_head(branch);

    let commit_hash = git_commit_tree(&tree_hash, parent_hash.as_deref(), "Sync reviews")?;

    // 7. Update local ref (so we track what we just synced)
    let local_ref = format!("refs/heads/{branch}");
    Command::new("git")
        .args(["update-ref", &local_ref, &commit_hash])
        .output()
        .with_context(|| format!("Failed to update local {branch} ref"))?;

    // 8. Push
    info!("Pushing to origin...");
    let push_status = Command::new("git")
        .args([
            "push",
            "origin",
            &format!("{commit_hash}:refs/heads/{branch}"),
        ])
        .status()
        .context("Failed to execute git push")?;

    if !push_status.success() {
        anyhow::bail!("Failed to push {branch} to origin (maybe conflict? try syncing again)");
    }

    info!("Sync complete.");
    Ok(())
}

fn get_remote_content(branch: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["show", &format!("origin/{branch}:reviews.jsonl")])
        .output()?;
    if output.status.success() {
        Ok(String::from_utf8(output.stdout)?)
    } else {
        Err(anyhow::anyhow!("Remote content not found"))
    }
}

fn get_remote_head(branch: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", &format!("origin/{branch}")])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
    } else {
        None
    }
}

fn git_hash_object(content: &str) -> Result<String> {
    git_hash_object_in_dir(content, None)
}

fn git_hash_object_in_dir(content: &str, current_dir: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    if let Some(dir) = current_dir {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn()?;

    {
        let stdin = child.stdin.as_mut().context("Failed to open stdin")?;
        stdin.write_all(content.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let blob_hash = String::from_utf8(output.stdout)?.trim().to_string();
    if blob_hash.is_empty() {
        anyhow::bail!("git hash-object returned empty output");
    }
    Ok(blob_hash)
}

fn git_mktree(blob_hash: &str) -> Result<String> {
    git_mktree_in_dir(blob_hash, None)
}

fn git_mktree_in_dir(blob_hash: &str, current_dir: Option<&Path>) -> Result<String> {
    let entry = format!("100644 blob {blob_hash}\treviews.jsonl");
    let mut cmd = Command::new("git");
    cmd.arg("mktree")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    if let Some(dir) = current_dir {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn()?;

    {
        let stdin = child.stdin.as_mut().context("Failed to open stdin")?;
        stdin.write_all(entry.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git mktree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let tree_hash = String::from_utf8(output.stdout)?.trim().to_string();
    if tree_hash.is_empty() {
        anyhow::bail!("git mktree returned empty output");
    }
    Ok(tree_hash)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_hash_object_errors_when_git_fails() -> Result<()> {
        let temp_dir = std::env::temp_dir().join(format!(
            "trueflow-sync-hash-object-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir)?;

        let result = git_hash_object_in_dir("payload\n", Some(&temp_dir));

        std::fs::remove_dir_all(&temp_dir)?;
        assert!(
            result.is_err(),
            "git_hash_object should fail outside a git repository"
        );
        Ok(())
    }

    #[test]
    fn git_mktree_errors_for_invalid_blob_hash() {
        let result = git_mktree("definitely-not-a-blob-hash");
        assert!(
            result.is_err(),
            "git_mktree should fail for invalid blob hash input"
        );
    }
}
