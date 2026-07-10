# Issue #16: Attribute block-start removals by matched base/head ownership

- Status: ready
- Date: 2026-07-10
- Baseline commit: `9a98914698c4`

## Problem

A removed diff line has an old-file coordinate but no real head-file coordinate. `DiffChangedLineIndex` currently gives each removal a synthetic head anchor at the current `new_line`, then `change_kind_for_block` discards every removal anchored at the first line of a head block when no addition shares that anchor. This protects a real boundary case—deleting one independent block immediately before another must not mark the surviving following block changed—but it also discards removals that belong to the surviving block's base version.

Rust makes the false negative easy to observe because the splitter attaches top-level attributes and comments to the following declaration. Removing `#[inline]`, `#[cfg(...)]`, or a `///` line while retaining the declaration should produce one `Changed` review block with both base and head sides. At present, the base block is reviewable, the head block is suppressed as unchanged, and review collection cannot pair them; the result is incorrectly represented as a base-only `Deleted` block.

The fix must resolve the coordinate ambiguity through generic block ownership after base/head matching. It must not recognize Rust attributes, comments, or any other language syntax inside VCS attribution.

## Evidence

- `trueflow/src/vcs.rs:136-145` stores a side-specific `DiffChangedLineIndex` whose entries have one `anchor_line` even though `DiffLine` already distinguishes `old_line` and `new_line`.
- `trueflow/src/vcs.rs:160-199` implements `DiffChangedLineIndex::change_kind_for_block`. Lines 172-183 special-case the head side: all changes at `block.start_line + 1` are removed from consideration when none is an addition. A non-whitespace removal then becomes `NoTextChanges` at lines 186-190.
- `trueflow/src/vcs.rs:691-737` shows the ambiguity's source. A removal is indexed at its actual `old_line` for the base side but at the current `new_line` cursor for the head side. `DiffLine.new_line` remains `None`; the head anchor is only a deletion insertion point, not ownership evidence.
- `trueflow/src/vcs.rs:741-813` already has the desired reviewability policy for whitespace-only lines and trim-equivalent remove/add runs. The ownership fix must continue to present both halves of a matched replacement to this classifier in hunk order.
- `trueflow/src/commands/review.rs:1054-1076` classifies and discards base and head blocks independently before matching. Consequently, a removal-only surviving head block eliminated by the heuristic is absent from `changed_head_blocks`.
- `trueflow/src/commands/review.rs:1078-1133` can pair a changed base block only with that prefiltered head set; otherwise it emits a base-only block. This converts a surviving declaration with removed attached metadata into `Deleted` rather than `Changed`.
- `trueflow/src/commands/review.rs:1146-1208` already supports unique semantic matching and mapped-range fallback, but matching happens too late and only over changed head blocks. Semantic matches also must be reserved globally before positional fallback so an earlier ambiguous base block cannot consume a later survivor's exact head match.
- `trueflow/src/commands/review.rs:1224-1255` maps a removed old line to the current head cursor. That anchor is useful for rendering a deletion but cannot decide which adjacent block owns it.
- `trueflow/src/block_splitter.rs:348-386` accumulates Rust `attribute_item`, `line_comment`, and `block_comment` nodes into the next top-level block. This establishes the reproduction but is not a source file to change.
- `trueflow/src/vcs.rs:1244-1274` contains the existing regression `head_changed_line_index_does_not_attribute_deletion_to_following_block`. Its behavioral invariant must remain covered under the ownership-aware API rather than being deleted or weakened.
- `trueflow/src/commands/review.rs:597-636` currently loads every target's base file, flattens all of their blocks into one vector, and separately obtains one concatenated hunk vector before block collection. `base_file_states_for_diff_targets` preserves target iteration only until `dedupe_blocks` erases that association.
- `trueflow/src/commands/review.rs:717-756` returns untagged base `FileState` values and deduplicates blocks only by hash and line range. Those coordinates are meaningful only in the target/source file that produced them.
- `trueflow/src/commands/review.rs:872-893` appends every target's hunks into one `Vec<DiffHunk>`. Two targets have independent old-coordinate spaces even when they share a destination path, so this flattened vector is not a valid input to ownership matching.
- Issues #15 and #25 define the prerequisite review input: an ordered, target-first batch result that retains each target's full `FileDiff`, including its `ChangedPath::source_location`, instead of projecting all targets into one destination-keyed hunk vector. Issue #16 must consume that retained grouping rather than reintroduce flattening.

