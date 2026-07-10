# Issue #14: Staged changes disappear from dirty review

Status: ready
Date: 2026-07-10
Baseline commit: 9a98914698c4

## Problem

`trueflow review --target dirty --json` discovers changed paths through `trueflow/src/vcs.rs::dirty_files`, but that function currently asks `gix` only for index-to-worktree status. Once a change is staged, the index and worktree agree, so a staged-only addition, modification, deletion, or rename can vanish from the dirty path set even though the index differs from `HEAD`.

The dirty target must represent the union of both Git status comparisons from one status operation:

1. `HEAD` tree to index: staged changes;
2. index to worktree: unstaged changes and untracked files.

The returned invariant is one normalized `RepoPath` per changed path, independent of status-event order. A staged rename must retain both the deleted source and added destination; if `gix` reports it as a rewrite, both rewrite locations must be expanded explicitly. A copy keeps only the destination because the source still exists unchanged.

## Evidence

- `trueflow/src/vcs.rs::dirty_files` builds `repo.status(...)`, enables `UntrackedFiles::Files`, and terminates it with `into_index_worktree_iter(Vec::new())`. It inserts only `item.rela_path()` values whose index/worktree summary is present.
- In `gix 0.78.0`, `status::Platform::into_index_worktree_iter` deliberately omits tree/index items; its outcome documents `tree_index: None` for that API. `status::Platform::into_iter` is the corresponding full iterator and emits `gix::status::Item::IndexWorktree` and `gix::status::Item::TreeIndex` from one status request.
- A tree/index `gix::diff::index::ChangeRef::Rewrite` exposes `source_location`, destination `location`, and `copy`. (`gix::diff::index::Change` is its owned alias.) The generic `location()` accessor returns only the destination, so using only that accessor would still lose the old side of a staged rename. Use this `gix` re-export rather than naming the transitive `gix_diff` crate, which is not a direct dependency.
- `trueflow/src/targets.rs::resolve_targets` supplies `vcs::dirty_files` to `resolve_targets_with`. `ResolvedReviewTarget::DirtyWorktree` extends its `changed` `HashSet<RepoPath>` with that result. If the set is empty, `ResolvedTargets::path_selection` returns `ReviewPathSelection::Empty` for the requested changed target.
- `trueflow/src/commands/review.rs::collect_review` returns `empty_collected_review()` immediately for `ReviewPathSelection::Empty`; JSON rendering then prints `[]`. For a nonempty dirty set, `preselected_paths_for_review` passes the selected `RepoPath`s to worktree scanning. Scanner output is sorted by path, so the status collector must not rely on `gix` iterator order.
- `trueflow/src/vcs.rs::block_state_for_path` first checks whether the exact fingerprint exists at the candidate path in `HEAD`. If not, it calls `dirty_files` and classifies a matching path as `BlockStateResult::Uncommitted`; otherwise it falls back to `Unknown`. A new fingerprint in a staged-only modification is therefore misclassified as `Unknown` today.
- Existing coverage provides the conventions to extend: `trueflow/tests/vcs_scope.rs` opens `TestRepo` fixtures with `gix` and calls public VCS functions; `trueflow/tests/bug_regressions.rs` executes the real CLI and parses top-level JSON with `json_array`; `trueflow/tests/e2e_mark_store_coverage.rs::test_mark_uncommitted_state` verifies persisted block-state classification; and `trueflow/tests/e2e_diff.rs::test_main_review_handles_renamed_file` uses `git mv` for rename fixtures.

## Reproduction

The already-observed reproduction is:

1. Commit a reviewable Rust file.
2. Change the file and stage it with `git add`.
3. Run `trueflow review --target dirty --json`.
4. The command exits successfully with `[]`.

From the repository root, the behavior can be reproduced without mutating this checkout:

```sh
cd trueflow
cargo build --bin trueflow
bin="$PWD/target/debug/trueflow"
repo="$(mktemp -d)"
git -C "$repo" init -q
git -C "$repo" config user.email test@example.com
git -C "$repo" config user.name "Test User"
mkdir -p "$repo/src"
printf 'pub fn value() -> i32 { 1 }\n' > "$repo/src/lib.rs"
git -C "$repo" add src/lib.rs
git -C "$repo" commit -q -m base
printf 'pub fn value() -> i32 { 2 }\n' > "$repo/src/lib.rs"
git -C "$repo" add src/lib.rs
(cd "$repo" && "$bin" review --target dirty --json)
```

