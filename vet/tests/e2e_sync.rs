use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct TestRepo {
    path: PathBuf,
}

impl TestRepo {
    fn new(name: &str) -> Result<Self> {
        let path = std::env::temp_dir().join("vet_tests").join(name);
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
        Command::new("git").args(&["config", "user.email", "test@example.com"]).current_dir(&path).output()?;
        Command::new("git").args(&["config", "user.name", "Test User"]).current_dir(&path).output()?;

        Ok(Self { path })
    }

    fn vet_cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vet"));
        cmd.current_dir(&self.path);
        cmd
    }
}

#[test]
fn test_vet_sync() -> Result<()> {
    // 1. Create "Remote" Repo (bare)
    let remote_dir = std::env::temp_dir().join("vet_tests").join("remote_repo.git");
    if remote_dir.exists() {
        fs::remove_dir_all(&remote_dir)?;
    }
    fs::create_dir_all(&remote_dir)?;
    Command::new("git")
        .args(&["init", "--bare"])
        .current_dir(&remote_dir)
        .output()?;

    // 2. Create "Local" Repo
    let local = TestRepo::new("local_repo")?;
    
    // Add remote
    Command::new("git")
        .args(&["remote", "add", "origin", remote_dir.to_str().unwrap()])
        .current_dir(&local.path)
        .output()?;

    // 3. Create some vet data locally
    local.vet_cmd()
        .args(&["mark", "--fingerprint", "fp1", "--verdict", "approved"])
        .output()?;
        
    // 4. Sync (Push)
    // First sync might fail fetch (remote empty), but should push
    let status = local.vet_cmd().arg("sync").status()?;
    assert!(status.success(), "First sync (push) failed");
    
    // Verify remote has the branch
    let output = Command::new("git")
        .args(&["branch"])
        .current_dir(&remote_dir)
        .output()?;
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("vet-db"));

    // 5. Clone another repo (simulating colleague)
    let colleague = TestRepo::new("colleague_repo")?;
    Command::new("git")
        .args(&["remote", "add", "origin", remote_dir.to_str().unwrap()])
        .current_dir(&colleague.path)
        .output()?;
        
    // 6. Sync Colleague (Fetch)
    let status = colleague.vet_cmd().arg("sync").status()?;
    assert!(status.success(), "Colleague sync (fetch) failed");
    
    // Verify colleague has data
    let output = Command::new("git")
        .args(&["show", "vet-db:reviews.jsonl"])
        .current_dir(&colleague.path)
        .output()?;
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("fp1"));
    assert!(stdout.contains("approved"));

    Ok(())
}
