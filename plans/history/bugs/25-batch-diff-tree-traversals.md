# Issue #25: Batch diff tree traversals

Status: ready
Date: 2026-07-10
Baseline commit: `9a98914698c4`

## Problem

Diff-scoped review computes changed paths in bulk, but then recomputes the complete tree-to-tree diff separately for every selected destination when it needs file diffs. For a target with `C` selected changed files, this performs `C` full `repo.diff_tree_to_tree` traversals and materializes the same `C`-change result `C` times. It also creates a fresh blob diff resource cache for every selected file.

For one diff target whose selected set is the changed set, the current implementation therefore performs:

- `C` full tree-diff traversals and `C²` change productions inside those traversals;
- `C(C + 1) / 2` local `ChangeDetached` inspections when selected paths and emitted changes cover the same set; and
- `C` diff resource cache constructions.

The current file-diff stage is thus `Theta(C²)` in change visits. With a fixed number `T` of diff targets, it is `Theta(T × C²)`. The earlier changed-path discovery pass is already one traversal per target and adds only a linear term; it does not remove the quadratic file-diff work.

The fix must perform one complete tree diff per `ReviewDiffTarget`, inspect each detached change once, index only requested changes by destination path, and reuse one blob diff resource cache while converting all selected changes in that target. For `D_t` emitted changes in target `t`, `C` selected destinations, and `H` generated hunk data, the target complexity is `Theta(sum(D_t) + T × C + H)`; when `D_t = C` and `T` is fixed, this is near-linear in `C`.

Issue #25 and issue #15, “Preserve rename source paths in semantic diffs,” share one load-bearing contract. Issue #15 introduces `ChangedPath { source_location, location }` and stores it in every `FileDiff`; issue #25 must batch those full `FileDiff` values by destination without stripping the source path. `collect_diff_review_files` then loads each target's base blob from that target's `source_location`, keeps head/display identity at `location`, and only then reads that `FileDiff`'s hunks. A hunk-only batch result would make issue #15 impossible and is prohibited.

The batching optimization must not alter output relative to the corrected post-issue-#15 semantics. It must preserve target-list order, duplicate or overlapping targets, hunk order within each target, binary and external-unavailable `FileDiff` states, destination display identity, per-target rename sources, additions, deletions, and the identity-pair `NoTextChanges` result for a selected destination absent from one target.
## Evidence

