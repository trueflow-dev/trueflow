#[path = "src/build_metadata.rs"]
mod build_metadata;
#[path = "src/build_script_support.rs"]
mod build_script_support;

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
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
    let head_contents = manifest_dir
        .as_ref()
        .and_then(|path| path.parent())
        .map(|repo_root| repo_root.join(".git").join("HEAD"))
        .and_then(|head_path| fs::read_to_string(head_path).ok());

    for path in build_script_support::git_rerun_hint_paths(
        manifest_dir.as_deref(),
        head_contents.as_deref(),
    ) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn git_commit_hash() -> String {
    run_command("git", &["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into())
}

fn run_command(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    build_script_support::normalized_command_stdout(output.status.success(), &output.stdout)
}