Before the fix, the final command exits 0 and prints `[]`. After the fix, it must still exit 0 but emit exactly one file entry whose `path` is `src/lib.rs` and whose blocks reflect the staged content in the worktree.

## Root cause

Staging moves a change across a comparison boundary; it does not make the repository clean. The current implementation confuses “no index-to-worktree difference” with “no dirty change” because `into_index_worktree_iter` never performs or emits the `HEAD`-to-index half of status.

That omission explains every affected form:

- staged modification: index and worktree contain the same new blob;
- staged addition: index and worktree both contain the new tracked path;
- staged deletion: both index and worktree omit the old path;
- staged rename: index and worktree agree that the source is absent and the destination is present.

Running a separate tree/index query and then the current index/worktree query would be the wrong boundary: it would duplicate status setup, could observe different index/worktree snapshots if files change between calls, and would complicate deduplication. The full `gix` status iterator already provides both item families from one configured pass. The only special case is a tree/index rewrite: its destination is the generic location, while the renamed-away source is stored separately.

## Implementation plan

1. **Add red VCS status tests at the public and status-item boundaries.**
   1. In `trueflow/tests/vcs_scope.rs`, use `TestRepo`, real Git staging commands, `gix::open`, and `trueflow::vcs::dirty_files`; compare exact `HashSet<RepoPath>` values rather than only checking that a path happens to be present.
   2. `dirty_files_reports_staged_only_modification`: commit `src/modified.rs`, change it, stage it, and expect exactly `src/modified.rs`.
   3. `dirty_files_reports_staged_only_addition`: commit a base file so `HEAD` exists, create and stage `src/added.rs`, and expect exactly `src/added.rs`.
   4. `dirty_files_reports_staged_only_deletion`: commit `src/deleted.rs`, remove it, stage the deletion with `git add -A`, and expect exactly the deleted path even though it no longer exists in the worktree.
   5. `dirty_files_reports_both_sides_of_staged_rename`: commit `src/old.rs`, stage `git mv src/old.rs src/new.rs`, and expect exactly `{src/old.rs, src/new.rs}`. This must pass whether `gix` reports deletion/addition events or combines them into a rewrite.
   6. `dirty_files_deduplicates_index_and_worktree_path`: commit a path, stage one modification, modify the same path again without staging, and assert the result has length one and contains that path. This guards the union when both status halves emit the same path.
   7. In `trueflow/src/vcs.rs`'s private test module, add `tree_index_rewrite_paths_distinguish_rename_from_copy`. Construct otherwise-equivalent synthetic `gix::diff::index::ChangeRef::Rewrite` values with `copy: false` and `copy: true`, pass each through the same private tree/index path-expansion helper used by `dirty_files`, and assert exactly `{source, destination}` for the rename versus exactly `{destination}` for the copy. This explicitly covers the boolean branch even though default Git status configuration normally does not detect copies.
   8. Run both focused VCS commands below before completing the implementation; the staged-only public cases and the new private helper test must be red against the baseline for the intended reason, while the overlap case protects the forthcoming union.

2. **Add red dirty-review CLI regressions in `trueflow/tests/bug_regressions.rs`.** Follow the existing `TestRepo::run`/`json_array` pattern and assert both cardinality and exact normalized path values.
   1. `review_dirty_staged_only_modification_is_not_empty` must encode the observed reproduction verbatim: committed Rust function, changed and staged function, `review --target dirty --json`, successful process, and exactly one `src/lib.rs` entry rather than `[]`.
   2. `review_dirty_staged_only_addition_is_not_empty` must stage a new reviewable Rust file and assert exactly one entry for the new path.
   3. `review_dirty_staged_only_rename_emits_destination_once` must stage `git mv` and assert that current-worktree review emits the destination exactly once. It must not expect an entry for the absent source; source preservation is asserted at the VCS path-set boundary in step 1.
   4. Keep the staged-deletion assertion at the `dirty_files` boundary. Dirty review intentionally scans current worktree content and has no file body to serialize for a deleted path; adding deletion tombstones or switching the dirty target to diff rendering is outside this issue.