## Reproduction

Use a base/head Rust fixture whose blocks are produced by `block_splitter::split(..., Language::Rust).into_review_blocks()` rather than hand-assigning an attribute to a function.

Attached removal, which is currently misclassified:

```diff
-#[inline]
 fn retained() {}
```

and the corresponding documentation-comment case:

```diff
-/// Calls the retained path.
 fn retained() {}
```

For each case, the base splitter returns one function block containing the prefix and declaration, while the head splitter returns the surviving function block. The expected review result is exactly one block with `change_kind == BlockChangeKind::Changed`, `sides.base.is_some()`, and `sides.head.is_some()`. The baseline instead suppresses the head candidate and emits the base side as `Deleted`.

The ambiguous mirror must continue to behave differently:

```diff
-fn removed() {}
 fn retained() {}
```

The removal belongs to the unmatched base block. The expected review result is one base-only `Deleted` block for `removed`; `retained` must not appear as `Changed` or `Added`.

Also exercise a same-anchor replacement and nonreviewable churn:

```diff
-#[inline]
+#[cold]
 fn retained() {}
```

must remain one paired `Changed` block, whereas a trim-equivalent remove/add run and whitespace-only add/remove lines owned by the same matched block must remain `OnlyNonreviewableChurn` and produce no diff review block. If whitespace churn is mixed with a real attached-line removal or replacement, the real change wins and the block remains reviewable.

## Root cause

The current implementation asks a head coordinate to answer a base-ownership question. A removal's `new_line` cursor can denote either:

1. a removed prefix that was inside the base version of the surviving head block, or
2. a removed preceding block whose deletion collapses onto the following head block's start.

The presence or absence of an addition at that anchor does not distinguish those cases. Blanket suppression chooses case 2 for every removal-only anchor. Conversely, simply deleting the suppression would choose case 1 for every such anchor and regress the existing following-block test.

Ownership is available only after blocks are matched. A removal belongs to a matched review unit when its actual `old_line` lies in that unit's base block; an addition belongs when its actual `new_line` lies in the head block. An unmatched base block owns its removals, an unmatched head block owns its additions, and a head block never owns a removal merely because the deletion anchor equals its start.

That ownership statement is target-relative. For a multi-target review, target A's old line 12 and target B's old line 12 refer to different base trees and may even come from different rename source paths. Base parsing, unchanged-line mapping, matching, and change classification must run against one ordered per-target `FileDiff` at a time. Only completed target-scoped review units may be deterministically deduplicated for display.

Matching must therefore precede final change classification within each target. Matching itself must not use deletion anchors as evidence that an unchanged following block is the survivor: reserve unique semantic matches first, then use actual unchanged old-to-new line correspondences, and expose only independently changed blocks to the existing positional fallback. Flattening bases or hunks across targets before this work would make even a correct single-target ownership algorithm unsound.

## Implementation plan

1. **Add the paired red-test matrix in `trueflow/src/commands/review.rs` before changing source behavior.**
   - Add `collect_diff_review_blocks_block_start_removal_marks_attached_attribute_as_changed` and `collect_diff_review_blocks_block_start_removal_marks_attached_doc_comment_as_changed` using real Rust splitter output for base and head content. Build the corresponding `DiffHunk` explicitly and assert exactly one `DiffReviewBlock`, `BlockChangeKind::Changed`, both sides present, and the head declaration used as the display side. Both tests must fail on the baseline's base-only `Deleted` result.
   - Add the mirror `collect_diff_review_blocks_block_start_removal_does_not_attribute_predecessor_to_survivor`: split a base with `removed` followed by `retained`, split a head containing only `retained`, and assert the sole result is base-only `Deleted` for `removed`. Assert by identity/content that no result includes the retained block as `Changed` or head-only `Added`.
   - Keep `vcs::tests::head_changed_line_index_does_not_attribute_deletion_to_following_block`. When the API changes, adapt its fixture to supply the following block's matched base range so it continues to prove the same observable boundary invariant; do not replace it with an assertion about implementation details.
   - Run the two attached-prefix tests first to establish the red state, and run the predecessor regression alongside them to pin the behavior the source fix is not allowed to trade away.

