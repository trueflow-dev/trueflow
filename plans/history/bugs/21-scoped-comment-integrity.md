# Issue #21: Scoped comment verdict and render-anchor integrity

Status: ready
Date: 2026-07-10
Baseline commit: 9a98914698c4

## Problem

Two coupled invariants are currently violated for comments created from a paged TUI viewport:

1. Feedback export uses the viewport-projected block as both the presentation identity and the latest-verdict identity. A scoped comment therefore has a different key from a later approval of the same full block. With approved entries excluded, the obsolete scoped comment can remain in feedback even though the reviewer subsequently approved its full verdict target.
2. The TUI captures `CommentScope` and `comment_context` from the final `BuiltContent` rendered at the scrollbar-adjusted width, but reconstructs `CommentAnchor` later from a new frame at `state.code_rect.width`. `code_rect.width` is the full code rectangle, including the column reserved for a visible scrollbar. A source or diff row that wraps at the effective width but not at the rebuilt width can therefore change which logical rows intersect the viewport. The stored scope/context and stored anchor then describe different content.

The required invariant is:

> For one comment action, the displayed logical rows, persisted scoped presentation (`CommentScope` plus `comment_context`, when the pane is paged), persisted `CommentAnchor`, and canonical verdict target must all describe the same rendered review target. Presentation scope may narrow the exported context, but it must not create a new verdict target. For a block verdict that canonical target is the resolved unscoped block; for file/tree verdicts it is the path target itself, not whichever child block supplied presentation context.

Line spans remain zero-indexed, half-open ranges. Diff anchors retain their existing ordered `kind`/`old_line`/`new_line` row representation.

## Evidence

- `trueflow/src/feedback_export.rs` defines a single `FeedbackEntryKey` from snapshot, `ReviewTargetRef`, resolved path, block hash, and block start/end lines. `ResolvedFeedbackRecord` stores only that key.
- `resolve_feedback_context` calls `scoped_block_for_record` before returning a resolved context. That projection preserves the full block hash but replaces its content and start/end lines with `comment_context` and `CommentScope`.
- `feedback_entry_parts` builds `FeedbackEntryKey` from that already-projected block. `latest_verdicts_by_entry_key` and the final presentation grouping both use the same key. Consequently, a comment projected to (for example) lines `3..8` cannot share the latest-verdict key of a later approval resolved to the full block range `0..12`.
- Existing feedback tests cover an unscoped comment followed by approval, hint precision, and the same block hash in another path. They do not cover a scoped comment followed by full-block approval or two distinct scoped presentations that share one projected range.
- `resolve_record_in_files` also uses `line_hint` to choose a child block for `ReviewTargetRef::File`/`Tree` presentation. A paged file comment can obtain its line hint from `CommentScope`, while a later whole-file approval has no such hint and can resolve to a different child block. A canonical verdict key based unconditionally on the resolved child block would preserve the same defect for non-block targets; target kind must determine canonical locator shape.
- `build_contextual_diff_rows_matching` assigns a removed row the current new-file `anchor_index` without advancing `new_line`; the following added row can therefore have the same `anchor_index`. If separate viewports capture only the removed row and only the added row, both records can persist the same half-open `CommentScope` even though `comment_context` and ordered `DiffCommentAnchorRow` identity differ. The current `FeedbackEntryKey` ignores both fields, so grouping can merge the records and retain whichever projected block/context was inserted first.
- `trueflow/src/commands/tui.rs::render_active_node` first builds content at the full code width, detects whether a scrollbar is needed, reserves one column, rebuilds, and repeats until the width stabilizes. The resulting `content` is the exact `BuiltContent` rendered by `Paragraph`.
- The same function derives `state.visible_comment_capture` from that final `content`, so `CommentScope` and `comment_context` use the effective width.
- `render_active_node` nevertheless stores `state.code_rect = focus_layout.code`, whose width still includes the scrollbar column.
- `mark_params_for_action` reads scope/context from `visible_comment_capture`, then calls `comment_anchor_for_current_action`. That function rebuilds content with `build_content_lines`, a default palette, and `state.code_rect.width`; it does not reuse the rendered `BuiltContent`, the final `render_code_width`, or the render cache path.
- `CommentContextRow::display_row_range` is width-dependent for both source rows (`source_comment_rows`) and diff rows (`build_diff_context_content` and `build_deleted_block_diff_content`). A one-column width discrepancy can therefore change the selected source range or ordered diff-anchor rows at the viewport boundary.
- Existing TUI unit tests independently exercise visible source/diff context and unpaged source/diff anchors. The VT100 suite verifies paged scope/context and scrollbar rendering, but it does not assert that one submitted action's scope, context, and anchor all came from the same scrollbar-adjusted frame.

