# Contributing

Contributor and internal development workflow docs live here instead of the public top-level `README.md`.

If you are just using trueflow, start with `README.md` or <https://trueflow.dev>.

For website/deployment/infrastructure operator docs, see `infra/README.md`.

## Development

```sh
# Default local gate (tests, lint, fmt)
nix develop -c just check

# Faster no-test local gate
nix develop -c just check-fast

# Broad local code verification path
nix develop -c just check-code

# Separate Nix packaging verification
nix develop -c just check-packaging

# Verify the host-default flake package directly
nix develop -c just nix-check

# Optional: verify the explicit release/static flake package
nix develop -c just nix-check-release

# Optional: rerun buildRustPackage's package-level test phase explicitly
nix develop -c just nix-check-with-tests

# Capture timing breakdowns for the current gate definitions
nix develop -c just measure-check
nix develop -c just measure-check-fast
nix develop -c just measure-check-code

# Generate a coverage report
nix develop -c just coverage
```

`just check` is the default local gate: tests, lint, and format checks.
`just check-fast` keeps the faster compile-only path for cases where you want a quicker no-test loop.
`just check-code` runs the broader lib/bin/test/example code path plus audit, docs, and coverage, while still excluding benches.
That code-focused path builds only this crate's docs (not dependency docs) and measures coverage for the main lib/bin/test target set.
Bench targets are opt-in and only run through `just bench`.
Normal compile/test/lint/doc/coverage recipes enable `tui-test-support` so the hidden vt100/PTy TUI regression harness keeps compiling in ordinary local gates.
`just check-packaging` runs host-default Nix packaging verification separately from the regular code checks.
`just nix-check` validates the host-default flake package (`native` on Darwin, release/static on Linux).
`just nix-check-release` is the explicit release/static package build.
`just nix-check-with-tests` is the explicit opt-in path for rerunning crate tests inside the host-default Nix package build.
Timing artifacts are written under `.trueflow/measurements/`.

The coverage report is written to `trueflow/target/llvm-cov/html/index.html`.