2. **Add replacement, churn, and target-boundary regressions before the source fix.**
   - In `trueflow/src/commands/review.rs`, add `collect_diff_review_blocks_keeps_block_start_replacement_paired`: a non-trim-equivalent removal/addition at the first line of the same base/head block must be one paired `Changed` result, never separate `Deleted` and `Added` results.
   - In `trueflow/src/vcs.rs`, add `diff_changed_line_index_block_start_whitespace_replacement_is_nonreviewable`, `diff_changed_line_index_block_start_whitespace_only_changes_are_nonreviewable`, and `diff_changed_line_index_block_start_whitespace_mixed_with_removal_is_reviewable`. Cover a trim-equivalent remove/add run, whitespace-only removal and addition, and mixed whitespace plus a real removal. These tests ensure the ownership query combines both sides before `trivial_formatting_only_replacement_run`; classifying the sides independently would incorrectly promote formatting churn.
   - Add `collect_diff_review_blocks_filters_block_start_whitespace_churn` in `trueflow/src/commands/review.rs` and assert that a completely nonreviewable matched run is absent from the observable review list.
   - Add `collect_diff_review_files_block_start_removal_scopes_ownership_by_target_source_path` at the target/file assembly seam introduced by issues #15/#25. Supply two ordered per-target full `FileDiff` inputs for the same destination: target A uses source path `src/old-a.rs` and removes attached metadata from a surviving block; target B uses source path `src/old-b.rs` and deletes an independent predecessor at the same numeric old/head anchors. Assert target A yields one paired `Changed` survivor, target B yields only its base-only `Deleted` predecessor, and neither target's old coordinates or base blocks participate in the other's match.
   - Add `collect_diff_review_files_block_start_removal_preserves_duplicate_target_boundaries`. Supply the same target twice and assert both ordered target entries reach the target-scoped mapping seam, rather than being deduplicated or their hunks merged first; after both produce candidates, assert deterministic final review-unit deduplication emits one logical unit. This preserves issue #25's duplicate-target/order contract without duplicating the visible block.

3. **Honor the prerequisite ordering with issues #15 and #25 before implementing ownership mapping.**
   - Land the coordinated full-`FileDiff` cutover from issues #15/#25 first: target-first batch traversal, `ChangedPath` retained on every `FileDiff`, base lookup through that target's `source_location`, destination-based head/display identity, and an ordered per-target result for every requested `ReviewDiffTarget`.
   - The per-target result must preserve the caller's target slice order and repeated entries. It must not be a `HashMap<RepoPath, Vec<DiffHunk>>`, an untagged `Vec<FileState>`, or any other projection that discards target/source association before issue #16 runs.
   - Treat absence of that API as a sequencing blocker, not an invitation to reconstruct target identity by zipping unordered maps or by flattening hunks and bases. Issue #16 changes classification/matching on the retained inputs; it does not create a second batch or rename data model.