## Reproduction

### Verdict-key mismatch

1. Resolve a comment record for a block at `src/lib.rs`, with the presentation projected to only the visible lines (for example `3..8`).
2. Resolve a later `Approved` record for the same snapshot, path, target hash, and full block, without a comment scope.
3. Call `collect_feedback_entries` with `include_approved: false`.
4. Expected: no feedback entry remains for the scoped comment because the later full-block approval is the latest verdict for its canonical target.
5. Current result: the comment and approval use different `FeedbackEntryKey` ranges, so the scoped comment is still exported.

Control cases are required because simply dropping line ranges from the existing key would be incorrect:

- Two comments on different viewport scopes of the same block must remain distinct presentation entries. More strongly, comments on a removed row and its replacement added row must remain distinct even when both persist the same `CommentScope`.
- An approval for the same block hash in `src/b.rs` must not suppress a scoped comment in `src/a.rs`.
- A paged `File` comment and a later approval of that same file must share a verdict key even if their different line hints select different child blocks for presentation.

### Render/capture mismatch

1. Render a source block or textual diff in a terminal whose content requires a scrollbar.
2. Put a logical row at the wrapping boundary: it fits in the full code rectangle but wraps when the scrollbar's one-column reservation is applied.
3. Scroll so that the extra wrapped display row intersects the top or bottom of the viewport, then submit a comment.
4. Expected: persisted scope/context and source-anchor range, or persisted scope/context and ordered diff-anchor rows, are derived from the exact rows visible in that rendered frame.
5. Current result: scope/context use the scrollbar-adjusted `BuiltContent`, while the anchor is recomputed at the full width and can include or omit a different boundary row.

A no-scroll frame is the control: it must persist no viewport scope/context and must anchor every logical source or diff row displayed for the full target.

## Root cause

The feedback exporter conflates two identities that intentionally have different granularity:

- **Presentation identity:** snapshot + target + path + projected block hash/range + a stable scoped-presentation discriminator. The discriminator uses the exact scoped context and optional structural source/diff anchor identity, so same-range removed/added rows remain separate while repeated reviews of the exact same presentation can still group.
- **Verdict identity:** snapshot plus a target-kind-aware canonical locator. A block locator uses target hash, resolved path, and the resolved unscoped block range so duplicate blocks remain distinct. A file/tree locator uses the typed target hash and target path identity, not the line-hint-selected child block used for presentation. This determines which later verdict supersedes earlier records for the same actual review target.

Because scope projection currently occurs inside context resolution and discards the unscoped block, the exporter cannot build both identities. Reusing the projected key for latest-verdict lookup makes presentation detail incorrectly alter verdict semantics.

The TUI similarly splits one render-derived fact into two pipelines. `visible_comment_capture_for_content` uses the final rendered `BuiltContent`, but `comment_anchor_for_current_action` treats width-dependent content as reproducible action-time state. The width is not reproducible from `code_rect`, because the effective render width is conditionally one column smaller. Rebuilding also creates a second source of truth for view mode, diff rows, wrapping, and row selection.

## Implementation plan

