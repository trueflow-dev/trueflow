use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

// Helpers to create a temp directory/repo
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

        // Init git repo
        Command::new("git")
            .arg("init")
            .current_dir(&path)
            .output()
            .context("Failed to init git repo")?;

        // Config basic user
        Command::new("git")
            .args(["config", "user.email", "you@example.com"])
            .current_dir(&path)
            .output()?;
        Command::new("git")
            .args(["config", "user.name", "Your Name"])
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
            .args(["add", "."])
            .current_dir(&self.path)
            .output()?;

        Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(&self.path)
            .output()?;
        Ok(())
    }

    fn run_trueflow(&self, args: &[&str]) -> Result<String> {
        let output = self.trueflow_cmd().args(args).output()?;
        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "trueflow failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8(output.stdout)?)
    }

    fn diff_json(&self) -> Result<Vec<Value>> {
        let output = self.run_trueflow(&["diff", "--json"])?;
        let json: Value = serde_json::from_str(&output)?;
        Ok(json.as_array().context("Expected array")?.clone())
    }

    fn trueflow_cmd(&self) -> Command {
        // Assume `trueflow` is built and accessible via cargo run or target/debug/trueflow
        // For E2E tests in cargo, typically we look for env!("CARGO_BIN_EXE_trueflow")
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_trueflow"));
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
        .args(["checkout", "-b", "feature/add-greeting"])
        .current_dir(&repo.path)
        .output()?;

    repo.write_file("src/main.rs", "fn main() { println!(\"Hello World\"); }")?;
    repo.git_add_commit("Update greeting")?;

    // Now main has "Hello", feature has "Hello World".
    // vet diff should show the hunk.

    let changes = repo.diff_json()?;

    // Validate we have 1 change
    assert_eq!(changes.len(), 1);

    let change = &changes[0];
    assert_eq!(change["file"].as_str().context("file")?, "src/main.rs");
    assert_eq!(change["status"].as_str().context("status")?, "unreviewed");

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
        .args(["checkout", "-b", "feature/sub"])
        .current_dir(&repo.path)
        .output()?;
    repo.write_file(
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\npub fn sub(a: i32, b: i32) -> i32 { a - b }",
    )?;
    repo.git_add_commit("Add sub")?;

    // 1. Get Diff
    let changes = repo.diff_json()?;
    let fp = changes[0]["fingerprint"].as_str().unwrap().to_string();

    // 2. Mark Approved
    let status = repo
        .trueflow_cmd()
        .arg("mark")
        .arg("--fingerprint")
        .arg(&fp)
        .arg("--verdict")
        .arg("approved")
        .status()?;
    assert!(status.success());

    // 3. Verify Diff is Empty
    let changes = repo.diff_json()?;
    assert!(changes.is_empty());

    // 4. Mark Rejected
    let status = repo
        .trueflow_cmd()
        .arg("mark")
        .arg("--fingerprint")
        .arg(&fp)
        .arg("--verdict")
        .arg("rejected")
        .status()?;
    assert!(status.success());

    // 5. Verify Diff shows Rejected
    let changes = repo.diff_json()?;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["status"].as_str().context("status")?, "rejected");

    Ok(())
}

#[test]
fn test_check_command_gates_unreviewed_changes() -> Result<()> {
    let repo = TestRepo::new("check_gate")?;
    repo.write_file(
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )?;
    repo.git_add_commit("Initial")?;

    Command::new("git")
        .args(["checkout", "-b", "feature/check"])
        .current_dir(&repo.path)
        .output()?;

    repo.write_file(
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\npub fn sub(a: i32, b: i32) -> i32 { a - b }\n",
    )?;
    repo.git_add_commit("Add sub")?;

    let status = repo.trueflow_cmd().arg("check").status()?;
    assert!(
        !status.success(),
        "check should fail with unreviewed changes"
    );

    let changes = repo.diff_json()?;
    let fp = changes[0]["fingerprint"].as_str().expect("fingerprint");

    let status = repo
        .trueflow_cmd()
        .arg("mark")
        .arg("--fingerprint")
        .arg(fp)
        .arg("--verdict")
        .arg("approved")
        .status()?;
    assert!(status.success(), "mark should succeed");

    let status = repo.trueflow_cmd().arg("check").status()?;
    assert!(status.success(), "check should pass after approval");

    Ok(())
}

