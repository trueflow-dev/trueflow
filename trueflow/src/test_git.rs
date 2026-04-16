use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

pub(crate) struct CurrentDirGuard(PathBuf);

impl CurrentDirGuard {
    pub(crate) fn push(path: &Path) -> Self {
        let current = std::env::current_dir()
            .unwrap_or_else(|error| panic!("failed to read current directory: {error}"));
        std::env::set_current_dir(path)
            .unwrap_or_else(|error| panic!("failed to enter test directory {path:?}: {error}"));
        Self(current)
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0)
            .unwrap_or_else(|error| panic!("failed to restore current directory: {error}"));
    }
}

pub(crate) fn temp_test_dir(name: &str) -> PathBuf {
    std::env::temp_dir()
        .join("trueflow_tests")
        .join(name)
        .join(Uuid::new_v4().to_string())
}

pub(crate) fn temp_git_repo(name: &str) -> PathBuf {
    let path = temp_test_dir(name);
    init_git_repo(&path);
    path
}

fn init_git_repo(path: &Path) {
    fs::create_dir_all(path)
        .unwrap_or_else(|error| panic!("failed to create git test directory {path:?}: {error}"));
    run_git(path, &["init", "-q"]);
    run_git(path, &["config", "user.email", "test@example.com"]);
    run_git(path, &["config", "user.name", "Test User"]);
}

pub(crate) fn run_git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap_or_else(|error| panic!("failed to execute git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {:?} failed: {}{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn run_git_stdout(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap_or_else(|error| panic!("failed to execute git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {:?} failed: {}{}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|error| panic!("invalid UTF-8 from git {args:?}: {error}"))
}