1. **Add red feedback-export tests before changing keying.**
   - File: `trueflow/src/feedback_export.rs`, test module.
   - Add `collect_feedback_entries_excludes_scoped_comment_after_later_full_block_approval`:
     - use one snapshot, path, target hash, and canonical full block;
     - give the earlier `Comment` a valid `CommentScope` and matching `comment_context` over a strict subrange;
     - give the later `Approved` record no scope;
     - resolve both records to the same canonical block, while the comment's presentation block is scoped;
     - assert `include_approved: false` returns no entries.
   - Add `collect_feedback_entries_keeps_distinct_same_scope_diff_comments_for_one_target`:
     - create two comment records for the same snapshot, canonical block, path, target, and identical one-line `CommentScope`;
     - model the first as a removed diff row and the second as its replacement added row, with distinct exact `comment_context` and structural `DiffCommentAnchorRow` values but the same scope index;
     - assert two entries remain, each entry has the expected context and only its corresponding review record;
     - this prevents canonical verdict identity or the shared projected range from replacing exact presentation grouping.
   - Strengthen the existing duplicate-hash/path protection or add `collect_feedback_entries_keeps_scoped_comment_when_same_hash_in_other_path_is_approved`:
     - use the same target/block hash in `src/a.rs` and `src/b.rs`;
     - scope the comment in `src/a.rs` and approve the full block in `src/b.rs` later;
     - assert only the `src/a.rs` scoped comment remains when approvals are excluded.
   - Add `collect_feedback_entries_excludes_paged_file_comment_after_later_file_approval`:
     - create a file containing at least two reviewable child blocks;
     - give the earlier `ReviewTargetRef::File` comment a paged scope/line hint that resolves presentation to the second child, and give the later approval of the same file no scope or line hint so presentation resolves to the first child;
     - assert the later file approval suppresses the paged file comment with `include_approved: false`;
     - this prevents the canonical key from unconditionally inheriting a child block range for path targets.
   - Keep a simpler different-scope control if useful, but the same-scope removed/added regression is mandatory because range alone cannot distinguish those rows.
   - Keep timestamp/index tie-breaking assertions unchanged; the defect is key identity, not ordering.

2. **Add red TUI unit tests around the render-derived capture contract.**
   - File: `trueflow/src/commands/tui.rs`, existing test module near the visible-comment and comment-anchor tests.
   - Add `rendered_comment_capture_source_uses_scrollbar_adjusted_boundary_rows`:
     - construct source `BuiltContent` with a logical line whose display width is exactly at the full-width versus reserved-width boundary;
     - use the reserved effective width and a scrolled viewport that cuts through that wrapped line;
     - submit through `mark_params_for_action` using the capture produced from that exact content;
     - assert `comment_context` is exactly the selected logical source text, `CommentScope` equals the selected source anchor's start/end, path and revision are unchanged, and the fingerprint still targets the full block.
   - Add `rendered_comment_capture_diff_uses_scrollbar_adjusted_boundary_rows`:
     - use context/removed/added rows with a boundary-width row and a viewport edge through its wrapped display range;
     - assert context text is the rendered text for the selected logical rows and the `DiffCommentAnchor.rows` vector contains exactly those rows, in display order, with the expected kinds and old/new line numbers;
     - assert the mark fingerprint remains the full diff block target rather than a synthetic scoped target.
   - Add no-scroll source and diff controls:
     - source: `comment_scope` and `comment_context` are `None`, while the source anchor covers all displayed source lines;
     - diff: `comment_scope` and `comment_context` are `None`, while the diff anchor contains every displayed logical diff row in order;
     - use the same boundary fixture with enough height to prove the behavior is caused by scrollbar/effective-width selection, not merely the fixture's content.
   - Preserve tests for symbolic revision resolution. Revision/path decoration remains action-time work; only row selection becomes render-time work.

3. **Add red VT100 submission tests at the real terminal boundary.**
   - Files: `trueflow/tests/tui_vt100.rs` and, only if needed to inspect submitted values without invoking the CLI, `trueflow/src/commands/tui/test_support.rs`.
   - Add `scrollbar_boundary_source_note_persists_matching_scope_context_and_anchor`:
     - render a narrow source fixture that forces the one-column scrollbar reservation and boundary wrapping;
     - set a deterministic commit scope, scroll to a known viewport, submit through the existing scripted note flow, and inspect the stored/captured `ScriptedMarkAction` or resulting review record;
     - assert the terminal rows used for the expected viewport, stored context, half-open scope, `SourceCommentAnchor`, path, revision, and full block target agree exactly.
   - Add `scrollbar_boundary_diff_note_persists_matching_scope_context_and_anchor_rows`:
     - preload a textual diff with deterministic context/removed/added line numbers;
     - render and scroll at the same width boundary, submit a comment, and assert the stored ordered `DiffCommentAnchorRow` sequence corresponds exactly to the logical rows represented by the stored context.
   - Add or extend no-scroll VT100 controls for source and diff so the full-height frame produces no `CommentScope`/context but still persists a complete anchor.
   - Prefer a small recording `MarkActionRunner` backed by shared test state if direct action inspection is needed. Do not add production-only getters or alter the mark/store schema.