- `trueflow/src/commands/review.rs:597-621` builds `head_files_by_path`, obtains and sorts `selected_paths`, then calls both `base_file_states_for_diff_targets` and `diff_hunks_for_file_targets` inside the per-path loop. Every selected destination starts fresh per-target VCS work.
- `trueflow/src/commands/review.rs:872-895` implements `diff_hunks_for_file_targets` as a loop over `ReviewDiffTarget`. Each target calls a singular VCS hunk API and appends its hunks. This establishes the current invariant that a file's hunks follow caller-supplied target order and duplicate targets are not deduplicated.
- `trueflow/src/vcs.rs:591-663` shows that the singular main, revision, and revision-range APIs resolve their base/head trees independently and all reach `diff_for_file_between_trees`.
- `trueflow/src/vcs.rs:666-688` creates `repo.diff_resource_cache_for_tree_diff()`, calls `repo.diff_tree_to_tree(...)`, and scans the returned changes from the beginning until `change_ref.location()` equals one requested path. Both the cache and complete tree diff are recreated for the next file.
- The locked `gix 0.78.0` API used by `trueflow/Cargo.toml:42` returns a fully materialized `Vec<ChangeDetached>` from `Repository::diff_tree_to_tree`; returning early from the subsequent local loop does not stop the already-completed traversal.
- `trueflow/src/vcs.rs:945-980` centralizes conversion of a selected change into `FileDiff::Text`, `FileDiff::NoTextChanges`, `FileDiff::Unavailable(Binary)`, or `FileDiff::Unavailable(External)`. The batch path must continue using this function and retain the complete enum value.
- `trueflow/src/vcs.rs:682-685` calls `clear_resource_cache_keep_allocation()` after one matching change, but immediately drops the cache because the singular function returns. A target batch can reuse those allocations across all selected changes.
- `trueflow/src/commands/review.rs:603-608` already has a unique destination map and sorted selected-destination vector. No new path-discovery walk is needed.
- `trueflow/src/targets.rs:544-554` preserves diff-target vector order. Any result vector must remain positionally aligned with this slice; map iteration must not establish observable order.
- `trueflow/src/commands/review.rs:717-746` already gives base lookup separate tree and output paths, but its caller passes the destination for both. Issue #15 corrects that by retaining each target's `ChangedPath`; issue #25 must not erase it when batching.
- The issue #15 contract changes every `FileDiff` variant from one `RepoPath` to `ChangedPath`, keeps `DiffHunk.file_path` at the destination, and requires each target's base lookup to use that same `FileDiff`'s source. There must not be a global destination-to-source map because two targets may map the same destination from different sources.
- Existing coverage in `trueflow/tests/e2e_diff.rs:330-401` exercises weak rename destination output, binary omission, and ordinary deletion. `trueflow/tests/bug_regressions.rs:999-1065` exercises historical deletion content. The mixed regression must use the stronger post-rename contract: source-side deleted content appears under the destination, not the known-bad current head-only result.
- `trueflow/benches/full_review.rs:21-64` and `trueflow/test-support/src/lib.rs:126-145` provide the Criterion and reusable-repository seams. The helper currently runs only `ReviewRequest::AllFiles`, so a generated main-diff fixture/helper is required.
## Reproduction

For a main/feature repository with `C` independently changed Rust files and a main-diff review selecting all of them:

1. Target resolution obtains the changed-path set with one target traversal.
2. `collect_diff_review_files` sorts those `C` destinations.
3. For every destination, `diff_hunks_for_file_targets` calls the singular target API.
4. `diff_for_file_between_trees` materializes all `C` changes again and scans from the beginning for that destination.

For `C = 100`, `200`, and `400`, file-diff preparation causes approximately `10,000`, `40,000`, and `160,000` tree-diff change productions. The local scans inspect `5,050`, `20,100`, and `80,200` detached changes. Doubling the changed-file count therefore produces approximately four times the work.

The first TDD test makes both defects deterministic. It constructs 128 changed files and two duplicate main-diff targets, resolves the query, resets test-only counters, and collects the review. On the baseline:

- the traversal counter records `128 × 2 = 256`, not `2`; and
- the inspected-change counter records `2 × (128 × 129 / 2) = 16,512`, not `128 × 2 = 256`.

The test must assert both post-fix values. A traversal-only assertion is insufficient because an implementation could materialize one change vector per target and still rescan it once per selected path.

```sh
cd trueflow && cargo test --lib commands::review::tests::collect_diff_review_files_traverses_and_inspects_once_per_diff_target -- --exact
```

The post-fix Criterion benchmark uses 100, 200, and 400 changed files so near-linear elapsed-time scaling corroborates the deterministic counters.
## Root cause

The API boundary is singular at the wrong layer. `collect_diff_review_files` knows the complete sorted selected-destination set, but passes one destination to `diff_hunks_for_file_targets`. That helper loops over targets, and each singular VCS call resolves trees and calls `diff_for_file_between_trees`. The only function that sees a tree change can satisfy only one file request.

```text
for selected destination                  C times
    for diff target                       T times
        resolve target trees
        construct resource cache
        materialize the full tree diff    D_t changes
        inspect changes until destination
        convert one change to FileDiff
```

The loop order prevents sharing the target traversal and its resource cache. Returning after a location match only shortens the scan of an already-materialized vector.

