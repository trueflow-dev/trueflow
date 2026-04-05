use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use uuid::Uuid;

#[path = "fs_support.rs"]
mod fs_support;

use trueflow::commands::review::{
    ReviewRequest, ReviewSummary, collect_review_summary, resolve_review_request,
};
use trueflow::config::BlockFilters;
use trueflow::scanner::ScanOptions;

static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub(crate) struct ReviewBenchRepo {
    pub path: PathBuf,
}

impl ReviewBenchRepo {
    pub(crate) fn fixture(name: &str) -> Result<Self> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("example_repos")
            .join(name);
        if !src.is_dir() {
            return Err(anyhow!("benchmark fixture not found: {}", src.display()));
        }

        let path = temp_dir("trueflow_review_bench", name);
        fs_support::copy_dir_all(&src, &path)?;
        init_git(&path)?;

        Ok(Self { path })
    }

    pub(crate) fn full_review_summary(&self) -> Result<ReviewSummary> {
        run_full_review(&self.path)
    }
}

impl Drop for ReviewBenchRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn run_full_review(path: &Path) -> Result<ReviewSummary> {
    with_current_dir(path, || {
        let query = resolve_review_request(
            ReviewRequest::AllFiles,
            BlockFilters::default(),
            ScanOptions::default(),
        )?;

        collect_review_summary(&query)
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

fn temp_dir(base: &str, name: &str) -> PathBuf {
    std::env::temp_dir()
        .join(base)
        .join(name)
        .join(Uuid::new_v4().to_string())
}

fn init_git(path: &Path) -> Result<()> {
    run_git(path, &["init", "-q"])?;
    run_git(path, &["config", "user.email", "bench@example.com"])?;
    run_git(path, &["config", "user.name", "Bench User"])?;
    Ok(())
}

fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git").args(args).current_dir(dir).output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}
