# Issue #15: Preserve rename source paths in semantic diffs

Status: ready

Date: 2026-07-10

Baseline commit: 9a98914698c4

## Problem

Tree diffs already ask gix to detect rewrites, but trueflow reduces every changed entry to one `RepoPath`: `ChangeRef::location()`, which is the destination/current path of a rewrite. That is sufficient to find and display the head file, but not to reconstruct the base side of a rename.

For a rename such as `src/old.rs -> src/new.rs`, `trueflow review --target main` currently:

- selects and scans `src/new.rs`;
- asks gix to diff the source blob against the destination blob, so the textual hunks are correct;
- then looks up `src/new.rs` in the base tree, where it does not exist;
- consequently assembles the semantic diff with no base blocks.

A head block touched by the rename is therefore classified as head-only rather than changed, and a function removed while the file was renamed has no head block and disappears from review entirely. Directory intersections have a second failure: a rename out of a selected directory is omitted because only the destination participates in changed-path selection.

The diff pipeline must retain a pair of paths for each gix-confirmed tree change. Base lookup uses the source path; head lookup, review identity, JSON/TUI display, coverage lookup, and hunk `file_path` use the destination path. A pure rename with identical content remains zero semantic churn.

## Evidence

- `trueflow/src/vcs.rs::collect_changed_paths` calls `repo.diff_tree_to_tree(..., None)`, filters blob-like changes, and inserts only `change_ref.location()` into `HashSet<RepoPath>`. For a gix `Rewrite`, `location()` is the destination while `source_location()` is the path in the old tree. For additions, deletions, and modifications, gix returns the same path from both accessors.
- The `None` options passed to `diff_tree_to_tree` load repository diff configuration. With no explicit override, gix enables its default rewrite tracking; `diff.renames` may explicitly enable or disable it. The fix must consume gix's result rather than perform another similarity search.
- `trueflow/src/vcs.rs::diff_for_file_between_trees` also matches a requested file by `change_ref.location()`, but then calls `diff_cache.set_resource_by_change(change_ref, ...)`. gix correctly loads the rewrite's old/source blob and new/destination blob here, which is why the resulting hunks can describe a deletion even though trueflow later has no base `FileState`.
- `trueflow/src/vcs.rs::FileDiff` retains only one `path` in all variants. In particular, `NoTextChanges` cannot retain the source path of an identical-content rename for later base reconstruction.
- `trueflow/src/vcs.rs::file_state_for_path_in_main_base`, `file_state_for_path_in_revision_base`, and `file_state_for_path_in_revision` already separate the tree lookup path from the output path. They can therefore load the old blob from a rename's source location while assigning the destination path to the returned `FileState`; their callers currently pass the same path for both arguments.
- `trueflow/src/targets.rs::ResolvedTargets` and `ReviewPathSelection::Scoped::changed` store `HashSet<RepoPath>`. `ReviewPathSelection::includes` can intersect an explicit file/directory scope with only that one path, so it cannot express “the source is in this directory, but the destination is outside it.”
- `trueflow/src/commands/review.rs::preselected_paths_for_review` uses the changed-path set to limit worktree/revision scanning. `selected_review_paths` uses the same single paths to drive diff assembly. Both must test scope membership against both endpoints but return only destination paths for head scanning and output.
- `trueflow/src/commands/review.rs::collect_diff_review_files` keys head files by the selected/display path, then passes that same path to both `base_file_states_for_diff_targets` and `diff_hunks_for_file_targets`. The base helper therefore looks for the destination in each target's base tree and returns `None` for a rename.
- `collect_diff_review_files` also calls the singular `diff_for_file*` path once per selected destination and target. Implementing rename lookup by adding another per-path call would preserve source metadata but retain the quadratic tree traversal addressed by issue #25; conversely, projecting issue #25's batch result to `Vec<DiffHunk>` would discard the `ChangedPath` needed here.
- `trueflow/src/commands/tui.rs::ensure_cached_file_diff`, the TUI test fixtures that populate `file_diff_cache`, and `trueflow/src/commands/tui/test_support.rs::TuiTestHarness::preload_text_diff` construct `FileDiff` directly with a bare `RepoPath`. They are part of the required `ChangedPath::identity` cutover even though the TUI's production diff loading remains intentionally lazy and singular.
- `collect_diff_review_blocks_for_file` can already create a base-only `DiffReviewBlock` when a changed base block has no matching head block. The missing behavior is upstream: the renamed file's base blocks never reach this function.
- `trueflow/tests/e2e_diff.rs::test_main_review_handles_renamed_file` checks only that some review file has path `src/new.rs`. Its fixture adds a function during the rename and never asserts that source-side blocks were loaded, that a removed block survives, or that a pure rename creates no review work.
- `trueflow/tests/e2e_diff.rs::test_main_review_json_keeps_deleted_whole_file_semantic_blocks` proves ordinary deletion reconstruction already works when the base and display paths are identical. That behavior is a required regression guard for the path-pair cutover.