Issue #15 exposes a second ownership requirement: the gix change contains both a rewrite source and destination, and that pair must stay attached to the target's `FileDiff` until review has loaded the correct base. Projecting a batch to `HashMap<RepoPath, Vec<DiffHunk>>` is therefore incorrect even if it is fast.

The corrected ownership boundary is target-first and full-value:

```text
build the sorted selected-destination slice
for diff target in caller order                          T times
    resolve that target's base/head trees once
    construct one reusable resource cache
    materialize/traverse that target diff once           D_t changes
    inspect each detached change exactly once
        ignore unselected destinations
        convert the first selected destination to full FileDiff
        retain FileDiff.changed_path source + destination
        clear resources while retaining reusable buffers
    retain one SelectedFileDiffs map for this target

for selected destination in deterministic order
    for target batch in caller order
        take full FileDiff (identity NoTextChanges if absent)
        load this target's base from FileDiff.source_location
        keep output/head identity at FileDiff.location
        only now consume/borrow this FileDiff's hunks
```

The ordered result is a `Vec<TargetDiffBatch>` aligned with `ReviewDiffTarget`, where each batch owns `SelectedFileDiffs: HashMap<destination, FileDiff>`. It is not a hunk map. This preserves target/source association, unavailable states, and a seam for later target-scoped block attribution while removing all per-path tree traversals.
## Implementation plan

1. **Add the observable red counters and correctness regressions before changing the production algorithm.**
   - In `trueflow/src/vcs.rs` test support, add two thread-local `Cell<usize>` counters with narrow `pub(crate)` reset/read helpers: one increment immediately before the tree-diff call used for file-diff preparation, and one increment for every `ChangeDetached` inspected by the location-selection loop. `#[cfg(test)]` removes both from production, and thread-local state avoids cross-test contention.
   - In `trueflow/src/commands/review.rs::tests`, add `collect_diff_review_files_traverses_and_inspects_once_per_diff_target`. Generate 128 deterministically named changed Rust files and two duplicate main-diff targets. Reset counters after target resolution so changed-path discovery is excluded. Assert all 128 destinations remain in sorted review output, traversal count is exactly `2` rather than `256`, and inspected-change count is exactly `256` rather than `16,512`.
   - Add a multi-target characterization with distinct historical targets and one duplicate. Give each target unique added-line markers; assert each destination retains one full `FileDiff` per target in target order, sources stay attached to the correct target, marker order follows target order, and reversing targets reverses only target-local ordering.
   - In `trueflow/tests/e2e_diff.rs`, add a mixed fixture with modification, addition, deletion, edited rename, and binary change. Configure `diff.renames=true`. The expected rename result must be the corrected issue #15 behavior: exactly one destination file, no source file, correct source-side removed content under the destination, and correct changed/deleted block sides. Characterize ordinary non-rename output before the batch edit, but do not freeze the known-bad pre-issue-#15 head-only rename result.
   - In `trueflow/src/vcs.rs::tests`, cover `Text`, `NoTextChanges`, `Unavailable(Binary)`, and an attribute-selected external driver in the selected-change indexing seam. Configure the test cache to skip internal diff only for the external case. Assert additions/deletions handle a missing side, rewrite results are keyed by `ChangedPath.location`, and every stored `FileDiff` retains the complete `ChangedPath`. Add `selected_destination_lookup_preserves_lossy_non_utf8_locations`, a direct helper-level regression that supplies a synthetic raw location such as `b"src/\xff.rs"` and a selected `RepoPath` containing the corresponding U+FFFD replacement; assert the lookup selects that canonical destination exactly as the current `to_str_lossy()` comparison does.

