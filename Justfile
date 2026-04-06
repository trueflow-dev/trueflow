# List available recipes
default:
    @just --list

# Run all checks (test, lint, fmt, audit, doc, coverage, flake builds)
check: test lint fmt-check audit doc coverage-check nix-check

# Measure check pipeline timings (writes under .trueflow/measurements/)
measure-check profile="current-check":
    ./scripts/measure-checks.sh --profile "{{profile}}"

# Measure a smaller local developer loop
measure-local-dev:
    ./scripts/measure-checks.sh --profile local-dev

# Measure the minimum local correctness gate
measure-local-minimum:
    ./scripts/measure-checks.sh --profile local-minimum

# Fix all auto-fixable issues
fix: fix-clippy fix-fmt fix-audit fix-cargo

# Run tests with nextest
test:
    cd trueflow && cargo nextest run --all-features --all-targets


# Run mutation tests
mutants:
    cd trueflow && cargo mutants


# Run clippy lints
lint:
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
nix-check:
    nix build .#native
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