## Reproduction

Use a repository with rewrite detection explicitly enabled so the test does not depend on developer Git configuration:

1. Set `diff.renames=true` in the `TestRepo`.
2. On the base branch, commit `src/old.rs` with a retained function and a second function containing a unique `removed during rename` marker.
3. Create the feature branch, `git mv src/old.rs src/new.rs`, remove the marked function while leaving enough content identical for gix to confirm a rewrite, and commit.
4. Run `trueflow review --target main --json` through `TestRepo::run`.

At the baseline, gix produces a deletion hunk against `src/new.rs`, but the JSON does not contain the removed function because review tries to load `src/new.rs` from the base tree. With the fix, there is one review file at `src/new.rs`, its blocks include the base-only removed function, and there is no separate `src/old.rs` entry.

A second reproducer moves `src/scoped/old.rs` to `archive/new.rs` and runs review with `--target dir:src/scoped` plus the relevant main/revision target. The baseline returns no renamed file because its changed set contains only `archive/new.rs`; the corrected result is still displayed as `archive/new.rs` because the source endpoint satisfies the directory scope.

The new E2E tranche must be committed before source changes. On the baseline, the rename-with-deletion and rename-out-of-scope assertions make the focused tranche red. The pure-rename and ordinary add/delete cases are control cases in the same test-first tranche: they lock down behavior that the source fix must not regress.

## Root cause

The code models a tree change as a path instead of as a transition between two locations. That lossy conversion occurs in `collect_changed_paths`, before target resolution and path-scope intersection. The review layer then has no way to distinguish:

- the location used to load the old tree (`source_location`), and
- the location used to load the head and identify the review file (`location`).

Although `set_resource_by_change` still has the original gix `ChangeRef` and constructs correct old/new text resources, `FileDiff` discards the source location. Review subsequently performs a separate base-file lookup using only the destination. This splits one rewrite into incompatible views: rename-aware text hunks and rename-unaware semantic blocks.

Flattening a rewrite into two independent `RepoPath` values would not fix the problem. It would scan the source as a deletion and the destination as an addition, lose the one-to-one relationship needed for base lookup, and make a pure rename look like whole-file churn. The source and destination must remain one value through changed-path resolution and each per-target `FileDiff`.

## Implementation plan

**Coordinated delivery order:** implement issue #14 first; then implement issues #15 and #25 as one coordinated `ChangedPath` plus target-first batch-`FileDiff` cutover; then implement issue #16 against the retained per-target base/hunk groups. Issue #15 must not introduce a per-destination traversal that issue #25 immediately removes, issue #25 must not project away source paths, and issue #16 must not receive hunks flattened across independent target coordinate spaces.