2. **Coordinate the issue #15 and #25 cutover rather than landing incompatible intermediate APIs.**
   - Apply issue #15's canonical `ChangedPath { source_location, location }` model, endpoint-aware changed selection, and `FileDiff` shape first within the shared branch/cutover.
   - Do not implement issue #15's review assembly as a permanent per-destination loop over singular `vcs::diff_for_file*` calls; that would immediately recreate issue #25's `Theta(T × C²)` defect.
   - Once `FileDiff` carries `ChangedPath`, implement the issue #25 target batch and migrate `collect_diff_review_files` in the same compileable cutover. If organized as commits, the order is: issue #15 red path-pair/scoping tests; shared `ChangedPath` and `FileDiff` type migration; issue #25 counter test; target-first VCS batch; joint review integration; joint focused smoke.
   - Issue #15 owns source/destination selection semantics. Issue #25 owns traversal count, one-pass indexing, cache reuse, and batch ownership. Neither plan may introduce a destination-only or hunk-only compatibility layer.

3. **Introduce one batch VCS representation and one tree-diff implementation in `trueflow/src/vcs.rs`.**
   - Add crate-private `SelectedFileDiffs { by_destination: HashMap<RepoPath, FileDiff> }`. Every `FileDiff` contains the issue #15 `ChangedPath`. `take(destination)` returns the stored value or `FileDiff::NoTextChanges { changed_path: ChangedPath::identity(destination) }` for a target that did not emit that destination.
   - Add crate-private batch entry points parallel to main-to-head, revision-to-parent, and revision-range singular APIs. Each accepts the complete selected-destination slice, resolves its tree pair once, and delegates to `diffs_for_paths_between_trees`.
   - Build one membership index over borrowed selected destination strings/path references, allocate the sparse result to at most `C` entries, create one `diff_resource_cache_for_tree_diff`, call `repo.diff_tree_to_tree` exactly once, and inspect the resulting `ChangeDetached` vector exactly once.
   - For each detached change, increment the inspected-change counter and read `change_ref.location()` without constructing a `RepoPath` or `ChangedPath`. If `std::str::from_utf8(location.as_ref())` succeeds, probe the selected-destination index with that borrowed `&str`, allocation-free. If it fails, compute `location.to_str_lossy()` and probe with the resulting `Cow<str>` before constructing a pair. This fallback intentionally preserves the baseline comparison at `trueflow/src/vcs.rs:678-679`, where non-UTF bytes are represented by U+FFFD; rejecting invalid bytes or comparing only raw bytes would silently lose an already-selected changed path.
   - Resolve the borrowed membership hit to its canonical selected `RepoPath`, then check whether the result map already occupies that destination. Skip occupied destinations before constructing a pair so duplicate emitted locations preserve first-match semantics without extra path allocation. Only for a vacant selected destination may the code construct/validate issue #15's `ChangedPath` from the original `ChangeRef`, pass that same `ChangeRef` to `set_resource_by_change`, call `file_diff_from_change`, insert by destination, and clear the resource cache while keeping allocation.
   - Do not add a second source-keyed map entry. A rewrite is one destination-keyed `FileDiff` whose internal `ChangedPath.source_location` selects the base. Additions, deletions, and modifications use identity pairs; copies follow the pair gix emits.
   - Keep all `FileDiff` variants intact. Binary/external/no-text states must carry `ChangedPath`, and `DiffHunk.file_path` remains the destination.
   - Refactor `diff_for_file_between_trees` to use the same one-path primitive/result. Retain active singular APIs for the TUI's lazy one-file loading (`trueflow/src/commands/tui.rs:6826-6868`), not as review compatibility shims. There must be one change-to-`FileDiff` converter.
   - Preserve fail-fast errors: a traversal, resource, or internal-diff failure aborts the request rather than dropping an entry or synthesizing no changes.

