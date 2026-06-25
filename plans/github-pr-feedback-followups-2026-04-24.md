# GitHub PR Feedback Follow-up Plan

Status: draft
Date: 2026-04-24
Baseline commit: `949a481` (`Add PR review anchors and pending GitHub feedback`)

## Goal

Continue the GitHub PR feedback work in this exact order, committing each slice independently after tests pass:

1. append to an existing trueflow-owned pending review
2. support removed-line anchors
3. add rename-aware remapping
4. add submit flow for pending reviews

This follows the already-landed baseline:

- `trueflow tui --target pr:...` reviews PR commits oldest -> newest
- TUI comment records persist richer `comment_anchor` data
- `trueflow feedback --pr ...` can create a pending GitHub review
- `.trueflow/github_delivery.json` tracks staged/delivered record IDs
- `--dry-run` and `--open` exist

## Current known behavior

The current implementation is intentionally conservative.

Supported now:

- source anchors on PR commits
- diff anchors that map to right-side lines on the PR head diff
- creating a new pending GitHub review for newly stageable comments

Skipped now:

- removed-only diff anchors
- rename-aware remapping
- appending to an already-existing pending review
- submitting a pending review from trueflow

## Global constraints

- Keep using `gh` CLI behind the GitHub client abstraction.
- Prefer thin vertical slices with TDD.
- Keep correctness conservative: skip ambiguous mappings rather than post to the wrong place.
- Avoid adding dependencies unless clearly necessary.
- GitHub submitted review event remains `COMMENT`.
- Verify each slice with at least:
  - `cargo fmt --all`
  - `cargo clippy --features tui-test-support --lib --bins --tests -- -D warnings`
  - `cargo test --features tui-test-support -q`

## Open semantic choices to confirm

These are not fully locked yet. Recommended defaults are listed so implementation can proceed unless changed.

1. **Ownership of a trueflow-managed pending review**
   - Options:
     - hidden machine marker in review body + local ledger review ID
     - ledger only
     - visible marker text in body
   - Recommended default:
     - hidden machine marker in review body + local ledger review ID

2. **What to do if the pending review was manually edited in GitHub**
   - Options:
     - append anyway
     - append only if marker still matches
     - create a fresh pending review
   - Recommended default:
     - append only if marker still matches; otherwise create a fresh pending review

3. **Removed-line anchor behavior**
   - Options:
     - exact left-side mapping only
     - fall back to nearest surviving right-side line
   - Recommended default:
     - exact left-side mapping only

4. **Rename-aware remapping scope**
   - Options:
     - pure renames only
     - include partial rewrites/copies
   - Recommended default:
     - pure renames only

5. **Submit UX**
   - Options:
     - `trueflow feedback --pr ... --submit`
     - separate submit subcommand
   - Recommended default:
     - `trueflow feedback --pr ... --submit`

6. **Submit review event semantics**
   - Options:
     - `COMMENT` only
     - approve/request-changes support
   - Recommended default:
     - `COMMENT` only

## Slice 1: Append to existing trueflow-owned pending review

### User-visible outcome

Re-running:

```sh
trueflow feedback --pr pr:11
```

should append newly stageable comments to the same trueflow-owned pending review when that review still exists and is still safe to treat as trueflow-managed.

### Work

- Define a durable trueflow pending-review identity scheme.
- Extend `GitHubClient` / `GhGitHubClient` to:
  - list or inspect pending reviews for a PR
  - add comments to an existing pending review, or otherwise update it via supported GitHub review APIs
- Teach `github_delivery.rs` to:
  - remember the current pending review ID for a PR
  - verify whether that review is still pending
  - fall back safely if it disappeared or no longer matches trueflow ownership expectations
- Update `feedback.rs` flow to:
  - reuse an existing pending review when possible
  - create a fresh one only when reuse is not safe
- Ensure `--dry-run` reports whether trueflow would append or create.

### Tests

Add focused tests for:

- pending review discovery / reuse
- mismatch between ledger review ID and live GitHub state
- manual edit / marker mismatch fallback behavior
- staged IDs remaining excluded across append runs
- dry-run output showing append vs create plan

### Commit boundary

Commit after append/reuse works and the full verification set passes.

## Slice 2: Support removed-line anchors

### User-visible outcome

Comments captured on removed lines in diff mode can be exported to GitHub when they can be mapped exactly to left-side PR diff locations.

### Work

- Extend mapping logic in `feedback.rs` to emit left-side GitHub inline comments for eligible removed-line anchors.
- Keep behavior strict:
  - support only exact left-side mapping
  - skip ambiguous or mixed unsupported cases
- Ensure the generated GitHub payload uses the correct side/line fields for removed-line comments.

### Tests

Add focused tests for:

- removed-only anchor rows mapping to left-side GitHub comments
- mixed added/removed anchors when exact mapping is and is not possible
- ambiguous removed-line anchors being skipped with explicit reasons
- no regression for existing right-side/source-anchor behavior

### Commit boundary

Commit after removed-line export works and the full verification set passes.

## Slice 3: Rename-aware remapping

### User-visible outcome

Comments can continue to map correctly when a file is renamed across the PR, as long as the rename is confidently detectable.

### Work

- Extend PR commit/diff inspection to understand path movement across commits.
- Add conservative path remapping logic:
  - support pure rename chains first
  - do not attempt fuzzy copy/rewrite heuristics
- Update source-anchor and diff-anchor export paths to consult the path remap layer before final PR head diff validation.
- Ensure skipped-reason reporting explains when rename remapping is unsupported or ambiguous.

### Tests

Add focused tests for:

- single pure rename across the PR
- multiple rename hops across commits
- rename + line-preserving mapping
- ambiguous path history causing skip
- no regression for non-renamed paths

### Commit boundary

Commit after rename-aware remapping works and the full verification set passes.

## Slice 4: Submit pending review

### User-visible outcome

Users can explicitly submit the current trueflow-owned pending review from the CLI.

Proposed UX:

```sh
trueflow feedback --pr pr:11 --submit
```

Behavior:

- find the current trueflow-owned pending review for the PR
- submit it as a GitHub `COMMENT` review
- transition staged IDs to delivered in the local ledger
- print the resulting submitted review URL/summary
- optionally honor `--open`

### Work

- Add CLI flag parsing and help text for `--submit`.
- Extend `GitHubClient` / `GhGitHubClient` with review submission support.
- Update `github_delivery.rs` to transition IDs from pending/staged to delivered on successful submit.
- Define behavior when `--submit` is requested but no trueflow-owned pending review exists.
- Ensure interactions between `--submit`, `--dry-run`, and `--open` are explicit and tested.

### Tests

Add focused tests for:

- successful submit of a trueflow-owned pending review
- no pending review found
- stale ledger entry with missing live review
- delivered-ID transition after submit
- dry-run reporting for submit mode

### Commit boundary

Commit after explicit submit flow works and the full verification set passes.

## Implementation order and discipline

For each slice:

1. write/adjust failing tests first
2. implement the minimal code to pass them
3. run formatting, clippy, and full tests
4. commit only that slice

Planned commit sequence:

1. `Reuse trueflow pending GitHub reviews`
2. `Support removed-line PR feedback anchors`
3. `Add rename-aware PR feedback remapping`
4. `Add pending PR review submission`

## Notes

- Do not overwrite `plans/github-pr-integration-design-2026-04-22.md`; treat it as the broader design doc.
- This file is the execution plan for the next implementation passes.