1. **Add the observable red E2E tranche in `trueflow/tests/e2e_diff.rs` before changing source.** Use inline Rust fixtures or the existing rename constants, call `repo.git(&["config", "diff.renames", "true"])` in rename scenarios, and keep enough unchanged lines for deterministic gix rewrite confirmation.
   1. Replace or strengthen the weak rename test as `test_rename_aware_diff_keeps_deleted_function_under_destination`: rename `src/old.rs` to `src/new.rs`, remove an EOF function, and assert exactly one destination-path review file contains that function's base-only marker. Also assert no `src/old.rs` file is emitted.
   2. Add `test_rename_aware_diff_includes_rename_into_directory_scope`: move a file from outside `src/scoped` into it, remove or edit reviewable content, query a revision range with `--target dir:src/scoped`, and assert the destination file and source-side changed content are reviewable. This exercises `file_state_for_path_in_revision` for the range base.
   3. Add `test_rename_aware_diff_includes_rename_out_of_directory_scope`: move a file from `src/scoped` to `archive`, remove a reviewable EOF function, query the rename commit with `--target dir:src/scoped --target rev:<commit>`, and assert the result is present under the destination `archive/...` path. This exercises `file_state_for_path_in_revision_base` and proves source-side scope membership survives destination preselection.
   4. Add `test_rename_aware_diff_ignores_pure_rename_churn`: perform an identical-content rename and assert the JSON array is empty. Retaining source metadata must not turn a pure rewrite into added/deleted semantic blocks.
   5. Add `test_rename_aware_diff_preserves_ordinary_add_delete`: add one Rust file and delete another without a rename; assert the added file exposes head content and the deleted file exposes base content, each under its unchanged path. This protects ordinary `Addition` and `Deletion` handling.
   6. Run the focused E2E command from the verification section and record the expected baseline failures before source edits.

2. **Add narrow data/selection regression tests.** These tests define the internal contract independently of semantic block matching.
   1. In `trueflow/tests/vcs_scope.rs`, add `test_files_changed_main_to_head_preserves_rename_source_and_destination`. Force rename detection, commit a high-similarity rename, and assert the public changed-path result contains one pair with `source_location == "src/old.rs"` and `location == "src/new.rs"`, rather than two unrelated paths.
   2. In `trueflow/src/targets.rs` tests, add `scoped_selection_matches_rename_when_either_endpoint_is_selected`. Cover both outside-to-inside and inside-to-outside pairs, assert the destination is selected in both cases, and assert an unrelated pair is excluded. Keep an explicit-file variant so `file:<old-path>` also follows the rename to its destination.
   3. Update existing target, feedback, and feedback-export test constructors to use identity path pairs for ordinary changed files; do not loosen their destination-based assertions.

3. **Introduce one canonical path-pair model in `trueflow/src/vcs.rs` and use it for every tree change.** Define:

   ```rust
   #[derive(Debug, Clone, PartialEq, Eq, Hash)]
   pub struct ChangedPath {
       pub source_location: RepoPath,
       pub location: RepoPath,
   }
   ```

   Add a constructor for an identity change and a private conversion from `gix::diff::tree_with_rewrites::ChangeRef`. The invariants are:

   - `source_location` is the old-tree lookup path;
   - `location` is the new-tree/current/display path;
   - the fields differ only when gix emitted a `Rewrite` (rename or a copy rewrite enabled by Git configuration);
   - additions, deletions, modifications, and dirty-worktree paths use an identity pair; whether a blob exists on a side is determined by tree lookup, not by making either path optional;
   - both paths are validated `RepoPath` values, and blob/tree filtering remains unchanged.

   Change `collect_changed_paths` and the `files_changed_main_to_head*`, `files_changed_in_revision*`, and `files_changed_in_range*` APIs to return `HashSet<ChangedPath>`. Do not add a destination-only compatibility API: update all callers in the same cutover.

