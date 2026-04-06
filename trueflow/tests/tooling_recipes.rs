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
fn flake_nix_package_build_policy_is_explicit() -> Result<()> {
    let flake_path = repo_root()?.join("flake.nix");
    if !flake_path.exists() {
        return Ok(());
    }
    let flake = fs::read_to_string(flake_path)?;

    assert_contains(
        &flake,
        "trueflow = mkTrueflowPackage {\n          rustPlatform = rustPlatform;\n          doCheck = false;\n        };",
        "flake native package build policy",
    );
    assert_contains(
        &flake,
        "trueflowWithTests = mkTrueflowPackage {\n          rustPlatform = rustPlatform;\n          doCheck = true;\n        };",
        "flake native package with tests policy",
    );
    assert_contains(
        &flake,
        "trueflowMusl = mkTrueflowPackage {\n          rustPlatform = pkgs.pkgsStatic.rustPlatform;\n          cargoBuildTarget = \"${pkgs.pkgsStatic.stdenv.hostPlatform.config}\";\n          doCheck = false;\n        };",
        "flake static package build policy",
    );
    assert_contains(
        &flake,
        "trueflowMuslWithTests = mkTrueflowPackage {\n          rustPlatform = pkgs.pkgsStatic.rustPlatform;\n          cargoBuildTarget = \"${pkgs.pkgsStatic.stdenv.hostPlatform.config}\";\n          doCheck = true;\n        };",
        "flake static package with tests policy",
    );
    assert_contains(
        &flake,
        "defaultPackage = if pkgs.stdenv.isDarwin then trueflow else trueflowMusl;",
        "flake conditional default package alias",
    );
    assert_contains(
        &flake,
        "defaultPackageWithTests = if pkgs.stdenv.isDarwin then trueflowWithTests else trueflowMuslWithTests;",
        "flake conditional default package-with-tests alias",
    );
    assert_contains(
        &flake,
        "packages.default = defaultPackage;",
        "flake default package output",
    );
    assert_contains(
        &flake,
        "packages.static = trueflowMusl;",
        "flake explicit static package output",
    );
    assert_contains(
        &flake,
        "packages.release = trueflowMusl;",
        "flake explicit release package output",
    );
    assert_contains(
        &flake,
        "packages.\"native-with-tests\" = trueflowWithTests;",
        "flake explicit native package-with-tests output",
    );
    assert_contains(
        &flake,
        "packages.\"default-with-tests\" = defaultPackageWithTests;",
        "flake explicit default package-with-tests output",
    );
    assert_contains(
        &flake,
        "packages.\"static-with-tests\" = trueflowMuslWithTests;",
        "flake explicit static package-with-tests output",
    );
    assert_contains(
        &flake,
        "packages.\"release-with-tests\" = trueflowMuslWithTests;",
        "flake explicit release package-with-tests output",
    );
    assert_contains(
        &flake,
        "apps.default = flake-utils.lib.mkApp { drv = defaultPackage; };",
        "flake default app follows default package",
    );
    assert_contains(
        &flake,
        "commonNativeBuildInputs = [ pkgs.pkg-config ];",
        "flake package native build tools",
    );
    assert_contains(
        &flake,
        "buildInputs = [ ];",
        "flake package build inputs are empty",
    );
    assert_contains(
        &flake,
        "\"--remap-path-prefix=$NIX_BUILD_TOP=/build\"",
        "flake nix build root remap flag",
    );
    assert_contains(
        &flake,
        "\"--remap-path-prefix=$PWD=/build/source\"",
        "flake source root remap flag",
    );
    assert_contains(&flake, "preConfigure = ''", "flake rust path remap hook");
    assert_contains(
        &flake,
        "export RUSTFLAGS=\"''${RUSTFLAGS:+$RUSTFLAGS }${rustPathRemapFlagsString}\"",
        "flake rustflags export with remap prefixes",
    );
    Ok(())
}

#[test]
fn cargo_manifest_bench_support_is_opt_in() -> Result<()> {
    let cargo_toml_path = repo_root()?.join("trueflow/Cargo.toml");
    if !cargo_toml_path.exists() {
        return Ok(());
    }
    let cargo_toml = fs::read_to_string(cargo_toml_path)?;

    assert_contains(
        &cargo_toml,
        "bench = [\"dep:criterion\"]",
        "Cargo.toml bench feature",
    );
    assert_contains(
        &cargo_toml,
        "criterion = { version = \"0.8.2\", optional = true, default-features = false, features = [\"cargo_bench_support\"] }",
        "Cargo.toml optional criterion dependency",
    );
    assert_not_contains(
        &cargo_toml,
        "[dev-dependencies]\ncriterion =",
        "Cargo.toml dev-dependencies criterion leak",
    );
    assert_contains(
        &cargo_toml,
        "required-features = [\"bench\"]",
        "Cargo.toml bench target required feature",
    );

    Ok(())
}

