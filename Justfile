# List available recipes
default:
    @just --list

# Run all checks (test, lint, fmt, audit, doc)
check: test lint fmt-check audit doc

# Fix all auto-fixable issues
fix: fix-clippy fix-fmt fix-audit fix-cargo

# Run tests
test:
    cd trueflow && cargo test --all-features --all-targets

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