4. **Keep rewrite metadata attached to each full per-target `FileDiff` and share issue #25's target-first VCS batch primitive.** Replace the lone `path` in every `FileDiff` variant with `ChangedPath`, and add shared accessors for the pair and optional hunks.
   1. Make `file_diff_from_change` accept the full pair, continue passing the original `ChangeRef` to `set_resource_by_change`, and build every `DiffHunk.file_path` from `pair.location`.
   2. Preserve the pair in `FileDiff::Text`, `FileDiff::NoTextChanges`, and `FileDiff::Unavailable`. This is required for pure renames and binary/external diffs to retain identity without inventing semantic churn.
   3. Coordinate with issue #25 on one crate-private, destination-keyed `SelectedFileDiffs` result containing full `FileDiff` values. Its main, revision, and range entry points accept the complete sorted selected-destination slice, resolve one target tree pair, create one reusable resource cache, traverse `repo.diff_tree_to_tree` once, and convert each selected change exactly once.
   4. Key the batch index by `ChangedPath::location` but keep the entire `FileDiff` as its value. Do not create a source-keyed second entry, and do not project the batch to `HashMap<RepoPath, Vec<DiffHunk>>` in VCS or at the review batch boundary.
   5. `SelectedFileDiffs::take(destination)` returns the indexed full diff or an identity-pair `NoTextChanges` for a path absent from that target. A missing path is target-local; never infer its source from the unioned changed-path set.
   6. Retain the singular `diff_for_file`, `diff_for_file_in_revision`, and `diff_for_file_in_range` APIs for the TUI's lazy one-file cache, but implement them by invoking the same batch primitive with one destination. They are active callers, not compatibility shims, and there must be only one change-to-`FileDiff` converter.
   7. The review implementation must never call a singular wrapper from inside the selected-path loop. This makes the issue #15 source-path fix and issue #25 traversal bound the same source cutover rather than serial refactors.

5. **Propagate pairs through target resolution and endpoint-aware scoping in `trueflow/src/targets.rs`.** Change `ResolvedTargets::changed` and `ReviewPathSelection::Scoped::changed` to `HashSet<ChangedPath>`, and update the main/revision/range resolver callback bounds accordingly. Convert `dirty_files()` results to identity pairs because worktree status does not provide a tree-rewrite source.
   1. Define `ReviewPathSelection::includes(destination)` so a changed pair is eligible only when its `location` equals that destination and the explicit file/directory selection matches either `source_location` or `location`.
   2. With no explicit file/directory selection, all changed destinations remain eligible.
   3. With no changed-path constraint, retain the existing ordinary explicit file/directory behavior.
   4. Keep destination identity at the API boundary: callers ask whether a destination is included; the old source path must not become a second review file.

6. **Update every consumer and constructor of the changed-path and `FileDiff` types as a clean cutover.** This is supporting work required by the central type change, not an expansion of review behavior.
   1. In `trueflow/src/commands/review.rs::preselected_paths_for_review`, filter pairs with endpoint-aware scope logic and collect only `location` values for worktree/revision head scanning. Deduplicate destinations with `HashSet`.
   2. In `trueflow/src/commands/review.rs::selected_review_paths`, use changed-pair destinations as candidates, test scope against both endpoints through `ReviewPathSelection`, and return each destination once. For non-diff directory scans, retain the current head-map behavior.
   3. In `trueflow/src/commands/feedback.rs` and `trueflow/src/feedback_export.rs`, update changed-selection types and tests. Record filtering remains destination-based; no source path is added to exported feedback.
   4. In `trueflow/src/commands/tui.rs::filter_commits_for_prefix` and its existing test, accept identity/path-pair values and retain a commit when either endpoint matches the workdir prefix.
   5. In `trueflow/src/commands/tui.rs::ensure_cached_file_diff`, construct the loader-error `NoTextChanges` fallback with `ChangedPath::identity`; keep `cached_file_diff_for_node` calling the singular lazy wrappers.
   6. Migrate every direct `FileDiff::{Text, NoTextChanges, Unavailable}` constructor in the `trueflow/src/commands/tui.rs` test module—including all `file_diff_cache` fixtures beginning with the source/diff rendering tests around the current line 11078—to use `ChangedPath::identity`. Do not leave bare `path: RepoPath` fixtures that make only non-TUI targets compile.
   7. Update `trueflow/src/commands/tui/test_support.rs::TuiTestHarness::preload_text_diff` to build an identity pair. Its `trueflow/tests/tui_vt100.rs` caller then exercises the same constructor through the supported harness seam.

