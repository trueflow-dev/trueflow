# Nix Build Time Investigation Candidate Plan — 2026-04-06

## Scope

Investigate where local heavyweight Nix time is actually going, and identify the next safe optimization steps for the `trueflow` flake package builds.

## Current measured timings

### Existing local gate measurements from this workstream

From `plans/build-check-timing-summary-2026-04-06.md`:

- `check-heavy`: package-build portion was dominated by:
  - `nix-check-native`: **1m27s**
  - `nix-check-default`: **1m33s**
- `check-full`: total **4m29s**, with the two Nix package builds as the largest remaining local cost.

### Fresh focused rebuild evidence gathered in this investigation

Commands run:

- `nix build -L --no-link --rebuild .#native`
- `nix build -L --no-link --rebuild .#default`
- `nix flake check`
- `nix derivation show .#native`
- `nix derivation show .#default`
- `nix path-info -Sh .#native .#default`

Observed package phase timings from rebuild logs:

- `.#native`
  - build phase: **1m35s**
  - check phase: **49s**
- `.#default`
  - build phase: **1m34s**
  - check phase: **52s**

Important note: both `--rebuild` commands ended with a Nix nondeterminism warning instead of a clean success:

- `error: derivation ... may not be deterministic: output ... differs`

That means the commands were still useful for timing and phase inspection, but the nondeterminism issue is a separate correctness concern and may deserve its own lane.

## What is known

### 1. The local heavyweight “nix check” path is not using flake `checks`

`nix flake show --all-systems --json` reported:

- `checks = null`

`nix flake check` completed in about **2s** and only evaluated outputs:

- `packages`
- `devShells`
- `apps`

It did **not** build the package outputs.

So the local heavyweight path in `Justfile` is currently **package builds**, not flake checks:

- `nix-check-native: nix build .#native`
- `nix-check-default: nix build .#default`

This means “nix checks” and “flake checks” are currently different concepts in this repo.

### 2. Both local package builds use the same source and vendored Cargo deps

From derivation inspection:

- both use the same `src`
- both use the same `cargoDeps`
- both have `doCheck = 1`

So the heavyweight local path is not paying for two unrelated source graphs. It is paying for two package derivations over the same crate.

### 3. On `aarch64-darwin`, `default` is not a Linux musl artifact

On this machine:

- `builtins.currentSystem = aarch64-darwin`
- `.#native` -> `trueflow-0.1.0`
- `.#default` -> `trueflow-static-arm64-apple-darwin-0.1.0`

So `packages.default` on Darwin is a Darwin “static” package, not a Linux musl release artifact.

### 4. On Darwin, `native` and `default` are rebuilding effectively the same Rust target

The rebuild logs for both package outputs showed:

- `cargoCheckHook flags: ... --target aarch64-apple-darwin ...`

So on this machine, both package derivations are compiling and testing the same effective Rust target triple.

This strongly suggests the duplicated local cost is not “native vs musl” in the meaningful Linux sense. It is two Darwin package derivations with different toolchain / stdenv wrapping.

### 5. Both package builds rerun the full Rust test suite inside Nix

Both rebuild logs showed:

- a full optimized release build in `buildPhase`
- then a second compile/test pass in `checkPhase`
- 24 `Running ...` test/doc-test artifacts per package build

This is the single clearest source of avoidable duplicated work.

The local verification path already has separate Rust test stages outside Nix:

- `test`
- `test-full`

So the package build lane is currently re-testing inside Nix after tests have already been run elsewhere in the local gate.

### 6. `packages.default` has a much larger runtime closure on Darwin

Closure sizes:

- `.#native`: **63.8 MiB**
- `.#default`: **1.2 GiB**

The Darwin `default` output contains:

- `nix-support/propagated-build-inputs`

with:

- `pkg-config-wrapper`
- `apple-sdk-14.4`
- `libiconv-...-dev`

That propagated-build-inputs file does **not** exist in `.#native`.

So `packages.default` is not only expensive to build locally on Darwin; it also appears to leak large build-time SDK/tooling inputs into its runtime closure.

## Strongest findings

