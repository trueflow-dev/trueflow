# List available recipes
default:
    @just --list

# Run the default local gate (tests, lint, fmt)
check: test lint fmt-check test-release-publication test-installer

# Run the faster no-test local gate (lint, fmt)
check-fast: lint fmt-check

# Run heavyweight code checks that are useful before CI / release work
check-heavy: audit doc coverage-check lint-full

# Run the broad local code gate (tests/examples/lint/docs/coverage; benches excluded)
check-code: test-code lint-code fmt-check audit doc coverage-check

# Run Nix packaging verification separately from the regular code checks
check-packaging: nix-check

# Measure check pipeline timings (writes under .trueflow/measurements/)
measure-check profile="check":
    ./scripts/measure-checks.sh --profile "{{profile}}"

# Measure the faster no-test local gate
measure-check-fast:
    ./scripts/measure-checks.sh --profile check-fast

# Measure the heavyweight local gate
measure-check-heavy:
    ./scripts/measure-checks.sh --profile check-heavy

# Measure the broad local code gate
measure-check-code:
    ./scripts/measure-checks.sh --profile check-code

# Measure the separate packaging gate
measure-check-packaging:
    ./scripts/measure-checks.sh --profile check-packaging

# Measure the legacy local developer alias
measure-local-dev:
    ./scripts/measure-checks.sh --profile local-dev

# Measure the legacy minimum local correctness alias
measure-local-minimum:
    ./scripts/measure-checks.sh --profile local-minimum

# Fix deterministic local issues
fix: fix-clippy fix-fmt fix-cargo

# Compile the local developer target set without running tests
compile-check:
    cd trueflow && cargo check --features tui-test-support --lib --bins --tests

# Normal local cargo gates enable tui-test-support so the hidden vt100/PTy
# TUI regression harness keeps compiling in the ordinary developer loop.

# Compile the broad code target set without benches
compile-check-code:
    cd trueflow && cargo check --features tui-test-support --lib --bins --tests --examples

# Run the local test suite with nextest
test:
    cd trueflow && cargo nextest run --features tui-test-support

# Run deterministic network-free release publication safety contracts
test-release-publication:
    sh scripts/tests/release-publication-safety.sh

# Run deterministic network-free installer safety contracts
test-installer:
    sh scripts/tests/installer-safety.sh

# Run the broad local test path without benches
test-code:
    cd trueflow && cargo nextest run --features tui-test-support --lib --bins --tests --examples

# Run the vt100-backed and PTY-backed TUI integration suite
test-tui-e2e:
    cd trueflow && cargo nextest run --features tui-test-support --test tui_vt100 --test tui_pty_smoke

# Run mutation tests
mutants:
    cd trueflow && cargo mutants

# Run clippy lints for the fast local gate
lint:
    cd trueflow && cargo clippy --features tui-test-support --lib --bins --tests -- -D warnings

# Run clippy across the broad local code target set without benches
lint-code:
    cd trueflow && cargo clippy --features tui-test-support --lib --bins --tests --examples -- -D warnings

# Run clippy across every feature and target, including benches
lint-full:
    cd trueflow && cargo clippy --all-features --all-targets -- -D warnings

# Check formatting
fmt-check:
    cd trueflow && cargo fmt --check --all

# Run cargo audit
audit:
    cd trueflow && cargo audit

# Build this crate's documentation without dependency docs
# to keep the heavyweight local path focused and fast.
doc:
    cd trueflow && cargo doc --features tui-test-support --no-deps

# Enforce minimum test coverage (line coverage) for the main crate target set.
coverage-check:
    cd trueflow && cargo llvm-cov --features tui-test-support --lib --bins --tests --summary-only --ignore-filename-regex "src/commands/tui.rs" --fail-under-lines 80

# Verify the default flake package builds for this host
nix-check:
    nix build --no-link .#default

# Verify only the native flake package builds
nix-check-native:
    nix build --no-link .#native

# Verify only the default flake package builds
nix-check-default:
    nix build --no-link .#default

# Verify only the static flake package builds
nix-check-static:
    nix build --no-link .#static

# Verify the release flake package builds
nix-check-release:
    nix build --no-link .#release

# Explicitly rerun package-build tests for the default flake package
nix-check-with-tests:
    nix build --no-link .#default-with-tests

# Explicitly rerun package-build tests for the native flake package
nix-check-native-with-tests:
    nix build --no-link .#native-with-tests

# Explicitly rerun package-build tests for the default flake package
nix-check-default-with-tests:
    nix build --no-link .#default-with-tests

# Explicitly rerun package-build tests for the static flake package
nix-check-static-with-tests:
    nix build --no-link .#static-with-tests

# Explicitly rerun package-build tests for the release flake package
nix-check-release-with-tests:
    nix build --no-link .#release-with-tests

# Package the native Linux x86_64 musl release artifact on Linux x86_64
package-linux-release:
    ./scripts/package-linux-release.sh

# Smoke-test a packaged release artifact
smoke-test-release artifact:
    ./scripts/smoke-test-release.sh "{{artifact}}"

# Fix clippy issues without pulling benches into the normal fix path
fix-clippy:
    cd trueflow && cargo clippy --features tui-test-support --lib --bins --tests --examples --fix --allow-dirty

# Format code
fix-fmt:
    cd trueflow && cargo fmt --all

# Fix audit issues
fix-audit:
    cd trueflow && cargo audit fix

# Run cargo fix without pulling benches into the normal fix path
fix-cargo:
    cd trueflow && cargo fix --features tui-test-support --lib --bins --tests --examples --allow-dirty

# Run benchmark fixture validation and benchmarks
bench:
    cd trueflow && cargo test --features bench --test e2e_bench_fixture && cargo bench --features bench

# Generate coverage report without pulling benches into the normal coverage path
coverage:
    cd trueflow && cargo llvm-cov --features tui-test-support --lib --bins --tests --examples --html
    @echo "Coverage report at trueflow/target/llvm-cov/html/index.html"