7. **Assemble semantic diffs from a target-first, full-`FileDiff` result in `trueflow/src/commands/review.rs`.** Replace the split `base_file_states_for_diff_targets`/`diff_hunks_for_file_targets` per-path work with one coordinated issue #15/#25 input builder.
   1. Compute and sort selected destination paths once. Keep `head_files_by_path` and `DiffReviewFile.path` keyed by `ChangedPath::location`.
   2. Iterate `ReviewDiffTarget` values in caller order. For each target, call exactly one matching VCS batch entry point with all selected destinations; do not iterate destinations outside and invoke singular `diff_for_file*`.
   3. Drain each target's `SelectedFileDiffs` in sorted destination order into a destination-keyed vector of full target inputs. Each element must retain its target index/identity, complete `FileDiff` (and therefore `ChangedPath` plus unavailable state), and the base `FileState` loaded for that target. Repeated targets remain repeated entries in the same order.
   4. Load each base from that element's `FileDiff.changed_path().source_location` using `file_state_for_path_in_main_base`, `file_state_for_path_in_revision_base`, or `file_state_for_path_in_revision`, and pass the destination as `output_path`. Never choose a source from the unioned `HashSet<ChangedPath>`.
   5. Do not project to hunks before source-based base lookup, and do not flatten target identity after lookup. The current issue may aggregate bases for existing block matching, but the retained per-target `{ base, FileDiff }` groups are the contract consumed by issue #16 so old-line mappings are never applied across targets.
   6. When `collect_diff_review_files` processes one destination, derive its current hunk view from the ordered full diffs without cloning hunk lines. `NoTextChanges` and unavailable variants contribute no semantic hunks but retain their pair/state until this boundary.
   7. Remove the independent second tree-diff pass and the now-invalid `repo_relative_path = display_path` assumption. Preserve target order, duplicate targets, selected-path sorting, current base-block deduplication, and final review-file ordering.
   8. Leave `collect_diff_review_blocks_for_file` behavior unchanged in this issue except for accepting the retained target-group boundary needed by issue #16. Once the correct rename base arrives, its existing unmatched-base path emits the removed function as `BlockChangeKind::Deleted`.
   9. Compute language and file hash with the existing head-first fallback. A rename that changes extension parses the base blob using the source tree path while displaying and preferring the head language at the destination.

8. **Run the focused smoke tests immediately after the minimal source cutover.** The E2E result must show the removed source block under only the destination path, both directory directions, no pure-rename churn, and unchanged ordinary add/delete semantics. Run the VCS pair and target-scope unit tests as well.

9. **Only after the focused smoke is green, perform cleanup and full validation.** Remove superseded destination-only path fields/helpers and the weak rename-only assertion rather than leaving parallel APIs. Update the `ReviewPathSelection` comment to state the endpoint-scope/destination-output invariant, format the touched Rust files, run the complete `e2e_diff` target, then run the repository gate. No migration or compatibility shim is needed because the project is beta.

### Ordered affected files and symbols

1. `trueflow/tests/e2e_diff.rs`
   - existing rename fixture/test;
   - new rename+deletion, into/out-of-directory, pure-rename, and ordinary add/delete E2E cases.
2. `trueflow/tests/vcs_scope.rs`
   - changed-path API assertions for source/destination preservation.
