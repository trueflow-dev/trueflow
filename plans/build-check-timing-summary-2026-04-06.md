# Build / Check Timing Summary — 2026-04-06

This note captures the measured state after splitting the local check gates, restoring tests to the default `just check` path, and then reducing local Nix package verification cost.

## Commits in this workstream

- `2581471` — `build: add check timing measurement tooling`
- `784a25b` — `Trim criterion bench compile surface`
- `e7250ef` — `build: split fast and full local check gates`
- `ebc5557` — `build: make recipe tests repo-aware`
- `e1b36d6` — `build: restore tests to default check gate`
- `73be453` — `build: speed up nix package verification`

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

## Heavy path before the Nix optimization

Before the Nix package-verification change, the heavyweight path was measured on a clean tree as:

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

## Heavy path after the Nix optimization

After `73be453` (`build: speed up nix package verification`), the heavyweight path now uses:

- combined `nix build --no-link .#native .#default`
- ordinary package outputs with `doCheck = false`
- explicit `*-with-tests` package outputs when package-level test reruns are desired

Measured on a clean tree:

- `check-heavy`: **1m25s**
  - `audit`: `3s`
  - `doc`: `45s`
  - `coverage-check`: `36s`
  - `nix-check`: `1s`
- `current-check` / full heavyweight path: **2m58s**
  - `test-full`: `2m04s`
  - `lint-all-targets`: `16s`
  - `fmt-check`: `0s`
  - `audit`: `3s`
  - `doc`: `1s`
  - `coverage-check`: `34s`
  - `nix-check`: `0s`

Measurement outputs:
- `.trueflow/measurements/nix-optimized-check-heavy/`
- `.trueflow/measurements/nix-optimized-current-check/`

## Nix-specific timing note

The clean-tree measurements above are cache-warm and show the practical local win after the first successful build.

For a stricter package-build view, the forced rebuild investigation showed:

- before the Nix package-verification change, rebuild logs showed roughly:
  - `.#native`: build `~1m35s` + check `~49s`
  - `.#default`: build `~1m34s` + check `~52s`
- after the change on current `main`:
  - `nix build --rebuild --no-link .#native`: **55s**
  - `nix build --rebuild --no-link .#default`: **58s**

Both forced rebuild commands currently end with a Nix nondeterminism check failure rather than a clean success, so these rebuild timings are useful for comparison but also indicate a separate reproducibility problem to investigate.

## Net effect on the heavyweight path

Compared to the pre-Nix-optimization clean-tree heavyweight baseline:

- `current-check` improved from **4m29s** to **2m58s**
- the heavyweight path is no longer dominated by local Nix package verification after the first build
- the main remaining costs are now:
  - `test-full`
  - `doc` / `coverage-check` on colder runs
  - a separate Nix nondeterminism issue during forced rebuilds

## Next optimization target

The next likely high-value directions are now:
1. investigate the Nix nondeterminism reported by `nix build --rebuild --no-link`
2. decide whether Darwin should really build both `native` and `default` package shapes in local heavy checks
3. investigate why the Darwin `default` package closure is so much larger than `native`

Related investigation note:
- `plans/nix-build-time-investigation-candidate-plan-2026-04-06.md`
