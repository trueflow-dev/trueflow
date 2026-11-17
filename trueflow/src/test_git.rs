use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use uuid::Uuid;

static CWD_MUTEX: Mutex<()> = Mutex::new(());

pub(crate) struct CurrentDirGuard {
    previous: PathBuf,
    _lock: MutexGuard<'static, ()>,
}

impl CurrentDirGuard {
    pub(crate) fn push(path: &Path) -> Self {
        let lock = CWD_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = std::env::current_dir()
            .unwrap_or_else(|error| panic!("failed to read current directory: {error}"));
        std::env::set_current_dir(path)
            .unwrap_or_else(|error| panic!("failed to enter test directory {path:?}: {error}"));
        Self {
            previous: current,
            _lock: lock,
        }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
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

/// A `--depth 1` clone of `source`, so its HEAD is the shallow boundary.
pub(crate) fn shallow_clone(source: &Path) -> PathBuf {
    let clone = temp_test_dir("shallow_clone").join("clone");
    let clone_arg = clone
        .to_str()
        .unwrap_or_else(|| panic!("clone path must be UTF-8: {clone:?}"));
    let source_url = format!("file://{}", source.display());
    run_git(
        source,
        &["clone", "-q", "--depth", "1", &source_url, clone_arg],
    );
    clone
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