#[test]
fn justfile_fast_and_code_gates_match_build_time_contract() -> Result<()> {
    let justfile_path = repo_root()?.join("Justfile");
    if !justfile_path.exists() {
        return Ok(());
    }
    let justfile = fs::read_to_string(justfile_path)?;

    assert_contains(
        &justfile,
        "check: test lint fmt-check",
        "Justfile default check recipe",
    );
    assert_contains(
        &justfile,
        "check-fast: compile-check lint fmt-check",
        "Justfile check-fast recipe",
    );
    assert_contains(
        &justfile,
        "check-heavy: audit doc coverage-check",
        "Justfile check-heavy recipe",
    );
    assert_contains(
        &justfile,
        "check-code: test-code lint-code fmt-check audit doc coverage-check",
        "Justfile check-code recipe",
    );
    assert_contains(
        &justfile,
        "check-packaging: nix-check",
        "Justfile check-packaging recipe",
    );
    assert_contains(
        &justfile,
        "compile-check:\n    cd trueflow && cargo check --features tui-test-support --lib --bins --tests\n",
        "Justfile compile-check recipe",
    );
    assert_contains(
        &justfile,
        "test:\n    cd trueflow && cargo nextest run --features tui-test-support\n",
        "Justfile test recipe",
    );
    assert_contains(
        &justfile,
        "test-code:\n    cd trueflow && cargo nextest run --features tui-test-support --lib --bins --tests --examples\n",
        "Justfile test-code recipe",
    );
    assert_contains(
        &justfile,
        "doc:\n    cd trueflow && cargo doc --features tui-test-support --no-deps\n",
        "Justfile doc recipe",
    );
    assert_contains(
        &justfile,
        "lint:\n    cd trueflow && cargo clippy --features tui-test-support --lib --bins --tests -- -D warnings\n",
        "Justfile lint recipe",
    );
    assert_contains(
        &justfile,
        "compile-check-code:\n    cd trueflow && cargo check --features tui-test-support --lib --bins --tests --examples\n",
        "Justfile compile-check-code recipe",
    );
    assert_contains(
        &justfile,
        "lint-code:\n    cd trueflow && cargo clippy --features tui-test-support --lib --bins --tests --examples -- -D warnings\n",
        "Justfile lint-code recipe",
    );
    assert_contains(
        &justfile,
        "fix-clippy:\n    cd trueflow && cargo clippy --features tui-test-support --lib --bins --tests --examples --fix --allow-dirty\n",
        "Justfile fix-clippy recipe",
    );
    assert_contains(
        &justfile,
        "fix-cargo:\n    cd trueflow && cargo fix --features tui-test-support --lib --bins --tests --examples --allow-dirty\n",
        "Justfile fix-cargo recipe",
    );
    assert_contains(
        &justfile,
        "bench:\n    cd trueflow && cargo test --features bench --test e2e_bench_fixture && cargo bench --features bench\n",
        "Justfile bench recipe",
    );
    assert_contains(
        &justfile,
        "coverage:\n    cd trueflow && cargo llvm-cov --features tui-test-support --lib --bins --tests --examples --html\n",
        "Justfile coverage recipe",
    );
    assert_contains(
        &justfile,
        "nix-check:\n    nix build --no-link .#default\n",
        "Justfile nix-check recipe",
    );
    assert_contains(
        &justfile,
        "nix-check-static:\n    nix build --no-link .#static\n",
        "Justfile nix-check-static recipe",
    );
    assert_contains(
        &justfile,
        "nix-check-release:\n    nix build --no-link .#release\n",
        "Justfile nix-check-release recipe",
    );
    assert_contains(
        &justfile,
        "nix-check-with-tests:\n    nix build --no-link .#default-with-tests\n",
        "Justfile nix-check-with-tests recipe",
    );
    assert_contains(
        &justfile,
        "nix-check-native-with-tests:\n    nix build --no-link .#native-with-tests\n",
        "Justfile nix-check-native-with-tests recipe",
    );
    assert_contains(
        &justfile,
        "nix-check-default-with-tests:\n    nix build --no-link .#default-with-tests\n",
        "Justfile nix-check-default-with-tests recipe",
    );
    assert_contains(
        &justfile,
        "nix-check-static-with-tests:\n    nix build --no-link .#static-with-tests\n",
        "Justfile nix-check-static-with-tests recipe",
    );
    assert_contains(
        &justfile,
        "nix-check-release-with-tests:\n    nix build --no-link .#release-with-tests\n",
        "Justfile nix-check-release-with-tests recipe",
    );

    assert_contains(
        &justfile,
        "tui-test-support so the hidden vt100/PTy\n# TUI regression harness keeps compiling in the ordinary developer loop.",
        "Justfile tui-test-support explanation",
    );
    assert_not_contains(
        &justfile,
        "compile-check-all-targets:",
        "Justfile legacy compile-check-all-targets recipe",
    );
    assert_not_contains(
        &justfile,
        "lint-all-targets:",
        "Justfile legacy lint-all-targets recipe",
    );
    assert_not_contains(
        &justfile,
        "test-full:",
        "Justfile legacy test-full recipe",
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
        "check\ncheck-fast\ncheck-heavy\ncheck-code\ncheck-packaging\nlocal-minimum\nlocal-dev",
        "measure-check profile list",
    );
    assert_contains(
        &measure_script,
        "compile-check-code\ntest\ntest-code\nlint\nlint-code",
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
        "    lint-code|lint-all-targets)\n      printf '%s\\n' 'cd trueflow && cargo clippy --features tui-test-support --lib --bins --tests --examples -- -D warnings'",
        "measure-check lint-code stage",
    );
    assert_contains(
        &measure_script,
        "    check|local-dev)\n      printf '%s\\n' test lint fmt-check",
        "measure-check default check profile",
    );
    assert_contains(
        &measure_script,
        "    check-fast|local-minimum)\n      printf '%s\\n' compile-check lint fmt-check",
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
        "    check-heavy)\n      printf '%s\\n' audit doc coverage-check",
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

    Ok(())
}