3. `trueflow/src/vcs.rs`
   - new `ChangedPath` and `ChangedPath::identity`;
   - all `FileDiff` variants/accessors and `file_diff_from_change`;
   - issue #25's `SelectedFileDiffs`, main/revision/range batch entry points, and sole `diffs_for_paths_between_trees` traversal/conversion path;
   - singular `diff_for_file*` wrappers delegated to the one-path batch primitive for lazy TUI use;
   - `collect_changed_paths` and all `files_changed_*` entry points;
   - existing base-tree `file_state_for_path_*` functions remain the lookup primitives.
4. `trueflow/src/targets.rs`
   - `ReviewPathSelection`, `ResolvedTargets`, `resolve_targets_with`, and their tests.
5. `trueflow/src/commands/review.rs`
   - `preselected_paths_for_review`, `selected_review_paths`, and `collect_diff_review_files`;
   - replace `base_file_states_for_diff_targets` plus `diff_hunks_for_file_targets` with the ordered target-first full-`FileDiff` input builder;
   - retain per-target base/`FileDiff` identity for issue #16 ownership mapping.
6. `trueflow/src/commands/feedback.rs` and `trueflow/src/feedback_export.rs`
   - changed-selection type cutover and existing selection tests only.
7. `trueflow/src/commands/tui.rs`
   - `filter_commits_for_prefix`;
   - `ensure_cached_file_diff` identity fallback;
   - every direct `FileDiff` test fixture/cache constructor;
   - lazy `cached_file_diff_for_node` remains on singular wrappers backed by the shared batch primitive.
8. `trueflow/src/commands/tui/test_support.rs`
   - `TuiTestHarness::preload_text_diff` identity-pair constructor.

No configuration, schema, persisted review format, JSON output shape, or fixture file needs to change.

## Verification and validation

Run these commands from the repository root, in order.

1. Red/green E2E tranche for the five required observable cases:

   ```sh
   cd trueflow && cargo test --features tui-test-support --test e2e_diff rename_aware_diff
   ```

2. Source/destination pair collection:

   ```sh
   cd trueflow && cargo test --features tui-test-support --test vcs_scope files_changed_main_to_head_preserves_rename_source_and_destination
   ```

3. Endpoint-aware target intersection and changed-selection regressions:

   ```sh
   cd trueflow && cargo test --features tui-test-support --lib targets::tests::scoped_selection_matches_rename_when_either_endpoint_is_selected
   cd trueflow && cargo test --features tui-test-support --lib commands::feedback::tests::feedback_changed_selection_keeps_main_target_changed_paths
   ```

4. Shared issue #15/#25 target-first full-`FileDiff` contract and TUI fallback:

   ```sh
   cd trueflow && cargo test --features tui-test-support --lib vcs::tests::diffs_for_paths_between_trees_preserve_changed_paths_and_file_diff_states -- --exact
   cd trueflow && cargo test --features tui-test-support --lib commands::review::tests::collect_diff_review_files_traverses_and_inspects_once_per_diff_target -- --exact
   cd trueflow && cargo test --features tui-test-support --lib commands::tui::tests::ensure_cached_file_diff_inserts_no_text_changes_on_loader_error -- --exact
   ```

5. Entire semantic diff integration target, including existing whole-file deletion and approval round-trip coverage:

   ```sh
   cd trueflow && cargo test --features tui-test-support --test e2e_diff
   ```

6. Compile and run all library unit tests affected by the clean type cutover, including review, feedback export, and every TUI `FileDiff` fixture:

   ```sh
   cd trueflow && cargo test --features tui-test-support --lib
   ```

7. Final repository validation:

   ```sh
   just check
   ```

Behavioral assertions to inspect in the focused E2E output/JSON:

- rename plus function deletion: one file at the destination, the removed function's base content is reviewable, and the source path is absent as a separate file;
- rename into a selected directory: the destination inside the directory is included and receives the correct base side from outside the directory;
- rename out of a selected directory: the source endpoint satisfies the scope, but the emitted path is the destination outside the directory;
- pure rename: `[]`, with no whole-file added/deleted blocks;
- ordinary addition and deletion: the add uses head content, the delete uses base content, and both retain identity paths;
- main, single-revision, and revision-range target variants all use their own source path for their own base tree.

