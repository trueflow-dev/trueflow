# List available recipes
default:
    @just --list

# Run the fast default local gate (compile, lint, fmt)
check: compile-check lint fmt-check

# Run the fast local developer loop (tests, lint, fmt)
check-dev: test lint fmt-check

# Run heavyweight checks that are useful before CI / release work
check-heavy: audit doc coverage-check nix-check

# Run the full local verification path
check-full: test-full lint-all-targets fmt-check audit doc coverage-check nix-check

# Measure check pipeline timings (writes under .trueflow/measurements/)
measure-check profile="check":
    ./scripts/measure-checks.sh --profile "{{profile}}"

# Measure the fast local developer loop
measure-check-dev:
    ./scripts/measure-checks.sh --profile check-dev

# Measure the heavyweight local gate
measure-check-heavy:
    ./scripts/measure-checks.sh --profile check-heavy

# Measure the full local gate
measure-check-full:
    ./scripts/measure-checks.sh --profile check-full

# Measure the legacy local developer alias
measure-local-dev:
    ./scripts/measure-checks.sh --profile local-dev

# Measure the legacy minimum local correctness alias
measure-local-minimum:
    ./scripts/measure-checks.sh --profile local-minimum

# Fix all auto-fixable issues
fix: fix-clippy fix-fmt fix-audit fix-cargo

# Compile the local developer target set without running tests
compile-check:
    cd trueflow && cargo check --all-features --lib --bins --tests

# Compile every target, including benches
compile-check-all-targets:
    cd trueflow && cargo check --all-features --all-targets

# Run the local test suite with nextest
test:
    cd trueflow && cargo nextest run --all-features

# Run the broader all-targets test compile path
test-full:
    cd trueflow && cargo nextest run --all-features --all-targets

# Run mutation tests
mutants:
    cd trueflow && cargo mutants

# Run clippy lints for the fast local gate
lint:
    cd trueflow && cargo clippy --all-features --lib --bins --tests -- -D warnings

# Run clippy across all targets, including benches
lint-all-targets:
    cd trueflow && cargo clippy --all-features --all-targets -- -D warnings

# Check formatting
fmt-check:
    cd trueflow && cargo fmt --check --all

# Run cargo audit
audit:
    cd trueflow && cargo audit

# Build documentation
doc:
    cd trueflow && cargo doc --all-features

# Enforce minimum test coverage (line coverage)
coverage-check:
    cd trueflow && cargo llvm-cov --all-features --all-targets --summary-only --ignore-filename-regex "src/commands/tui.rs" --fail-under-lines 80

# Verify flake packages build
nix-check: nix-check-native nix-check-default

# Verify the native flake package builds
nix-check-native:
    nix build .#native

# Verify the default flake package builds
nix-check-default:
    nix build .#default

# Fix clippy issues
fix-clippy:
    cd trueflow && cargo clippy --all-targets --all-features --fix --allow-dirty

# Format code
fix-fmt:
    cd trueflow && cargo fmt --all

# Fix audit issues
fix-audit:
    cd trueflow && cargo audit fix

# Run cargo fix
fix-cargo:
    cd trueflow && cargo fix --all-targets --all-features --allow-dirty

# Run benchmarks
bench:
    cd trueflow && cargo bench

# Generate coverage report
coverage:
    cd trueflow && cargo llvm-cov --all-targets --html
    @echo "Coverage report at trueflow/target/llvm-cov/html/index.html"