4. **Replace split base/hunk lookup with ordered full target batches in `trueflow/src/commands/review.rs`.**
   - Replace `diff_hunks_for_file_targets` and the independent `base_file_states_for_diff_targets` orchestration with a target-first builder returning `Vec<TargetDiffBatch>`. Each element holds its `ReviewDiffTarget` association and one destination-keyed `SelectedFileDiffs`; vector order exactly matches the input target slice.
   - Build all target batches once, immediately after sorting `selected_paths`, before entering the destination loop. Assert or structurally guarantee one batch per target.
   - Inside the destination loop, iterate `TargetDiffBatch` in slice order and `take(destination)` to obtain the complete per-target `FileDiff`. Use `file_diff.changed_path().source_location` as the tree lookup path and the selected destination as `output_path` when calling the existing main-base, revision-base, or range-base file-state helper.
   - Only after that target's base state is associated may review borrow/move its hunks. `Text` contributes hunks; `NoTextChanges`, `Unavailable(Binary)`, and `Unavailable(External)` contribute none. Do not create an earlier `HashMap<RepoPath, Vec<DiffHunk>>`.
   - Keep per-target `FileDiff`, base state, and hunk association explicit until the existing review-block input boundary. Then preserve current target-order hunk concatenation and deterministic base-block deduplication. This avoids choosing one rename source from a union and leaves the target boundary available to issue #16's later block-attribution fix.
   - Do not sort, deduplicate, group, or parallelize targets. Duplicate targets produce duplicate `FileDiff`/hunk contributions, and the same destination may carry different source paths in different target batches.
   - Preserve selected destination sorting, head-file lookup by destination, head-first language/hash fallback, final review-file ordering, and output identity. A rename with an extension change parses its base from the source tree path but displays and prefers head language at the destination.
   - Review the call at `trueflow/src/commands/review.rs:349-358`. Prove it remains unreachable for the diff-scoped early-return path or migrate its reachable diff-target case to the batch contract; it must not leave a second per-file traversal.

5. **Run the coordinated issue #15/#25 focused smoke before benchmark or cleanup work.**
   - Require both counters to pass: two traversals and 256 inspected changes for the 128-file/two-target fixture.
   - Run the full-value target-order test and VCS state/pair test.
   - Run issue #15's rename-aware destination/source regressions plus the mixed fixture. The destination must contain corrected source-side semantic content, not the known-bad baseline sides.
   - Re-run ordinary rename destination, binary, whole-file deletion, and historical deletion regressions. Batching must introduce no output delta beyond issue #15's intentional rename correction.
   - Inspect both target orders: target-local source pairs and hunks reverse together; the destination file order remains sorted.

6. **After the smoke passes, add the Criterion scaling benchmark and fixture smoke.**
   - In `trueflow/test-support/src/lib.rs`, extend `ReviewBenchRepo` with a generated main-diff constructor and `main_diff_review_summary` using `ReviewRequest::Targets(vec![ReviewTarget::MainDiff])`. Generate exactly 100, 200, or 400 small Rust files, commit a common base on `main`, modify every file on a feature branch, and keep setup outside timing.
   - In `trueflow/tests/e2e_bench_fixture.rs`, add a feature-gated smoke test asserting requested file count, deterministic first/last destination, and stable reviewable block count.
   - In `trueflow/benches/full_review.rs`, add `batch_diff_review` with `BenchmarkId` values `100`, `200`, and `400` plus `Throughput::Elements(file_count)`. Warm each repository once; validate output outside `b.iter`; time repeated `main_diff_review_summary`; black-box file/block counts; use ten samples and explicit measurement time.
   - Record medians `M100`, `M200`, `M400`. Require `M200 / M100 <= 2.5` and `M400 / M200 <= 2.5`; below `1.5` is acceptable as better-than-linear only when fixture counts remain exact. Neither ratio may approach `4×` or exceed `3×`.
   - Keep the benchmark end-to-end. The two counters are the hard local complexity proof; the benchmark is the user-visible scaling proof. Do not add or tune parser benchmarks.

7. **Perform final cleanup and validation.**
   - Remove superseded split review helpers, temporary comparison code, and any destination-only/hunk-only intermediate. Keep both permanent `#[cfg(test)]` counters.
   - Verify target batches move `FileDiff` values and hunks without cloning their payloads, and that no detached change vector or resource cache survives into the next target traversal.
   - Run the focused commands, benchmark fixture smoke, Criterion group, and final repository gate below.