4. **Replace side/anchor inference with an explicit ownership API in `trueflow/src/vcs.rs`.**
   - Introduce an explicit, non-empty ownership enum for `DiffChangedLineIndex::change_kind_for_block`, with variants equivalent to `BaseOnly(&Block)`, `HeadOnly(&Block)`, and `Matched { base: &Block, head: &Block }`. This prevents an invalid neither-side query and makes the caller state which file ranges own changed lines.
   - Construct one `DiffChangedLineIndex` only from one target's `FileDiff::Text` hunks. Index removals by their real `DiffLine.old_line` and additions by their real `DiffLine.new_line`; retain stable hunk/run order so remove/add replacement detection continues to compare the correct lines. Keep coordinate-sorted views (or an equivalent range index) so querying many blocks does not rescan every hunk for every block, and do not clone line text per ownership query.
   - Make `change_kind_for_block` select removals only from the owned base range and additions only from the owned head range. `Matched` combines both sets in diff order before applying the existing `all_changed_lines_are_nonreviewable` policy. `BaseOnly` cannot own additions, and `HeadOnly` cannot own removals.
   - Delete the `side == DiffBlockSide::Head` block-start suppression and `had_nonreviewable_churn_at_block_start`; whitespace remains nonreviewable because selected owned lines still flow through the existing classifier, not because an anchor is discarded.
   - Preserve saturating 0-based `Block` to 1-based diff range conversion and the exclusive `Block.end_line` invariant. Empty ownership overlap returns `NoTextChanges`.

5. **Match and classify within each ordered target input in `trueflow/src/commands/review.rs`.**
   - Replace the current `base_files`/aggregate `hunks` call into `collect_diff_review_blocks_for_file` with a target-scoped helper that receives exactly one target ordinal, its full `FileDiff`, its base `FileState` loaded from that diff's source path, and the shared destination head blocks. `NoTextChanges` and `Unavailable` inputs contribute no owned changed lines and must not borrow another target's hunks.
   - Within one target, produce one-to-one `DiffBlockSides` candidates from that target's complete base-block inventory and the destination head inventory before asking its `DiffChangedLineIndex` for `ReviewableChanges`.
   - Perform matching in explicit phases. First reserve every unambiguous same-kind semantic match from the complete per-target inventories before any positional match. This prevents an earlier deletion from consuming a head block needed by a later exact survivor.
   - For still-unmatched blocks, calculate same-kind overlap from real unchanged old-line/head-line correspondences in that target's hunks and unchanged gaps. Removed and added lines do not contribute to this survivor score. Require positive, unambiguous ownership overlap before reserving the pair. This generically matches a base block whose attached comment was removed to the head declaration, while a completely deleted predecessor has no surviving correspondence to the following block.
   - Retain mapped-range fallback only for remaining base and head blocks independently reviewable on their valid sides (`BaseOnly` removals and `HeadOnly` additions). Never offer an unchanged head block to this fallback solely because a removal is anchored at its start. Keep same-kind, one-to-one matching and deterministic tie handling.
   - Classify each target's `Matched`, `BaseOnly`, and `HeadOnly` candidates with that target's index. Preserve target ordinal until candidate production is complete, concatenate target candidate lists in caller order, then deduplicate equivalent final `DiffReviewBlock` units deterministically. Do not deduplicate base blocks or hunks across target/source coordinate spaces. Preserve final display-position sorting and derive `BlockChangeKind::{Changed, Deleted, Added}` from the sides as today.
   - Update or remove the unreachable side-specific `file_changed_lines` path at `trueflow/src/commands/review.rs:348-358,378-382`. The function has already returned through `collect_diff_scoped_review` whenever both a repository and diff targets exist, so retaining the old head-anchor API only for this branch would leave dead compatibility code.

6. **Run the focused behavioral smoke before cleanup.**
   - Run the exact new attached-prefix, predecessor, replacement, churn, different-source-path, and duplicate-target tests. Confirm attached removal is paired `Changed`, independent deletion leaves the survivor absent, and target A never consumes target B's base lines or hunks.
   - Confirm reversing two distinct target inputs reverses only target-scoped candidate processing, not the deterministic final display order; confirm a repeated target is processed twice before final review-unit deduplication.

7. **Only after the focused smoke passes, remove superseded scaffolding and perform final validation.**
   - Remove `DiffBlockSide`, `IndexedChangedDiffLine::anchor_line`, side-specific constructors/helpers, aggregate cross-target block/hunk inputs, and comments describing head-anchor suppression if they have no remaining callers. Do not leave an alias, deprecated path, or Rust-specific fallback.
   - Keep tests organized around observable ownership and review results; retain target ordinal/source path only as long as required to prove coordinate isolation and deterministic deduplication.
   - Re-run only the exact focused commands below, then the required repository gate.