4. **Separate canonical verdict context from scoped presentation context.**
   - File: `trueflow/src/feedback_export.rs`.
   - Replace the lossy single-block resolved context with explicit composition that retains both the unscoped resolved block and scoped presentation block, for example a `ResolvedFeedbackBlock { unscoped: Block, presentation: Block }` held by `ResolvedFeedbackContext`. The unscoped block is canonical locator input only for block targets; file/tree records still use it to render presentation, not to identify the verdict target.
   - In `resolve_feedback_context`, retain the block returned by `resolve_record_in_files` as `unscoped`; derive `presentation` with `scoped_block_for_record(record, &unscoped).unwrap_or_else(|| unscoped.clone())`.
   - Refactor unresolved fallback construction so it also separates presentation from verdict location:
     - block-target fallback uses target hash plus stable path/`line_hint` location and never reads `CommentScope`;
     - presentation fallback applies valid scope/context when present;
     - file/tree verdict location ignores `line_hint` and whichever fallback child block was selected, using the typed target hash plus target path identity instead;
     - do not let the current scoped-first behavior of `unresolved_block_for_record` leak into any canonical verdict locator.
   - Introduce separate key types, or typed wrappers over shared locator values:
     - `FeedbackEntryKey` (presentation key) retains snapshot, target, resolved presentation path, block hash, projected block range, and a scoped-presentation discriminator;
     - `FeedbackVerdictKey` contains snapshot plus an explicit target-kind-aware locator enum;
     - the block locator retains target hash, resolved path, unscoped block hash, start line, and end line, preserving duplicate-hash and duplicate-block isolation;
     - file/tree locators retain their distinct target kind, target hash, and optional target path (the same hash/path precision modeled by `ReviewIndex`'s `PathTargetLocator`) and contain no child block range or line hint.
     - define the presentation discriminator structurally rather than with process-random hashing: unscoped records use an `Unscoped` variant; scoped records include exact `comment_context` and an optional internal source/diff anchor identity (revision, path, source span, or ordered diff row kind/old/new tuples). `CommentScope` is already represented by the projected range but may also be included for clarity;
     - do not include record id, timestamp, note, or verdict in the presentation discriminator: repeated reviews of the exact same presented rows should continue to group and sort as reviews of one entry;
     - keep the presentation discriminator entirely out of `FeedbackVerdictKey`, so removed/added comments and a later full-block approval still share one canonical latest-verdict identity.
   - Change `ResolvedFeedbackRecord` to carry both keys. `feedback_entry_parts` should return the presentation block/key and canonical verdict key from one resolved context and record target.
   - Rename/refactor `latest_verdicts_by_entry_key` to key its map by `FeedbackVerdictKey`. In `collect_feedback_entries`, look up filtering/`latest_verdict` through `resolved.verdict_key`, but group emitted entries through `resolved.entry_key`.
   - Preserve the existing timestamp comparison and source-order tie-breaker. Apply latest-verdict calculation before `since` filtering exactly as today.

5. **Capture presentation and anchor atomically from the final rendered frame.**
   - File: `trueflow/src/commands/tui.rs`.
   - Replace the scope-only `VisibleCommentCapture`/`AppState.visible_comment_capture` with one render-derived state value, for example:
     - `RenderedCommentCapture { node_id, presentation, anchor }`;
     - an explicit presentation enum with `FullBlock` and `Scoped { scope, context }` variants;
     - the existing `CommentAnchorSelection::{Source, Diff}` for undecorated row identity.
   - Use the enum to make invalid combinations unrepresentable: `CommentScope` and context are either both present for a paged viewport or both absent for a fully visible target; an anchor selection is part of the same capture.
   - Refactor row selection so one selected `Vec<CommentContextRow>` produces both the presentation and anchor selection:
     - if `content_height > viewport_height`, select only rows intersecting the visible display-row range and build `Scoped` plus its anchor from those same rows;
     - otherwise select all logical rows, build `FullBlock`, and build the complete anchor from those same rows.
   - In `render_active_node`, assign this capture only after the scrollbar fixed-point loop has settled and after scroll offset is clamped. Pass the final `BuiltContent`, final content/viewport heights, current scroll offset, and current node id. At this point `BuiltContent.comment_rows` already reflects the effective `render_code_width`, including the reserved scrollbar column.
   - In `mark_params_for_action`, accept a capture only for `Verdict::Comment`, the current node, and the capture's matching `node_id`. Convert its presentation directly into `comment_scope`/`comment_context`.
   - Replace `comment_anchor_for_current_action` with a decoration-only helper that combines the stored `CommentAnchorSelection` with the action-time resolved revision and path. It must not call `build_content_lines`, rebuild a frame, derive row ranges, or consult `code_rect.width`.
   - Keep the full node fingerprint/`ReviewTargetKind` and normal line hint as the verdict target. Scope is presentation metadata, never a replacement fingerprint.

6. **Run a focused behavioral smoke before cleanup.**
   - Run the new feedback tests and confirm the scoped-block-comment/approval case and paged-file-comment/file-approval case are empty, same-scope removed/added presentations remain distinct, and the cross-path same-hash control remains visible.
   - Run the new TUI unit tests and both VT100 boundary submissions. Confirm the one-column scrollbar case now records one coherent row selection in source and diff modes.
   - Confirm the no-scroll controls preserve current behavior: no scope/context projection, full source or diff anchor.

7. **Cleanup only after the focused smoke passes.**
   - Remove the obsolete action-time `BuiltContent` rebuild and any now-unused `CommentRowSelectionMode`/selection helper branches.
   - Update all `AppState` constructors in `trueflow/src/commands/tui.rs` tests and `trueflow/src/commands/tui/test_support.rs` to initialize the composed render capture consistently.
   - Retain any small internal source/diff presentation-key structs needed for exact `Hash`/`Eq`; do not use `DefaultHasher`, debug text, record ids, or lossy scope-only identity.
   - Keep `state.code_rect` for layout/rendering uses if still needed, but do not retain it as comment-row provenance.
   - Do not add aliases, compatibility fields, or migration code. `Record.comment_scope`, `comment_context`, and `comment_anchor` wire formats do not change.
   - Run the focused suites again, then the repository gate.

### Ordered affected files and symbols

1. `trueflow/src/feedback_export.rs`
   - `ResolvedFeedbackContext` and new resolved canonical/presentation block composition
   - `FeedbackEntryKey`, new structural scoped-presentation discriminator/source-diff anchor key types, and new `FeedbackVerdictKey`
   - `ResolvedFeedbackRecord`
   - `resolve_feedback_records`, `feedback_entry_parts`, `latest_verdicts_by_entry_key`, `collect_feedback_entries`
   - `resolve_feedback_context`, `scoped_block_for_record`, `scoped_unresolved_block_for_record`, `unresolved_block_for_record`
   - focused unit fixtures/tests
2. `trueflow/src/commands/tui.rs`
   - `VisibleCommentCapture`, `CommentAnchorSelection`, and the new composed render capture/presentation state
   - `AppState.visible_comment_capture` and all local test constructors
   - `render_active_node`
   - `visible_comment_capture_for_content`, `comment_anchor_selection_for_content`, `selected_comment_rows` (refactored into one row-selection pipeline)
   - `mark_params_for_action`, `comment_anchor_for_current_action`
   - source/diff boundary and no-scroll unit tests
3. `trueflow/src/commands/tui/test_support.rs`
   - `AppState` fixture initialization
   - optional test-only recording runner support, only if the existing `MarkActionRunner` interface is insufficient
4. `trueflow/tests/tui_vt100.rs`
   - source and diff scrollbar-boundary submission regressions
   - source and diff no-scroll controls

No changes are required to `trueflow/src/store.rs`; the existing `CommentScope`, `SourceCommentAnchor`, `DiffCommentAnchor`, and `DiffCommentAnchorRow` structures already express the required persisted result.

## Verification and validation

Run from the repository root.

### Red/green feedback-key tests

```sh
cd trueflow && cargo test --lib feedback_export::tests::collect_feedback_entries_excludes_scoped_comment_after_later_full_block_approval -- --exact
cd trueflow && cargo test --lib feedback_export::tests::collect_feedback_entries_keeps_distinct_same_scope_diff_comments_for_one_target -- --exact
cd trueflow && cargo test --lib feedback_export::tests::collect_feedback_entries_keeps_scoped_comment_when_same_hash_in_other_path_is_approved -- --exact
cd trueflow && cargo test --lib feedback_export::tests::collect_feedback_entries_excludes_paged_file_comment_after_later_file_approval -- --exact
```

Before the source fix, the scoped block test must fail because the projected and approval ranges key differently, the same-scope diff test must fail because removed/added presentations group under one range, and the paged file test must fail if canonicalization follows the line-hint-selected child block. The cross-path case is a control guarding against over-broad canonicalization.

### Focused TUI unit tests

```sh
cd trueflow && cargo test --lib commands::tui::tests::rendered_comment_capture_source_uses_scrollbar_adjusted_boundary_rows -- --exact
cd trueflow && cargo test --lib commands::tui::tests::rendered_comment_capture_diff_uses_scrollbar_adjusted_boundary_rows -- --exact
cd trueflow && cargo test --lib commands::tui::tests::rendered_comment_capture_source_no_scroll_anchors_full_target -- --exact
cd trueflow && cargo test --lib commands::tui::tests::rendered_comment_capture_diff_no_scroll_anchors_full_target -- --exact
```

The boundary tests must fail against the baseline action-time full-width rebuild and pass when the render-derived capture supplies both presentation and anchor. The controls must prove that fully visible content is not spuriously scoped.

### VT100 behavioral tests

```sh
cd trueflow && cargo test --features tui-test-support --test tui_vt100 scrollbar_boundary_source_note_persists_matching_scope_context_and_anchor -- --exact
cd trueflow && cargo test --features tui-test-support --test tui_vt100 scrollbar_boundary_diff_note_persists_matching_scope_context_and_anchor_rows -- --exact
cd trueflow && cargo test --features tui-test-support --test tui_vt100 no_scroll_source_note_anchors_full_rendered_target -- --exact
cd trueflow && cargo test --features tui-test-support --test tui_vt100 no_scroll_diff_note_anchors_all_rendered_rows -- --exact
```

For the two scrollbar cases, inspect/assert all of the following in the test itself rather than relying on a screenshot:

- the VT100 screen contains the expected boundary text;
- the submitted record/action retains the full block fingerprint and expected path/revision;
- source scope/context and source anchor cover the same logical lines;
- diff scope/context and ordered diff rows describe the same displayed logical diff rows;
- no off-screen boundary row is silently added or a visible row silently omitted.

For no-scroll controls, assert scope/context are absent and the anchor covers every displayed logical row.

### Focused regression suites

```sh
cd trueflow && cargo test --lib feedback_export::tests::collect_feedback_entries
cd trueflow && cargo test --lib commands::tui::tests::rendered_comment_capture
cd trueflow && cargo test --lib commands::tui::tests::mark_params_for_action
cd trueflow && cargo test --features tui-test-support --test tui_vt100 note_submit
cd trueflow && cargo test --features tui-test-support --test tui_vt100 scrollbar
```

### Final repository validation

```sh
just check
```

## Acceptance criteria

- A later full-block `Approved` record suppresses an earlier viewport-scoped comment for the same snapshot, target, resolved path, and canonical full block when `include_approved` is false.
- Distinct removed-row and replacement-added-row comments remain separate feedback presentation entries even when they share the same canonical block and identical `CommentScope`; exact context and ordered anchor identity select the correct presentation.
- The same hash in another resolved path remains a different canonical verdict target and cannot suppress the scoped comment.
- A paged file comment and later approval of the same file share a canonical verdict key even when scope-derived versus absent line hints resolve different child blocks for presentation.
- Feedback export has separate, explicitly typed presentation and canonical verdict keys; presentation grouping uses the scoped range plus stable exact context/anchor identity, while latest-verdict lookup uses only the canonical unscoped key.
- Resolved feedback context retains the unscoped block through scope projection for block locator construction, while file/tree canonical locators remain independent of presentation child-block choice; unresolved historical records follow the same target-kind-aware rule.
- The TUI derives scope, context, and anchor row selection once from the exact final `BuiltContent` after scrollbar width stabilization and scroll clamping.
- A comment action never rebuilds width-dependent content to derive its anchor. Action time only adds revision and path to the stored source/diff selection.
- At scrollbar wrapping boundaries, a source comment's half-open scope equals its source-anchor range and its context is exactly those logical source lines.
- At scrollbar wrapping boundaries, a diff comment's context and ordered anchor rows refer to the same displayed context/removed/added logical rows with correct old/new line numbers.
- In no-scroll source and diff controls, scope/context remain absent and anchors cover the complete displayed target.
- The persisted verdict fingerprint remains the full review node target; viewport scope is presentation metadata and does not create a scoped verdict target.
- Existing timestamp/source-order latest-verdict precedence, path filtering, symbolic revision resolution, and store serialization remain unchanged.
- All focused commands and final `just check` pass.

## Non-goals and risks

### Non-goals

- Do not redesign GitHub inline-comment remapping in `trueflow/src/commands/feedback.rs`. Source-to-GitHub and diff-to-GitHub translation continue to consume the captured `CommentAnchor` exactly as before.
- Do not remap anchors across later commits beyond the already captured source/diff anchor.
- Do not change `Record`, `CommentScope`, or `CommentAnchor` serialization, the reviews JSONL schema, CLI hidden comment arguments, or signing/canonicalization behavior.
- Do not merge different scoped comments into one feedback entry.
- Do not change review coverage semantics, TUI navigation, scroll behavior, diff formatting, or latest-verdict timestamp precedence.
- Do not add compatibility shims or migrations; the project is beta and the persisted schema is unchanged.

### Risks and mitigations

- **Accidental over-keying:** Keying every verdict only by target hash would suppress identical block hashes in other paths or duplicate blocks. Use a target-kind-aware locator: block targets retain snapshot, resolved path, unscoped block hash/range; file/tree targets retain snapshot, typed target hash, and optional target path. Keep both the cross-path block regression and paged file regression.
- **Accidental under-suppression:** If block fallback still uses `CommentScope`, or if file/tree canonicalization uses `line_hint` or a selected child block, the defect survives. Construct verdict locators independently from presentation projection and cover both resolved block and paged file cases.
- **Presentation collision:** Diff removed/added rows can share a scope index, so scope range alone is not a presentation key. Include exact scoped context and structural optional anchor identity; never use note/id (over-splitting) or process-random/debug-text hashes (unstable identity).
- **Scope/context drift:** Two optional fields can represent invalid half-states. Use an explicit `FullBlock` versus `Scoped { scope, context }` presentation enum inside render state, then translate to the existing optional wire fields only at `MarkParams` construction.
- **Stale rendered capture:** Navigation or mode changes could otherwise reuse a previous node's capture. Store the node id with the capture and require it to match the current action target; normal rerendering replaces the capture.
- **Width off-by-one:** `focus_layout.code.width` is not always the effective content width. Compute capture only from the final `BuiltContent` after the existing scrollbar fixed-point loop; never infer provenance from `code_rect` later.
- **Diff indexing confusion:** `CommentScope` uses zero-indexed half-open presentation indices, while diff anchor rows carry existing one-indexed old/new line numbers. Assert both independently against deterministic fixtures rather than comparing unlike numeric fields directly.
- **Mixed or unavailable content:** Speed-read and non-code views may have no `comment_rows`. Preserve the existing behavior of producing no render capture/anchor rather than fabricating rows or falling back to a rebuilt frame.
