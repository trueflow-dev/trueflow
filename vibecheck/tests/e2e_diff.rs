use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

// Helpers to create a temp directory/repo
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

        // Init git repo
        Command::new("git")
            .arg("init")
            .current_dir(&path)
            .output()
            .context("Failed to init git repo")?;

        // Config basic user
        Command::new("git")
            .args(&["config", "user.email", "you@example.com"])
            .current_dir(&path)
            .output()?;
        Command::new("git")
            .args(&["config", "user.name", "Your Name"])
            .current_dir(&path)
            .output()?;

        Ok(Self { path })
    }

    fn write_file(&self, filename: &str, content: &str) -> Result<()> {
        let p = self.path.join(filename);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(p, content)?;
        Ok(())
    }

    fn git_add_commit(&self, msg: &str) -> Result<()> {
        Command::new("git")
            .args(&["add", "."])
            .current_dir(&self.path)
            .output()?;
        
        Command::new("git")
            .args(&["commit", "-m", msg])
            .current_dir(&self.path)
            .output()?;
        Ok(())
    }

    fn vet_cmd(&self) -> Command {
        // Assume `vet` is built and accessible via cargo run or target/debug/vet
        // For E2E tests in cargo, typically we look for env!("CARGO_BIN_EXE_vet")
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vet"));
        cmd.current_dir(&self.path);
        cmd
    }
}

#[test]
fn test_vet_diff_initial_state() -> Result<()> {
    let repo = TestRepo::new("initial_state")?;

    // 1. Create a file and commit it to main
    repo.write_file("src/main.rs", "fn main() { println!(\"Hello\"); }")?;
    repo.git_add_commit("Initial commit")?;

    // 2. Run `vet diff --json`
    // Since we just added code and haven't reviewed it, vet should show it as unreviewed.
    // Wait... if it's the initial commit, does it show up in diff?
    // "vet diff" usually compares HEAD vs main.
    // If we are ON main, diff main..HEAD is empty.
    // Ah, design says: "Get diff HEAD vs main".
    // If I just committed to main, HEAD == main.
    // So usually we vet a feature branch.
    
    // Let's create a feature branch.
    Command::new("git")
        .args(&["checkout", "-b", "feature/add-greeting"])
        .current_dir(&repo.path)
        .output()?;
    
    repo.write_file("src/main.rs", "fn main() { println!(\"Hello World\"); }")?;
    repo.git_add_commit("Update greeting")?;

    // Now main has "Hello", feature has "Hello World".
    // vet diff should show the hunk.

    let output = repo.vet_cmd()
        .arg("diff")
        .arg("--json")
        .output()?;
    
    assert!(output.status.success(), "vet diff failed: {:?}", output);
    let stdout = String::from_utf8(output.stdout)?;
    println!("Output: {}", stdout);

    let changes: serde_json::Value = serde_json::from_str(&stdout)?;
    
    // Validate we have 1 change
    assert!(changes.is_array());
    let changes_arr = changes.as_array().unwrap();
    assert_eq!(changes_arr.len(), 1);
    
    let change = &changes_arr[0];
    assert_eq!(change["file"], "src/main.rs");
    assert_eq!(change["status"], "unreviewed");
    
    let content = change["diff_content"].as_str().unwrap();
    assert!(content.contains("Hello World"));
    assert!(content.contains("-fn main() { println!(\"Hello\"); }"));
    
    Ok(())
}

#[test]
fn test_vet_mark_flow() -> Result<()> {
    let repo = TestRepo::new("mark_flow")?;
    repo.write_file("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }")?;
    repo.git_add_commit("Initial")?;
    
    // Feature
    Command::new("git")
        .args(&["checkout", "-b", "feature/sub"])
        .current_dir(&repo.path)
        .output()?;
    repo.write_file("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 { a + b }\npub fn sub(a: i32, b: i32) -> i32 { a - b }")?;
    repo.git_add_commit("Add sub")?;
    
    // 1. Get Diff
    let output = repo.vet_cmd().arg("diff").arg("--json").output()?;
    let changes: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let fp = changes[0]["fingerprint"].as_str().unwrap().to_string();
    
    // 2. Mark Approved
    let status = repo.vet_cmd()
        .arg("mark")
        .arg("--fingerprint")
        .arg(&fp)
        .arg("--verdict")
        .arg("approved")
        .status()?;
    assert!(status.success());
    
    // 3. Verify Diff is Empty
    let output = repo.vet_cmd().arg("diff").arg("--json").output()?;
    let changes: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(changes.as_array().unwrap().len(), 0);
    
    // 4. Mark Rejected
    let status = repo.vet_cmd()
        .arg("mark")
        .arg("--fingerprint")
        .arg(&fp)
        .arg("--verdict")
        .arg("rejected")
        .status()?;
    assert!(status.success());
    
    // 5. Verify Diff shows Rejected
    let output = repo.vet_cmd().arg("diff").arg("--json").output()?;
    let changes: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let arr = changes.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["status"], "rejected");
    
    Ok(())
}
