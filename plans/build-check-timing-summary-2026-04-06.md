# Build / Check Timing Summary — 2026-04-06

This note captures the measured state after splitting the local check gates and then restoring tests to the default `just check` path.

## Commits in this workstream

- `2581471` — `build: add check timing measurement tooling`
- `784a25b` — `Trim criterion bench compile surface`
- `e7250ef` — `build: split fast and full local check gates`
- `ebc5557` — `build: make recipe tests repo-aware`
- `e1b36d6` — `build: restore tests to default check gate`

## Original baseline

Measured with the original heavyweight `current-check` profile before the gate split:

- `current-check`: **2m48s**
  - `test`: `1m47s`
  - `lint`: `15s`
  - `fmt-check`: `0s`
  - `audit`: `4s`
  - `doc`: `4s`
  - `coverage-check`: `36s`
  - `nix-check-native`: `1s`
  - `nix-check-default`: `1s`

That baseline was captured by:
- measurement output: `.trueflow/measurements/baseline-current-check/`

## Current local gate timings

Measured on a clean tree after restoring tests to the default `check` path:

- `check`: **2m03s**
  - `test`: `1m48s`
  - `lint`: `15s`
  - `fmt-check`: `0s`
- `check-fast`: **13s**
  - `compile-check`: `12s`
  - `lint`: `0s`
  - `fmt-check`: `1s`

Measurement outputs:
- `.trueflow/measurements/clean-check-with-tests/`
- `.trueflow/measurements/clean-check-fast/`

## Net effect

Compared to the original 2m48s baseline:

- default `just check` is now **45s faster** while still running tests
- `just check-fast` provides a much smaller compile/lint/fmt loop for cases where tests are unnecessary

## Remaining heavy path

The heavyweight path is now explicitly separated from the default local gate.

Measured on a clean tree:

- `current-check` / full heavyweight path: **4m29s**
  - `test-full`: `8s`
  - `lint-all-targets`: `2s`
  - `fmt-check`: `0s`
  - `audit`: `3s`
  - `doc`: `40s`
  - `coverage-check`: `36s`
  - `nix-check-native`: `1m27s`
  - `nix-check-default`: `1m33s`

Measurement output:
- `.trueflow/measurements/clean-current-check-after-restore/`

## Next optimization target

The heavyweight path is now dominated by the two nix package builds.

The next workstream should focus on:
1. understanding why each nix package build still recompiles so much work
2. determining whether package builds should run `doCheck`
3. preserving packaging correctness while reducing local wall clock for `nix-check-*`
