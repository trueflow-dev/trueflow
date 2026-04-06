use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

fn repo_root() -> Result<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("crate should live under the repo root")
}

fn assert_contains(haystack: &str, needle: &str, context: &str) {
    assert!(
        haystack.contains(needle),
        "expected {context} to contain {needle:?}"
    );
}

#[test]
fn justfile_fast_and_full_gates_match_build_time_contract() -> Result<()> {
    let justfile_path = repo_root()?.join("Justfile");
    if !justfile_path.exists() {
        return Ok(());
    }
    let justfile = fs::read_to_string(justfile_path)?;

    assert_contains(
        &justfile,
        "check: compile-check lint fmt-check",
        "Justfile fast check recipe",
    );
    assert_contains(
        &justfile,
        "check-dev: test lint fmt-check",
        "Justfile check-dev recipe",
    );
    assert_contains(
        &justfile,
        "check-heavy: audit doc coverage-check nix-check",
        "Justfile check-heavy recipe",
    );
    assert_contains(
        &justfile,
        "check-full: test-full lint-all-targets fmt-check audit doc coverage-check nix-check",
        "Justfile check-full recipe",
    );
    assert_contains(
        &justfile,
        "compile-check:\n    cd trueflow && cargo check --all-features --lib --bins --tests\n",
        "Justfile compile-check recipe",
    );
    assert_contains(
        &justfile,
        "test:\n    cd trueflow && cargo nextest run --all-features\n",
        "Justfile test recipe",
    );
    assert_contains(
        &justfile,
        "test-full:\n    cd trueflow && cargo nextest run --all-features --all-targets\n",
        "Justfile test-full recipe",
    );
    assert_contains(
        &justfile,
        "lint:\n    cd trueflow && cargo clippy --all-features --lib --bins --tests -- -D warnings\n",
        "Justfile lint recipe",
    );
    assert_contains(
        &justfile,
        "lint-all-targets:\n    cd trueflow && cargo clippy --all-features --all-targets -- -D warnings\n",
        "Justfile lint-all-targets recipe",
    );

    Ok(())
}

#[test]
fn measurement_profiles_and_stage_commands_match_recipe_split() -> Result<()> {
    let measure_script_path = repo_root()?.join("scripts/measure-checks.sh");
    if !measure_script_path.exists() {
        return Ok(());
    }
    let measure_script = fs::read_to_string(measure_script_path)?;

    assert_contains(
        &measure_script,
        "check\ncheck-dev\ncheck-heavy\ncheck-full\ncurrent-check\nlocal-minimum\nlocal-dev",
        "measure-check profile list",
    );
    assert_contains(
        &measure_script,
        "compile-check-all-targets\ntest\ntest-full\nlint\nlint-all-targets",
        "measure-check stage list",
    );
    assert_contains(
        &measure_script,
        "    compile-check)\n      printf '%s\\n' 'cd trueflow && cargo check --all-features --lib --bins --tests'",
        "measure-check compile-check stage",
    );
    assert_contains(
        &measure_script,
        "    test)\n      printf '%s\\n' 'cd trueflow && cargo nextest run --all-features'",
        "measure-check test stage",
    );
    assert_contains(
        &measure_script,
        "    test-full)\n      printf '%s\\n' 'cd trueflow && cargo nextest run --all-features --all-targets'",
        "measure-check test-full stage",
    );
    assert_contains(
        &measure_script,
        "    lint)\n      printf '%s\\n' 'cd trueflow && cargo clippy --all-features --lib --bins --tests -- -D warnings'",
        "measure-check lint stage",
    );
    assert_contains(
        &measure_script,
        "    lint-all-targets)\n      printf '%s\\n' 'cd trueflow && cargo clippy --all-features --all-targets -- -D warnings'",
        "measure-check lint-all-targets stage",
    );
    assert_contains(
        &measure_script,
        "    check|local-minimum)\n      printf '%s\\n' compile-check lint fmt-check",
        "measure-check fast check profile",
    );
    assert_contains(
        &measure_script,
        "    check-dev|local-dev)\n      printf '%s\\n' test lint fmt-check",
        "measure-check check-dev profile",
    );
    assert_contains(
        &measure_script,
        "    check-full|current-check)\n      printf '%s\\n' test-full lint-all-targets fmt-check audit doc coverage-check nix-check-native nix-check-default",
        "measure-check check-full profile",
    );

    Ok(())
}
