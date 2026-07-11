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

fn assert_not_contains(haystack: &str, needle: &str, context: &str) {
    assert!(
        !haystack.contains(needle),
        "expected {context} to not contain {needle:?}"
    );
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
        "check\ncheck-fast\ncheck-heavy\ncheck-code\ncheck-packaging\ncurrent-check\ncheck-full\nlocal-minimum\nlocal-dev",
        "measure-check profile list",
    );
    assert_contains(
        &measure_script,
        "compile-check\ncompile-check-code\ncompile-check-all-targets\ntest\ntest-code\ntest-full\nlint\nlint-code\nlint-full\nlint-all-targets",
        "measure-check stage list",
    );
    assert_contains(
        &measure_script,
        "nix-check\nnix-check-native\nnix-check-default\nnix-check-static\nnix-check-release\nnix-check-native-with-tests\nnix-check-default-with-tests\nnix-check-static-with-tests\nnix-check-release-with-tests",
        "measure-check nix stage list",
    );
    assert_contains(
        &measure_script,
        "    compile-check)\n      printf '%s\\n' 'cd trueflow && cargo check --features tui-test-support --lib --bins --tests'",
        "measure-check compile-check stage",
    );
    assert_contains(
        &measure_script,
        "    test)\n      printf '%s\\n' 'cd trueflow && cargo nextest run --features tui-test-support'",
        "measure-check test stage",
    );
    assert_contains(
        &measure_script,
        "    compile-check-code|compile-check-all-targets)\n      printf '%s\\n' 'cd trueflow && cargo check --features tui-test-support --lib --bins --tests --examples'",
        "measure-check compile-check-code stage",
    );
    assert_contains(
        &measure_script,
        "    test-code|test-full)\n      printf '%s\\n' 'cd trueflow && cargo nextest run --features tui-test-support --lib --bins --tests --examples'",
        "measure-check test-code stage",
    );
    assert_contains(
        &measure_script,
        "    doc)\n      printf '%s\\n' 'cd trueflow && cargo doc --features tui-test-support --no-deps'",
        "measure-check doc stage",
    );
    assert_contains(
        &measure_script,
        "    lint)\n      printf '%s\\n' 'cd trueflow && cargo clippy --features tui-test-support --lib --bins --tests -- -D warnings'",
        "measure-check lint stage",
    );
    assert_contains(
        &measure_script,
        "    coverage-check)\n      printf '%s\\n' 'cd trueflow && cargo llvm-cov --features tui-test-support --lib --bins --tests --summary-only --ignore-filename-regex \"src/commands/tui.rs\" --fail-under-lines 80'",
        "measure-check coverage-check stage",
    );
    assert_contains(
        &measure_script,
        "    lint-code)\n      printf '%s\\n' 'cd trueflow && cargo clippy --features tui-test-support --lib --bins --tests --examples -- -D warnings'",
        "measure-check lint-code stage",
    );
    assert_contains(
        &measure_script,
        "    lint-full|lint-all-targets)\n      printf '%s\\n' 'cd trueflow && cargo clippy --all-features --all-targets -- -D warnings'",
        "measure-check lint-full stage",
    );
    assert_contains(
        &measure_script,
        "    check|local-dev)\n      printf '%s\\n' test lint fmt-check",
        "measure-check default check profile",
    );
    assert_contains(
        &measure_script,
        "    check-fast|local-minimum)\n      printf '%s\\n' lint fmt-check",
        "measure-check check-fast profile",
    );
    assert_contains(
        &measure_script,
        "    nix-check)\n      printf '%s\\n' 'nix build --no-link .#default'",
        "measure-check nix-check stage",
    );
    assert_contains(
        &measure_script,
        "    nix-check-static)\n      printf '%s\\n' 'nix build --no-link .#static'",
        "measure-check nix-check-static stage",
    );
    assert_contains(
        &measure_script,
        "    nix-check-release)\n      printf '%s\\n' 'nix build --no-link .#release'",
        "measure-check nix-check-release stage",
    );
    assert_contains(
        &measure_script,
        "    nix-check-native-with-tests)\n      printf '%s\\n' 'nix build --no-link .#native-with-tests'",
        "measure-check nix-check-native-with-tests stage",
    );
    assert_contains(
        &measure_script,
        "    nix-check-default-with-tests)\n      printf '%s\\n' 'nix build --no-link .#default-with-tests'",
        "measure-check nix-check-default-with-tests stage",
    );
    assert_contains(
        &measure_script,
        "    nix-check-static-with-tests)\n      printf '%s\\n' 'nix build --no-link .#static-with-tests'",
        "measure-check nix-check-static-with-tests stage",
    );
    assert_contains(
        &measure_script,
        "    nix-check-release-with-tests)\n      printf '%s\\n' 'nix build --no-link .#release-with-tests'",
        "measure-check nix-check-release-with-tests stage",
    );
    assert_contains(
        &measure_script,
        "    check-heavy)\n      printf '%s\\n' audit doc coverage-check lint-full",
        "measure-check check-heavy profile",
    );
    assert_contains(
        &measure_script,
        "    check-packaging)\n      printf '%s\\n' nix-check",
        "measure-check check-packaging profile",
    );
    assert_contains(
        &measure_script,
        "    check-code|check-full|current-check)\n      printf '%s\\n' test-code lint-code fmt-check audit doc coverage-check",
        "measure-check check-code profile",
    );
    assert_contains(
        &measure_script,
        "  bash -c \"cd '$repo_root' && set -euo pipefail && $command_text\" \\",
        "measure-check stage runner avoids login shell startup files",
    );
    assert_not_contains(
        &measure_script,
        "bash -lc",
        "measure-check stage runner should not source login shell startup files",
    );
    assert_contains(
        &measure_script,
        "clock_gettime(CLOCK_MONOTONIC)",
        "measure-check stage durations should use a monotonic clock",
    );
    assert_not_contains(
        &measure_script,
        "SECONDS=0",
        "measure-check stage durations should not use wall-clock-sensitive Bash SECONDS",
    );
    assert_not_contains(
        &measure_script,
        "duration=$SECONDS",
        "measure-check stage durations should not use wall-clock-sensitive Bash SECONDS",
    );

    Ok(())
}