### Ordered affected files and symbols

1. `trueflow/src/vcs.rs`
   - issue #15 `ChangedPath`/`FileDiff` contract;
   - test-only traversal and inspected-change counters;
   - `SelectedFileDiffs`;
   - main/revision/range batch entry points;
   - `diffs_for_paths_between_trees`;
   - `diff_for_file_between_trees`;
   - `file_diff_from_change` as the sole converter.
2. `trueflow/src/commands/review.rs`
   - `tests::collect_diff_review_files_traverses_and_inspects_once_per_diff_target`;
   - full-value target/source ordering regression;
   - `TargetDiffBatch`;
   - `collect_diff_review_files`;
   - removal of split `base_file_states_for_diff_targets`/`diff_hunks_for_file_targets` orchestration.
3. `trueflow/tests/e2e_diff.rs`
   - mixed modified/added/deleted/rename/binary post-issue-#15 characterization.
4. `trueflow/test-support/src/lib.rs`
   - generated `ReviewBenchRepo` main-diff fixture and summary helper.
5. `trueflow/tests/e2e_bench_fixture.rs`
   - generated 100-file fixture smoke.
6. `trueflow/benches/full_review.rs`
   - Criterion `batch_diff_review/{100,200,400}`.

Issue #15 separately inventories the `ChangedPath` propagation through `targets.rs`, feedback/export, and TUI constructors. Those type-cutover edits must precede the joint review integration, but issue #25 must not duplicate or weaken that inventory.

### Allocation and ownership bounds

Let `D_t` be emitted changes for target `t`, `C` unique selected destinations, `T` targets, `H` retained hunk payload, `P` retained source/destination path bytes, `L_max` the largest temporary lossy location string, and `B_max` the largest selected old/new blob pair.

- The membership index has at most `C` entries per active target and borrows destination strings/path references from the selected slice; it clones no selected path strings.
- Every emitted change is counted. Valid UTF-8 locations use an allocation-free borrowed lookup. Invalid UTF-8 locations may allocate one temporary `Cow<str>` to reproduce `to_str_lossy()`/U+FFFD selection semantics, even when unselected or already occupied; that temporary is dropped before the next change and contributes at most `O(L_max)` peak storage.
- After membership probing, unselected and already-occupied changes allocate no `RepoPath`, `ChangedPath`, blob resource, or result entry. Path-pair validation/construction happens only for a vacant selected destination.
- Each sparse `SelectedFileDiffs` has at most `min(C, D_t)` destination keys. Across retained ordered target batches, map entries and owned non-identity `ChangedPath` pairs are bounded by `sum(min(C, D_t)) <= T × C`, never by `sum(D_t)` when most changes are unselected.
- Missing results are not preallocated. `take(destination)` creates one identity-pair `NoTextChanges` value only when that target/destination is consumed.
- Full `FileDiff` values, including their `ChangedPath` and hunk vectors, remain in target batches until the destination loop consumes them. No parallel hunk-only map duplicates `H`.
- `repo.diff_tree_to_tree` temporarily owns `Theta(D_t)` detached values. Drop each target's vector and resource cache before building the next target; never retain detached vectors for all targets.
- `clear_resource_cache_keep_allocation` bounds reusable selected-blob storage by `O(B_max)`, rather than the sum of selected file bytes.
- Peak auxiliary memory is `O(C + sum(min(C, D_t)) + max(D_t) + H + P + L_max + B_max)`, or `O(T × C + H + P + L_max + max(D_t) + B_max)` in the worst case. `P` includes only selected vacant pairs/map keys plus identity no-change values consumed later; lossy fallback strings are temporary and unselected/duplicate changes do not contribute retained owned path storage. The bound is linear for fixed `T` and never `O(C²)`.
## Verification and validation

Run these commands from the repository root in order.

