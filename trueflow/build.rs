#[path = "src/build_metadata.rs"]
mod build_metadata;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    emit_git_rerun_hints();
    println!("cargo:rustc-env=TRUEFLOW_GIT_COMMIT={}", git_commit_hash());

    let source_date_epoch = env::var("SOURCE_DATE_EPOCH").ok();
    println!(
        "cargo:rustc-env=TRUEFLOW_BUILD_TIMESTAMP={}",
        build_metadata::build_timestamp_from_source_date_epoch(source_date_epoch.as_deref())
    );
}

fn emit_git_rerun_hints() {
    let Some(manifest_dir) = env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from) else {
        return;
    };
    let Some(repo_root) = manifest_dir.parent() else {
        return;
    };
    let git_dir = repo_root.join(".git");
    let head_path = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head_path.display());
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );

    let Ok(head_contents) = fs::read_to_string(&head_path) else {
        return;
    };
    let Some(reference) = head_contents.strip_prefix("ref: ").map(str::trim) else {
        return;
    };
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join(reference).display()
    );
}

fn git_commit_hash() -> String {
    run_command("git", &["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into())
}

fn run_command(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