#[test]
fn test_diff_ignores_non_review_checks() -> Result<()> {
    let repo = TestRepo::new("diff_non_review")?;
    repo.write_file(
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )?;
    repo.git_add_commit("Initial")?;

    Command::new("git")
        .args(["checkout", "-b", "feature/security"])
        .current_dir(&repo.path)
        .output()?;

    repo.write_file(
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\npub fn sub(a: i32, b: i32) -> i32 { a - b }\n",
    )?;
    repo.git_add_commit("Add sub")?;

    let changes = repo.diff_json()?;
    let fp = changes[0]["fingerprint"].as_str().context("fingerprint")?;

    let status = repo
        .trueflow_cmd()
        .args([
            "mark",
            "--fingerprint",
            fp,
            "--verdict",
            "approved",
            "--check",
            "security",
        ])
        .status()?;
    assert!(status.success());

    let changes = repo.diff_json()?;
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0]["status"].as_str().context("status")?,
        "unreviewed"
    );

    Ok(())
}

#[test]
fn test_diff_ignores_untracked_files() -> Result<()> {
    let repo = TestRepo::new("diff_untracked")?;
    repo.write_file("src/lib.rs", "pub fn stable() {}\n")?;
    repo.git_add_commit("Initial")?;

    repo.write_file("src/untracked.rs", "pub fn draft() {}\n")?;

    let changes = repo.diff_json()?;
    assert!(changes.is_empty());

    Ok(())
}

#[test]
fn test_diff_handles_renamed_file() -> Result<()> {
    let repo = TestRepo::new("diff_rename")?;
    repo.write_file("src/old.rs", "pub fn alpha() {}\n")?;
    repo.git_add_commit("Add alpha")?;

    Command::new("git")
        .args(["checkout", "-b", "feature/rename"])
        .current_dir(&repo.path)
        .output()?;

    Command::new("git")
        .args(["mv", "src/old.rs", "src/new.rs"])
        .current_dir(&repo.path)
        .output()?;
    repo.write_file("src/new.rs", "pub fn alpha() {}\npub fn beta() {}\n")?;
    repo.git_add_commit("Rename and expand")?;

    let changes = repo.diff_json()?;
    assert!(!changes.is_empty());
    assert!(changes.iter().any(|change| {
        change["file"]
            .as_str()
            .map(|path| path == "src/new.rs")
            .unwrap_or(false)
    }));

    Ok(())
}

#[test]
fn test_diff_skips_binary_changes() -> Result<()> {
    let repo = TestRepo::new("diff_binary")?;
    let binary_path = repo.path.join("binary.bin");
    fs::write(&binary_path, [0, 255, 0, 1])?;
    repo.git_add_commit("Add binary")?;

    Command::new("git")
        .args(["checkout", "-b", "feature/binary"])
        .current_dir(&repo.path)
        .output()?;

    fs::write(&binary_path, [0, 255, 2, 3])?;
    repo.git_add_commit("Update binary")?;

    let changes = repo.diff_json()?;
    assert!(changes.is_empty());

    Ok(())
}

#[test]
fn test_diff_errors_without_main_branch() -> Result<()> {
    let repo = TestRepo::new("diff_no_main")?;
    repo.write_file("src/lib.rs", "pub fn core() {}\n")?;
    repo.git_add_commit("Initial")?;

    Command::new("git")
        .args(["branch", "-m", "trunk"])
        .current_dir(&repo.path)
        .output()?;

    let output = repo.trueflow_cmd().arg("diff").arg("--json").output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("main") || stderr.contains("master"));

    Ok(())
}