1. Deterministic traversal and inspected-change regression:

```sh
cd trueflow && cargo test --lib commands::review::tests::collect_diff_review_files_traverses_and_inspects_once_per_diff_target -- --exact
```

The 128-file/two-target fixture must report exactly `2` tree-diff invocations and `256` inspected detached changes.

2. Batch VCS `ChangedPath`, unavailable states, additions/deletions, and rewrite destination key:

```sh
cd trueflow && cargo test --lib vcs::tests::diffs_for_paths_between_trees_preserve_changed_paths_and_file_diff_states -- --exact
```

Direct non-UTF/lossy destination membership:

```sh
cd trueflow && cargo test --lib vcs::tests::selected_destination_lookup_preserves_lossy_non_utf8_locations -- --exact
```

3. Full-value multi-target/duplicate-target ordering and source association:

```sh
cd trueflow && cargo test --lib commands::review::tests::target_diff_batches_preserve_order_duplicates_and_sources -- --exact
```

4. Coordinated rename-aware semantics from issue #15:

```sh
cd trueflow && cargo test --features tui-test-support --test e2e_diff rename_aware_diff
cd trueflow && cargo test --features tui-test-support --test vcs_scope files_changed_main_to_head_preserves_rename_source_and_destination
```

5. Mixed-state post-rename JSON contract:

```sh
cd trueflow && cargo test --test e2e_diff test_batched_main_diff_preserves_mixed_file_states_and_output -- --exact
```

6. Existing binary and deletion regressions:

```sh
cd trueflow && cargo test --test e2e_diff test_main_review_skips_binary_changes -- --exact
cd trueflow && cargo test --test e2e_diff test_main_review_json_keeps_deleted_whole_file_semantic_blocks -- --exact
cd trueflow && cargo test --test bug_regressions test_review_historical_deletion_target_and_range_preserve_deleted_base_content -- --exact
```

7. Generated benchmark fixture smoke:

```sh
cd trueflow && cargo test --features bench --test e2e_bench_fixture test_generated_main_diff_bench_fixture_smoke -- --exact
```

8. Criterion scaling:

```sh
cd trueflow && cargo bench --features bench --bench full_review -- batch_diff_review
```

Record medians and ratios. Both `M200 / M100` and `M400 / M200` must be at most `2.5`, with output counts exactly `100`, `200`, and `400`.

9. Final repository gate:

```sh
just check
```

Behavioral/manual checks:

- For each selected destination, inspect the ordered per-target values before block assembly: each `FileDiff.changed_path.source_location` belongs to the target at the same position.
- Reverse two historical targets. Target-local sources and hunk markers must reverse together; destination file order must not change.
- Confirm duplicate targets contribute duplicate full `FileDiff`/hunk inputs rather than being coalesced.
- Confirm an edited rename displays only the destination and includes source-side removed content under that destination; do not compare against the known-bad pre-issue-#15 head-only result.
- Confirm binary and external values retain distinct `FileDiffUnavailableReason` states until review intentionally projects them to no semantic hunks.
- Confirm a destination absent from one target receives identity `NoTextChanges` for that target while a later target's source pair and hunks remain intact.
## Acceptance criteria

