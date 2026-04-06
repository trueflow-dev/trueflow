# Build / Check Timing Summary — 2026-04-06

This note captures the measured state after splitting the local check gates, restoring tests to the default `just check` path, and then reducing local Nix package verification cost.

## Commits in this workstream

- `2581471` — `build: add check timing measurement tooling`
- `784a25b` — `Trim criterion bench compile surface`
- `e7250ef` — `build: split fast and full local check gates`
- `ebc5557` — `build: make recipe tests repo-aware`
- `e1b36d6` — `build: restore tests to default check gate`
- `73be453` — `build: speed up nix package verification`
- `01ab510` — `build: use SOURCE_DATE_EPOCH for build metadata`
- `f6fe7f0` — `nix: remap rust build paths for reproducibility`
- `8e48dd6` — `nix: make darwin default package native`
- `5aa67c4` — `nix: drop unnecessary darwin package inputs`
- `3b2fa46` — `build: narrow doc and coverage heavy checks`

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

Those rebuild timings directly identified the later reproducibility work.

## Nix reproducibility follow-up

The Nix nondeterminism was later fixed in two steps:

- `01ab510` — switched build metadata to `SOURCE_DATE_EPOCH` instead of wall-clock time
- `f6fe7f0` — added Rust `--remap-path-prefix` flags in the flake package builds

After those two changes, forced rebuilds succeeded cleanly again for both package outputs:

- `nix build --rebuild --no-link .#native`
- `nix build --rebuild --no-link .#default`

## Darwin default package follow-up

The remaining large local performance issue on `aarch64-darwin` was that `packages.default` still pointed at the static/release-style package.

That was changed in:

- `8e48dd6` — `nix: make darwin default package native`

Current package shape on Darwin:

- `packages.default` -> `native`
- explicit `packages.release` / `packages.static` -> static Darwin package
- `apps.default` follows `packages.default`

Current output sizes on Darwin:

- `.#default` / `.#native`: **63.5 MiB**
- `.#release`: **1.2 GiB**

Clean-tree custom stage timing run (cache-warm after build):

- `.trueflow/measurements/darwin-default-stage-compare/`
  - `nix-check`: `0s`
  - `nix-check-release`: `1s`
  - `nix-check-default-with-tests`: `1s`

Forced rebuild timings on current `main`:

- `nix build --rebuild --no-link .#default`: **35s**
- `nix build --rebuild --no-link .#release`: **3m02s**
- `nix build --rebuild --no-link .#default-with-tests`: **58s**

This means the default local Darwin Nix path now validates the native package instead of paying the static/release package cost.

## Darwin release package closure follow-up

The remaining Darwin release/static package bloat turned out to be caused by unnecessary explicit package `buildInputs`.

That was fixed in:

- `5aa67c4` — `nix: drop unnecessary darwin package inputs`

Resulting package/output changes on Darwin:

- `.#release` / `.#static` closure size: **1.2 GiB** -> **19.9 MiB**
- `.#release` references: now empty
- `.#release` binary links only:
  - `CoreFoundation`
  - `/usr/lib/libSystem.B.dylib`

Forced rebuild timings in a clean worktree at the same code state:

- `nix build --rebuild --no-link .#release`: **55s**
- `nix build --rebuild --no-link .#default`: **50s**
- `nix build --rebuild --no-link .#default-with-tests`: **87s**

This reduced the Darwin release/static package from a giant propagated SDK closure to a small executable output while preserving successful native and release builds.

## Native Darwin libiconv follow-up

I also investigated whether the native Darwin package could drop its `libiconv` reference.

Result: no safe project-level change was landed.

Reason:

- the native output directly references `/nix/store/.../libiconv.2.dylib`
- `nix why-depends` shows a direct dependency edge from the package output to `libiconv`
- `otool -L` shows a direct `LC_LOAD_DYLIB` entry for the Nix `libiconv` dylib

So unlike the release/static closure issue, this is not propagated metadata bloat. It is a real runtime link in the native binary.

## Heavy-path doc / coverage follow-up

The next heavy-path optimization targeted the two remaining cold-path bottlenecks:

- dependency doc generation
- all-targets coverage instrumentation

That was changed in:

- `3b2fa46` — `build: narrow doc and coverage heavy checks`

New behavior:

- `doc` -> `cargo doc --all-features --no-deps`
- `coverage-check` -> `cargo llvm-cov --all-features --lib --bins --tests --summary-only ...`

Cold-path experiments in the clean green worktree showed:

- `cargo doc --all-features`: **8m54s**
- `cargo doc --all-features --no-deps`: **16s**
- old coverage-check (`--all-targets`): **2m29s**
- new coverage-check (`--lib --bins --tests`): **54s**

Fresh clean-tree profile measurements after `3b2fa46`:

- `check-heavy`: **2m33s**
  - `audit`: `3s`
  - `doc`: `24s`
  - `coverage-check`: `1m00s`
  - `nix-check`: `1m06s`
- `current-check`: **1m18s**
  - `test-full`: `31s`
  - `lint-all-targets`: `6s`
  - `fmt-check`: `0s`
  - `audit`: `3s`
  - `doc`: `2s`
  - `coverage-check`: `35s`
  - `nix-check`: `1s`

Measurement outputs:
- `.trueflow/measurements/doccov-optimized-check-heavy/`
- `.trueflow/measurements/doccov-optimized-current-check/`
- `.trueflow/measurements/doccov-optimized-stages/`

## Net effect on the heavyweight path

Compared to the pre-Nix-optimization clean-tree heavyweight baseline:

- `current-check` improved from **4m29s** to **2m58s** after the first gate split / Nix package-test change
- in the later clean green worktree with narrowed docs and coverage, `current-check` measured **1m18s**
- reproducible forced rebuilds now succeed again
- on Darwin, the default local Nix path dropped from the old static/release-style cost to the native package cost:
  - `.#default` forced rebuild: **50s**
  - `.#release` forced rebuild: **55s**
- after narrowing docs and coverage, the cold heavy path in the clean green worktree dropped from **11m29s** to **2m33s**
- the main remaining local heavy costs are now:
  - `nix-check` on cold clean worktrees
  - `coverage-check`
  - `test-full`
  - explicit package-build-with-tests reruns when intentionally requested

## Next optimization target

The next likely high-value directions are now:
1. decide whether host-default `nix-check` still belongs inside `check-heavy` / `check-full`, or should become an explicit opt-in / CI-oriented stage
2. investigate whether `test-full` still needs `--all-targets`, or whether benches/examples can move out of the heavyweight local path
3. investigate whether coverage can be made cheaper still without weakening the intended signal

Related investigation note:
- `plans/nix-build-time-investigation-candidate-plan-2026-04-06.md`