### Ordered affected files and symbols

Prerequisite, already owned by issues #15/#25: `vcs::ChangedPath`, full per-target `vcs::FileDiff` batch results, and target-first review assembly. Issue #16 consumes those interfaces and must not replace them with a hunk-only aggregate.

1. `trueflow/src/commands/review.rs`
   - the per-target file-diff/base assembly introduced by issues #15/#25;
   - tests for `collect_diff_review_blocks_for_file` and target-scoped assembly;
   - `collect_diff_review_blocks_for_file` or its target-scoped replacement;
   - `HeadBlockMatchIndex`, `find_matching_head_block`, unchanged-line mapping helpers, and final review-unit deduplication;
   - obsolete `file_changed_lines` side-specific caller.
2. `trueflow/src/vcs.rs`
   - `DiffChangedLineIndex`;
   - `DiffBlockSide` replacement by explicit ownership;
   - `IndexedChangedDiffLine`;
   - `push_indexed_changed_lines_for_hunk`;
   - `DiffChangedLineIndex::change_kind_for_block`;
   - existing and new ownership/churn tests.

No block-splitter, language-registration, configuration, persistence, TUI, `ChangedPath`, or batch traversal implementation should change as part of issue #16.

## Verification and validation

Run from the repository root. Keep pre-gate commands exact; do not substitute module-wide or workspace-wide test filters.

Establish the baseline red/guard behavior before implementation:

```sh
cd trueflow && cargo test --lib commands::review::tests::collect_diff_review_blocks_block_start_removal_marks_attached_attribute_as_changed -- --exact
cd trueflow && cargo test --lib commands::review::tests::collect_diff_review_blocks_block_start_removal_marks_attached_doc_comment_as_changed -- --exact
cd trueflow && cargo test --lib vcs::tests::head_changed_line_index_does_not_attribute_deletion_to_following_block -- --exact
```

The first two commands must fail before the fix and pass afterward. The third is the pre-existing green guard and must stay green throughout.

Run the ownership mirror and target-coordinate isolation tests:

```sh
cd trueflow && cargo test --lib commands::review::tests::collect_diff_review_blocks_block_start_removal_does_not_attribute_predecessor_to_survivor -- --exact
cd trueflow && cargo test --lib commands::review::tests::collect_diff_review_files_block_start_removal_scopes_ownership_by_target_source_path -- --exact
cd trueflow && cargo test --lib commands::review::tests::collect_diff_review_files_block_start_removal_preserves_duplicate_target_boundaries -- --exact
```

Run replacement and nonreviewable-churn coverage:

```sh
cd trueflow && cargo test --lib commands::review::tests::collect_diff_review_blocks_keeps_block_start_replacement_paired -- --exact
cd trueflow && cargo test --lib commands::review::tests::collect_diff_review_blocks_filters_block_start_whitespace_churn -- --exact
cd trueflow && cargo test --lib vcs::tests::diff_changed_line_index_block_start_whitespace_replacement_is_nonreviewable -- --exact
cd trueflow && cargo test --lib vcs::tests::diff_changed_line_index_block_start_whitespace_only_changes_are_nonreviewable -- --exact
cd trueflow && cargo test --lib vcs::tests::diff_changed_line_index_block_start_whitespace_mixed_with_removal_is_reviewable -- --exact
```

Behavioral assertions must verify all of the following, not only result counts:

- attached attribute and doc-comment removals return one `Changed` unit with both base and head ownership;
- the surviving head block is the display block;
- an independently deleted predecessor is the only `Deleted` unit and the following survivor is absent;
- meaningful same-anchor replacements are paired and reviewable;
- trim-equivalent replacements and pure whitespace churn produce no review unit;
- mixed churn plus a real content change remains reviewable;
- two targets with the same destination but different source paths use only their own base blocks and hunk coordinates;
- a duplicate target remains two ordered mapping inputs and becomes one visible logical unit only during final deterministic deduplication.