- A permanent instrumented test proves exactly one file-diff `repo.diff_tree_to_tree` invocation per `ReviewDiffTarget`: the 128-file/two-target fixture records `2`, not `256`.
- The same test proves each detached change vector is inspected once: it records `256` inspections, not the triangular `16,512`. A one-traversal implementation that rescans by selected path does not pass.
- For fixed `T` and `D_t = C`, the implemented file-diff bound is `Theta(T × C + H)`, not `Theta(T × C²)`.
- Issue #15's `ChangedPath` and issue #25's batch API land as one coordinated cutover: every `FileDiff` retains its target-specific source/destination pair, and no permanent singular-review or hunk-only bridge remains.
- The batch VCS API resolves each target tree pair once, builds one borrowed selected-destination index, creates one reusable resource cache, and returns full `FileDiff` values keyed by destination. Valid UTF-8 `change_ref.location()` values use borrowed allocation-free lookup; invalid locations use `to_str_lossy()` before probing so U+FFFD selection matches the singular baseline. Only a vacant selected destination proceeds to `ChangedPath` construction/validation.
- `collect_diff_review_files` builds ordered target batches before its destination loop. It uses each full value's `source_location` for that target's base lookup and `location` for head/output identity before consuming hunks.
- The same destination may have different sources in different targets. Association remains positional/per-target; no unioned or global destination-to-source map exists.
- `file_diff_from_change` remains the sole converter for text, no-text, binary, and external states. All variants retain `ChangedPath`; hunk paths remain destinations.
- Target order and within-target hunk order are unchanged. Duplicate and overlapping targets are not deduplicated.
- Additions, deletions, rewrites, absent paths, binary files, and external states pass explicit regressions. Rewrite output is keyed only by destination while base lookup uses the retained source.
- The mixed E2E expectation uses corrected post-issue-#15 rename semantics: one destination file with source-side removed content and no separate source entry. Batching introduces no output changes beyond that intentional rename fix.
- Target batches and review assembly do not clone `FileDiff`, `DiffHunk`, or hunk-line payloads. After membership lookup, unselected and already-occupied changes allocate no owned path/pair or blob resources; invalid UTF-8 locations may allocate only the documented temporary lossy `Cow`. Peak auxiliary allocation obeys `O(T × C + H + P + L_max + max(D_t) + B_max)` and remains linear for fixed `T`.
- Criterion measures exactly 100, 200, and 400 changed files. On one machine/run, both median doubling ratios are `<= 2.5` (or better-than-linear with exact fixture counts), not approximately `4×`.
- All focused commands and final `just check` pass.
## Non-goals and risks

- No parser, tree-sitter, block-splitting, hashing, coverage, or semantic parser micro-optimization belongs in this issue. Do not tune or parallelize parsing to improve the benchmark.
- Do not change main/merge-base resolution, revision-parent semantics, revision-range endpoints, rename detection configuration, diff context, diff algorithm, or public output schema.
- Issue #15 owns endpoint-aware path selection and the canonical `ChangedPath` migration. Issue #25 consumes that contract; it must not invent another path-pair type, infer fuzzy renames, or add a destination-only shim.
- Do not preserve the known-bad pre-issue-#15 rename block sides in a golden. “No output change” for issue #25 means equivalence to corrected rename-aware semantics.
- Do not combine or deduplicate diff targets. A destination may have different sources per target, so unordered flattening is a correctness bug.
- Do not redesign issue #16's block-start removal attribution here. Retain the target boundary and full per-target inputs so issue #16 can operate without reconstructing lost associations.
- Do not separately batch base blob parsing in this issue. Loading from the correct retained source is required; optimizing those linear lookups is a separate profiling question.
- Do not replace the TUI's lazy one-file cache with eager repository-wide diffing. Its active singular APIs share the canonical converter but retain lazy behavior.
- `gix::Repository::diff_tree_to_tree` still materializes one detached vector per target. That linear locked-API allocation is not justification for an unproven low-level streaming rewrite.
- Non-UTF tree locations are a compatibility edge case. Preserve the current lossy U+FFFD lookup behavior and its bounded temporary allocation; do not reject such changes or require a raw-byte `RepoPath` redesign in this performance fix.
- Reusing a cache without clearing resources can retain all selected blobs. Call `clear_resource_cache_keep_allocation` after every converted selected change and fail rather than continue with stale resources.
- `HashMap` order must never determine target, hunk, or file order. Use the input target slice and sorted destination slice exclusively for observable ordering.
- End-to-end benchmark time includes scanning and base-state work. The traversal plus inspected-change counters are the hard algorithmic guards; Criterion is the user-visible scaling guard.