3. **Add a red persisted block-state regression in `trueflow/tests/e2e_mark_store_coverage.rs`.** Add `mark_staged_only_change_is_uncommitted` alongside, not in place of, `test_mark_uncommitted_state` so the existing unstaged contract remains covered. Commit a Rust block, change it to produce a new fingerprint, stage the file, obtain the current fingerprint through `review --all --json`, run `mark` with the path, load `.trueflow/reviews.jsonl`, and assert `BlockState::Uncommitted`. Against the baseline this should be `Unknown`, proving the test exercises `block_state_for_path` rather than only target selection.

4. **Replace the partial status traversal in `trueflow/src/vcs.rs::dirty_files` with one full status pass.** Keep `repo.status(gix::progress::Discard)?` and `.untracked_files(UntrackedFiles::Files)`, but call `.into_iter(Vec::new())?` exactly once.
   1. For `gix::status::Item::IndexWorktree(item)`, preserve existing behavior: ignore an item whose `summary()` is `None`, otherwise normalize and insert `item.rela_path()`.
   2. For `gix::status::Item::TreeIndex(change)`, pass the owned `gix::diff::index::Change` to one private path-expansion helper. Insert `change.location()` for additions, deletions, modifications, and rewrites.
   3. In that helper, match `gix::diff::index::ChangeRef::Rewrite { source_location, copy: false, .. }` and also insert `source_location`. For `Rewrite { copy: true, .. }`, do not insert the unchanged source; the destination insertion is sufficient. Keep this exact branch under the synthetic test from step 1.
   4. Route every emitted byte path through one small private insertion helper taking `&gix::bstr::BStr` (or an equivalently single conversion point) that uses the existing `ByteSlice::to_str_lossy` behavior and `RepoPath::new`. Propagate conversion and iterator errors as `dirty_files` does now; do not silently discard malformed paths.
   5. Continue collecting into `HashSet<RepoPath>`. This unions tree/index and index/worktree events, collapses an overlapping path to one value, normalizes separators through `RepoPath::new`, and makes the result independent of the full iterator's undefined event order. Do not add another status call, a second Git implementation, a direct `gix_diff` dependency, a compatibility path, or any other new dependency.

5. **Run the focused green smoke checks immediately after the source fix.** The synthetic rewrite test proves rename and copy endpoints diverge correctly, the public VCS matrix proves staged status collection, the CLI tests prove dirty target resolution no longer collapses to `ReviewPathSelection::Empty`, and the mark test proves the corrected path set reaches `block_state_for_path`. Also rerun the manual reproduction and confirm one `src/lib.rs` JSON entry.

6. **Cleanup only after the focused smoke is green.** Remove imports made obsolete by the iterator change, keep any path insertion helper private to `vcs.rs`, and confirm there is no duplicated tree/index conversion logic. Do not rename `ReviewTarget::DirtyWorktree`, alter `ResolvedTargets`, or change review/scanner behavior; those callers should benefit solely through the corrected `dirty_files` result.

7. **Run touched-suite and repository validation in order.** Run the affected library tests and all three affected integration-test binaries, then the final repository gate `just check`.

Ordered affected files and symbols:

1. `trueflow/src/vcs.rs` — `dirty_files`, private tree/index path expansion, private `gix::bstr::BStr`-to-`RepoPath` insertion, and the synthetic rename-versus-copy rewrite unit test.
2. `trueflow/tests/vcs_scope.rs` — new `dirty_files_*` staged-status and deduplication regressions.
3. `trueflow/tests/bug_regressions.rs` — new `review_dirty_staged_only_*` real-CLI regressions.
4. `trueflow/tests/e2e_mark_store_coverage.rs` — new `mark_staged_only_change_is_uncommitted` regression.

Inspected integration boundaries that should remain source-compatible and unchanged:

- `trueflow/src/targets.rs::{resolve_targets, resolve_targets_with, ResolvedTargets::path_selection}`;
- `trueflow/src/commands/review.rs::{collect_review, preselected_paths_for_review}`;
- `trueflow/src/vcs.rs::block_state_for_path`.

## Verification and validation

Run these commands from the repository root.

1. Red/green synthetic rewrite branch:

   ```sh
   cd trueflow && cargo test --features tui-test-support --lib tree_index_rewrite_paths_distinguish_rename_from_copy -- --nocapture
   ```

2. Red/green public VCS status matrix:

   ```sh
   cd trueflow && cargo test --features tui-test-support --test vcs_scope dirty_files_ -- --nocapture
   ```

3. Red/green real-CLI dirty-review cases:

   ```sh
   cd trueflow && cargo test --features tui-test-support --test bug_regressions review_dirty_staged_only_ -- --nocapture
   ```

4. Red/green block-state classification:

   ```sh
   cd trueflow && cargo test --features tui-test-support --test e2e_mark_store_coverage mark_staged_only_change_is_uncommitted -- --nocapture
   ```

5. Focused behavioral smoke: rerun the reproduction script in `## Reproduction`. Confirm exit status 0, valid JSON, exactly one file object, `path == "src/lib.rs"`, and content from the staged/worktree version. The output must not contain the file twice.

6. All touched library and integration-test targets:

   ```sh
   cd trueflow && cargo test --features tui-test-support --lib --test vcs_scope --test bug_regressions --test e2e_mark_store_coverage
   ```

7. Final repository gate:

   ```sh
   just check
   ```

## Acceptance criteria

- A staged-only modification is returned by `dirty_files` and appears once in `review --target dirty --json`.
- A staged-only addition is returned by `dirty_files` and appears once in dirty review.
- A staged-only deletion contributes its normalized old path to `dirty_files`, even though worktree-only review rendering has no deleted file body to emit.
- A staged rename contributes both normalized source and destination paths to `dirty_files`; dirty worktree review emits the extant destination once.
- A tree/index copy contributes its destination without falsely marking the unchanged source dirty.
- Synthetic `gix::diff::index::ChangeRef::Rewrite` coverage proves `copy: false` contributes source and destination while `copy: true` contributes only destination.
- A path changed in both tree/index and index/worktree status is present only once in the returned `HashSet` and only once in JSON review output.
- Status collection uses one `gix::status::Platform::into_iter` pass, not separate HEAD/index and index/worktree operations.
- All status paths, including rewrite source paths, pass through `RepoPath::new`; results do not depend on `gix` event order.
- Existing unstaged modifications and untracked files remain discoverable because the index/worktree branch and `UntrackedFiles::Files` configuration are preserved.
- Marking a new block fingerprint from a staged-only modification records `BlockState::Uncommitted`, while the existing committed and unstaged-state tests remain green.
- Iterator, status, and path-conversion failures continue to propagate instead of being treated as a clean repository.
- The focused commands, touched integration suites, and final `just check` pass.

## Non-goals and risks

- Do not change working-tree-only semantics unrelated to staged status: ignore handling, untracked-file enumeration, submodule policy, scanner filtering, review filters, and cache behavior stay as they are.
- Do not turn the dirty target into a `HEAD` diff, add deletion tombstones, or teach current-worktree JSON to render deleted source content. The deleted source belongs in the VCS/target path set; historical rendering is a separate concern.
- Do not change `main`, revision, revision-range, file, or directory targets, and do not add compatibility shims, migrations, a direct dependency on transitive `gix_diff`, or any other dependency; the project is beta and the existing `dirty_files` API can be corrected in place using `gix::diff`.
- Rename detection is configuration-sensitive: `gix` may emit separate deletion/addition items or one `gix::diff::index::ChangeRef::Rewrite`. The collector must handle both forms, and the synthetic branch test must prove that a rewrite marked as a copy does not make the still-existing source dirty.
- The full status iterator documents undefined item ordering under parallel operation. Correctness must come from normalized set insertion, not encounter order; downstream scanner sorting remains responsible for stable review ordering.
- Non-UTF-8 Git paths continue to use the existing lossy conversion before `RepoPath` validation. Changing that repository-path representation is outside this issue.