## Acceptance criteria

- A gix-confirmed rewrite is represented by one `ChangedPath` containing both validated `source_location` and destination `location`.
- Non-rewrite modifications, additions, deletions, and dirty-worktree changes use identity pairs and preserve current behavior.
- `collect_changed_paths` no longer discards `ChangeRef::source_location()`, and all main/revision/range changed-path APIs carry pairs without a destination-only shim.
- `FileDiff` retains its pair for text, no-text, and unavailable outcomes; hunk paths remain destinations.
- Each target's base blob is loaded from that target's `source_location` and is assigned the destination output path.
- Head scanning, `DiffReviewFile.path`, tree nodes, coverage lookup, marking identity, JSON, and TUI display use only the destination.
- Explicit file/directory scopes intersect a changed pair when either endpoint matches, then select/scan/display the destination exactly once.
- A file renamed while deleting a function exposes that removed base block under the destination path and can proceed through the existing deleted-block review/approval flow.
- A pure identical-content rename produces no reviewable semantic blocks.
- Rename-into-scope and rename-out-of-scope behavior is covered at E2E level, including revision-range and single-revision base lookup.
- Ordinary add/delete behavior remains covered and unchanged.
- Multi-target reviews preserve target-to-path-pair association before base blocks and hunks are combined.
- The coordinated issue #15/#25 API performs one target-first tree traversal per diff target, returns full destination-keyed `FileDiff` values, and performs no singular per-destination traversal or early hunk-only projection in review.
- All TUI production fallbacks, unit fixtures, and `TuiTestHarness::preload_text_diff` construct `FileDiff` with `ChangedPath::identity`; lazy TUI loading continues through singular wrappers backed by the shared primitive.
- Delivery order is issue #14, then the coordinated issues #15 and #25 cutover, then issue #16 using the retained per-target inputs.
- All focused commands and the final `just check` pass.

## Non-goals and risks

- **No fuzzy rename or copy detector in trueflow.** Consume only `Rewrite` values emitted by gix under the repository's configured rewrite policy. Do not override `diff.renames`, run a second similarity pass, or infer a pair from unrelated additions/deletions. If rewrite detection is disabled or gix does not confirm similarity, ordinary add/delete semantics are expected.
- **No dirty-worktree rename inference.** The status path has no old-tree partner in this flow and remains an identity pair.
- **No source path in public review output.** This issue repairs internal base lookup and scoping; JSON/TUI file identity remains the destination and the output schema is unchanged.
- **No compatibility layer or persisted-data migration.** `ChangedPath` is an in-process selection/diff type. Review records and block hashes are not rewritten.
- **No block-matching redesign.** Existing changed/deleted block matching should work once the correct base blocks are supplied. Expand that algorithm only if the focused rename test exposes an independent defect.
- **Gix configuration is a test risk.** Rename E2E fixtures must set `diff.renames=true` and retain enough identical content to guarantee gix emits a rewrite. Tests must not rely on a developer's global Git configuration.
- **Multiple targets are an association risk.** A destination may have different source paths for different targets. Keep the pair attached to each `FileDiff`; never select an arbitrary source from a unioned `HashSet`.
- **Cross-plan contract risk.** Issue #25 must characterize edited-renames after this rename-aware behavior is established (or compare batch output with the rename-aware singular primitive), not freeze the known-bad destination-only base sides. Issue #16 must consume the retained per-target groups rather than a flat hunk slice.
- **Rename with extension change is a parser risk.** Load and parse the base blob using its source tree path, but retain destination display identity and head-first language/hash selection.
- **Gix-confirmed copy rewrites may also have different endpoints.** Preserve the pair gix supplies, but do not broaden copy discovery or special-case copies beyond the same source/base and destination/head invariants.