Finally run the required repository gate:

```sh
just check
```

## Acceptance criteria

- Issues #15 and #25's coordinated target-first, full-`FileDiff` batch interface is present before issue #16 begins; no ownership mapping consumes a flattened cross-target hunk vector.
- Removing an attached attribute from a surviving block yields exactly one `BlockChangeKind::Changed` review unit with both base and head sides.
- Removing an attached documentation comment from a surviving block yields the same paired `Changed` result.
- The implementation contains no Rust-token, attribute, decorator, or comment-prefix check; it decides ownership from matched base/head ranges and real diff coordinates.
- Deleting a preceding independent block yields a base-only `Deleted` unit and does not mark the following block `Changed` or `Added`.
- The behavioral invariant of `head_changed_line_index_does_not_attribute_deletion_to_following_block` remains explicitly tested after the API cutover.
- Unique semantic survivors are reserved before positional matching; removed/added anchor positions alone cannot steal an unchanged survivor.
- A surviving unchanged line can establish generic base/head ownership even when removed leading metadata prevents or changes a semantic identifier.
- Every base lookup, unchanged-line map, match, and `DiffChangedLineIndex` query is scoped to one `ReviewDiffTarget` and that target's full `FileDiff`/source path.
- Two targets mapping different source paths to one destination cannot cross-match equal numeric line coordinates.
- Repeated targets are preserved as repeated ordered inputs through target-scoped mapping; equivalent visible review units are deduplicated only after target candidate production.
- Meaningful block-start replacements remain paired `Changed` units.
- Trim-equivalent replacements, whitespace-only additions, and whitespace-only removals remain nonreviewable; mixing them with a real change remains reviewable.
- Base-only deletions, head-only additions, exclusive block ends, and unsorted hunk input retain their existing behavior within each target.
- Every caller uses the ownership-aware API; blanket head-start suppression, aggregate cross-target ownership input, obsolete side-only index paths, and compatibility shims are absent.
- All exact focused commands and `just check` pass.

## Non-goals and risks

- Do not change how Rust or any other language attaches attributes, decorators, or comments to blocks. The splitter behavior is input evidence, not the fix location.
- Do not add a Rust-specific `#[...]`, `///`, or comment special-case to VCS or review matching.
- Do not change the definitions of closing-brace churn, whitespace-only churn, or trim-equivalent replacement runs.
- Do not reimplement or revise issues #15/#25's `ChangedPath`, rename-aware base lookup, target-first batch traversal, or full per-target `FileDiff` contract here. They are ordering prerequisites.
- Do not add cross-file move detection, rename migration, fuzzy syntax matching, or compatibility aliases. Different source paths matter only because the prerequisite `FileDiff` identifies the correct base for each existing target.
- Do not make deletion anchors count as unchanged overlap. They are rendering positions, not survivor identity.
- Never flatten, sort together, or deduplicate hunks/base blocks from different targets before ownership mapping. Target B may reuse target A's numeric lines while referring to an unrelated base tree.
- Matching changes can create false pairs if positional fallback sees unchanged blocks. Mitigate this by per-target global semantic reservation, positive unchanged-line correspondence for unchanged survivors, and restricting mapped-range fallback to independently changed blocks on both sides.
- Duplicate semantic identifiers or equal overlap scores are ambiguous. Preserve one-to-one deterministic behavior within the target and leave genuinely unresolved sides separate rather than allowing an earlier candidate to steal a later exact match.
- Duplicate targets must not be collapsed at input, while duplicate visible blocks must not multiply. Keep target ordinal through candidate production and apply one deterministic final review-unit deduplication.
- Multiple removals at file start, multiple attached metadata lines, and adjacent blocks share the same anchor shape; tests must cover a multi-line prefix so the implementation does not accidentally fix only one removed line.
- Build one index per target text diff and preserve coordinate indexing/stable diff order so correctness does not introduce an avoidable full-hunk rescan, cross-target line-text clone, or retained batch of all target blob resources.
