use std::path::{Path, PathBuf};
use std::process::Command;

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