1. **Both Nix package builds rerun the full Rust test suite.**
   - Roughly **~50s extra per package build** on this machine.
   - This is duplicated against the existing non-Nix local Rust test stages.

2. **On Darwin, `default` is not meaningfully testing a different target in the way the name implies.**
   - Both package builds are effectively building/testing `aarch64-apple-darwin`.
   - The local heavy path is paying for two package derivations over the same crate and same effective target.

3. **The Darwin `default` package currently has a pathological runtime closure (~1.2 GiB).**
   - It propagates `apple-sdk`, `pkg-config-wrapper`, and `libiconv-dev`.
   - That makes the Darwin `default` package suspicious even aside from build time.

## What is unknown

1. **Policy:** should package derivations run `doCheck` at all in local heavyweight checks?
   - There is a correctness argument for package-level tests.
   - There is also a strong duplication argument because local Rust tests already run elsewhere.

2. **Intent:** what should `packages.default` mean on Darwin?
   - current behavior: Darwin static package
   - possible desired behavior: native package on Darwin, static/musl package on Linux
   - possible desired behavior: always release-like package, even if Darwin cannot produce the same artifact shape as Linux

3. **Cross-platform behavior:** how different are `native` and `default` on Linux?
   - This investigation was run on `aarch64-darwin`.
   - Linux may still justify building both outputs locally if `default` is a real musl target there.

4. **Why does the Darwin `pkgsStatic` package propagate build inputs into runtime closure?**
   - This looks suspicious and may indicate either:
     - expected Darwin `pkgsStatic` behavior, or
     - an avoidable packaging bug.

5. **Nondeterminism source:** why do both rebuilt package derivations fail the reproducibility check?
   - This was outside the main timing scope, but it is important.

## Recommended next experiments / safe changes

### A. Highest-signal next experiment: test a no-package-check variant

Goal: measure how much wall clock is saved by removing Nix package `checkPhase` duplication.

Candidate experiment:

- temporarily set `doCheck = false` for the package derivations
- rebuild `.#native` and `.#default`
- compare against the observed ~1m35s build + ~50s check split

Expected result on this machine:

- likely save roughly **~50s per package build**
- likely cut local heavy Nix cost by about **~1m40s total**

This is probably the largest safe performance lever, but it is a package-policy decision and should be confirmed before landing.

### B. Separate “flake checks” from “package builds” more explicitly

Current state:

- flake `checks`: absent
- local `nix-check-*`: package builds only

Recommended direction:

- add explicit `checks` outputs if there are Nix-native validations we want `nix flake check` to own
- rename or document the existing `nix-check-*` stages as package builds, not flake checks

This is a low-risk clarity improvement and would make future optimization work easier to reason about.

### C. Revisit what `packages.default` should be on Darwin

The biggest Darwin-specific question:

- should `packages.default` point at `native` on Darwin?
- should the static-ish package live under an explicit name like `static` or `release` instead?

This would avoid paying for two near-duplicate Darwin package builds in local heavy checks.

Candidate policy shape:

- Linux: keep `default = musl/static` if that is the release artifact we care about
- Darwin: make `default = native`
- keep explicit `native` and `static`/`musl` attrs for release workflows

This is a design decision, not a purely mechanical optimization, so it should be chosen deliberately.

### D. Investigate and fix the Darwin `default` closure leak

The `default` package on Darwin currently propagates:

- Apple SDK
- pkg-config wrapper
- libiconv dev output

That is probably not what we want in a runtime package.

Recommended follow-up:

- inspect why `pkgsStatic.rustPlatform.buildRustPackage` produces `nix-support/propagated-build-inputs` here
- determine whether this is caused by:
  - using `commonBuildInputs` from the non-static package set
  - Darwin `pkgsStatic` behavior
  - toolchain wrapper propagation
- fix closure correctness before treating the Darwin static package as a default local verification target

## Recommended next step

If only one change is taken next, it should be:

1. **make package-build tests explicit/opt-in rather than default**
2. then re-measure the local heavyweight path

That is the strongest likely time win with the smallest conceptual surface area.
